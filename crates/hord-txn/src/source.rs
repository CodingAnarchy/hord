//! [`ObjectSource`]: where a [`crate::Repo`] and its workspaces read objects
//! from (ADR 0024).
//!
//! A local repository reads its own [`Store`]; a remote workspace (M4
//! `hord-remote`) reads through a gRPC client with a local cache, fetching
//! only the objects it touches (spec §8.3). Everything that reads a
//! snapshot, tree, identity tree, blob, or change record goes through the
//! repository's local store first and then its source
//! ([`crate::RepoOptions::objects`]), keeping what the source returns in the
//! store as a cache; writes, the log, and the indexes stay on the local
//! store. A [`crate::Repo`] is itself an [`ObjectSource`] over both.

use hord_core::{ObjectId, Snapshot, SnapshotId};
use hord_store::Store;

use crate::{Error, Result};

/// Read access to content-addressed objects by id (ADR 0024).
///
/// Methods block: they are called from tokio's blocking pool, never from an
/// async context, so a remote implementation may wait on the network there
/// (for example with `Handle::block_on`). A missing object is
/// `Error::Store(hord_store::Error::MissingObject(id))`, whatever the
/// source, so callers classify it the same way.
pub trait ObjectSource: Send + Sync {
    /// Canonical CBOR of each id, in order. Any missing id fails the call.
    fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<Vec<u8>>>;

    /// Whether each id is available, in order.
    fn has(&self, ids: &[ObjectId]) -> Result<Vec<bool>>;

    /// Canonical CBOR of one object.
    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.get_objects(&[id])?
            .pop()
            .ok_or(Error::Store(hord_store::Error::MissingObject(id)))
    }

    /// The identity tree `snapshot` carries (ADR 0017), if it has one.
    fn snapshot_identity(&self, snapshot: SnapshotId) -> Result<Option<ObjectId>> {
        let bytes = self.get(snapshot)?;
        let snapshot: Snapshot = hord_encoding::decode(&bytes).map_err(|err| Error::Corrupt {
            id: snapshot,
            reason: format!("not a snapshot: {err}"),
        })?;
        Ok(snapshot.identity())
    }
}

impl ObjectSource for Store {
    fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<Vec<u8>>> {
        ids.iter().map(|id| Ok(Store::get(self, *id)?)).collect()
    }

    fn has(&self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        ids.iter().map(|id| Ok(self.contains(*id)?)).collect()
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        Ok(Store::get(self, id)?)
    }
}

impl ObjectSource for crate::Repo {
    fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<Vec<u8>>> {
        ids.iter().map(|id| self.inner.get_bytes(*id)).collect()
    }

    fn has(&self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        let local = ObjectSource::has(&self.inner.store, ids)?;
        let Some(source) = &self.inner.objects else {
            return Ok(local);
        };
        let missing: Vec<ObjectId> = ids
            .iter()
            .zip(&local)
            .filter(|(_, here)| !**here)
            .map(|(id, _)| *id)
            .collect();
        if missing.is_empty() {
            return Ok(local);
        }
        let mut remote = source.has(&missing)?.into_iter();
        Ok(local
            .into_iter()
            .map(|here| here || remote.next().unwrap_or(false))
            .collect())
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.inner.get_bytes(id)
    }
}
