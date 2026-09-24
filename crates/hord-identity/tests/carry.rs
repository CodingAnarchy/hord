//! Carrying rules from spec §3.4.
//!
//! ADR 0007 rename similarity is applied by `default_identify` and is not
//! retuned here. A different name and a different body are not a rename.

use std::collections::BTreeMap;
use std::str::FromStr;

use hord_core::{
    Bytes, IdentityDelta, LangId, NodeId, NodeKind, NodePath, ObjectId, Op, QualifiedName, RepoPath,
};
use hord_identity::{Declaration, Error, SnapshotFile, assign, carry, identity_map, is_fresh};
use hord_lang::{
    AttachedToken, IdentifiedTree, LangAdapter, NodeTree, ParseError, Site, Tier, default_identify,
};

struct TestAdapter;

impl LangAdapter for TestAdapter {
    fn lang(&self) -> LangId {
        LangId::new("test")
    }

    fn tier(&self) -> Tier {
        Tier::Syntax
    }

    fn matches(&self, path: &RepoPath, _head: &[u8]) -> bool {
        path.to_string().ends_with(".t")
    }

    fn parse(&self, _bytes: &[u8]) -> Result<NodeTree, ParseError> {
        Err(ParseError::failed("hord-identity tests do not parse"))
    }

    fn project(&self, tree: &NodeTree) -> Bytes {
        tree.to_bytes()
    }

    fn is_definition(&self, kind: &NodeKind) -> bool {
        matches!(kind.as_str(), "fn" | "mod")
    }
}

fn lang() -> LangId {
    LangId::new("test")
}

fn nid(n: u128) -> NodeId {
    NodeId::from_u128(n)
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

fn branch(
    tree: &mut NodeTree,
    kind: &str,
    children: Vec<ObjectId>,
    name: Option<&str>,
) -> ObjectId {
    tree.intern_branch(
        kind_of(kind),
        lang(),
        children,
        name.map(QualifiedName::new),
    )
    .unwrap()
}

fn kind_of(kind: &str) -> NodeKind {
    NodeKind::new(kind)
}

fn finish(tree: &mut NodeTree, children: Vec<ObjectId>) {
    let root = branch(tree, "file", children, None);
    tree.set_root(root).unwrap();
}

fn identified(tree: NodeTree, ids: &[(ObjectId, u128)]) -> IdentifiedTree {
    let mut map = BTreeMap::new();
    for (oid, id) in ids {
        map.insert(site(&tree, *oid), nid(*id));
    }
    IdentifiedTree::new(tree, map)
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

fn path() -> RepoPath {
    RepoPath::from_str("src/lib.t").unwrap()
}

fn fn_file(body: &str, name: &str) -> (NodeTree, ObjectId) {
    let mut tree = NodeTree::new();
    let fun = leaf(&mut tree, "fn", body, Some(name));
    finish(&mut tree, vec![fun]);
    (tree, fun)
}

#[test]
fn exact_carry_keeps_id() {
    let adapter = TestAdapter;
    let (tree, foo) = fn_file("foo_body", "foo");
    let base = identified(tree.clone(), &[(foo, 1)]);

    let carried = carry(&adapter, &path(), None, &base, &tree, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &tree);
    assert_eq!(carried, direct);
    assert_eq!(carried.nodes.get(&site(&tree, foo)).copied(), Some(nid(1)));
    assert!(carried.deltas.is_empty());
    assert!(carried.moves.is_empty());
}

#[test]
fn named_carry_keeps_id_when_body_changes() {
    let adapter = TestAdapter;
    let (base_tree, foo_old) = fn_file("old_body", "foo");
    let base = identified(base_tree, &[(foo_old, 1)]);
    let (result, foo_new) = fn_file("new_body", "foo");

    let carried = carry(&adapter, &path(), None, &base, &result, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &result);
    assert_eq!(carried, direct);
    assert_ne!(foo_old, foo_new);
    assert_eq!(
        carried.nodes.get(&site(&result, foo_new)).copied(),
        Some(nid(1))
    );
    assert!(carried.deltas.is_empty());
    assert!(carried.moves.is_empty());
}

#[test]
fn moved_carry_keeps_id_and_emits_move() {
    let adapter = TestAdapter;
    let (base_tree, foo, mod_a, mod_b) = moved_base();
    let base = identified(base_tree, &[(foo, 1), (mod_a, 2), (mod_b, 3)]);
    let (result, foo_r, mod_a_r, mod_b_r) = moved_result();
    assert_eq!(foo, foo_r);

    let carried = carry(&adapter, &path(), None, &base, &result, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &result);
    assert_eq!(carried, direct);
    assert_eq!(
        carried.nodes.get(&site(&result, foo_r)).copied(),
        Some(nid(1))
    );
    assert_eq!(
        carried.nodes.get(&site(&result, mod_a_r)).copied(),
        Some(nid(2))
    );
    assert_eq!(
        carried.nodes.get(&site(&result, mod_b_r)).copied(),
        Some(nid(3))
    );
    assert!(carried.deltas.is_empty());
    assert_eq!(
        carried.moves,
        vec![Op::Move {
            node: nid(1),
            from_parent: nid(2),
            to_parent: nid(3),
            index: 0,
        }]
    );

    let view = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(result, carried.nodes),
        }],
        carried.deltas,
    )
    .unwrap();
    assert!(view.deltas.is_empty());
    assert_eq!(
        view.nodes.get(&nid(2)),
        Some(&NodePath {
            file: path(),
            pointer: vec![0],
        })
    );
    assert_eq!(
        view.nodes.get(&nid(3)),
        Some(&NodePath {
            file: path(),
            pointer: vec![1],
        })
    );
    assert_eq!(
        view.nodes.get(&nid(1)),
        Some(&NodePath {
            file: path(),
            pointer: vec![1, 0],
        })
    );
}

