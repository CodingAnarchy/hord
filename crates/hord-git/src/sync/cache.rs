//! Objects the bridge reads from the repository backend, kept in memory.
//!
//! Export and import are synchronous ([`Store`]); the backend is async. A
//! [`CacheStore`] reads from the cache and, on a miss, from the backend on
//! the runtime (it runs on the blocking pool). [`ObjectCache::prefetch`]
//! fills the cache a tree level at a time in batched calls first, so a miss
//! is rare.

use std::any::type_name;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hord_api::{ApiError, MAX_BATCH_BYTES, MAX_BATCH_IDS, RepoBackend, proto, wire};
use hord_core::{ChangeId, ChangeRecord, NodeFile, ObjectId, Snapshot, Tree, TreeEntry};
use serde::de::DeserializeOwned;
use tokio::runtime::Handle;

use super::SyncError;
use crate::leaf::GitLeaf;
use crate::{Error, Store};

/// Canonical bytes by id, shared by every step of one bridge.
pub(crate) struct ObjectCache {
    backend: Arc<dyn RepoBackend>,
    objects: Mutex<HashMap<ObjectId, Vec<u8>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ObjectCache {
    pub(crate) fn new(backend: Arc<dyn RepoBackend>) -> Self {
        Self {
            backend,
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, id: ObjectId) -> Option<Vec<u8>> {
        lock(&self.objects).get(&id).cloned()
    }

    /// Fetch whichever of `ids` are not cached, in batches.
    async fn fetch(&self, ids: &[ObjectId]) -> Result<(), SyncError> {
        let missing: Vec<ObjectId> = {
            let objects = lock(&self.objects);
            let mut seen = BTreeSet::new();
            ids.iter()
                .copied()
                .filter(|id| !objects.contains_key(id) && seen.insert(*id))
                .collect()
        };
        for chunk in missing.chunks(MAX_BATCH_IDS) {
            self.fetch_batch(chunk).await?;
        }
        Ok(())
    }

    /// One `GetObjects` call; halved while the reply is over the byte limit.
    fn fetch_batch<'a>(
        &'a self,
        ids: &'a [ObjectId],
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncError>> + Send + 'a>> {
        Box::pin(async move {
            let reply = self
                .backend
                .get_objects(proto::GetObjectsRequest {
                    ids: ids.iter().copied().map(wire::id).collect(),
                })
                .await;
            match reply {
                Ok(reply) => {
                    let mut objects = lock(&self.objects);
                    for object in reply.objects {
                        let id = wire::verified_object(&object)?;
                        objects.insert(id, object.cbor);
                    }
                    Ok(())
                }
                Err(ApiError::ResourceExhausted(_)) if ids.len() > 1 => {
                    let (head, tail) = ids.split_at(ids.len() / 2);
                    self.fetch_batch(head).await?;
                    self.fetch_batch(tail).await
                }
                Err(err) => Err(err.into()),
            }
        })
    }

    /// Fetch `changes`' records and everything export reads from their
    /// result snapshots: the content trees and their leaves, a level at a
    /// time.
    pub(crate) async fn prefetch(&self, changes: &[ChangeId]) -> Result<(), SyncError> {
        self.fetch(changes).await?;
        let mut snapshots = Vec::new();
        for change in changes {
            let record: ChangeRecord = self.decode(*change)?;
            snapshots.push(record.result);
        }
        self.fetch(&snapshots).await?;
        let mut trees = Vec::new();
        for id in snapshots {
            let bytes = self.cached(id).ok_or(Error::Missing(id))?;
            // ADR 0017: a snapshot, or a root tree id.
            trees.push(hord_encoding::decode::<Snapshot>(&bytes).map_or(id, |s| s.tree));
        }
        let mut seen = BTreeSet::new();
        while !trees.is_empty() {
            trees.retain(|id| seen.insert(*id));
            self.fetch(&trees).await?;
            let mut next = Vec::new();
            let mut leaves = Vec::new();
            for id in &trees {
                let tree: Tree = self.decode(*id)?;
                for entry in tree.entries.values() {
                    match entry {
                        TreeEntry::Tree(child) => next.push(*child),
                        TreeEntry::Blob(leaf) | TreeEntry::NodeFile(leaf) => leaves.push(*leaf),
                    }
                }
            }
            self.fetch(&leaves).await?;
            // A leaf that wraps a blob (a git mode, a parsed file) names it.
            let mut inner = Vec::new();
            for id in leaves {
                let Some(bytes) = self.cached(id) else {
                    continue;
                };
                if let Ok(leaf) = hord_encoding::decode::<GitLeaf>(&bytes) {
                    inner.push(leaf.blob);
                } else if let Ok(file) = hord_encoding::decode::<NodeFile>(&bytes) {
                    inner.push(file.raw_hash);
                }
            }
            self.fetch(&inner).await?;
            trees = next;
        }
        Ok(())
    }

    fn decode<T: DeserializeOwned>(&self, id: ObjectId) -> Result<T, SyncError> {
        let bytes = self.cached(id).ok_or(Error::Missing(id))?;
        hord_encoding::decode(&bytes).map_err(|source| {
            Error::UnexpectedObject {
                id,
                kind: type_name::<T>(),
                source,
            }
            .into()
        })
    }

    /// Store on the backend each of `ids` it lacks, in batches.
    pub(crate) async fn upload(&self, ids: &[ObjectId]) -> Result<(), SyncError> {
        for chunk in ids.chunks(MAX_BATCH_IDS) {
            let has = self
                .backend
                .has(proto::HasRequest {
                    ids: chunk.iter().copied().map(wire::id).collect(),
                })
                .await?;
            let missing: Vec<ObjectId> = chunk
                .iter()
                .zip(has.present)
                .filter(|(_, present)| !present)
                .map(|(id, _)| *id)
                .collect();
            let mut batch = Vec::new();
            let mut bytes = 0;
            for id in missing {
                let cbor = self.cached(id).ok_or(Error::Missing(id))?;
                if !batch.is_empty() && bytes + cbor.len() > MAX_BATCH_BYTES {
                    self.put(mem::take(&mut batch)).await?;
                    bytes = 0;
                }
                bytes += cbor.len();
                batch.push(wire::object(cbor));
            }
            if !batch.is_empty() {
                self.put(batch).await?;
            }
        }
        Ok(())
    }

    async fn put(&self, objects: Vec<proto::Object>) -> Result<(), SyncError> {
        self.backend
            .put_objects(proto::PutObjectsRequest { objects })
            .await?;
        Ok(())
    }
}

/// A [`Store`] over an [`ObjectCache`] for one synchronous step, on the
/// blocking pool. Objects it writes are cached and listed in
/// [`Self::written`]; it has no log, head, or refs.
pub(crate) struct CacheStore {
    cache: Arc<ObjectCache>,
    runtime: Handle,
    pub(crate) written: Vec<ObjectId>,
}

impl CacheStore {
    pub(crate) fn new(cache: Arc<ObjectCache>, runtime: Handle) -> Self {
        Self {
            cache,
            runtime,
            written: Vec::new(),
        }
    }
}

fn no_log() -> Error {
    Error::Git("the bridge's object cache has no log or refs".into())
}

impl Store for CacheStore {
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> Result<(), Error> {
        let fresh = lock(&self.cache.objects).insert(id, bytes).is_none();
        if fresh {
            self.written.push(id);
        }
        Ok(())
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>, Error> {
        if let Some(bytes) = self.cache.cached(id) {
            return Ok(bytes);
        }
        self.runtime
            .block_on(self.cache.fetch(&[id]))
            .map_err(|err| Error::Git(format!("fetch {id}: {err}")))?;
        self.cache.cached(id).ok_or(Error::Missing(id))
    }

    fn append_log(&mut self, _change: ChangeId) -> Result<(), Error> {
        Err(no_log())
    }

    fn log(&self) -> Result<Vec<ChangeId>, Error> {
        Err(no_log())
    }

    fn set_head(&mut self, _change: ChangeId) -> Result<(), Error> {
        Err(no_log())
    }

    fn head(&self) -> Result<Option<ChangeId>, Error> {
        Err(no_log())
    }

    fn set_ref(&mut self, _name: &str, _id: ObjectId) -> Result<(), Error> {
        Err(no_log())
    }

    fn get_ref(&self, _name: &str) -> Result<Option<ObjectId>, Error> {
        Ok(None)
    }
}
