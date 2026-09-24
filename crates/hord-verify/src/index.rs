//! The evidence index (spec §7.1 item 1, ADR 0025).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use hord_core::{Blob, Evidence, NodeId, ObjectId, SnapshotId};
use hord_store::Store;

use crate::{Error, Result};

/// Where evidence and its logs are stored and found again.
///
/// Evidence is indexed by `(snapshot, toolchain, command, scope)`: the
/// inputs that decide its result (§7.1). [`Store`] implements it
/// durably; [`MemoryIndex`] in memory.
pub trait EvidenceIndex: Send + Sync {
    /// Store canonical CBOR bytes as an object.
    fn put_raw(&self, canonical_cbor: &[u8]) -> Result<ObjectId>;
    /// Load an object's canonical CBOR bytes.
    fn get_raw(&self, id: ObjectId) -> Result<Vec<u8>>;
    /// Store `evidence` and index it under its key.
    fn put_evidence(&self, evidence: &Evidence) -> Result<ObjectId>;
    /// Store and index several pieces of evidence, ids in input order. The
    /// default puts them one at a time; an index that can commit them
    /// together (the store: one durable commit) overrides it.
    fn put_evidence_batch(&self, evidence: &[Evidence]) -> Result<Vec<ObjectId>> {
        evidence.iter().map(|e| self.put_evidence(e)).collect()
    }
    /// Evidence indexed under exactly this key.
    fn evidence_for_key(
        &self,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        command: &str,
        scope: Option<&BTreeSet<NodeId>>,
    ) -> Result<Vec<ObjectId>>;
    /// Every evidence object indexed for `snapshot`.
    fn evidence_at(&self, snapshot: SnapshotId) -> Result<Vec<ObjectId>>;
}

/// Store `bytes` as a [`Blob`] (an evidence log) and return its id.
pub fn put_log(index: &dyn EvidenceIndex, bytes: &[u8]) -> Result<ObjectId> {
    index.put_raw(&hord_encoding::encode(&Blob::new(bytes.to_vec()))?)
}

/// Load the [`Blob`] `id` (an evidence log).
pub fn get_log(index: &dyn EvidenceIndex, id: ObjectId) -> Result<Vec<u8>> {
    let blob: Blob = hord_encoding::decode(&index.get_raw(id)?)?;
    Ok(blob.bytes.as_slice().to_vec())
}

/// Load the [`Evidence`] object `id`.
pub fn get_evidence(index: &dyn EvidenceIndex, id: ObjectId) -> Result<Evidence> {
    Ok(hord_encoding::decode(&index.get_raw(id)?)?)
}

impl EvidenceIndex for Store {
    fn put_raw(&self, canonical_cbor: &[u8]) -> Result<ObjectId> {
        Ok(self.put(canonical_cbor)?)
    }

    fn get_raw(&self, id: ObjectId) -> Result<Vec<u8>> {
        match self.get(id) {
            Err(hord_store::Error::MissingObject(id)) => Err(Error::MissingObject(id)),
            other => Ok(other?),
        }
    }

    fn put_evidence(&self, evidence: &Evidence) -> Result<ObjectId> {
        Ok(Store::put_evidence(self, evidence)?)
    }

    fn put_evidence_batch(&self, evidence: &[Evidence]) -> Result<Vec<ObjectId>> {
        Ok(Store::put_evidence_batch(self, evidence)?)
    }

    fn evidence_for_key(
        &self,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        command: &str,
        scope: Option<&BTreeSet<NodeId>>,
    ) -> Result<Vec<ObjectId>> {
        Ok(Store::evidence_for_key(
            self, snapshot, toolchain, command, scope,
        )?)
    }

    fn evidence_at(&self, snapshot: SnapshotId) -> Result<Vec<ObjectId>> {
        Ok(Store::evidence_at(self, snapshot)?)
    }
}

/// Key of one index row in [`MemoryIndex`].
type Row = (SnapshotId, ObjectId, ObjectId);

/// An in-memory [`EvidenceIndex`], for tests and dry runs.
#[derive(Debug, Default)]
pub struct MemoryIndex {
    objects: Mutex<HashMap<ObjectId, Vec<u8>>>,
    rows: Mutex<BTreeSet<Row>>,
}

