//! Errors from structural apply.

use hord_core::{NodeId, ObjectId};
use hord_lang::ParseError;
use thiserror::Error;

/// Failure to apply an edit script or intern a spliced tree.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An [`ObjectId`] was not interned in the working tree or node store.
    #[error("node {0} is not interned")]
    MissingNode(ObjectId),
    /// A [`NodeId`] is not in the working identity map.
    #[error("unknown node id {0}")]
    MissingId(NodeId),
    /// The edit script could not be applied.
    #[error("{0}")]
    Apply(String),
    /// A [`hord_core::Op::Replace`] expected different content at its node:
    /// the tree it is applied to is not the one the op was diffed from.
    #[error("replace of {node} expected content {expected}, found {found}")]
    StaleReplace {
        /// The replaced definition (or file root).
        node: NodeId,
        /// The op's `from`.
        expected: ObjectId,
        /// The content at `node` in the tree being edited.
        found: ObjectId,
    },
    /// CST intern or concat failed while splicing.
    #[error(transparent)]
    Parse(#[from] ParseError),
}

impl Error {
    pub(crate) fn apply(message: impl Into<String>) -> Self {
        Self::Apply(message.into())
    }
}
