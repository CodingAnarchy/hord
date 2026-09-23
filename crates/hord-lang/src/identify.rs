//! Default identity carrying (spec §3.4 steps 1–3).

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, Node, NodeId, NodeKind, ObjectId, Op, QualifiedName};

use crate::adapter::LangAdapter;
use crate::tree::NodeTree;

/// Where a node sits in a tree: the child indices from the root down to it.
/// The root is the empty site.
///
/// Identity is keyed by site, not by content: two definitions with the same
/// text (two identical `use` lines in different functions) are one interned
/// node but two sites, and each keeps its own [`NodeId`] (spec §3.4). This is
/// the [`hord_core::NodePath::pointer`] of an identity map. Sites order
/// lexicographically, which is preorder.
pub type Site = Vec<u32>;

/// A [`NodeTree`] plus durable [`NodeId`]s for definition nodes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentifiedTree {
    /// Interned CST.
    pub tree: NodeTree,
    /// Definition site → stable identity.
    pub ids: BTreeMap<Site, NodeId>,
}

impl IdentifiedTree {
    /// Wrap an interned tree and its definition ids.
    #[must_use]
    pub fn new(tree: NodeTree, ids: BTreeMap<Site, NodeId>) -> Self {
        Self { tree, ids }
    }

    /// Estimated heap bytes: [`NodeTree::resident_bytes`] plus the id map.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        // BTreeMap slot, the site vector's header, and its indices.
        const PER_ID: usize = 64 + std::mem::size_of::<(Site, NodeId)>();
        self.tree.resident_bytes()
            + self
                .ids
                .keys()
                .map(|site| PER_ID + site.len() * std::mem::size_of::<u32>())
                .sum::<usize>()
    }

    /// Content id of the node at `site`.
    #[must_use]
    pub fn oid_at(&self, site: &[u32]) -> Option<ObjectId> {
        oid_at(&self.tree, site)
    }

    /// The node at `site`.
    #[must_use]
    pub fn node_at(&self, site: &[u32]) -> Option<&Node> {
        self.tree.get(self.oid_at(site)?)
    }

    /// Site of the definition with identity `node`.
    #[must_use]
    pub fn site_of(&self, node: NodeId) -> Option<&Site> {
        self.ids
            .iter()
            .find_map(|(site, id)| (*id == node).then_some(site))
    }

    /// Every identified definition: site, content id, identity, in preorder.
    pub fn definitions(&self) -> impl Iterator<Item = (&Site, ObjectId, NodeId)> + '_ {
        self.ids
            .iter()
            .filter_map(|(site, id)| Some((site, self.oid_at(site)?, *id)))
    }
}

/// Content id of the node at `site` in `tree`.
#[must_use]
pub fn oid_at(tree: &NodeTree, site: &[u32]) -> Option<ObjectId> {
    let mut oid = tree.root()?;
    for index in site {
        oid = *tree.get(oid)?.children.get(*index as usize)?;
    }
    Some(oid)
}

/// Result of [`default_identify`]: carried ids, births/deaths, and moves.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentityMapping {
    /// Result definition [`Site`] → carried or newly assigned [`NodeId`].
    pub nodes: BTreeMap<Site, NodeId>,
    /// [`IdentityDelta::Birth`] for unmatched result defs and
    /// [`IdentityDelta::Death`] for unmatched base defs.
    pub deltas: Vec<IdentityDelta>,
    /// [`Op::Move`] from carrying rule 3 (same `normalized`, different parent).
    ///
    /// Emitted only when both the old and new nearest enclosing definitions
    /// have [`NodeId`]s. `index` is the node's position in its immediate CST
    /// parent's `children` list.
    pub moves: Vec<Op>,
    /// [`Op::Rename`] from carrying rule 4 (ADR 0007).
    ///
    /// Emitted only when both sides have a [`QualifiedName`]. The carried
    /// [`NodeId`] is the same either way; [`IdentityDelta::DerivedFrom`]
    /// records the pair.
    pub renames: Vec<Op>,
}

#[derive(Clone, Debug)]
struct BaseDef {
    object_id: ObjectId,
    node_id: NodeId,
    parent_id: Option<NodeId>,
    kind: NodeKind,
    normalized: ObjectId,
    name: Option<QualifiedName>,
    used: bool,
}

