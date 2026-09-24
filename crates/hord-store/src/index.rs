//! Rebuildable indexes for the spec §8.1 tables `node_history` and `edges`,
//! the `rebased` index (ADR 0018), and the store format version.
//!
//! Redb is not the only copy of a fact:
//! - `node_history` is derived from landed [`hord_core::ChangeRecord`] objects.
//! - `rebased` (submitted change → the id it landed under) is derived from
//!   the `rebased_from` field of landed records.
//! - each edge is its own content-addressed object.
//!
//! [`Store::rebuild_index`] drops those indexes and fills them from the
//! objects. NodeIds are not here: a snapshot's identity lives in its
//! [`hord_core::Snapshot`] object (ADR 0017), which `hord-txn` reads.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use hord_core::{ChangeId, ChangeRecord, IdentityDelta, NodeId, ObjectId, Op, SnapshotId};
use redb::{Database, ReadableTable, Table, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::{META, Store};
use crate::{Error, Result};

/// Object format this build reads and writes. 2: snapshot ids are
/// [`hord_core::Snapshot`] object ids (ADR 0017). Stores without a recorded
/// format are format 1.
pub(super) const STORE_FORMAT: u32 = 2;
const META_FORMAT: &str = "format";

const INDEX_FACT_FORMAT: u8 = 1;
const PRESENT: &[u8] = &[0];

/// Canonical CBOR prefix of [`IndexFact::Edge`]: a one-entry map keyed by `"Edge"`.
const EDGE_FACT_PREFIX: &[u8] = b"\xa1\x64Edge";

const SNAP_LEN: usize = ObjectId::LEN;
pub(super) const NODE_LEN: usize = 16;
/// `snapshot || kind || source || target`
pub(super) const EDGE_KEY_LEN: usize = SNAP_LEN + 1 + NODE_LEN + NODE_LEN;
const EDGE_PREFIX_LEN: usize = SNAP_LEN + 1 + NODE_LEN;

const NODE_HISTORY: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("node_history");
const EDGES: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("edges");
/// Submitted change id → the id it landed under (ADR 0018).
pub(super) const REBASED: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("rebased");

/// Edge kinds stored per snapshot (spec §3.8).
///
/// The source is the first endpoint in the spec: `a` in `Contains(a, b)` and
/// `References(a, b)`, `t` in `Tests(t, a)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Structural parent/child between definitions.
    Contains,
    /// `source`'s body names `target`.
    References,
    /// Build or type dependency, coarser than [`Self::References`].
    Depends,
    /// Test `source` exercises `target`.
    Tests,
    /// `source` was split from, renamed from, or copied from `target`.
    DerivedFrom,
}

impl EdgeKind {
    fn tag(self) -> u8 {
        match self {
            Self::Contains => 1,
            Self::References => 2,
            Self::Depends => 3,
            Self::Tests => 4,
            Self::DerivedFrom => 5,
        }
    }
}

