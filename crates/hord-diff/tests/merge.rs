//! Spec §5.2 merge rules: compose, identical Replace, Delete vs Replace.

mod common;

use hord_core::Op;
use hord_diff::{ConflictKind, apply, diff, merge, merge_ops};
use hord_lang::LangAdapter;

use common::{identify_result, parse_identified, rust};

#[test]
fn identical_normalized_replace_composes() {
    let adapter = rust();
    let base_src = b"fn a() { let x = 1; }\nfn b() {}\n";
    let ours_src = b"fn a() { let x = 2; }\nfn b() {}\n";
    // Same tokens, extra trivia: same normalized, different ObjectId.
    let theirs_src = b"fn a() { let x  =  2; }\nfn b() {}\n";

    let base = parse_identified(&adapter, base_src);
    let ours = {
        let (tree, mapping) = identify_result(&adapter, &base, ours_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };
    let theirs = {
        let (tree, mapping) = identify_result(&adapter, &base, theirs_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };

    let merged = merge(&adapter, &base, &ours, &theirs)
        .unwrap_or_else(|c| panic!("expected compose, got {c:?}"));
    let got = adapter.project(&merged.tree.tree);
    // Landing order picks ours.
    assert_eq!(got.as_slice(), ours_src);
}

#[test]
fn delete_vs_replace_is_hard_conflict() {
    let adapter = rust();
    let base_src = b"fn a() { let x = 1; }\nfn b() {}\n";
    let ours_src = b"fn b() {}\n";
    let theirs_src = b"fn a() { let x = 2; }\nfn b() {}\n";

    let base = parse_identified(&adapter, base_src);
    let ours = {
        let (tree, mapping) = identify_result(&adapter, &base, ours_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };
    let theirs = {
        let (tree, mapping) = identify_result(&adapter, &base, theirs_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };

    let err = merge(&adapter, &base, &ours, &theirs).expect_err("hard conflict");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert!(
        !err.nodes.is_empty(),
        "conflict should list NodeIds, got {err:?}"
    );
}

#[test]
fn disjoint_ops_compose() {
    let adapter = rust();
    let base_src = b"fn a() { let x = 1; }\nfn b() { let y = 1; }\n";
    let ours_src = b"fn a() { let x = 2; }\nfn b() { let y = 1; }\n";
    let theirs_src = b"fn a() { let x = 1; }\nfn b() { let y = 2; }\n";

    let base = parse_identified(&adapter, base_src);
    let ours = {
        let (tree, mapping) = identify_result(&adapter, &base, ours_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };
    let theirs = {
        let (tree, mapping) = identify_result(&adapter, &base, theirs_src);
        hord_lang::IdentifiedTree::new(tree, mapping.nodes)
    };

    let merged = merge(&adapter, &base, &ours, &theirs)
        .unwrap_or_else(|c| panic!("disjoint should compose: {c:?}"));
    let got = adapter.project(&merged.tree.tree);
    let want = b"fn a() { let x = 2; }\nfn b() { let y = 2; }\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
}

#[test]
fn same_index_inserts_are_soft_conflicts() {
    let adapter = rust();
    let base_src = b"fn a() {}\nfn b() {}\n";
    let ours_src = b"fn a() {}\nfn x() {}\nfn b() {}\n";
    let theirs_src = b"fn a() {}\nfn y() {}\nfn b() {}\n";

    let base = parse_identified(&adapter, base_src);
    let (ours_tree, ours_map) = identify_result(&adapter, &base, ours_src);
    let (theirs_tree, theirs_map) = identify_result(&adapter, &base, theirs_src);
    let ours_ops = diff(&base, &ours_tree, &ours_map);
    let theirs_ops = diff(&base, &theirs_tree, &theirs_map);
    let ours = hord_lang::IdentifiedTree::new(ours_tree, ours_map.nodes);
    let theirs = hord_lang::IdentifiedTree::new(theirs_tree, theirs_map.nodes);
    let merged = merge(&adapter, &base, &ours, &theirs)
        .unwrap_or_else(|c| panic!("soft conflict should still merge: {c:?}"));
    assert!(
        merged.soft.iter().any(|c| c.kind == ConflictKind::Soft),
        "expected a soft conflict, ops ours={ours_ops:?} theirs={theirs_ops:?} soft={:?}",
        merged.soft
    );
    let got = adapter.project(&merged.tree.tree);
    // Landing order: ours insert first, then theirs.
    let text = String::from_utf8_lossy(got.as_slice());
    let x = text.find("fn x").expect("ours insert present");
    let y = text.find("fn y").expect("theirs insert present");
    assert!(x < y, "landing order ours then theirs: {text}");
}

#[test]
fn merge_ops_delete_vs_replace() {
    let adapter = rust();
    let base_src = b"fn a() { let x = 1; }\nfn keep() {}\n";
    let base = parse_identified(&adapter, base_src);
    let (_, a_id) = common::def_named(&base, "fn a").expect("fn a");
    let ours = vec![Op::Delete { node: a_id }];
    let (theirs_tree, theirs_map) =
        identify_result(&adapter, &base, b"fn a() { let x = 9; }\nfn keep() {}\n");
    let theirs = diff(&base, &theirs_tree, &theirs_map);
    let err =
        merge_ops(&adapter, &base, &ours, &theirs, &theirs_tree).expect_err("delete vs replace");
    assert_eq!(err.kind, ConflictKind::Hard);
}

#[test]
fn apply_of_diff_used_by_merge_round_trip() {
    let adapter = rust();
    let base_src = b"fn a() {}\n";
    let ours_src = b"fn a() {}\nfn b() {}\n";
    let base = parse_identified(&adapter, base_src);
    let (ours_tree, ours_map) = identify_result(&adapter, &base, ours_src);
    let ops = diff(&base, &ours_tree, &ours_map);
    let applied = apply(&base, &ops, &ours_tree).unwrap();
    assert_eq!(adapter.project(&applied.tree).as_slice(), ours_src);
}
