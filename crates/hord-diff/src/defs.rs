//! Definition sites and file-root glue (definition-granularity matching).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, IdentityMapping, NodeTree};

/// Parent [`NodeId`] of file-level definitions.
///
/// The CST root (`source_file`, `document`, …) is not a definition, but
/// [`hord_core::Op::Insert`] requires a parent id. [`NodeId::nil`] stands in
/// for that file root. [`hord_core::Op::Replace`] on this id replaces the
/// whole file (file-level non-definition glue).
#[must_use]
pub fn file_parent() -> NodeId {
    NodeId::nil()
}

#[derive(Clone, Debug)]
pub(crate) struct DefSite {
    pub node_id: NodeId,
    pub object_id: ObjectId,
    pub parent_id: NodeId,
    pub cst_index: u32,
}

/// Preorder definition sites in `tree` using `ids` as the def set.
pub(crate) fn collect_sites(tree: &NodeTree, ids: &BTreeMap<ObjectId, NodeId>) -> Vec<DefSite> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree, ids, root, file_parent(), 0, &mut out);
    out
}

fn walk(
    tree: &NodeTree,
    ids: &BTreeMap<ObjectId, NodeId>,
    oid: ObjectId,
    parent_id: NodeId,
    cst_index: u32,
    out: &mut Vec<DefSite>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let mut child_parent = parent_id;
    if let Some(&node_id) = ids.get(&oid) {
        out.push(DefSite {
            node_id,
            object_id: oid,
            parent_id,
            cst_index,
        });
        child_parent = node_id;
    }
    for (i, child) in node.children.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        walk(tree, ids, *child, child_parent, idx, out);
    }
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
/// with no def ops collapses to [`file_parent`] Replace.
pub(crate) fn root_glue(tree: &NodeTree, def_oids: &BTreeSet<ObjectId>) -> Vec<ObjectId> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    glue_walk(tree, root, def_oids, true, &mut out);
    out
}

fn glue_walk(
    tree: &NodeTree,
    oid: ObjectId,
    def_oids: &BTreeSet<ObjectId>,
    at_root: bool,
    out: &mut Vec<ObjectId>,
) {
    if !at_root && def_oids.contains(&oid) {
        return;
    }
    let Some(node) = tree.get(oid) else {
        return;
    };
    if node.children.is_empty() {
        if !at_root || !def_oids.contains(&oid) {
            out.push(oid);
        }
        return;
    }
    for child in &node.children {
        glue_walk(tree, *child, def_oids, false, out);
    }
}

/// Non-definition leaves of a definition, using `normalized` so trivia-only
/// edits do not look like a container change.
///
/// Child definitions are skipped entirely (not recorded as holes). Adding,
/// deleting, or editing a method therefore does not [`Op::Replace`] the
/// enclosing `impl`/`mod`/`table`; those edits are child ops so two sides
/// that touch different children can compose (spec §5.2 rule 1).
pub(crate) fn local_glue(
    tree: &NodeTree,
    start: ObjectId,
    def_ids: &BTreeMap<ObjectId, NodeId>,
) -> Vec<ObjectId> {
    let mut out = Vec::new();
    glue_skel(tree, start, start, def_ids, &mut out);
    out
}

fn glue_skel(
    tree: &NodeTree,
    oid: ObjectId,
    start: ObjectId,
    def_ids: &BTreeMap<ObjectId, NodeId>,
    out: &mut Vec<ObjectId>,
) {
    if oid != start && def_ids.contains_key(&oid) {
        return;
    }
    let Some(node) = tree.get(oid) else {
        return;
    };
    if node.children.is_empty() {
        // List separators sit between child definitions (`struct { a, b }`,
        // `enum { A, B }`). They are not container header glue; skipping
        // them lets adding a field/variant be a child Insert instead of
        // Replace on the parent.
        if matches!(node.kind.as_str(), "," | ";") {
            return;
        }
        out.push(node.normalized);
        return;
    }
    for child in &node.children {
        glue_skel(tree, *child, start, def_ids, out);
    }
}

pub(crate) fn oid_of(tree: &IdentifiedTree, id: NodeId) -> Option<ObjectId> {
    if id == file_parent() {
        return tree.tree.root();
    }
    tree.ids
        .iter()
        .find_map(|(oid, nid)| (*nid == id).then_some(*oid))
}

pub(crate) fn path_to(tree: &NodeTree, target: ObjectId) -> Option<Vec<(ObjectId, usize)>> {
    let root = tree.root()?;
    let mut path = Vec::new();
    if dfs(tree, root, target, &mut path) {
        Some(path)
    } else {
        None
    }
}

fn dfs(
    tree: &NodeTree,
    current: ObjectId,
    target: ObjectId,
    path: &mut Vec<(ObjectId, usize)>,
) -> bool {
    if current == target {
        return true;
    }
    let Some(node) = tree.get(current) else {
        return false;
    };
    for (i, child) in node.children.iter().enumerate() {
        path.push((current, i));
        if dfs(tree, *child, target, path) {
            return true;
        }
        path.pop();
    }
    false
}

pub(crate) fn def_oids(ids: &BTreeMap<ObjectId, NodeId>) -> BTreeSet<ObjectId> {
    ids.keys().copied().collect()
}

/// Build an [`IdentityMapping`] from ids already on `side`, relative to `base`.
///
/// [`LangAdapter::identify`] is not used: the caller owns carrying (tests
/// pair by source label; M2 will pass names). Deltas and moves are
/// recovered from the two id maps and CST parents.
pub(crate) fn mapping_between(base: &IdentifiedTree, side: &IdentifiedTree) -> IdentityMapping {
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
    let base_by = sites_by_id(&collect_sites(&base.tree, &base.ids));
    let side_by = sites_by_id(&collect_sites(&side.tree, &side.ids));
    for (nid, r) in &side_by {
        let Some(b) = base_by.get(nid) else {
            continue;
        };
        if b.parent_id == r.parent_id {
            continue;
        }
        if b.parent_id == file_parent() || r.parent_id == file_parent() {
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
