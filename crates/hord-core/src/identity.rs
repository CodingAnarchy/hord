//! Node identity maps and deltas (spec §3.2, §3.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{NodeId, RepoPath};

/// Per-snapshot mapping `NodeId → path-in-tree`, plus births/deaths/derivations.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IdentityMap {
    /// Location of every identified definition in the snapshot tree.
    pub nodes: BTreeMap<NodeId, NodePath>,
    /// Births, deaths, and derivations relative to the parent snapshot.
    pub deltas: Vec<IdentityDelta>,
}

/// Location of a node inside a snapshot: file path plus a child-index walk
/// from the [`crate::NodeFile`] root.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct NodePath {
    /// Repository path of the file that contains the node.
    pub file: RepoPath,
    /// Child indices from the file root [`crate::Node`] down to this node.
    pub pointer: Vec<u32>,
}

/// Birth, death, or declared derivation of a [`NodeId`] (spec §3.4).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum IdentityDelta {
    /// A definition that did not exist in the base tree.
    Birth {
        /// Newly assigned identity.
        node: NodeId,
    },
    /// A definition that no longer exists in the result tree.
    Death {
        /// Retired identity.
        node: NodeId,
    },
    /// `node` was derived from `from` (rename, copy, or explicit declaration).
    DerivedFrom {
        /// Result identity.
        node: NodeId,
        /// Base identity it was derived from.
        from: NodeId,
    },
    /// `node` was split into `into`.
    SplitInto {
        /// Base identity that was split.
        node: NodeId,
        /// Result identities.
        into: Vec<NodeId>,
    },
    /// `node` was merged from `from`.
    MergedFrom {
        /// Result identity.
        node: NodeId,
        /// Base identities that were merged.
        from: Vec<NodeId>,
    },
}

impl IdentityDelta {
    /// Every [`NodeId`] the delta names: `node`, then `from` or `into`.
    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        let (node, rest): (&NodeId, &[NodeId]) = match self {
            Self::Birth { node } | Self::Death { node } => (node, &[]),
            Self::DerivedFrom { node, from } => (node, std::slice::from_ref(from)),
            Self::SplitInto { node, into } => (node, into),
            Self::MergedFrom { node, from } => (node, from),
        };
        std::iter::once(*node).chain(rest.iter().copied())
    }
}

/// One directory of a snapshot's identity (ADR 0017): name → subdirectory or
/// [`FileIdentity`].
///
/// Merkle-shaped like [`crate::Tree`], so unchanged subtrees share objects
/// between snapshots. A parsed file whose ids equal the fresh deterministic
/// assignment (ADR 0019) has no entry; readers fall back to that
/// assignment. Directories with no entries are omitted, except the root.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IdentityTree {
    /// Child entries, keyed by basename. Encoded as a CBOR map.
    pub entries: BTreeMap<String, IdentityEntry>,
}

/// One child of an [`IdentityTree`].
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum IdentityEntry {
    /// Nested [`IdentityTree`] object.
    Dir(crate::ObjectId),
    /// [`FileIdentity`] object of the file with this name.
    File(crate::ObjectId),
}

/// The [`NodeId`]s of one parsed file (ADR 0017).
///
/// Bound to the blob they were computed for: a reader whose tree has a
/// different blob at the path must not use them.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    /// [`crate::Blob`] the ids describe.
    pub blob: crate::ObjectId,
    /// Definition site (the child-index walk from the file's root node) →
    /// identity, sorted by site.
    pub nodes: Vec<(Vec<u32>, NodeId)>,
}
