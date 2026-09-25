//! Verification errors.

use std::io;

use hord_core::SnapshotId;
use thiserror::Error;

/// Failure to plan, run, or record verification.
///
/// A check that runs and fails is not an error: it is
/// [`hord_core::EvidenceResult::Fail`] evidence. These are failures to
/// produce evidence at all.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] hord_store::Error),
    /// Canonical CBOR encoding or decoding failed.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
    /// Filesystem or process I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// An object the index names is missing.
    #[error("object {0} not found")]
    MissingObject(hord_core::ObjectId),
    /// [`crate::Verifier::run`] was given a checkout of another snapshot.
    #[error("checkout is snapshot {checkout}, but the plan is for {planned}")]
    WrongCheckout {
        /// Snapshot the plan was made for.
        planned: SnapshotId,
        /// Snapshot the checkout holds.
        checkout: SnapshotId,
    },
    /// The reference graph could not answer.
    #[error("reference graph: {0}")]
    Graph(String),
    /// An external tool (cargo, llvm-profdata, …) could not be run or
    /// printed something unreadable.
    #[error("{tool}: {message}")]
    Tool {
        /// The tool.
        tool: String,
        /// What went wrong.
        message: String,
    },
}

impl Error {
    /// A [`Self::Tool`] error.
    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }
}

/// Result of a verification operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;