/// Content-addressed copy of an index fact. The redb rows are a cache of these.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum IndexFact {
    Edge {
        format: u8,
        snapshot: SnapshotId,
        kind: EdgeKind,
        source: NodeId,
        target: NodeId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredEdge {
    snapshot: SnapshotId,
    kind: EdgeKind,
    source: NodeId,
    target: NodeId,
}

pub(super) fn open_tables(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(NODE_HISTORY).map_err(Error::index)?;
    txn.open_table(EDGES).map_err(Error::index)?;
    txn.open_table(REBASED).map_err(Error::index)?;
    Ok(())
}

/// Record [`STORE_FORMAT`] in a new store.
pub(super) fn write_format(txn: &WriteTransaction) -> Result<()> {
    let mut meta = txn.open_table(META).map_err(Error::index)?;
    meta.insert(META_FORMAT, STORE_FORMAT.to_be_bytes().as_slice())
        .map_err(Error::index)?;
    Ok(())
}

/// Refuse a store whose format is not [`STORE_FORMAT`] (ADR 0017: snapshot
/// ids changed, and there is no migration), then create missing tables.
pub(super) fn ensure_tables(db: &Database) -> Result<()> {
    let found = {
        let txn = db.begin_read().map_err(Error::index)?;
        match txn.open_table(META) {
            Ok(meta) => match meta.get(META_FORMAT).map_err(Error::index)? {
                Some(value) => {
                    let bytes: [u8; 4] = value
                        .value()
                        .try_into()
                        .map_err(|_| Error::CorruptIndex("format"))?;
                    Some(u32::from_be_bytes(bytes))
                }
                None => None,
            },
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(Error::index(err)),
        }
    };
    if found != Some(STORE_FORMAT) {
        return Err(Error::StoreFormat {
            found,
            expected: STORE_FORMAT,
        });
    }
    let txn = db.begin_write().map_err(Error::index)?;
    open_tables(&txn)?;
    txn.commit().map_err(Error::index)?;
    Ok(())
}

impl Store {
    /// Record one edge in `snapshot`.
    ///
    /// Writes a content-addressed object (the fact) and updates the redb cache.
    /// Putting the same endpoints and kind again returns the same [`ObjectId`]
    /// and does not duplicate [`Self::edges`] results.
    pub fn put_edge(
        &self,
        snapshot: SnapshotId,
        kind: EdgeKind,
        source: NodeId,
        target: NodeId,
    ) -> Result<ObjectId> {
        let _guard = self.lock_index();
        let key = edge_key(snapshot, kind, source, target);
        if super::lock(&self.seen_edges).contains(&key) {
            let bytes = hord_encoding::encode(&IndexFact::Edge {
                format: INDEX_FACT_FORMAT,
                snapshot,
                kind,
                source,
                target,
            })?;
            return Ok(ObjectId::from_canonical(&bytes));
        }
        let id = self.put_object(&IndexFact::Edge {
            format: INDEX_FACT_FORMAT,
            snapshot,
            kind,
            source,
            target,
        })?;
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(EDGES).map_err(Error::index)?;
            table
                .insert(key.as_slice(), PRESENT)
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        super::lock(&self.seen_edges).insert(key);
        Ok(id)
    }

    /// Targets of `kind` edges leaving `source` in `snapshot`, in [`NodeId`] order.
    ///
    /// Empty when nothing was recorded for that source and kind.
    pub fn edges(
        &self,
        snapshot: SnapshotId,
        source: NodeId,
        kind: EdgeKind,
    ) -> Result<Vec<NodeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(EDGES).map_err(Error::index)?;
        let (start, end) = edge_bounds(snapshot, kind, source);
        let mut targets = Vec::new();
        for entry in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(Error::index)?
        {
            let (key, _) = entry.map_err(Error::index)?;
            let key = key.value();
            if key.len() != EDGE_KEY_LEN || key[..EDGE_PREFIX_LEN] != start[..EDGE_PREFIX_LEN] {
                return Err(Error::CorruptIndex("edges"));
            }
            targets.push(node_from_bytes(&key[EDGE_PREFIX_LEN..])?);
        }
        // The target is the last key field, big-endian, so the range is already
        // in NodeId order.
        Ok(targets)
    }

    /// Landed [`ChangeId`]s that touched `node`, in landing order.
    ///
    /// A change touches a node when that [`NodeId`] is in its `write_set`, in an
    /// [`Op`] field, or in an [`IdentityDelta`]. `read_set` does not count. The
    /// list is empty until [`Self::index_change`] or [`Self::rebuild_index`].
    /// Both only copy facts out of stored [`ChangeRecord`]s.
    pub fn node_history(&self, node: NodeId) -> Result<Vec<ChangeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(NODE_HISTORY).map_err(Error::index)?;
        let key = node_key(node);
        match table.get(key.as_slice()).map_err(Error::index)? {
            Some(value) => decode_change_ids(value.value()),
            None => Ok(Vec::new()),
        }
    }

    /// Copy one landed change into `node_history`.
    ///
    /// `change` must already be stored as a [`ChangeRecord`] and be present in
    /// the landing log. Indexing it again is a no-op. Rows stay in landing
    /// order even when changes are indexed out of order.
    pub fn index_change(&self, change: ChangeId) -> Result<()> {
        let _guard = self.lock_index();
        self.flush()?;
        if !self.ensure_landing_log()?.first_pos.contains_key(&change) {
            return Err(Error::NotInLog(change));
        }
        // Read the record without the landing-log lock held; `append_log`
        // needs that lock. The log is append-only, so `change` stays in it.
        let bytes = self.get(change)?;
        let record: ChangeRecord = hord_encoding::decode(&bytes)?;
        self.insert_history(change, &touched_nodes(&record))
    }

    /// The id `submitted` landed under when the lander rebased it
    /// (ADR 0018), from the landed record's `rebased_from`.
    pub fn rebased_to(&self, submitted: ChangeId) -> Result<Option<ChangeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(REBASED).map_err(Error::index)?;
        match table
            .get(submitted.as_bytes().as_slice())
            .map_err(Error::index)?
        {
            Some(value) => Ok(Some(ObjectId::try_from(value.value())?)),
            None => Ok(None),
        }
    }

    /// Replace `node_history`, `edges`, and `rebased` from stored objects.
    ///
    /// Landing-log entries that are not [`ChangeRecord`]s are skipped. A log
    /// entry with no stored object is an error, and the previous index is
    /// left in place. Edge rows come from the objects [`Self::put_edge`]
    /// writes.
    pub fn rebuild_index(&self) -> Result<()> {
        let _guard = self.lock_index();
        self.flush()?;
        let (history, rebased) = self.history_from_log()?;
        let edges = self.scan_edges()?;
        self.write_rebuilt_index(&history, &edges, &rebased)
    }

    pub(super) fn lock_index(&self) -> std::sync::MutexGuard<'_, ()> {
        self.index_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn insert_history(&self, change: ChangeId, nodes: &BTreeSet<NodeId>) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        // `index_change` holds `index_lock`, so these rows cannot change before
        // the write below. A no-op reindex then skips the durable commit. The
        // landing-log lock covers only the planning, not the commit.
        let mut updates = Vec::new();
        {
            let log = self.ensure_landing_log()?;
            let pos_of = |id: &ChangeId| log.first_pos.get(id).copied();
            let txn = self.db.begin_read().map_err(Error::index)?;
            let table = txn.open_table(NODE_HISTORY).map_err(Error::index)?;
            for node in nodes {
                let key = node_key(*node);
                let existing = table
                    .get(key.as_slice())
                    .map_err(Error::index)?
                    .map(|value| value.value().to_vec());
                if let Some(encoded) = history_value(existing.as_deref(), change, &pos_of)? {
                    updates.push((key, encoded));
                }
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(Error::index)?;
        write_history_rows(&txn, &updates)?;
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// `node_history` rows and `rebased` pairs of every landed record.
    fn history_from_log(&self) -> Result<RebuiltHistory> {
        let log = self.log()?;
        let mut history: BTreeMap<NodeId, Vec<ChangeId>> = BTreeMap::new();
        let mut rebased = BTreeMap::new();
        let mut seen = HashSet::with_capacity(log.len());
        for change in log {
            // A repeated ChangeId is the same record. One pass records every touch.
            if !seen.insert(change) {
                continue;
            }
            let bytes = self.get(change)?;
            let Ok(record) = hord_encoding::decode::<ChangeRecord>(&bytes) else {
                continue;
            };
            for node in touched_nodes(&record) {
                history.entry(node).or_default().push(change);
            }
            if let Some(submitted) = record.rebased_from {
                rebased.insert(submitted, change);
            }
        }
        Ok((history, rebased))
    }

    fn scan_edges(&self) -> Result<Vec<StoredEdge>> {
        let mut edges = Vec::new();
        self.for_each_stored_object(|_, bytes| {
            if !looks_like_index_fact(bytes) {
                return Ok(());
            }
            if let Ok(IndexFact::Edge {
                format,
                snapshot,
                kind,
                source,
                target,
            }) = hord_encoding::decode::<IndexFact>(bytes)
                && format == INDEX_FACT_FORMAT
            {
                edges.push(StoredEdge {
                    snapshot,
                    kind,
                    source,
                    target,
                });
            }
            Ok(())
        })?;
        Ok(edges)
    }

    fn write_rebuilt_index(
        &self,
        history: &BTreeMap<NodeId, Vec<ChangeId>>,
        edges: &[StoredEdge],
        rebased: &BTreeMap<ChangeId, ChangeId>,
    ) -> Result<()> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(NODE_HISTORY).map_err(Error::index)?;
            clear_table(&mut table)?;
            for (node, changes) in history {
                let key = node_key(*node);
                let value = encode_change_ids(changes);
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(Error::index)?;
            }
        }
        {
            let mut table = txn.open_table(EDGES).map_err(Error::index)?;
            clear_table(&mut table)?;
            for edge in edges {
                let key = edge_key(edge.snapshot, edge.kind, edge.source, edge.target);
                table
                    .insert(key.as_slice(), PRESENT)
                    .map_err(Error::index)?;
            }
        }
        {
            let mut table = txn.open_table(REBASED).map_err(Error::index)?;
            clear_table(&mut table)?;
            for (submitted, landed) in rebased {
                table
                    .insert(
                        submitted.as_bytes().as_slice(),
                        landed.as_bytes().as_slice(),
                    )
                    .map_err(Error::index)?;
            }
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
    }
}

