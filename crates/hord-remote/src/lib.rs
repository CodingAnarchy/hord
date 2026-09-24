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
//! endpoint ([`RemoteRepo::connect_local`], ADR 0021).

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

mod client;
mod push;
mod transport;
mod workspaces;

pub use client::{RemoteRepo, open_cache};
pub use push::push_change;
pub use workspaces::RemoteWorkspaces;

/// Failure to connect to a server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The address is not `http://host:port[/r/<name>]`.
    #[error("invalid remote address {0:?}: expected http://host:port or http://host:port/r/<name>")]
    InvalidUrl(String),
    /// The connection failed.
    #[error("connect to {url}: {source}")]
    Connect {
        /// Where.
        url: String,
        /// Why.
        source: tonic::transport::Error,
    },
}
