//! `hord policy check [-w <ws>] [--policy <file>]`: dry-run policy against
//! a workspace's current proposal (spec §7.2, §10.2, ADR 0026).
//!
//! Nothing is stored. The command previews the change `propose` would
//! record, gathers its [`ChangeFacts`](hord_policy::ChangeFacts), and prints
//! the [`Decision`]. It exits 1 on deny.
//!
//! The policy is head's `.hord-policy.toml`, as the lander reads it: the
//! landing base judges a change, never the change's own result. `--policy`
//! evaluates a local file instead, for trying a policy edit out.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hord_api::proto;
use hord_core::Actor;
use hord_policy::{ActorClass, CompiledPolicy, Decision, EvidenceFact, POLICY_PATH};
use hord_txn::Repo;
use serde::{Deserialize, Serialize};

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on};
use crate::workspaces::this_caller;

/// The `hord policy check --json` document (hord-policy's shapes, ADR
/// 0026). It travels as `PolicyCheckResponse.result_json`.
#[derive(Debug, Serialize, Deserialize)]
struct CheckResult {
    workspace: String,
    /// Where the policy came from: `head`, `file`, or `default` (head has
    /// no policy file, so only the `[land]` defaults apply).
    policy_source: String,
    /// The policy file: `.hord-policy.toml` or the `--policy` path.
    policy: String,
    /// `false` when the workspace has nothing to propose; no decision then.
    changes: bool,
    /// The proposal's result snapshot, whose evidence is checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    actor: ActorClass,
    write_set_size: usize,
    /// Evidence indexed for the proposal's result snapshot (ADR 0025).
    evidence: Vec<EvidenceFact>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    decision: Option<Decision>,
}

/// The library entry point: evaluate `policy` (a file's name and text),
/// else head's policy, against the proposal of `workspace` in `repo`, as
/// `actor`. Returns the `--json` document and whether the policy denies.
/// Runs in the repository's daemon, or in this process with
/// `--no-daemon` or against a remote.
pub fn check(
    repo: &Repo,
    workspace: Option<&str>,
    policy: Option<(&str, &str)>,
    actor: Actor,
    session: Option<String>,
) -> Result<(serde_json::Value, bool)> {
    let meta = crate::repo::resolve_workspace(repo.store(), workspace)?;
    let (policy, policy_source, policy_name) = match policy {
        Some((name, text)) => (parse(text, name)?, "file", name.to_owned()),
        None => {
            let (policy, source) = block_on(repo.head_policy())?;
            (policy, source.as_str(), POLICY_PATH.to_owned())
        }
    };
    let mut ws = block_on(repo.open_workspace(meta.id, actor, session))?;
    let actor = ActorClass::from(ws.actor());
    let record = txn::preview(&mut ws, "(policy check preview)")?;
    let mut result = CheckResult {
        workspace: meta.id.to_string(),
        policy_source: policy_source.to_owned(),
        policy: policy_name,
        changes: record.is_some(),
        snapshot: record.as_ref().map(|r| txn::hex(r.result)),
        actor,
        write_set_size: 0,
        evidence: Vec::new(),
        decision: None,
    };
    if let Some(record) = record {
        // ADR 0026 amendment: a policy edit that does not parse would be
        // rejected at landing; report it now.
        block_on(repo.check_policy_file(&record))?;
        let facts = block_on(repo.policy_facts(record))?;
        result.write_set_size = facts.write_set_len;
        result.evidence = facts.evidence.clone();
        result.decision = Some(policy.evaluate(&facts));
    }
    let deny = result.decision.as_ref().is_some_and(|d| !d.is_allow());
    Ok((serde_json::to_value(&result)?, deny))
}

pub fn run_check(
    json: bool,
    target: &Target,
    workspace: Option<String>,
    policy_file: Option<PathBuf>,
) -> Result<()> {
    let policy = match &policy_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading policy {}", path.display()))?,
        ),
        None => None,
    };
    let session = Session::open(target)?;
    let reply = block_on(
        session
            .workspaces()
            .policy_check(proto::PolicyCheckRequest {
                caller: Some(this_caller()),
                workspace,
                policy_path: policy_file.map(|p| p.display().to_string()),
                policy,
            }),
    )?;
    let result: CheckResult =
        serde_json::from_str(&reply.result_json).context("policy check result")?;
    if json {
        output::print_json(&result)?;
    } else {
        print_text(&result);
    }
    if reply.deny {
        std::process::exit(1);
    }
    Ok(())
}

fn parse(source: &str, name: &str) -> Result<CompiledPolicy> {
    hord_policy::parse(source).map_err(|e| anyhow::anyhow!("{name}:{e}"))
}

fn print_text(result: &CheckResult) {
    println!("workspace {}", result.workspace);
    println!("policy {} ({})", result.policy, result.policy_source);
    let Some(decision) = &result.decision else {
        println!("no changes");
        return;
    };
    println!(
        "actor {}, write set {}, evidence {}",
        result.actor.as_str(),
        result.write_set_size,
        result.evidence.len()
    );
    let Decision::Deny { reasons } = decision else {
        println!("allow");
        return;
    };
    println!("deny");
    for reason in reasons {
        let source = match &reason.rule {
            Some(rule) => format!("rule {rule:?}"),
            None => match reason.source {
                hord_policy::ViolationSource::MaxWriteSet => "[land] max_write_set".into(),
                _ => "[land] require".into(),
            },
        };
        let evidence = match reason.evidence {
            hord_policy::EvidenceState::Absent => "missing",
            hord_policy::EvidenceState::Failed => "failed",
            hord_policy::EvidenceState::Skipped => "skipped",
        };
        println!("  {source}: {} {evidence}", reason.requirement);
        for trigger in &reason.triggers {
            let text = match trigger {
                hord_policy::Trigger::Definition { node, path } => format!("{node} in {path}"),
                hord_policy::Trigger::Path { path } => path.clone(),
                hord_policy::Trigger::Actor { actor } => format!("actor {}", actor.as_str()),
                hord_policy::Trigger::WriteSet { size, limit } => {
                    format!("write set {size} > {limit}")
                }
            };
            println!("    triggered by {text}");
        }
    }
}
