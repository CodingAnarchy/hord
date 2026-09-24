//! Spec §5.2 merge rules: compose, identical Replace, Delete vs Replace.

mod common;

use std::path::PathBuf;

use hord_diff::{ConflictKind, MergeMode, apply, diff, merge};
use hord_lang::{IdentifiedTree, LangAdapter, default_identify};

use common::{identify_result, parse_identified, rust, toml};

fn merge_identified<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
) -> Result<hord_diff::MergeResult, hord_diff::Conflict> {
    merge_in(adapter, base_src, ours_src, theirs_src, MergeMode::Corpus)
}

fn merge_in<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
    mode: MergeMode,
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
    merge(adapter, &common::file(), &base, &ours, &theirs, mode)
}

fn projected_text<A: LangAdapter>(adapter: &A, tree: &hord_lang::IdentifiedTree) -> String {
    let bytes = adapter.project(&tree.tree);
    String::from_utf8_lossy(bytes.as_slice()).into_owned()
}

#[test]
fn identical_normalized_replace_composes() -> Result<(), Box<dyn std::error::Error>> {
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

    let merged = merge(
        &adapter,
        &common::file(),
        &base,
        &ours,
        &theirs,
        MergeMode::Corpus,
    )
    .map_err(|c| format!("expected compose, got {c:?}"))?;
    let got = adapter.project(&merged.tree.tree);
    // Landing order picks ours.
    assert_eq!(got.as_slice(), ours_src);
    Ok(())
}

#[test]
fn same_field_with_different_attributes_is_not_duplicated() -> Result<(), Box<dyn std::error::Error>>
{
    let adapter = rust();
    let base_src = b"struct S {\n    a: i32,\n}\n";
    let ours_src = b"struct S {\n    a: i32,\n    #[cfg(a)]\n    b: i32,\n}\n";
    let theirs_src = b"struct S {\n    a: i32,\n    #[cfg(b)]\n    b: i32,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|e| format!("attribute-only field insert should resolve: {e:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert_eq!(
        text.matches("b: i32").count(),
        1,
        "field inserted twice: {text}"
    );
    assert!(
        text.contains("#[cfg(a)]"),
        "landing order should keep ours' attribute: {text}"
    );
    assert!(
        !text.contains("#[cfg(b)]"),
        "theirs' attribute should not also be inserted: {text}"
    );
    Ok(())
}

#[test]
fn container_replace_keeps_theirs_unique_fields() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"struct S {\n    a: i32,\n}\n";
    let ours_src = b"pub struct S {\n    a: i32,\n    b: i32,\n}\n";
    let theirs_src = b"struct S {\n    a: i32,\n    c: i32,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|e| format!("should keep theirs field: {e:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("pub struct S"), "ours vis missing: {text}");
    assert!(text.contains("b: i32"), "ours field missing: {text}");
    assert!(text.contains("c: i32"), "theirs field missing: {text}");
    Ok(())
}

#[test]
fn overlapping_edits_inside_one_function_keep_both() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpora/merges/0069");
    let read = |name: &str| std::fs::read(root.join(name)).expect(name);
    let base = read("base.rs");
    let ours = read("ours.rs");
    let theirs = read("theirs.rs");
    let want = read("result.rs");
    let adapter = rust();
    let merged = merge_identified(&adapter, &base, &ours, &theirs)
        .map_err(|c| format!("0069 should resolve: {c:?}"))?;
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
    Ok(())
}

#[test]
fn overlapping_body_picks_ours() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"fn f() { let a = 1; }\n";
    let ours_src = b"fn f() { let a = 2; }\n";
    let theirs_src = b"fn f() { let a = 3; }\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("landing order should keep ours: {c:?}"))?;
    let got = adapter.project(&merged.tree.tree);
    assert_eq!(got.as_slice(), ours_src);
    Ok(())
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

    let err = merge(
        &adapter,
        &common::file(),
        &base,
        &ours,
        &theirs,
        MergeMode::Corpus,
    )
    .expect_err("hard conflict");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert!(
        !err.nodes.is_empty(),
        "conflict should list NodeIds, got {err:?}"
    );
}