#[test]
fn derived_from_overrides_birth() {
    let adapter = TestAdapter;
    let (base_tree, old) = fn_file("old_body", "old");
    let base = identified(base_tree, &[(old, 1)]);
    let (result, new) = fn_file("new_body", "new");

    let without = carry(&adapter, &path(), None, &base, &result, &[]).unwrap();
    let born = without.nodes.get(&site(&result, new)).copied().unwrap();
    assert_ne!(born, nid(1));
    assert!(
        without
            .deltas
            .iter()
            .any(|d| matches!(d, IdentityDelta::Birth { node } if *node == born))
    );
    assert!(
        without
            .deltas
            .iter()
            .any(|d| matches!(d, IdentityDelta::Death { node } if *node == nid(1)))
    );

    let with = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: new,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(with.nodes.get(&site(&result, new)).copied(), Some(nid(1)));
    assert_eq!(
        with.deltas,
        vec![IdentityDelta::DerivedFrom {
            node: nid(1),
            from: nid(1),
        }]
    );
    assert!(with.moves.is_empty());

    let view = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(result, with.nodes),
        }],
        with.deltas.clone(),
    )
    .unwrap();
    assert_eq!(view.deltas, with.deltas);
    assert_eq!(view.nodes.get(&nid(1)).unwrap().pointer, vec![0]);
}

#[test]
fn derived_from_across_parents_emits_move() {
    let adapter = TestAdapter;
    let (base_tree, foo, mod_a, mod_b) = moved_base();
    let base = identified(base_tree, &[(foo, 1), (mod_a, 2), (mod_b, 3)]);

    let mut result = NodeTree::new();
    let renamed = leaf(&mut result, "fn", "renamed_body", Some("bar"));
    let mod_a_r = branch(&mut result, "mod", vec![], Some("A"));
    let mod_b_r = branch(&mut result, "mod", vec![renamed], Some("B"));
    finish(&mut result, vec![mod_a_r, mod_b_r]);

    let carried = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: renamed,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(
        carried.nodes.get(&site(&result, renamed)).copied(),
        Some(nid(1))
    );
    assert_eq!(
        carried.nodes.get(&site(&result, mod_a_r)).copied(),
        Some(nid(2))
    );
    assert_eq!(
        carried.nodes.get(&site(&result, mod_b_r)).copied(),
        Some(nid(3))
    );
    assert_eq!(
        carried.deltas,
        vec![IdentityDelta::DerivedFrom {
            node: nid(1),
            from: nid(1),
        }]
    );
    assert_eq!(
        carried.moves,
        vec![Op::Move {
            node: nid(1),
            from_parent: nid(2),
            to_parent: nid(3),
            index: 0,
        }]
    );
}

