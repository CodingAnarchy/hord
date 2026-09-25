//! Transactions and landing (spec §6): workspaces, access logs, `propose`,
//! the serializability check, structural rebase, and a single-process lander.
//!
//! ```text
//! Repo::begin(base)            O(1) in-memory workspace (or begin_directory)
//!   Workspace::read_* / write_*   access log at definition granularity
//!   Workspace::propose(intent)    diff + identity → stored ChangeRecord
//! Repo::submit(change)         persistent lander queue
//! Repo::land_local()           check → rebase → Verifier → log, head, index
//! Lander::spawn(repo, cancel)  the same lander as a long-running task
//! Repo::events(from)           the event stream (spec §10.5.3), resumable
//! Repo::conflicts(change)      machine-readable ConflictReport
//! Repo::query()                blame, log filters, and edges (Query)
//! LocalRepo                    hord_api::RepoBackend over all of the above
//! ```
//!
//! ADR 0024 layering: a [`Repo`] reads objects through an [`ObjectSource`]
//! (its store, then an optional source that fetches on demand, for a
//! remote workspace), the [`Lander`] is the only writer of the log and head
//! and records every event it emits in `.hord/events.redb` with a
//! persisted cursor, and [`LocalRepo`] serves it all as
//! [`hord_api::RepoBackend`].
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
//! ([`NodeId::file_root`](hord_core::NodeId::file_root)); see [`propose`](Workspace::propose) for the op layout
//! and sets.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod conflict;
mod error;
mod events;
mod files;
mod gate;
mod graph;
mod lander;
mod local;
mod materialize;
mod propose;
mod query;
mod rebase;
mod repo;
mod semantic;
mod sets;
mod snapshot;
mod source;
mod workspace;

pub use conflict::{
    AdapterMerge, ConflictKind, ConflictReport, MergeConflict, MergeSeverity, SetConflict,
};
pub use error::{Error, Result};
pub use gate::{
    Built, EngineVerifier, FailClosedVerifier, InstrumentedTests, PolicySource, RustFactory,
    StubVerifier, Verdict, Verifier, VerifierFactory, VerifyContext, VerifyFuture, VerifyRequest,
    WorkspaceVerification, applicable_requirements, unverified_overlap,
};
pub use hord_store::{EdgeKind, WorkspaceId};
pub use lander::{Lander, QueueEntry, QueueStatus, SPECULATIVE_WINDOW};
pub use local::{LocalRepo, conflict_report_message, queue_entry_message};
pub use materialize::MaterializeMode;
pub use propose::ReadDeclaration;
pub use query::{Query, line_start, touches_node};
pub use repo::{Base, BeginOptions, Head, Repo, RepoConfig, RepoOptions, default_adapters};
pub use semantic::DefinitionInfo;
pub use source::ObjectSource;
pub use workspace::{AccessLog, Materialization, Proposal, Workspace};
