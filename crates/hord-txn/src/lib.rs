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
//! ```
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
mod rebase;
mod repo;
mod semantic;
mod snapshot;
mod workspace;

pub use conflict::{
    AdapterMerge, ConflictKind, ConflictReport, MergeConflict, MergeSeverity, SetConflict,
};
pub use error::{Error, Result};
pub use hord_store::WorkspaceId;
#[allow(deprecated)]
pub use ids::glue_node_id;
pub use ids::path_node_id;
pub use lander::{
    QueueEntry, QueueStatus, StubVerifier, Verdict, Verifier, VerifyFuture, VerifyRequest,
};
pub use materialize::MaterializeMode;
pub use propose::ReadDeclaration;
pub use repo::{
    Base, BeginOptions, FailClosedVerifier, Head, Repo, RepoConfig, RepoOptions, default_adapters,
};
pub use semantic::DefinitionInfo;
pub use workspace::{AccessLog, Materialization, Proposal, Workspace};
