//! A change's `write_set` and `identity_deltas`, from its base and result
//! snapshots (ADR 0015 amendment, ADR 0018).
//!
//! `propose` and the lander's rebased records both use [`sets_between`], so a
//! landed record's sets are computed from `head → result` exactly as
//! `propose` computes them from `base → result`.
//!
//! Identity is compared across every changed file at once: a definition
//! carried from a deleted file into a created one (a file move, ADR 0020) is
//! neither a birth nor a death.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{IdentityDelta, NodeId, ObjectId, Op, RepoPath, SnapshotId, TreeOpKind};
use hord_lang::{IdentifiedTree, Site, enclosing_site};

use crate::Result;
use crate::repo::Inner;

/// `write_set` and `identity_deltas` of the change `base → result` whose ops
/// are `ops`.
///
/// Writes are: every definition whose own content (leaves outside nested
/// definitions) or parent changed; births and deaths; the definitions ops
/// move; the file root of each file whose glue changed or that is not parsed
/// on both sides (created, deleted, blob tier, or coarsely rewritten); both
/// paths of a file rename; and every id `declared` mentions.
///
/// Deltas are the births (result ids no changed base file has), then the
/// deaths (base ids no changed result file has), each in id order, then
/// `declared`: derivations recorded by identity carrying or declarations,
/// which a comparison of ids cannot see.
pub(crate) fn sets_between(
    inner: &Inner,
    base: SnapshotId,
    result: SnapshotId,
    ops: &[Op],
    declared: &[IdentityDelta],
) -> Result<(BTreeSet<NodeId>, Vec<IdentityDelta>)> {
    let renamed: BTreeMap<RepoPath, RepoPath> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Tree {
                path,
                kind: TreeOpKind::Rename { to },
            } => Some((to.clone(), path.clone())),
            _ => None,
        })
        .collect();
    let mut writes = BTreeSet::new();
    let mut before: BTreeMap<NodeId, Def> = BTreeMap::new();
    let mut after: BTreeMap<NodeId, Def> = BTreeMap::new();
    for delta in inner.changed_paths(base, result)? {
        let path = &delta.path;
        let old_path = renamed.get(path).unwrap_or(path);
        let base_tree = parsed(inner, base, old_path)?;
        let result_tree = parsed(inner, result, path)?;
        if let Some(tree) = &base_tree
            && old_path == path
        {
            before.extend(defs(tree));
        }
        if let Some(tree) = &result_tree {
            after.extend(defs(tree));
        }
        if let Some(from) = renamed.get(path) {
            // The source file is its own changed path, so its definitions
            // are already in `before`. A move writes both paths (ADR 0020).
            writes.insert(NodeId::file_root(from));
            writes.insert(NodeId::file_root(path));
        }
        let glue_changed = match (&base_tree, &result_tree) {
            (Some(b), Some(r)) => root_glue(b) != root_glue(r),
            _ => true,
        };
        if glue_changed {
            writes.insert(NodeId::file_root(path));
        }
    }
    let mut births = Vec::new();
    for (node, now) in &after {
        match before.get(node) {
            Some(was) if was.changed(now) => {
                writes.insert(*node);
            }
            Some(_) => {}
            None => births.push(*node),
        }
    }
    let deaths: Vec<NodeId> = before
        .keys()
        .filter(|node| !after.contains_key(node))
        .copied()
        .collect();
    for op in ops {
        if let Op::Move { node, .. } = op {
            writes.insert(*node);
        }
    }
    let mut deltas: Vec<IdentityDelta> = births
        .into_iter()
        .map(|node| IdentityDelta::Birth { node })
        .chain(deaths.into_iter().map(|node| IdentityDelta::Death { node }))
        .collect();
    deltas.extend(declared.iter().cloned());
    for delta in &deltas {
        writes.extend(delta.node_ids());
    }
    Ok((writes, deltas))
}