#[derive(Clone, Debug)]
struct ResultDef {
    object_id: ObjectId,
    site: Site,
    parent_def_site: Option<Site>,
    index: u32,
    kind: NodeKind,
    normalized: ObjectId,
    name: Option<QualifiedName>,
}

/// Carry [`NodeId`]s from `base` onto `result` (spec §3.4).
///
/// Applied in order, definition nodes only:
///
/// 1. **Exact:** same kind, same `normalized`, and same parent [`NodeId`].
///    Two named defs with different names are not paired (identical empty
///    siblings would otherwise swap).
/// 2. **Named:** same [`QualifiedName`] under the same parent (`name` from
///    [`Node::name`] or [`LangAdapter::qualified_name`]).
/// 3. **Moved:** same `normalized` under a different parent; emit [`Op::Move`].
/// 4. **Renamed:** unmatched definitions of the same kind whose trivia-stripped
///    tree-edit-distance ratio is at least 0.8 (ADR 0007). Emit [`Op::Rename`]
///    and [`IdentityDelta::DerivedFrom`].
///
/// Step 5 (declared relations) is not supplied to this function.
/// Remaining result defs are births; remaining base defs are deaths.
///
/// Matching is top-down: a parent is identified before its children are
/// considered for exact/named. After a move identifies a parent, children
/// are re-tried. Pairing is first-unmatched in preorder, deterministically.
pub fn default_identify<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    result: &NodeTree,
) -> IdentityMapping {
    let mut base_defs = collect_base_defs(adapter, base);
    let result_defs = collect_result_defs(adapter, result);

    let mut mapping = IdentityMapping::default();

    loop {
        let mut progress = false;
        progress |= match_pass(
            &mut mapping.nodes,
            &mut base_defs,
            &result_defs,
            MatchKind::Exact,
        );
        progress |= match_pass(
            &mut mapping.nodes,
            &mut base_defs,
            &result_defs,
            MatchKind::Named,
        );
        if progress {
            continue;
        }
        if match_pass(
            &mut mapping.nodes,
            &mut base_defs,
            &result_defs,
            MatchKind::Moved,
        ) {
            continue;
        }
        break;
    }

    rename_pass(
        &base.tree,
        result,
        &mut mapping,
        &mut base_defs,
        &result_defs,
    );

    for r in &result_defs {
        if mapping.nodes.contains_key(&r.site) {
            continue;
        }
        let id = NodeId::generate();
        mapping.nodes.insert(r.site.clone(), id);
        mapping.deltas.push(IdentityDelta::Birth { node: id });
    }

    for b in &base_defs {
        if !b.used {
            mapping
                .deltas
                .push(IdentityDelta::Death { node: b.node_id });
        }
    }

    for r in &result_defs {
        let Some(&nid) = mapping.nodes.get(&r.site) else {
            continue;
        };
        let Some(b) = base_defs.iter().find(|d| d.used && d.node_id == nid) else {
            continue;
        };
        let to_parent = r
            .parent_def_site
            .as_ref()
            .and_then(|site| mapping.nodes.get(site).copied());
        if b.parent_id == to_parent {
            continue;
        }
        let (Some(from_parent), Some(to_parent)) = (b.parent_id, to_parent) else {
            continue;
        };
        mapping.moves.push(Op::Move {
            node: nid,
            from_parent,
            to_parent,
            index: r.index,
        });
    }

    mapping
}

#[derive(Clone, Copy)]
enum MatchKind {
    Exact,
    Named,
    Moved,
}

