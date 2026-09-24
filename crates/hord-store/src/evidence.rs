//! The evidence index (spec §7.1, ADR 0025): evidence beside snapshots.
//!
//! Evidence objects are stored like any other object. The
//! `evidence_by_snapshot` table indexes them by the reuse key
//! `(snapshot, toolchain, command, scope)`, so a verifier finds evidence
//! for the exact inputs it would run and skips the run (§7.1 item 1), and
//! policy lists the evidence for a snapshot. A row is
//! `snapshot || key hash || evidence id` with an empty value; the key hash
//! is the [`ObjectId`] of the canonical CBOR of `(toolchain, command,
//! scope)`, so the one table answers both lookups with a range scan.
//!
//! Evidence is never written into a change record by this index (ADR 0025):
//! verification evidence is attached to the snapshot it verified.

use std::collections::BTreeSet;

use hord_core::{Evidence, NodeId, ObjectId, SnapshotId};
use redb::TableDefinition;

use super::Store;
use crate::{Error, Result};

/// Created empty by `init_tables` for new stores.
pub(super) const EVIDENCE: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("evidence_by_snapshot");

const LEN: usize = ObjectId::LEN;
const ROW_LEN: usize = 3 * LEN;

/// Hash of the non-snapshot part of the reuse key.
fn key_hash(
    toolchain: ObjectId,
    command: &str,
    scope: Option<&BTreeSet<NodeId>>,
) -> Result<ObjectId> {
    Ok(ObjectId::of(&(
        "hord/evidence-key",
        toolchain,
        command,
        scope,
    ))?)
}

fn row(snapshot: SnapshotId, key: ObjectId, id: ObjectId) -> [u8; ROW_LEN] {
    let mut out = [0u8; ROW_LEN];
    out[..LEN].copy_from_slice(snapshot.as_bytes());
    out[LEN..2 * LEN].copy_from_slice(key.as_bytes());
    out[2 * LEN..].copy_from_slice(id.as_bytes());
    out
}

impl Store {
    /// Store `evidence` as an object and index it under its reuse key.
    ///
    /// Idempotent: the same evidence value has the same id and one row.
    pub fn put_evidence(&self, evidence: &Evidence) -> Result<ObjectId> {
        Ok(self.put_evidence_batch(std::slice::from_ref(evidence))?[0])
    }

