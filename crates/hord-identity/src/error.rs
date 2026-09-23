//! Failures from identity carrying and snapshot views.

use hord_core::{NodeId, ObjectId};
use thiserror::Error;

/// A declaration could not be applied, or a snapshot view was inconsistent.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A declaration named a result [`ObjectId`] that is not a definition.
    #[error("node {0} is not a definition in the result tree")]
    UnknownResult(ObjectId),
    /// The same result definition was named by two declarations.
    #[error("conflicting declarations for result node {0}")]
    Conflict(ObjectId),
    /// Two declarations adopted the same base [`NodeId`], or a split source
    /// is still live on a definition that is not one of the pieces.
    #[error("node id {0} is claimed by more than one declaration")]
    Claimed(NodeId),
    /// [`crate::Declaration::SplitInto`] listed no result definitions.
    #[error("split of {0} needs at least one result node")]
    EmptySplit(NodeId),
    /// [`crate::Declaration::MergedFrom`] listed no source identities.
    #[error("merge into {0} needs at least one source")]
    EmptyMerge(ObjectId),
    /// The same [`NodeId`] was located at two paths, or assigned twice.
    #[error("duplicate node id {0}")]
    DuplicateNode(NodeId),
    /// An identified content id is not interned in its file.
    #[error("node {0} is not interned")]
    MissingNode(ObjectId),
    /// A [`NodeId`] is in the file's id map but not reachable from the root.
    #[error("node id {0} is not reachable in the snapshot")]
    Unmapped(NodeId),
}
