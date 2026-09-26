//! [`RemoteRepo`]: a `hord serve` client (spec §10.5.2, ADR 0024).
//!
//! It implements [`hord_api::RepoBackend`] over the generated tonic client,
//! so the CLI and the conformance suite use it exactly as they use
//! `hord_txn::LocalRepo`, and [`hord_txn::ObjectSource`], so a client-side
//! [`hord_txn::Repo`] opened with [`open_cache`] reads objects lazily: its
//! own store is the local object cache, and what it lacks is fetched from
//! the server in batches and kept (spec §8.3). [`push_change`] sends a
//! proposal's new objects before it is submitted.
//!
//! Addresses are `http://host:port` for a server hosting one repository,
//! `http://host:port/r/<name>` for one of several, or a repository's local
//! endpoint ([`RemoteRepo::connect_local`], ADR 0021). `https://` connects
//! over TLS (ADR 0032), trusting the system's roots plus an optional CA
//! certificate ([`ConnectOptions::ca_pem`]).
//!
//! [`RemoteRepo::connect_with_token`] sends a bearer token on every call,
//! and [`RemoteRepo::auth`] reaches the server's `Auth` service (spec
//! §10.5.4).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

/// One unary call on a generated client: `$self.client.$method($request)`,
/// unwrapped, with the status as an [`hord_api::ApiError`].
macro_rules! call {
    ($self:ident, $method:ident, $request:ident) => {
        $self
            .client
            .clone()
            .$method($request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(hord_api::ApiError::from)
    };
}

mod audit;
mod auth;
mod changes;
mod client;
mod push;
mod transport;
mod workspaces;

pub use audit::RemoteAudit;
pub use auth::RemoteAuth;
pub use changes::RemoteChanges;
pub use client::{ConnectOptions, RemoteRepo, open_cache};
pub use push::push_change;
pub use workspaces::RemoteWorkspaces;

/// Failure to connect to a server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The address is not `http[s]://host:port[/r/<name>]`.
    #[error(
        "invalid remote address {0:?}: expected http[s]://host:port or http[s]://host:port/r/<name>"
    )]
    InvalidUrl(String),
    /// TLS could not be set up: no trusted roots, or a CA certificate
    /// that is not PEM.
    #[error("TLS for {url}: {reason}")]
    Tls {
        /// Where.
        url: String,
        /// Why.
        reason: String,
    },
    /// A bearer token that cannot travel in a header.
    #[error("invalid token: not a valid header value")]
    InvalidToken,
    /// The connection failed.
    #[error("connect to {url}: {source}")]
    Connect {
        /// Where.
        url: String,
        /// Why.
        source: tonic::transport::Error,
    },
}
