//! Hand fixtures: insert / delete / replace / move a definition.

mod common;

use hord_diff::{apply, diff};
use hord_lang::LangAdapter;

use common::{has_kind, identify_result, parse_identified, project, rust};

fn round_trip(base_src: &[u8], result_src: &[u8]) -> Vec<hord_core::Op> {
    let adapter = rust();
    let base = parse_identified(&adapter, base_src);
    let (result, mapping) = identify_result(&adapter, &base, result_src);
    let ops = diff(&base, &result, &mapping);
    let applied =
        apply(&base, &ops, &result).unwrap_or_else(|e| panic!("apply failed: {e}; ops={ops:?}"));
    let got = project(&adapter, &applied.tree);
    assert_eq!(
        got.as_slice(),
        result_src,
        "apply(diff) projection mismatch\nops={ops:?}\ngot:\n{}\nwant:\n{}",
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(result_src)
    );
    ops
}

#[test]
fn insert_a_function() {
    let base = b"fn a() {}\nfn b() {}\n";
    let result = b"fn a() {}\nfn c() {}\nfn b() {}\n";
    let ops = round_trip(base, result);
    assert!(
        has_kind(&ops, "insert"),
        "expected Insert for new function, got {ops:?}"
    );
}

#[test]
fn delete_a_function() {
    let base = b"fn a() {}\nfn b() {}\nfn c() {}\n";
    let result = b"fn a() {}\nfn c() {}\n";
    let ops = round_trip(base, result);
    assert!(
        has_kind(&ops, "delete"),
        "expected Delete for removed function, got {ops:?}"
    );
}

#[test]
fn replace_a_function() {
    let base = b"fn a() { let x = 1; }\nfn b() {}\n";
    let result = b"fn a() { let x = 2; }\nfn b() {}\n";
    let ops = round_trip(base, result);
    assert!(
        has_kind(&ops, "replace"),
        "expected Replace for edited function, got {ops:?}"
    );
}

#[test]
fn move_a_definition() {
    let base = b"mod a {\n    fn f() {}\n}\nmod b {}\n";
    let result = b"mod a {}\nmod b {\n    fn f() {}\n}\n";
    let adapter = rust();
    let base_tree = parse_identified(&adapter, base);
    let (result_tree, mapping) = identify_result(&adapter, &base_tree, result);
    assert!(
        !mapping.moves.is_empty(),
        "identify should emit Move for fn f; deltas={:?} moves={:?}",
        mapping.deltas,
        mapping.moves
    );
    let ops = diff(&base_tree, &result_tree, &mapping);
    assert!(
        has_kind(&ops, "move")
            || mapping
                .moves
                .iter()
                .any(|o| matches!(o, hord_core::Op::Move { .. })),
        "expected Move in mapping/ops, ops={ops:?} moves={:?}",
        mapping.moves
    );
    let applied = apply(&base_tree, &ops, &result_tree).expect("apply move");
    let got = project(&adapter, &applied.tree);
    assert_eq!(
        got.as_slice(),
        result,
        "move apply mismatch\nops={ops:?}\ngot:\n{}\nwant:\n{}",
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(result)
    );
}

#[test]
fn reorder_sibling_functions_applies() {
    let base = b"fn a() {}\nfn b() {}\n";
    let result = b"fn b() {}\nfn a() {}\n";
    round_trip(base, result);
}

#[test]
fn identical_trees_empty_ops() {
    let src = b"fn a() {}\nfn b() {}\n";
    let ops = round_trip(src, src);
    assert!(ops.is_empty(), "expected no ops, got {ops:?}");
}

#[test]
fn trivia_only_replace_still_applies() {
    let base = b"fn a() { let x = 1; }\n";
    let result = b"fn a() { let x  =  1; }\n";
    let ops = round_trip(base, result);
    assert!(
        has_kind(&ops, "replace") || ops.is_empty(),
        "trivia-only should Replace or be a no-op if normalized+raw match, got {ops:?}"
    );
}

#[test]
fn toml_insert_table() {
    let adapter = common::toml();
    let base_src = b"[a]\nx = 1\n\n[b]\ny = 2\n";
    let result_src = b"[a]\nx = 1\n\n[c]\nz = 3\n\n[b]\ny = 2\n";
    let base = parse_identified(&adapter, base_src);
    let (result, mapping) = identify_result(&adapter, &base, result_src);
    let ops = diff(&base, &result, &mapping);
    let applied = apply(&base, &ops, &result).expect("apply toml");
    assert_eq!(adapter.project(&applied.tree).as_slice(), result_src);
}
