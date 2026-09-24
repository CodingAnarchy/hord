//! Errors from starting and running the server.

use std::path::PathBuf;

use thiserror::Error;

/// Failure to configure, open, or run the server.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A hosted repository failed to open.
    #[error("repository {path}: {source}")]
    Repo {
        /// Its root.
        path: PathBuf,
        /// Why.
        source: hord_txn::Error,
    },
    /// `--root` found no repository.
    #[error("no repository (.hord/) under {0}")]
    NoRepos(PathBuf),
    /// A non-loopback bind address without `--insecure-bind` (ADR 0024:
    /// there is no authentication until M5).
    #[error(
        "refusing to bind {0}: not a loopback address, and there is no authentication \
         until M5; pass --insecure-bind to bind it anyway"
    )]
    InsecureBind(std::net::SocketAddr),
    /// `server.toml` is invalid.
    #[error("server config {path}: {reason}")]
    Config {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// Socket or file I/O.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The gRPC transport failed.
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