#[test]
fn derived_from_copies_when_source_remains() {
    let adapter = TestAdapter;
    let mut base_tree = NodeTree::new();
    let foo = leaf(&mut base_tree, "fn", "foo_body", Some("foo"));
    finish(&mut base_tree, vec![foo]);
    let base = identified(base_tree, &[(foo, 1)]);

    let mut result = NodeTree::new();
    let foo_r = leaf(&mut result, "fn", "foo_body", Some("foo"));
    let bar = leaf(&mut result, "fn", "bar_body", Some("bar"));
    finish(&mut result, vec![foo_r, bar]);

    let carried = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: bar,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(
        carried.nodes.get(&site(&result, foo_r)).copied(),
        Some(nid(1))
    );
    let copy = carried.nodes.get(&site(&result, bar)).copied().unwrap();
    assert_ne!(copy, nid(1));
    assert_eq!(
        carried.deltas,
        vec![IdentityDelta::DerivedFrom {
            node: copy,
            from: nid(1),
        }]
    );
    assert!(carried.moves.is_empty());
}

#[test]
fn split_into_replaces_births_and_death() {
    let adapter = TestAdapter;
    let (base_tree, orig) = fn_file("orig_body", "orig");
    let base = identified(base_tree, &[(orig, 1)]);

    let mut result = NodeTree::new();
    let left = leaf(&mut result, "fn", "left_body", Some("left"));
    let right = leaf(&mut result, "fn", "right_body", Some("right"));
    finish(&mut result, vec![left, right]);

    let carried = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::SplitInto {
            node: nid(1),
            into: vec![left, right],
        }],
    )
    .unwrap();
    let left_id = carried.nodes.get(&site(&result, left)).copied().unwrap();
    let right_id = carried.nodes.get(&site(&result, right)).copied().unwrap();
    assert_ne!(left_id, nid(1));
    assert_ne!(right_id, nid(1));
    assert_ne!(left_id, right_id);
    assert_eq!(
        carried.deltas,
        vec![IdentityDelta::SplitInto {
            node: nid(1),
            into: vec![left_id, right_id],
        }]
    );
}

#[test]
fn merged_from_adopts_first_free_source() {
    let adapter = TestAdapter;
    let mut base_tree = NodeTree::new();
    let a = leaf(&mut base_tree, "fn", "a_body", Some("a"));
    let b = leaf(&mut base_tree, "fn", "b_body", Some("b"));
    finish(&mut base_tree, vec![a, b]);
    let base = identified(base_tree, &[(a, 1), (b, 2)]);
    let (result, merged) = fn_file("merged_body", "merged");

    let carried = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::MergedFrom {
            result: merged,
            from: vec![nid(1), nid(2)],
        }],
    )
    .unwrap();
    assert_eq!(
        carried.nodes.get(&site(&result, merged)).copied(),
        Some(nid(1))
    );
    assert_eq!(
        carried.deltas,
        vec![IdentityDelta::MergedFrom {
            node: nid(1),
            from: vec![nid(1), nid(2)],
        }]
    );
    assert!(carried.moves.is_empty());
}

#[test]
fn different_name_and_body_is_not_a_rename() {
    let adapter = TestAdapter;
    let (base_tree, old) = fn_file("alpha_body", "alpha");
    let base = identified(base_tree, &[(old, 1)]);
    let (result, new) = fn_file("beta_body", "beta");

    let carried = carry(&adapter, &path(), None, &base, &result, &[]).unwrap();
    assert_ne!(
        carried.nodes.get(&site(&result, new)).copied(),
        Some(nid(1))
    );
    assert!(carried.moves.is_empty());
    assert!(
        carried
            .deltas
            .iter()
            .any(|d| matches!(d, IdentityDelta::Birth { .. }))
    );
    assert!(
        carried
            .deltas
            .iter()
            .any(|d| matches!(d, IdentityDelta::Death { node } if *node == nid(1)))
    );
}