/// The parsed, identified file at `path` in `snapshot`, if any.
fn parsed(
    inner: &Inner,
    snapshot: SnapshotId,
    path: &RepoPath,
) -> Result<Option<Arc<IdentifiedTree>>> {
    Ok(inner
        .file_view(snapshot, path)?
        .and_then(|view| view.parsed)
        .map(|parsed| parsed.tree))
}

/// One definition of a changed file: where it is and under which parent.
struct Def {
    tree: Arc<IdentifiedTree>,
    site: Site,
    /// Content id of the definition's whole subtree.
    content: Option<ObjectId>,
    parent: Option<NodeId>,
}

impl Def {
    /// Whether the definition's own content (leaf content ids outside
    /// nested definitions, list separators skipped) or its parent differs.
    /// Equal subtrees under the same parent are unchanged without a walk.
    fn changed(&self, now: &Def) -> bool {
        if self.parent != now.parent {
            return true;
        }
        if self.content.is_some() && self.content == now.content {
            return false;
        }
        own_leaves(&self.tree, &self.site) != own_leaves(&now.tree, &now.site)
    }
}

/// Every identified definition of `tree`, by id.
fn defs(tree: &Arc<IdentifiedTree>) -> BTreeMap<NodeId, Def> {
    tree.ids
        .iter()
        .map(|(site, id)| {
            let parent = enclosing_site(&tree.ids, site).map(|p| tree.ids[p]);
            let def = Def {
                tree: Arc::clone(tree),
                site: site.clone(),
                content: tree.oid_at(site),
                parent,
            };
            (*id, def)
        })
        .collect()
}

/// Leaves of the file root outside every definition.
pub(crate) fn root_glue(tree: &IdentifiedTree) -> Vec<ObjectId> {
    if tree.tree.root().is_none() {
        return Vec::new();
    }
    own_leaves(tree, &[])
}

/// Leaf content ids under `site`, skipping nested definition sites and the
/// `,`/`;` separators between child definitions (adding a field is a birth,
/// not a write of the struct). A skipped separator that carries a comment
/// still counts: the comment is content of the enclosing definition.
fn own_leaves(tree: &IdentifiedTree, site: &[u32]) -> Vec<ObjectId> {
    fn walk(
        tree: &IdentifiedTree,
        oid: ObjectId,
        site: &mut Vec<u32>,
        top: bool,
        separator: bool,
        out: &mut Vec<ObjectId>,
    ) {
        if !top && tree.ids.contains_key(site.as_slice()) {
            return;
        }
        let Some(node) = tree.tree.get(oid) else {
            return;
        };
        if node.children.is_empty() {
            if !separator || has_comment(&node.raw) {
                out.push(oid);
            }
            return;
        }
        for (i, child) in node.children.iter().enumerate() {
            let separator = is_separator(tree, &node.children, site, i);
            site.push(u32::try_from(i).unwrap_or(u32::MAX));
            walk(tree, *child, site, false, separator, out);
            site.pop();
        }
    }
    /// Child `i` of the node at `site` is a `,`/`;` leaf next to a child
    /// definition.
    fn is_separator(tree: &IdentifiedTree, children: &[ObjectId], site: &[u32], i: usize) -> bool {
        let Some(child) = tree.tree.get(children[i]) else {
            return false;
        };
        if !child.children.is_empty() || !matches!(child.kind.as_str(), "," | ";") {
            return false;
        }
        let is_def = |j: usize| {
            let mut at = site.to_vec();
            at.push(u32::try_from(j).unwrap_or(u32::MAX));
            tree.ids.contains_key(at.as_slice())
        };
        (i > 0 && is_def(i - 1)) || (i + 1 < children.len() && is_def(i + 1))
    }
    /// A separator's raw bytes hold more than the token and whitespace.
    fn has_comment(raw: &hord_core::Bytes) -> bool {
        raw.as_slice()
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .nth(1)
            .is_some()
    }
    let mut out = Vec::new();
    if let Some(oid) = tree.oid_at(site) {
        walk(tree, oid, &mut site.to_vec(), true, false, &mut out);
    }
    out
}
