//! `apply(base, diff(base, result))` projects equal to `result` (spec §5.1 / M1).

mod common;

use hord_diff::{apply, diff};
use hord_lang::LangAdapter;
use hord_lang_rust::RustAdapter;
use hord_lang_toml::TomlAdapter;
use proptest::prelude::*;

use common::{identify_result, parse_identified};

fn rust_file(fns: &[(String, u8)]) -> String {
    let mut s = String::new();
    for (name, body) in fns {
        s.push_str("fn ");
        s.push_str(name);
        s.push_str("() { let x = ");
        s.push_str(&body.to_string());
        s.push_str("; }\n");
    }
    s
}

fn toml_file(tables: &[(String, u8)]) -> String {
    let mut s = String::new();
    for (name, val) in tables {
        s.push('[');
        s.push_str(name);
        s.push_str("]\nvalue = ");
        s.push_str(&val.to_string());
        s.push('\n');
    }
    s
}

fn unique_names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("n{i}")).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn rust_apply_diff_projects_equal(
        n in 1usize..5,
        bodies in prop::collection::vec(0u8..20, 1..5),
        bodies2 in prop::collection::vec(0u8..20, 1..5),
        extra in 0u8..20,
        drop_i in 0usize..4,
        insert_at in 0usize..5,
    ) {
        let names = unique_names(n);
        let base_fns: Vec<(String, u8)> = names
            .iter()
            .zip(bodies.iter().cycle())
            .map(|(n, b)| (n.clone(), *b))
            .collect();
        let mut result_fns = base_fns.clone();
        // Replace some bodies.
        for (slot, b) in result_fns.iter_mut().zip(bodies2.iter().cycle()) {
            slot.1 = *b;
        }
        // Maybe drop one.
        if !result_fns.is_empty() {
            let i = drop_i % result_fns.len();
            if result_fns.len() > 1 {
                result_fns.remove(i);
            }
        }
        // Maybe insert one.
        let at = insert_at % (result_fns.len() + 1);
        result_fns.insert(at, ("extra".to_owned(), extra));

        let base_src = rust_file(&base_fns);
        let result_src = rust_file(&result_fns);
        let adapter = RustAdapter;
        let base = parse_identified(&adapter, base_src.as_bytes());
        let (result, mapping) = identify_result(&adapter, &base, result_src.as_bytes());
        let ops = diff(&common::file(), &base, &result, &mapping);
        let applied = apply(&common::file(), &base, &ops, &result).map_err(|e| {
            TestCaseError::fail(format!("apply: {e}; ops={ops:?}"))
        })?;
        let projected = adapter.project(&applied.tree);
        prop_assert_eq!(projected.as_slice(), result_src.as_bytes());
        let _ = base_fns;
    }

    #[test]
    fn toml_apply_diff_projects_equal(
        n in 1usize..4,
        vals in prop::collection::vec(0u8..20, 1..4),
        vals2 in prop::collection::vec(0u8..20, 1..4),
        extra in 0u8..20,
        drop_i in 0usize..3,
        insert_at in 0usize..4,
    ) {
        let names = unique_names(n);
        let base_tbl: Vec<(String, u8)> = names
            .iter()
            .zip(vals.iter().cycle())
            .map(|(n, b)| (n.clone(), *b))
            .collect();
        let mut result_tbl = base_tbl.clone();
        for (slot, b) in result_tbl.iter_mut().zip(vals2.iter().cycle()) {
            slot.1 = *b;
        }
        if result_tbl.len() > 1 {
            let i = drop_i % result_tbl.len();
            result_tbl.remove(i);
        }
        let at = insert_at % (result_tbl.len() + 1);
        result_tbl.insert(at, ("extra".to_owned(), extra));

        let base_src = toml_file(&base_tbl);
        let result_src = toml_file(&result_tbl);
        let adapter = TomlAdapter;
        let base = parse_identified(&adapter, base_src.as_bytes());
        let (result, mapping) = identify_result(&adapter, &base, result_src.as_bytes());
        let ops = diff(&common::file(), &base, &result, &mapping);
        let applied = apply(&common::file(), &base, &ops, &result).map_err(|e| {
            TestCaseError::fail(format!("apply: {e}; ops={ops:?}"))
        })?;
        let projected = adapter.project(&applied.tree);
        prop_assert_eq!(projected.as_slice(), result_src.as_bytes());
    }
}
