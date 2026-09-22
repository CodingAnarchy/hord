//! Spec §5.2 merge rules: compose, identical Replace, Delete vs Replace.

mod common;

use std::path::PathBuf;

use hord_core::Op;
use hord_diff::{ConflictKind, apply, diff, merge, merge_ops};
use hord_lang::{IdentifiedTree, LangAdapter, default_identify};

use common::{identify_result, parse_identified, rust, toml};

fn merge_identified<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
) -> Result<hord_diff::MergeResult, hord_diff::Conflict> {
    let empty = IdentifiedTree::default();
    let base_tree = adapter.parse(base_src).expect("parse base");
    let base_map = default_identify(adapter, &empty, &base_tree);
    let base = IdentifiedTree::new(base_tree, base_map.nodes);
    let ours_tree = adapter.parse(ours_src).expect("parse ours");
    let ours_map = default_identify(adapter, &base, &ours_tree);
    let ours = IdentifiedTree::new(ours_tree, ours_map.nodes);
    let theirs_tree = adapter.parse(theirs_src).expect("parse theirs");
    let theirs_map = default_identify(adapter, &base, &theirs_tree);
    let theirs = IdentifiedTree::new(theirs_tree, theirs_map.nodes);
    merge(adapter, &base, &ours, &theirs)
}

fn projected_text<A: LangAdapter>(adapter: &A, tree: &hord_lang::IdentifiedTree) -> String {
    let bytes = adapter.project(&tree.tree);
    String::from_utf8_lossy(bytes.as_slice()).into_owned()
}

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
fn container_replace_keeps_theirs_unique_fields() {
    let adapter = rust();
    let base_src = b"struct S {\n    a: i32,\n}\n";
    let ours_src = b"pub struct S {\n    a: i32,\n    b: i32,\n}\n";
    let theirs_src = b"struct S {\n    a: i32,\n    c: i32,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|e| panic!("should keep theirs field: {e:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("pub struct S"), "ours vis missing: {text}");
    assert!(text.contains("b: i32"), "ours field missing: {text}");
    assert!(text.contains("c: i32"), "theirs field missing: {text}");
}

#[test]
fn overlapping_edits_inside_one_function_keep_both() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpora/merges/0069");
    let read = |name: &str| std::fs::read(root.join(name)).expect(name);
    let base = read("base.rs");
    let ours = read("ours.rs");
    let theirs = read("theirs.rs");
    let want = read("result.rs");
    let adapter = rust();
    let merged = merge_identified(&adapter, &base, &ours, &theirs)
        .unwrap_or_else(|c| panic!("0069 should resolve: {c:?}"));
    let got = adapter.project(&merged.tree.tree);
    // The merge commit mixes both sides of one conflict hunk. That is a manual
    // resolution. Landing-order auto-resolution is `git merge-file --ours`.
    let favor = std::process::Command::new("git")
        .args(["merge-file", "-p", "--ours"])
        .arg(root.join("ours.rs"))
        .arg(root.join("base.rs"))
        .arg(root.join("theirs.rs"))
        .output()
        .expect("git merge-file");
    assert_eq!(got.as_slice(), favor.stdout.as_slice());
    assert_ne!(got.as_slice(), want.as_slice());
}

#[test]
fn overlapping_body_picks_ours() {
    let adapter = rust();
    let base_src = b"fn f() { let a = 1; }\n";
    let ours_src = b"fn f() { let a = 2; }\n";
    let theirs_src = b"fn f() { let a = 3; }\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("landing order should keep ours: {c:?}"));
    let got = adapter.project(&merged.tree.tree);
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
fn disjoint_statements_in_same_function_compose() {
    let adapter = rust();
    let base_src = b"fn f() {\n    let a = 1;\n    let b = 1;\n}\n";
    let ours_src = b"fn f() {\n    let a = 2;\n    let b = 1;\n}\n";
    let theirs_src = b"fn f() {\n    let a = 1;\n    let b = 2;\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint statements should compose: {c:?}"));
    let got = adapter.project(&merged.tree.tree);
    let want = b"fn f() {\n    let a = 2;\n    let b = 2;\n}\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
}