    /// Store every one of `evidence` and index them all in one write
    /// transaction: one durable commit for a verification's evidence
    /// instead of one per piece. Ids in input order.
    pub fn put_evidence_batch(&self, evidence: &[Evidence]) -> Result<Vec<ObjectId>> {
        let mut ids = Vec::with_capacity(evidence.len());
        let mut rows = Vec::with_capacity(evidence.len());
        for piece in evidence {
            let id = self.put_object(piece)?;
            let key = key_hash(piece.toolchain, &piece.command, piece.scope.as_ref())?;
            rows.push(row(piece.snapshot, key, id));
            ids.push(id);
        }
        if rows.is_empty() {
            return Ok(ids);
        }
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(EVIDENCE).map_err(Error::index)?;
            for row in &rows {
                table
                    .insert(row.as_slice(), [].as_slice())
                    .map_err(Error::index)?;
            }
        }
        txn.commit().map_err(Error::index)?;
        Ok(ids)
    }

    /// Evidence indexed for exactly `(snapshot, toolchain, command, scope)`,
    /// in [`ObjectId`] order. Empty when nothing matches.
    pub fn evidence_for_key(
        &self,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        command: &str,
        scope: Option<&BTreeSet<NodeId>>,
    ) -> Result<Vec<ObjectId>> {
        let key = key_hash(toolchain, command, scope)?;
        let mut prefix = [0u8; 2 * LEN];
        prefix[..LEN].copy_from_slice(snapshot.as_bytes());
        prefix[LEN..].copy_from_slice(key.as_bytes());
        self.evidence_rows(&prefix)
    }

    /// Every evidence object indexed for `snapshot`, in key order.
    pub fn evidence_at(&self, snapshot: SnapshotId) -> Result<Vec<ObjectId>> {
        self.evidence_rows(snapshot.as_bytes())
    }

    fn evidence_rows(&self, prefix: &[u8]) -> Result<Vec<ObjectId>> {
        let txn = self.db.begin_read().map_err(Error::index)?;
        let table = match txn.open_table(EVIDENCE) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(Error::index(err)),
        };
        let mut start = [0u8; ROW_LEN];
        start[..prefix.len()].copy_from_slice(prefix);
        let mut end = [0xffu8; ROW_LEN];
        end[..prefix.len()].copy_from_slice(prefix);
        let mut out = Vec::new();
        for entry in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(Error::index)?
        {
            let (key, _) = entry.map_err(Error::index)?;
            let key = key.value();
            if key.len() != ROW_LEN || !key.starts_with(prefix) {
                return Err(Error::CorruptIndex("evidence_by_snapshot"));
            }
            out.push(ObjectId::try_from(&key[2 * LEN..])?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hord_core::{Actor, EvidenceKind, EvidenceResult, Timestamp};

    use super::*;

    struct Repo(PathBuf, Option<Store>);

    impl Drop for Repo {
        fn drop(&mut self) {
            self.1.take();
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn repo() -> std::result::Result<Repo, Box<dyn std::error::Error>> {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hord-store-evidence-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path)?;
        let store = Store::create(&path)?;
        Ok(Repo(path, Some(store)))
    }

    fn id(n: u8) -> ObjectId {
        ObjectId::from_bytes([n; 32])
    }

    fn evidence(snapshot: u8, command: &str, scope: Option<&[u128]>) -> Evidence {
        Evidence {
            kind: EvidenceKind::Test,
            qualifier: None,
            snapshot: id(snapshot),
            toolchain: id(9),
            command: command.to_owned(),
            scope: scope.map(|s| s.iter().copied().map(NodeId::from_u128).collect()),
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 5,
            produced_by: Actor::Human { id: "t".into() },
            produced_at: Timestamp::from_millis(1),
        }
    }

    /// A batch indexes like one `put_evidence` per piece, in one commit,
    /// and survives a reopen (the commit is durable).
    #[test]
    fn a_batch_indexes_every_piece() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut repo = repo()?;
        let batch = [
            evidence(1, "cargo check", None),
            evidence(1, "cargo test", Some(&[3])),
            evidence(2, "cargo check", None),
        ];
        let ids = repo
            .1
            .as_ref()
            .ok_or("the repo holds an open store")?
            .put_evidence_batch(&batch)?;
        repo.1.take();
        let store = Store::open(&repo.0)?;
        for (piece, id) in batch.iter().zip(&ids) {
            assert_eq!(*id, ObjectId::of(piece)?);
            assert_eq!(
                store.evidence_for_key(
                    piece.snapshot,
                    piece.toolchain,
                    &piece.command,
                    piece.scope.as_ref()
                )?,
                vec![*id]
            );
        }
        assert_eq!(store.evidence_at(id(1))?.len(), 2);
        assert!(store.put_evidence_batch(&[])?.is_empty());
        repo.1 = Some(store);
        Ok(())
    }

    #[test]
    fn finds_evidence_by_exact_key_only() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let repo = repo()?;
        let store = repo.1.as_ref().ok_or("the repo holds an open store")?;
        let a = evidence(1, "cargo test", Some(&[1, 2]));
        let a_id = store.put_evidence(&a)?;
        let scope: BTreeSet<NodeId> = [1, 2].into_iter().map(NodeId::from_u128).collect();
        assert_eq!(
            store.evidence_for_key(id(1), id(9), "cargo test", Some(&scope))?,
            vec![a_id]
        );
        // Every other component of the key misses.
        assert!(
            store
                .evidence_for_key(id(2), id(9), "cargo test", Some(&scope))?
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(8), "cargo test", Some(&scope))?
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(9), "cargo check", Some(&scope))?
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(9), "cargo test", None)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn lists_a_snapshots_evidence_and_is_idempotent()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let repo = repo()?;
        let store = repo.1.as_ref().ok_or("the repo holds an open store")?;
        let a = store.put_evidence(&evidence(1, "a", None))?;
        let b = store.put_evidence(&evidence(1, "b", Some(&[3])))?;
        store.put_evidence(&evidence(2, "a", None))?;
        assert_eq!(store.put_evidence(&evidence(1, "a", None))?, a);
        let mut at = store.evidence_at(id(1))?;
        at.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(at, want);
        assert_eq!(store.evidence_at(id(3))?, Vec::<ObjectId>::new());
        Ok(())
    }
}
