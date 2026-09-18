//! Errors from git import/export.

use std::path::PathBuf;

use hord_core::ObjectId;
use thiserror::Error;

/// Failure to import or export a git repository (spec §9).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A gix operation failed.
    #[error("git error: {0}")]
    Git(String),
    /// Canonical encoding or [`ObjectId`](hord_core::ObjectId) parsing failed.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
    /// The requested object is not in the store.
    #[error("missing object {0}")]
    Missing(ObjectId),
    /// A stored object could not be decoded as the expected type.
    #[error("object {id} is not a valid {kind}: {source}")]
    UnexpectedObject {
        /// Object that failed to decode.
        id: ObjectId,
        /// Expected type name.
        kind: &'static str,
        /// Decode error.
        #[source]
        source: hord_encoding::Error,
    },
    /// A git path was not valid UTF-8.
    #[error("git path is not valid UTF-8: {0:?}")]
    PathEncoding(String),
    /// The git repository has no commits to import.
    #[error("git repository has no commits (unborn HEAD or missing ref {0:?})")]
    EmptyHistory(String),
    /// A named git ref could not be resolved.
    #[error("git ref {0:?} not found")]
    MissingRef(String),
    /// Filesystem error while creating a git directory.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn git(err: impl std::fmt::Display) -> Self {
        Self::Git(err.to_string())
    }
}

impl From<hord_store::Error> for Error {
    fn from(err: hord_store::Error) -> Self {
        match err {
            hord_store::Error::MissingObject(id) => Self::Missing(id),
            hord_store::Error::Encoding(e) => Self::Encoding(e),
            other => Self::Git(other.to_string()),
        }
    }
}
