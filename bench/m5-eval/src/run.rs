//! Running one case end to end, through the same API the CLI and the web
//! workbench use: the case's repository gets its own daemon (`hord serve
//! --daemon`), whose lander runs the replay harness from
//! `.hord/replay.toml`; tasks are proposed through its `Workspaces`
//! service, and parked cases are resolved with a signed `Arbitrate`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, WorkspacesBackend, proto, wire};
use hord_core::sign::SigningKey;
use hord_core::{Actor, Bytes, ChangeId, ChangeRecord, Signature};
use hord_remote::RemoteRepo;
use hord_txn::Arbitration;
use serde::Serialize;
use tokio::process::Command;
use tokio_stream::StreamExt;

use crate::corpus::Case;

/// Slack on the wall-clock check: an attempt that is not killed must end
/// within its budget, give or take timer and scheduling noise.
const WALL_GRACE_MS: u64 = 500;
/// How often the queue is polled.
const POLL: Duration = Duration::from_millis(200);

/// Which harness the lander runs.
#[derive(Clone, Debug)]
pub enum Harness {
    /// `hord-eval-m5 scripted`: the case's script (CI).
    Scripted,
    /// A model command for `hord-replay-ref --cmd`, such as `claude -p`.
    Command(String),
    /// No harness: every conflict waits for an arbiter.
    None,
}

/// How to run the corpus.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `hord` binary.
    pub hord: PathBuf,
    /// The `hord-replay-ref` binary.
    pub replay_ref: PathBuf,
    /// This binary (for `scripted`).
    pub eval: PathBuf,
    /// The harness.
    pub harness: Harness,
    /// A model name for `hord-replay-ref --model`.
    pub model: Option<String>,
    /// `[replay] budget` wall-clock time per attempt.
    pub wall_time_secs: u64,
    /// `[replay] budget` tokens per attempt.
    pub tokens: u64,
    /// `[land] max_replay_attempts`.
    pub max_attempts: u64,
    /// Where case repositories are built.
    pub work: PathBuf,
    /// Keep each case's repository.
    pub keep: bool,
}

/// How a case ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// A replay landed and both acceptance tests pass on the landed head.
    ResolvedByReplay,
    /// A replay landed, but an acceptance test fails on the landed head.
    ReplayFailedAcceptance {
        /// The test output.
        why: String,
    },
    /// Parked for arbitration (or conflicted with no harness).
    Parked,
    /// The case does not do what the corpus needs (the first task did not
    /// land, or the second did not collide).
    Invalid {
        /// What happened instead.
        why: String,
    },
    /// The runner failed.
    Error {
        /// The error.
        why: String,
    },
}

/// One replay attempt, as the lander recorded it.
#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    /// Attempt number.
    pub attempt: u32,
    /// `proposed`, `gave_up`, `killed`, `over_budget`, `failed`, or
    /// `running`.
    pub outcome: String,
    /// Wall-clock time.
    pub elapsed_ms: u64,
    /// Reported tokens.
    pub tokens: Option<u64>,
    /// The lander's note.
    pub detail: Option<String>,
}

/// The arbitration round-trip of a parked case.
#[derive(Clone, Debug, Serialize)]
pub struct RoundTrip {
    /// It landed with both parents and a verified signed `Arbitrated`.
    pub ok: bool,
    /// What went wrong, or what landed.
    pub detail: String,
}

/// Everything about one case's run.
#[derive(Clone, Debug, Serialize)]
pub struct CaseResult {
    /// Case id.
    pub id: String,
    /// Template.
    pub kind: String,
    /// `hard` or `semantic`, as designed.
    pub designed_conflict: String,
    /// What the lander saw: `hard` (a hard merge conflict) or `semantic`.
    pub observed_conflict: Option<String>,
    /// Intents contradict.
    pub ambiguous: bool,
    /// The two intents.
    pub intents: [String; 2],
    /// How it ended.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Replay attempts.
    pub attempts: Vec<Attempt>,
    /// Attempts that ran past their budget without being killed, or whose
    /// over-budget result was accepted: hard failures.
    pub budget_violations: Vec<String>,
    /// For a parked case: its conflict summary.
    pub summary: Option<String>,
    /// For a parked case: the arbitration round-trip.
    pub arbitration: Option<RoundTrip>,
    /// With the scripted harness: whether the script resolves it.
    pub expected_resolved: Option<bool>,
    /// Wall-clock seconds for the case.
    pub seconds: f64,
}