#[test]
fn assign_births_each_definition_and_locates_it() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let foo = leaf(&mut tree, "fn", "foo_body", Some("foo"));
    let bar = leaf(&mut tree, "fn", "bar_body", Some("bar"));
    finish(&mut tree, vec![foo, bar]);

    let mapping = assign(&adapter, &path(), None, &tree);
    assert_eq!(mapping.nodes.len(), 2);
    assert!(mapping.moves.is_empty());
    let foo_id = mapping.nodes.get(&site(&tree, foo)).copied().unwrap();
    let bar_id = mapping.nodes.get(&site(&tree, bar)).copied().unwrap();
    assert_ne!(foo_id, bar_id);
    assert_ne!(foo_id, NodeId::nil());
    assert_eq!(
        mapping.deltas,
        vec![
            IdentityDelta::Birth { node: foo_id },
            IdentityDelta::Birth { node: bar_id },
        ]
    );

    let view = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(tree, mapping.nodes),
        }],
        mapping.deltas.clone(),
    )
    .unwrap();
    assert_eq!(view.deltas, mapping.deltas);
    assert_eq!(view.nodes.get(&foo_id).unwrap().pointer, vec![0]);
    assert_eq!(view.nodes.get(&bar_id).unwrap().pointer, vec![1]);
}

#[test]
fn identity_map_root_definition_has_empty_pointer() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let foo = leaf(&mut tree, "fn", "foo_body", Some("foo"));
    let root = branch(&mut tree, "mod", vec![foo], Some("root"));
    tree.set_root(root).unwrap();

    let mapping = assign(&adapter, &path(), None, &tree);
    let root_id = mapping.nodes.get(&site(&tree, root)).copied().unwrap();
    let foo_id = mapping.nodes.get(&site(&tree, foo)).copied().unwrap();
    let view = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(tree, mapping.nodes),
        }],
        Vec::new(),
    )
    .unwrap();
    assert_eq!(view.nodes.get(&root_id).unwrap().pointer, Vec::<u32>::new());
    assert_eq!(view.nodes.get(&foo_id).unwrap().pointer, vec![0]);
}

#[test]
fn identity_map_rejects_duplicate_node_id() {
    let (tree, foo) = fn_file("foo_body", "foo");
    let identified = identified(tree, &[(foo, 1)]);
    let files = [
        SnapshotFile {
            path: path(),
            identified: identified.clone(),
        },
        SnapshotFile {
            path: RepoPath::from_str("src/other.t").unwrap(),
            identified,
        },
    ];
    let err = identity_map(&files, Vec::new()).unwrap_err();
    assert!(matches!(err, Error::DuplicateNode(id) if id == nid(1)));
}

#[test]
fn identity_map_rejects_ids_at_sites_not_in_the_tree() {
    // Ids are keyed by site: an id whose site does not exist in the tree
    // (a stale or foreign site) is unmapped.
    let (tree, _foo) = fn_file("foo_body", "foo");
    let mut ids = BTreeMap::new();
    ids.insert(vec![9], nid(4));
    let err = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(tree, ids),
        }],
        Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Unmapped(id) if id == nid(4)));
}

/// `assign` equals the stabilized `default_identify` against an empty base
/// (`carry` with no declarations), which it computes directly.
#[test]
fn assign_equals_carry_from_an_empty_base() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let dup = leaf(&mut tree, "fn", "same_body", Some("same"));
    let other = leaf(&mut tree, "fn", "other_body", Some("other"));
    let inner = branch(&mut tree, "mod", vec![dup, other], Some("inner"));
    let mod_a = branch(&mut tree, "mod", vec![inner, dup, other], Some("a"));
    let text = leaf(&mut tree, "text", "glue", None);
    finish(&mut tree, vec![mod_a, text, dup, mod_a]);
    let empty = IdentifiedTree::default();
    for snapshot in [None, Some(ObjectId::from_canonical(b"base"))] {
        let assigned = assign(&adapter, &path(), snapshot, &tree);
        let carried = carry(&adapter, &path(), snapshot, &empty, &tree, &[]).unwrap();
        assert_eq!(assigned, carried);
        assert_eq!(assigned.nodes.len(), 13);
    }
    let bare = NodeTree::new();
    assert_eq!(assign(&adapter, &path(), None, &bare), Default::default());
}

