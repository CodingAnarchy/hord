//! Snapshots: a content root plus the identity of its definitions (spec §3.2,
//! §3.7, ADR 0017).

use serde::{Deserialize, Serialize};

use crate::{IdentityTree, ObjectId, Tree};

/// Immutable state of the whole repository (ADR 0017).
///
/// [`crate::SnapshotId`] is the [`ObjectId`] of this object. It names the
/// content root ([`Self::tree`]) and the Merkle identity tree
/// ([`IndexPointers::identity`]) that carries the [`crate::NodeId`]s of its
/// parsed files, so a snapshot's objects are self-describing: whoever
/// fetches them learns its NodeIds. Two snapshots have the same content when
/// their `tree`s are equal; they are the same snapshot only when their
/// identity is equal too.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Snapshot {
    /// Object id of the root [`crate::Tree`]: a pure function of file
    /// content. The git bridge maps it to a git tree.
    pub tree: ObjectId,
    /// Repository-level metadata attached to this snapshot.
    pub metadata: SnapshotMetadata,
    /// The identity tree, and pointers to rebuildable index objects.
    pub index: IndexPointers,
}

impl Snapshot {
    /// A snapshot of `tree` whose identity is the [`IdentityTree`] `identity`.
    #[must_use]
    pub fn new(tree: ObjectId, identity: ObjectId) -> Self {
        Self {
            tree,
            metadata: SnapshotMetadata::default(),
            index: IndexPointers {
                identity: Some(identity),
                edges: None,
            },
        }
    }

    /// The empty repository: an empty [`Tree`] with an empty
    /// [`IdentityTree`]. The objects it names are `Tree::default()` and
    /// `IdentityTree::default()`; a store that uses it must hold them.
    #[must_use]
    pub fn empty() -> Self {
        // Both defaults encode as a one-key map; encoding cannot fail.
        let tree = ObjectId::of(&Tree::default()).expect("empty tree encodes");
        let identity = ObjectId::of(&IdentityTree::default()).expect("empty identity encodes");
        Self::new(tree, identity)
    }

    /// The content root ([`Self::tree`]).
    #[must_use]
    pub fn root(&self) -> ObjectId {
        self.tree
    }

    /// The identity tree, if this snapshot names one. A snapshot the log
    /// names always does (ADR 0017: a missing identity is an error, never a
    /// fresh assignment).
    #[must_use]
    pub fn identity(&self) -> Option<ObjectId> {
        self.index.identity
    }
}

/// Metadata stored on a [`Snapshot`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// [`ObjectId`] of the toolchain this snapshot was parsed with, if known.
    ///
    /// Grammar versions are pinned per snapshot in that toolchain object
    /// (spec §15).
    pub toolchain: Option<ObjectId>,
}

/// The identity tree of a [`Snapshot`], plus pointers to per-snapshot index
/// objects (spec §8.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IndexPointers {
    /// Root [`IdentityTree`] (ADR 0017). Not an index: NodeIds depend on
    /// history, so this is the only record of them. `None` only on a value
    /// that is not a repository snapshot.
    pub identity: Option<ObjectId>,
    /// Per-snapshot edge index object, if computed. Edges are derived and
    /// rebuildable (spec §3.8).
    pub edges: Option<ObjectId>,
}