fn match_pass(
    result_ids: &mut BTreeMap<Site, NodeId>,
    base_defs: &mut [BaseDef],
    result_defs: &[ResultDef],
    kind: MatchKind,
) -> bool {
    let mut progress = false;
    for r in result_defs {
        if result_ids.contains_key(&r.site) {
            continue;
        }
        let parent_nid = r
            .parent_def_site
            .as_ref()
            .and_then(|site| result_ids.get(site).copied());
        // Wait until the nearest enclosing definition is identified so
        // exact/named/moved see a stable parent [`NodeId`].
        if r.parent_def_site.is_some() && parent_nid.is_none() {
            continue;
        }

        let found = base_defs.iter_mut().find(|b| {
            if b.used {
                return false;
            }
            match kind {
                MatchKind::Exact => {
                    b.kind == r.kind
                        && b.normalized == r.normalized
                        && b.parent_id == parent_nid
                        && !names_conflict(&b.name, &r.name)
                }
                MatchKind::Named => names_equal(&b.name, &r.name) && b.parent_id == parent_nid,
                MatchKind::Moved => b.kind == r.kind && b.normalized == r.normalized,
            }
        });
        if let Some(b) = found {
            b.used = true;
            result_ids.insert(r.site.clone(), b.node_id);
            progress = true;
        }
    }
    progress
}

struct RenameCandidate {
    base_id: NodeId,
    preorder: usize,
    slack: usize,
}

fn rename_pass(
    base_tree: &NodeTree,
    result_tree: &NodeTree,
    mapping: &mut IdentityMapping,
    base_defs: &mut [BaseDef],
    result_defs: &[ResultDef],
) {
    let mut candidates = rename_candidates(base_tree, result_tree, mapping, base_defs, result_defs);
    // Highest slack, then stable ids, then preorder (ADR 0007 tie-break).
    candidates.sort_by(|left, right| {
        right
            .slack
            .cmp(&left.slack)
            .then(left.base_id.as_u128().cmp(&right.base_id.as_u128()))
            .then(left.preorder.cmp(&right.preorder))
    });
    assign_renames(mapping, base_defs, result_defs, &candidates);
}

fn rename_candidates(
    base_tree: &NodeTree,
    result_tree: &NodeTree,
    mapping: &IdentityMapping,
    base_defs: &[BaseDef],
    result_defs: &[ResultDef],
) -> Vec<RenameCandidate> {
    let mut candidates = Vec::new();
    for (preorder, result_def) in result_defs.iter().enumerate() {
        if mapping.nodes.contains_key(&result_def.site) {
            continue;
        }
        for base_def in base_defs {
            if base_def.used || base_def.kind != result_def.kind {
                continue;
            }
            let Some(slack) = crate::rename::similarity_slack(
                base_tree,
                base_def.object_id,
                result_tree,
                result_def.object_id,
            ) else {
                continue;
            };
            candidates.push(RenameCandidate {
                base_id: base_def.node_id,
                preorder,
                slack,
            });
        }
    }
    candidates
}

fn assign_renames(
    mapping: &mut IdentityMapping,
    base_defs: &mut [BaseDef],
    result_defs: &[ResultDef],
    candidates: &[RenameCandidate],
) {
    let mut taken_result = BTreeSet::new();
    let mut taken_base = BTreeSet::new();
    for candidate in candidates {
        if taken_base.contains(&candidate.base_id) || taken_result.contains(&candidate.preorder) {
            continue;
        }
        taken_base.insert(candidate.base_id);
        taken_result.insert(candidate.preorder);
        let Some(base_def) = base_defs
            .iter_mut()
            .find(|def| def.node_id == candidate.base_id)
        else {
            continue;
        };
        base_def.used = true;
        let base_name = base_def.name.clone();
        mapping.nodes.insert(
            result_defs[candidate.preorder].site.clone(),
            candidate.base_id,
        );
        mapping.deltas.push(IdentityDelta::DerivedFrom {
            node: candidate.base_id,
            from: candidate.base_id,
        });
        let result_name = result_defs[candidate.preorder].name.clone();
        if let (Some(from), Some(to)) = (base_name, result_name)
            && from != to
        {
            mapping.renames.push(Op::Rename {
                node: candidate.base_id,
                from,
                to,
            });
        }
    }
}

