//! Glue between the sync command layer and the async `hord-txn` API, plus
//! JSON views of its types (ids as hex/ULID text, not byte arrays).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use anyhow::{Context, Result, anyhow};
use hord_core::{Actor, Bytes, ChangeId, ChangeRecord, NodeId, ObjectId, Op, RepoPath};
use hord_store::Store;
use hord_txn::{
    ConflictKind, ConflictReport, MergeSeverity, QueueEntry, QueueStatus, Repo, RepoOptions,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::repo;

/// Run `future` to completion from a command (which runs on tokio's
/// blocking pool, so blocking on the runtime here is allowed).
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(future)
}

/// Open the discovered store as a [`Repo`].
pub fn open() -> Result<Repo> {
    open_store(repo::discover()?)
}

pub fn open_store(store: Store) -> Result<Repo> {
    Ok(block_on(Repo::from_store(store, RepoOptions::default()))?)
}

/// Who is running the command: `HORD_ACTOR` (else `USER`), recorded as an
/// agent when `HORD_AGENT_MODEL` is set (with `HORD_AGENT_HARNESS`).
pub fn actor() -> Actor {
    let id = std::env::var("HORD_ACTOR")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into());
    match std::env::var("HORD_AGENT_MODEL") {
        Ok(model) => Actor::Agent {
            id,
            model,
            model_hash: Bytes::default(),
            harness: std::env::var("HORD_AGENT_HARNESS").unwrap_or_else(|_| "hord-cli".into()),
        },
        Err(_) => Actor::Human { id },
    }
}

/// Harness session id from `HORD_SESSION`, if set.
pub fn session() -> Option<String> {
    std::env::var("HORD_SESSION").ok()
}

pub fn parse_change(spec: &str) -> Result<ChangeId> {
    spec.parse::<ObjectId>()
        .map_err(|_| anyhow!("{spec:?} is not a change id (64 hex digits)"))
}

pub fn hex(id: ObjectId) -> String {
    id.to_hex()
}

pub fn short(id: ObjectId) -> String {
    id.to_hex()[..12].to_owned()
}

pub fn status_name(status: &QueueStatus) -> &'static str {
    match status {
        QueueStatus::Queued => "queued",
        QueueStatus::Landed { .. } => "landed",
        QueueStatus::Conflicted => "conflicted",
        QueueStatus::Rejected { .. } => "rejected",
    }
}

pub fn kind_name(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::WriteWrite => "write-write",
        ConflictKind::ReadWrite => "read-write",
        ConflictKind::WriteRead => "write-read",
    }
}

/// One queue entry for `--json` and text output.
#[derive(Debug, Serialize)]
pub struct EntryView {
    pub seq: u64,
    pub change: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub summary: String,
    pub actor: String,
    pub submitted_at: u64,
    pub updated_at: u64,
    pub conflicts: usize,
    pub hard: bool,
}

pub fn entry_view(entry: &QueueEntry, record: Option<&ChangeRecord>) -> EntryView {
    let (landed, reason) = match &entry.status {
        QueueStatus::Landed { landed } => (Some(hex(*landed)), None),
        QueueStatus::Rejected { reason } => (None, Some(reason.clone())),
        QueueStatus::Queued | QueueStatus::Conflicted => (None, None),
    };
    EntryView {
        seq: entry.seq,
        change: hex(entry.change),
        status: status_name(&entry.status),
        landed,
        reason,
        summary: record.map(|r| r.intent.summary.clone()).unwrap_or_default(),
        actor: record
            .map(|r| crate::resolve::actor_id(&r.provenance.actor).to_owned())
            .unwrap_or_default(),
        submitted_at: entry.submitted_at.as_millis(),
        updated_at: entry.updated_at.as_millis(),
        conflicts: entry
            .report
            .as_ref()
            .map_or(0, |r| r.conflicts.len() + r.merge.len()),
        hard: entry.report.as_ref().is_some_and(ConflictReport::has_hard),
    }
}

