//! Tier 1 TOML language adapter (spec §4.4).
//!
//! Parses every path [`TomlAdapter::matches`] accepts (suffix `.toml`,
//! including `Cargo.toml`). There is no size cutoff (ADR 0003). `Cargo.lock`
//! is out of scope until M3.
//!
//! Trivia attachment follows spec §3.3 via [`hord_lang::attach_trivia`].
//!
//! ## Definition-bearing kinds
//!
//! Named tables and keys from the tree-sitter-toml grammar:
//!
//! - `table` — `[foo]` / `[foo.bar]`
//! - `table_array_element` — `[[foo]]`
//! - `pair` — `key = value` (named by key, so two sides adding different
//!   keys in one table compose; spec §5.2 rule 1)
//!
//! Inline tables are anonymous containers; their inner `pair`s still name.
//! String contents omitted by the grammar are interned as `_content` (not a
//! definition).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod cst;

use hord_core::{LangId, NodeKind, RepoPath};
use hord_lang::{LangAdapter, NodeTree, ParseError, Tier};

/// tree-sitter-toml adapter (spec §4.4, Tier 1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TomlAdapter;

/// Language id reported by [`TomlAdapter`] (`toml`).
pub const LANG: &str = "toml";

impl LangAdapter for TomlAdapter {
    fn lang(&self) -> LangId {
        LangId::new(LANG)
    }

    fn tier(&self) -> Tier {
        Tier::Syntax
    }

