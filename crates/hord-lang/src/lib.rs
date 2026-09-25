//! Language adapter trait, interned CSTs, trivia attachment, and default
//! identity carrying (spec §3.3–3.4, §4.1–4.3).
//!
//! This crate does **not** parse source. Language crates (`hord-lang-rust`,
//! `hord-lang-toml`) own grammars (tree-sitter) and implement [`LangAdapter`].
//!
//! [`NodeTree`] interns [`hord_core::Node`]s by content
//! [`hord_core::ObjectId`]. Children are [`hord_core::ObjectId`]s;
//! `concat(children.raw) == raw` is the lossless invariant parse/project must
//! hold.
//!
//! [`normalized_hash`] documents the leaf trivia strip and hashes that text
//! via [`hord_core::ObjectId::of`]. An internal node's `normalized` id hashes
//! its children's `normalized` ids instead (ADR 0008).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod adapter;
mod error;
mod identify;
mod normalized;
mod rename;
mod tree;
mod trivia;

pub use adapter::{
    AdapterRegistry, Anchor, DefinitionFacts, LangAdapter, NameRef, ResolveCtx, Tier,
};
pub use error::ParseError;
pub use identify::{
    DefSite, IdentifiedTree, IdentityMapping, Site, def_sites, default_identify, enclosing_site,
    oid_at,
};
pub use normalized::normalized_hash;
pub use tree::{NodeTree, TreeBuilder};
pub use trivia::{
    AttachedSpan, AttachedToken, Lexeme, TokenSpan, attach_trivia, attach_trivia_spans,
};

/// The 3-way merge of a value that only one side changed: ours when both
/// sides agree or only ours changed, theirs when only theirs changed, and
/// `None` when both changed it differently.
pub fn prefer_unchanged<'a, T: PartialEq + ?Sized>(
    base: &'a T,
    ours: &'a T,
    theirs: &'a T,
) -> Option<&'a T> {
    if ours == theirs || theirs == base {
        Some(ours)
    } else if ours == base {
        Some(theirs)
    } else {
        None
    }
}

#[cfg(test)]
mod test_util {
    use super::*;
    use hord_core::{LangId, NodeKind, RepoPath};

    /// Synthetic adapter with no grammar. `fn` and `mod` are definitions.
    pub(crate) struct TestAdapter;

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
            Err(ParseError::failed(
                "hord-lang has no grammar; adapters own parse",
            ))
        }

        fn is_definition(&self, kind: &NodeKind) -> bool {
            matches!(kind.as_str(), "fn" | "mod")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::prefer_unchanged;

    #[test]
    fn prefer_unchanged_takes_the_side_that_changed() {
        assert_eq!(prefer_unchanged(&1, &1, &1), Some(&1));
        assert_eq!(prefer_unchanged(&1, &2, &1), Some(&2));
        assert_eq!(prefer_unchanged(&1, &1, &3), Some(&3));
        assert_eq!(prefer_unchanged(&1, &2, &2), Some(&2));
        assert_eq!(prefer_unchanged(&1, &2, &3), None);
        let (none, a, b) = (None, Some(&"a"), Some(&"b"));
        assert_eq!(prefer_unchanged(&none, &a, &none).copied(), Some(a));
        assert_eq!(prefer_unchanged(&a, &none, &a).copied(), Some(none));
        assert_eq!(prefer_unchanged(&none, &a, &b), None);
    }
}
