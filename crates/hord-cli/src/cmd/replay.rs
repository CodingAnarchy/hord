//! `hord replay <change> --harness <cmd>` (spec §6.6, §10.2): run the
//! replay protocol by hand, and the repository's harness configuration,
//! `.hord/replay.toml`, which the daemon and `hord serve --repo` give their
//! lander (spec §6.4 rung 2).
//!
//! ```toml
//! # .hord/replay.toml
//! harness = ["hord-replay-ref", "--cmd", "my-agent --prompt-file \"$HORD_REPLAY_PROMPT_FILE\""]
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hord_api::RepoBackend;
use hord_api::proto::replay_result::Status;
use hord_api::{proto, wire};
use hord_core::{Blob, ObjectId, ReplayBudget, Snapshot, Tree, TreeEntry};
use hord_policy::POLICY_PATH;
use hord_txn::{CommandHarness, ReplayHarness, RepoOptions};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, backend_change, block_on, short};
use crate::workspaces::this_caller;

/// File name of the harness configuration under `.hord/`.
pub const CONFIG_FILE: &str = "replay.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The harness command line: program, then arguments.
    harness: Vec<String>,
}

/// The harness `<root>/.hord/replay.toml` configures, if any.
pub fn configured_harness(root: &Path) -> Result<Option<Arc<dyn ReplayHarness>>> {
    let path = root.join(hord_store::HORD_DIR).join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    let harness = CommandHarness::new(config.harness)
        .with_context(|| format!("{}: `harness` is empty", path.display()))?;
    Ok(Some(Arc::new(harness)))
}

/// Repository options for a process that runs `root`'s lander: its
/// configured replay harness.
pub fn lander_options(root: &Path) -> Result<RepoOptions> {
    Ok(RepoOptions {
        harness: configured_harness(root)?,
        ..RepoOptions::default()
    })
}

/// Budget flags; each one left out is head's (ADR 0028).
pub struct Limits {
    pub wall_time_secs: Option<u64>,
    pub tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl Limits {
    /// `head`'s budget with the flags given replacing its fields.
    fn budget(&self, head: ReplayBudget) -> Result<ReplayBudget> {
        let wall_time_ms = match self.wall_time_secs {
            None => head.wall_time_ms,
            Some(0) => bail!("--wall-time-secs must be at least 1"),
            Some(secs) => secs.saturating_mul(1_000),
        };
        let cost_micros = match self.cost_usd {
            None => head.cost_micros,
            Some(usd) => {
                let micros = (usd * 1_000_000.0).round();
                if !micros.is_finite() || micros < 0.0 || micros >= u64::MAX as f64 {
                    bail!("--cost-usd must be a non-negative number of dollars");
                }
                // Checked just above: integral and in range.
                Some(micros as u64)
            }
        };
        Ok(ReplayBudget {
            wall_time_ms,
            tokens: self.tokens.or(head.tokens),
            cost_micros,
        })
    }
}

/// Read and decode object `id` through `backend`.
fn backend_object<T: DeserializeOwned>(backend: &dyn RepoBackend, id: ObjectId) -> Result<T> {
    let reply = block_on(backend.get_objects(proto::GetObjectsRequest {
        ids: vec![wire::id(id)],
    }))?;
    let object = reply
        .objects
        .into_iter()
        .next()
        .with_context(|| format!("no object {id}"))?;
    hord_encoding::decode(&object.cbor).with_context(|| format!("decode object {id}"))
}

/// Head's `[replay] budget` (ADR 0028): the `.hord-policy.toml` at the root
/// of head's snapshot, or the default when there is none (ADR 0026).
pub fn head_budget(backend: &dyn RepoBackend) -> Result<ReplayBudget> {
    let Some((_, snapshot)) = txn::backend_head(backend)? else {
        return Ok(ReplayBudget::default());
    };
    let snapshot: Snapshot = backend_object(backend, snapshot)?;
    let root: Tree = backend_object(backend, snapshot.root())?;
    let Some(TreeEntry::Blob(blob)) = root.entries.get(POLICY_PATH) else {
        return Ok(ReplayBudget::default());
    };
    let blob: Blob = backend_object(backend, *blob)?;
    let text = std::str::from_utf8(blob.bytes.as_slice())
        .with_context(|| format!("head's {} is not UTF-8", POLICY_PATH))?;
    let policy = hord_policy::parse(text)
        .with_context(|| format!("head's {} does not parse", POLICY_PATH))?;
    Ok(policy.replay().budget.clone())
}

/// `cmd` run by the platform shell.
fn shell(cmd: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), cmd.into()]
    } else {
        vec!["sh".into(), "-c".into(), cmd.into()]
    }
}