pub fn print_entry(view: &EntryView) {
    let mut line = format!(
        "{:>4} {} {:<10} {}",
        view.seq,
        &view.change[..12],
        view.status,
        view.summary
    );
    if let Some(landed) = &view.landed
        && *landed != view.change
    {
        line.push_str(&format!(" (landed as {})", &landed[..12]));
    }
    if let Some(reason) = &view.reason {
        line.push_str(&format!(" ({reason})"));
    }
    if view.conflicts > 0 {
        line.push_str(&format!(
            " [{} conflict{}{}]",
            view.conflicts,
            if view.conflicts == 1 { "" } else { "s" },
            if view.hard { ", hard" } else { "" }
        ));
    }
    println!("{line}");
}

/// A definition id with its name and file, when they could be found.
#[derive(Clone, Debug, Serialize)]
pub struct NodeView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl NodeView {
    pub fn text(&self) -> String {
        match (&self.name, &self.path) {
            (Some(name), Some(path)) => format!("{name} ({path})"),
            (None, Some(path)) => format!("{} ({path})", self.id),
            _ => self.id.clone(),
        }
    }
}

/// Names for definitions in the files these records touch, looked up in
/// each record's base and result.
pub struct Names {
    known: BTreeMap<NodeId, (Option<String>, RepoPath)>,
}

impl Names {
    pub fn for_records(repo: &Repo, records: &[&ChangeRecord]) -> Result<Self> {
        let mut paths = BTreeSet::new();
        let mut snapshots = BTreeSet::new();
        for record in records {
            snapshots.insert(record.base);
            snapshots.insert(record.result);
            paths.extend(block_on(repo.changed_paths(record.base, record.result))?);
        }
        let mut known = BTreeMap::new();
        // The file root id (ADR 0015) is the whole-file and glue identity.
        for path in &paths {
            known.insert(
                hord_txn::path_node_id(path),
                (Some("(file)".into()), path.clone()),
            );
        }
        for snapshot in snapshots {
            for path in &paths {
                let defs = block_on(repo.definitions_in(snapshot, path.clone()))?;
                for def in defs {
                    known
                        .entry(def.node)
                        .or_insert_with(|| (def.name.map(|n| n.as_str().to_owned()), def.path));
                }
            }
        }
        Ok(Self { known })
    }

    pub fn view(&self, node: NodeId) -> NodeView {
        match self.known.get(&node) {
            Some((name, path)) => NodeView {
                id: node.to_string(),
                name: name.clone(),
                path: Some(path.to_string()),
            },
            None => NodeView {
                id: node.to_string(),
                name: None,
                path: None,
            },
        }
    }
}

/// JSON form of an [`Op`] with readable ids.
pub fn op_view(op: &Op) -> Value {
    let oid = |id: &ObjectId| hex(*id);
    let opt = |id: &Option<ObjectId>| id.map(hex);
    match op {
        Op::Insert {
            parent,
            index,
            node,
        } => {
            json!({ "op": "insert", "parent": parent.to_string(), "index": index, "node": oid(node) })
        }
        Op::Delete { node } => json!({ "op": "delete", "node": node.to_string() }),
        Op::Replace { node, from, to } => {
            json!({ "op": "replace", "node": node.to_string(), "from": oid(from), "to": oid(to) })
        }
        Op::Move {
            node,
            from_parent,
            to_parent,
            index,
        } => json!({
            "op": "move", "node": node.to_string(), "from_parent": from_parent.to_string(),
            "to_parent": to_parent.to_string(), "index": index
        }),
        Op::Rename { node, from, to } => json!({
            "op": "rename", "node": node.to_string(), "from": from.as_str(), "to": to.as_str()
        }),
        Op::Blob { path, from, to } => {
            json!({ "op": "blob", "path": path.to_string(), "from": opt(from), "to": opt(to) })
        }
        Op::Tree { path, kind } => {
            json!({ "op": "tree", "path": path.to_string(), "kind": format!("{kind:?}") })
        }
    }
}

/// One-line text form of an [`Op`].
pub fn op_text(op: &Op, names: &Names) -> String {
    match op {
        Op::Insert { node, .. } => format!("insert {}", short(*node)),
        Op::Delete { node } => format!("delete {}", names.view(*node).text()),
        Op::Replace { node, .. } => format!("replace {}", names.view(*node).text()),
        Op::Move { node, .. } => format!("move {}", names.view(*node).text()),
        Op::Rename { from, to, .. } => format!("rename {from} -> {to}"),
        Op::Blob { path, from, to } => match (from, to) {
            (None, Some(_)) => format!("create {path}"),
            (Some(_), None) => format!("delete {path}"),
            _ => format!("modify {path}"),
        },
        Op::Tree { path, kind } => format!("tree {kind:?} {path}"),
    }
}