/// `is_fresh` answers exactly what comparing with `assign` does.
#[test]
fn is_fresh_agrees_with_comparing_against_assign() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let dup = leaf(&mut tree, "fn", "same_body", Some("same"));
    let other = leaf(&mut tree, "fn", "other_body", Some("other"));
    let mod_a = branch(&mut tree, "mod", vec![dup, other], Some("a"));
    let mod_b = branch(&mut tree, "mod", vec![dup], Some("b"));
    finish(&mut tree, vec![mod_a, mod_b]);
    let fresh = assign(&adapter, &path(), None, &tree).nodes;
    let agrees = |ids: BTreeMap<Site, NodeId>| {
        let identified = IdentifiedTree::new(tree.clone(), ids);
        let expected = fresh == identified.ids;
        assert_eq!(is_fresh(&adapter, &path(), &identified), expected);
        expected
    };
    assert!(agrees(fresh.clone()));
    let snapshot = ObjectId::from_canonical(b"base");
    assert!(!agrees(
        assign(&adapter, &path(), Some(snapshot), &tree).nodes
    ));
    let mut changed = fresh.clone();
    changed.insert(vec![1, 0], nid(7));
    assert!(!agrees(changed));
    let mut missing = fresh.clone();
    missing.remove(&vec![0, 1]);
    assert!(!agrees(missing));
    let mut extra = fresh.clone();
    extra.insert(vec![5], nid(8));
    assert!(!agrees(extra));
    assert!(!agrees(BTreeMap::new()));
    let empty = IdentifiedTree::default();
    assert!(is_fresh(&adapter, &path(), &empty));
}

/// Two definitions with the same text under different parents are one
/// interned node but two sites; each keeps its own id (spec §3.4).
#[test]
fn identical_definitions_at_two_sites_keep_distinct_ids() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let dup = leaf(&mut tree, "fn", "same_body", Some("same"));
    let mod_a = branch(&mut tree, "mod", vec![dup], Some("a"));
    let mod_b = branch(&mut tree, "mod", vec![dup], Some("b"));
    finish(&mut tree, vec![mod_a, mod_b]);
    let mapping = assign(&adapter, &path(), None, &tree);
    let first = mapping.nodes.get(&vec![0, 0]).copied().unwrap();
    let second = mapping.nodes.get(&vec![1, 0]).copied().unwrap();
    assert_ne!(first, second);
    // Deterministic, and carried unchanged with no spurious move.
    assert_eq!(assign(&adapter, &path(), None, &tree).nodes, mapping.nodes);
    let base = IdentifiedTree::new(tree.clone(), mapping.nodes.clone());
    let carried = carry(&adapter, &path(), None, &base, &tree, &[]).unwrap();
    assert_eq!(carried.nodes, mapping.nodes);
    assert!(carried.moves.is_empty(), "{:?}", carried.moves);
    assert!(carried.deltas.is_empty(), "{:?}", carried.deltas);
    let view = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: base,
        }],
        Vec::new(),
    )
    .unwrap();
    assert_eq!(view.nodes.get(&first).unwrap().pointer, vec![0, 0]);
    assert_eq!(view.nodes.get(&second).unwrap().pointer, vec![1, 0]);
}

#[test]
fn declaration_errors() {
    let adapter = TestAdapter;
    let (base_tree, old) = fn_file("old_body", "old");
    let base = identified(base_tree, &[(old, 1)]);
    let (result, new) = fn_file("new_body", "new");
    let missing = ObjectId::from_bytes([7; 32]);

    let unknown = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: missing,
            from: nid(1),
        }],
    )
    .unwrap_err();
    assert!(matches!(unknown, Error::UnknownResult(id) if id == missing));

    let conflict = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[
            Declaration::DerivedFrom {
                result: new,
                from: nid(1),
            },
            Declaration::SplitInto {
                node: nid(1),
                into: vec![new],
            },
        ],
    )
    .unwrap_err();
    assert!(matches!(conflict, Error::Conflict(id) if id == new));

    let empty = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[Declaration::SplitInto {
            node: nid(1),
            into: Vec::new(),
        }],
    )
    .unwrap_err();
    assert!(matches!(empty, Error::EmptySplit(id) if id == nid(1)));
}

