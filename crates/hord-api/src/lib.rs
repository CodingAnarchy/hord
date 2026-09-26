//! The Hord API (spec §10.5, ADR 0024).
//!
//! `proto/hord.proto` is the one schema for the remote protocol, the web UI,
//! and `hord --json`. This crate owns:
//!
//! - [`proto`]: the prost types generated from it, tonic client and server
//!   stubs ([`proto::repo_backend_client`], [`proto::repo_backend_server`]),
//!   and pbjson `serde` impls, which are the canonical protobuf JSON mapping;
//! - [`ChangesBackend`]: the read-only `Changes` service (ADR 0030) the
//!   web UI reads through;
//! - [`AuditBackend`]: the `Audit` service, M6's acceptance auditor;
//! - [`WorkspacesBackend`]: the local-only `Workspaces` service (ADR 0024
//!   amendment) a per-repo daemon serves;
//! - [`RepoBackend`]: spec §10.5.2's trait, method for method, expressed in
//!   the generated request and response messages, implemented by
//!   `hord_txn::LocalRepo` and (later) `hord-remote`'s `RemoteRepo`;
//! - [`auth`]: scopes, and the one each RPC needs (spec §10.5.4);
//! - [`schema`]: the descriptor set and a JSON Schema generated from it;
//! - [`wire`]: conversions between wire strings and [`hord_core`] ids and
//!   values;
//! - `conformance` (feature `conformance`): one test suite, written against
//!   `&dyn RepoBackend`, that every implementation runs.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod audit;
pub mod auth;
mod backend;
mod changes;
#[cfg(feature = "conformance")]
pub mod conformance;
mod error;
pub mod local;
pub mod recording;
pub mod schema;
pub mod wire;
mod workspaces;

pub use audit::AuditBackend;
pub use backend::{
    DEFAULT_LOG_LIMIT, EventCursor, EventStream, MAX_BATCH_BYTES, MAX_BATCH_IDS, MAX_MESSAGE_BYTES,
    RepoBackend, SubmissionId,
};
pub use changes::ChangesBackend;
pub use error::{ApiError, ApiResult};
pub use workspaces::WorkspacesBackend;

/// Types generated from `proto/hord.proto` (package `hord.v1`), with their
/// canonical JSON mapping (pbjson) and gRPC stubs (tonic).
#[allow(
    missing_docs,
    rustdoc::broken_intra_doc_links,
    rustdoc::bare_urls,
    rustdoc::invalid_html_tags,
    clippy::all,
    clippy::pedantic
)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hord.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/hord.v1.serde.rs"));
}