/// Snapshot of a change, for commands that print one.
pub fn load(repo: &Repo, change: ChangeId) -> Result<ChangeRecord> {
    block_on(repo.change(change)).with_context(|| format!("load change {}", hex(change)))
}

/// `ConflictReport` explained with names, for `hord conflicts`.
#[derive(Debug, Serialize)]
pub struct ReportView {
    pub change: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed: Option<String>,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub checked_against: Vec<String>,
    pub strict_reads: bool,
    pub clean: bool,
    pub conflicts: Vec<SetConflictView>,
    pub merge: Vec<MergeView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetConflictView {
    pub kind: &'static str,
    pub landed: String,
    pub landed_summary: String,
    pub nodes: Vec<NodeView>,
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MergeView {
    pub path: String,
    pub severity: &'static str,
    pub nodes: Vec<NodeView>,
    pub reason: String,
}

pub fn report_view(
    repo: &Repo,
    report: &ConflictReport,
    entry: Option<&QueueEntry>,
) -> Result<ReportView> {
    let change = load(repo, report.change)?;
    let mut landed_records = BTreeMap::new();
    for conflict in &report.conflicts {
        if let std::collections::btree_map::Entry::Vacant(slot) =
            landed_records.entry(conflict.landed)
        {
            slot.insert(load(repo, conflict.landed)?);
        }
    }
    let mut records: Vec<&ChangeRecord> = vec![&change];
    records.extend(landed_records.values());
    let names = Names::for_records(repo, &records)?;
    let conflicts = report
        .conflicts
        .iter()
        .map(|c| SetConflictView {
            kind: kind_name(c.kind),
            landed: hex(c.landed),
            landed_summary: landed_records
                .get(&c.landed)
                .map(|r| r.intent.summary.clone())
                .unwrap_or_default(),
            nodes: c.nodes.iter().map(|n| names.view(*n)).collect(),
            paths: c.paths.iter().map(ToString::to_string).collect(),
        })
        .collect();
    let merge = report
        .merge
        .iter()
        .map(|m| MergeView {
            path: m.path.to_string(),
            severity: match m.severity {
                MergeSeverity::Hard => "hard",
                MergeSeverity::Soft => "soft",
            },
            nodes: m.nodes.iter().map(|n| names.view(*n)).collect(),
            reason: m.reason.clone(),
        })
        .collect();
    Ok(ReportView {
        change: hex(report.change),
        status: entry.map(|e| status_name(&e.status)),
        landed: entry.and_then(|e| match e.status {
            QueueStatus::Landed { landed } => Some(hex(landed)),
            _ => None,
        }),
        base: hex(report.base),
        head: report.head.map(hex),
        checked_against: report.checked_against.iter().map(|c| hex(*c)).collect(),
        strict_reads: report.strict_reads,
        clean: report.is_clean(),
        conflicts,
        merge,
        verification: report.verification.clone(),
    })
}

pub fn print_report(view: &ReportView) {
    let status = view.status.unwrap_or("not submitted");
    println!("change {} ({status})", &view.change[..12]);
    println!(
        "checked against {} landed change{} since its base",
        view.checked_against.len(),
        if view.checked_against.len() == 1 {
            ""
        } else {
            "s"
        }
    );
    if view.clean {
        println!("no conflicts");
    }
    for c in &view.conflicts {
        let mut what: Vec<String> = c.nodes.iter().map(NodeView::text).collect();
        what.extend(c.paths.iter().map(|p| format!("file {p}")));
        println!(
            "{} with {} \"{}\": {}",
            c.kind,
            &c.landed[..12],
            c.landed_summary,
            what.join(", ")
        );
    }
    for m in &view.merge {
        let nodes: Vec<String> = m.nodes.iter().map(NodeView::text).collect();
        let nodes = if nodes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", nodes.join(", "))
        };
        println!("merge {} {}: {}{nodes}", m.severity, m.path, m.reason);
    }
    if let Some(reason) = &view.verification {
        println!("verification failed: {reason}");
    }
    match view.status {
        Some("conflicted") => println!("parked: needs replay (spec §6.4)"),
        Some("landed") if !view.clean => println!("landed, flagged for re-verification"),
        _ => {}
    }
}
