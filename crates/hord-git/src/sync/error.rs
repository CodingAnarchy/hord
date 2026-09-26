//! Errors from the git bridge.

use std::path::PathBuf;

use hord_api::ApiError;
use thiserror::Error;

/// Failure of one bridge step (spec §9 Sync).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SyncError {
    /// Export, import, or a git object read failed.
    #[error(transparent)]
    Git(#[from] crate::Error),
    /// The repository backend refused or failed a call.
    #[error("repository: {0}")]
    Api(#[from] ApiError),
    /// A `git` command failed.
    #[error("git {command} failed: {detail}")]
    Command {
        /// The subcommand, without credentials.
        command: String,
        /// Its standard error, or why it could not run.
        detail: String,
    },
    /// A file in the bridge's work directory could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The bridge's state file does not parse.
    #[error("bridge state {path}: {reason}")]
    State {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The pull request host refused or failed a call.
    #[error("pull requests: {0}")]
    Pulls(String),
    /// A pull request cannot become a proposal (for example, it is not
    /// based on `main`). Reported on the pull request.
    #[error("{0}")]
    Unproposable(String),
}
