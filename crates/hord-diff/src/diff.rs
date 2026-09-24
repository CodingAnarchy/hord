//! GumTree-style definition-granularity diff (spec §5.1, ADR 0002).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, Op, RepoPath};
use hord_lang::{IdentifiedTree, IdentityMapping, NodeTree};

use crate::defs::{DefSite, collect_sites, local_glue, root_glue, sites_by_id};

/// Diff `base` → `result` at definition granularity.
///
/// Matching uses [`IdentityMapping`] (exact, named, moved, and rename by
/// tree-edit distance, ADR 0007). Sub-definition edits collapse to [`Op::Replace`] on
/// the enclosing definition (ADR 0002). File-level non-definition glue
/// (`use` items, …) collapses to [`Op::Replace`] on [`NodeId::file_root`]`(path)`.
///
/// `Op`s name new content by [`ObjectId`]; those nodes are interned in
/// `result` and must be passed to [`crate::apply`] as `store`.
#[must_use]
pub fn diff(
    path: &RepoPath,
    base: &IdentifiedTree,
    result: &NodeTree,
    mapping: &IdentityMapping,
) -> Vec<Op> {
    let root = NodeId::file_root(path);
    let ops = diff_structural(base, result, mapping, root);
    ensure_apply_identity(base, result, ops, root)
}

/// Definition-granularity script **without** a whole-file Replace fallback.
///
/// [`diff`] adds that fallback so `apply(diff)` reconstructs `result`. [`crate::merge`]
/// uses this form so two Git-conflicted sides are not collapsed to
/// `Replace(file)` vs `Replace(file)` (which either hard-conflicts or
/// picks one entire file). `root` is the file root's id.
pub(crate) fn diff_structural(
    base: &IdentifiedTree,
    result: &NodeTree,
    mapping: &IdentityMapping,
    root: NodeId,
) -> Vec<Op> {
    if base.tree.root().is_none() {
        return root_only(result, root);
    }
    if result.root().is_none() {
        return deaths_only(mapping);
    }

    let ctx = DiffCtx::new(base, result, mapping, root);
    let (replaces, replace_cover) = ctx.replaces();
    let mut ops = ctx.deletes(&replace_cover);
    ops.extend(replaces);
    ops.extend(ctx.moves(&replace_cover));
    ops.extend(ctx.inserts(&replace_cover));
    ops.extend(ctx.renames(&replace_cover));

    // File-level non-definition glue (comments, leftover tokens) has no
    // NodeId. Represent it as a file-level Replace only when there are no
    // def ops; otherwise keep the def script so merge can compose disjoint
    // definition edits.
    if ctx.glue_changed
        && ops.is_empty()
        && let (Some(from), Some(to)) = (base.tree.root(), result.root())
    {
        return vec![Op::Replace {
            node: root,
            from,
            to,
        }];
    }
    ops
}

struct DiffCtx<'a> {
    base: &'a IdentifiedTree,
    result: &'a NodeTree,
    mapping: &'a IdentityMapping,
    root: NodeId,
    base_sites: Vec<DefSite>,
    result_sites: Vec<DefSite>,
    base_by_id: BTreeMap<NodeId, DefSite>,
    result_by_id: BTreeMap<NodeId, DefSite>,
    dead: BTreeSet<NodeId>,
    born: BTreeSet<NodeId>,
    glue_changed: bool,
}

impl<'a> DiffCtx<'a> {
    fn new(
        base: &'a IdentifiedTree,
        result: &'a NodeTree,
        mapping: &'a IdentityMapping,
        root: NodeId,
    ) -> Self {
        let result_ids = &mapping.nodes;
        let base_sites = collect_sites(&base.tree, &base.ids, root);
        let result_sites = collect_sites(result, result_ids, root);
        let glue_changed = root_glue(&base.tree, &base.ids) != root_glue(result, result_ids);
        let (dead, born) = delta_sets(mapping);
        Self {
            base,
            result,
            mapping,
            root,
            base_by_id: sites_by_id(&base_sites),
            result_by_id: sites_by_id(&result_sites),
            base_sites,
            result_sites,
            dead,
            born,
            glue_changed,
        }
    }

    /// Preorder, not NodeId order. NodeIds are random ULIDs, and `covered`
    /// keeps a parent Replace from also emitting child Replaces.
    fn replaces(&self) -> (Vec<Op>, BTreeSet<NodeId>) {
        let mut replace_cover = BTreeSet::new();
        let mut replaces = Vec::new();
        for site in &self.result_sites {
            let Some(base_site) = self.base_by_id.get(&site.node_id) else {
                continue;
            };
            if base_site.object_id == site.object_id {
                continue;
            }
            let base_glue = local_glue(&self.base.tree, &base_site.site, &self.base.ids);
            let result_glue = local_glue(self.result, &site.site, &self.mapping.nodes);
            if base_glue == result_glue {
                // Container header/braces unchanged: child def ops carry the edit.
                continue;
            }
            if covered(site.node_id, &self.result_by_id, &replace_cover, self.root) {
                continue;
            }
            replaces.push(Op::Replace {
                node: site.node_id,
                from: base_site.object_id,
                to: site.object_id,
            });
            replace_cover.insert(site.node_id);
        }
        (replaces, replace_cover)
    }

