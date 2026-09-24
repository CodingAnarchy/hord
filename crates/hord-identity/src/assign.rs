//! Fresh [`hord_core::NodeId`] assignment for a tree with no base.

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, RepoPath, SnapshotId};
use hord_lang::{IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, Site, oid_at};

/// Assign a fresh [`hord_core::NodeId`] to every definition of the file at
/// `path` (ADR 0019).
///
/// This is [`hord_lang::default_identify`] against an empty base: each
/// definition is an [`hord_core::IdentityDelta::Birth`], in preorder.
/// Non-definitions are omitted. Rename detection is not involved. Each id
/// is derived from the definition's content id, the file's root id
/// ([`NodeId::file_root`]), its site, and `snapshot`: the base snapshot of
/// the change that creates it. With `snapshot` `None` this is the *fresh
/// assignment* that readers fall back to for a file whose identity a
/// snapshot omits (ADR 0017). The same inputs give the same ids on every
/// call.
#[must_use]
pub fn assign<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    snapshot: Option<SnapshotId>,
    tree: &NodeTree,
) -> IdentityMapping {
    let mut mapping = IdentityMapping::default();
    births(adapter, path, snapshot, tree, &mut |site, node| {
        mapping.nodes.insert(site.to_vec(), node);
        mapping.deltas.push(IdentityDelta::Birth { node });
        true
    });
    mapping
}

/// Whether `tree`'s ids are exactly the fresh assignment of the file at
/// `path`: `assign(adapter, path, None, &tree.tree).nodes == tree.ids`,
/// without building that mapping. Stops at the first difference.
#[must_use]
pub fn is_fresh<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    tree: &IdentifiedTree,
) -> bool {
    let mut ids = tree.ids.iter();
    births(adapter, path, None, &tree.tree, &mut |site, node| {
        ids.next()
            .is_some_and(|(at, id)| at.as_slice() == site && *id == node)
    }) && ids.next().is_none()
}

/// Hand each definition of `tree`, in preorder, its birth id to `visit`
/// with its site, until `visit` returns false. Returns whether it never
/// did.
fn births<A: LangAdapter + ?Sized>(
    adapter: &A,
    path: &RepoPath,
    snapshot: Option<SnapshotId>,
    tree: &NodeTree,
    visit: &mut impl FnMut(&[u32], NodeId) -> bool,
) -> bool {
    /// Where the walk is: the file's root id, the snapshot, and the ids
    /// handed out so far.
    struct Births {
        root: NodeId,
        snapshot: Option<SnapshotId>,
        taken: BTreeSet<NodeId>,
    }
    fn walk<A: LangAdapter + ?Sized>(
        adapter: &A,
        tree: &NodeTree,
        oid: ObjectId,
        site: &mut Site,
        births: &mut Births,
        visit: &mut impl FnMut(&[u32], NodeId) -> bool,
    ) -> bool {
        let Some(node) = tree.get(oid) else {
            return true;
        };
        if adapter.is_definition(&node.kind) {
            let id = fresh_birth(oid, births.root, site, births.snapshot, &births.taken);
            births.taken.insert(id);
            if !visit(site, id) {
                return false;
            }
        }
        for (i, child) in node.children.iter().enumerate() {
            site.push(u32::try_from(i).unwrap_or(u32::MAX));
            let go_on = walk(adapter, tree, *child, site, births, visit);
            site.pop();
            if !go_on {
                return false;
            }
        }
        true
    }
    let Some(root) = tree.root() else {
        return true;
    };
    walk(
        adapter,
        tree,
        root,
        &mut Vec::new(),
        &mut Births {
            root: NodeId::file_root(path),
            snapshot,
            taken: BTreeSet::new(),
        },
        visit,
    )
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
    let root = NodeId::file_root(scope.path);

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
        let assigned = fresh_birth(oid, root, &site, scope.snapshot, &reserved);
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

/// The birth id of the definition with content `oid` at `site`: the first
/// occurrence salt whose [`birth_id`] is not nil and not in `taken` (after
/// 64 tries, the last one).
fn fresh_birth(
    oid: ObjectId,
    root: NodeId,
    site: &[u32],
    snapshot: Option<SnapshotId>,
    taken: &BTreeSet<NodeId>,
) -> NodeId {
    let mut assigned = birth_id(oid, root, site, snapshot, 0);
    for occurrence in 1..64u32 {
        if assigned != NodeId::nil() && !taken.contains(&assigned) {
            break;
        }
        assigned = birth_id(oid, root, site, snapshot, occurrence);
    }
    assigned
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