pub fn run(
    json: bool,
    target: &Target,
    change: String,
    harness: String,
    limits: Limits,
    note: Option<String>,
) -> Result<()> {
    let id = txn::parse_change(&change)?;
    let root = crate::repo::discover_root()?;
    let session = Session::open(target)?;
    if matches!(session, Session::Direct { .. }) {
        bail!(
            "hord replay needs the repository's daemon or a remote: the harness proposes \
             through it (drop --no-daemon)"
        );
    }
    let backend = session.backend();
    let budget = limits.budget(head_budget(backend.as_ref())?)?;
    let record = backend_change(backend.as_ref(), id)?;
    let entry = block_on(backend.queue(proto::QueueQuery {
        change: Some(change.clone()),
        ..Default::default()
    }))?
    .entries
    .into_iter()
    .rev()
    .find(|e| e.change == change);
    let attempt_number = entry
        .as_ref()
        .and_then(|e| e.escalation.as_ref())
        .map_or(0, |e| e.attempts.len())
        + 1;
    let ws = block_on(session.workspaces().ws_new(proto::WsNewRequest {
        caller: Some(this_caller()),
        base: None,
        materialize: proto::Materialize::Clone.into(),
    }))?;
    let request = proto::ReplayRequest {
        change: change.clone(),
        attempt: u32::try_from(attempt_number).unwrap_or(u32::MAX),
        intent: Some(hord_txn::intent_message(&record)),
        provenance: Some(hord_txn::provenance_message(&record)),
        base: ws.base.clone(),
        workspace: ws.id.clone(),
        workspace_path: ws.materialization.clone(),
        repo: root.display().to_string(),
        conflict: entry.as_ref().and_then(|e| e.report.clone()),
        summary: entry
            .as_ref()
            .and_then(|e| e.escalation.as_ref())
            .and_then(|e| e.summary.clone()),
        budget: Some(hord_txn::budget_message(&budget)),
        note: note.clone(),
        protected_tests: protected_tests(backend.as_ref(), &record, entry.as_ref())?,
    };
    let command = CommandHarness::new(shell(&harness)).context("an empty --harness")?;
    let started = Instant::now();
    let answer = block_on(tokio::time::timeout(
        Duration::from_millis(budget.wall_time_ms),
        command.run(&request),
    ));
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut attempt = proto::ReplayAttempt {
        attempt: request.attempt,
        harness: harness.clone(),
        elapsed_ms,
        note,
        ..Default::default()
    };
    let (outcome, detail) = match answer {
        Err(_) => (
            proto::ReplayOutcome::Killed,
            format!(
                "ran past its wall-clock budget of {} ms and was killed",
                budget.wall_time_ms
            ),
        ),
        Ok(Err(reason)) => (proto::ReplayOutcome::Failed, reason),
        Ok(Ok(result)) => {
            attempt.tokens = result.tokens;
            attempt.cost_micros = result.cost_micros;
            attempt.model.clone_from(&result.model);
            match hord_txn::over_budget(&result, &budget) {
                Some(why) => (proto::ReplayOutcome::OverBudget, why),
                None => match result.status {
                    Some(Status::GaveUp(g)) => (proto::ReplayOutcome::GaveUp, g.reason),
                    None => (
                        proto::ReplayOutcome::Failed,
                        "the result has no status".into(),
                    ),
                    Some(Status::Proposed(p)) => {
                        match submit_replay(backend.as_ref(), id, &ws.base, &p.change) {
                            Ok(replay) => {
                                attempt.change = Some(replay);
                                (proto::ReplayOutcome::Proposed, "submitted".into())
                            }
                            Err(err) => (proto::ReplayOutcome::Failed, format!("{err:#}")),
                        }
                    }
                },
            }
        }
    };
    attempt.outcome = outcome.into();
    attempt.detail = Some(detail);
    // The proposal (if any) is stored; the checkout is not needed.
    let _ = block_on(
        session
            .workspaces()
            .ws_rm(proto::WsRmRequest { id: ws.id.clone() }),
    );
    let result = proto::ReplayRunResult {
        change,
        attempt: Some(attempt),
    };
    if json {
        output::print_json(&result)?;
    } else if let Some(attempt) = &result.attempt {
        let outcome = match attempt.outcome() {
            proto::ReplayOutcome::Proposed => "proposed",
            proto::ReplayOutcome::GaveUp => "gave up",
            proto::ReplayOutcome::Killed => "killed",
            proto::ReplayOutcome::OverBudget => "over budget",
            proto::ReplayOutcome::Failed => "failed",
            proto::ReplayOutcome::Tampered => "changed an acceptance test (rejected)",
            proto::ReplayOutcome::Running | proto::ReplayOutcome::Unspecified => "unknown",
        };
        println!(
            "replay of {} (attempt {}): {outcome}",
            short(&result.change),
            attempt.attempt
        );
        if let Some(replay) = &attempt.change {
            println!("queued replay {replay}");
        }
        if let Some(detail) = &attempt.detail {
            println!("{detail}");
        }
    }
    Ok(())
}