#[test]
fn disjoint_ops_compose() -> Result<(), Box<dyn std::error::Error>> {
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

    let merged = merge(
        &adapter,
        &common::file(),
        &base,
        &ours,
        &theirs,
        MergeMode::Corpus,
    )
    .map_err(|c| format!("disjoint should compose: {c:?}"))?;
    let got = adapter.project(&merged.tree.tree);
    let want = b"fn a() { let x = 2; }\nfn b() { let y = 2; }\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
    Ok(())
}

#[test]
fn same_index_inserts_are_soft_conflicts() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"fn a() {}\nfn b() {}\n";
    let ours_src = b"fn a() {}\nfn x() {}\nfn b() {}\n";
    let theirs_src = b"fn a() {}\nfn y() {}\nfn b() {}\n";

    let base = parse_identified(&adapter, base_src);
    let (ours_tree, ours_map) = identify_result(&adapter, &base, ours_src);
    let (theirs_tree, theirs_map) = identify_result(&adapter, &base, theirs_src);
    let ours_ops = diff(&common::file(), &base, &ours_tree, &ours_map);
    let theirs_ops = diff(&common::file(), &base, &theirs_tree, &theirs_map);
    let ours = hord_lang::IdentifiedTree::new(ours_tree, ours_map.nodes);
    let theirs = hord_lang::IdentifiedTree::new(theirs_tree, theirs_map.nodes);
    let merged = merge(
        &adapter,
        &common::file(),
        &base,
        &ours,
        &theirs,
        MergeMode::Corpus,
    )
    .map_err(|c| format!("soft conflict should still merge: {c:?}"))?;
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
    Ok(())
}

#[test]
fn disjoint_statements_in_same_function_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"fn f() {\n    let a = 1;\n    let b = 1;\n}\n";
    let ours_src = b"fn f() {\n    let a = 2;\n    let b = 1;\n}\n";
    let theirs_src = b"fn f() {\n    let a = 1;\n    let b = 2;\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint statements should compose: {c:?}"))?;
    let got = adapter.project(&merged.tree.tree);
    let want = b"fn f() {\n    let a = 2;\n    let b = 2;\n}\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
    Ok(())
}

#[test]
fn disjoint_methods_in_same_impl_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"impl Foo {\n    fn a() { let x = 1; }\n    fn b() { let y = 1; }\n}\n";
    let ours_src = b"impl Foo {\n    fn a() { let x = 2; }\n    fn b() { let y = 1; }\n}\n";
    let theirs_src = b"impl Foo {\n    fn a() { let x = 1; }\n    fn b() { let y = 2; }\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint methods should compose: {c:?}"))?;
    let got = adapter.project(&merged.tree.tree);
    let want = b"impl Foo {\n    fn a() { let x = 2; }\n    fn b() { let y = 2; }\n}\n";
    assert_eq!(
        got.as_slice(),
        want,
        "got:\n{}",
        String::from_utf8_lossy(got.as_slice())
    );
    Ok(())
}

#[test]
fn disjoint_method_inserts_in_same_impl_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"impl Foo {\n    fn a() {}\n}\n";
    let ours_src = b"impl Foo {\n    fn a() {}\n    fn b() {}\n}\n";
    let theirs_src = b"impl Foo {\n    fn a() {}\n    fn c() {}\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint method inserts should compose: {c:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("fn b()"), "ours method missing: {text}");
    assert!(text.contains("fn c()"), "theirs method missing: {text}");
    assert!(text.contains("fn a()"), "base method missing: {text}");
    Ok(())
}

#[test]
fn disjoint_use_items_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"use a::x;\nfn f() {}\n";
    let ours_src = b"use a::x;\nuse b::y;\nfn f() {}\n";
    let theirs_src = b"use a::x;\nuse c::z;\nfn f() {}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint uses should compose: {c:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("use b::y;"), "ours use missing: {text}");
    assert!(text.contains("use c::z;"), "theirs use missing: {text}");
    Ok(())
}

