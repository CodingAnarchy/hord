//! Glue between the sync command layer and the async `hord-txn` and
//! `hord-api` APIs: blocking on them, the caller's actor, id parsing, and
//! definition names for display.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use anyhow::{Context, Result, anyhow};
use hord_core::{Actor, Bytes, ChangeId, ChangeRecord, NodeId, ObjectId, Op, RepoPath};
use hord_store::Store;
use hord_txn::{Repo, RepoOptions};
use serde::Serialize;

/// Run `future` to completion from a command (which runs on tokio's
/// blocking pool, so blocking on the runtime here is allowed).
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(future)
}

/// The default options use `FailClosedVerifier`: until M4 verifies,
/// `land --local` lands only changes with a clean conflict report.
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
                NodeId::file_root(path),
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

/// Head's change and result snapshot through a backend; `None` before
/// anything lands.
pub fn backend_head(
    backend: &dyn hord_api::RepoBackend,
) -> Result<Option<(ChangeId, hord_core::SnapshotId)>> {
    let head = block_on(backend.head(hord_api::proto::HeadRequest {}))?;
    let Some(change) = head.change else {
        return Ok(None);
    };
    let change = hord_api::wire::object_id("head", &change)?;
    let record = backend_change(backend, change)?;
    Ok(Some((change, record.result)))
}

/// A change record through a backend.
pub fn backend_change(
    backend: &dyn hord_api::RepoBackend,
    change: ChangeId,
) -> Result<ChangeRecord> {
    let reply = block_on(backend.get_objects(hord_api::proto::GetObjectsRequest {
        ids: vec![hord_api::wire::id(change)],
    }))
    .with_context(|| format!("load change {}", hex(change)))?;
    let object = reply
        .objects
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no change {}", hex(change)))?;
    hord_encoding::decode(&object.cbor).with_context(|| format!("{} is not a change", hex(change)))
}

/// A NodeId, or a qualified name resolved at head through a backend: exact
/// matches, else names ending in `::name`; exactly one must match.
pub fn backend_resolve_node(backend: &dyn hord_api::RepoBackend, spec: &str) -> Result<NodeId> {
    if let Some(id) = crate::resolve::parse_node_id(spec) {
        return Ok(id);
    }
    let Some((_, snapshot)) = backend_head(backend)? else {
        return Err(hord_txn::Error::UnknownName(spec.to_owned()).into());
    };
    let reply = block_on(backend.resolve_name(hord_api::proto::ResolveNameRequest {
        snapshot: hex(snapshot),
        name: spec.to_owned(),
    }))?;
    let nodes: Vec<NodeId> = reply
        .nodes
        .iter()
        .map(|n| hord_api::wire::node_id("node", n))
        .collect::<std::result::Result<_, _>>()?;
    match nodes.as_slice() {
        [] => Err(hord_txn::Error::UnknownName(spec.to_owned()).into()),
        [one] => Ok(*one),
        _ => Err(hord_txn::Error::AmbiguousName {
            name: spec.to_owned(),
            nodes,
        }
        .into()),
    }
}
