//! Fresh [`hord_core::NodeId`] assignment for a tree with no base.

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, RepoPath, SnapshotId};
use hord_lang::{
    IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, Site, default_identify, oid_at,
};

/// Assign a fresh [`hord_core::NodeId`] to every definition in `tree`.
///
/// This is [`default_identify`] against an empty base: each definition is an
/// [`hord_core::IdentityDelta::Birth`]. Non-definitions are omitted. Rename
/// detection is not involved. It is [`assign_in`] at the repository root path
/// with no base snapshot.
#[must_use]
pub fn assign<A: LangAdapter + ?Sized>(adapter: &A, tree: &NodeTree) -> IdentityMapping {
    assign_in(adapter, &RepoPath::default(), None, tree)
}

/// Assign a fresh [`hord_core::NodeId`] to every definition of the file at
/// `path` (ADR 0019).
///
/// Each id is derived from the definition's content id, the file's root id
/// ([`crate::file_root_id`]), its site, and `snapshot`: the base snapshot of
/// the change that creates it. With `snapshot` `None` this is the *fresh
/// assignment* that readers fall back to for a file whose identity a
/// snapshot omits (ADR 0017). The same inputs give the same ids on every
/// call.
#[must_use]
pub fn assign_in<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    snapshot: Option<SnapshotId>,
    tree: &NodeTree,
) -> IdentityMapping {
    let base = IdentifiedTree::default();
    let mut mapping = default_identify(adapter, &base, tree);
    stabilize_births(&mut mapping, tree, &BirthScope { path, snapshot });
    mapping
}

/// Where births happen: the file and the base snapshot of the change
/// (ADR 0019).
pub(crate) struct BirthScope<'a> {
    pub path: &'a RepoPath,
    pub snapshot: Option<SnapshotId>,
}

/// Replace [`IdentityDelta::Birth`] ids from [`NodeId::generate`] with ids
/// derived per ADR 0019 from the definition's content id, file root, site,
/// and base snapshot. A derived id that is already in use in `mapping` (a
/// carried id, a death, or an earlier birth) takes the next occurrence salt.
pub(crate) fn stabilize_births(
    mapping: &mut IdentityMapping,
    tree: &NodeTree,
    scope: &BirthScope<'_>,
) {
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
    // Preorder (sites sort that way), so the result is independent of the
    // random ids `default_identify` handed out.
    let pairs: Vec<(ObjectId, Site, NodeId)> = mapping
        .nodes
        .iter()
        .filter(|(_, id)| births.contains(id))
        .filter_map(|(site, id)| Some((oid_at(tree, site)?, site.clone(), *id)))
        .collect();
    let root = crate::file_root_id(scope.path);

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
    for (oid, site, old) in pairs {
        let mut assigned = birth_id(oid, root, &site, scope.snapshot, 0);
        for occurrence in 1..64u32 {
            if assigned != NodeId::nil()
                && !reserved.contains(&assigned)
                && !replace.values().any(|id| *id == assigned)
            {
                break;
            }
            assigned = birth_id(oid, root, &site, scope.snapshot, occurrence);
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

/// ADR 0019: `derive("hord/birth", content id, file root id, site, base
/// snapshot)` plus an occurrence salt, as the high 128 bits of the BLAKE3
/// hash of their canonical CBOR (injective, so distinct inputs hash
/// distinct bytes). Never [`NodeId::nil`].
#[must_use]
pub fn birth_id(
    content: ObjectId,
    file_root: NodeId,
    site: &[u32],
    snapshot: Option<SnapshotId>,
    occurrence: u32,
) -> NodeId {
    let input = ("hord/birth", content, file_root, site, snapshot, occurrence);
    // Encoding a tuple of ids, integers, and a string cannot fail.
    let id = ObjectId::of(&input).unwrap_or_else(|_| ObjectId::from_canonical(b"hord/birth"));
    let mut high = [0u8; 16];
    high.copy_from_slice(&id.as_bytes()[..16]);
    let value = u128::from_be_bytes(high);
    NodeId::from_u128(if value == 0 { 1 } else { value })
}
