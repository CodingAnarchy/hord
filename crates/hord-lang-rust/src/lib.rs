//! Rust language adapter (spec §4.2).
//!
//! Tier 1 parses every path that [`RustAdapter::matches`] accepts (`.rs`
//! suffix). There is no size cutoff (ADR 0003). Trivia attachment uses
//! [`hord_lang::attach_trivia`] (spec §3.3).
//!
//! Tier 2 resolves names with tree-sitter: module tree from `mod` items and
//! file layout, `use` paths, and reference edges by name inside the crate.
//! A path dependency whose source is in the snapshot resolves too (ADR 0010).
//! The resolver does not do trait resolution or type inference. It is sound
//! for write sets (every definition kind gets a qualified name) and
//! conservative for read sets (ambiguous names produce one edge per
//! candidate). Macro invocations are not expanded.
//!
//! [`cargo_lock`] bridges `Cargo.lock` onto the generic TOML adapter and
//! merge (ADR 0013): [`CargoLockAdapter`] and [`merge_cargo_lock`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod cargo_lock;
mod cst;
mod facts;
mod manifest;
mod resolve;

use hord_core::{LangId, Node, NodeId, NodeKind, QualifiedName, RepoPath};
use hord_lang::{
    Anchor, DefinitionFacts, LangAdapter, NameRef, NodeTree, ParseError, ResolveCtx, Tier,
};

pub use cargo_lock::{
    CARGO_LOCK_LANG, CargoLockAdapter, CargoLockConflict, CargoLockMergeError, is_cargo_lock,
    merge_cargo_lock,
};
pub use manifest::ManifestFile;
pub use resolve::RustFile;

/// tree-sitter-rust adapter (spec §4.2). Tier 1 syntax plus Tier 2 names.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustAdapter;

/// Language id reported by [`RustAdapter`] (`rust`).
pub const LANG: &str = "rust";

impl LangAdapter for RustAdapter {
    fn lang(&self) -> LangId {
        LangId::new(LANG)
    }

    fn tier(&self) -> Tier {
        Tier::Semantic
    }