    fn deletes(&self, replace_cover: &BTreeSet<NodeId>) -> Vec<Op> {
        let mut ops = Vec::new();
        for site in &self.base_sites {
            if !self.dead.contains(&site.node_id) {
                continue;
            }
            if self.dead.contains(&site.parent_id) && site.parent_id != self.root {
                continue;
            }
            if covered(site.node_id, &self.base_by_id, replace_cover, self.root) {
                continue;
            }
            ops.push(Op::Delete { node: site.node_id });
        }
        ops
    }

    fn moves(&self, replace_cover: &BTreeSet<NodeId>) -> Vec<Op> {
        let mut ops = Vec::new();
        for mv in &self.mapping.moves {
            let Op::Move {
                node,
                from_parent,
                to_parent,
                ..
            } = mv
            else {
                continue;
            };
            if self.dead.contains(node) || self.born.contains(node) {
                continue;
            }
            if covered(*node, &self.result_by_id, replace_cover, self.root) {
                continue;
            }
            if covered(*to_parent, &self.result_by_id, replace_cover, self.root)
                || covered(*from_parent, &self.base_by_id, replace_cover, self.root)
            {
                continue;
            }
            ops.push(mv.clone());
        }
        ops
    }

    fn inserts(&self, replace_cover: &BTreeSet<NodeId>) -> Vec<Op> {
        let mut ops = Vec::new();
        for site in &self.result_sites {
            if !self.born.contains(&site.node_id) {
                continue;
            }
            if self.born.contains(&site.parent_id) && site.parent_id != self.root {
                continue;
            }
            if covered(site.node_id, &self.result_by_id, replace_cover, self.root) {
                continue;
            }
            if covered(site.parent_id, &self.result_by_id, replace_cover, self.root)
                && site.parent_id != self.root
            {
                continue;
            }
            ops.push(Op::Insert {
                parent: site.parent_id,
                index: site.cst_index,
                node: site.object_id,
            });
        }
        ops
    }

    fn renames(&self, replace_cover: &BTreeSet<NodeId>) -> Vec<Op> {
        let mut ops = Vec::new();
        for (nid, site) in &self.result_by_id {
            let Some(base_site) = self.base_by_id.get(nid) else {
                continue;
            };
            let Some(base_node) = self.base.tree.get(base_site.object_id) else {
                continue;
            };
            let Some(result_node) = self.result.get(site.object_id) else {
                continue;
            };
            if base_node.name == result_node.name {
                continue;
            }
            let (Some(from), Some(to)) = (base_node.name.clone(), result_node.name.clone()) else {
                continue;
            };
            if covered(*nid, &self.result_by_id, replace_cover, self.root) {
                continue;
            }
            ops.push(Op::Rename {
                node: *nid,
                from,
                to,
            });
        }
        ops
    }
}

fn delta_sets(mapping: &IdentityMapping) -> (BTreeSet<NodeId>, BTreeSet<NodeId>) {
    let mut dead = BTreeSet::new();
    let mut born = BTreeSet::new();
    for delta in &mapping.deltas {
        match delta {
            IdentityDelta::Death { node } => {
                dead.insert(*node);
            }
            IdentityDelta::Birth { node } => {
                born.insert(*node);
            }
            _ => {}
        }
    }
    (dead, born)
}

/// If the script does not reconstruct `result`'s root, fall back to a file-level
/// Replace. Definition granularity is a quality metric; apply identity is the gate.
fn ensure_apply_identity(
    base: &IdentifiedTree,
    result: &NodeTree,
    ops: Vec<Op>,
    root: NodeId,
) -> Vec<Op> {
    let Some(to) = result.root() else {
        return ops;
    };
    let Some(from) = base.tree.root() else {
        return ops;
    };
    if from == to {
        return ops;
    }
    let whole_file = || {
        vec![Op::Replace {
            node: root,
            from,
            to,
        }]
    };
    if ops.is_empty() {
        return whole_file();
    }
    match crate::apply::apply_internal(base, &ops, result, root) {
        Ok(applied) if applied.tree.root() == Some(to) => ops,
        _ => whole_file(),
    }
}

fn root_only(result: &NodeTree, root: NodeId) -> Vec<Op> {
    let Some(to) = result.root() else {
        return Vec::new();
    };
    vec![Op::Replace {
        node: root,
        from: ObjectId::from_bytes([0; 32]),
        to,
    }]
}

fn deaths_only(mapping: &IdentityMapping) -> Vec<Op> {
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
    root: NodeId,
) -> bool {
    let mut cur = nid;
    loop {
        if replace_cover.contains(&cur) && cur != nid {
            return true;
        }
        let Some(site) = sites.get(&cur) else {
            return false;
        };
        if site.parent_id == root || site.parent_id == cur {
            return replace_cover.contains(&root);
        }
        cur = site.parent_id;
    }
}