#[test]
fn disjoint_struct_fields_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"struct S {\n    a: i32,\n}\n";
    let ours_src = b"struct S {\n    a: i32,\n    b: i32,\n}\n";
    let theirs_src = b"struct S {\n    a: i32,\n    c: i32,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint fields should compose: {c:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("b: i32"), "ours field missing: {text}");
    assert!(text.contains("c: i32"), "theirs field missing: {text}");
    Ok(())
}

#[test]
fn disjoint_enum_variants_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"enum E {\n    A,\n}\n";
    let ours_src = b"enum E {\n    A,\n    B,\n}\n";
    let theirs_src = b"enum E {\n    A,\n    C,\n}\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint variants should compose: {c:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("B"), "ours variant missing: {text}");
    assert!(text.contains("C"), "theirs variant missing: {text}");
    Ok(())
}

#[test]
fn disjoint_toml_keys_compose() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = toml();
    let base_src = b"[dependencies]\nfoo = \"1\"\n";
    let ours_src = b"[dependencies]\nfoo = \"1\"\nbar = \"2\"\n";
    let theirs_src = b"[dependencies]\nfoo = \"1\"\nbaz = \"3\"\n";
    let merged = merge_identified(&adapter, base_src, ours_src, theirs_src)
        .map_err(|c| format!("disjoint toml keys should compose: {c:?}"))?;
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("bar"), "ours key missing: {text}");
    assert!(text.contains("baz"), "theirs key missing: {text}");
    Ok(())
}

#[test]
fn merge_bytes_do_not_depend_on_generated_node_ids() -> Result<(), Box<dyn std::error::Error>> {
    // These labels are `git merge-file --ours`. Two merges of the same bytes
    // must project the same file: NodeIds are random ULIDs.
    for case in ["0076", "0114", "0143"] {
        let first = project_case(case)?;
        let second = project_case(case)?;
        assert_eq!(
            first,
            second,
            "{case} changed between runs ({} vs {} bytes)",
            first.len(),
            second.len()
        );
    }
    Ok(())
}

fn project_case(case: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../corpora/merges/{case}"));
    let ext = ["rs", "toml"]
        .into_iter()
        .find(|ext| root.join(format!("base.{ext}")).exists())
        .ok_or_else(|| format!("{case} has no base.rs/base.toml"))?;
    let read = |name: &str| -> Result<Vec<u8>, String> {
        std::fs::read(root.join(format!("{name}.{ext}"))).map_err(|e| format!("{case} {name}: {e}"))
    };
    let base = read("base")?;
    let ours = read("ours")?;
    let theirs = read("theirs")?;
    if ext == "rs" {
        let adapter = rust();
        let merged = merge_identified(&adapter, &base, &ours, &theirs)
            .map_err(|c| format!("{case} should resolve: {c}"))?;
        Ok(adapter.project(&merged.tree.tree).as_slice().to_vec())
    } else {
        let adapter = toml();
        let merged = merge_identified(&adapter, &base, &ours, &theirs)
            .map_err(|c| format!("{case} should resolve: {c}"))?;
        Ok(adapter.project(&merged.tree.tree).as_slice().to_vec())
    }
}

#[test]
fn apply_of_diff_used_by_merge_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = rust();
    let base_src = b"fn a() {}\n";
    let ours_src = b"fn a() {}\nfn b() {}\n";
    let base = parse_identified(&adapter, base_src);
    let (ours_tree, ours_map) = identify_result(&adapter, &base, ours_src);
    let ops = diff(&common::file(), &base, &ours_tree, &ours_map);
    let applied = apply(&common::file(), &base, &ops, &ours_tree)?;
    assert_eq!(adapter.project(&applied.tree).as_slice(), ours_src);
    Ok(())
}