/// `node_history` rows and `rebased` pairs rebuilt from the log.
type RebuiltHistory = (
    BTreeMap<NodeId, Vec<ChangeId>>,
    BTreeMap<ChangeId, ChangeId>,
);

/// Node ids a change touched, as [`crate::Store::node_history`] indexes
/// them. `read_set` is a dependency, not an edit, so it is omitted. Every
/// [`NodeId`] field on [`Op`] and [`IdentityDelta`] counts, plus
/// `write_set`.
#[must_use]
pub fn touched_nodes(change: &ChangeRecord) -> BTreeSet<NodeId> {
    let mut nodes = BTreeSet::new();
    nodes.extend(change.write_set.iter().copied());
    nodes.extend(change.ops.iter().flat_map(Op::node_ids));
    nodes.extend(
        change
            .identity_deltas
            .iter()
            .flat_map(IdentityDelta::node_ids),
    );
    nodes
}

fn clear_table(table: &mut Table<'_, &[u8], &[u8]>) -> Result<()> {
    table.retain(|_, _| false).map_err(Error::index)?;
    Ok(())
}

/// `node_history` rows to write when `change` lands, read inside `txn`.
/// `pos_of` gives landing positions, `change`'s included.
pub(super) fn plan_history_rows(
    txn: &WriteTransaction,
    change: ChangeId,
    nodes: &BTreeSet<NodeId>,
    pos_of: &impl Fn(&ChangeId) -> Option<usize>,
) -> Result<Vec<([u8; NODE_LEN], Vec<u8>)>> {
    let table = txn.open_table(NODE_HISTORY).map_err(Error::index)?;
    let mut updates = Vec::new();
    for node in nodes {
        let key = node_key(*node);
        let existing = table
            .get(key.as_slice())
            .map_err(Error::index)?
            .map(|value| value.value().to_vec());
        if let Some(encoded) = history_value(existing.as_deref(), change, pos_of)? {
            updates.push((key, encoded));
        }
    }
    Ok(updates)
}