#[test]
fn second_continuation_of_the_same_id_is_claimed() {
    let adapter = TestAdapter;
    let (base_tree, old) = fn_file("old_body", "old");
    let base = identified(base_tree, &[(old, 1)]);
    let mut result = NodeTree::new();
    let a = leaf(&mut result, "fn", "a_body", Some("a"));
    let b = leaf(&mut result, "fn", "b_body", Some("b"));
    finish(&mut result, vec![a, b]);

    let err = carry(
        &adapter,
        &path(),
        None,
        &base,
        &result,
        &[
            Declaration::DerivedFrom {
                result: a,
                from: nid(1),
            },
            Declaration::DerivedFrom {
                result: b,
                from: nid(1),
            },
        ],
    )
    .unwrap_err();
    assert!(matches!(err, Error::Claimed(id) if id == nid(1)));
}

/// ADR 0019: births are a function of content, file, site, and base
/// snapshot. A definition appended after `foo` does not move it, so `foo`
/// keeps its id; the same inputs give the same ids on every call.
#[test]
fn birth_ids_are_stable_and_ignore_nodes_after_them() {
    let adapter = TestAdapter;
    let (tree, foo) = fn_file("foo_body", "foo");
    let once = assign(&adapter, &path(), None, &tree);
    let twice = assign(&adapter, &path(), None, &tree);
    assert_eq!(once, twice);
    let foo_id = once.nodes.get(&site(&tree, foo)).copied().unwrap();

    let mut with_extra = NodeTree::new();
    let foo2 = leaf(&mut with_extra, "fn", "foo_body", Some("foo"));
    let extra = leaf(&mut with_extra, "fn", "zzz_body", Some("zzz"));
    finish(&mut with_extra, vec![foo2, extra]);
    let mapping = assign(&adapter, &path(), None, &with_extra);
    assert_eq!(
        mapping.nodes.get(&site(&with_extra, foo2)).copied(),
        Some(foo_id)
    );
    assert_ne!(
        mapping.nodes.get(&site(&with_extra, extra)).copied(),
        Some(foo_id)
    );
}

/// ADR 0019: the base snapshot is an input. A definition deleted on one
/// base and re-added on a later one is a new lifetime, and a fresh
/// assignment (no snapshot) differs from both.
#[test]
fn birth_ids_depend_on_the_base_snapshot() {
    let adapter = TestAdapter;
    let (tree, foo) = fn_file("foo_body", "foo");
    let path = RepoPath::from_str("src/a.t").unwrap();
    let first = ObjectId::from_bytes([1; 32]);
    let later = ObjectId::from_bytes([2; 32]);
    let empty = IdentifiedTree::default();
    let born = |snapshot| {
        let mapping = carry(&adapter, &path, snapshot, &empty, &tree, &[]).unwrap();
        mapping.nodes.get(&site(&tree, foo)).copied().unwrap()
    };
    assert_eq!(born(Some(first)), born(Some(first)));
    assert_ne!(born(Some(first)), born(Some(later)));
    assert_ne!(born(Some(first)), born(None));
    let fresh = assign(&adapter, &path, None, &tree);
    assert_eq!(
        fresh.nodes.get(&site(&tree, foo)).copied(),
        Some(born(None))
    );
    // Derived as the ADR says: content, file root, site, snapshot.
    let expected = hord_identity::birth_id(
        foo,
        NodeId::file_root(&path),
        &site(&tree, foo),
        Some(first),
        0,
    );
    assert_eq!(born(Some(first)), expected);
}

