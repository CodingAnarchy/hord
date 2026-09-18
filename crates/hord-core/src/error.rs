//! Parse errors for `hord-core` types.

use thiserror::Error;

/// Failure to parse a [`NodeId`](crate::NodeId) or [`RepoPath`](crate::RepoPath).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A string was not a valid 26-character ULID [`NodeId`](crate::NodeId).
    #[error("invalid NodeId {0:?}")]
    NodeId(String),
    /// A string was not a valid repository-relative path.
    #[error("invalid repository path {0:?}")]
    RepoPath(String),
}
