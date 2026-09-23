//! Fresh [`hord_core::NodeId`] assignment for a tree with no base.

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, RepoPath};
use hord_lang::{
    IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, Site, default_identify, oid_at,
};

/// Assign a fresh [`hord_core::NodeId`] to every definition in `tree`.
///
/// This is [`default_identify`] against an empty base: each definition is an
/// [`hord_core::IdentityDelta::Birth`]. Non-definitions are omitted. Rename
/// detection is not involved.
///
/// Birth ids are a pure function of the definition's content [`ObjectId`].
/// The same definition gets the same id on every call, and inserting an
/// unrelated definition does not change it.
#[must_use]
pub fn assign<A: LangAdapter + ?Sized>(adapter: &A, tree: &NodeTree) -> IdentityMapping {
    let base = IdentifiedTree::default();
    let mut mapping = default_identify(adapter, &base, tree);
    stabilize_births(&mut mapping, tree, 0);
    mapping
}

/// [`assign`] for the file at `path`. Birth ids also depend on the path, so
/// identical definitions in two files of one snapshot get different ids
/// ([`assign`] only keeps them apart within one file).
#[must_use]
pub fn assign_in<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    tree: &NodeTree,
) -> IdentityMapping {
    let base = IdentifiedTree::default();
    let mut mapping = default_identify(adapter, &base, tree);
    stabilize_births(&mut mapping, tree, path_salt(path));
    mapping
}

/// Salt that makes content-derived ids distinct per file. Zero keeps the
/// path-free derivation of [`assign`] and [`crate::carry`].
pub(crate) fn path_salt(path: &RepoPath) -> u128 {
    crate::file_root_id(path).as_u128()
}

/// Replace [`IdentityDelta::Birth`] ids from [`NodeId::generate`] with ids
/// derived from the definition content hash. Identical definitions born at
/// two sites share a content hash; the later one in preorder takes the next
/// salt, so each site keeps its own id.
pub(crate) fn stabilize_births(mapping: &mut IdentityMapping, tree: &NodeTree, scope: u128) {
    let births: BTreeSet<NodeId> = mapping
        .deltas
        .iter()
        .filter_map(|delta| match delta {
            IdentityDelta::Birth { node } => Some(*node),
            _ => None,
        })
        .collect();
    if births.is_empty() {
        return;
    }
    let mut pairs: Vec<(ObjectId, &Site, NodeId)> = mapping
        .nodes
        .iter()
        .filter(|(_, id)| births.contains(id))
        .filter_map(|(site, id)| Some((oid_at(tree, site)?, site, *id)))
        .collect();
    // Content order, not source order: a sibling inserted earlier does not
    // change which salt a definition receives. Sites break ties between
    // identical definitions.
    pairs.sort();
    let pairs: Vec<(ObjectId, NodeId)> = pairs.into_iter().map(|(oid, _, id)| (oid, id)).collect();

    let mut reserved: BTreeSet<NodeId> = mapping
        .nodes
        .values()
        .copied()
        .filter(|id| !births.contains(id))
        .collect();
    for delta in &mapping.deltas {
        if let IdentityDelta::Death { node } = delta {
            reserved.insert(*node);
        }
    }

    let mut replace: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    for (oid, old) in pairs {
        let mut assigned = stable_birth_id(oid, 0, scope);
        for salt in 1..64u32 {
            if assigned != NodeId::nil()
                && !reserved.contains(&assigned)
                && !replace.values().any(|id| *id == assigned)
            {
                break;
            }
            assigned = stable_birth_id(oid, salt, scope);
        }
        reserved.insert(assigned);
        replace.insert(old, assigned);
    }

    for id in mapping.nodes.values_mut() {
        if let Some(updated) = replace.get(id) {
            *id = *updated;
        }
    }
    for delta in &mut mapping.deltas {
        if let IdentityDelta::Birth { node } = delta
            && let Some(updated) = replace.get(node)
        {
            *node = *updated;
        }
    }
}

fn stable_birth_id(oid: ObjectId, salt: u32, scope: u128) -> NodeId {
    let bytes = oid.as_bytes();
    let hi = u128::from_be_bytes(bytes[..16].try_into().expect("16 bytes"));
    let lo = u128::from_be_bytes(bytes[16..].try_into().expect("16 bytes"));
    let mut mixed = hi
        ^ lo.rotate_left(17)
        ^ u128::from(salt).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ scope.rotate_left(41);
    mixed ^= 0xB100_0000_0000_0000;
    if mixed == 0 {
        mixed = 1;
    }
    NodeId::from_u128(mixed)
}