fn names_equal(a: &Option<QualifiedName>, b: &Option<QualifiedName>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Exact matching does not pair two named defs with different names, even
/// when `normalized` collides (e.g. two empty modules).
fn names_conflict(a: &Option<QualifiedName>, b: &Option<QualifiedName>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

/// Preorder. `visit` gets the node's site and returns the parent state seen
/// by its children.
fn walk_defs<T: Clone>(
    tree: &NodeTree,
    oid: ObjectId,
    ancestors: &mut Vec<ObjectId>,
    site: &mut Site,
    state: T,
    visit: &mut impl FnMut(&Node, ObjectId, &[ObjectId], &[u32], T) -> T,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let child_state = visit(node, oid, ancestors, site, state);
    ancestors.push(oid);
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        walk_defs(tree, *child, ancestors, site, child_state.clone(), visit);
        site.pop();
    }
    ancestors.pop();
}

fn collect_base_defs<A: LangAdapter + ?Sized>(adapter: &A, base: &IdentifiedTree) -> Vec<BaseDef> {
    let Some(root) = base.tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ancestors = Vec::new();
    walk_defs(
        &base.tree,
        root,
        &mut ancestors,
        &mut Vec::new(),
        None,
        &mut |node, oid, ancestors, site, parent: Option<NodeId>| {
            if !(adapter.is_definition(&node.kind)) {
                return parent;
            }
            let Some(&node_id) = base.ids.get(site) else {
                return parent;
            };
            out.push(BaseDef {
                object_id: oid,
                node_id,
                parent_id: parent,
                kind: node.kind,
                normalized: node.normalized,
                name: def_name(adapter, &base.tree, ancestors, node),
                used: false,
            });
            Some(node_id)
        },
    );
    out
}

fn collect_result_defs<A: LangAdapter + ?Sized>(adapter: &A, tree: &NodeTree) -> Vec<ResultDef> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ancestors = Vec::new();
    walk_defs(
        tree,
        root,
        &mut ancestors,
        &mut Vec::new(),
        None,
        &mut |node, oid, ancestors, site, parent: Option<Site>| {
            if !adapter.is_definition(&node.kind) {
                return parent;
            }
            out.push(ResultDef {
                object_id: oid,
                site: site.to_vec(),
                parent_def_site: parent,
                index: site.last().copied().unwrap_or(0),
                kind: node.kind,
                normalized: node.normalized,
                name: def_name(adapter, tree, ancestors, node),
            });
            Some(site.to_vec())
        },
    );
    out
}

