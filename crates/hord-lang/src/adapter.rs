//! [`LangAdapter`] trait and supporting types (spec §4.3).

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use hord_core::{Bytes, LangId, Node, NodeId, NodeKind, ObjectId, QualifiedName, RepoPath};

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

/// One definition in a [`ResolveCtx`] snapshot.
///
/// Language adapters fill these through [`ResolveCtx::add_definition`].
/// `parent` is adapter-defined linkage (for example an impl or enum) and is
/// empty for names in the module's own namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Definition {
    node_id: NodeId,
    object_id: Option<ObjectId>,
    kind: NodeKind,
    module: QualifiedName,
    simple: QualifiedName,
    parent: QualifiedName,
    is_test: bool,
    cfg_test: bool,
}

/// A `use` (or glob) recorded in the module that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Import {
    module: QualifiedName,
    /// Binding name, or `*` for a glob.
    local: QualifiedName,
    /// Path as written (`crate::foo::Bar`, `super::baz`).
    path: QualifiedName,
    glob: bool,
}

/// A parsed file root, so a whole-file node can be placed in a module.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRoot {
    object_id: ObjectId,
    /// Repository path, when the adapter recorded it. Two files with the
    /// same content share `object_id`; the path tells them apart.
    path: Option<RepoPath>,
    module: QualifiedName,
}

/// Which definition (or file) a node is, for Tier 2 scope.
///
/// Content does not identify a site: two identical definitions in different
/// modules are one interned [`Node`]. An anchor names the site, so
/// [`LangAdapter::references_at`] resolves names in that site's own module
/// (spec §3.4, positional identity).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Anchor {
    /// The definition with this [`NodeId`] in the context.
    Definition(NodeId),
    /// Glue or a definition with no id of its own, placed in this file's
    /// module.
    File(RepoPath),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Link {
    from: QualifiedName,
    name: QualifiedName,
    target: QualifiedName,
}

/// Snapshot-scoped context for Tier 2 name resolution (spec §4.3).
///
/// Holds the definitions, imports, and file modules a [`LangAdapter`] needs
/// to resolve names. An empty context (from [`ResolveCtx::new`]) resolves
/// nothing. Adapters fill it for one snapshot; the Rust adapter's entry
/// point is `RustAdapter::resolve_context`.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ResolveCtx {
    definitions: Vec<Definition>,
    imports: Vec<Import>,
    files: Vec<FileRoot>,
    links: Vec<Link>,
    /// Changes whenever a definition, import, file, or link is added.
    ///
    /// Adapters use this to reuse a name index. Equal stamps mean equal
    /// contents, including two clones that have not been edited since.
    stamp: u64,
    /// Adapter data derived from these contents (a name index), built on
    /// first use by [`Self::derived`]. Replaced, not cleared, on every edit,
    /// so a clone that has not been edited keeps sharing it.
    derived: Derived,
    /// Adapter state kept for building the next snapshot's context from
    /// this one ([`Self::set_carry`]). Not part of the contents.
    carry: Option<Arc<dyn Any + Send + Sync>>,
}

/// One lazily built, shareable value derived from a [`ResolveCtx`].
#[derive(Clone, Default)]
struct Derived(Arc<OnceLock<Arc<dyn Any + Send + Sync>>>);

impl std::fmt::Debug for Derived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Derived")
            .field(&self.0.get().is_some())
            .finish()
    }
}

