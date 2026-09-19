//! Tier 1 Rust language adapter (spec §4.2).
//!
//! Parses every path that [`RustAdapter::matches`] accepts (`.rs` suffix).
//! There is no size cutoff (ADR 0003). Trivia attachment uses
//! [`hord_lang::attach_trivia`] (spec §3.3).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod cst;

use hord_core::{Bytes, LangId, NodeKind, RepoPath};
use hord_lang::{LangAdapter, NodeTree, ParseError, Tier};

/// tree-sitter-rust adapter (spec §4.2, Tier 1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustAdapter;

/// Language id reported by [`RustAdapter`] (`rust`).
pub const LANG: &str = "rust";

impl LangAdapter for RustAdapter {
    fn lang(&self) -> LangId {
        LangId::new(LANG)
    }

    fn tier(&self) -> Tier {
        Tier::Syntax
    }

    fn matches(&self, path: &RepoPath, _head: &[u8]) -> bool {
        path.components()
            .last()
            .is_some_and(|name| name.ends_with(".rs"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError> {
        cst::parse(bytes, &self.lang())
    }

    fn project(&self, tree: &NodeTree) -> Bytes {
        tree.to_bytes()
    }

    fn is_definition(&self, kind: &NodeKind) -> bool {
        matches!(
            kind.as_str(),
            "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
                | "type_item"
                | "associated_type"
                | "union_item"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;

    fn testdata_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
    }

    fn adapter() -> RustAdapter {
        RustAdapter
    }

    fn parse_ok(bytes: &[u8]) -> NodeTree {
        adapter().parse(bytes).expect("parse")
    }

    fn assert_lossless(bytes: &[u8]) {
        let tree = parse_ok(bytes);
        let projected = adapter().project(&tree);
        assert_eq!(projected.as_slice(), bytes);
        tree.check_concat().expect("concat invariant");
    }

    fn kinds_in(tree: &NodeTree) -> BTreeSet<String> {
        tree.iter()
            .map(|(_, n)| n.kind.as_str().to_owned())
            .collect()
    }

    #[test]
    fn matches_rs_suffix_only() {
        let a = adapter();
        let rs = RepoPath::from_str("src/lib.rs").unwrap();
        let toml = RepoPath::from_str("Cargo.toml").unwrap();
        let nested = RepoPath::from_str("crates/foo/src/main.rs").unwrap();
        assert!(a.matches(&rs, b"fn main() {}"));
        assert!(a.matches(&nested, &[]));
        assert!(!a.matches(&toml, b"[package]"));
        assert!(!a.matches(&RepoPath::from_str("foo.rs.bak").unwrap(), &[]));
        // ADR 0003: matches ignores head; parse has no size cutoff.
        assert!(a.matches(&rs, &[]));
    }

    #[test]
    fn fixtures_are_lossless() {
        let mut found = 0;
        for entry in fs::read_dir(testdata_dir()).expect("testdata") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
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
    fn is_definition_covers_tier1_kinds() {
        let bytes = fs::read(testdata_dir().join("definitions.rs")).unwrap();
        let tree = parse_ok(&bytes);
        let kinds = kinds_in(&tree);
        let a = adapter();
        for kind in [
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "mod_item",
            "const_item",
            "static_item",
            "macro_definition",
            "type_item",
            "union_item",
        ] {
            assert!(kinds.contains(kind), "fixture missing kind {kind}");
            assert!(
                a.is_definition(&NodeKind::new(kind)),
                "{kind} should be a definition"
            );
        }
        assert!(!a.is_definition(&NodeKind::new("let_declaration")));
        assert!(!a.is_definition(&NodeKind::new("source_file")));
        assert!(!a.is_definition(&NodeKind::new("line_comment")));
    }

    #[test]
    fn impls_and_macros_kinds() {
        let impls = parse_ok(&fs::read(testdata_dir().join("impls.rs")).unwrap());
        assert!(kinds_in(&impls).contains("impl_item"));
        let macros = parse_ok(&fs::read(testdata_dir().join("macros.rs")).unwrap());
        assert!(kinds_in(&macros).contains("macro_definition"));
        assert!(kinds_in(&macros).contains("macro_invocation"));
    }

    #[test]
    fn whitespace_does_not_change_normalized() {
        let a = parse_ok(b"fn foo() {}");
        let b = parse_ok(b"fn  foo ( ) {  }");
        let na = a.iter().find(|(_, n)| n.kind.as_str() == "function_item");
        let nb = b.iter().find(|(_, n)| n.kind.as_str() == "function_item");
        assert_eq!(
            na.map(|(_, n)| n.normalized),
            nb.map(|(_, n)| n.normalized),
            "whitespace-only change must preserve normalized"
        );
        assert_ne!(a.root(), b.root(), "raw change must change ObjectId");
    }

    #[test]
    fn lang_is_rust() {
        assert_eq!(adapter().lang().as_str(), "rust");
        assert_eq!(adapter().tier(), Tier::Syntax);
    }

    #[test]
    fn leading_doc_comment_attaches_to_function() {
        let src = b"/// doc\nfn foo() {}\n";
        let tree = parse_ok(src);
        let func = tree
            .iter()
            .map(|(_, n)| n)
            .find(|n| n.kind.as_str() == "function_item")
            .expect("function_item");
        let raw = std::str::from_utf8(func.raw.as_slice()).unwrap();
        assert!(
            raw.starts_with("/// doc"),
            "doc comment should move with the function, got {raw:?}"
        );
    }

    #[test]
    fn same_line_trailing_comment_attaches_to_preceding_item() {
        let src = b"fn a() {} // note\nfn b() {}\n";
        let tree = parse_ok(src);
        let a = tree
            .iter()
            .map(|(_, n)| n)
            .find(|n| {
                std::str::from_utf8(n.raw.as_slice())
                    .unwrap()
                    .contains("fn a")
                    && n.kind.as_str() == "function_item"
            })
            .expect("fn a");
        let raw_a = std::str::from_utf8(a.raw.as_slice()).unwrap();
        assert!(
            raw_a.contains("// note"),
            "same-line trailing comment should stay with fn a, got {raw_a:?}"
        );
    }
}