/// ADR 0014: the same line changed two ways. Corpus mode keeps ours (git
/// `--ours`); lander mode is a hard conflict naming the definition.
#[test]
fn lander_mode_never_resolves_by_landing_order() {
    let adapter = rust();
    let base = b"fn f() -> u32 {\n    1\n}\n\nfn g() -> u32 {\n    2\n}\n";
    let ours = b"fn f() -> u32 {\n    10\n}\n\nfn g() -> u32 {\n    2\n}\n";
    let theirs = b"fn f() -> u32 {\n    11\n}\n\nfn g() -> u32 {\n    2\n}\n";

    let corpus = merge_in(&adapter, base, ours, theirs, MergeMode::Corpus).expect("corpus merges");
    assert_eq!(projected_text(&adapter, &corpus.tree).as_bytes(), ours);

    let err = merge_in(&adapter, base, ours, theirs, MergeMode::Lander).expect_err("lander");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert_eq!(err.nodes.len(), 1, "{err}");

    // Edits that do combine still merge in lander mode.
    let theirs_g = b"fn f() -> u32 {\n    1\n}\n\nfn g() -> u32 {\n    20\n}\n";
    let merged = merge_in(&adapter, base, ours, theirs_g, MergeMode::Lander).expect("disjoint");
    let text = projected_text(&adapter, &merged.tree);
    assert!(
        text.contains("    10\n") && text.contains("    20\n"),
        "{text}"
    );
    // Both sides made the same edit: composes (spec §5.2 rule 2).
    let same = merge_in(&adapter, base, ours, ours, MergeMode::Lander).expect("same edit");
    assert_eq!(projected_text(&adapter, &same.tree).as_bytes(), ours);
}

/// ADR 0014: in lander mode a definition both sides edited, when the edits
/// do combine, re-parses and is flagged soft for the verifier.
#[test]
fn lander_mode_flags_a_combined_same_definition_edit_soft() {
    let adapter = rust();
    let base = b"fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n";
    let ours = b"fn f() -> u32 {\n    let a = 10;\n    let b = 2;\n    a + b\n}\n";
    let theirs = b"fn f() -> u32 {\n    let a = 1;\n    let b = 20;\n    a + b\n}\n";
    let merged = merge_in(&adapter, base, ours, theirs, MergeMode::Lander).expect("combines");
    let text = projected_text(&adapter, &merged.tree);
    assert!(text.contains("= 10;") && text.contains("= 20;"), "{text}");
    assert!(
        merged.soft.iter().any(|c| c.kind == ConflictKind::Soft),
        "{:?}",
        merged.soft
    );
    assert!(adapter.parse(text.as_bytes()).is_ok());
}

/// ADR 0015 amendment: contention at file level names the file root, in a
/// soft conflict (two top-level inserts at one index) and in a hard one
/// (both sides rewrote the same top-level macro call, which is file glue). Nil is never named.
#[test]
fn file_level_conflicts_name_the_file_root() -> Result<(), Box<dyn std::error::Error>> {
    use hord_core::NodeId;

    let adapter = rust();
    let root = NodeId::file_root(&common::file());
    let base = b"fn a() {}\nfn b() {}\n";
    let ours = b"fn a() {}\nfn x() {}\nfn b() {}\n";
    let theirs = b"fn a() {}\nfn y() {}\nfn b() {}\n";
    let merged = merge_in(&adapter, base, ours, theirs, MergeMode::Lander).expect("soft");
    let inserts = merged
        .soft
        .iter()
        .find(|c| c.reason.starts_with("two inserts"))
        .ok_or_else(|| format!("{:?}", merged.soft))?;
    assert_eq!(inserts.nodes, vec![root]);
    assert!(
        inserts.reason.contains(&root.to_string()),
        "{}",
        inserts.reason
    );

    let base = b"m!(one);\nfn f() {}\n";
    let ours = b"m!(two);\nfn f() {}\n";
    let theirs = b"m!(three);\nfn f() {}\n";
    let err = merge_in(&adapter, base, ours, theirs, MergeMode::Lander).expect_err("hard");
    assert_eq!(err.kind, ConflictKind::Hard);
    assert_eq!(err.nodes, vec![root], "{err}");
    Ok(())
}