pub(super) fn write_history_rows(
    txn: &WriteTransaction,
    updates: &[([u8; NODE_LEN], Vec<u8>)],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let mut table = txn.open_table(NODE_HISTORY).map_err(Error::index)?;
    for (key, encoded) in updates {
        table
            .insert(key.as_slice(), encoded.as_slice())
            .map_err(Error::index)?;
    }
    Ok(())
}

fn history_value(
    existing: Option<&[u8]>,
    change: ChangeId,
    pos_of: &impl Fn(&ChangeId) -> Option<usize>,
) -> Result<Option<Vec<u8>>> {
    let Some(bytes) = existing else {
        return Ok(Some(change.as_bytes().to_vec()));
    };
    match plan_history_update(bytes, change, pos_of) {
        HistoryPlan::Unchanged => Ok(None),
        HistoryPlan::Replace(updated) => Ok(Some(updated)),
        HistoryPlan::Resort => {
            let mut ids = decode_change_ids(bytes)?;
            let before = ids.len();
            insert_in_log_order(&mut ids, change, pos_of);
            if ids.len() == before {
                Ok(None)
            } else {
                Ok(Some(encode_change_ids(&ids)))
            }
        }
    }
}

enum HistoryPlan {
    Unchanged,
    Replace(Vec<u8>),
    Resort,
}

/// Append when `change` lands after every id already stored. Otherwise the
/// caller re-sorts. Positions come from the first landing index.
fn plan_history_update(
    existing: &[u8],
    change: ChangeId,
    pos_of: &impl Fn(&ChangeId) -> Option<usize>,
) -> HistoryPlan {
    if existing.is_empty() || !existing.len().is_multiple_of(ObjectId::LEN) {
        return HistoryPlan::Resort;
    }
    let last = &existing[existing.len() - ObjectId::LEN..];
    let Ok(last_id) = ObjectId::try_from(last) else {
        return HistoryPlan::Resort;
    };
    let last_pos = pos_of(&last_id).unwrap_or(usize::MAX);
    let change_pos = pos_of(&change).unwrap_or(usize::MAX);
    if change_pos < last_pos {
        return HistoryPlan::Resort;
    }
    if change_pos == last_pos {
        return if last_id == change {
            HistoryPlan::Unchanged
        } else {
            HistoryPlan::Resort
        };
    }
    let mut out = Vec::with_capacity(existing.len() + ObjectId::LEN);
    out.extend_from_slice(existing);
    out.extend_from_slice(change.as_bytes());
    HistoryPlan::Replace(out)
}

