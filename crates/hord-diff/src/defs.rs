//! Definition sites and file-root glue (definition-granularity matching).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, Node, NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, IdentityMapping, NodeTree, Site, def_sites, oid_at};

#[derive(Clone, Debug)]
pub(crate) struct DefSite {
    pub node_id: NodeId,
    pub object_id: ObjectId,
    pub site: Site,
    pub parent_id: NodeId,
    pub cst_index: u32,
}

/// Preorder definition sites in `tree` using `ids` as the def set.
/// File-level definitions have the parent `root`, the file root's id.
pub(crate) fn collect_sites(
    tree: &NodeTree,
    ids: &BTreeMap<Site, NodeId>,
    root: NodeId,
) -> Vec<DefSite> {
    def_sites(tree, ids)
        .map(|def| DefSite {
            node_id: def.node,
            object_id: def.oid,
            site: def.site.clone(),
            parent_id: def.parent.unwrap_or(root),
            cst_index: def.site.last().copied().unwrap_or(0),
        })
        .collect()
}

pub(crate) fn sites_by_id(sites: &[DefSite]) -> BTreeMap<NodeId, DefSite> {
    let mut map = BTreeMap::new();
    for s in sites {
        map.insert(s.node_id, s.clone());
    }
    map
}

/// Glue leaves of the file root: tokens not inside any definition.
///
/// Equal glue means definition-granularity ops suffice. Unequal glue
/// with no def ops collapses to a file-root Replace.
pub(crate) fn root_glue(tree: &NodeTree, ids: &BTreeMap<Site, NodeId>) -> Vec<ObjectId> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_outside_defs(
        tree,
        root,
        &mut Vec::new(),
        true,
        ids,
        &mut |at_start, oid, _node| {
            if at_start && ids.contains_key([].as_slice()) {
                return;
            }
            out.push(oid);
        },
    );
    out
}

/// Non-definition leaves of the definition at `site`, using `normalized` so
/// trivia-only edits do not look like a container change.
///
/// Child definitions are skipped entirely (not recorded as holes). Adding,
/// deleting, or editing a method therefore does not [`Op::Replace`] the
/// enclosing `impl`/`mod`/`table`; those edits are child ops so two sides
/// that touch different children can compose (spec §5.2 rule 1).
pub(crate) fn local_glue(
    tree: &NodeTree,
    site: &[u32],
    ids: &BTreeMap<Site, NodeId>,
) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let Some(start) = oid_at(tree, site) else {
        return out;
    };
    walk_outside_defs(
        tree,
        start,
        &mut site.to_vec(),
        true,
        ids,
        &mut |_at_start, _oid, node| {
            // List separators sit between child definitions (`struct { a, b }`,
            // `enum { A, B }`). They are not container header glue; skipping
            // them lets adding a field/variant be a child Insert instead of
            // Replace on the parent.
            if matches!(node.kind.as_str(), "," | ";") {
                return;
            }
            out.push(node.normalized);
        },
    );
    out
}

/// Preorder leaves that are not inside a nested definition.
///
/// `at_start` is true only for the starting node, so it is visited even
/// when it is a definition. Nested definitions (sites in `ids`) are skipped
/// entirely.
fn walk_outside_defs(
    tree: &NodeTree,
    oid: ObjectId,
    site: &mut Site,
    at_start: bool,
    ids: &BTreeMap<Site, NodeId>,
    on_leaf: &mut impl FnMut(bool, ObjectId, &Node),
) {
    if !at_start && ids.contains_key(site.as_slice()) {
        return;
    }
    let Some(node) = tree.get(oid) else {
        return;
    };
    if node.children.is_empty() {
        on_leaf(at_start, oid, node);
        return;
    }
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        walk_outside_defs(tree, *child, site, false, ids, on_leaf);
        site.pop();
    }
}

/// Site of `id` in `tree`; the empty site when `id` is the file `root`.
pub(crate) fn site_of(tree: &IdentifiedTree, id: NodeId, root: NodeId) -> Option<Site> {
    if id == root {
        return tree.tree.root().map(|_| Vec::new());
    }
    tree.site_of(id).cloned()
}

/// Content id of the definition `id` (of the whole file when `id` is the
/// file `root`).
pub(crate) fn oid_of(tree: &IdentifiedTree, id: NodeId, root: NodeId) -> Option<ObjectId> {
    tree.oid_at(&site_of(tree, id, root)?)
}

/// `(parent content id, child index)` for each step from the root to `site`.
pub(crate) fn chain(tree: &NodeTree, site: &[u32]) -> Option<Vec<(ObjectId, usize)>> {
    let mut out = Vec::with_capacity(site.len());
    let mut oid = tree.root()?;
    for index in site {
        let index = *index as usize;
        let child = *tree.get(oid)?.children.get(index)?;
        out.push((oid, index));
        oid = child;
    }
    Some(out)
}

/// Build an [`IdentityMapping`] from ids already on `side`, relative to `base`
/// (a file whose root id is `root`).
///
/// [`LangAdapter::identify`] is not used: the caller owns carrying (tests
/// pair by source label; M2 will pass names). Deltas and moves are
/// recovered from the two id maps and CST parents.
pub(crate) fn mapping_between(
    base: &IdentifiedTree,
    side: &IdentifiedTree,
    root: NodeId,
) -> IdentityMapping {
    let mut mapping = IdentityMapping {
        nodes: side.ids.clone(),
        ..IdentityMapping::default()
    };
    let base_nids: BTreeSet<NodeId> = base.ids.values().copied().collect();
    let mut seen_births = BTreeSet::new();
    for nid in side.ids.values() {
        if !base_nids.contains(nid) && seen_births.insert(*nid) {
            mapping.deltas.push(IdentityDelta::Birth { node: *nid });
        }
    }
    for nid in base.ids.values() {
        if !side.ids.values().any(|id| id == nid) {
            mapping.deltas.push(IdentityDelta::Death { node: *nid });
        }
    }
    let base_by = sites_by_id(&collect_sites(&base.tree, &base.ids, root));
    let side_by = sites_by_id(&collect_sites(&side.tree, &side.ids, root));
    for (nid, r) in &side_by {
        let Some(b) = base_by.get(nid) else {
            continue;
        };
        if b.parent_id == r.parent_id {
            continue;
        }
        if b.parent_id == root || r.parent_id == root {
            continue;
        }
        mapping.moves.push(Op::Move {
            node: *nid,
            from_parent: b.parent_id,
            to_parent: r.parent_id,
            index: r.cst_index,
        });
    }
    mapping
}
