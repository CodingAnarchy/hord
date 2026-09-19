//! Parse and CST construction errors.

use hord_core::{NodeKind, ObjectId};
use thiserror::Error;

/// Failure to parse source or to intern a lossless [`crate::NodeTree`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// The adapter could not parse the input.
    #[error("{0}")]
    Failed(String),
    /// `concat(children.raw) != node.raw` (spec §3.3).
    #[error(
        "concat(children.raw) != node.raw for kind {kind} (raw {raw_len} bytes, children {children_raw_len} bytes)"
    )]
    ConcatInvariant {
        /// Kind of the node that failed the check.
        kind: NodeKind,
        /// Length of the node's `raw` bytes.
        raw_len: usize,
        /// Length of the concatenation of children's `raw` bytes.
        children_raw_len: usize,
    },
    /// A child (or root) [`ObjectId`] is not interned in this tree.
    #[error("node {0} is not interned in this NodeTree")]
    MissingNode(ObjectId),
    /// Canonical encoding of a [`hord_core::Node`] failed while hashing.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
}

impl ParseError {
    /// Adapter-level parse failure with a human-readable message.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}
