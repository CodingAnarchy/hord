//! Tables for `hord-txn` (spec §6.1, §6.7): the lander queue and the
//! per-snapshot identity index pointer.
//!
//! The store does not interpret either value. `hord-txn` owns the encoding of
//! a queue entry and of the identity index object.

use hord_core::{ObjectId, SnapshotId};
use redb::{Database, Durability, ReadableTable, TableDefinition, WriteTransaction};

use super::Store;
use crate::{Error, Result};

const QUEUE: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("lander_queue");
const IDENTITY_INDEX: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("identity_index");

pub(super) fn open_tables(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(QUEUE).map_err(Error::index)?;
    txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
    Ok(())
}

pub(super) fn ensure_tables(db: &Database) -> Result<()> {
    let txn = db.begin_write().map_err(Error::index)?;
    open_tables(&txn)?;
    txn.commit().map_err(Error::index)?;
    Ok(())
}

impl Store {
    /// Append an entry to the persistent lander queue (spec §6.7) and return
    /// its sequence number. Sequence numbers start at 0 and only grow.
    ///
    /// The write is durable when this returns.
    pub fn queue_push(&self, entry: &[u8]) -> Result<u64> {
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
        txn.commit().map_err(Error::index)?;
        Ok(seq)
    }

    /// Replace the queue entry at `seq`.
    ///
    /// The commit does not fsync. It becomes durable with the next durable
    /// commit, such as [`Store::set_head`], [`Store::queue_push`], or
    /// [`Store::flush`] with pending refs or log entries. A lander that
    /// crashes before then sees the previous entry and processes it again.
    pub fn queue_set(&self, seq: u64, entry: &[u8]) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(QUEUE).map_err(Error::index)?;
            if table.get(seq).map_err(Error::index)?.is_none() {
                return Err(Error::CorruptIndex("lander_queue: no such entry"));
            }
            table.insert(seq, entry).map_err(Error::index)?;
        }
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

    /// Point `snapshot` at its identity index object.
    ///
    /// `hord-txn` stores, per snapshot, which files carry [`hord_core::NodeId`]s
    /// that differ from a fresh assignment. Replaces any previous pointer.
    /// Like [`Store::queue_set`], the commit is made durable by the next
    /// durable commit; the lander writes it before [`Store::set_head`].
    pub fn set_identity_index(&self, snapshot: SnapshotId, index: ObjectId) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(Error::index)?;
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
            table
                .insert(snapshot.as_bytes().as_slice(), index.as_bytes().as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
    }

    /// The identity index object recorded for `snapshot`, if any.
    pub fn identity_index(&self, snapshot: SnapshotId) -> Result<Option<ObjectId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = txn.open_table(IDENTITY_INDEX).map_err(Error::index)?;
        match table
            .get(snapshot.as_bytes().as_slice())
            .map_err(Error::index)?
        {
            Some(value) => Ok(Some(ObjectId::try_from(value.value())?)),
            None => Ok(None),
        }
    }
}