async fn run_tool(program: &Path, args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "m5-eval")
        .env("GIT_AUTHOR_EMAIL", "m5-eval@example.com")
        .env("GIT_COMMITTER_NAME", "m5-eval")
        .env("GIT_COMMITTER_EMAIL", "m5-eval@example.com")
        .env("HORD_ACTOR", "m5-eval")
        .env("HORD_NO_DAEMON", "1")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {}", program.display()))?;
    if !out.status.success() {
        bail!(
            "{} {args:?}: {}{}",
            program.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn write_files<'a>(
    dir: &Path,
    files: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Result<()> {
    for (path, text) in files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, text).with_context(|| format!("write {}", target.display()))?;
    }
    Ok(())
}

/// Remove `dir`, making read-only pristine checkouts writable first.
fn remove_tree(dir: &Path) {
    fn writable(dir: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(dir) {
                let mode = meta.permissions().mode() | 0o700;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
            }
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    writable(&entry.path());
                }
            }
        }
    }
    if dir.exists() {
        writable(dir);
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// A case's repository with its daemon; the daemon stops when dropped.
struct CaseRepo {
    dir: PathBuf,
    remote: RemoteRepo,
    daemon: tokio::process::Child,
}

impl CaseRepo {
    async fn stop(mut self) {
        let _ = self
            .remote
            .workspaces()
            .shutdown(proto::ShutdownRequest {})
            .await;
        if tokio::time::timeout(Duration::from_secs(10), self.daemon.wait())
            .await
            .is_err()
        {
            let _ = self.daemon.kill().await;
        }
    }
}

fn caller(id: &str) -> proto::Caller {
    proto::Caller {
        actor: Some(wire::actor(&Actor::Agent {
            id: id.into(),
            model: "m5-eval".into(),
            model_hash: Bytes::default(),
            harness: "hord-eval-m5".into(),
        })),
        session: None,
    }
}

/// Build the case's repository, import it, configure the harness, and
/// start its daemon.
async fn build(cfg: &Config, path: &Path, case: &Case) -> Result<CaseRepo> {
    let dir = cfg.work.join(&case.id);
    remove_tree(&dir);
    std::fs::create_dir_all(&dir)?;
    write_files(&dir, &case.base)?;
    let policy = format!(
        "[land]\nrequire = [\"check\", \"test:full\"]\nmax_replay_attempts = {}\n\n[replay]\nbudget = {{ wall_time_secs = {}, tokens = {} }}\n",
        cfg.max_attempts, cfg.wall_time_secs, cfg.tokens
    );
    std::fs::write(dir.join(".hord-policy.toml"), policy)?;
    let git = Path::new("git");
    run_tool(git, &["init", "-q", "-b", "main"], &dir).await?;
    run_tool(git, &["add", "."], &dir).await?;
    run_tool(git, &["commit", "-q", "-m", "base"], &dir).await?;
    let dir_text = dir.to_str().ok_or_else(|| anyhow!("non-UTF-8 work path"))?;
    run_tool(&cfg.hord, &["init", "--from-git", dir_text], &dir).await?;

    let command = match &cfg.harness {
        Harness::None => None,
        Harness::Scripted => Some(format!(
            "{} scripted --case {}",
            shell_quote(&cfg.eval),
            shell_quote(&path.canonicalize()?)
        )),
        Harness::Command(cmd) => Some(cmd.clone()),
    };
    if let Some(command) = command {
        let mut harness = vec![
            cfg.replay_ref.display().to_string(),
            "--cmd".into(),
            command,
            "--hord".into(),
            cfg.hord.display().to_string(),
        ];
        let model = match &cfg.harness {
            Harness::Scripted => Some("scripted".to_owned()),
            _ => cfg.model.clone(),
        };
        if let Some(model) = model {
            harness.extend(["--model".into(), model]);
        }
        #[derive(Serialize)]
        struct ReplayToml {
            harness: Vec<String>,
        }
        std::fs::write(
            dir.join(".hord").join("replay.toml"),
            toml::to_string(&ReplayToml { harness })?,
        )?;
    }

    let log = std::fs::File::create(cfg.work.join(format!("{}.daemon.log", case.id)))?;
    let daemon = Command::new(&cfg.hord)
        .args(["serve", "--repo", dir_text, "--daemon"])
        .current_dir(&dir)
        .env("HORD_DAEMON_IDLE_SECS", "3600")
        .env("HORD_ACTOR", "m5-eval")
        .env_remove("HORD_NO_DAEMON")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .context("start the case's daemon")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let remote = loop {
        if let Ok(remote) = RemoteRepo::connect_local(&dir).await
            && remote.head(proto::HeadRequest {}).await.is_ok()
        {
            break remote;
        }
        if Instant::now() > deadline {
            bail!("the daemon for {} did not answer", case.id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    Ok(CaseRepo {
        dir,
        remote,
        daemon,
    })
}

/// The latest queue entry submitted as `change`.
async fn entry(remote: &RemoteRepo, change: &str) -> Result<Option<proto::QueueEntry>> {
    Ok(remote
        .queue(proto::QueueQuery {
            change: Some(change.into()),
            ..Default::default()
        })
        .await?
        .entries
        .into_iter()
        .rev()
        .find(|e| e.change == change))
}

/// Poll `change`'s entry until `done` holds, for up to `limit`.
async fn wait_for(
    remote: &RemoteRepo,
    change: &str,
    limit: Duration,
    done: impl Fn(&proto::QueueEntry) -> bool,
) -> Result<proto::QueueEntry> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(entry) = entry(remote, change).await?
            && done(&entry)
        {
            return Ok(entry);
        }
        if Instant::now() > deadline {
            let last = entry(remote, change).await?;
            bail!("{change} did not settle in {limit:?}: {last:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Propose `task` from a new workspace (all on the same base).
async fn workspace(remote: &RemoteRepo, who: &str) -> Result<proto::WsNewResponse> {
    Ok(remote
        .workspaces()
        .ws_new(proto::WsNewRequest {
            caller: Some(caller(who)),
            base: None,
            materialize: proto::Materialize::Clone.into(),
        })
        .await?)
}

async fn propose(
    remote: &RemoteRepo,
    ws: &proto::WsNewResponse,
    task: &crate::corpus::Task,
    who: &str,
) -> Result<String> {
    write_files(Path::new(&ws.materialization), &task.writes())?;
    Ok(remote
        .workspaces()
        .propose(proto::ProposeRequest {
            caller: Some(caller(who)),
            workspace: Some(ws.id.clone()),
            intent: task.intent_text()?,
            intent_path: format!("{}.md", task.name),
        })
        .await?
        .change)
}

fn attempt_outcome(outcome: proto::ReplayOutcome) -> &'static str {
    match outcome {
        proto::ReplayOutcome::Running => "running",
        proto::ReplayOutcome::Proposed => "proposed",
        proto::ReplayOutcome::GaveUp => "gave_up",
        proto::ReplayOutcome::Killed => "killed",
        proto::ReplayOutcome::OverBudget => "over_budget",
        proto::ReplayOutcome::Failed => "failed",
        proto::ReplayOutcome::Unspecified => "unknown",
    }
}

/// Budget enforcement (ADR 0028): an attempt that was not killed ended
/// within its wall-clock budget, and no over-budget result was accepted.
fn budget_violations(cfg: &Config, attempts: &[proto::ReplayAttempt]) -> Vec<String> {
    let wall = cfg.wall_time_secs * 1_000;
    let mut out = Vec::new();
    for a in attempts {
        let killed = a.outcome() == proto::ReplayOutcome::Killed;
        if !killed && a.elapsed_ms > wall + WALL_GRACE_MS {
            out.push(format!(
                "attempt {} ran {} ms, past its {wall} ms budget, and was not killed",
                a.attempt, a.elapsed_ms
            ));
        }
        if a.outcome() == proto::ReplayOutcome::Proposed && a.tokens.is_some_and(|t| t > cfg.tokens)
        {
            out.push(format!(
                "attempt {} reported {:?} tokens over the {} budget and was accepted",
                a.attempt, a.tokens, cfg.tokens
            ));
        }
    }
    out
}

/// Both tasks' acceptance tests, as the case defines them, on the landed
/// head (ADR 0029 grading). `Ok(None)` when they pass.
async fn acceptance(remote: &RemoteRepo, case: &Case) -> Result<Option<String>> {
    let ws = workspace(remote, "m5-grader").await?;
    let checkout = PathBuf::from(&ws.materialization);
    let mut args = vec!["test".to_owned(), "--quiet".to_owned()];
    for task in &case.tasks {
        write_files(&checkout, [(&task.test_path, &task.test_source)])?;
        let target = Path::new(&task.test_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("test path {}", task.test_path))?;
        args.extend(["--test".to_owned(), target.to_owned()]);
    }
    let out = Command::new("cargo")
        .args(&args)
        .current_dir(&checkout)
        .stdin(Stdio::null())
        .output()
        .await
        .context("run cargo test")?;
    let _ = remote
        .workspaces()
        .ws_rm(proto::WsRmRequest { id: ws.id })
        .await;
    if out.status.success() {
        return Ok(None);
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let tail: Vec<&str> = text.lines().rev().take(15).collect();
    Ok(Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n")))
}

/// Resolve the parked `change` as the workbench does: a signed `Arbitrate`
/// (keep ours, which always lands), then check that the resolution landed
/// with both colliding changes as parents and that the `Arbitrated` event
/// carries a signature that verifies.
async fn round_trip(remote: &RemoteRepo, change: &str, first_landed: &str) -> Result<String> {
    let id: ChangeId = wire::object_id("change", change)?;
    let key = SigningKey::generate()?;
    let decision = Arbitration::PickOurs;
    let signature = hord_txn::sign_arbitration(id, &decision, &key)?;
    let arbiter = Actor::Human {
        id: "m5-arbiter".into(),
    };
    let reply = remote
        .arbitrate(proto::ArbitrateRequest {
            change: change.into(),
            action: Some(proto::Arbitration {
                action: Some(proto::arbitration::Action::PickOurs(true)),
            }),
            arbiter: Some(wire::actor(&arbiter)),
            note: None,
            key_id: Some(signature.key_id.clone()),
            signature: Some(signature.bytes.as_slice().to_vec()),
        })
        .await?;
    // Until it is arbitrated, or back in the arbitration queue with no
    // resolution pending (the resolution did not land).
    let entry = wait_for(remote, change, Duration::from_secs(300), |e| {
        e.status() == proto::QueueStatus::Arbitrated
            || e.escalation
                .as_ref()
                .is_some_and(|x| x.resolution.is_none())
    })
    .await?;
    if entry.status() != proto::QueueStatus::Arbitrated {
        bail!(
            "the resolution {} did not land: {:?}",
            reply.change,
            entry.escalation.and_then(|e| e.note)
        );
    }
    let landed = entry.landed.ok_or_else(|| anyhow!("no landed id"))?;
    let objects = remote
        .get_objects(proto::GetObjectsRequest {
            ids: vec![landed.clone()],
        })
        .await?;
    let record: ChangeRecord = hord_encoding::decode(
        &objects
            .objects
            .first()
            .ok_or_else(|| anyhow!("resolution {landed} is not stored"))?
            .cbor,
    )?;
    let parents: Vec<String> = record.parents.iter().copied().map(wire::id).collect();
    if !parents.iter().any(|p| p == change) || !parents.iter().any(|p| p == first_landed) {
        bail!("resolution {landed} has parents {parents:?}, not both colliding changes");
    }
    let mut events = remote
        .events(proto::EventsRequest { from: Some(0) })
        .await?;
    let arbitrated = loop {
        let next = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .map_err(|_| anyhow!("no Arbitrated event for {change}"))?
            .ok_or_else(|| anyhow!("the event stream ended"))??;
        if let Some(Kind::Arbitrated(a)) = next.event.and_then(|e| e.kind)
            && a.change == change
        {
            break a;
        }
    };
    if arbitrated.result != landed {
        bail!("Arbitrated names {}, not {landed}", arbitrated.result);
    }
    if arbitrated.by != Some(wire::actor(&arbiter)) {
        bail!("Arbitrated names {:?} as the arbiter", arbitrated.by);
    }
    let stored = Signature {
        key_id: arbitrated
            .key_id
            .ok_or_else(|| anyhow!("Arbitrated is not signed"))?,
        bytes: Bytes::new(
            arbitrated
                .signature
                .ok_or_else(|| anyhow!("Arbitrated has no signature"))?,
        ),
    };
    hord_txn::verify_arbitration(id, &decision, &stored, &key.public())
        .context("the Arbitrated signature does not verify")?;
    Ok(format!("landed {landed} with parents {parents:?}"))
}

/// Run `case` end to end.
pub async fn run_case(cfg: &Config, path: &Path, case: &Case) -> CaseResult {
    let started = Instant::now();
    let mut result = CaseResult {
        id: case.id.clone(),
        kind: case.kind.clone(),
        designed_conflict: case.conflict.clone(),
        observed_conflict: None,
        ambiguous: case.ambiguous,
        intents: [case.first().summary.clone(), case.second().summary.clone()],
        outcome: Outcome::Error {
            why: "not run".into(),
        },
        attempts: Vec::new(),
        budget_violations: Vec::new(),
        summary: None,
        arbitration: None,
        expected_resolved: matches!(cfg.harness, Harness::Scripted).then(|| {
            case.scripted_resolves(usize::try_from(cfg.max_attempts).unwrap_or(usize::MAX))
        }),
        seconds: 0.0,
    };
    let repo = match build(cfg, path, case).await {
        Ok(repo) => repo,
        Err(err) => {
            result.outcome = Outcome::Error {
                why: format!("{err:#}"),
            };
            return result;
        }
    };
    if let Err(err) = drive(cfg, case, &repo.remote, &mut result).await {
        result.outcome = Outcome::Error {
            why: format!("{err:#}"),
        };
    }
    let dir = repo.dir.clone();
    repo.stop().await;
    if !cfg.keep {
        remove_tree(&dir);
    }
    result.seconds = started.elapsed().as_secs_f64();
    result
}

async fn drive(
    cfg: &Config,
    case: &Case,
    remote: &RemoteRepo,
    result: &mut CaseResult,
) -> Result<()> {
    let ws_a = workspace(remote, "agent-a").await?;
    let ws_b = workspace(remote, "agent-b").await?;
    let a = propose(remote, &ws_a, case.first(), "agent-a").await?;
    let b = propose(remote, &ws_b, case.second(), "agent-b").await?;
    let settled = |e: &proto::QueueEntry| {
        !matches!(
            e.status(),
            proto::QueueStatus::Queued
                | proto::QueueStatus::Replaying
                | proto::QueueStatus::Unspecified
        )
    };
    remote
        .submit(proto::SubmitRequest { change: a.clone() })
        .await?;
    let first = wait_for(remote, &a, Duration::from_secs(300), settled).await?;
    if first.status() != proto::QueueStatus::Landed {
        result.outcome = Outcome::Invalid {
            why: format!(
                "the first task did not land: {:?} {:?}",
                first.status(),
                first.reason
            ),
        };
        return Ok(());
    }
    let first_landed = first.landed.unwrap_or_else(|| a.clone());
    remote
        .submit(proto::SubmitRequest { change: b.clone() })
        .await?;
    let limit = Duration::from_secs(300 + 2 * cfg.max_attempts * cfg.wall_time_secs);
    let second = wait_for(remote, &b, limit, settled).await?;
    if let Some(report) = &second.report {
        result.observed_conflict = Some(
            if report
                .merge
                .iter()
                .any(|m| m.severity() == proto::MergeSeverity::Hard)
            {
                "hard".into()
            } else if report.verification.is_some() {
                "semantic".into()
            } else {
                "none".into()
            },
        );
    }
    let escalation = second.escalation.clone().unwrap_or_default();
    result.attempts = escalation
        .attempts
        .iter()
        .map(|a| Attempt {
            attempt: a.attempt,
            outcome: attempt_outcome(a.outcome()).into(),
            elapsed_ms: a.elapsed_ms,
            tokens: a.tokens,
            detail: a.detail.clone(),
        })
        .collect();
    result.budget_violations = budget_violations(cfg, &escalation.attempts);
    match second.status() {
        proto::QueueStatus::Landed => {
            result.outcome = Outcome::Invalid {
                why: "the second task landed without a conflict".into(),
            };
        }
        proto::QueueStatus::Replayed => {
            result.outcome = match acceptance(remote, case).await? {
                None => Outcome::ResolvedByReplay,
                Some(why) => Outcome::ReplayFailedAcceptance { why },
            };
        }
        proto::QueueStatus::NeedsArbitration | proto::QueueStatus::Conflicted => {
            result.outcome = Outcome::Parked;
            result.summary = escalation.summary.map(|s| s.text);
            result.arbitration = Some(match round_trip(remote, &b, &first_landed).await {
                Ok(detail) => RoundTrip { ok: true, detail },
                Err(err) => RoundTrip {
                    ok: false,
                    detail: format!("{err:#}"),
                },
            });
        }
        other => {
            result.outcome = Outcome::Error {
                why: format!("the second task ended {other:?}: {:?}", second.reason),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            hord: PathBuf::new(),
            replay_ref: PathBuf::new(),
            eval: PathBuf::new(),
            harness: Harness::Scripted,
            model: None,
            wall_time_secs: 10,
            tokens: 100,
            max_attempts: 2,
            work: PathBuf::new(),
            keep: false,
        }
    }

    fn attempt(
        outcome: proto::ReplayOutcome,
        elapsed_ms: u64,
        tokens: u64,
    ) -> proto::ReplayAttempt {
        proto::ReplayAttempt {
            attempt: 1,
            outcome: outcome.into(),
            elapsed_ms,
            tokens: Some(tokens),
            ..Default::default()
        }
    }

    /// An attempt past its wall-clock budget must have been killed, and an
    /// over-budget result must not have been accepted (ADR 0028).
    #[test]
    fn budget_violations_are_found() {
        let cfg = cfg();
        let fine = [
            attempt(proto::ReplayOutcome::Proposed, 9_000, 50),
            attempt(proto::ReplayOutcome::Killed, 10_050, 0),
            attempt(proto::ReplayOutcome::OverBudget, 1_000, 500),
        ];
        assert!(budget_violations(&cfg, &fine).is_empty());
        let late = [attempt(proto::ReplayOutcome::GaveUp, 12_000, 0)];
        assert_eq!(budget_violations(&cfg, &late).len(), 1);
        let greedy = [attempt(proto::ReplayOutcome::Proposed, 1_000, 500)];
        assert_eq!(budget_violations(&cfg, &greedy).len(), 1);
    }
}
