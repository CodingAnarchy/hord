//! Store contract used by import/export (spec §8.1).
//!
//! [`hord_store::Store`] implements this trait. [`MemoryStore`] is an in-memory
//! stand-in for unit tests.

use std::collections::HashMap;

use hord_core::{ChangeId, ObjectId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

/// Content-addressed object store plus the append-only change log.
pub trait Store {
    /// Write canonical bytes at `id`, which must be their BLAKE3 hash.
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> Result<(), Error>;

    /// Write canonical bytes and return their [`ObjectId`].
    ///
    /// Stores that hash on write (like [`hord_store::Store`]) override this
    /// so the import path hashes each object once.
    fn put_bytes(&mut self, bytes: Vec<u8>) -> Result<ObjectId, Error> {
        let id = ObjectId::from_canonical(&bytes);
        self.put(id, bytes)?;
        Ok(id)
    }

    /// Read canonical bytes for `id`.
    fn get(&self, id: ObjectId) -> Result<Vec<u8>, Error>;

    /// Canonical-encode `value`, store it, and return its [`ObjectId`].
    fn put_object<T: Serialize>(&mut self, value: &T) -> Result<ObjectId, Error> {
        self.put_bytes(hord_encoding::encode(value)?)
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

/// In-memory [`Store`] for unit tests.
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

impl Store for hord_store::Store {
    fn put(&mut self, id: ObjectId, bytes: Vec<u8>) -> Result<(), Error> {
        let got = hord_store::Store::put(self, &bytes)?;
        if got != id {
            return Err(Error::Git(format!(
                "object id mismatch: store computed {got}, importer supplied {id}"
            )));
        }
        Ok(())
    }

    fn put_bytes(&mut self, bytes: Vec<u8>) -> Result<ObjectId, Error> {
        hord_store::Store::put(self, &bytes).map_err(Into::into)
    }

    fn get(&self, id: ObjectId) -> Result<Vec<u8>, Error> {
        hord_store::Store::get(self, id).map_err(Into::into)
    }

    fn append_log(&mut self, change: ChangeId) -> Result<(), Error> {
        hord_store::Store::append_log(self, change).map_err(Into::into)
    }

    fn log(&self) -> Result<Vec<ChangeId>, Error> {
        hord_store::Store::log(self).map_err(Into::into)
    }

    fn set_head(&mut self, change: ChangeId) -> Result<(), Error> {
        hord_store::Store::set_head(self, change).map_err(Into::into)
    }

    fn head(&self) -> Result<Option<ChangeId>, Error> {
        hord_store::Store::head(self).map_err(Into::into)
    }

    fn set_ref(&mut self, name: &str, id: ObjectId) -> Result<(), Error> {
        hord_store::Store::set_ref(self, name, id).map_err(Into::into)
    }

    fn get_ref(&self, name: &str) -> Result<Option<ObjectId>, Error> {
        hord_store::Store::get_ref(self, name).map_err(Into::into)
    }
}
