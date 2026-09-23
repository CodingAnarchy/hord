//! [`LangAdapter`] trait and supporting types (spec §4.3).

use std::sync::Arc;

use hord_core::{Bytes, LangId, Node, NodeId, NodeKind, QualifiedName, RepoPath};

use crate::ParseError;
use crate::identify::{IdentifiedTree, IdentityMapping, default_identify};
use crate::tree::NodeTree;

/// Language support tier (spec §4.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Tier {
    /// Bytes only; git-equivalent behavior.
    Blob,
    /// Lossless CST, structural diff/merge.
    Syntax,
    /// Definitions, qualified names, references, stable identity.
    Semantic,
    /// Type-check, test, lint as evidence.
    Verified,
}

impl Tier {
    /// Numeric tier from spec §4.1 (0–3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Blob => 0,
            Self::Syntax => 1,
            Self::Semantic => 2,
            Self::Verified => 3,
        }
    }
}

/// Snapshot-scoped context for Tier 2 name resolution.
///
/// M1 leaves this empty. M2 fills it with identity and edge data.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ResolveCtx {}

impl ResolveCtx {
    /// An empty resolution context.
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }
}

/// An unresolved name mentioned in a node's body (spec §4.3).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub struct NameRef {
    /// The name as written, not yet resolved to a [`NodeId`].
    pub name: QualifiedName,
}

impl NameRef {
    /// Wrap a qualified name.
    #[must_use]
    pub fn new(name: impl Into<QualifiedName>) -> Self {
        Self { name: name.into() }
    }
}

/// Per-language adapter (spec §4.3).
///
/// Tier 1 methods (`parse`, `project`, `is_definition`) are required.
/// `project(parse(bytes))` must equal `bytes`. Tier 2 methods default to
/// empty/`None`. [`identify`](Self::identify) defaults to
/// [`default_identify`] (spec §3.4, ADR 0007).
///
/// Tree-sitter is **not** used here; language crates own grammars.
pub trait LangAdapter: Send + Sync {
    /// Language this adapter claims, e.g. `rust`.
    fn lang(&self) -> LangId;

    /// Highest tier this adapter implements.
    fn tier(&self) -> Tier;

    /// Whether this adapter should parse `path` given a byte prefix `head`.
    fn matches(&self, path: &RepoPath, head: &[u8]) -> bool;

    /// Tier 1. Must be lossless: `project(parse(bytes)) == bytes`.
    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError>;

    /// Tier 1. Project a tree back to source bytes.
    fn project(&self, tree: &NodeTree) -> Bytes;

    /// Tier 1. Which node kinds bear durable identity (spec §3.4).
    fn is_definition(&self, kind: &NodeKind) -> bool;

    /// Tier 2. Qualified name of a definition, given its ancestor chain.
    fn qualified_name(&self, path: &[&Node], node: &Node) -> Option<QualifiedName> {
        let _ = (path, node);
        None
    }

    /// Tier 2. Outgoing references from a node's body (names, not targets).
    fn references(&self, ctx: &ResolveCtx, node: &Node) -> Vec<NameRef> {
        let _ = (ctx, node);
        Vec::new()
    }

    /// Tier 2. Resolve a [`NameRef`] to a definition [`NodeId`], if possible.
    fn resolve(&self, ctx: &ResolveCtx, r: &NameRef) -> Option<NodeId> {
        let _ = (ctx, r);
        None
    }

    /// Tier 2. Definitions this test likely exercises.
    fn test_targets(&self, ctx: &ResolveCtx, test: &Node) -> Vec<NodeId> {
        let _ = (ctx, test);
        Vec::new()
    }

    /// Tier 2. Identity carrying between base and result. Default is
    /// [`default_identify`].
    fn identify(&self, base: &IdentifiedTree, result: &NodeTree) -> IdentityMapping {
        default_identify(self, base, result)
    }
}

/// Compiled-in adapter registry (spec §4.3). First match wins.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn LangAdapter>>,
}

impl AdapterRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an adapter. [`get`](Self::get) returns the first match.
    pub fn register(&mut self, adapter: Arc<dyn LangAdapter>) {
        self.adapters.push(adapter);
    }

    /// The first registered adapter for which [`LangAdapter::matches`] is true.
    #[must_use]
    pub fn get(&self, path: &RepoPath, head: &[u8]) -> Option<&dyn LangAdapter> {
        self.adapters
            .iter()
            .find(|a| a.matches(path, head))
            .map(Arc::as_ref)
    }

    /// Registered adapters in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &dyn LangAdapter> + '_ {
        self.adapters.iter().map(Arc::as_ref)
    }

    /// Number of registered adapters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    /// Whether no adapters are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestAdapter;
    use std::str::FromStr;

    #[test]
    fn registry_first_match_wins() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(TestAdapter));
        let path = RepoPath::from_str("src/lib.t").unwrap();
        assert!(reg.get(&path, b"").is_some());
        let other = RepoPath::from_str("src/lib.rs").unwrap();
        assert!(reg.get(&other, b"").is_none());
    }

    #[test]
    fn tier2_defaults_are_empty() {
        let a = TestAdapter;
        let node = Node {
            kind: NodeKind::new("fn"),
            lang: a.lang(),
            raw: Bytes::default(),
            normalized: hord_core::ObjectId::from_bytes([0; 32]),
            children: Vec::new(),
            name: None,
        };
        let ctx = ResolveCtx::new();
        assert!(a.qualified_name(&[], &node).is_none());
        assert!(a.references(&ctx, &node).is_empty());
        assert!(a.resolve(&ctx, &NameRef::new("foo")).is_none());
        assert!(a.test_targets(&ctx, &node).is_empty());
        assert_eq!(a.tier(), Tier::Syntax);
        assert_eq!(a.tier().as_u8(), 1);
    }
}
