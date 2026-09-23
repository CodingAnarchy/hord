//! Rebuildable caches for the spec §8.1 tables `node_history`, `edges`, and `identity`.
//!
//! Redb is not the only copy of a fact:
//! - `node_history` is derived from landed [`hord_core::ChangeRecord`] objects.
//! - each edge is its own content-addressed object.
//! - each identity table is an [`hord_core::IdentityMap`] plus a binding object
//!   that names the snapshot and the binding it replaces.
//!
//! [`Store::rebuild_index`] drops those caches and fills them from the objects.
//! The longest identity supersede chain for a snapshot wins. Equal lengths break
//! ties by the greater binding [`hord_core::ObjectId`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hord_core::{
    ChangeId, ChangeRecord, IdentityDelta, IdentityMap, NodeId, NodePath, ObjectId, Op, SnapshotId,
};
use redb::{Database, Durability, ReadableTable, Table, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::Store;
use super::queue::IDENTITY_INDEX;
use crate::{Error, Result};

const INDEX_FACT_FORMAT: u8 = 1;
const PRESENT: &[u8] = &[0];

/// Canonical CBOR prefix of [`IndexFact::Edge`]: a one-entry map keyed by `"Edge"`.
const EDGE_FACT_PREFIX: &[u8] = b"\xa1\x64Edge";
/// Canonical CBOR prefix of [`IndexFact::Identity`].
const IDENTITY_FACT_PREFIX: &[u8] = b"\xa1\x68Identity";
/// Canonical CBOR prefix of [`IndexFact::IdentityIndex`].
const IDENTITY_INDEX_FACT_PREFIX: &[u8] = b"\xa1\x6dIdentityIndex";
/// `identity_index` row value: `index || binding`.
pub(super) const IDENTITY_INDEX_ROW_LEN: usize = 2 * ObjectId::LEN;

const SNAP_LEN: usize = ObjectId::LEN;
pub(super) const NODE_LEN: usize = 16;
/// `snapshot || kind || source || target`
pub(super) const EDGE_KEY_LEN: usize = SNAP_LEN + 1 + NODE_LEN + NODE_LEN;
const EDGE_PREFIX_LEN: usize = SNAP_LEN + 1 + NODE_LEN;

const NODE_HISTORY: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("node_history");
const EDGES: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("edges");
const IDENTITY: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("identity");
const INDEX_HEADS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("index_heads");

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
    Identity {
        format: u8,
        snapshot: SnapshotId,
        map: ObjectId,
        supersedes: Option<ObjectId>,
    },
    /// `hord-txn`'s identity index object for `snapshot`
    /// ([`Store::set_identity_index`]). `supersedes` is the binding it
    /// replaced for the same snapshot.
    IdentityIndex {
        format: u8,
        snapshot: SnapshotId,
        index: ObjectId,
        supersedes: Option<ObjectId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredEdge {
    snapshot: SnapshotId,
    kind: EdgeKind,
    source: NodeId,
    target: NodeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FoundBinding {
    id: ObjectId,
    snapshot: SnapshotId,
    map: ObjectId,
    supersedes: Option<ObjectId>,
}

pub(super) fn open_tables(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(NODE_HISTORY).map_err(Error::index)?;
    txn.open_table(EDGES).map_err(Error::index)?;
    txn.open_table(IDENTITY).map_err(Error::index)?;
    txn.open_table(INDEX_HEADS).map_err(Error::index)?;
    Ok(())
}

pub(super) fn ensure_tables(db: &Database) -> Result<()> {
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

    /// Record which [`NodeId`] each definition had in `snapshot`.
    ///
    /// `map.nodes` is [`NodeId`] → location. The identity table answers the
    /// inverse, location → [`NodeId`], via [`Self::identity_at`]. The stored
    /// [`IdentityMap`] is the fact; redb is a cache. Returns that object's
    /// [`ObjectId`] (the value [`hord_core::IndexPointers::identity`] holds).
    ///
    /// Replaces any identity previously recorded for `snapshot`. Fails if two
    /// entries share a [`NodePath`].
    pub fn put_identity(&self, snapshot: SnapshotId, map: &IdentityMap) -> Result<ObjectId> {
        let _guard = self.lock_index();
        let rows = identity_rows(snapshot, map)?;
        let map_id = self.put_object(map)?;
        let supersedes = self.identity_head(snapshot)?;
        let binding_id = self.put_object(&IndexFact::Identity {
            format: INDEX_FACT_FORMAT,
            snapshot,
            map: map_id,
            supersedes,
        })?;
        self.commit_identity(snapshot, &rows, binding_id)?;
        Ok(map_id)
    }

    /// [`NodeId`] recorded for the definition at `path` in `snapshot`.
    pub fn identity_at(&self, snapshot: SnapshotId, path: &NodePath) -> Result<Option<NodeId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(IDENTITY).map_err(Error::index)?;
        let key = identity_key(snapshot, path)?;
        match table.get(key.as_slice()).map_err(Error::index)? {
            Some(value) => Ok(Some(node_from_bytes(value.value())?)),
            None => Ok(None),
        }
    }

    /// Stored identity fact for `snapshot`, if [`Self::put_identity`] has run.
    pub fn identity_map(&self, snapshot: SnapshotId) -> Result<Option<IdentityMap>> {
        let Some(binding_id) = self.identity_head(snapshot)? else {
            return Ok(None);
        };
        let IndexFact::Identity {
            format,
            snapshot: got,
            map,
            ..
        } = self.get_object(binding_id)?
        else {
            return Err(Error::CorruptIndex("identity head"));
        };
        if format != INDEX_FACT_FORMAT || got != snapshot {
            return Err(Error::CorruptIndex("identity head"));
        }
        Ok(Some(self.get_object(map)?))
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

    /// Point `snapshot` at `hord-txn`'s identity index object `index`.
    ///
    /// `hord-txn` stores, per snapshot, which files carry
    /// [`hord_core::NodeId`]s that differ from a fresh assignment. Besides the
    /// redb row, this writes a content-addressed binding object naming
    /// `(snapshot, index)` and the binding it replaces, so
    /// [`Self::rebuild_index`] restores the row. Replaces any previous
    /// pointer.
    ///
    /// Like [`Store::queue_set`], the commit does not fsync. redb commits
    /// are ordered, so the row is durable once any later durable commit is:
    /// `propose` writes it before the durable [`Store::queue_push`] of
    /// `submit`. A clean close is also durable. The lander writes its row
    /// inside [`Store::land`] instead.
    pub fn set_identity_index(&self, snapshot: SnapshotId, index: ObjectId) -> Result<()> {
        let _guard = self.lock_index();
        let row = self.identity_index_binding(snapshot, index)?;
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::None);
        insert_identity_index_row(&txn, snapshot, &row)?;
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// Store the binding object for `(snapshot, index)` and return the
    /// `identity_index` row that points at both. The caller holds
    /// `index_lock` until the row is committed, so the supersede chain
    /// cannot fork.
    pub(super) fn identity_index_binding(
        &self,
        snapshot: SnapshotId,
        index: ObjectId,
    ) -> Result<[u8; IDENTITY_INDEX_ROW_LEN]> {
        let supersedes = self.identity_index_row(snapshot)?.and_then(|(_, b)| b);
        let binding = self.put_object(&IndexFact::IdentityIndex {
            format: INDEX_FACT_FORMAT,
            snapshot,
            index,
            supersedes,
        })?;
        Ok(identity_index_row(index, binding))
    }

    /// The identity index object recorded for `snapshot`, if any.
    pub fn identity_index(&self, snapshot: SnapshotId) -> Result<Option<ObjectId>> {
        Ok(self.identity_index_row(snapshot)?.map(|(index, _)| index))
    }

    /// `(index, binding)` for `snapshot`. Rows written before bindings
    /// existed hold only the index.
    fn identity_index_row(
        &self,
        snapshot: SnapshotId,
    ) -> Result<Option<(ObjectId, Option<ObjectId>)>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
        let Some(value) = table
            .get(snapshot.as_bytes().as_slice())
            .map_err(Error::index)?
        else {
            return Ok(None);
        };
        let bytes = value.value();
        match bytes.len() {
            ObjectId::LEN => Ok(Some((ObjectId::try_from(bytes)?, None))),
            IDENTITY_INDEX_ROW_LEN => Ok(Some((
                ObjectId::try_from(&bytes[..ObjectId::LEN])?,
                Some(ObjectId::try_from(&bytes[ObjectId::LEN..])?),
            ))),
            _ => Err(Error::CorruptIndex("identity_index")),
        }
    }

    /// Replace `node_history`, `edges`, `identity`, and `identity_index`
    /// from stored objects.
    ///
    /// Landing-log entries that are not [`ChangeRecord`]s are skipped. A log
    /// entry with no stored object is an error, and the previous cache is left
    /// in place. Edge and identity rows come from the objects written by
    /// [`Self::put_edge`] and [`Self::put_identity`], and identity index
    /// pointers from the bindings [`Self::set_identity_index`] writes (the
    /// longest supersede chain per snapshot wins, as for identity).
    pub fn rebuild_index(&self) -> Result<()> {
        let _guard = self.lock_index();
        self.flush()?;
        let history = self.history_from_log()?;
        let (edges, bindings, index_bindings) = self.scan_index_facts()?;
        let heads = select_identity_heads(&bindings);
        let index_heads = select_identity_heads(&index_bindings);
        let mut identity = BTreeMap::new();
        for (snapshot, binding) in heads {
            let map: IdentityMap = self.get_object(binding.map)?;
            let rows = identity_rows(snapshot, &map)?;
            identity.insert(snapshot, (binding.id, rows));
        }
        self.write_rebuilt_index(&history, &edges, &identity, &index_heads)
    }

    pub(super) fn lock_index(&self) -> std::sync::MutexGuard<'_, ()> {
        self.index_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn identity_head(&self, snapshot: SnapshotId) -> Result<Option<ObjectId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(INDEX_HEADS).map_err(Error::index)?;
        let key = identity_head_key(snapshot);
        match table.get(key.as_slice()).map_err(Error::index)? {
            Some(value) => Ok(Some(ObjectId::try_from(value.value())?)),
            None => Ok(None),
        }
    }

    fn commit_identity(
        &self,
        snapshot: SnapshotId,
        rows: &BTreeMap<Vec<u8>, NodeId>,
        binding_id: ObjectId,
    ) -> Result<()> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(IDENTITY).map_err(Error::index)?;
            remove_snapshot_identity(&mut table, snapshot)?;
            insert_identity_rows(&mut table, rows)?;
        }
        {
            let mut heads = txn.open_table(INDEX_HEADS).map_err(Error::index)?;
            let key = identity_head_key(snapshot);
            heads
                .insert(key.as_slice(), binding_id.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
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

    fn history_from_log(&self) -> Result<BTreeMap<NodeId, Vec<ChangeId>>> {
        let log = self.log()?;
        let mut history: BTreeMap<NodeId, Vec<ChangeId>> = BTreeMap::new();
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
        }
        Ok(history)
    }

    /// Edges, identity bindings, and identity index bindings (the latter
    /// with the index object in `map`).
    fn scan_index_facts(&self) -> Result<(Vec<StoredEdge>, Vec<FoundBinding>, Vec<FoundBinding>)> {
        let mut edges = Vec::new();
        let mut bindings = Vec::new();
        let mut index_bindings = Vec::new();
        self.for_each_stored_object(|id, bytes| {
            if !looks_like_index_fact(bytes) {
                return Ok(());
            }
            match hord_encoding::decode::<IndexFact>(bytes) {
                Ok(IndexFact::Edge {
                    format,
                    snapshot,
                    kind,
                    source,
                    target,
                }) if format == INDEX_FACT_FORMAT => {
                    edges.push(StoredEdge {
                        snapshot,
                        kind,
                        source,
                        target,
                    });
                }
                Ok(IndexFact::Identity {
                    format,
                    snapshot,
                    map,
                    supersedes,
                }) if format == INDEX_FACT_FORMAT => {
                    bindings.push(FoundBinding {
                        id,
                        snapshot,
                        map,
                        supersedes,
                    });
                }
                Ok(IndexFact::IdentityIndex {
                    format,
                    snapshot,
                    index,
                    supersedes,
                }) if format == INDEX_FACT_FORMAT => {
                    index_bindings.push(FoundBinding {
                        id,
                        snapshot,
                        map: index,
                        supersedes,
                    });
                }
                _ => {}
            }
            Ok(())
        })?;
        Ok((edges, bindings, index_bindings))
    }

    fn write_rebuilt_index(
        &self,
        history: &BTreeMap<NodeId, Vec<ChangeId>>,
        edges: &[StoredEdge],
        identity: &BTreeMap<SnapshotId, (ObjectId, BTreeMap<Vec<u8>, NodeId>)>,
        identity_index: &BTreeMap<SnapshotId, FoundBinding>,
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
            let mut table = txn.open_table(IDENTITY).map_err(Error::index)?;
            clear_table(&mut table)?;
            for (_, rows) in identity.values() {
                insert_identity_rows(&mut table, rows)?;
            }
        }
        {
            let mut heads = txn.open_table(INDEX_HEADS).map_err(Error::index)?;
            clear_table(&mut heads)?;
            for (snapshot, (binding_id, _)) in identity {
                let key = identity_head_key(*snapshot);
                heads
                    .insert(key.as_slice(), binding_id.as_bytes().as_slice())
                    .map_err(Error::index)?;
            }
        }
        {
            let mut table = txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
            clear_table(&mut table)?;
            for (snapshot, binding) in identity_index {
                let row = identity_index_row(binding.map, binding.id);
                table
                    .insert(snapshot.as_bytes().as_slice(), row.as_slice())
                    .map_err(Error::index)?;
            }
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
    }
}

/// Node ids a change touched. `read_set` is a dependency, not an edit, so it
/// is omitted. Every [`NodeId`] field on [`Op`] and [`IdentityDelta`] counts,
/// plus `write_set`.
pub(super) fn touched_nodes(change: &ChangeRecord) -> BTreeSet<NodeId> {
    let mut nodes = BTreeSet::new();
    nodes.extend(change.write_set.iter().copied());
    for op in &change.ops {
        match op {
            Op::Insert { parent, .. } => {
                nodes.insert(*parent);
            }
            Op::Delete { node } | Op::Replace { node, .. } | Op::Rename { node, .. } => {
                nodes.insert(*node);
            }
            Op::Move {
                node,
                from_parent,
                to_parent,
                ..
            } => {
                nodes.insert(*node);
                nodes.insert(*from_parent);
                nodes.insert(*to_parent);
            }
            Op::Blob { .. } | Op::Tree { .. } => {}
        }
    }
    for delta in &change.identity_deltas {
        match delta {
            IdentityDelta::Birth { node } | IdentityDelta::Death { node } => {
                nodes.insert(*node);
            }
            IdentityDelta::DerivedFrom { node, from } => {
                nodes.insert(*node);
                nodes.insert(*from);
            }
            IdentityDelta::SplitInto { node, into } => {
                nodes.insert(*node);
                nodes.extend(into.iter().copied());
            }
            IdentityDelta::MergedFrom { node, from } => {
                nodes.insert(*node);
                nodes.extend(from.iter().copied());
            }
        }
    }
    nodes
}

fn select_identity_heads(bindings: &[FoundBinding]) -> BTreeMap<SnapshotId, FoundBinding> {
    let by_id: HashMap<ObjectId, FoundBinding> =
        bindings.iter().copied().map(|b| (b.id, b)).collect();
    let mut memo = HashMap::new();
    let mut best: BTreeMap<SnapshotId, (u32, FoundBinding)> = BTreeMap::new();
    for binding in bindings {
        let len = chain_len(binding.id, &by_id, &mut memo);
        let replace = match best.get(&binding.snapshot) {
            Some((best_len, best_binding)) => (len, binding.id) > (*best_len, best_binding.id),
            None => true,
        };
        if replace {
            best.insert(binding.snapshot, (len, *binding));
        }
    }
    best.into_iter()
        .map(|(snapshot, (_, binding))| (snapshot, binding))
        .collect()
}

fn chain_len(
    start: ObjectId,
    by_id: &HashMap<ObjectId, FoundBinding>,
    memo: &mut HashMap<ObjectId, u32>,
) -> u32 {
    let mut stack = vec![start];
    while let Some(&id) = stack.last() {
        if memo.contains_key(&id) {
            stack.pop();
            continue;
        }
        match by_id
            .get(&id)
            .and_then(|binding| binding.supersedes)
            .filter(|parent| by_id.contains_key(parent))
        {
            Some(parent) if memo.contains_key(&parent) => {
                let len = memo[&parent].saturating_add(1);
                memo.insert(id, len);
                stack.pop();
            }
            Some(parent) if stack.contains(&parent) => {
                memo.insert(id, 0);
                stack.pop();
            }
            Some(parent) => stack.push(parent),
            None => {
                memo.insert(id, 0);
                stack.pop();
            }
        }
    }
    memo.get(&start).copied().unwrap_or(0)
}

fn identity_rows(snapshot: SnapshotId, map: &IdentityMap) -> Result<BTreeMap<Vec<u8>, NodeId>> {
    let mut rows = BTreeMap::new();
    for (node, path) in &map.nodes {
        let key = identity_key(snapshot, path)?;
        if rows.insert(key, *node).is_some() {
            return Err(Error::DuplicateIdentity);
        }
    }
    Ok(rows)
}

fn insert_identity_rows(
    table: &mut Table<'_, &[u8], &[u8]>,
    rows: &BTreeMap<Vec<u8>, NodeId>,
) -> Result<()> {
    for (key, node) in rows {
        let value = node.as_u128().to_be_bytes();
        table
            .insert(key.as_slice(), value.as_slice())
            .map_err(Error::index)?;
    }
    Ok(())
}

fn remove_snapshot_identity(
    table: &mut Table<'_, &[u8], &[u8]>,
    snapshot: SnapshotId,
) -> Result<()> {
    let prefix = *snapshot.as_bytes();
    match prefix_successor(&prefix) {
        Some(end) => table.retain_in(prefix.as_slice()..end.as_slice(), |_, _| false),
        None => table.retain_in(prefix.as_slice().., |_, _| false),
    }
    .map_err(Error::index)?;
    Ok(())
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

pub(super) fn insert_identity_index_row(
    txn: &WriteTransaction,
    snapshot: SnapshotId,
    row: &[u8; IDENTITY_INDEX_ROW_LEN],
) -> Result<()> {
    let mut table = txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
    table
        .insert(snapshot.as_bytes().as_slice(), row.as_slice())
        .map_err(Error::index)?;
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
        || bytes.starts_with(IDENTITY_FACT_PREFIX)
        || bytes.starts_with(IDENTITY_INDEX_FACT_PREFIX)
}

fn identity_index_row(index: ObjectId, binding: ObjectId) -> [u8; IDENTITY_INDEX_ROW_LEN] {
    let mut row = [0u8; IDENTITY_INDEX_ROW_LEN];
    row[..ObjectId::LEN].copy_from_slice(index.as_bytes());
    row[ObjectId::LEN..].copy_from_slice(binding.as_bytes());
    row
}

/// Exclusive end of the key range that starts with `prefix`.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
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

fn identity_key(snapshot: SnapshotId, path: &NodePath) -> Result<Vec<u8>> {
    let encoded = hord_encoding::encode(path)?;
    let mut key = Vec::with_capacity(SNAP_LEN + encoded.len());
    key.extend_from_slice(snapshot.as_bytes());
    key.extend_from_slice(&encoded);
    Ok(key)
}

fn identity_head_key(snapshot: SnapshotId) -> [u8; SNAP_LEN + 1] {
    let mut key = [0u8; SNAP_LEN + 1];
    key[0] = b'i';
    key[1..].copy_from_slice(snapshot.as_bytes());
    key
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

    use super::{
        FoundBinding, IndexFact, chain_len, insert_in_log_order, looks_like_index_fact,
        prefix_successor, select_identity_heads, touched_nodes,
    };

    fn nid(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    fn oid(n: u8) -> ObjectId {
        let mut bytes = [0u8; 32];
        bytes[31] = n;
        ObjectId::from_bytes(bytes)
    }

    fn binding(id: u8, snapshot: u8, supersedes: Option<u8>) -> FoundBinding {
        FoundBinding {
            id: oid(id),
            snapshot: oid(snapshot),
            map: oid(id),
            supersedes: supersedes.map(oid),
        }
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

        let identity = IndexFact::Identity {
            format: super::INDEX_FACT_FORMAT,
            snapshot: oid(1),
            map: oid(4),
            supersedes: Some(oid(5)),
        };
        let bytes = encode(&identity).unwrap();
        assert!(bytes.starts_with(super::IDENTITY_FACT_PREFIX));
        assert!(looks_like_index_fact(&bytes));
        assert_eq!(decode::<IndexFact>(&bytes).unwrap(), identity);

        let blob = encode(&Blob::new(b"hi".to_vec())).unwrap();
        assert!(decode::<IndexFact>(&blob).is_err());
        let map = encode(&IdentityMap::default()).unwrap();
        assert!(decode::<IndexFact>(&map).is_err());
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

    #[test]
    fn prefix_successor_covers_exactly_the_prefix() {
        assert_eq!(prefix_successor(&[0x01, 0x02]).unwrap(), vec![0x01, 0x03]);
        assert_eq!(prefix_successor(&[0x01, 0xff]).unwrap(), vec![0x02]);
        assert!(prefix_successor(&[0xff, 0xff]).is_none());
        let prefix = [0x10u8, 0xff];
        let end = prefix_successor(&prefix).unwrap();
        assert!(prefix.as_slice() < end.as_slice());
        let with_tail = [0x10u8, 0xff, 0x00];
        assert!(with_tail.as_slice() < end.as_slice());
        assert!(end.as_slice() <= [0x11].as_slice());
    }

    #[test]
    fn identity_head_prefers_the_longest_chain_then_the_greater_id() {
        // b1 <- b2 (id 4) and b1 <- b3 (id 2) <- b4 (id 3). b4 is longer than b2.
        let bindings = vec![
            binding(1, 1, None),
            binding(4, 1, Some(1)),
            binding(2, 1, Some(1)),
            binding(3, 1, Some(2)),
            binding(9, 2, None),
        ];
        let heads = select_identity_heads(&bindings);
        assert_eq!(heads.get(&oid(1)).unwrap().id, oid(3));
        assert_eq!(heads.get(&oid(2)).unwrap().id, oid(9));

        let fork = vec![
            binding(1, 1, None),
            binding(4, 1, Some(1)),
            binding(2, 1, Some(1)),
        ];
        let heads = select_identity_heads(&fork);
        assert_eq!(heads.get(&oid(1)).unwrap().id, oid(4));
    }

    #[test]
    fn chain_len_stops_on_a_cycle() {
        let bindings = [binding(1, 1, Some(2)), binding(2, 1, Some(1))];
        let by_id = bindings.into_iter().map(|b| (b.id, b)).collect();
        let mut memo = HashMap::new();
        let len = chain_len(oid(1), &by_id, &mut memo);
        assert!(len < 3);
    }
}
