//! Tables for `hord-txn` (spec §6.1, §6.7): the lander queue, an index from
//! change ids to the queue entries that name them, the changes whose ops
//! were checked, and the per-snapshot identity index pointer. The pointer's
//! methods live in `index.rs`, next to the content-addressed fact that makes
//! it rebuildable.
//!
//! The store does not interpret a queue entry or an identity index object.
//! `hord-txn` owns their encoding and says which ids name an entry.
//!
//! [`Store::land`] writes everything one landing changes in one durable
//! commit.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use hord_core::{ChangeId, ChangeRecord, NodeId, ObjectId, SnapshotId};
use redb::{
    Database, Durability, ReadableTable, ReadableTableMetadata, TableDefinition, WriteTransaction,
};

use super::index::{
    insert_identity_index_row, plan_history_rows, touched_nodes, write_history_rows,
};
use super::{
    META, META_HEAD, Store, lock, lock_map, lock_vec, write_pending_log, write_pending_refs,
};
use crate::{Error, Result};

const QUEUE: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("lander_queue");
/// Change id → the sequence numbers of queue entries it names, ascending,
/// as 8-byte big-endian integers. A superset: an entry listed here may no
/// longer name the id (a landed status can be reset by recovery).
const QUEUE_NAMES: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("lander_queue_names");
/// Changes whose ops are known to reproduce their result (spec §3.5).
const CHECKED: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("checked_changes");
pub(super) const IDENTITY_INDEX: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("identity_index");
/// `META` key present once `lander_queue_names` covers every queue entry.
const META_QUEUE_NAMES: &str = "lander_queue_names";

pub(super) fn open_tables(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(QUEUE).map_err(Error::index)?;
    txn.open_table(QUEUE_NAMES).map_err(Error::index)?;
    txn.open_table(CHECKED).map_err(Error::index)?;
    txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
    Ok(())
}

/// Create missing tables. A store with an empty queue has a complete name
/// index; one with entries from before the index existed is left for
/// [`Store::index_queue_names`].
pub(super) fn ensure_tables(db: &Database) -> Result<()> {
    let txn = db.begin_write().map_err(Error::index)?;
    open_tables(&txn)?;
    let empty = txn
        .open_table(QUEUE)
        .map_err(Error::index)?
        .is_empty()
        .map_err(Error::index)?;
    if empty {
        mark_names_indexed(&txn)?;
    }
    txn.commit().map_err(Error::index)?;
    Ok(())
}

pub(super) fn mark_names_indexed(txn: &WriteTransaction) -> Result<()> {
    let mut meta = txn.open_table(META).map_err(Error::index)?;
    meta.insert(META_QUEUE_NAMES, [1u8].as_slice())
        .map_err(Error::index)?;
    Ok(())
}

/// Everything one landing writes to the index (spec §6.7), for
/// [`Store::land`].
#[derive(Clone, Copy, Debug)]
pub struct Landing<'a> {
    /// The id the change lands under; appended to the log and made `head`.
    pub change: ChangeId,
    /// Its stored record, for the `node_history` rows.
    pub record: &'a ChangeRecord,
    /// The queue entry to replace, with its new value.
    pub entry: (u64, &'a [u8]),
    /// Ids that name the entry, besides those it was pushed with.
    pub names: &'a [ObjectId],
    /// `hord-txn`'s identity index object for `record.result`, if any
    /// ([`Store::set_identity_index`]).
    pub identity_index: Option<(SnapshotId, ObjectId)>,
}