fn insert_in_log_order(
    ids: &mut Vec<ChangeId>,
    change: ChangeId,
    pos_of: &impl Fn(&ChangeId) -> Option<usize>,
) {
    let pos = pos_of(&change).unwrap_or(usize::MAX);
    let at = ids.partition_point(|existing| pos_of(existing).unwrap_or(usize::MAX) <= pos);
    let mut index = at;
    while index > 0 && pos_of(&ids[index - 1]).unwrap_or(usize::MAX) == pos {
        if ids[index - 1] == change {
            return;
        }
        index -= 1;
    }
    ids.insert(at, change);
}

fn looks_like_index_fact(bytes: &[u8]) -> bool {
    bytes.starts_with(EDGE_FACT_PREFIX)
}

fn edge_key(
    snapshot: SnapshotId,
    kind: EdgeKind,
    source: NodeId,
    target: NodeId,
) -> [u8; EDGE_KEY_LEN] {
    let mut key = [0u8; EDGE_KEY_LEN];
    key[..SNAP_LEN].copy_from_slice(snapshot.as_bytes());
    key[SNAP_LEN] = kind.tag();
    key[SNAP_LEN + 1..EDGE_PREFIX_LEN].copy_from_slice(&source.as_u128().to_be_bytes());
    key[EDGE_PREFIX_LEN..].copy_from_slice(&target.as_u128().to_be_bytes());
    key
}

fn edge_bounds(
    snapshot: SnapshotId,
    kind: EdgeKind,
    source: NodeId,
) -> ([u8; EDGE_KEY_LEN], [u8; EDGE_KEY_LEN]) {
    (
        edge_key(snapshot, kind, source, NodeId::nil()),
        edge_key(snapshot, kind, source, NodeId::from_u128(u128::MAX)),
    )
}

fn node_key(node: NodeId) -> [u8; NODE_LEN] {
    node.as_u128().to_be_bytes()
}

fn node_from_bytes(bytes: &[u8]) -> Result<NodeId> {
    let bytes: [u8; NODE_LEN] = bytes
        .try_into()
        .map_err(|_| Error::CorruptIndex("node id"))?;
    Ok(NodeId::from_u128(u128::from_be_bytes(bytes)))
}

fn decode_change_ids(bytes: &[u8]) -> Result<Vec<ChangeId>> {
    let (chunks, rest) = bytes.as_chunks::<{ ObjectId::LEN }>();
    if !rest.is_empty() {
        return Err(Error::CorruptIndex("node_history"));
    }
    let mut ids = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        ids.push(ObjectId::try_from(chunk.as_slice())?);
    }
    Ok(ids)
}

