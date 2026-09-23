//! Transactions and landing (spec §6): workspaces, access logs, `propose`,
//! the serializability check, structural rebase, and a single-process lander.
//!
//! ```text
//! Repo::begin(base)            O(1) in-memory workspace (or begin_directory)
//!   Workspace::read_* / write_*   access log at definition granularity
//!   Workspace::propose(intent)    diff + identity → stored ChangeRecord
//! Repo::submit(change)         persistent lander queue
//! Repo::land_local()           check → rebase → Verifier → log, head, index
//! Repo::conflicts(change)      machine-readable ConflictReport
//! Repo::query()                blame, log filters, and edges (Query)
//! ```
//!
//! A snapshot is a [`hord_core::Snapshot`] object: a content tree plus the
//! identity tree that carries its NodeIds (ADR 0017). Birth ids are derived
//! from content, file, site, and base snapshot (ADR 0019); a change that
//! lands rebased records what it did on the head it landed on and names the
//! submitted record (ADR 0018); file moves carry identity (ADR 0020).
//!
//! Read sets follow ADR 0012 (access log ∪ one hop of outgoing references
//! from written definitions, base and result ∪ declarations). Blob-tier
//! files and whole-file writes use a path-derived [`NodeId`](hord_core::NodeId)
//! ([`path_node_id`]); see [`propose`](Workspace::propose) for the op layout
//! and sets.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod conflict;
mod error;
mod files;
mod ids;
mod lander;
mod materialize;
mod propose;
mod query;
mod rebase;
mod repo;
mod semantic;
mod sets;
mod snapshot;
mod workspace;

pub use conflict::{
    AdapterMerge, ConflictKind, ConflictReport, MergeConflict, MergeSeverity, SetConflict,
};
pub use error::{Error, Result};
pub use hord_store::{EdgeKind, WorkspaceId};
#[allow(deprecated)]
pub use ids::glue_node_id;
pub use ids::path_node_id;
pub use lander::{
    QueueEntry, QueueStatus, StubVerifier, Verdict, Verifier, VerifyFuture, VerifyRequest,
};
pub use materialize::MaterializeMode;
pub use propose::ReadDeclaration;
pub use query::{Query, line_start, touches_node};
pub use repo::{
    Base, BeginOptions, FailClosedVerifier, Head, Repo, RepoConfig, RepoOptions, default_adapters,
};
pub use semantic::DefinitionInfo;
pub use workspace::{AccessLog, Materialization, Proposal, Workspace};
