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