/// The acceptance tests a replay must not change (ADR 0034): those the
/// change's intent names, and those of the changes it collided with.
fn protected_tests(
    backend: &dyn RepoBackend,
    record: &hord_core::ChangeRecord,
    entry: Option<&proto::QueueEntry>,
) -> Result<Vec<String>> {
    let mut intents = vec![record.intent.clone()];
    let colliders: Vec<String> = entry
        .and_then(|e| e.report.as_ref())
        .map(|r| r.conflicts.iter().map(|c| c.landed.clone()).collect())
        .unwrap_or_default();
    for landed in colliders {
        let id = txn::parse_change(&landed)?;
        intents.push(backend_change(backend, id)?.intent);
    }
    let mut names: Vec<String> = Vec::new();
    for intent in intents {
        for acceptance in intent.acceptance {
            if let hord_core::Acceptance::Test { name } = acceptance
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    Ok(names)
}

/// Record what the harness proposed as a replay of `of` (`parent_intent`,
/// spec §6.6) and submit it. Returns the replay's id.
fn submit_replay(
    backend: &dyn RepoBackend,
    of: hord_core::ChangeId,
    base: &str,
    proposed: &str,
) -> Result<String> {
    let proposed = txn::parse_change(proposed).context("the harness proposed")?;
    let record = backend_change(backend, proposed)?;
    if wire::id(record.base) != base {
        bail!(
            "proposed change {proposed} is based on {}, not on the replay's base {base}",
            record.base
        );
    }
    let replay = match record.provenance.parent_intent {
        Some(parent) if parent == of => record,
        Some(other) => bail!("proposed change {proposed} is a replay of {other}, not of {of}"),
        None => hord_txn::as_replay(record, of),
    };
    let bytes = hord_encoding::encode(&replay)?;
    let object = wire::object(bytes);
    let id = object.id.clone();
    block_on(backend.put_objects(proto::PutObjectsRequest {
        objects: vec![object],
    }))?;
    block_on(backend.submit(proto::SubmitRequest { change: id.clone() }))?;
    Ok(id)
}