fn def_name<A: LangAdapter + ?Sized>(
    adapter: &A,
    tree: &NodeTree,
    ancestor_ids: &[ObjectId],
    node: &Node,
) -> Option<QualifiedName> {
    if let Some(name) = node.name.clone() {
        return Some(name);
    }
    let ancestors: Vec<&Node> = ancestor_ids.iter().filter_map(|id| tree.get(*id)).collect();
    adapter.qualified_name(&ancestors, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestAdapter;
    use crate::tree::NodeTree;
    use crate::trivia::AttachedToken;
    use hord_core::{Bytes, LangId, NodeKind, QualifiedName};

    fn lang() -> LangId {
        LangId::new("test")
    }

    fn leaf(tree: &mut NodeTree, kind: &str, text: &str, name: Option<&str>) -> ObjectId {
        let token = AttachedToken {
            kind: NodeKind::new(kind),
            leading: Bytes::default(),
            text: Bytes::from(text.as_bytes()),
            trailing: Bytes::default(),
        };
        tree.intern_token(lang(), &token, name.map(QualifiedName::new))
            .unwrap()
    }

    /// First site of `oid` in `tree`, in preorder.
    fn site(tree: &NodeTree, oid: ObjectId) -> Site {
        fn walk(tree: &NodeTree, at: ObjectId, want: ObjectId, path: &mut Site) -> bool {
            if at == want {
                return true;
            }
            let Some(node) = tree.get(at) else {
                return false;
            };
            for (i, child) in node.children.iter().enumerate() {
                path.push(u32::try_from(i).unwrap());
                if walk(tree, *child, want, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
        let mut path = Vec::new();
        assert!(
            walk(tree, tree.root().expect("root"), oid, &mut path),
            "{oid} not in tree"
        );
        path
    }

    fn nid(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    #[test]
    fn exact_same_normalized_and_parent_keeps_id() {
        let adapter = TestAdapter;
        let mut tree = NodeTree::new();
        let foo = leaf(&mut tree, "fn", "foo_body", Some("foo"));
        let root = tree
            .intern_branch(NodeKind::new("file"), lang(), vec![foo], None)
            .unwrap();
        tree.set_root(root).unwrap();

        let mut ids = BTreeMap::new();
        ids.insert(site(&tree, foo), nid(1));
        let base = IdentifiedTree::new(tree.clone(), ids);
        let mapping = default_identify(&adapter, &base, &tree);

        assert_eq!(mapping.nodes.get(&site(&tree, foo)).copied(), Some(nid(1)));
        assert!(mapping.deltas.is_empty());
        assert!(mapping.moves.is_empty());
    }

    #[test]
    fn named_same_parent_keeps_id_when_body_changes() {
        let adapter = TestAdapter;
        let mut base_tree = NodeTree::new();
        let foo_old = leaf(&mut base_tree, "fn", "old_body", Some("foo"));
        let base_root = base_tree
            .intern_branch(NodeKind::new("file"), lang(), vec![foo_old], None)
            .unwrap();
        base_tree.set_root(base_root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&base_tree, foo_old), nid(1));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let foo_new = leaf(&mut result, "fn", "new_body", Some("foo"));
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![foo_new], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        let mapping = default_identify(&adapter, &base, &result);
        assert_ne!(foo_old, foo_new);
        assert_eq!(
            mapping.nodes.get(&site(&result, foo_new)).copied(),
            Some(nid(1))
        );
        assert!(mapping.deltas.is_empty());
        assert!(mapping.moves.is_empty());
    }

    #[test]
    fn moved_same_normalized_different_parent_emits_move() {
        let adapter = TestAdapter;

        let mut base_tree = NodeTree::new();
        let foo = leaf(&mut base_tree, "fn", "foo_body", Some("foo"));
        let mod_a = base_tree
            .intern_branch(
                NodeKind::new("mod"),
                lang(),
                vec![foo],
                Some(QualifiedName::new("A")),
            )
            .unwrap();
        let mod_b = base_tree
            .intern_branch(
                NodeKind::new("mod"),
                lang(),
                vec![],
                Some(QualifiedName::new("B")),
            )
            .unwrap();
        let base_root = base_tree
            .intern_branch(NodeKind::new("file"), lang(), vec![mod_a, mod_b], None)
            .unwrap();
        base_tree.set_root(base_root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&base_tree, foo), nid(1));
        ids.insert(site(&base_tree, mod_a), nid(2));
        ids.insert(site(&base_tree, mod_b), nid(3));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let foo_r = leaf(&mut result, "fn", "foo_body", Some("foo"));
        let mod_a_r = result
            .intern_branch(
                NodeKind::new("mod"),
                lang(),
                vec![],
                Some(QualifiedName::new("A")),
            )
            .unwrap();
        let mod_b_r = result
            .intern_branch(
                NodeKind::new("mod"),
                lang(),
                vec![foo_r],
                Some(QualifiedName::new("B")),
            )
            .unwrap();
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![mod_a_r, mod_b_r], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        assert_eq!(foo, foo_r);

        let mapping = default_identify(&adapter, &base, &result);
        assert_eq!(
            mapping.nodes.get(&site(&result, foo_r)).copied(),
            Some(nid(1))
        );
        assert_eq!(
            mapping.nodes.get(&site(&result, mod_a_r)).copied(),
            Some(nid(2))
        );
        assert_eq!(
            mapping.nodes.get(&site(&result, mod_b_r)).copied(),
            Some(nid(3))
        );
        assert!(mapping.deltas.is_empty());
        assert_eq!(
            mapping.moves,
            vec![Op::Move {
                node: nid(1),
                from_parent: nid(2),
                to_parent: nid(3),
                index: 0,
            }]
        );
    }

    #[test]
    fn unmatched_result_is_birth_unmatched_base_is_death() {
        let adapter = TestAdapter;
        let mut base_tree = NodeTree::new();
        let old = leaf(&mut base_tree, "fn", "old", Some("old"));
        let base_root = base_tree
            .intern_branch(NodeKind::new("file"), lang(), vec![old], None)
            .unwrap();
        base_tree.set_root(base_root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&base_tree, old), nid(1));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let new = leaf(&mut result, "fn", "new", Some("new"));
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![new], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        let mapping = default_identify(&adapter, &base, &result);
        assert_eq!(mapping.deltas.len(), 2);
        assert!(
            mapping
                .deltas
                .iter()
                .any(|d| matches!(d, IdentityDelta::Death { node } if *node == nid(1)))
        );
        assert!(
            mapping
                .deltas
                .iter()
                .any(|d| matches!(d, IdentityDelta::Birth { .. }))
        );
        let born = mapping.nodes.get(&site(&result, new)).copied().unwrap();
        assert_ne!(born, nid(1));
        assert!(mapping.moves.is_empty());
    }

    fn fn_with_leaves(tree: &mut NodeTree, name: &str, texts: &[&str]) -> ObjectId {
        let leaves: Vec<ObjectId> = texts
            .iter()
            .map(|text| leaf(tree, "id", text, None))
            .collect();
        tree.intern_branch(
            NodeKind::new("fn"),
            lang(),
            leaves,
            Some(QualifiedName::new(name)),
        )
        .unwrap()
    }

    #[test]
    fn rename_keeps_id_when_body_distance_is_within_threshold() {
        let adapter = TestAdapter;
        let mut base_tree = NodeTree::new();
        let old = fn_with_leaves(&mut base_tree, "foo", &["a", "b", "c", "d", "e"]);
        let base_root = base_tree
            .intern_branch(NodeKind::new("file"), lang(), vec![old], None)
            .unwrap();
        base_tree.set_root(base_root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&base_tree, old), nid(1));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let new = fn_with_leaves(&mut result, "bar", &["a", "b", "c", "d", "z"]);
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![new], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        let mapping = default_identify(&adapter, &base, &result);
        assert_eq!(
            mapping.nodes.get(&site(&result, new)).copied(),
            Some(nid(1))
        );
        assert!(mapping.deltas.iter().any(|delta| matches!(
            delta,
            IdentityDelta::DerivedFrom { node, from } if *node == nid(1) && *from == nid(1)
        )));
        assert_eq!(
            mapping.renames,
            vec![Op::Rename {
                node: nid(1),
                from: QualifiedName::new("foo"),
                to: QualifiedName::new("bar"),
            }]
        );
        assert!(mapping.moves.is_empty());
    }

    #[test]
    fn dissimilar_bodies_stay_a_birth_and_a_death() {
        let adapter = TestAdapter;
        let mut base_tree = NodeTree::new();
        let old = fn_with_leaves(&mut base_tree, "foo", &["a", "b", "c", "d", "e"]);
        let base_root = base_tree
            .intern_branch(NodeKind::new("file"), lang(), vec![old], None)
            .unwrap();
        base_tree.set_root(base_root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&base_tree, old), nid(1));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let new = fn_with_leaves(&mut result, "bar", &["a", "x", "y", "z", "e"]);
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![new], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        let mapping = default_identify(&adapter, &base, &result);
        assert_ne!(
            mapping.nodes.get(&site(&result, new)).copied(),
            Some(nid(1))
        );
        assert!(mapping.renames.is_empty());
        assert!(
            mapping
                .deltas
                .iter()
                .any(|delta| matches!(delta, IdentityDelta::Death { node } if *node == nid(1)))
        );
    }

    #[test]
    fn trait_identify_uses_default() {
        let adapter = TestAdapter;
        let mut tree = NodeTree::new();
        let foo = leaf(&mut tree, "fn", "x", Some("x"));
        let root = tree
            .intern_branch(NodeKind::new("file"), lang(), vec![foo], None)
            .unwrap();
        tree.set_root(root).unwrap();
        let mut ids = BTreeMap::new();
        ids.insert(site(&tree, foo), nid(9));
        let base = IdentifiedTree::new(tree.clone(), ids);
        let via_trait = adapter.identify(&base, &tree);
        let via_fn = default_identify(&adapter, &base, &tree);
        assert_eq!(via_trait.nodes, via_fn.nodes);
        assert_eq!(
            via_trait.nodes.get(&site(&tree, foo)).copied(),
            Some(nid(9))
        );
    }
}