    fn matches(&self, path: &RepoPath, _head: &[u8]) -> bool {
        path.components()
            .last()
            .is_some_and(|name| name.ends_with(".toml"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError> {
        cst::parse(bytes, &self.lang())
    }

    fn is_definition(&self, kind: &NodeKind) -> bool {
        matches!(kind.as_str(), "table" | "table_array_element" | "pair")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;

    use hord_lang::LangAdapter;

    fn testdata_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
    }

    fn adapter() -> TomlAdapter {
        TomlAdapter
    }

    fn parse_ok(bytes: &[u8]) -> NodeTree {
        adapter().parse(bytes).expect("parse")
    }

    fn assert_lossless(bytes: &[u8]) {
        let tree = parse_ok(bytes);
        let projected = adapter().project(&tree);
        assert_eq!(projected.as_slice(), bytes);
        tree.check_concat().expect("concat(children.raw) == raw");
    }

    fn kinds_in(tree: &NodeTree) -> BTreeSet<String> {
        tree.iter()
            .map(|(_, n)| n.kind.as_str().to_owned())
            .collect()
    }

    #[test]
    fn matches_toml_suffix_including_cargo_toml() {
        let a = adapter();
        let cargo = RepoPath::from_str("Cargo.toml").unwrap();
        let nested = RepoPath::from_str("crates/foo/Cargo.toml").unwrap();
        let other = RepoPath::from_str("settings.toml").unwrap();
        let rs = RepoPath::from_str("src/lib.rs").unwrap();
        let lock = RepoPath::from_str("Cargo.lock").unwrap();
        assert!(a.matches(&cargo, b"[package]"));
        assert!(a.matches(&nested, &[]));
        assert!(a.matches(&other, &[]));
        assert!(!a.matches(&rs, b"fn main() {}"));
        assert!(!a.matches(&lock, b"# This file is automatically"));
        assert!(!a.matches(&RepoPath::from_str("foo.toml.bak").unwrap(), &[]));
        // ADR 0003: matches ignores head; parse has no size cutoff.
        assert!(a.matches(&cargo, &[]));
    }

    #[test]
    fn lang_and_tier() {
        let a = adapter();
        assert_eq!(a.lang().as_str(), "toml");
        assert_eq!(a.tier(), Tier::Syntax);
        assert_eq!(a.tier().as_u8(), 1);
    }

    #[test]
    fn fixtures_are_lossless() {
        let mut found = 0;
        for entry in fs::read_dir(testdata_dir()).expect("testdata") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let tree = adapter()
                .parse(&bytes)
                .unwrap_or_else(|e| panic!("parse {path:?}: {e}"));
            let projected = adapter().project(&tree);
            assert_eq!(
                projected.as_slice(),
                bytes.as_slice(),
                "lossless {}",
                path.display()
            );
            tree.check_concat()
                .unwrap_or_else(|e| panic!("concat {}: {e}", path.display()));
            found += 1;
        }
        assert!(found >= 5, "expected several fixtures, found {found}");
    }

    #[test]
    fn empty_file() {
        assert_lossless(b"");
    }

    #[test]
    fn comment_only_file() {
        assert_lossless(b"# just a comment\n");
    }

    #[test]
    fn is_definition_tables_and_pairs() {
        let bytes = fs::read(testdata_dir().join("nested_tables.toml")).unwrap();
        let tree = parse_ok(&bytes);
        let kinds = kinds_in(&tree);
        let a = adapter();
        assert!(kinds.contains("table"), "fixture missing table");
        assert!(kinds.contains("pair"), "fixture missing pair");
        assert!(a.is_definition(&NodeKind::new("table")));
        assert!(a.is_definition(&NodeKind::new("table_array_element")));
        assert!(a.is_definition(&NodeKind::new("pair")));

        let arrays = parse_ok(&fs::read(testdata_dir().join("arrays_of_tables.toml")).unwrap());
        assert!(kinds_in(&arrays).contains("table_array_element"));

        assert!(!a.is_definition(&NodeKind::new("document")));
        assert!(!a.is_definition(&NodeKind::new("inline_table")));
        assert!(!a.is_definition(&NodeKind::new("bare_key")));
        assert!(!a.is_definition(&NodeKind::new(cst::CONTENT_KIND)));
        assert!(!a.is_definition(&NodeKind::new("comment")));
    }

    #[test]
    fn dotted_keys_and_strings_kinds() {
        let dotted = parse_ok(&fs::read(testdata_dir().join("dotted_keys.toml")).unwrap());
        assert!(kinds_in(&dotted).contains("dotted_key"));
        let strings = parse_ok(&fs::read(testdata_dir().join("strings.toml")).unwrap());
        assert!(kinds_in(&strings).contains("string"));
    }

    #[test]
    fn whitespace_does_not_change_normalized() {
        let a = parse_ok(b"a=1\n");
        let b = parse_ok(b"a = 1\n");
        let na = a.iter().find(|(_, n)| n.kind.as_str() == "pair");
        let nb = b.iter().find(|(_, n)| n.kind.as_str() == "pair");
        assert_eq!(
            na.map(|(_, n)| n.normalized),
            nb.map(|(_, n)| n.normalized),
            "whitespace-only change must preserve normalized"
        );
        assert_ne!(
            a.root(),
            b.root(),
            "raw change must change the root ObjectId"
        );
    }

    #[test]
    fn string_spaces_are_not_trivia() {
        let compact = parse_ok(b"a = \"x\"\n");
        let padded = parse_ok(b"a = \" x \"\n");
        let sa = compact
            .iter()
            .find(|(_, n)| n.kind.as_str() == "string")
            .map(|(_, n)| n.normalized);
        let sb = padded
            .iter()
            .find(|(_, n)| n.kind.as_str() == "string")
            .map(|(_, n)| n.normalized);
        assert_ne!(
            sa, sb,
            "spaces inside a TOML string are content, not trivia"
        );
    }

    #[test]
    fn leading_comment_attaches_to_following_table() {
        let src = b"# doc\n[package]\nname = \"x\"\n";
        let tree = parse_ok(src);
        let table = tree
            .iter()
            .find(|(_, n)| n.kind.as_str() == "table")
            .map(|(_, n)| n)
            .expect("table");
        let raw = std::str::from_utf8(table.raw.as_slice()).unwrap();
        assert!(
            raw.contains("# doc"),
            "leading comment should move with the table, got {raw:?}"
        );
    }

    #[test]
    fn same_line_trailing_comment_attaches_to_preceding_pair() {
        let src = b"name = \"x\" # note\n[other]\n";
        let tree = parse_ok(src);
        let pair = tree
            .iter()
            .find(|(_, n)| n.kind.as_str() == "pair")
            .map(|(_, n)| n)
            .expect("pair");
        let raw = std::str::from_utf8(pair.raw.as_slice()).unwrap();
        assert!(
            raw.contains("# note"),
            "same-line trailing comment should stay with the pair, got {raw:?}"
        );
    }

    #[test]
    fn as_lang_adapter_trait_object() {
        let a: &dyn LangAdapter = &TomlAdapter;
        assert!(a.matches(&RepoPath::from_str("Cargo.toml").unwrap(), &[]));
        assert_lossless(b"[a]\nb = 1\n");
    }
}
