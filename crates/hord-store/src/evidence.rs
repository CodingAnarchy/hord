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

/// Same table `init_tables` creates for new stores.
const EVIDENCE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("evidence_by_snapshot");

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
        let id = self.put_object(evidence)?;
        self.index_evidence_row(id, evidence)?;
        Ok(id)
    }

    /// Index an evidence object that is already stored (for example one an
    /// author attached at `propose`, or one fetched from a remote).
    ///
    /// Fails with [`Error::MissingObject`] if `id` is not stored, and with
    /// an encoding error if it is not an [`Evidence`] object.
    pub fn index_evidence(&self, id: ObjectId) -> Result<()> {
        let evidence: Evidence = self.get_object(id)?;
        self.index_evidence_row(id, &evidence)
    }

    fn index_evidence_row(&self, id: ObjectId, evidence: &Evidence) -> Result<()> {
        let key = key_hash(
            evidence.toolchain,
            &evidence.command,
            evidence.scope.as_ref(),
        )?;
        let row = row(evidence.snapshot, key, id);
        let txn = self.db.begin_write().map_err(Error::index)?;
        {
            let mut table = txn.open_table(EVIDENCE).map_err(Error::index)?;
            table
                .insert(row.as_slice(), [].as_slice())
                .map_err(Error::index)?;
        }
        txn.commit().map_err(Error::index)?;
        Ok(())
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

    fn repo() -> Repo {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hord-store-evidence-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        let store = Store::create(&path).unwrap();
        Repo(path, Some(store))
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

    #[test]
    fn finds_evidence_by_exact_key_only() {
        let repo = repo();
        let store = repo.1.as_ref().unwrap();
        let a = evidence(1, "cargo test", Some(&[1, 2]));
        let a_id = store.put_evidence(&a).unwrap();
        let scope: BTreeSet<NodeId> = [1, 2].into_iter().map(NodeId::from_u128).collect();
        assert_eq!(
            store
                .evidence_for_key(id(1), id(9), "cargo test", Some(&scope))
                .unwrap(),
            vec![a_id]
        );
        // Every other component of the key misses.
        assert!(
            store
                .evidence_for_key(id(2), id(9), "cargo test", Some(&scope))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(8), "cargo test", Some(&scope))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(9), "cargo check", Some(&scope))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .evidence_for_key(id(1), id(9), "cargo test", None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lists_a_snapshots_evidence_and_is_idempotent() {
        let repo = repo();
        let store = repo.1.as_ref().unwrap();
        let a = store.put_evidence(&evidence(1, "a", None)).unwrap();
        let b = store.put_evidence(&evidence(1, "b", Some(&[3]))).unwrap();
        store.put_evidence(&evidence(2, "a", None)).unwrap();
        assert_eq!(store.put_evidence(&evidence(1, "a", None)).unwrap(), a);
        let mut at = store.evidence_at(id(1)).unwrap();
        at.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(at, want);
        assert_eq!(store.evidence_at(id(3)).unwrap(), Vec::<ObjectId>::new());
    }

    #[test]
    fn indexes_stored_objects_and_survives_reopen() {
        let mut repo = repo();
        let ev = evidence(4, "cargo clippy", None);
        let stored = {
            let store = repo.1.as_ref().unwrap();
            let stored = store.put_object(&ev).unwrap();
            assert!(store.evidence_at(id(4)).unwrap().is_empty());
            store.index_evidence(stored).unwrap();
            assert!(matches!(
                store.index_evidence(id(77)),
                Err(Error::MissingObject(_))
            ));
            stored
        };
        repo.1.take();
        let store = Store::open(&repo.0).unwrap();
        assert_eq!(
            store
                .evidence_for_key(id(4), id(9), "cargo clippy", None)
                .unwrap(),
            vec![stored]
        );
        repo.1 = Some(store);
    }
}