impl Store {
    /// Append an entry to the persistent lander queue (spec §6.7) and return
    /// its sequence number. Sequence numbers start at 0 and only grow.
    /// `names` are the ids [`Store::queue_named`] finds it by; `checked`
    /// are recorded as for [`Store::mark_checked`], in the same commit.
    ///
    /// The write is durable when this returns.
    pub fn queue_push(
        &self,
        entry: &[u8],
        names: &[ObjectId],
        checked: &[ChangeId],
    ) -> Result<u64> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        let seq = {
            let mut table = txn.open_table(QUEUE).map_err(Error::index)?;
            let seq = match table.last().map_err(Error::index)? {
                Some((key, _)) => key.value() + 1,
                None => 0,
            };
            table.insert(seq, entry).map_err(Error::index)?;
            seq
        };
        add_names(&txn, names.iter().map(|name| (*name, seq)))?;
        insert_checked(&txn, checked)?;
        txn.commit().map_err(Error::index)?;
        Ok(seq)
    }

    /// Replace the queue entry at `seq`.
    ///
    /// The commit does not fsync. It becomes durable with the next durable
    /// commit, such as [`Store::land`], [`Store::set_head`],
    /// [`Store::queue_push`], or [`Store::flush`] with pending refs or log
    /// entries. A lander that crashes before then sees the previous entry
    /// and processes it again.
    pub fn queue_set(&self, seq: u64, entry: &[u8]) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::None);
        set_entry(&txn, seq, entry)?;
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// One queue entry, if `seq` exists.
    pub fn queue_entry(&self, seq: u64) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(QUEUE).map_err(Error::index)?;
        Ok(table
            .get(seq)
            .map_err(Error::index)?
            .map(|value| value.value().to_vec()))
    }

    /// Every queue entry in sequence order.
    pub fn queue_entries(&self) -> Result<Vec<(u64, Vec<u8>)>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(QUEUE).map_err(Error::index)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(Error::index)? {
            let (key, value) = entry.map_err(Error::index)?;
            out.push((key.value(), value.value().to_vec()));
        }
        Ok(out)
    }

    /// Sequence numbers of the queue entries `name` was given to, ascending
    /// ([`Store::queue_push`], [`Store::land`]). The caller re-checks each
    /// entry: an entry may no longer carry the name.
    ///
    /// Complete only once [`Store::queue_names_indexed`] is true.
    pub fn queue_named(&self, name: ObjectId) -> Result<Vec<u64>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(QUEUE_NAMES).map_err(Error::index)?;
        match table
            .get(name.as_bytes().as_slice())
            .map_err(Error::index)?
        {
            Some(value) => decode_seqs(value.value()),
            None => Ok(Vec::new()),
        }
    }

    /// Whether [`Store::queue_named`] covers every queue entry. False only
    /// for a store whose queue predates the name index, until
    /// [`Store::index_queue_names`].
    pub fn queue_names_indexed(&self) -> Result<bool> {
        if self.queue_names_ready.load(Ordering::Acquire) {
            return Ok(true);
        }
        let ready = self.meta_get(META_QUEUE_NAMES)?.is_some();
        if ready {
            self.queue_names_ready.store(true, Ordering::Release);
        }
        Ok(ready)
    }

    /// Add `(seq, names)` for existing entries to the name index and mark it
    /// complete. The caller lists every entry it read with
    /// [`Store::queue_entries`]; entries pushed since carry their own names.
    pub fn index_queue_names(&self, rows: &[(u64, Vec<ObjectId>)]) -> Result<()> {
        let txn = self.db.begin_write().map_err(Error::index)?;
        add_names(
            &txn,
            rows.iter()
                .flat_map(|(seq, names)| names.iter().map(move |name| (*name, *seq))),
        )?;
        mark_names_indexed(&txn)?;
        txn.commit().map_err(Error::index)?;
        self.queue_names_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Record that `change`'s ops reproduce its result, so a lander in any
    /// process can skip checking them again. The id is content-addressed, so
    /// the fact never goes stale.
    ///
    /// Like [`Store::queue_set`], the commit does not fsync; losing it only
    /// costs a second check.
    pub fn mark_checked(&self, change: ChangeId) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::None);
        insert_checked(&txn, &[change])?;
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// Whether [`Store::mark_checked`] recorded `change`.
    pub fn is_checked(&self, change: ChangeId) -> Result<bool> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(CHECKED).map_err(Error::index)?;
        Ok(table
            .get(change.as_bytes().as_slice())
            .map_err(Error::index)?
            .is_some())
    }

    /// Land a change: in one durable redb commit, point `record.result` at
    /// its identity index, replace the queue entry, index its new names,
    /// append `change` to the log (after any buffered entries), move `head`
    /// to it, and add its `node_history` rows. Buffered refs are flushed in
    /// the same commit.
    ///
    /// Either all of it is durable or none of it is. Objects the rows name
    /// (the record, the identity index object) must already be stored.
    pub fn land(&self, landing: &Landing<'_>) -> Result<()> {
        let _guard = self.lock_index();
        let binding = match landing.identity_index {
            Some((snapshot, index)) => {
                Some((snapshot, self.identity_index_binding(snapshot, index)?))
            }
            None => None,
        };
        let nodes: BTreeSet<NodeId> = touched_nodes(landing.record);
        // Load the log before taking `pending_log`; loading takes it too.
        drop(self.ensure_landing_log()?);
        // `append_log` takes `pending_log` first, so holding it keeps the
        // log still until the commit and the in-memory append below.
        let mut refs = lock_map(&self.pending_refs);
        let mut pending = lock_vec(&self.pending_log);
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::Immediate);
        if let Some((snapshot, row)) = &binding {
            insert_identity_index_row(&txn, *snapshot, row)?;
        }
        let (seq, entry) = landing.entry;
        set_entry(&txn, seq, entry)?;
        add_names(&txn, landing.names.iter().map(|name| (*name, seq)))?;
        write_pending_refs(&txn, &refs)?;
        let mut log_rows = std::mem::take(&mut *pending);
        log_rows.push(landing.change);
        let written = write_pending_log(&txn, &log_rows);
        // Keep the buffered entries buffered if this commit fails.
        log_rows.pop();
        *pending = log_rows;
        written?;
        {
            let mut meta = txn.open_table(META).map_err(Error::index)?;
            meta.insert(META_HEAD, landing.change.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        let history = {
            let log = lock(&self.landing_log);
            let next = log.order.len();
            let pos_of = |id: &ChangeId| {
                log.first_pos
                    .get(id)
                    .copied()
                    .or_else(|| (*id == landing.change).then_some(next))
            };
            plan_history_rows(&txn, landing.change, &nodes, &pos_of)?
        };
        write_history_rows(&txn, &history)?;
        txn.commit().map_err(Error::index)?;
        refs.clear();
        pending.clear();
        lock(&self.landing_log).note(landing.change);
        Ok(())
    }
}

