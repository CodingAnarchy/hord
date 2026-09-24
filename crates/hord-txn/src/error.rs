//! Errors from workspaces, propose, and the lander.

use hord_core::{ChangeId, NodeId, ObjectId, RepoPath, SnapshotId};
use thiserror::Error;

/// Failure of a `hord-txn` operation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The object store or its index failed.
    #[error(transparent)]
    Store(#[from] hord_store::Error),
    /// Canonical CBOR encoding or decoding failed.
    #[error(transparent)]
    Encoding(#[from] hord_encoding::Error),
    /// Filesystem I/O on a `Directory` workspace failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Identity carrying failed (spec §3.4).
    #[error("identity for {path}: {source}")]
    Identity {
        /// File whose definitions could not be identified.
        path: RepoPath,
        /// Underlying failure.
        source: hord_identity::Error,
    },
    /// The file has no definition with this [`NodeId`] in the workspace view.
    #[error("no definition {node} in {path}")]
    UnknownDefinition {
        /// File that was searched.
        path: RepoPath,
        /// Definition that was not found.
        node: NodeId,
    },
    /// The path is not a file in the workspace view.
    #[error("no file {0}")]
    MissingFile(RepoPath),
    /// The file has no language adapter, or does not parse, so it has no
    /// definitions.
    #[error("{0} has no definitions (no adapter, or it does not parse)")]
    NotParsed(RepoPath),
    /// A declared read names nothing in the base or result snapshot
    /// (ADR 0012: unresolvable names are an error at propose).
    #[error("declared read {0:?} does not name a definition")]
    UnresolvedDeclaration(String),
    /// The derived ops do not reproduce `result` from `base` (spec §3.5).
    #[error("ops do not reproduce {path}: {reason}")]
    OpsDoNotReproduce {
        /// File whose ops failed the check.
        path: RepoPath,
        /// What went wrong.
        reason: String,
    },
    /// A change record's ops do not reproduce its result tree (spec §3.5).
    #[error("change {change} does not reproduce its result snapshot {result}: {reason}")]
    InvalidChange {
        /// Change that failed validation.
        change: ChangeId,
        /// Result snapshot the change claims.
        result: SnapshotId,
        /// What went wrong.
        reason: String,
    },
    /// No stored object decodes as a [`hord_core::ChangeRecord`] with this id.
    #[error("no change record {0}")]
    MissingChange(ChangeId),
    /// The change was never submitted to the lander.
    #[error("change {0} is not in the lander queue")]
    NotQueued(ChangeId),
    /// A tree entry is a kind this crate does not read (for example a
    /// [`hord_core::NodeFile`]).
    #[error("unsupported tree entry at {path}: {kind}")]
    UnsupportedEntry {
        /// Path of the entry.
        path: RepoPath,
        /// What kind of entry it was.
        kind: &'static str,
    },
    /// A file path cannot be represented as a [`RepoPath`].
    #[error("invalid path {0:?}")]
    InvalidPath(String),
    /// The workspace has no changes against its base.
    #[error("nothing to propose: the workspace matches its base")]
    NothingToPropose,
    /// No workspace with this id is recorded in the store.
    #[error("unknown workspace {0}")]
    UnknownWorkspace(hord_store::WorkspaceId),
    /// The repository already has landed changes.
    #[error("repository is not empty (head is {0})")]
    NotEmpty(ChangeId),
    /// A stored object had an unexpected shape.
    #[error("corrupt object {id}: {reason}")]
    Corrupt {
        /// Object that failed to decode as expected.
        id: ObjectId,
        /// What was wrong.
        reason: String,
    },
    /// The snapshot names no identity tree, or its identity tree is not
    /// stored (ADR 0017). Its NodeIds are unknown; they are never guessed by
    /// a fresh assignment.
    #[error("no identity recorded for snapshot {0}")]
    MissingIdentity(SnapshotId),
    /// No definition in the history has this qualified name.
    #[error("cannot resolve node {0:?}")]
    UnknownName(String),
    /// Several definitions of one snapshot have this name.
    #[error("qualified name {name:?} resolves to multiple nodes: {}", list(.nodes))]
    AmbiguousName {
        /// The name that was looked up.
        name: String,
        /// Every matching definition, in id order.
        nodes: Vec<NodeId>,
    },
    /// The file has fewer lines.
    #[error("line {line} is outside {path}")]
    LineOutOfRange {
        /// File that was read.
        path: RepoPath,
        /// Requested 1-based line.
        line: u32,
    },
    /// No definition's span covers the line.
    #[error("no definition covers {path}:{line}")]
    NoDefinitionAt {
        /// File that was read.
        path: RepoPath,
        /// Requested 1-based line.
        line: u32,
    },
    /// A blocking task panicked or was cancelled.
    #[error("background task failed: {0}")]
    Task(String),
    /// The event log (`.hord/events.redb`, spec §10.5.3) failed.
    #[error("event log: {0}")]
    EventLog(String),
}

fn list(nodes: &[NodeId]) -> String {
    nodes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
