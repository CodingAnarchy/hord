//! Store contract used by import/export (spec §8.1).
//!
//! `hord-store` is written in parallel. These methods match the expected
//! `hord_store::Store` surface (`create`/`open`, `put`/`get`/`put_object`/
//! `get_object`, `append_log`/`log`, `set_head`/`head`, `set_ref`/`get_ref`).
//! [`MemoryStore`] is an in-memory stand-in for unit tests; integration tests
//! should use `hord_store::Store` once that crate compiles.

use std::collections::HashMap;
use std::path::Path;

use hord_core::{ChangeId, ObjectId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

/// Content-addressed object store plus the append-only change log.
///
/// Implement this for `hord_store::Store` when that crate is available.
pub trait Store {
    /// Write canonical bytes at `id`.
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> Result<(), Error>;

    /// Read canonical bytes for `id`.
    fn get(&self, id: ObjectId) -> Result<Vec<u8>, Error>;

    /// Canonical-encode `value`, store it, and return its [`ObjectId`].
    fn put_object<T: Serialize>(&mut self, value: &T) -> Result<ObjectId, Error> {
        let bytes = hord_encoding::encode(value)?;
        let id = ObjectId::from_canonical(&bytes);
        self.put(id, bytes)?;
        Ok(id)
    }

    /// Load and decode the object at `id`.
    fn get_object<T: DeserializeOwned>(&self, id: ObjectId) -> Result<T, Error> {
        let bytes = self.get(id)?;
        hord_encoding::decode(&bytes).map_err(|source| Error::UnexpectedObject {
            id,
            kind: std::any::type_name::<T>(),
            source,
        })
    }

    /// Append a landed change to the log (landing order).
    fn append_log(&mut self, change: ChangeId) -> Result<(), Error>;

    /// Landed changes in landing order.
    fn log(&self) -> Result<Vec<ChangeId>, Error>;

    /// Set the latest landed change.
    fn set_head(&mut self, change: ChangeId) -> Result<(), Error>;

    /// Latest landed change, if any.
    fn head(&self) -> Result<Option<ChangeId>, Error>;

    /// Point a named ref at an object.
    fn set_ref(&mut self, name: &str, id: ObjectId) -> Result<(), Error>;

    /// Resolve a named ref.
    fn get_ref(&self, name: &str) -> Result<Option<ObjectId>, Error>;
}

/// In-memory [`Store`] for tests while `hord-store` is a stub.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    objects: HashMap<ObjectId, Vec<u8>>,
    log: Vec<ChangeId>,
    head: Option<ChangeId>,
    refs: HashMap<String, ObjectId>,
}

impl MemoryStore {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new in-memory store. `repo_root` is ignored.
    pub fn create(_repo_root: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self::new())
    }

    /// Open an in-memory store. `repo_root` is ignored; always empty.
    pub fn open(_repo_root: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self::new())
    }
}

impl Store for MemoryStore {
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> Result<(), Error> {
        self.objects.insert(id, bytes);
        Ok(())
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>, Error> {
        self.objects.get(&id).cloned().ok_or(Error::Missing(id))
    }

    fn append_log(&mut self, change: ChangeId) -> Result<(), Error> {
        self.log.push(change);
        Ok(())
    }

    fn log(&self) -> Result<Vec<ChangeId>, Error> {
        Ok(self.log.clone())
    }

    fn set_head(&mut self, change: ChangeId) -> Result<(), Error> {
        self.head = Some(change);
        Ok(())
    }

    fn head(&self) -> Result<Option<ChangeId>, Error> {
        Ok(self.head)
    }

    fn set_ref(&mut self, name: &str, id: ObjectId) -> Result<(), Error> {
        self.refs.insert(name.to_owned(), id);
        Ok(())
    }

    fn get_ref(&self, name: &str) -> Result<Option<ObjectId>, Error> {
        Ok(self.refs.get(name).copied())
    }
}