    fn matches(&self, path: &RepoPath, _head: &[u8]) -> bool {
        path.components()
            .last()
            .is_some_and(|name| name.ends_with(".rs"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError> {
        cst::parse(bytes, &self.lang())
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
                | "use_declaration"
                | "extern_crate_declaration"
                | "enum_variant"
                | "field_declaration"
                | "inner_attribute_item"
                | "foreign_mod_item"
        )
    }

    fn qualified_name(&self, path: &[&Node], node: &Node) -> Option<QualifiedName> {
        resolve::qualified_name(path, node)
    }

    fn references(&self, ctx: &ResolveCtx, node: &Node) -> Vec<NameRef> {
        resolve::references(ctx, node)
    }

    fn references_at(&self, ctx: &ResolveCtx, anchor: &Anchor, node: &Node) -> Vec<NameRef> {
        resolve::references_at(ctx, anchor, node)
    }

    fn resolve(&self, ctx: &ResolveCtx, name: &NameRef) -> Option<NodeId> {
        resolve::resolve_name(ctx, name)
    }

    fn test_targets(&self, ctx: &ResolveCtx, test: &Node) -> Vec<NodeId> {
        resolve::test_targets(ctx, test)
    }

    fn definition_facts(
        &self,
        source: &[u8],
        spans: &[std::ops::Range<usize>],
    ) -> Vec<DefinitionFacts> {
        facts::definition_facts(source, spans, |kind| {
            self.is_definition(&NodeKind::new(kind))
        })
    }
}

impl RustAdapter {
    /// Index `files` for Tier 2 resolution (spec §4.2).
    ///
    /// Builds the module tree from `mod` items and Cargo-style file layout
    /// (`src/lib.rs`, `src/foo.rs`, `src/foo/mod.rs`, `#[path]`), records
    /// `use` imports, and extracts definitions. `#[test]` (any attribute path
    /// whose last segment is `test`, such as `#[tokio::test]`) and
    /// `#[cfg(test)]` are recognized by attribute inspection.
    ///
    /// A single crate root is the Rust path `crate`. Several roots in one
    /// context (lib and bin, integration tests) are disambiguated by
    /// repository path so their modules do not collide.
    ///
    /// The name index for a context is built on the first
    /// [`LangAdapter::references`], [`LangAdapter::resolve`], or
    /// [`LangAdapter::test_targets`] call and reused until the context
    /// changes. Repeated calls return the same edges.
    #[must_use]
    pub fn resolve_context(&self, files: &[RustFile<'_>]) -> ResolveCtx {
        self.resolve_context_with(files, &[])
    }

    /// Like [`Self::resolve_context`], and also follow path dependencies in
    /// `manifests` (ADR 0010).
    #[must_use]
    pub fn resolve_context_with(
        &self,
        files: &[RustFile<'_>],
        manifests: &[ManifestFile<'_>],
    ) -> ResolveCtx {
        resolve::resolve_context_with(files, manifests)
    }

    /// Module path of each file, aligned with `files`.
    #[must_use]
    pub fn file_modules(&self, files: &[RustFile<'_>]) -> Vec<String> {
        resolve::file_modules(files)
    }

    /// Path-dependency links `(from crate module, extern name, target module)`.
    #[must_use]
    pub fn manifest_links(
        &self,
        files: &[RustFile<'_>],
        manifests: &[ManifestFile<'_>],
    ) -> Vec<(String, String, String)> {
        resolve::manifest_links(files, manifests)
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
        let rs = RepoPath::from_str("src/lib.rs").expect("parse repo path src/lib.rs");
        let toml = RepoPath::from_str("Cargo.toml").expect("parse repo path Cargo.toml");
        let nested = RepoPath::from_str("crates/foo/src/main.rs")
            .expect("parse repo path crates/foo/src/main.rs");
        assert!(a.matches(&rs, b"fn main() {}"));
        assert!(a.matches(&nested, &[]));
        assert!(!a.matches(&toml, b"[package]"));
        assert!(!a.matches(
            &RepoPath::from_str("foo.rs.bak").expect("parse repo path foo.rs.bak"),
            &[]
        ));
        // ADR 0003: matches ignores head; parse has no size cutoff.
        assert!(a.matches(&rs, &[]));
    }

    #[test]
    fn fixtures_are_lossless() -> Result<(), Box<dyn std::error::Error>> {
        let mut found = 0;
        for entry in fs::read_dir(testdata_dir()).expect("testdata") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| format!("read {path:?}: {e}"))?;
            let tree = adapter()
                .parse(&bytes)
                .map_err(|e| format!("parse {path:?}: {e}"))?;
            let projected = adapter().project(&tree);
            assert_eq!(
                projected.as_slice(),
                bytes.as_slice(),
                "lossless {}",
                path.display()
            );
            tree.check_concat()
                .map_err(|e| format!("concat {}: {e}", path.display()))?;
            found += 1;
        }
        assert!(found >= 5, "expected several fixtures, found {found}");
        Ok(())
    }

    #[test]
    fn empty_file() {
        assert_lossless(b"");
    }

    #[test]
    fn is_definition_covers_tier1_kinds() {
        let bytes =
            fs::read(testdata_dir().join("definitions.rs")).expect("read testdata/definitions.rs");
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
            "use_declaration",
            "enum_variant",
            "field_declaration",
        ] {
            assert!(kinds.contains(kind), "fixture missing kind {kind}");
            assert!(
                a.is_definition(&NodeKind::new(kind)),
                "{kind} should be a definition"
            );
        }
        assert!(a.is_definition(&NodeKind::new("extern_crate_declaration")));
        assert!(a.is_definition(&NodeKind::new("inner_attribute_item")));
        assert!(a.is_definition(&NodeKind::new("foreign_mod_item")));
        assert!(!a.is_definition(&NodeKind::new("let_declaration")));
        assert!(!a.is_definition(&NodeKind::new("source_file")));
        assert!(!a.is_definition(&NodeKind::new("line_comment")));
    }

    #[test]
    fn impls_and_macros_kinds() {
        let impls =
            parse_ok(&fs::read(testdata_dir().join("impls.rs")).expect("read testdata/impls.rs"));
        assert!(kinds_in(&impls).contains("impl_item"));
        let macros =
            parse_ok(&fs::read(testdata_dir().join("macros.rs")).expect("read testdata/macros.rs"));
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
        assert_eq!(adapter().tier(), Tier::Semantic);
        assert_eq!(adapter().tier().as_u8(), 2);
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
        let raw = std::str::from_utf8(func.raw.as_slice()).expect("function raw is UTF-8");
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
                std::str::from_utf8(n.raw.as_slice()).is_ok_and(|raw| raw.contains("fn a"))
                    && n.kind.as_str() == "function_item"
            })
            .expect("fn a");
        let raw_a = std::str::from_utf8(a.raw.as_slice()).expect("fn a raw is UTF-8");
        assert!(
            raw_a.contains("// note"),
            "same-line trailing comment should stay with fn a, got {raw_a:?}"
        );
    }
}