fn fresh_stamp() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl ResolveCtx {
    /// An empty resolution context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stamp: fresh_stamp(),
            ..Self::default()
        }
    }

    /// Identity of this context's contents.
    ///
    /// Stable until [`Self::add_definition`], [`Self::add_import`],
    /// [`Self::add_file`], or [`Self::add_link`]. Adapters may cache an
    /// index keyed by it.
    #[must_use]
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// Number of definitions in this snapshot.
    #[must_use]
    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    fn bump(&mut self) {
        self.stamp = fresh_stamp();
        self.derived = Derived::default();
    }

    /// Attach adapter state that a later build can start from (for example
    /// per-file facts, so the next snapshot re-indexes only changed files).
    /// It does not change the contents or [`Self::stamp`].
    pub fn set_carry<T: Any + Send + Sync>(&mut self, carry: Arc<T>) {
        self.carry = Some(carry);
    }

    /// The state attached by [`Self::set_carry`], if it has type `T`.
    #[must_use]
    pub fn carry<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        Arc::clone(self.carry.as_ref()?).downcast::<T>().ok()
    }

    /// Whether `self` and `other` hold the same definitions, imports, file
    /// roots, and links, in the same order. Stamps, derived data, and carry
    /// are ignored.
    #[must_use]
    pub fn same_contents(&self, other: &Self) -> bool {
        self.definitions == other.definitions
            && self.imports == other.imports
            && self.files == other.files
            && self.links == other.links
    }

    /// The value `build` derives from this context, built once per contents.
    ///
    /// Adapters keep their name index here, so it lives exactly as long as
    /// the context (per snapshot and per repository; no process-global
    /// cache). Concurrent callers wait for one build. Any edit through the
    /// `add_*` methods drops it. If an earlier call stored a different type,
    /// the value is built without being cached.
    pub fn derived<T: Any + Send + Sync>(&self, build: impl FnOnce(&Self) -> T) -> Arc<T> {
        let mut build = Some(build);
        let stored = self.derived.0.get_or_init(|| {
            let build = build.take().expect("built once");
            Arc::new(build(self))
        });
        match Arc::clone(stored).downcast::<T>() {
            Ok(value) => value,
            Err(_) => Arc::new(match build.take() {
                Some(build) => build(self),
                None => unreachable!("a value stored by this call has type T"),
            }),
        }
    }

    /// Record a definition.
    ///
    /// `module` is the module that contains the item (`crate::foo`).
    /// `simple` is its identifier (`bar`), not a path.
    /// `parent` is empty when `simple` is a name in that module's namespace;
    /// otherwise it is adapter-defined (an impl, a struct field, a `use`).
    /// `is_test` is `#[test]`; `cfg_test` is `#[cfg(test)]` on the item or
    /// an enclosing module.
    ///
    /// `node_id` may be [`NodeId::nil`] when the snapshot has not assigned
    /// identity yet. [`LangAdapter::resolve`] then skips that definition.
    #[allow(clippy::too_many_arguments)]
    pub fn add_definition(
        &mut self,
        node_id: NodeId,
        object_id: Option<ObjectId>,
        kind: NodeKind,
        module: impl Into<QualifiedName>,
        simple: impl Into<QualifiedName>,
        parent: impl Into<QualifiedName>,
        is_test: bool,
        cfg_test: bool,
    ) {
        self.bump();
        self.definitions.push(Definition {
            node_id,
            object_id,
            kind,
            module: module.into(),
            simple: simple.into(),
            parent: parent.into(),
            is_test,
            cfg_test,
        });
    }

    /// Record a `use` binding in `module`.
    ///
    /// `local` is the name brought into scope, or `*` when `glob` is set.
    /// `path` is the path as written.
    pub fn add_import(
        &mut self,
        module: impl Into<QualifiedName>,
        local: impl Into<QualifiedName>,
        path: impl Into<QualifiedName>,
        glob: bool,
    ) {
        self.bump();
        self.imports.push(Import {
            module: module.into(),
            local: local.into(),
            path: path.into(),
            glob,
        });
    }

    /// Record a file root's module, for nodes that are not themselves definitions.
    ///
    /// Prefer [`Self::add_file_at`]: without a path, two files with the same
    /// content cannot be told apart.
    pub fn add_file(&mut self, object_id: ObjectId, module: impl Into<QualifiedName>) {
        self.bump();
        self.files.push(FileRoot {
            object_id,
            path: None,
            module: module.into(),
        });
    }

    /// Record the root of the file at `path` and its module.
    pub fn add_file_at(
        &mut self,
        path: RepoPath,
        object_id: ObjectId,
        module: impl Into<QualifiedName>,
    ) {
        self.bump();
        self.files.push(FileRoot {
            object_id,
            path: Some(path),
            module: module.into(),
        });
    }

    /// Visit every definition in insertion order.
    ///
    /// Arguments are `node_id`, `object_id`, `kind`, `module`, `simple`,
    /// `parent`, `is_test`, `cfg_test`.
    pub fn for_each_definition(
        &self,
        mut visit: impl FnMut(
            NodeId,
            Option<ObjectId>,
            &NodeKind,
            &QualifiedName,
            &QualifiedName,
            &QualifiedName,
            bool,
            bool,
        ),
    ) {
        for d in &self.definitions {
            visit(
                d.node_id,
                d.object_id,
                &d.kind,
                &d.module,
                &d.simple,
                &d.parent,
                d.is_test,
                d.cfg_test,
            );
        }
    }

    /// Visit every import in insertion order.
    ///
    /// Arguments are `module`, `local`, `path`, `glob`.
    pub fn for_each_import(
        &self,
        mut visit: impl FnMut(&QualifiedName, &QualifiedName, &QualifiedName, bool),
    ) {
        for i in &self.imports {
            visit(&i.module, &i.local, &i.path, i.glob);
        }
    }

    /// Visit every file root in insertion order (`object_id`, `module`).
    pub fn for_each_file(&self, mut visit: impl FnMut(ObjectId, &QualifiedName)) {
        for f in &self.files {
            visit(f.object_id, &f.module);
        }
    }

    /// Visit every file root in insertion order (`path`, `object_id`,
    /// `module`). `path` is `None` for roots added with [`Self::add_file`].
    pub fn for_each_file_at(
        &self,
        mut visit: impl FnMut(Option<&RepoPath>, ObjectId, &QualifiedName),
    ) {
        for f in &self.files {
            visit(f.path.as_ref(), f.object_id, &f.module);
        }
    }

    /// Record that `name` in the crate `from` denotes the module `target`.
    ///
    /// Language adapters use this for a manifest link whose target source is
    /// in the snapshot (ADR 0010). `from` is that adapter's crate key.
    pub fn add_link(
        &mut self,
        from: impl Into<QualifiedName>,
        name: impl Into<QualifiedName>,
        target: impl Into<QualifiedName>,
    ) {
        self.bump();
        self.links.push(Link {
            from: from.into(),
            name: name.into(),
            target: target.into(),
        });
    }

    /// Visit every link in insertion order (`from`, `name`, `target`).
    pub fn for_each_link(
        &self,
        mut visit: impl FnMut(&QualifiedName, &QualifiedName, &QualifiedName),
    ) {
        for link in &self.links {
            visit(&link.from, &link.name, &link.target);
        }
    }
}

