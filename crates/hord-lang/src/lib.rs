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
//! [`normalized_hash`] documents the trivia strip and hashes the stripped
//! bytes via [`hord_core::ObjectId::of`] (canonical CBOR of a
//! [`hord_core::Bytes`] payload).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod adapter;
mod error;
mod identify;
mod normalized;
mod tree;
mod trivia;

pub use adapter::{AdapterRegistry, LangAdapter, NameRef, ResolveCtx, Tier};
pub use error::ParseError;
pub use identify::{IdentifiedTree, IdentityMapping, default_identify};
pub use normalized::normalized_hash;
pub use tree::NodeTree;
pub use trivia::{
    AttachedSpan, AttachedToken, Lexeme, TokenSpan, attach_trivia, attach_trivia_spans,
};

/// Result alias for this crate.
pub type Result<T, E = ParseError> = std::result::Result<T, E>;

#[cfg(test)]
mod test_util {
    use super::*;
    use hord_core::{Bytes, LangId, NodeKind, RepoPath};

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

        fn project(&self, tree: &NodeTree) -> Bytes {
            tree.to_bytes()
        }

        fn is_definition(&self, kind: &NodeKind) -> bool {
            matches!(kind.as_str(), "fn" | "mod")
        }
    }
}