fn encode_change_ids(ids: &[ChangeId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * ObjectId::LEN);
    for id in ids {
        out.extend_from_slice(id.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use hord_core::{
        Actor, Blob, IdentityDelta, IdentityMap, Intent, NodeId, ObjectId, Op, Provenance,
        QualifiedName, RepoPath, Timestamp, TreeOpKind,
    };
    use hord_encoding::{decode, encode};

    use super::{IndexFact, insert_in_log_order, looks_like_index_fact, touched_nodes};

    fn nid(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    fn oid(n: u8) -> ObjectId {
        let mut bytes = [0u8; 32];
        bytes[31] = n;
        ObjectId::from_bytes(bytes)
    }

    #[test]
    fn index_facts_round_trip_and_do_not_match_other_objects() {
        let edge = IndexFact::Edge {
            format: super::INDEX_FACT_FORMAT,
            snapshot: oid(1),
            kind: super::EdgeKind::References,
            source: nid(2),
            target: nid(3),
        };
        let bytes = encode(&edge).unwrap();
        assert_eq!(encode(&edge).unwrap(), bytes);
        assert!(bytes.starts_with(super::EDGE_FACT_PREFIX));
        assert!(looks_like_index_fact(&bytes));
        assert_eq!(decode::<IndexFact>(&bytes).unwrap(), edge);

        let blob = encode(&Blob::new(b"hi".to_vec())).unwrap();
        assert!(decode::<IndexFact>(&blob).is_err());
        let map = encode(&IdentityMap::default()).unwrap();
        assert!(decode::<IndexFact>(&map).is_err());
        assert!(!looks_like_index_fact(&map));
    }

    #[test]
    fn touched_nodes_include_writes_ops_and_deltas_not_reads() {
        let write = nid(1);
        let read = nid(2);
        let parent = nid(3);
        let deleted = nid(4);
        let replaced = nid(5);
        let moved = nid(6);
        let from_parent = nid(7);
        let to_parent = nid(8);
        let renamed = nid(9);
        let born = nid(10);
        let died = nid(11);
        let derived = nid(12);
        let derived_from = nid(13);
        let split = nid(14);
        let split_into = nid(15);
        let merged = nid(16);
        let merged_from = nid(17);
        let inserted = ObjectId::from_canonical(b"inserted");
        let record = hord_core::ChangeRecord {
            base: oid(1),
            result: oid(2),
            parents: Vec::new(),
            ops: vec![
                Op::Insert {
                    parent,
                    index: 0,
                    node: inserted,
                },
                Op::Delete { node: deleted },
                Op::Replace {
                    node: replaced,
                    from: inserted,
                    to: inserted,
                },
                Op::Move {
                    node: moved,
                    from_parent,
                    to_parent,
                    index: 1,
                },
                Op::Rename {
                    node: renamed,
                    from: QualifiedName::new("old"),
                    to: QualifiedName::new("new"),
                },
                Op::Blob {
                    path: RepoPath::default(),
                    from: None,
                    to: None,
                },
                Op::Tree {
                    path: RepoPath::default(),
                    kind: TreeOpKind::CreateFile,
                },
            ],
            intent: Intent {
                summary: "s".into(),
                body: String::new(),
                refs: Vec::new(),
                acceptance: Vec::new(),
            },
            provenance: Provenance {
                actor: Actor::Human { id: "t".into() },
                toolchain: oid(3),
                created_at: Timestamp::from_millis(0),
                session: None,
                parent_intent: None,
            },
            read_set: BTreeSet::from([read]),
            write_set: BTreeSet::from([write]),
            identity_deltas: vec![
                IdentityDelta::Birth { node: born },
                IdentityDelta::Death { node: died },
                IdentityDelta::DerivedFrom {
                    node: derived,
                    from: derived_from,
                },
                IdentityDelta::SplitInto {
                    node: split,
                    into: vec![split_into],
                },
                IdentityDelta::MergedFrom {
                    node: merged,
                    from: vec![merged_from],
                },
            ],
            evidence: Vec::new(),
            signature: None,
            rebased_from: None,
        };
        let got = touched_nodes(&record);
        assert_eq!(
            got,
            BTreeSet::from([
                write,
                parent,
                deleted,
                replaced,
                moved,
                from_parent,
                to_parent,
                renamed,
                born,
                died,
                derived,
                derived_from,
                split,
                split_into,
                merged,
                merged_from,
            ])
        );
        assert!(!got.contains(&read));
        let bytes = encode(&record).unwrap();
        assert!(decode::<IndexFact>(&bytes).is_err());
        assert!(!looks_like_index_fact(&bytes));
    }

    #[test]
    fn insert_in_log_order_matches_a_full_scan() {
        let log: Vec<ObjectId> = (0..24u8).map(oid).collect();
        let pos: HashMap<_, _> = log
            .iter()
            .copied()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        let mut got = Vec::new();
        let mut expect = Vec::new();
        let steps = [5u8, 1, 19, 0, 5, 7, 3, 2, 18, 4, 19, 23, 6, 1, 8];
        for step in steps {
            let change = oid(step);
            insert_in_log_order(&mut got, change, &|id: &ObjectId| pos.get(id).copied());
            insert_by_scanning(&mut expect, change, &log);
            assert_eq!(got, expect);
        }
    }

    fn insert_by_scanning(ids: &mut Vec<ObjectId>, change: ObjectId, log: &[ObjectId]) {
        if ids.contains(&change) {
            return;
        }
        let pos = log
            .iter()
            .position(|id| *id == change)
            .unwrap_or(usize::MAX);
        let at = ids
            .iter()
            .position(|existing| {
                log.iter()
                    .position(|id| id == existing)
                    .unwrap_or(usize::MAX)
                    > pos
            })
            .unwrap_or(ids.len());
        ids.insert(at, change);
    }
}