/// A name mentioned in a node's body (spec §4.3).
///
/// `name` is the path as written. `scope` is the module that contains the
/// mention, when the adapter knows it. `resolved` is one candidate
/// definition: [`LangAdapter::references`] sets it so an over-approximated
/// name can produce one edge per candidate. [`LangAdapter::resolve`] returns
/// `resolved` when it is set, and otherwise looks `name` up in `scope`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub struct NameRef {
    /// The name as written, not yet necessarily resolved to a [`NodeId`].
    pub name: QualifiedName,
    /// Module in which `name` was written (`crate::foo`). `None` if unknown.
    pub scope: Option<QualifiedName>,
    /// One candidate definition. `None` when the name is unresolved or not
    /// yet expanded into candidates.
    pub resolved: Option<NodeId>,
}

impl NameRef {
    /// A name with unknown scope and no candidate.
    #[must_use]
    pub fn new(name: impl Into<QualifiedName>) -> Self {
        Self {
            name: name.into(),
            scope: None,
            resolved: None,
        }
    }

    /// A name written in `scope`, bound to `resolved` when the adapter has
    /// already picked a candidate.
    #[must_use]
    pub fn at(
        name: impl Into<QualifiedName>,
        scope: Option<QualifiedName>,
        resolved: Option<NodeId>,
    ) -> Self {
        Self {
            name: name.into(),
            scope,
            resolved,
        }
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
    ///
    /// The default walks `tree` with [`NodeTree::to_bytes`].
    fn project(&self, tree: &NodeTree) -> Bytes {
        tree.to_bytes()
    }

    /// Tier 1. Which node kinds bear durable identity (spec §3.4).
    fn is_definition(&self, kind: &NodeKind) -> bool;

    /// Tier 2. Qualified name of a definition, given its ancestor chain.
    fn qualified_name(&self, path: &[&Node], node: &Node) -> Option<QualifiedName> {
        let _ = (path, node);
        None
    }

    /// Tier 2. Outgoing references from a node's body (names, not targets).
    ///
    /// The node's scope is looked up by its content. When identical
    /// definitions sit in several modules, an adapter must search every one
    /// of their scopes (a superset). Prefer [`Self::references_at`].
    fn references(&self, ctx: &ResolveCtx, node: &Node) -> Vec<NameRef> {
        let _ = (ctx, node);
        Vec::new()
    }

    /// Tier 2. Outgoing references from `node`'s body, resolved in the scope
    /// of `anchor`: the site `node` stands for. `node` supplies the text (it
    /// may be an edited version of the anchored definition). The default
    /// ignores the anchor.
    fn references_at(&self, ctx: &ResolveCtx, anchor: &Anchor, node: &Node) -> Vec<NameRef> {
        let _ = anchor;
        self.references(ctx, node)
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
