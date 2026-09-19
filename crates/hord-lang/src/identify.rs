//! Default identity carrying (spec §3.4 steps 1–3).

use std::collections::BTreeMap;

use hord_core::{IdentityDelta, Node, NodeId, NodeKind, ObjectId, Op, QualifiedName};

use crate::adapter::LangAdapter;
use crate::tree::NodeTree;

/// A [`NodeTree`] plus durable [`NodeId`]s for definition nodes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentifiedTree {
    /// Interned CST.
    pub tree: NodeTree,
    /// Definition content id → stable identity.
    pub ids: BTreeMap<ObjectId, NodeId>,
}

impl IdentifiedTree {
    /// Wrap an interned tree and its definition ids.
    #[must_use]
    pub fn new(tree: NodeTree, ids: BTreeMap<ObjectId, NodeId>) -> Self {
        Self { tree, ids }
    }
}

/// Result of [`default_identify`]: carried ids, births/deaths, and moves.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentityMapping {
    /// Result definition [`ObjectId`] → carried or newly assigned [`NodeId`].
    pub nodes: BTreeMap<ObjectId, NodeId>,
    /// [`IdentityDelta::Birth`] for unmatched result defs and
    /// [`IdentityDelta::Death`] for unmatched base defs.
    pub deltas: Vec<IdentityDelta>,
    /// [`Op::Move`] from carrying rule 3 (same `normalized`, different parent).
    ///
    /// Emitted only when both the old and new nearest enclosing definitions
    /// have [`NodeId`]s. `index` is the node's position in its immediate CST
    /// parent's `children` list.
    pub moves: Vec<Op>,
}

#[derive(Clone, Debug)]
struct BaseDef {
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
    parent_def_oid: Option<ObjectId>,
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
///
/// Step 4 (rename by body similarity) is skipped: it is OPEN until M2.
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
    let result_defs = match result.root() {
        Some(root) => {
            let mut out = Vec::new();
            let mut ancestors = Vec::new();
            collect_result_defs(adapter, result, root, &mut ancestors, None, 0, &mut out);
            out
        }
        None => Vec::new(),
    };

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

    for r in &result_defs {
        if mapping.nodes.contains_key(&r.object_id) {
            continue;
        }
        let id = NodeId::generate();
        mapping.nodes.insert(r.object_id, id);
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
        let Some(&nid) = mapping.nodes.get(&r.object_id) else {
            continue;
        };
        let Some(b) = base_defs.iter().find(|d| d.used && d.node_id == nid) else {
            continue;
        };
        let to_parent = r
            .parent_def_oid
            .and_then(|oid| mapping.nodes.get(&oid).copied());
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
    result_ids: &mut BTreeMap<ObjectId, NodeId>,
    base_defs: &mut [BaseDef],
    result_defs: &[ResultDef],
    kind: MatchKind,
) -> bool {
    let mut progress = false;
    for r in result_defs {
        if result_ids.contains_key(&r.object_id) {
            continue;
        }
        let parent_nid = r
            .parent_def_oid
            .and_then(|oid| result_ids.get(&oid).copied());
        // Wait until the nearest enclosing definition is identified so
        // exact/named/moved see a stable parent [`NodeId`].
        if r.parent_def_oid.is_some() && parent_nid.is_none() {
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
            result_ids.insert(r.object_id, b.node_id);
            progress = true;
        }
    }
    progress
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

fn collect_base_defs<A: LangAdapter + ?Sized>(adapter: &A, base: &IdentifiedTree) -> Vec<BaseDef> {
    let Some(root) = base.tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ancestors = Vec::new();
    collect_base_walk(adapter, base, root, &mut ancestors, None, &mut out);
    out
}

fn collect_base_walk<A: LangAdapter + ?Sized>(
    adapter: &A,
    base: &IdentifiedTree,
    oid: ObjectId,
    ancestor_ids: &mut Vec<ObjectId>,
    nearest_def_id: Option<NodeId>,
    out: &mut Vec<BaseDef>,
) {
    let Some(node) = base.tree.get(oid) else {
        return;
    };
    let mut child_parent = nearest_def_id;
    if adapter.is_definition(&node.kind)
        && let Some(&node_id) = base.ids.get(&oid)
    {
        let name = def_name(adapter, &base.tree, ancestor_ids, node);
        out.push(BaseDef {
            node_id,
            parent_id: nearest_def_id,
            kind: node.kind,
            normalized: node.normalized,
            name,
            used: false,
        });
        child_parent = Some(node_id);
    }
    ancestor_ids.push(oid);
    for child in &node.children {
        collect_base_walk(adapter, base, *child, ancestor_ids, child_parent, out);
    }
    ancestor_ids.pop();
}

fn collect_result_defs<A: LangAdapter + ?Sized>(
    adapter: &A,
    tree: &NodeTree,
    oid: ObjectId,
    ancestor_ids: &mut Vec<ObjectId>,
    nearest_def: Option<ObjectId>,
    index: u32,
    out: &mut Vec<ResultDef>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let mut child_parent = nearest_def;
    if adapter.is_definition(&node.kind) {
        let name = def_name(adapter, tree, ancestor_ids, node);
        out.push(ResultDef {
            object_id: oid,
            parent_def_oid: nearest_def,
            index,
            kind: node.kind,
            normalized: node.normalized,
            name,
        });
        child_parent = Some(oid);
    }
    ancestor_ids.push(oid);
    for (i, child) in node.children.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        collect_result_defs(adapter, tree, *child, ancestor_ids, child_parent, idx, out);
    }
    ancestor_ids.pop();
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
        ids.insert(foo, nid(1));
        let base = IdentifiedTree::new(tree.clone(), ids);
        let mapping = default_identify(&adapter, &base, &tree);

        assert_eq!(mapping.nodes.get(&foo).copied(), Some(nid(1)));
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
        ids.insert(foo_old, nid(1));
        let base = IdentifiedTree::new(base_tree, ids);

        let mut result = NodeTree::new();
        let foo_new = leaf(&mut result, "fn", "new_body", Some("foo"));
        let result_root = result
            .intern_branch(NodeKind::new("file"), lang(), vec![foo_new], None)
            .unwrap();
        result.set_root(result_root).unwrap();

        let mapping = default_identify(&adapter, &base, &result);
        assert_ne!(foo_old, foo_new);
        assert_eq!(mapping.nodes.get(&foo_new).copied(), Some(nid(1)));
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
        ids.insert(foo, nid(1));
        ids.insert(mod_a, nid(2));
        ids.insert(mod_b, nid(3));
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
        assert_eq!(mapping.nodes.get(&foo_r).copied(), Some(nid(1)));
        assert_eq!(mapping.nodes.get(&mod_a_r).copied(), Some(nid(2)));
        assert_eq!(mapping.nodes.get(&mod_b_r).copied(), Some(nid(3)));
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
        ids.insert(old, nid(1));
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
        let born = mapping.nodes.get(&new).copied().unwrap();
        assert_ne!(born, nid(1));
        assert!(mapping.moves.is_empty());
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
        ids.insert(foo, nid(9));
        let base = IdentifiedTree::new(tree.clone(), ids);
        let via_trait = adapter.identify(&base, &tree);
        let via_fn = default_identify(&adapter, &base, &tree);
        assert_eq!(via_trait.nodes, via_fn.nodes);
        assert_eq!(via_trait.nodes.get(&foo).copied(), Some(nid(9)));
    }
}