impl MemoryIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn key(
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

    fn rows(&self, snapshot: SnapshotId, key: Option<ObjectId>) -> Vec<ObjectId> {
        let rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        rows.iter()
            .filter(|(s, k, _)| *s == snapshot && key.is_none_or(|key| *k == key))
            .map(|(_, _, id)| *id)
            .collect()
    }
}

impl EvidenceIndex for MemoryIndex {
    fn put_raw(&self, canonical_cbor: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::from_canonical(canonical_cbor);
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, canonical_cbor.to_vec());
        Ok(id)
    }

    fn get_raw(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .ok_or(Error::MissingObject(id))
    }

    fn put_evidence(&self, evidence: &Evidence) -> Result<ObjectId> {
        let id = self.put_raw(&hord_encoding::encode(evidence)?)?;
        let key = Self::key(
            evidence.toolchain,
            &evidence.command,
            evidence.scope.as_ref(),
        )?;
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((evidence.snapshot, key, id));
        Ok(id)
    }

    fn evidence_for_key(
        &self,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        command: &str,
        scope: Option<&BTreeSet<NodeId>>,
    ) -> Result<Vec<ObjectId>> {
        Ok(self.rows(snapshot, Some(Self::key(toolchain, command, scope)?)))
    }

    fn evidence_at(&self, snapshot: SnapshotId) -> Result<Vec<ObjectId>> {
        Ok(self.rows(snapshot, None))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hord_core::{Actor, EvidenceKind, EvidenceResult, Timestamp};

    use super::*;

    fn ev(snapshot: u8, command: &str) -> Evidence {
        crate::EvidenceFields {
            kind: EvidenceKind::Check,
            qualifier: None,
            snapshot: ObjectId::from_bytes([snapshot; 32]),
            toolchain: ObjectId::from_bytes([7; 32]),
            command: command.to_owned(),
            scope: None,
            result: EvidenceResult::Pass,
            log: None,
            cost_ms: 1,
            produced_by: Actor::Human { id: "t".into() },
            produced_at: Timestamp::from_millis(3),
        }
        .build()
    }

    fn exercise(index: &dyn EvidenceIndex) {
        let log = put_log(index, b"hello").unwrap();
        assert_eq!(get_log(index, log).unwrap(), b"hello");
        let a = index.put_evidence(&ev(1, "cargo check")).unwrap();
        index.put_evidence(&ev(2, "cargo check")).unwrap();
        assert_eq!(get_evidence(index, a).unwrap(), ev(1, "cargo check"));
        let snap = ObjectId::from_bytes([1; 32]);
        let tc = ObjectId::from_bytes([7; 32]);
        assert_eq!(
            index
                .evidence_for_key(snap, tc, "cargo check", None)
                .unwrap(),
            vec![a]
        );
        assert!(
            index
                .evidence_for_key(snap, tc, "cargo test", None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(index.evidence_at(snap).unwrap(), vec![a]);
        // A batch: the same ids and rows as putting each alone.
        let batch = [ev(3, "cargo check"), ev(3, "cargo clippy")];
        let ids = index.put_evidence_batch(&batch).unwrap();
        let expected: Vec<ObjectId> = batch.iter().map(|e| ObjectId::of(e).unwrap()).collect();
        assert_eq!(ids, expected);
        let mut at = index.evidence_at(ObjectId::from_bytes([3; 32])).unwrap();
        at.sort();
        let mut want = expected.clone();
        want.sort();
        assert_eq!(at, want);
        assert_eq!(get_evidence(index, ids[1]).unwrap(), batch[1]);
        assert!(index.put_evidence_batch(&[]).unwrap().is_empty());
        assert!(matches!(
            index.get_raw(ObjectId::from_bytes([0; 32])),
            Err(Error::MissingObject(_))
        ));
    }

    #[test]
    fn memory_index_round_trips() {
        exercise(&MemoryIndex::new());
    }

    #[test]
    fn store_index_round_trips() {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hord-verify-index-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        {
            let store = Store::create(&dir).unwrap();
            exercise(&store);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
