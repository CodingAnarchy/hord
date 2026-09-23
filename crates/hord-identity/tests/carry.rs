//! Carrying rules from spec §3.4 steps 1–3, 5, and 6.
//!
//! Step 4 (rename similarity) is open and must not match.

use std::collections::BTreeMap;
use std::str::FromStr;

use hord_core::{
    Bytes, IdentityDelta, LangId, NodeId, NodeKind, NodePath, ObjectId, Op, QualifiedName, RepoPath,
};
use hord_identity::{Declaration, Error, SnapshotFile, assign, carry, identity_map};
use hord_lang::{
    AttachedToken, IdentifiedTree, LangAdapter, NodeTree, ParseError, Tier, default_identify,
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
        map.insert(*oid, nid(*id));
    }
    IdentifiedTree::new(tree, map)
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

    let carried = carry(&adapter, &base, &tree, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &tree);
    assert_eq!(carried, direct);
    assert_eq!(carried.nodes.get(&foo).copied(), Some(nid(1)));
    assert!(carried.deltas.is_empty());
    assert!(carried.moves.is_empty());
}

#[test]
fn named_carry_keeps_id_when_body_changes() {
    let adapter = TestAdapter;
    let (base_tree, foo_old) = fn_file("old_body", "foo");
    let base = identified(base_tree, &[(foo_old, 1)]);
    let (result, foo_new) = fn_file("new_body", "foo");

    let carried = carry(&adapter, &base, &result, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &result);
    assert_eq!(carried, direct);
    assert_ne!(foo_old, foo_new);
    assert_eq!(carried.nodes.get(&foo_new).copied(), Some(nid(1)));
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

    let carried = carry(&adapter, &base, &result, &[]).unwrap();
    let direct = default_identify(&adapter, &base, &result);
    assert_eq!(carried, direct);
    assert_eq!(carried.nodes.get(&foo_r).copied(), Some(nid(1)));
    assert_eq!(carried.nodes.get(&mod_a_r).copied(), Some(nid(2)));
    assert_eq!(carried.nodes.get(&mod_b_r).copied(), Some(nid(3)));
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

    let without = carry(&adapter, &base, &result, &[]).unwrap();
    let born = without.nodes.get(&new).copied().unwrap();
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
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: new,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(with.nodes.get(&new).copied(), Some(nid(1)));
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
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: renamed,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(carried.nodes.get(&renamed).copied(), Some(nid(1)));
    assert_eq!(carried.nodes.get(&mod_a_r).copied(), Some(nid(2)));
    assert_eq!(carried.nodes.get(&mod_b_r).copied(), Some(nid(3)));
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
        &base,
        &result,
        &[Declaration::DerivedFrom {
            result: bar,
            from: nid(1),
        }],
    )
    .unwrap();
    assert_eq!(carried.nodes.get(&foo_r).copied(), Some(nid(1)));
    let copy = carried.nodes.get(&bar).copied().unwrap();
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
        &base,
        &result,
        &[Declaration::SplitInto {
            node: nid(1),
            into: vec![left, right],
        }],
    )
    .unwrap();
    let left_id = carried.nodes.get(&left).copied().unwrap();
    let right_id = carried.nodes.get(&right).copied().unwrap();
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
        &base,
        &result,
        &[Declaration::MergedFrom {
            result: merged,
            from: vec![nid(1), nid(2)],
        }],
    )
    .unwrap();
    assert_eq!(carried.nodes.get(&merged).copied(), Some(nid(1)));
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

    let carried = carry(&adapter, &base, &result, &[]).unwrap();
    assert_ne!(carried.nodes.get(&new).copied(), Some(nid(1)));
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

    let mapping = assign(&adapter, &tree);
    assert_eq!(mapping.nodes.len(), 2);
    assert!(mapping.moves.is_empty());
    let foo_id = mapping.nodes.get(&foo).copied().unwrap();
    let bar_id = mapping.nodes.get(&bar).copied().unwrap();
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

    let mapping = assign(&adapter, &tree);
    let root_id = mapping.nodes.get(&root).copied().unwrap();
    let foo_id = mapping.nodes.get(&foo).copied().unwrap();
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
fn identity_map_rejects_missing_and_unreachable_ids() {
    let (tree, _foo) = fn_file("foo_body", "foo");
    let mut missing = BTreeMap::new();
    missing.insert(ObjectId::from_bytes([9; 32]), nid(4));
    let missing_err = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(tree, missing),
        }],
        Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(missing_err, Error::MissingNode(_)));

    let mut tree = NodeTree::new();
    let foo = leaf(&mut tree, "fn", "foo_body", Some("foo"));
    let orphan = leaf(&mut tree, "fn", "orphan_body", Some("orphan"));
    finish(&mut tree, vec![foo]);
    let mut ids = BTreeMap::new();
    ids.insert(orphan, nid(5));
    let unmapped = identity_map(
        &[SnapshotFile {
            path: path(),
            identified: IdentifiedTree::new(tree, ids),
        }],
        Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(unmapped, Error::Unmapped(id) if id == nid(5)));
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
