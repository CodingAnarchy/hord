//! GumTree-style definition-granularity diff (spec §5.1, ADR 0002).

use std::collections::BTreeSet;

use hord_core::{IdentityDelta, NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, IdentityMapping, NodeTree};

use crate::defs::{
    collect_sites, def_oids, file_parent, local_skeleton, result_ids, root_glue, sites_by_id,
};

/// Diff `base` → `result` at definition granularity.
///
/// Matching uses [`IdentityMapping`] (exact / named / moved; rename
/// similarity is M2). Sub-definition edits collapse to [`Op::Replace`] on
/// the enclosing definition (ADR 0002). File-level non-definition glue
/// (`use` items, …) collapses to [`Op::Replace`] on [`file_parent`].
///
/// `Op`s name new content by [`ObjectId`]; those nodes are interned in
/// `result` and must be passed to [`crate::apply`] as `store`.
#[must_use]
pub fn diff(base: &IdentifiedTree, result: &NodeTree, mapping: &IdentityMapping) -> Vec<Op> {
    let Some(base_root) = base.tree.root() else {
        return root_only(result);
    };
    let Some(result_root) = result.root() else {
        return deaths_only(base, mapping);
    };

    let result_ids = result_ids(mapping);
    let base_sites = collect_sites(&base.tree, &base.ids);
    let result_sites = collect_sites(result, result_ids);
    let base_by_id = sites_by_id(&base_sites);
    let result_by_id = sites_by_id(&result_sites);

    let base_glue = root_glue(&base.tree, &def_oids(&base.ids));
    let result_glue = root_glue(result, &def_oids(result_ids));
    if base_glue != result_glue {
        return vec![Op::Replace {
            node: file_parent(),
            from: base_root,
            to: result_root,
        }];
    }

    let dead: BTreeSet<NodeId> = mapping
        .deltas
        .iter()
        .filter_map(|d| match d {
            IdentityDelta::Death { node } => Some(*node),
            _ => None,
        })
        .collect();
    let born: BTreeSet<NodeId> = mapping
        .deltas
        .iter()
        .filter_map(|d| match d {
            IdentityDelta::Birth { node } => Some(*node),
            _ => None,
        })
        .collect();

    let mut replace_cover: BTreeSet<NodeId> = BTreeSet::new();
    let mut replaces = Vec::new();

    for (nid, r) in &result_by_id {
        let Some(b) = base_by_id.get(nid) else {
            continue;
        };
        if b.object_id == r.object_id {
            continue;
        }
        let base_skel = local_skeleton(&base.tree, b.object_id, &base.ids);
        let result_skel = local_skeleton(result, r.object_id, result_ids);
        if base_skel == result_skel {
            continue;
        }
        if covered(*nid, &result_by_id, &replace_cover) {
            continue;
        }
        replaces.push(Op::Replace {
            node: *nid,
            from: b.object_id,
            to: r.object_id,
        });
        replace_cover.insert(*nid);
    }

    let mut ops = Vec::new();

    for site in &base_sites {
        if !dead.contains(&site.node_id) {
            continue;
        }
        if dead.contains(&site.parent_id) && site.parent_id != file_parent() {
            continue;
        }
        if covered(site.node_id, &base_by_id, &replace_cover) {
            continue;
        }
        ops.push(Op::Delete { node: site.node_id });
    }

    ops.extend(replaces);

    for mv in &mapping.moves {
        let Op::Move {
            node, to_parent, ..
        } = mv
        else {
            continue;
        };
        if dead.contains(node) || born.contains(node) {
            continue;
        }
        if covered(*node, &result_by_id, &replace_cover) {
            continue;
        }
        if born.contains(to_parent) {
            // Destination arrives as a grafted Insert; still emit Move so
            // apply removes the node from the old parent.
        }
        if covered(*to_parent, &result_by_id, &replace_cover)
            || covered(move_from_parent(mv), &base_by_id, &replace_cover)
        {
            continue;
        }
        ops.push(mv.clone());
    }

    for site in &result_sites {
        if !born.contains(&site.node_id) {
            continue;
        }
        if born.contains(&site.parent_id) && site.parent_id != file_parent() {
            continue;
        }
        if covered(site.node_id, &result_by_id, &replace_cover) {
            continue;
        }
        if covered(site.parent_id, &result_by_id, &replace_cover) && site.parent_id != file_parent()
        {
            continue;
        }
        ops.push(Op::Insert {
            parent: site.parent_id,
            index: site.cst_index,
            node: site.object_id,
        });
    }

    for (nid, r) in &result_by_id {
        let Some(b) = base_by_id.get(nid) else {
            continue;
        };
        let Some(base_node) = base.tree.get(b.object_id) else {
            continue;
        };
        let Some(result_node) = result.get(r.object_id) else {
            continue;
        };
        if base_node.name == result_node.name {
            continue;
        }
        let (Some(from), Some(to)) = (base_node.name.clone(), result_node.name.clone()) else {
            continue;
        };
        if covered(*nid, &result_by_id, &replace_cover) {
            continue;
        }
        ops.push(Op::Rename {
            node: *nid,
            from,
            to,
        });
    }

    ensure_apply_identity(base, result, ops)
}

/// If the script does not reconstruct `result`'s root, fall back to a file-level
/// Replace. Definition granularity is a quality metric; apply identity is the gate.
fn ensure_apply_identity(base: &IdentifiedTree, result: &NodeTree, ops: Vec<Op>) -> Vec<Op> {
    let Some(to) = result.root() else {
        return ops;
    };
    let Some(from) = base.tree.root() else {
        return ops;
    };
    if from == to {
        return ops;
    }
    if ops.is_empty() {
        return vec![Op::Replace {
            node: file_parent(),
            from,
            to,
        }];
    }
    match crate::apply(base, &ops, result) {
        Ok(applied) if applied.tree.root() == Some(to) => ops,
        _ => vec![Op::Replace {
            node: file_parent(),
            from,
            to,
        }],
    }
}

fn root_only(result: &NodeTree) -> Vec<Op> {
    let Some(root) = result.root() else {
        return Vec::new();
    };
    vec![Op::Replace {
        node: file_parent(),
        from: ObjectId::from_bytes([0; 32]),
        to: root,
    }]
}

fn deaths_only(_base: &IdentifiedTree, mapping: &IdentityMapping) -> Vec<Op> {
    mapping
        .deltas
        .iter()
        .filter_map(|d| match d {
            IdentityDelta::Death { node } => Some(Op::Delete { node: *node }),
            _ => None,
        })
        .collect()
}

fn covered(
    nid: NodeId,
    sites: &std::collections::BTreeMap<NodeId, crate::defs::DefSite>,
    replace_cover: &BTreeSet<NodeId>,
) -> bool {
    let mut cur = nid;
    loop {
        if replace_cover.contains(&cur) && cur != nid {
            return true;
        }
        let Some(site) = sites.get(&cur) else {
            return false;
        };
        if site.parent_id == file_parent() || site.parent_id == cur {
            return replace_cover.contains(&file_parent());
        }
        cur = site.parent_id;
    }
}

fn move_from_parent(op: &Op) -> NodeId {
    match op {
        Op::Move { from_parent, .. } => *from_parent,
        _ => file_parent(),
    }
}
