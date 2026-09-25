//! Store errors.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

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
    /// Another process held the store's index lock for the whole lock
    /// timeout (`HORD_LOCK_TIMEOUT`, ADR 0021).
    #[error(
        "hord store is locked by {}: {} (waited {:.1}s; set HORD_LOCK_TIMEOUT to wait longer)",
        holder_name(*.holder),
        .lock.display(),
        .waited.as_secs_f64()
    )]
    Locked {
        /// The locked index file.
        lock: PathBuf,
        /// Pid of the process holding it, when known and still running.
        holder: Option<u32>,
        /// How long the open waited before giving up.
        waited: Duration,
    },
    /// `HORD_LOCK_TIMEOUT` was not a non-negative number of seconds.
    #[error("invalid HORD_LOCK_TIMEOUT {0:?}: expected seconds, such as 30 or 0")]
    LockTimeout(String),
    /// No object with this id is in the store.
    #[error("object {0} not found")]
    MissingObject(ObjectId),
    /// [`crate::Store::index_change`] was given a change that is not in the landing log.
    #[error("change {0} is not in the landing log")]
    NotInLog(hord_core::ChangeId),
    /// The store was written by an older (or newer) hord whose object
    /// format differs: ADR 0017 made a snapshot id the id of a `Snapshot`
    /// object, so older stores are not read. There is no migration.
    #[error(
        "hord store has format {}, but this hord reads format {expected}; \
         re-create it (for example `hord init --from-git`)",
        .found.map_or_else(|| "1 (unversioned)".to_owned(), |v| v.to_string())
    )]
    StoreFormat {
        /// Format recorded in the store; `None` before formats were recorded.
        found: Option<u32>,
        /// Format this build reads and writes.
        expected: u32,
    },
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
}

fn holder_name(holder: Option<u32>) -> String {
    holder.map_or_else(|| "another process".to_owned(), |pid| format!("pid {pid}"))
}

impl Error {
    pub(crate) fn index(err: impl std::fmt::Display) -> Self {
        Self::Index(err.to_string())
    }
}
