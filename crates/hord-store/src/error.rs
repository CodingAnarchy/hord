//! Store errors.

use std::io;
use std::path::PathBuf;

use hord_core::ObjectId;
use thiserror::Error;

/// Failure to create, open, or use a [`crate::Store`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// [`crate::Store::create`] was called on a path that already has `.hord/`.
    #[error("hord store already exists at {}", .0.display())]
    AlreadyExists(PathBuf),
    /// [`crate::Store::open`] was called on a path with no `.hord/` store.
    #[error("no hord store at {}", .0.display())]
    MissingStore(PathBuf),
    /// No object with this id is in the store.
    #[error("object {0} not found")]
    MissingObject(ObjectId),
    /// A named ref was empty or contained a NUL byte.
    #[error("invalid ref name {0:?}")]
    InvalidRef(String),
    /// A workspace id string was not a 26-character ULID.
    #[error("invalid workspace id {0:?}")]
    WorkspaceId(String),
    /// Canonical CBOR encoding or decoding failed.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The redb index failed.
    #[error("index: {0}")]
    Index(String),
    /// Stored bytes did not hash to the requested [`ObjectId`].
    #[error("corrupt object {id}: {reason}")]
    Corrupt {
        /// Object that failed the hash check.
        id: ObjectId,
        /// Why the object is considered corrupt.
        reason: String,
    },
    /// A redb metadata value was the wrong length or missing required shape.
    #[error("corrupt index: {0}")]
    CorruptIndex(&'static str),
    /// An object (compressed or uncompressed) exceeded 4 GiB.
    #[error("object exceeds 4 GiB")]
    ObjectTooLarge,
    /// A pack file or its sidecar index was unreadable.
    #[error("invalid pack {}", .0.display())]
    InvalidPack(PathBuf),
}

impl Error {
    pub(crate) fn index(err: impl std::fmt::Display) -> Self {
        Self::Index(err.to_string())
    }
}
