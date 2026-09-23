//! Fresh [`hord_core::NodeId`] assignment for a tree with no base.

use hord_lang::{IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, default_identify};

/// Assign a fresh [`hord_core::NodeId`] to every definition in `tree`.
///
/// This is [`default_identify`] against an empty base: each definition is an
/// [`hord_core::IdentityDelta::Birth`]. Non-definitions are omitted. Rename
/// detection is not involved.
#[must_use]
pub fn assign<A: LangAdapter + ?Sized>(adapter: &A, tree: &NodeTree) -> IdentityMapping {
    let base = IdentifiedTree::default();
    default_identify(adapter, &base, tree)
}