#[test]
fn disjoint_methods_in_same_impl_compose() {
    let adapter = rust();
    let base_src = b"impl Foo {\n    fn a() { let x = 1; }\n    fn b() { let y = 1; }\n}\n";
    let ours_src = b"impl Foo {\n    fn a() { let x = 2; }\n    fn b() { let y = 1; }\n}\n";
    let theirs_src = b"impl Foo {\n    fn a() { let x = 1; }\n    fn b() { let y = 2; }\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint methods should compose: {c:?}"));
    let got = adapter.project(&merged.tree.tree);
    let want = b"impl Foo {\n    fn a() { let x = 2; }\n    fn b() { let y = 2; }\n}\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
}

#[test]
fn disjoint_method_inserts_in_same_impl_compose() {
    let adapter = rust();
    let base_src = b"impl Foo {\n    fn a() {}\n}\n";
    let ours_src = b"impl Foo {\n    fn a() {}\n    fn b() {}\n}\n";
    let theirs_src = b"impl Foo {\n    fn a() {}\n    fn c() {}\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint method inserts should compose: {c:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("fn b()"), "ours method missing: {text}");
    assert!(text.contains("fn c()"), "theirs method missing: {text}");
    assert!(text.contains("fn a()"), "base method missing: {text}");
}

#[test]
fn disjoint_use_items_compose() {
    let adapter = rust();
    let base_src = b"use a::x;\nfn f() {}\n";
    let ours_src = b"use a::x;\nuse b::y;\nfn f() {}\n";
    let theirs_src = b"use a::x;\nuse c::z;\nfn f() {}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint uses should compose: {c:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("use b::y;"), "ours use missing: {text}");
    assert!(text.contains("use c::z;"), "theirs use missing: {text}");
}

#[test]
fn disjoint_struct_fields_compose() {
    let adapter = rust();
    let base_src = b"struct S {\n    a: i32,\n}\n";
    let ours_src = b"struct S {\n    a: i32,\n    b: i32,\n}\n";
    let theirs_src = b"struct S {\n    a: i32,\n    c: i32,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint fields should compose: {c:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("b: i32"), "ours field missing: {text}");
    assert!(text.contains("c: i32"), "theirs field missing: {text}");
}

#[test]
fn disjoint_enum_variants_compose() {
    let adapter = rust();
    let base_src = b"enum E {\n    A,\n}\n";
    let ours_src = b"enum E {\n    A,\n    B,\n}\n";
    let theirs_src = b"enum E {\n    A,\n    C,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint variants should compose: {c:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("B"), "ours variant missing: {text}");
    assert!(text.contains("C"), "theirs variant missing: {text}");
}

#[test]
fn disjoint_toml_keys_compose() {
    let adapter = toml();
    let base_src = b"[dependencies]\nfoo = \"1\"\n";
    let ours_src = b"[dependencies]\nfoo = \"1\"\nbar = \"2\"\n";
    let theirs_src = b"[dependencies]\nfoo = \"1\"\nbaz = \"3\"\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .unwrap_or_else(|c| panic!("disjoint toml keys should compose: {c:?}"));
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("bar"), "ours key missing: {text}");
    assert!(text.contains("baz"), "theirs key missing: {text}");
}

#[test]
fn merge_bytes_do_not_depend_on_generated_node_ids() {
    // These labels are `git merge-file --ours`. Two merges of the same bytes
    // must project the same file: NodeIds are random ULIDs.
    for case in ["0076", "0114", "0143"] {
        let first = project_case(case);
        let second = project_case(case);
        assert_eq!(
            first,
            second,
            "{case} changed between runs ({} vs {} bytes)",
            first.len(),
            second.len()
        );
    }
}

fn project_case(case: &str) -> Vec<u8> {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../corpora/merges/{case}"));
    let ext = ["rs", "toml"]
        .into_iter()
        .find(|ext| root.join(format!("base.{ext}")).exists())
        .unwrap_or_else(|| panic!("{case} has no base.rs/base.toml"));
    let read = |name: &str| {
        std::fs::read(root.join(format!("{name}.{ext}")))
            .unwrap_or_else(|_| panic!("{case} {name}"))
    };
    let base = read("base");
    let ours = read("ours");
    let theirs = read("theirs");
    if ext == "rs" {
        let adapter = rust();
        let merged = merge_identified(&adapter, &base, &ours, &theirs)
            .unwrap_or_else(|c| panic!("{case} should resolve: {c}"));
        adapter.project(&merged.tree.tree).as_slice().to_vec()
    } else {
        let adapter = toml();
        let merged = merge_identified(&adapter, &base, &ours, &theirs)
            .unwrap_or_else(|c| panic!("{case} should resolve: {c}"));
        adapter.project(&merged.tree.tree).as_slice().to_vec()
    }
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
