//! Change records, operations, intent, and provenance (spec §3.5).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    Actor, Bytes, ChangeId, IdentityDelta, NodeId, ObjectId, QualifiedName, RepoPath, SnapshotId,
    Timestamp,
};

/// A landed (or proposed) change: base → result with intent, ops, and sets.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// Snapshot the change was computed against.
    pub base: SnapshotId,
    /// Snapshot produced by applying the change.
    pub result: SnapshotId,
    /// Changes this one was landed after. Usually one; merge commits have many.
    pub parents: Vec<ChangeId>,
    /// Semantic operations, base → result.
    pub ops: Vec<Op>,
    /// Human- and machine-readable description of what this change is for.
    pub intent: Intent,
    /// Who produced the change, with which toolchain, and when.
    pub provenance: Provenance,
    /// [`NodeId`]s whose content the author depended on.
    pub read_set: BTreeSet<NodeId>,
    /// [`NodeId`]s whose content the author wrote.
    pub write_set: BTreeSet<NodeId>,
    /// Births, deaths, and derivations introduced by this change.
    pub identity_deltas: Vec<IdentityDelta>,
    /// [`crate::Evidence`] object ids attached to this change.
    pub evidence: Vec<ObjectId>,
    /// Signature over the change (spec §10.5.4). An author signs the
    /// record they submit; a record the lander rebased carries none
    /// (ADR 0018).
    pub signature: Option<Signature>,
    /// The submitted record this one was rebased from (ADR 0018).
    ///
    /// `Some` when the lander landed the change on a head other than its
    /// base: `base`, `result`, `parents`, `ops`, `write_set`, and
    /// `identity_deltas` were recomputed for that head, while `read_set`,
    /// `intent`, `provenance`, and `evidence` are the submitted record's.
    /// The author's signature stays valid on the submitted record, which
    /// must be stored. Omitted from the encoding when `None`, so records
    /// that were not rebased keep their ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebased_from: Option<ChangeId>,
}

/// One semantic operation derived by the diff engine (spec §3.5).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Op {
    /// Insert `node` as child `index` of `parent`.
    Insert {
        /// Parent definition that gains a child.
        parent: NodeId,
        /// Index among the parent's children.
        index: u32,
        /// Inserted [`crate::Node`] object.
        node: ObjectId,
    },
    /// Delete the identified node.
    Delete {
        /// Node to remove.
        node: NodeId,
    },
    /// Replace the content of `node` (`from` → `to`) without changing identity.
    Replace {
        /// Stable identity whose content changed.
        node: NodeId,
        /// Previous [`crate::Node`] object.
        from: ObjectId,
        /// New [`crate::Node`] object.
        to: ObjectId,
    },
    /// Move `node` to a new parent and index.
    Move {
        /// Identity being moved.
        node: NodeId,
        /// Parent before the move.
        from_parent: NodeId,
        /// Parent after the move.
        to_parent: NodeId,
        /// Index among the new parent's children.
        index: u32,
    },
    /// Rename `node` without changing its identity.
    Rename {
        /// Identity being renamed.
        node: NodeId,
        /// Previous qualified name.
        from: QualifiedName,
        /// New qualified name.
        to: QualifiedName,
    },
    /// Blob-level create, delete, or replace at `path`.
    Blob {
        /// Repository path of the blob.
        path: RepoPath,
        /// Previous blob object, if any.
        from: Option<ObjectId>,
        /// New blob object, if any.
        to: Option<ObjectId>,
    },
    /// File or directory create, delete, or rename at `path`.
    Tree {
        /// Repository path of the tree entry.
        path: RepoPath,
        /// Kind of tree mutation.
        kind: TreeOpKind,
    },
}

impl Op {
    /// Every [`NodeId`] field of the op: an `Insert`'s parent, the node of a
    /// `Delete`, `Replace`, or `Rename`, and a `Move`'s node and both
    /// parents. `Blob` and `Tree` name none.
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> {
        let ids = match self {
            Self::Insert { parent, .. } => [Some(*parent), None, None],
            Self::Delete { node } | Self::Replace { node, .. } | Self::Rename { node, .. } => {
                [Some(*node), None, None]
            }
            Self::Move {
                node,
                from_parent,
                to_parent,
                ..
            } => [Some(*node), Some(*from_parent), Some(*to_parent)],
            Self::Blob { .. } | Self::Tree { .. } => [None; 3],
        };
        ids.into_iter().flatten()
    }
}

/// File/directory mutation used by [`Op::Tree`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum TreeOpKind {
    /// Create a file at the path.
    CreateFile,
    /// Create a directory at the path.
    CreateDir,
    /// Delete the file or directory at the path.
    Delete,
    /// Rename the path to `to`.
    Rename {
        /// Destination path.
        to: RepoPath,
    },
}

/// What the change is for (spec §3.5).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Intent {
    /// One-line human-readable summary.
    pub summary: String,
    /// Full task description, prompt, or spec excerpt.
    pub body: String,
    /// Issue ids, spec URLs, parent change ids, imported git commits.
    pub refs: Vec<IntentRef>,
    /// Machine-checkable acceptance criteria.
    pub acceptance: Vec<Acceptance>,
}

/// A reference attached to an [`Intent`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum IntentRef {
    /// Issue or ticket identifier.
    Issue {
        /// Opaque issue id.
        id: String,
    },
    /// URL (spec section, design doc, …).
    Url {
        /// Absolute URL.
        url: String,
    },
    /// Another change this intent refers to.
    Change {
        /// Referenced change.
        change: ChangeId,
    },
    /// Git commit SHA recorded on import (spec §9).
    GitCommit {
        /// Hex-encoded git object name.
        sha: String,
    },
}

/// Machine-checkable acceptance criterion on an [`Intent`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Acceptance {
    /// A named test that must pass.
    Test {
        /// Test name or filter.
        name: String,
    },
    /// A check command that must succeed.
    Check {
        /// Command string (e.g. `cargo clippy --all-targets`).
        command: String,
    },
    /// An invariant described in prose, checked by a verifier.
    Invariant {
        /// Human-readable invariant.
        description: String,
    },
}

/// Who produced the change and under what tooling (spec §3.5).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Provenance {
    /// Human or agent author.
    pub actor: Actor,
    /// Hash of a toolchain object (compiler, cargo, tree-sitter versions, …).
    pub toolchain: ObjectId,
    /// When the change was created.
    pub created_at: Timestamp,
    /// Opaque harness session id, if any.
    pub session: Option<String>,
    /// If this change was a replay of another, the original change.
    pub parent_intent: Option<ChangeId>,
}

/// Detached signature over a [`ChangeRecord`] (spec §10.5.4).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Signature {
    /// Key identifier recorded with the signature.
    pub key_id: String,
    /// Raw signature bytes (Ed25519 on the hosting server).
    pub bytes: Bytes,
}
