//! Hand fixtures: insert / delete / replace / move a definition.

mod common;

use hord_diff::{apply, diff};
use hord_lang::LangAdapter;

use common::{has_kind, identify_result, parse_identified, project, rust};

fn round_trip(base_src: &[u8], result_src: &[u8]) -> Vec<hord_core::Op> {
    let adapter = rust();
    let base = parse_identified(&adapter, base_src);
    let (result, mapping) = identify_result(&adapter, &base, result_src);
    let ops = diff(&common::file(), &base, &result, &mapping);
    let applied = apply(&common::file(), &base, &ops, &result)
        .unwrap_or_else(|e| panic!("apply failed: {e}; ops={ops:?}"));
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
    let ops = diff(&common::file(), &base_tree, &result_tree, &mapping);
    assert!(
        has_kind(&ops, "move")
            || mapping
                .moves
                .iter()
                .any(|o| matches!(o, hord_core::Op::Move { .. })),
        "expected Move in mapping/ops, ops={ops:?} moves={:?}",
        mapping.moves
    );
    let applied = apply(&common::file(), &base_tree, &ops, &result_tree).expect("apply move");
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
    let ops = diff(&common::file(), &base, &result, &mapping);
    let applied = apply(&common::file(), &base, &ops, &result).expect("apply toml");
    assert_eq!(adapter.project(&applied.tree).as_slice(), result_src);
}

/// ADR 0015: file-level ops name the path-derived root, never nil, and the
/// same edit in another file names that file's root.
#[test]
fn file_level_ops_name_the_path_derived_root() -> Result<(), Box<dyn std::error::Error>> {
    use hord_core::{NodeId, Op, RepoPath};

    let adapter = rust();
    let base = parse_identified(&adapter, b"fn a() {}\n");
    let (result, mapping) = identify_result(&adapter, &base, b"fn a() {}\nfn b() {}\n");
    let lib: RepoPath = "src/lib.rs".parse()?;
    let other: RepoPath = "src/other.rs".parse()?;
    let ops = diff(&lib, &base, &result, &mapping);
    let root = NodeId::file_root(&lib);
    assert!(
        ops.iter().any(|op| matches!(
            op,
            Op::Insert { parent, .. } | Op::Replace { node: parent, .. } if *parent == root
        )),
        "{ops:?}"
    );
    let nil_free = |ops: &[Op]| {
        ops.iter().all(|op| match op {
            Op::Insert { parent, .. } => *parent != NodeId::nil(),
            Op::Replace { node, .. } | Op::Delete { node } => *node != NodeId::nil(),
            _ => true,
        })
    };
    assert!(nil_free(&ops));
    let elsewhere = diff(&other, &base, &result, &mapping);
    assert_ne!(ops, elsewhere);
    // Ops for one file do not apply to another: the parent is unknown there.
    assert!(apply(&other, &base, &ops, &result).is_err());
    // Nil is not a file root: like any unknown id, it does not apply.
    let nil = vec![Op::Insert {
        parent: NodeId::nil(),
        index: 0,
        node: result.root().ok_or("result tree has a root")?,
    }];
    assert!(apply(&lib, &base, &nil, &result).is_err());
    // A glue-only edit replaces the root id.
    let (glue, glue_map) = identify_result(&adapter, &base, b"// note\nfn a() {}\n");
    let glue_ops = diff(&lib, &base, &glue, &glue_map);
    assert!(
        glue_ops
            .iter()
            .any(|op| matches!(op, Op::Replace { node, .. } if *node == NodeId::file_root(&lib))),
        "{glue_ops:?}"
    );
    Ok(())
}

/// Identical definitions at two sites (the same `use` in two functions) are
/// two identities. Editing a third function is a Replace of that function,
/// not a whole-file replace, and apply reproduces the result.
#[test]
fn identical_definitions_do_not_collapse() -> Result<(), Box<dyn std::error::Error>> {
    use hord_core::Op;
    let adapter = rust();
    let src = "pub fn target(x: u32) -> u32 {\n    x + 1\n}\n\npub fn first(p: &str) -> usize {\n    #[cfg(unix)]\n    {\n        use std::os::unix::prelude::*;\n        p.len()\n    }\n}\n\npub fn second(p: &str) -> usize {\n    #[cfg(unix)]\n    {\n        use std::os::unix::prelude::*;\n        p.len() + 1\n    }\n}\n";
    let edited = src.replace("    x + 1\n", "    let _sim = 1_u64;\n    x + 1\n");
    let base = parse_identified(&adapter, src.as_bytes());
    assert_eq!(base.ids.len(), 5, "target, first, second, and both uses");
    let (result, mapping) = identify_result(&adapter, &base, edited.as_bytes());
    assert!(mapping.moves.is_empty(), "{:?}", mapping.moves);
    assert!(mapping.deltas.is_empty(), "{:?}", mapping.deltas);
    let ops = diff(&common::file(), &base, &result, &mapping);
    let (_, target) = common::def_named(&base, "fn target").ok_or("base defines fn target")?;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert!(
        matches!(ops[0], Op::Replace { node, .. } if node == target),
        "{ops:?}"
    );
    let applied = apply(&common::file(), &base, &ops, &result)?;
    assert_eq!(project(&adapter, &applied.tree), edited.as_bytes());

    // Editing the second copy of the duplicate edits `second`, not `first`.
    let edited = src.replace("p.len() + 1", "p.len() + 2");
    let (result, mapping) = identify_result(&adapter, &base, edited.as_bytes());
    let ops = diff(&common::file(), &base, &result, &mapping);
    let (_, second) = common::def_named(&base, "fn second").ok_or("base defines fn second")?;
    assert!(
        matches!(ops[..], [Op::Replace { node, .. }] if node == second),
        "{ops:?}"
    );
    let applied = apply(&common::file(), &base, &ops, &result)?;
    assert_eq!(project(&adapter, &applied.tree), edited.as_bytes());
    Ok(())
}

/// `apply_identified` takes the ids of replaced and inserted content from
/// the store (the change's own result), nested definitions included, and
/// invents none. Content it cannot pair keeps no id.
#[test]
fn apply_identified_takes_ids_from_the_store() {
    use hord_diff::apply_identified;
    use hord_lang::IdentifiedTree;

    let adapter = rust();
    let lib = common::file();
    let base = parse_identified(&adapter, b"fn a() { 1 }\n");
    let result_src: &[u8] = b"fn a() { 2 }\nmod m {\n    fn inner() {}\n}\n";
    let (result, mapping) = identify_result(&adapter, &base, result_src);
    let store = IdentifiedTree::new(result.clone(), mapping.nodes.clone());
    let ops = diff(&lib, &base, &result, &mapping);
    assert_eq!(common::ops_kinds(&ops), ["replace", "insert"]);

    let applied = apply_identified(&lib, &base, &ops, &store).expect("apply_identified");
    assert_eq!(applied.tree.root(), result.root());
    assert_eq!(applied.ids, store.ids);
    // Plain `apply` has no ids for the inserted content to take.
    let plain = apply(&lib, &base, &ops, &result).expect("apply");
    assert_ne!(plain.ids, store.ids);

    // A store without ids: nothing new can be placed, so only base ids stay.
    let bare = IdentifiedTree::new(result.clone(), Default::default());
    let unplaced = apply_identified(&lib, &base, &ops, &bare).expect("apply_identified");
    let base_ids: Vec<_> = base.ids.values().collect();
    assert!(unplaced.ids.values().all(|id| base_ids.contains(&id)));
    assert!(unplaced.ids.len() < store.ids.len());
}
