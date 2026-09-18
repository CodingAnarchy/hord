//! Snapshots: Merkle trees plus metadata (spec §3.2, §3.7).

use serde::{Deserialize, Serialize};

use crate::ObjectId;

/// Immutable Merkle tree of the whole repository.
///
/// [`crate::SnapshotId`] is the [`ObjectId`] of [`Self::tree`], not of this
/// wrapper (spec §3.1).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Snapshot {
    /// Object id of the root [`crate::Tree`].
    pub tree: ObjectId,
    /// Repository-level metadata attached to this snapshot.
    pub metadata: SnapshotMetadata,
    /// Pointers to rebuildable index objects (not part of the file tree).
    pub index: IndexPointers,
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

/// Pointers to per-snapshot index objects (spec §8.1).
///
/// The index is fully rebuildable from objects; these ids are optional until
/// the corresponding tables have been written.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IndexPointers {
    /// [`crate::IdentityMap`] object for this snapshot, if computed.
    pub identity: Option<ObjectId>,
    /// Per-snapshot edge index object, if computed.
    pub edges: Option<ObjectId>,
}