/// Two identical definitions at two sites of one file get distinct ids.
#[test]
fn identical_births_at_two_sites_differ() {
    let adapter = TestAdapter;
    let mut tree = NodeTree::new();
    let a = leaf(&mut tree, "fn", "same_body", Some("same"));
    finish(&mut tree, vec![a, a]);
    let mapping = assign(&adapter, &path(), None, &tree);
    let ids: Vec<_> = mapping.nodes.values().copied().collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn copy_id_is_stable_and_ignores_unrelated_nodes() {
    let adapter = TestAdapter;
    let mut base_tree = NodeTree::new();
    let foo = leaf(&mut base_tree, "fn", "foo_body", Some("foo"));
    let other = leaf(&mut base_tree, "fn", "other_body", Some("other"));
    finish(&mut base_tree, vec![foo, other]);
    let base = identified(base_tree, &[(foo, 1), (other, 2)]);

    let copy_of = |extra: bool| {
        let mut result = NodeTree::new();
        let foo_r = leaf(&mut result, "fn", "foo_body", Some("foo"));
        let other_r = leaf(&mut result, "fn", "other_body", Some("other"));
        if extra {
            let extra = leaf(&mut result, "fn", "zzz_body", Some("zzz"));
            finish(&mut result, vec![foo_r, extra, other_r]);
        } else {
            finish(&mut result, vec![foo_r, other_r]);
        }
        let carried = carry(
            &adapter,
            &path(),
            None,
            &base,
            &result,
            &[Declaration::DerivedFrom {
                result: other_r,
                from: nid(1),
            }],
        )
        .unwrap();
        carried.nodes.get(&site(&result, other_r)).copied().unwrap()
    };

    let first = copy_of(false);
    assert_eq!(first, copy_of(false));
    assert_ne!(first, nid(1));
    assert_ne!(first, nid(2));
    assert_eq!(first, copy_of(true));
}

fn moved_base() -> (NodeTree, ObjectId, ObjectId, ObjectId) {
    let mut tree = NodeTree::new();
    let fun = leaf(&mut tree, "fn", "foo_body", Some("foo"));
    let mod_a = branch(&mut tree, "mod", vec![fun], Some("A"));
    let mod_b = branch(&mut tree, "mod", vec![], Some("B"));
    finish(&mut tree, vec![mod_a, mod_b]);
    (tree, fun, mod_a, mod_b)
}

fn moved_result() -> (NodeTree, ObjectId, ObjectId, ObjectId) {
    let mut tree = NodeTree::new();
    let fun = leaf(&mut tree, "fn", "foo_body", Some("foo"));
    let mod_a = branch(&mut tree, "mod", vec![], Some("A"));
    let mod_b = branch(&mut tree, "mod", vec![fun], Some("B"));
    finish(&mut tree, vec![mod_a, mod_b]);
    (tree, fun, mod_a, mod_b)
}

/// Identical files at two paths: `assign` gives both the same (content-
/// derived) ids; `assign` keeps them apart per path, deterministically.
/// `carry` births follow the same rule.
#[test]
fn assign_keeps_identical_files_apart() {
    let adapter = TestAdapter;
    let (tree, _) = fn_file("same_body", "same");
    let a = RepoPath::from_str("src/a.t").unwrap();
    let b = RepoPath::from_str("src/b.t").unwrap();
    assert_eq!(
        assign(&adapter, &path(), None, &tree).nodes,
        assign(&adapter, &path(), None, &tree).nodes
    );
    let in_a = assign(&adapter, &a, None, &tree);
    let in_b = assign(&adapter, &b, None, &tree);
    assert_eq!(in_a.nodes, assign(&adapter, &a, None, &tree).nodes);
    let ids = |m: &hord_lang::IdentityMapping| m.nodes.values().copied().collect::<Vec<_>>();
    assert!(ids(&in_a).iter().all(|id| !ids(&in_b).contains(id)));

    let empty = IdentifiedTree::default();
    let born_a = carry(&adapter, &a, None, &empty, &tree, &[]).unwrap();
    let born_b = carry(&adapter, &b, None, &empty, &tree, &[]).unwrap();
    assert_eq!(born_a.nodes, in_a.nodes);
    assert_ne!(born_a.nodes, born_b.nodes);
}