fn insert_checked(txn: &WriteTransaction, changes: &[ChangeId]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut table = txn.open_table(CHECKED).map_err(Error::index)?;
    for change in changes {
        table
            .insert(change.as_bytes().as_slice(), [0u8; 0].as_slice())
            .map_err(Error::index)?;
    }
    Ok(())
}

fn set_entry(txn: &WriteTransaction, seq: u64, entry: &[u8]) -> Result<()> {
    let mut table = txn.open_table(QUEUE).map_err(Error::index)?;
    if table.get(seq).map_err(Error::index)?.is_none() {
        return Err(Error::CorruptIndex("lander_queue: no such entry"));
    }
    table.insert(seq, entry).map_err(Error::index)?;
    Ok(())
}

/// Add each `(name, seq)` to `lander_queue_names`, keeping each list
/// ascending and free of repeats.
fn add_names(txn: &WriteTransaction, pairs: impl Iterator<Item = (ObjectId, u64)>) -> Result<()> {
    let mut table = txn.open_table(QUEUE_NAMES).map_err(Error::index)?;
    for (name, seq) in pairs {
        let key = name.as_bytes().as_slice();
        let mut seqs = match table.get(key).map_err(Error::index)? {
            Some(value) => decode_seqs(value.value())?,
            None => Vec::new(),
        };
        let Err(at) = seqs.binary_search(&seq) else {
            continue;
        };
        seqs.insert(at, seq);
        let bytes: Vec<u8> = seqs.iter().flat_map(|s| s.to_be_bytes()).collect();
        table.insert(key, bytes.as_slice()).map_err(Error::index)?;
    }
    Ok(())
}

fn decode_seqs(bytes: &[u8]) -> Result<Vec<u64>> {
    let (seqs, rest) = bytes.as_chunks::<8>();
    if !rest.is_empty() {
        return Err(Error::CorruptIndex("lander_queue_names"));
    }
    Ok(seqs.iter().map(|be| u64::from_be_bytes(*be)).collect())
}
