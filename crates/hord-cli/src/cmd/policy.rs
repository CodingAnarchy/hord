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

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hord_core::{ChangeRecord, Evidence, Intent, NodeId, Op, RepoPath};
use hord_lang::AdapterRegistry;
use hord_policy::{
    ActorClass, CompiledPolicy, Decision, EvidenceFact, Facts, POLICY_PATH, TouchedDefinition,
};
use hord_txn::{Base, BeginOptions, DefinitionInfo, Repo, Workspace};
use serde::Serialize;

use crate::txn::{self, block_on};
use crate::{output, repo};

#[derive(Debug, Serialize)]
struct CheckResult {
    workspace: String,
    /// Where the policy came from: `head`, `file`, or `default` (head has
    /// no policy file, so only the `[land]` defaults apply).
    policy_source: &'static str,
    /// The policy file: `.hord-policy.toml` or the `--policy` path.
    policy: String,
    /// `false` when the workspace has nothing to propose; no decision then.
    changes: bool,
    /// The proposal's result snapshot, whose evidence is checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    actor: ActorClass,
    write_set_size: usize,
    /// Evidence indexed for the proposal's result snapshot (ADR 0025).
    evidence: Vec<EvidenceFact>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    decision: Option<Decision>,
}

pub fn run_check(
    json: bool,
    workspace: Option<String>,
    policy_file: Option<PathBuf>,
) -> Result<()> {
    let store = repo::discover()?;
    let meta = repo::resolve_workspace(&store, workspace.as_deref())?;
    let repo = txn::open_store(store)?;
    let (policy, policy_source, policy_name) = match policy_file {
        Some(path) => {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("reading policy {}", path.display()))?;
            let name = path.display().to_string();
            (parse(&source, &name)?, "file", name)
        }
        None => {
            let (policy, source) = head_policy(&repo)?;
            (policy, source, POLICY_PATH.to_owned())
        }
    };
    let mut ws = block_on(repo.open_workspace(meta.id, txn::actor(), txn::session()))?;
    let actor = ActorClass::from(ws.actor());
    let preview = Intent {
        summary: "(policy check preview)".into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    };
    let record = match block_on(ws.preview(preview)) {
        Ok(proposal) => Some(proposal.record),
        Err(hord_txn::Error::NothingToPropose) => None,
        Err(err) => return Err(err.into()),
    };
    let mut result = CheckResult {
        workspace: meta.id.to_string(),
        policy_source,
        policy: policy_name,
        changes: record.is_some(),
        snapshot: record.as_ref().map(|r| txn::hex(r.result)),
        actor,
        write_set_size: 0,
        evidence: Vec::new(),
        decision: None,
    };
    if let Some(record) = &record {
        let facts = facts(&repo, &mut ws, record, actor)?;
        result.write_set_size = facts.write_set_len;
        result.evidence = facts.evidence.clone();
        result.decision = Some(policy.evaluate(&facts));
    }
    let deny = result.decision.as_ref().is_some_and(|d| !d.is_allow());
    if json {
        output::print_json(&result)?;
    } else {
        print_text(&result);
    }
    if deny {
        std::process::exit(1);
    }
    Ok(())
}

fn parse(source: &str, name: &str) -> Result<CompiledPolicy> {
    hord_policy::parse(source).map_err(|e| anyhow::anyhow!("{name}:{e}"))
}

/// Head's `.hord-policy.toml`, or the defaults when head has none.
fn head_policy(repo: &Repo) -> Result<(CompiledPolicy, &'static str)> {
    let mut head = block_on(repo.begin(BeginOptions::at_head(txn::actor())))?;
    let path: RepoPath = POLICY_PATH.parse()?;
    match block_on(head.read_file(&path))? {
        Some(bytes) => {
            let source = std::str::from_utf8(&bytes)
                .with_context(|| format!("{POLICY_PATH} at head is not UTF-8"))?;
            Ok((parse(source, POLICY_PATH)?, "head"))
        }
        None => Ok((CompiledPolicy::default(), "default")),
    }
}

/// Evidence indexed for `record`'s result snapshot (ADR 0025).
fn indexed_evidence(repo: &Repo, record: &ChangeRecord) -> Result<Vec<EvidenceFact>> {
    let store = repo.store();
    let mut facts = Vec::new();
    for id in store.evidence_at(record.result)? {
        let evidence: Evidence = store.get_object(id)?;
        facts.extend(EvidenceFact::of(&evidence));
    }
    Ok(facts)
}

/// Facts for `record`: its write set, the files it touches, and the
/// write-set definitions in those files with the adapter's kinds and
/// visibility (ADR 0026). A definition is read from the workspace, or from
/// the base when the change deletes it.
fn facts(
    repo: &Repo,
    ws: &mut Workspace,
    record: &ChangeRecord,
    actor: ActorClass,
) -> Result<Facts> {
    let mut paths: BTreeSet<RepoPath> = ws.access_log().written_paths.clone();
    for op in &record.ops {
        if let Op::Blob { path, .. } | Op::Tree { path, .. } = op {
            paths.insert(path.clone());
        }
    }
    let adapters = hord_txn::default_adapters();
    let mut base = block_on(repo.begin(BeginOptions {
        base: Base::Snapshot(ws.base()),
        actor: ws.actor().clone(),
        session: None,
    }))?;
    let mut definitions: BTreeMap<NodeId, TouchedDefinition> = BTreeMap::new();
    for path in &paths {
        for side in [&mut *ws, &mut base] {
            for def in touched_in(side, path, record, &adapters)? {
                definitions.entry(def.node).or_insert(def);
            }
        }
    }
    Ok(Facts {
        actor,
        write_set_len: record.write_set.len(),
        definitions: definitions.into_values().collect(),
        paths: paths.into_iter().collect(),
        evidence: indexed_evidence(repo, record)?,
    })
}

/// Definitions of `path` in `ws` that are in `record`'s write set.
fn touched_in(
    ws: &mut Workspace,
    path: &RepoPath,
    record: &ChangeRecord,
    adapters: &AdapterRegistry,
) -> Result<Vec<TouchedDefinition>> {
    let defs: Vec<DefinitionInfo> = match block_on(ws.definitions(path)) {
        Ok(defs) => defs
            .into_iter()
            .filter(|d| record.write_set.contains(&d.node))
            .collect(),
        Err(hord_txn::Error::MissingFile(_) | hord_txn::Error::NotParsed(_)) => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    if defs.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = block_on(ws.read_file(path))?.unwrap_or_default();
    let bytes = bytes.as_slice();
    let spans: Vec<Range<usize>> = defs.iter().map(|d| d.span.clone()).collect();
    let head = &bytes[..bytes.len().min(512)];
    let adapter_facts = match adapters.get(path, head) {
        Some(adapter) => adapter.definition_facts(bytes, &spans),
        None => vec![hord_lang::DefinitionFacts::default(); spans.len()],
    };
    Ok(defs
        .into_iter()
        .zip(adapter_facts)
        .map(|(def, facts)| {
            let mut kinds = facts.inner_kinds;
            kinds.insert(def.kind.as_str().to_owned());
            TouchedDefinition {
                node: def.node,
                path: def.path,
                kinds,
                visibility: facts.visibility,
            }
        })
        .collect())
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
