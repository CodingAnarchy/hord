//! Tier 2 Rust name resolution (spec §4.2).
//!
//! Tree-sitter only: module tree from `mod` items and file layout, `use`
//! paths, definition extraction, and reference edges by name. No trait
//! resolution, no type inference, and no macro expansion.
//!
//! A path resolves inside this crate, or across a manifest path dependency
//! whose target is in the snapshot (ADR 0010). A call with no resolved path,
//! and any selector, keeps every same-named function or method in this crate
//! and those linked packages (ADR 0011). An outer attribute is part of the
//! next definition, so names written on it are references of that definition.
//!
//! Write sets: every definition kind [`RustAdapter::is_definition`] accepts
//! gets a [`QualifiedName`] from its ancestor chain (or from the name stored
//! at parse). Read sets over-approximate: ambiguous names become one edge
//! per candidate. `#[test]` and `#[cfg(test)]` are read off attributes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use hord_core::{Node, NodeId, NodeKind, ObjectId, QualifiedName, RepoPath};
use hord_lang::{Anchor, IdentifiedTree, LangAdapter, NameRef, NodeTree, ResolveCtx};

use crate::RustAdapter;
use crate::cst::{self, is_name_container, local_name_from_raw};

/// One parsed Rust file in a crate snapshot.
pub struct RustFile<'a> {
    /// Repository path (`src/lib.rs`, `crates/foo/src/bar.rs`, …).
    pub path: &'a RepoPath,
    /// Lossless CST from [`RustAdapter::parse`](crate::RustAdapter::parse).
    pub tree: &'a NodeTree,
    /// Definition content id → durable [`NodeId`].
    ///
    /// Definitions missing from this map are stored as [`NodeId::nil`] and
    /// are not returned from [`RustAdapter::resolve`](crate::RustAdapter::resolve).
    /// Keyed by site (child-index path from the root), so identical
    /// definitions at two places keep their own ids.
    pub ids: &'a std::collections::BTreeMap<hord_lang::Site, NodeId>,
}

/// Qualified name of a definition from its ancestor chain.
///
/// Uses the name recorded at parse when present. Otherwise rebuilds the same
/// file-local path `def_name` stored: container locals (`mod`, `impl`, …)
/// joined with `::`, then this node's local name.
pub(crate) fn qualified_name(path: &[&Node], node: &Node) -> Option<QualifiedName> {
    if !RustAdapter.is_definition(&node.kind) {
        return None;
    }
    if let Some(name) = &node.name {
        return Some(name.clone());
    }
    let local = local_name_from_raw(node.kind.as_str(), node.raw.as_slice())?;
    let mut parts = Vec::new();
    for anc in path {
        if is_name_container(anc.kind.as_str())
            && let Some(piece) = local_name_from_raw(anc.kind.as_str(), anc.raw.as_slice())
        {
            parts.push(piece);
        }
    }
    parts.push(local);
    Some(QualifiedName::new(parts.join("::")))
}

/// Index `files` into a [`ResolveCtx`].
pub(crate) fn file_modules(files: &[RustFile<'_>]) -> Vec<String> {
    assign_modules(files)
        .into_iter()
        .map(|module| module.join("::"))
        .collect()
}

pub(crate) fn manifest_links(
    files: &[RustFile<'_>],
    manifests: &[crate::manifest::ManifestFile<'_>],
) -> Vec<(String, String, String)> {
    let modules = assign_modules(files);
    let mut ctx = ResolveCtx::new();
    crate::manifest::install_links(files, &modules, manifests, &mut ctx);
    let mut out = Vec::new();
    ctx.for_each_link(|from, name, target| {
        out.push((
            from.as_str().to_owned(),
            name.as_str().to_owned(),
            target.as_str().to_owned(),
        ));
    });
    out
}

pub(crate) fn resolve_context_with(
    files: &[RustFile<'_>],
    manifests: &[crate::manifest::ManifestFile<'_>],
) -> ResolveCtx {
    let modules = assign_modules(files);
    let mut ctx = ResolveCtx::new();
    for (file, module) in files.iter().zip(modules.iter()) {
        if module.is_empty() {
            continue;
        }
        index_file(file, module, &mut ctx);
    }
    crate::manifest::install_links(files, &modules, manifests, &mut ctx);
    ctx
}

/// Identifies one file's tree and ids across snapshots. At one path, equal
/// keys mean an equal parse and equal [`NodeId`]s.
type FileKey = (ObjectId, Option<ObjectId>);

impl RustAdapter {
    /// [`Self::resolve_context_with`] for a snapshot, reusing the per-file
    /// work of `prev`, the context of an earlier snapshot built by this
    /// method (perf review #1).
    ///
    /// `files` lists every candidate Rust path in snapshot order, each with
    /// a key: at one path, equal keys must mean an equal parse and equal
    /// [`NodeId`]s (hord-txn passes the blob id and the file's carried
    /// identity object). `load(path)` returns the identified tree, or `None`
    /// when the file does not parse as Rust. It is called only for a path
    /// whose key differs from `prev`'s, or whose module moved, so a landing
    /// that changed three files re-indexes three files. Manifest links are
    /// recomputed only when a manifest, a path, or a module changed.
    ///
    /// The contents equal [`Self::resolve_context_with`] over the files that
    /// parse, in `files` order ([`ResolveCtx::same_contents`]).
    pub fn resolve_context_incremental<E>(
        &self,
        prev: Option<&ResolveCtx>,
        files: &[(RepoPath, (ObjectId, Option<ObjectId>))],
        manifests: &[crate::manifest::ManifestFile<'_>],
        load: impl FnMut(&RepoPath) -> Result<Option<Arc<IdentifiedTree>>, E>,
    ) -> Result<ResolveCtx, E> {
        resolve_context_incremental(prev, files, manifests, load)
    }
}

/// Per-file results a context carries to the next snapshot's build
/// ([`ResolveCtx::set_carry`]). Trees are not kept: a file whose key and
/// module are unchanged is never loaded again.
#[derive(Default)]
struct Carry {
    files: HashMap<RepoPath, Arc<FileFacts>>,
    links: Option<Arc<LinksMemo>>,
}

struct FileFacts {
    key: FileKey,
    /// `None` when the file does not parse as Rust.
    parsed: Option<ParsedFacts>,
}

struct ParsedFacts {
    mods: Arc<[ModFound]>,
    module: Vec<String>,
    rows: Arc<FileRows>,
}

/// Inputs and output of [`crate::manifest::install_links`] for one build.
struct LinksMemo {
    paths: Vec<RepoPath>,
    modules: Vec<Vec<String>>,
    manifests: Vec<(RepoPath, Vec<u8>)>,
    links: Vec<(String, String, String)>,
}

/// A file being built: its reused facts, or its freshly loaded tree.
struct Pending {
    reused: Option<Arc<FileFacts>>,
    tree: Option<Arc<IdentifiedTree>>,
    mods: Option<Arc<[ModFound]>>,
}

/// See [`RustAdapter::resolve_context_incremental`]. The result carries its
/// own facts for the next build.
fn resolve_context_incremental<E>(
    prev: Option<&ResolveCtx>,
    files: &[(RepoPath, FileKey)],
    manifests: &[crate::manifest::ManifestFile<'_>],
    mut load: impl FnMut(&RepoPath) -> Result<Option<Arc<IdentifiedTree>>, E>,
) -> Result<ResolveCtx, E> {
    let prev = prev
        .and_then(ResolveCtx::carry::<Carry>)
        .unwrap_or_default();

    // Which files parse, and their `mod` items.
    let mut pending = Vec::new();
    let mut rust = Vec::new();
    for (i, (path, key)) in files.iter().enumerate() {
        let reused = prev.files.get(path).filter(|f| f.key == *key).cloned();
        let entry = match reused {
            Some(facts) => {
                let mods = facts.parsed.as_ref().map(|p| Arc::clone(&p.mods));
                Pending {
                    reused: Some(facts),
                    tree: None,
                    mods,
                }
            }
            None => {
                let tree = load(path)?;
                let mods = tree
                    .as_ref()
                    .map(|t| Arc::from(parse_mods(t.tree.to_bytes().as_slice())));
                Pending {
                    reused: None,
                    tree,
                    mods,
                }
            }
        };
        if entry.mods.is_some() {
            rust.push(i);
        }
        pending.push(entry);
    }

    let paths: Vec<&RepoPath> = rust.iter().map(|&i| &files[i].0).collect();
    let modules = assign_modules_by(&paths, |r| {
        Cow::Owned(
            pending[rust[r]]
                .mods
                .as_deref()
                .unwrap_or_default()
                .to_vec(),
        )
    });

    let mut ctx = ResolveCtx::new();
    let mut carry = Carry::default();
    for (r, &i) in rust.iter().enumerate() {
        let (path, key) = &files[i];
        let module = &modules[r];
        let entry = &mut pending[i];
        let facts = match entry.reused.take() {
            Some(facts) if facts.parsed.as_ref().is_some_and(|p| &p.module == module) => facts,
            _ => {
                let tree = match entry.tree.take() {
                    Some(tree) => Some(tree),
                    None => load(path)?,
                };
                let mods = entry.mods.clone().unwrap_or_else(|| Arc::from([]));
                let rows = match &tree {
                    Some(tree) if !module.is_empty() => file_rows(
                        &RustFile {
                            path,
                            tree: &tree.tree,
                            ids: &tree.ids,
                        },
                        module,
                    ),
                    _ => FileRows {
                        root: None,
                        defs: Vec::new(),
                        imports: Vec::new(),
                    },
                };
                Arc::new(FileFacts {
                    key: *key,
                    parsed: Some(ParsedFacts {
                        mods,
                        module: module.clone(),
                        rows: Arc::new(rows),
                    }),
                })
            }
        };
        if let Some(parsed) = &facts.parsed
            && !module.is_empty()
        {
            add_rows(path, &parsed.rows, &mut ctx);
        }
        carry.files.insert(path.clone(), facts);
    }
    for (i, (path, key)) in files.iter().enumerate() {
        if pending[i].mods.is_none() {
            carry.files.insert(
                path.clone(),
                Arc::new(FileFacts {
                    key: *key,
                    parsed: None,
                }),
            );
        }
    }

    let memo = links_memo(prev.links.as_ref(), &paths, &modules, manifests);
    for (from, name, target) in &memo.links {
        ctx.add_link(from.as_str(), name.as_str(), target.as_str());
    }
    carry.links = Some(memo);
    ctx.set_carry(Arc::new(carry));
    Ok(ctx)
}

/// Manifest links for `paths` in `modules`: `prev`'s when every input is
/// unchanged, else [`crate::manifest::install_links`] run again.
fn links_memo(
    prev: Option<&Arc<LinksMemo>>,
    paths: &[&RepoPath],
    modules: &[Vec<String>],
    manifests: &[crate::manifest::ManifestFile<'_>],
) -> Arc<LinksMemo> {
    if let Some(prev) = prev
        && prev.modules == modules
        && prev.paths.len() == paths.len()
        && prev.paths.iter().zip(paths).all(|(a, b)| a == *b)
        && prev.manifests.len() == manifests.len()
        && prev
            .manifests
            .iter()
            .zip(manifests)
            .all(|((path, bytes), m)| path == m.path && bytes.as_slice() == m.bytes)
    {
        return Arc::clone(prev);
    }
    // `install_links` reads only paths and modules, never the trees.
    let empty_tree = NodeTree::new();
    let empty_ids = BTreeMap::new();
    let views: Vec<RustFile<'_>> = paths
        .iter()
        .map(|path| RustFile {
            path,
            tree: &empty_tree,
            ids: &empty_ids,
        })
        .collect();
    let mut scratch = ResolveCtx::new();
    crate::manifest::install_links(&views, modules, manifests, &mut scratch);
    let mut links = Vec::new();
    scratch.for_each_link(|from, name, target| {
        links.push((
            from.as_str().to_owned(),
            name.as_str().to_owned(),
            target.as_str().to_owned(),
        ));
    });
    Arc::new(LinksMemo {
        paths: paths.iter().map(|p| (*p).clone()).collect(),
        modules: modules.to_vec(),
        manifests: manifests
            .iter()
            .map(|m| (m.path.clone(), m.bytes.to_vec()))
            .collect(),
        links,
    })
}

pub(crate) fn references(ctx: &ResolveCtx, node: &Node) -> Vec<NameRef> {
    with_index(ctx, |idx| refs_from_node(idx, node))
}

pub(crate) fn references_at(ctx: &ResolveCtx, anchor: &Anchor, node: &Node) -> Vec<NameRef> {
    with_index(ctx, |idx| {
        refs_in_scopes(idx, node, idx.scopes_at(anchor, node))
    })
}

pub(crate) fn resolve_name(ctx: &ResolveCtx, name: &NameRef) -> Option<NodeId> {
    if let Some(id) = name.resolved {
        return real_id(id);
    }
    with_index(ctx, |idx| {
        let scope = name.scope.as_ref().map(QualifiedName::as_str);
        let hits = idx.lookup(scope, None, name.name.as_str());
        let mut ids: Vec<NodeId> = hits.finals.into_iter().filter_map(real_id).collect();
        ids.sort();
        ids.dedup();
        if ids.len() == 1 { Some(ids[0]) } else { None }
    })
}

pub(crate) fn test_targets(ctx: &ResolveCtx, test: &Node) -> Vec<NodeId> {
    with_index(ctx, |idx| {
        let mut ids = Vec::new();
        if idx.is_test_node(test) {
            ids.extend(resolved_ids(&refs_from_node(idx, test)));
        }
        ids.extend(inner_test_targets(idx, test));
        let own = idx.node_ids_of(test);
        ids.retain(|id| !own.contains(id));
        ids.sort();
        ids.dedup();
        ids
    })
}

/// The name index of `ctx`, built once and kept inside the context
/// ([`ResolveCtx::derived`]), so each snapshot's context has its own and
/// nothing is shared across contexts or repositories.
fn with_index<R>(ctx: &ResolveCtx, f: impl FnOnce(&Index) -> R) -> R {
    f(&ctx.derived(Index::build))
}

#[cfg(test)]
pub(crate) fn index_builds() -> u64 {
    INDEX_BUILDS.with(std::cell::Cell::get)
}

fn real_id(id: NodeId) -> Option<NodeId> {
    (id != NodeId::nil()).then_some(id)
}

fn resolved_ids(refs: &[NameRef]) -> Vec<NodeId> {
    refs.iter()
        .filter_map(|r| r.resolved)
        .filter_map(real_id)
        .collect()
}

// --- module tree -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModFound {
    name: String,
    path_attr: Option<String>,
    inline: Vec<ModFound>,
    has_body: bool,
}

fn assign_modules(files: &[RustFile<'_>]) -> Vec<Vec<String>> {
    let paths: Vec<&RepoPath> = files.iter().map(|f| f.path).collect();
    assign_modules_by(&paths, |i| {
        Cow::Owned(parse_mods(files[i].tree.to_bytes().as_slice()))
    })
}

/// Module path of each of `paths`. `mods_of(i)` is the `mod` items of file
/// `i`; it is asked only for files reached from a crate root.
fn assign_modules_by<'m>(
    paths: &[&RepoPath],
    mut mods_of: impl FnMut(usize) -> Cow<'m, [ModFound]>,
) -> Vec<Vec<String>> {
    let mut assigned: HashMap<RepoPath, Vec<String>> = HashMap::new();
    let mut by_path: HashMap<RepoPath, usize> = HashMap::new();
    for (i, path) in paths.iter().enumerate() {
        by_path.insert((*path).clone(), i);
    }

    let mut roots: Vec<usize> = (0..paths.len())
        .filter(|i| is_crate_root(paths[*i]))
        .collect();
    if roots.is_empty() && paths.len() == 1 {
        roots.push(0);
    }
    roots.sort_by(|&a, &b| paths[a].cmp(paths[b]));
    let multi = roots.len() > 1;

    let mut queue: VecDeque<usize> = VecDeque::new();
    for idx in roots {
        let key = if multi {
            path_key(paths[idx])
        } else {
            "crate".to_owned()
        };
        if assigned.contains_key(paths[idx]) {
            continue;
        }
        assigned.insert(paths[idx].clone(), vec![key]);
        queue.push_back(idx);
    }

    while let Some(idx) = queue.pop_front() {
        let path = paths[idx].clone();
        let base = assigned.get(&path).cloned().unwrap_or_default();
        let subdir = module_subdir(paths[idx]);
        let file_dir = parent_dir(paths[idx]);
        let mods = mods_of(idx);
        link_mods(
            &mods,
            &base,
            &subdir,
            &file_dir,
            &by_path,
            &mut assigned,
            &mut queue,
        );
    }

    for path in paths {
        if assigned.contains_key(*path) {
            continue;
        }
        let key = if paths.len() == 1 {
            "crate".to_owned()
        } else {
            path_key(path)
        };
        assigned.insert((*path).clone(), vec![key]);
    }

    paths
        .iter()
        .map(|p| assigned.remove(*p).unwrap_or_default())
        .collect()
}

/// Cargo roots in one snapshot: `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`,
/// a top-level `build.rs`, and each `tests`, `examples`, or `benches` entry
/// file. With more than one root, a root's module path is its repository
/// path without the `.rs` suffix.
fn is_crate_root(path: &RepoPath) -> bool {
    let c = path.components();
    let Some(name) = c.last().map(String::as_str) else {
        return false;
    };
    let parent = c
        .len()
        .checked_sub(2)
        .and_then(|i| c.get(i))
        .map(String::as_str);
    let gparent = c
        .len()
        .checked_sub(3)
        .and_then(|i| c.get(i))
        .map(String::as_str);
    if name == "lib.rs" {
        return true;
    }
    if name == "build.rs" && parent != Some("src") {
        return true;
    }
    if name == "main.rs" {
        return matches!(
            parent,
            Some("src" | "tests" | "examples" | "benches" | "bin") | None
        ) || matches!(gparent, Some("bin" | "tests" | "examples" | "benches"));
    }
    if matches!(parent, Some("tests" | "examples" | "benches")) && name.ends_with(".rs") {
        return true;
    }
    if parent == Some("bin") && gparent == Some("src") && name.ends_with(".rs") {
        return true;
    }
    false
}

fn path_key(path: &RepoPath) -> String {
    let s = path.to_string();
    s.strip_suffix(".rs").unwrap_or(&s).to_owned()
}

fn parent_dir(path: &RepoPath) -> Vec<String> {
    let c = path.components();
    if c.len() <= 1 {
        Vec::new()
    } else {
        c[..c.len() - 1].to_vec()
    }
}

/// Directory in which this file's child `mod` items are looked up.
fn module_subdir(path: &RepoPath) -> Vec<String> {
    let c = path.components();
    let Some(name) = c.last() else {
        return Vec::new();
    };
    let parent = parent_dir(path);
    if name == "mod.rs" || name == "lib.rs" || name == "main.rs" {
        parent
    } else {
        let stem = name.trim_end_matches(".rs");
        let mut dir = parent;
        dir.push(stem.to_owned());
        dir
    }
}

fn link_mods(
    mods: &[ModFound],
    parent_module: &[String],
    parent_subdir: &[String],
    file_dir: &[String],
    by_path: &HashMap<RepoPath, usize>,
    assigned: &mut HashMap<RepoPath, Vec<String>>,
    queue: &mut VecDeque<usize>,
) {
    for m in mods {
        let mut child_mod = parent_module.to_vec();
        child_mod.push(m.name.clone());
        if m.has_body {
            let mut sub = parent_subdir.to_vec();
            sub.push(m.name.clone());
            link_mods(
                &m.inline, &child_mod, &sub, file_dir, by_path, assigned, queue,
            );
            continue;
        }
        let comps = if let Some(rel) = &m.path_attr {
            join_relative(file_dir, rel)
        } else {
            find_mod_file(by_path, parent_subdir, &m.name)
        };
        let Some(comps) = comps else {
            continue;
        };
        let path = RepoPath::new(comps);
        if assigned.contains_key(&path) {
            continue;
        }
        if let Some(&idx) = by_path.get(&path) {
            assigned.insert(path, child_mod);
            queue.push_back(idx);
        }
    }
}

fn find_mod_file(
    by_path: &HashMap<RepoPath, usize>,
    subdir: &[String],
    name: &str,
) -> Option<Vec<String>> {
    let mut rs = subdir.to_vec();
    rs.push(format!("{name}.rs"));
    if by_path.contains_key(&RepoPath::new(rs.clone())) {
        return Some(rs);
    }
    let mut modrs = subdir.to_vec();
    modrs.push(name.to_owned());
    modrs.push("mod.rs".to_owned());
    if by_path.contains_key(&RepoPath::new(modrs.clone())) {
        return Some(modrs);
    }
    None
}

fn join_relative(dir: &[String], rel: &str) -> Option<Vec<String>> {
    let mut out = dir.to_vec();
    for comp in rel.split(['/', '\\']) {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            out.pop()?;
            continue;
        }
        out.push(comp.to_owned());
    }
    Some(out)
}

fn parse_mods(source: &[u8]) -> Vec<ModFound> {
    let Ok(Some(tree)) = cst::with_parser(|parser| parser.parse(source, None)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_mods(tree.root_node(), source, &mut out);
    out
}

fn collect_mods(node: tree_sitter::Node<'_>, source: &[u8], out: &mut Vec<ModFound>) {
    let mut pending = Vec::new();
    for child in structural_children(node) {
        match child.kind() {
            "attribute_item" => pending.push(child),
            "inner_attribute_item" => {}
            "mod_item" => {
                let path_attr = path_attr_of(&pending, source);
                pending.clear();
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, source))
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let mut inline = Vec::new();
                let has_body = child.child_by_field_name("body").is_some();
                if let Some(body) = child.child_by_field_name("body") {
                    collect_mods(body, source, &mut inline);
                }
                out.push(ModFound {
                    name,
                    path_attr,
                    inline,
                    has_body,
                });
            }
            _ => pending.clear(),
        }
    }
}

// --- per-file index --------------------------------------------------------

#[derive(Clone, Debug)]
struct TsInfo {
    simple: String,
    parent: String,
    is_test: bool,
    cfg_test: bool,
}

#[derive(Clone, Debug)]
struct RawImport {
    module: String,
    local: String,
    path: String,
    glob: bool,
}

struct RawDef {
    node_id: NodeId,
    object_id: ObjectId,
    kind: NodeKind,
    module: String,
    local_name: Option<String>,
}

/// What [`index_file`] adds to a context for one file in one module.
struct FileRows {
    /// Root content id and module, when the tree has a root.
    root: Option<(ObjectId, String)>,
    defs: Vec<DefRow>,
    imports: Vec<RawImport>,
}

struct DefRow {
    node_id: NodeId,
    object_id: ObjectId,
    kind: NodeKind,
    module: String,
    simple: String,
    parent: String,
    is_test: bool,
    cfg_test: bool,
}

fn index_file(file: &RustFile<'_>, module: &[String], ctx: &mut ResolveCtx) {
    add_rows(file.path, &file_rows(file, module), ctx);
}

fn add_rows(path: &RepoPath, rows: &FileRows, ctx: &mut ResolveCtx) {
    if let Some((root, module)) = &rows.root {
        ctx.add_file_at(path.clone(), *root, module.as_str());
    }
    for def in &rows.defs {
        ctx.add_definition(
            def.node_id,
            Some(def.object_id),
            def.kind,
            def.module.as_str(),
            def.simple.as_str(),
            def.parent.as_str(),
            def.is_test,
            def.cfg_test,
        );
    }
    for import in &rows.imports {
        ctx.add_import(
            import.module.as_str(),
            import.local.as_str(),
            import.path.as_str(),
            import.glob,
        );
    }
}

fn file_rows(file: &RustFile<'_>, module: &[String]) -> FileRows {
    let bytes = file.tree.to_bytes();
    let source = bytes.as_slice();
    let (infos, imports) = collect_ts(source, module);
    let mut queues: HashMap<String, VecDeque<TsInfo>> = HashMap::new();
    for (qname, info) in infos {
        queues.entry(qname).or_default().push_back(info);
    }

    let mut raws = Vec::new();
    let mut root_row = None;
    if let Some(root) = file.tree.root() {
        root_row = Some((root, segs_join(module)));
        walk_nt(file, root, &mut Vec::new(), module, &mut raws);
    }

    let mut defs = Vec::with_capacity(raws.len());
    for raw in raws {
        let info = raw
            .local_name
            .as_ref()
            .and_then(|name| queues.get_mut(name).and_then(VecDeque::pop_front));
        let (simple, parent, is_test, cfg_test) = if let Some(info) = info {
            (info.simple, info.parent, info.is_test, info.cfg_test)
        } else {
            (
                fallback_simple(raw.kind.as_str(), raw.local_name.as_deref()),
                fallback_parent(raw.kind.as_str()),
                false,
                false,
            )
        };
        defs.push(DefRow {
            node_id: raw.node_id,
            object_id: raw.object_id,
            kind: raw.kind,
            module: raw.module,
            simple,
            parent,
            is_test,
            cfg_test,
        });
    }

    FileRows {
        root: root_row,
        defs,
        imports,
    }
}

fn walk_nt(
    file: &RustFile<'_>,
    oid: ObjectId,
    site: &mut Vec<u32>,
    module: &[String],
    out: &mut Vec<RawDef>,
) {
    let Some(node) = file.tree.get(oid) else {
        return;
    };
    if RustAdapter.is_definition(&node.kind) {
        out.push(RawDef {
            node_id: file
                .ids
                .get(site.as_slice())
                .copied()
                .unwrap_or_else(NodeId::nil),
            object_id: oid,
            kind: node.kind,
            module: segs_join(module),
            local_name: node.name.as_ref().map(|n| n.as_str().to_owned()),
        });
    }
    let mut child_mod = module.to_vec();
    if node.kind.as_str() == "mod_item"
        && let Some(name) = node
            .name
            .as_ref()
            .and_then(|n| n.as_str().rsplit("::").next())
            .filter(|s| !s.is_empty() && !s.contains(' '))
    {
        // `mod` local names are identifiers. Impl-like names contain spaces
        // and are not modules; a nested `mod` stores `outer::inner`.
        child_mod.push(name.to_owned());
    }
    for (i, child) in node.children.iter().enumerate() {
        site.push(u32::try_from(i).unwrap_or(u32::MAX));
        walk_nt(file, *child, site, &child_mod, out);
        site.pop();
    }
}

fn fallback_simple(kind: &str, local: Option<&str>) -> String {
    let Some(local) = local else {
        return String::new();
    };
    match kind {
        "use_declaration"
        | "extern_crate_declaration"
        | "inner_attribute_item"
        | "impl_item"
        | "foreign_mod_item" => String::new(),
        _ => local.rsplit("::").next().unwrap_or(local).trim().to_owned(),
    }
}

fn fallback_parent(kind: &str) -> String {
    match kind {
        "use_declaration" => "use".to_owned(),
        "extern_crate_declaration" => "extern".to_owned(),
        "inner_attribute_item" => "attr".to_owned(),
        "impl_item" => "impl-item".to_owned(),
        _ => String::new(),
    }
}

fn collect_ts(source: &[u8], module: &[String]) -> (Vec<(String, TsInfo)>, Vec<RawImport>) {
    let Ok(Some(tree)) = cst::with_parser(|parser| parser.parse(source, None)) else {
        return (Vec::new(), Vec::new());
    };
    let mut infos = Vec::new();
    let mut imports = Vec::new();
    walk_ts(
        tree.root_node(),
        source,
        module,
        &[],
        false,
        Flags::default(),
        &mut infos,
        &mut imports,
    );
    (infos, imports)
}

#[derive(Clone, Copy, Default)]
struct Flags {
    is_test: bool,
    cfg_test: bool,
}

#[derive(Clone, Debug)]
enum Frame {
    Impl { ty: String, tr: String },
    Trait(String),
    Struct(String),
    Enum(String),
    Union(String),
    Variant(String),
}

#[allow(clippy::too_many_arguments)]
fn walk_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    module: &[String],
    frames: &[Frame],
    inherited_cfg: bool,
    outer: Flags,
    infos: &mut Vec<(String, TsInfo)>,
    imports: &mut Vec<RawImport>,
) {
    let kind = node.kind();
    let cfg_test = inherited_cfg || outer.cfg_test || item_inner_cfg_test(node, source);
    let module_s = segs_join(module);

    if RustAdapter.is_definition(&NodeKind::new(kind))
        && let Some(qname) = cst::def_name(node, source)
    {
        infos.push((
            qname.as_str().to_owned(),
            TsInfo {
                simple: simple_of(node, source, kind),
                parent: parent_of(frames, kind),
                is_test: outer.is_test,
                cfg_test,
            },
        ));
    }

    if kind == "use_declaration"
        && let Some(arg) = node.child_by_field_name("argument")
    {
        expand_use(arg, source, &[], &module_s, imports);
    }
    if kind == "extern_crate_declaration" {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| node_text(n, source))
            .unwrap_or_default();
        let local = node
            .child_by_field_name("alias")
            .and_then(|n| node_text(n, source))
            .unwrap_or_else(|| name.clone());
        if !local.is_empty() && !name.is_empty() {
            imports.push(RawImport {
                module: module_s.clone(),
                local,
                path: format!("::{name}"),
                glob: false,
            });
        }
    }

    let mut child_module = module.to_vec();
    let mut child_frames = frames.to_vec();
    match kind {
        "mod_item" => {
            if let Some(name) = field_text(node, source, "name") {
                child_module.push(name);
            }
        }
        "impl_item" => child_frames.push(Frame::Impl {
            ty: type_key(&field_text(node, source, "type").unwrap_or_default()),
            tr: field_text(node, source, "trait")
                .map(|t| type_key(&t))
                .unwrap_or_default(),
        }),
        "trait_item" => {
            if let Some(name) = field_text(node, source, "name") {
                child_frames.push(Frame::Trait(name));
            }
        }
        "struct_item" => {
            if let Some(name) = field_text(node, source, "name") {
                child_frames.push(Frame::Struct(name));
            }
        }
        "enum_item" => {
            if let Some(name) = field_text(node, source, "name") {
                child_frames.push(Frame::Enum(name));
            }
        }
        "union_item" => {
            if let Some(name) = field_text(node, source, "name") {
                child_frames.push(Frame::Union(name));
            }
        }
        "enum_variant" => {
            if let Some(name) = field_text(node, source, "name") {
                child_frames.push(Frame::Variant(name));
            }
        }
        _ => {}
    }

    let mut pending = Vec::new();
    for child in structural_children(node) {
        match child.kind() {
            "attribute_item" => {
                pending.push(child);
                walk_ts(
                    child,
                    source,
                    &child_module,
                    &child_frames,
                    cfg_test,
                    Flags::default(),
                    infos,
                    imports,
                );
            }
            "inner_attribute_item" => walk_ts(
                child,
                source,
                &child_module,
                &child_frames,
                cfg_test,
                Flags::default(),
                infos,
                imports,
            ),
            _ => {
                let flags = flags_of_attrs(&pending, source);
                pending.clear();
                walk_ts(
                    child,
                    source,
                    &child_module,
                    &child_frames,
                    cfg_test,
                    flags,
                    infos,
                    imports,
                );
            }
        }
    }
}

fn simple_of(node: tree_sitter::Node<'_>, source: &[u8], kind: &str) -> String {
    if let Some(name) = field_text(node, source, "name") {
        return name;
    }
    match kind {
        "impl_item" => type_key(&field_text(node, source, "type").unwrap_or_default()),
        _ => String::new(),
    }
}

/// Link from a nested definition back to its owner.
///
/// `impl|{type}|{trait}`, `trait|{name}`, `assoc|{name}`, `field|{owner}`,
/// or `variant|{enum}`. Empty when the name lives in the module namespace.
fn parent_of(frames: &[Frame], kind: &str) -> String {
    let in_impl = matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "associated_type"
            | "macro_definition"
    );
    for frame in frames.iter().rev() {
        match frame {
            Frame::Impl { ty, tr } if in_impl => return format!("impl|{ty}|{tr}"),
            Frame::Trait(t)
                if matches!(
                    kind,
                    "function_item"
                        | "function_signature_item"
                        | "const_item"
                        | "associated_type"
                        | "type_item"
                ) =>
            {
                if matches!(kind, "associated_type" | "type_item") {
                    return format!("assoc|{t}");
                }
                return format!("trait|{t}");
            }
            Frame::Struct(s) | Frame::Union(s) if kind == "field_declaration" => {
                return format!("field|{s}");
            }
            Frame::Enum(e) if kind == "enum_variant" => return format!("variant|{e}"),
            Frame::Variant(v) if kind == "field_declaration" => return format!("field|{v}"),
            _ => {}
        }
    }
    fallback_parent(kind)
}

fn expand_use(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    prefix: &[String],
    module: &str,
    out: &mut Vec<RawImport>,
) {
    match node.kind() {
        "use_list" => {
            for child in structural_children(node) {
                if !matches!(child.kind(), "{" | "}" | ",") {
                    expand_use(child, source, prefix, module, out);
                }
            }
        }
        "scoped_use_list" => {
            let mut pre = prefix.to_vec();
            if let Some(path) = node.child_by_field_name("path") {
                pre.extend(path_segments(path, source));
            }
            if let Some(list) = node.child_by_field_name("list") {
                expand_use(list, source, &pre, module, out);
            }
        }
        "use_as_clause" => {
            let Some(path) = node.child_by_field_name("path") else {
                return;
            };
            let Some(alias) = node.child_by_field_name("alias") else {
                return;
            };
            let mut segs = prefix.to_vec();
            segs.extend(path_segments(path, source));
            let Some(local) = node_text(alias, source) else {
                return;
            };
            out.push(RawImport {
                module: module.to_owned(),
                local,
                path: segs_join(&segs),
                glob: false,
            });
        }
        "use_wildcard" => {
            let mut segs = prefix.to_vec();
            for child in structural_children(node) {
                if is_path_kind(child.kind()) {
                    segs.extend(path_segments(child, source));
                }
            }
            out.push(RawImport {
                module: module.to_owned(),
                local: "*".to_owned(),
                path: segs_join(&segs),
                glob: true,
            });
        }
        kind if is_path_kind(kind) => {
            let mut segs = prefix.to_vec();
            segs.extend(path_segments(node, source));
            let Some(local) = segs.last().cloned() else {
                return;
            };
            if local.is_empty() || local == "*" {
                return;
            }
            out.push(RawImport {
                module: module.to_owned(),
                local,
                path: segs_join(&segs),
                glob: false,
            });
        }
        _ => {}
    }
}

// --- attributes ------------------------------------------------------------

fn flags_of_attrs(attrs: &[tree_sitter::Node<'_>], source: &[u8]) -> Flags {
    let mut flags = Flags::default();
    for attr in attrs {
        if attr_is_test(*attr, source) {
            flags.is_test = true;
        }
        if attr_is_cfg_test(*attr, source) {
            flags.cfg_test = true;
        }
    }
    flags
}

fn attr_is_test(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let segs = attribute_path(attr_item, source);
    segs.last().is_some_and(|s| s == "test")
}

fn attr_is_cfg_test(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let segs = attribute_path(attr_item, source);
    if segs != ["cfg"] {
        return false;
    }
    let Some(attr) = attribute_body(attr_item) else {
        return false;
    };
    let Some(args) = attr.child_by_field_name("arguments") else {
        return false;
    };
    cfg_tree_is_test(args, source)
}

fn is_inner_cfg_test(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    node.kind() == "inner_attribute_item" && attr_is_cfg_test(node, source)
}

fn direct_inner_cfg_test(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    structural_children(node)
        .into_iter()
        .any(|child| is_inner_cfg_test(child, source))
}

/// `#![cfg(test)]` attached to `node`.
///
/// The attribute is a direct child of `source_file`, or a direct child of the
/// item body (`declaration_list` for modules, `block` for functions).
fn item_inner_cfg_test(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    if is_inner_cfg_test(node, source) || direct_inner_cfg_test(node, source) {
        return true;
    }
    if let Some(body) = node.child_by_field_name("body")
        && direct_inner_cfg_test(body, source)
    {
        return true;
    }
    structural_children(node).into_iter().any(|child| {
        let body_list = matches!(child.kind(), "declaration_list" | "block")
            || child.child_by_field_name("body").is_some();
        body_list && direct_inner_cfg_test(child, source)
    })
}

fn cfg_tree_is_test(tree: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let kids = structural_children(tree);
    let inner = trim_delims(&kids);
    eval_cfg(&inner, source)
}

fn eval_cfg(nodes: &[tree_sitter::Node<'_>], source: &[u8]) -> bool {
    if nodes.is_empty() {
        return false;
    }
    if nodes.len() == 1 && nodes[0].kind() == "identifier" {
        return node_text(nodes[0], source).as_deref() == Some("test");
    }
    let Some(head) = nodes
        .iter()
        .find(|n| n.kind() == "identifier")
        .and_then(|n| node_text(*n, source))
    else {
        return nodes
            .iter()
            .any(|n| n.kind() == "token_tree" && cfg_tree_is_test(*n, source));
    };
    // `not` is not unfolded: `cfg(not(test))` is never test code.
    // `all` counts when any argument mentions `test`, same as `any`.
    if head == "not" {
        return false;
    }
    if (head == "any" || head == "all")
        && let Some(group) = nodes.iter().find(|n| n.kind() == "token_tree")
    {
        return split_comma(*group)
            .into_iter()
            .any(|part| eval_cfg(&part, source));
    }
    nodes.iter().any(|n| match n.kind() {
        "identifier" => node_text(*n, source).as_deref() == Some("test"),
        "token_tree" => cfg_tree_is_test(*n, source),
        _ => false,
    })
}

fn split_comma(group: tree_sitter::Node<'_>) -> Vec<Vec<tree_sitter::Node<'_>>> {
    let kids = structural_children(group);
    let inner = trim_delims(&kids);
    let mut parts = Vec::new();
    let mut cur = Vec::new();
    for n in inner {
        if n.kind() == "," {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(n);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn trim_delims<'a>(nodes: &[tree_sitter::Node<'a>]) -> Vec<tree_sitter::Node<'a>> {
    let mut nodes = nodes.to_vec();
    if nodes
        .first()
        .is_some_and(|n| matches!(n.kind(), "(" | "[" | "{"))
    {
        nodes.remove(0);
    }
    if nodes
        .last()
        .is_some_and(|n| matches!(n.kind(), ")" | "]" | "}"))
    {
        nodes.pop();
    }
    nodes
}

fn path_attr_of(attrs: &[tree_sitter::Node<'_>], source: &[u8]) -> Option<String> {
    for attr in attrs {
        if attribute_path(*attr, source) == ["path"] {
            let body = attribute_body(*attr)?;
            let value = body.child_by_field_name("value")?;
            return unquote(value, source);
        }
    }
    None
}

fn attribute_body(attr_item: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    structural_children(attr_item)
        .into_iter()
        .find(|n| n.kind() == "attribute")
}

fn attribute_path(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(attr) = attribute_body(attr_item) else {
        return Vec::new();
    };
    for child in structural_children(attr) {
        if child.kind() == "=" || child.kind() == "token_tree" {
            break;
        }
        if is_path_kind(child.kind()) {
            return path_segments(child, source);
        }
    }
    Vec::new()
}

/// Contents of a `#[path = …]` string: `"…"`, `b"…"`, `r"…"`, `r#"…"#`, `br#"…"#`.
fn unquote(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text = node_text(node, source)?;
    rust_string_contents(text.trim())
}

fn rust_string_contents(text: &str) -> Option<String> {
    let rest = text.strip_prefix('b').unwrap_or(text);
    if let Some(rest) = rest.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        let rest = rest.get(hashes..)?.strip_prefix('"')?;
        let suffix_len = 1 + hashes;
        let (body, suffix) = rest.split_at_checked(rest.len().checked_sub(suffix_len)?)?;
        if suffix.starts_with('"') && suffix[1..].bytes().all(|b| b == b'#') {
            return Some(body.to_owned());
        }
        return None;
    }
    if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
        return Some(rest[1..rest.len() - 1].to_owned());
    }
    None
}

// --- paths -----------------------------------------------------------------

fn path_segments(node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    match node.kind() {
        "scoped_identifier" | "scoped_type_identifier" => {
            let mut segs = Vec::new();
            if let Some(path) = node.child_by_field_name("path") {
                segs.extend(path_segments(path, source));
            } else if node_text(node, source).is_some_and(|t| t.trim_start().starts_with("::")) {
                segs.push("::".to_owned());
            }
            if let Some(name) = node.child_by_field_name("name")
                && let Some(text) = node_text(name, source)
            {
                segs.push(text);
            }
            segs
        }
        "generic_type" | "generic_type_with_turbofish" => node
            .child_by_field_name("type")
            .map(|n| path_segments(n, source))
            .unwrap_or_default(),
        "generic_function" => node
            .child_by_field_name("function")
            .map(|n| path_segments(n, source))
            .unwrap_or_default(),
        "identifier" | "type_identifier" | "field_identifier" => {
            node_text(node, source).into_iter().collect()
        }
        "crate" => vec!["crate".to_owned()],
        "self" => vec!["self".to_owned()],
        "super" => vec!["super".to_owned()],
        "scoped_use_list" => {
            let mut segs = Vec::new();
            if let Some(path) = node.child_by_field_name("path") {
                segs.extend(path_segments(path, source));
            }
            segs
        }
        _ => Vec::new(),
    }
}

fn is_path_kind(kind: &str) -> bool {
    matches!(
        kind,
        "scoped_identifier"
            | "scoped_type_identifier"
            | "identifier"
            | "type_identifier"
            | "crate"
            | "self"
            | "super"
            | "generic_type"
            | "generic_type_with_turbofish"
            | "generic_function"
    )
}

fn type_key(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = collapsed.trim();
    if let Some(rest) = s.strip_prefix('&') {
        s = rest.trim();
        if let Some(rest) = s.strip_prefix('\'') {
            s = rest
                .split_once(|c: char| c.is_whitespace())
                .map(|(_, r)| r.trim())
                .unwrap_or(rest);
        }
        s = s.trim_start_matches("mut ").trim();
    }
    s = s
        .trim_start_matches("dyn ")
        .trim_start_matches("impl ")
        .trim();
    if let Some(i) = s.find(['<', ' ']) {
        s = s[..i].trim();
    }
    s.rsplit("::").next().unwrap_or(s).trim().to_owned()
}

fn segs_join(segs: &[String]) -> String {
    segs.join("::")
}

fn mod_join(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn crate_key(module: &str) -> &str {
    module.split("::").next().unwrap_or(module)
}

fn parent_module(module: &str) -> Option<String> {
    let mut segs: Vec<&str> = module.split("::").collect();
    if segs.len() <= 1 {
        return None;
    }
    segs.pop();
    Some(segs.join("::"))
}

// --- resolution index ------------------------------------------------------

struct Def {
    node_id: NodeId,
    kind: String,
    module: String,
    simple: String,
    parent: String,
    is_test: bool,
    cfg_test: bool,
}

/// One import; [`Index::named_imps`] and [`Index::glob_imps`] hold its
/// binding name and glob flag.
struct Imp {
    module: String,
    path: String,
    done: bool,
    external: bool,
    def_indices: Vec<usize>,
    target_module: Option<String>,
}

struct Index {
    defs: Vec<Def>,
    imps: Vec<Imp>,
    /// `(module, local)` → non-glob import indices, ascending.
    named_imps: HashMap<(String, String), Vec<usize>>,
    /// Module → glob import indices, ascending.
    glob_imps: HashMap<String, Vec<usize>>,
    /// `(module, simple)` → direct (parent-empty) definition indices.
    direct: HashMap<(String, String), Vec<usize>>,
    /// Simple name → associated definition indices.
    associated: HashMap<String, Vec<usize>>,
    /// Simple name → function and method indices (bare-call candidates).
    callables: HashMap<String, Vec<usize>>,
    /// Content id → every definition with that content. Identical
    /// definitions in several modules share one content id.
    by_object: HashMap<ObjectId, Vec<usize>>,
    /// [`NodeId`] → definition: one per site.
    by_node: HashMap<NodeId, usize>,
    /// File root content id → modules of every file with that content.
    files: HashMap<ObjectId, Vec<String>>,
    /// File path → module.
    files_by_path: HashMap<RepoPath, String>,
    crate_keys: BTreeSet<String>,
    /// Crate key → (extern name, target module).
    externs: HashMap<String, Vec<(String, String)>>,
}

struct PathHits {
    all: Vec<NodeId>,
    finals: Vec<NodeId>,
}

#[derive(Clone, Debug)]
struct Hit {
    node_id: Option<NodeId>,
    /// Module path, or the crate key when `kind` is [`HitKind::Type`].
    module: String,
    def_index: Option<usize>,
    kind: HitKind,
    /// Set for [`HitKind::Type`]: the type `Self` names (`Point`, or a trait).
    type_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HitKind {
    Module,
    Def,
    Type,
}

enum Head {
    Ready(Vec<Hit>),
    Pending,
    Missing,
}

#[cfg(test)]
thread_local! {
    static INDEX_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Index {
    fn build(ctx: &ResolveCtx) -> Self {
        #[cfg(test)]
        INDEX_BUILDS.with(|n| n.set(n.get() + 1));
        let mut idx = Self {
            defs: Vec::new(),
            imps: Vec::new(),
            named_imps: HashMap::new(),
            glob_imps: HashMap::new(),
            direct: HashMap::new(),
            associated: HashMap::new(),
            callables: HashMap::new(),
            by_object: HashMap::new(),
            by_node: HashMap::new(),
            files: HashMap::new(),
            files_by_path: HashMap::new(),
            crate_keys: BTreeSet::new(),
            externs: HashMap::new(),
        };
        ctx.for_each_link(|from, name, target| {
            idx.externs
                .entry(from.as_str().to_owned())
                .or_default()
                .push((name.as_str().to_owned(), target.as_str().to_owned()));
        });
        ctx.for_each_definition(
            |node_id, object_id, kind, module, simple, parent, is_test, cfg_test| {
                let i = idx.defs.len();
                let module = module.as_str().to_owned();
                let simple = simple.as_str().to_owned();
                let parent = parent.as_str().to_owned();
                let kind_s = kind.as_str().to_owned();
                if !module.is_empty() {
                    idx.crate_keys.insert(crate_key(&module).to_owned());
                }
                if parent.is_empty() && !simple.is_empty() {
                    idx.direct
                        .entry((module.clone(), simple.clone()))
                        .or_default()
                        .push(i);
                } else if !simple.is_empty() && is_associated_kind(&kind_s) {
                    idx.associated.entry(simple.clone()).or_default().push(i);
                }
                if is_fn_kind(&kind_s) && !simple.is_empty() {
                    idx.callables.entry(simple.clone()).or_default().push(i);
                }
                if let Some(oid) = object_id {
                    idx.by_object.entry(oid).or_default().push(i);
                }
                if real_id(node_id).is_some() {
                    idx.by_node.insert(node_id, i);
                }
                idx.defs.push(Def {
                    node_id,
                    kind: kind_s,
                    module,
                    simple,
                    parent,
                    is_test,
                    cfg_test,
                });
            },
        );
        ctx.for_each_import(|module, local, path, glob| {
            let i = idx.imps.len();
            if glob {
                idx.glob_imps
                    .entry(module.as_str().to_owned())
                    .or_default()
                    .push(i);
            } else {
                idx.named_imps
                    .entry((module.as_str().to_owned(), local.as_str().to_owned()))
                    .or_default()
                    .push(i);
            }
            idx.imps.push(Imp {
                module: module.as_str().to_owned(),
                path: path.as_str().to_owned(),
                done: false,
                external: false,
                def_indices: Vec::new(),
                target_module: None,
            });
        });
        ctx.for_each_file_at(|path, oid, module| {
            let module = module.as_str().to_owned();
            if !module.is_empty() {
                idx.crate_keys.insert(crate_key(&module).to_owned());
            }
            if let Some(path) = path {
                idx.files_by_path.insert(path.clone(), module.clone());
            }
            let modules = idx.files.entry(oid).or_default();
            if !modules.contains(&module) {
                modules.push(module);
            }
        });
        idx.resolve_imports();
        idx
    }

    fn resolve_imports(&mut self) {
        for _ in 0..=self.imps.len() {
            let mut updates = Vec::new();
            for (i, imp) in self.imps.iter().enumerate() {
                if imp.done {
                    continue;
                }
                if let Some(res) = self.attempt_import(imp) {
                    updates.push((i, res));
                }
            }
            if updates.is_empty() {
                break;
            }
            for (i, res) in updates {
                let imp = &mut self.imps[i];
                imp.done = true;
                imp.external = res.external;
                imp.def_indices = res.def_indices;
                imp.target_module = res.target_module;
            }
        }
        for imp in &mut self.imps {
            if !imp.done {
                imp.done = true;
                imp.external = true;
            }
        }
    }

    fn attempt_import(&self, imp: &Imp) -> Option<Resolution> {
        if imp.path.starts_with("::") {
            return Some(Resolution::external());
        }
        let segs = split_path(&imp.path);
        if segs.is_empty() {
            return Some(Resolution::external());
        }
        let walked = self.walk_import(&imp.module, &segs)?;
        if walked.def_indices.is_empty() && walked.module.is_none() {
            return Some(Resolution::external());
        }
        Some(Resolution {
            external: false,
            def_indices: walked.def_indices,
            target_module: walked.module,
        })
    }

    /// `None` means a segment is waiting on an import that is not done yet.
    fn walk_import(&self, module: &str, segs: &[String]) -> Option<ImportWalk> {
        let mut hits = Vec::new();
        for (n, seg) in segs.iter().enumerate() {
            let head = if n == 0 {
                self.head_status(module, seg, None)?
            } else {
                let mut next = Vec::new();
                let mut pending = false;
                for hit in &hits {
                    match self.step_status(hit, seg) {
                        Head::Pending => pending = true,
                        Head::Ready(h) => next.extend(h),
                        Head::Missing => {}
                    }
                }
                if pending && next.is_empty() {
                    return None;
                }
                Head::Ready(next)
            };
            match head {
                Head::Pending => return None,
                Head::Missing => {
                    return Some(ImportWalk {
                        def_indices: Vec::new(),
                        module: None,
                    });
                }
                Head::Ready(ready) => {
                    hits = ready;
                }
            }
        }
        let mut def_indices = Vec::new();
        let mut mod_path = None;
        for hit in &hits {
            if let Some(i) = hit.def_index {
                def_indices.push(i);
            }
            if hit.kind == HitKind::Module {
                mod_path = Some(hit.module.clone());
            }
        }
        Some(ImportWalk {
            def_indices,
            module: mod_path,
        })
    }

    fn head_status(&self, module: &str, seg: &str, self_ty: Option<&str>) -> Option<Head> {
        match seg {
            "crate" => Some(Head::Ready(vec![self.module_hit(crate_key(module))])),
            "self" => Some(Head::Ready(vec![self.module_hit(module)])),
            "super" => Some(match parent_module(module) {
                Some(p) => Head::Ready(vec![self.module_hit(&p)]),
                None => Head::Missing,
            }),
            "::" => Some(Head::Missing),
            "Self" => Some(Head::Ready(self.self_hits(module, self_ty))),
            _ => Some(self.ident_status(module, seg)),
        }
    }

    /// Non-glob imports of `name` in `module`, in declaration order.
    fn named_imports(&self, module: &str, name: &str) -> impl Iterator<Item = &Imp> + '_ {
        self.named_imps
            .get(&(module.to_owned(), name.to_owned()))
            .into_iter()
            .flatten()
            .map(|&i| &self.imps[i])
    }

    /// Glob imports in `module`, in declaration order.
    fn glob_imports(&self, module: &str) -> impl Iterator<Item = &Imp> + '_ {
        self.glob_imps
            .get(module)
            .into_iter()
            .flatten()
            .map(|&i| &self.imps[i])
    }

    fn ident_status(&self, module: &str, name: &str) -> Head {
        let key = (module.to_owned(), name.to_owned());
        let locals = self.direct.get(&key);
        let mut ready = Vec::new();
        let mut pending = false;
        let mut external = false;
        for imp in self.named_imports(module, name) {
            if !imp.done {
                pending = true;
                continue;
            }
            if imp.external {
                external = true;
                continue;
            }
            ready.extend(self.hits_from_import(imp));
        }
        if let Some(ids) = locals {
            for &i in ids {
                ready.push(self.hit_from_def(i));
            }
        }
        if !ready.is_empty() {
            return Head::Ready(ready);
        }
        if pending {
            return Head::Pending;
        }
        if self.glob_imports(module).any(|i| !i.done) {
            return Head::Pending;
        }
        if let Some(links) = self.externs.get(crate_key(module)) {
            let mut hits = Vec::new();
            for (extern_name, target) in links {
                if extern_name == name {
                    hits.push(self.module_hit(target));
                }
            }
            if !hits.is_empty() {
                return Head::Ready(hits);
            }
        }
        if external {
            return Head::Missing;
        }
        let mut glob_hits = Vec::new();
        for imp in self.glob_imports(module).filter(|i| i.done) {
            glob_hits.extend(self.glob_hits(imp, name));
        }
        if glob_hits.is_empty() {
            Head::Missing
        } else {
            Head::Ready(glob_hits)
        }
    }

    fn hits_from_import(&self, imp: &Imp) -> Vec<Hit> {
        let mut hits = Vec::new();
        for &i in &imp.def_indices {
            hits.push(self.hit_from_def(i));
        }
        if hits.is_empty()
            && let Some(m) = &imp.target_module
        {
            hits.push(self.module_hit(m));
        }
        hits
    }

    fn glob_hits(&self, imp: &Imp, name: &str) -> Vec<Hit> {
        let Some(module) = imp.target_module.as_deref() else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        if let Some(ids) = self.direct.get(&(module.to_owned(), name.to_owned())) {
            for &i in ids {
                hits.push(self.hit_from_def(i));
            }
        }
        for other in self
            .named_imports(module, name)
            .filter(|i| i.done && !i.external)
        {
            hits.extend(self.hits_from_import(other));
        }
        hits
    }

    fn step_status(&self, hit: &Hit, seg: &str) -> Head {
        match hit.kind {
            HitKind::Module => self.ident_status(&hit.module, seg),
            HitKind::Def => {
                let Some(i) = hit.def_index else {
                    return Head::Missing;
                };
                let def = &self.defs[i];
                let from_trait = def.kind == "trait_item";
                let key = def.simple.clone();
                let crate_k = crate_key(&def.module).to_owned();
                let hits = self.associated_hits(&crate_k, &key, seg, from_trait);
                if hits.is_empty() {
                    Head::Missing
                } else {
                    Head::Ready(hits)
                }
            }
            HitKind::Type => {
                let key = hit.type_name.as_deref().unwrap_or("");
                let hits = self.associated_hits(&hit.module, key, seg, false);
                if hits.is_empty() {
                    Head::Missing
                } else {
                    Head::Ready(hits)
                }
            }
        }
    }

    fn associated_hits(
        &self,
        crate_k: &str,
        type_key: &str,
        seg: &str,
        from_trait: bool,
    ) -> Vec<Hit> {
        let Some(ids) = self.associated.get(seg) else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        for &i in ids {
            let def = &self.defs[i];
            if crate_key(&def.module) != crate_k {
                continue;
            }
            if parent_matches(&def.parent, type_key, from_trait) {
                hits.push(self.hit_from_def(i));
            }
        }
        hits
    }

    fn self_hits(&self, module: &str, self_ty: Option<&str>) -> Vec<Hit> {
        let Some(ty) = self_ty.filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        if let Some(ids) = self.direct.get(&(module.to_owned(), ty.to_owned())) {
            for &i in ids {
                hits.push(self.hit_from_def(i));
            }
        }
        if hits.is_empty() {
            let key = crate_key(module);
            for (i, def) in self.defs.iter().enumerate() {
                if def.parent.is_empty() && def.simple == ty && crate_key(&def.module) == key {
                    hits.push(self.hit_from_def(i));
                }
            }
        }
        // Type hit: `Self::item` matches associated items of `ty` with no NodeId of its own.
        hits.push(Hit {
            node_id: None,
            module: crate_key(module).to_owned(),
            def_index: None,
            kind: HitKind::Type,
            type_name: Some(ty.to_owned()),
        });
        hits
    }

    fn module_hit(&self, module: &str) -> Hit {
        // A `mod` item whose denoted path is `module`, if one exists.
        if let Some(parent) = parent_module(module) {
            let simple = module.rsplit("::").next().unwrap_or(module);
            if let Some(ids) = self.direct.get(&(parent, simple.to_owned())) {
                for &i in ids {
                    if self.defs[i].kind == "mod_item" {
                        return self.hit_from_def(i);
                    }
                }
            }
        }
        Hit {
            node_id: None,
            module: module.to_owned(),
            def_index: None,
            kind: HitKind::Module,
            type_name: None,
        }
    }

    fn hit_from_def(&self, i: usize) -> Hit {
        let def = &self.defs[i];
        let node_id = real_id(def.node_id);
        if def.kind == "mod_item" {
            Hit {
                node_id,
                module: mod_join(&def.module, &def.simple),
                def_index: Some(i),
                kind: HitKind::Module,
                type_name: None,
            }
        } else {
            Hit {
                node_id,
                module: def.module.clone(),
                def_index: Some(i),
                kind: HitKind::Def,
                type_name: None,
            }
        }
    }

    fn lookup(&self, scope: Option<&str>, self_ty: Option<&str>, written: &str) -> PathHits {
        let scope_owned;
        let module = if let Some(scope) = scope.filter(|s| !s.is_empty()) {
            scope
        } else if self.crate_keys.len() == 1 {
            scope_owned = self.crate_keys.iter().next().cloned().unwrap_or_default();
            scope_owned.as_str()
        } else {
            return self.lookup_everywhere(written);
        };
        self.lookup_in(module, self_ty, written)
    }

    fn lookup_everywhere(&self, written: &str) -> PathHits {
        let mut all = Vec::new();
        let mut finals = Vec::new();
        let modules: BTreeSet<String> = self
            .defs
            .iter()
            .map(|d| d.module.clone())
            .chain(self.files.values().flatten().cloned())
            .collect();
        for module in modules {
            let hits = self.lookup_in(&module, None, written);
            all.extend(hits.all);
            finals.extend(hits.finals);
        }
        dedup_ids(&mut all);
        dedup_ids(&mut finals);
        PathHits { all, finals }
    }

    fn lookup_in(&self, module: &str, self_ty: Option<&str>, written: &str) -> PathHits {
        let written = written.trim();
        if let Some(name) = written.strip_prefix('.') {
            let ids = self.method_ids(module, name.trim());
            return PathHits {
                all: ids.clone(),
                finals: ids,
            };
        }
        if written.starts_with("::") {
            return PathHits {
                all: Vec::new(),
                finals: Vec::new(),
            };
        }
        let segs: Vec<String> = written
            .split("::")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if segs.is_empty() {
            return PathHits {
                all: Vec::new(),
                finals: Vec::new(),
            };
        }
        let mut all = Vec::new();
        let mut current = match self.head_status(module, &segs[0], self_ty) {
            Some(Head::Ready(hits)) => hits,
            _ => Vec::new(),
        };
        let first_ids = hit_ids(&current);
        if segs.len() == 1 {
            return PathHits {
                all: first_ids.clone(),
                finals: first_ids,
            };
        }
        all.extend(first_ids);
        for seg in segs.iter().skip(1) {
            let mut next = Vec::new();
            for hit in &current {
                if let Head::Ready(hits) = self.step_status(hit, seg) {
                    next.extend(hits);
                }
            }
            current = next;
            all.extend(hit_ids(&current));
        }
        dedup_ids(&mut all);
        PathHits {
            all,
            finals: hit_ids(&current),
        }
    }

    fn visible_crates(&self, module: &str) -> Vec<String> {
        let mut keys = vec![crate_key(module).to_owned()];
        if let Some(links) = self.externs.get(crate_key(module)) {
            for (_, target) in links {
                let key = crate_key(target).to_owned();
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    fn method_ids(&self, module: &str, name: &str) -> Vec<NodeId> {
        let Some(ids) = self.associated.get(name) else {
            return Vec::new();
        };
        let crates = self.visible_crates(module);
        let mut out = Vec::new();
        for &i in ids {
            let def = &self.defs[i];
            if !crates.iter().any(|key| crate_key(&def.module) == key) {
                continue;
            }
            if is_method_parent(&def.parent)
                && let Some(id) = real_id(def.node_id)
            {
                out.push(id);
            }
        }
        dedup_ids(&mut out);
        out
    }

    /// Functions and methods named `name` in this crate or a linked package.
    ///
    /// A bare call has no type to pick one of them (ADR 0011).
    fn callable_ids(&self, module: &str, name: &str) -> Vec<NodeId> {
        let Some(ids) = self.callables.get(name) else {
            return Vec::new();
        };
        let crates = self.visible_crates(module);
        let mut out = Vec::new();
        for &i in ids {
            let def = &self.defs[i];
            if !crates.iter().any(|key| crate_key(&def.module) == key) {
                continue;
            }
            if let Some(id) = real_id(def.node_id) {
                out.push(id);
            }
        }
        dedup_ids(&mut out);
        out
    }

    /// Every definition with `node`'s content (several when identical
    /// definitions sit in different modules).
    fn defs_of(&self, node: &Node) -> &[usize] {
        node.content_id()
            .ok()
            .and_then(|oid| self.by_object.get(&oid))
            .map_or(&[], Vec::as_slice)
    }

    /// Ids of every definition with `node`'s content.
    fn node_ids_of(&self, node: &Node) -> Vec<NodeId> {
        self.defs_of(node)
            .iter()
            .filter_map(|i| real_id(self.defs[*i].node_id))
            .collect()
    }

    fn is_test_node(&self, node: &Node) -> bool {
        self.defs_of(node).iter().any(|i| {
            let def = &self.defs[*i];
            def.is_test || (def.cfg_test && is_fn_kind(&def.kind))
        })
    }

    fn def_scope(&self, i: usize) -> Scope {
        let def = &self.defs[i];
        Scope {
            module: def.module.clone(),
            self_ty: self_type_of(&def.parent),
            cfg_test: def.cfg_test,
        }
    }

    fn module_scope(module: &str) -> Scope {
        Scope {
            module: module.to_owned(),
            self_ty: None,
            cfg_test: false,
        }
    }

    fn fallback_scope(&self) -> Scope {
        if self.crate_keys.len() == 1 {
            return Self::module_scope(&self.crate_keys.iter().next().cloned().unwrap_or_default());
        }
        Self::module_scope("")
    }

    /// Scopes `node` may sit in, found by content: one per distinct scope of
    /// the definitions (or file roots) with that content. Identical
    /// definitions in two modules give two scopes.
    fn scopes_of(&self, node: &Node) -> Vec<Scope> {
        let mut out: Vec<Scope> = Vec::new();
        for i in self.defs_of(node) {
            let scope = self.def_scope(*i);
            if !out.iter().any(|s| s.same_as(&scope)) {
                out.push(scope);
            }
        }
        if out.is_empty()
            && let Ok(oid) = node.content_id()
            && let Some(modules) = self.files.get(&oid)
        {
            out.extend(modules.iter().map(|m| Self::module_scope(m)));
        }
        if out.is_empty() {
            out.push(self.fallback_scope());
        }
        out
    }

    /// The scope of the site `anchor` names, falling back to a content
    /// lookup when the anchor is unknown to this context.
    fn scopes_at(&self, anchor: &Anchor, node: &Node) -> Vec<Scope> {
        match anchor {
            Anchor::Definition(id) => {
                if let Some(i) = self.by_node.get(id) {
                    return vec![self.def_scope(*i)];
                }
            }
            Anchor::File(path) => {
                if let Some(module) = self.files_by_path.get(path) {
                    return vec![Self::module_scope(module)];
                }
            }
            _ => {}
        }
        self.scopes_of(node)
    }
}

struct Resolution {
    external: bool,
    def_indices: Vec<usize>,
    target_module: Option<String>,
}

impl Resolution {
    fn external() -> Self {
        Self {
            external: true,
            def_indices: Vec::new(),
            target_module: None,
        }
    }
}

struct ImportWalk {
    def_indices: Vec<usize>,
    module: Option<String>,
}

struct Scope {
    module: String,
    self_ty: Option<String>,
    cfg_test: bool,
}

impl Scope {
    fn same_as(&self, other: &Self) -> bool {
        self.module == other.module && self.self_ty == other.self_ty
    }
}

/// Type that `Self` names in `parent`.
///
/// `impl|{type}|{trait}` uses the type. `trait|{name}` and `assoc|{name}`
/// use that trait, so `Self::item` inside the trait sees its associated items.
fn self_type_of(parent: &str) -> Option<String> {
    if let Some((ty, _)) = split_impl(parent) {
        return (!ty.is_empty()).then(|| ty.to_owned());
    }
    for prefix in ["trait|", "assoc|"] {
        if let Some(name) = parent.strip_prefix(prefix)
            && !name.is_empty()
            && !name.contains('|')
        {
            return Some(name.to_owned());
        }
    }
    None
}

fn is_method_parent(parent: &str) -> bool {
    parent.starts_with("impl|")
        || parent.starts_with("trait|")
        || parent.starts_with("field|")
        || parent.starts_with("assoc|")
        || parent.starts_with("variant|")
}

fn parent_matches(parent: &str, key: &str, from_trait: bool) -> bool {
    if let Some((ty, tr)) = split_impl(parent) {
        if ty == key {
            return true;
        }
        if from_trait && tr == key {
            return true;
        }
        return false;
    }
    parent == format!("field|{key}")
        || parent == format!("variant|{key}")
        || parent == format!("trait|{key}")
        || parent == format!("assoc|{key}")
}

fn split_impl(parent: &str) -> Option<(&str, &str)> {
    let rest = parent.strip_prefix("impl|")?;
    rest.split_once('|')
}

fn is_associated_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "associated_type"
            | "macro_definition"
            | "field_declaration"
            | "enum_variant"
    )
}

fn is_fn_kind(kind: &str) -> bool {
    matches!(kind, "function_item" | "function_signature_item")
}

fn split_path(path: &str) -> Vec<String> {
    path.split("::")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn hit_ids(hits: &[Hit]) -> Vec<NodeId> {
    let mut ids = Vec::new();
    for hit in hits {
        if let Some(id) = hit.node_id {
            ids.push(id);
        }
    }
    dedup_ids(&mut ids);
    ids
}

fn dedup_ids(ids: &mut Vec<NodeId>) {
    ids.sort();
    ids.dedup();
}

// --- reference walk --------------------------------------------------------

/// References of `node`, found by content: every scope its content may sit
/// in is searched (see [`Index::scopes_of`]).
fn refs_from_node(idx: &Index, node: &Node) -> Vec<NameRef> {
    refs_in_scopes(idx, node, idx.scopes_of(node))
}

fn refs_in_scopes(idx: &Index, node: &Node, scopes: Vec<Scope>) -> Vec<NameRef> {
    let bytes = node.raw.clone();
    let mut refs = Vec::new();
    let _ = cst::with_parser(|parser| {
        if let Some(tree) = parser.parse(bytes.as_slice(), None) {
            for scope in scopes {
                let mut walk = RefWalk {
                    idx,
                    source: bytes.as_slice(),
                    module: scope.module,
                    self_ty: scope.self_ty,
                    refs: Vec::new(),
                };
                walk.walk(tree.root_node());
                refs.extend(walk.refs);
            }
        }
    });
    refs.sort_by(|a, b| {
        a.name
            .as_str()
            .cmp(b.name.as_str())
            .then(a.resolved.cmp(&b.resolved))
            .then(a.scope.cmp(&b.scope))
    });
    refs.dedup();
    refs
}

struct RefWalk<'a> {
    idx: &'a Index,
    source: &'a [u8],
    module: String,
    self_ty: Option<String>,
    refs: Vec<NameRef>,
}

impl RefWalk<'_> {
    fn walk(&mut self, node: tree_sitter::Node<'_>) {
        if node.is_extra() || node.is_missing() {
            return;
        }
        match node.kind() {
            "mod_item" => {
                let saved = self.module.clone();
                if let Some(name) = field_text(node, self.source, "name") {
                    self.module = mod_join(&saved, &name);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.module = saved;
            }
            "impl_item" => {
                let saved_ty = self.self_ty.clone();
                let ty = type_key(&field_text(node, self.source, "type").unwrap_or_default());
                if !ty.is_empty() {
                    self.self_ty = Some(ty);
                }
                // Header bounds and the where clause are part of the item.
                for child in structural_children(node) {
                    self.walk(child);
                }
                self.self_ty = saved_ty;
            }
            "trait_item" => {
                let saved_ty = self.self_ty.clone();
                if let Some(name) = field_text(node, self.source, "name") {
                    self.self_ty = Some(name);
                }
                for child in structural_children(node) {
                    self.walk(child);
                }
                self.self_ty = saved_ty;
            }
            "macro_invocation" => self.walk_macro(node),
            "scoped_identifier" | "scoped_type_identifier" => {
                let segs = path_segments(node, self.source);
                self.emit_path(&segs);
            }
            "generic_type" | "generic_type_with_turbofish" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    let segs = path_segments(ty, self.source);
                    self.emit_path(&segs);
                }
                if let Some(args) = node.child_by_field_name("type_arguments") {
                    self.walk(args);
                }
            }
            "generic_function" => {
                // Walk the callee. `path_segments` on a field expression is
                // empty, which used to drop the receiver (`xs.map(...).collect::<T>()`).
                if let Some(fun) = node.child_by_field_name("function") {
                    self.walk(fun);
                }
                if let Some(args) = node.child_by_field_name("type_arguments") {
                    self.walk(args);
                }
            }
            "field_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
                if let Some(field) = node.child_by_field_name("field")
                    && field.kind() != "integer_literal"
                    && let Some(name) = node_text(field, self.source)
                {
                    self.emit_method(&name);
                }
            }
            "lifetime" | "metavariable" | "primitive_type" | "string_literal"
            | "raw_string_literal" | "char_literal" | "string_content" => {}
            "identifier" | "type_identifier" | "field_identifier" => {
                if is_declarator(node) {
                    return;
                }
                if node.kind() == "field_identifier" {
                    if let Some(name) = node_text(node, self.source) {
                        self.emit_method(&name);
                    }
                    return;
                }
                if let Some(parent) = node.parent()
                    && parent.kind() == "shorthand_field_initializer"
                    && let Some(name) = node_text(node, self.source)
                {
                    self.emit_method(&name);
                    self.emit_path(&[name]);
                    return;
                }
                if let Some(name) = node_text(node, self.source) {
                    if is_call_callee(node) {
                        self.emit_call(&name);
                    } else {
                        self.emit_path(&[name]);
                    }
                }
            }
            _ => {
                for child in structural_children(node) {
                    self.walk(child);
                }
            }
        }
    }

    fn walk_macro(&mut self, node: tree_sitter::Node<'_>) {
        if let Some(mac) = node.child_by_field_name("macro") {
            let segs = path_segments(mac, self.source);
            if segs.is_empty() {
                self.walk(mac);
            } else {
                self.emit_path(&segs);
            }
        }
        for child in structural_children(node) {
            if child.kind() == "token_tree" {
                self.emit_lexical(child);
            }
        }
    }

    fn emit_lexical(&mut self, node: tree_sitter::Node<'_>) {
        let mut leaves = Vec::new();
        flatten_leaves(node, &mut leaves);
        let mut i = 0;
        while i < leaves.len() {
            if is_name_leaf(leaves[i]) {
                let mut segs = Vec::new();
                if let Some(text) = node_text(leaves[i], self.source) {
                    segs.push(text);
                }
                let mut j = i;
                while j + 2 < leaves.len()
                    && leaves[j + 1].kind() == "::"
                    && is_name_leaf(leaves[j + 2])
                {
                    if let Some(text) = node_text(leaves[j + 2], self.source) {
                        segs.push(text);
                    }
                    j += 2;
                }
                self.emit_path(&segs);
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }

    fn emit_path(&mut self, segs: &[String]) {
        if segs.is_empty() {
            return;
        }
        if segs.len() == 1 && is_skippable_bare(&segs[0]) {
            return;
        }
        if segs.first().is_some_and(|s| s == "::") {
            self.push(
                &segs[1..].join("::"),
                &PathHits {
                    all: Vec::new(),
                    finals: Vec::new(),
                },
            );
            return;
        }
        let written = segs.join("::");
        let module = self.module.clone();
        let self_ty = self.self_ty.clone();
        let hits = self.idx.lookup_in(&module, self_ty.as_deref(), &written);
        self.push(&written, &hits);
    }

    fn emit_method(&mut self, name: &str) {
        if is_skippable_bare(name) {
            return;
        }
        let written = format!(".{name}");
        let ids = self.idx.method_ids(&self.module, name);
        let hits = PathHits {
            all: ids.clone(),
            finals: ids,
        };
        self.push(&written, &hits);
    }

    /// A call with no path. Resolved names stay, and every visible same-named
    /// function or method is included (ADR 0011).
    fn emit_call(&mut self, name: &str) {
        if is_skippable_bare(name) {
            return;
        }
        let module = self.module.clone();
        let self_ty = self.self_ty.clone();
        let mut hits = self.idx.lookup_in(&module, self_ty.as_deref(), name);
        let extra = self.idx.callable_ids(&module, name);
        hits.all.extend(extra.iter().copied());
        hits.finals.extend(extra);
        dedup_ids(&mut hits.all);
        dedup_ids(&mut hits.finals);
        self.push(name, &hits);
    }

    fn push(&mut self, written: &str, hits: &PathHits) {
        if written.is_empty() {
            return;
        }
        let scope = if self.module.is_empty() {
            None
        } else {
            Some(QualifiedName::new(self.module.clone()))
        };
        let mut ids = hits.all.clone();
        dedup_ids(&mut ids);
        if ids.is_empty() {
            self.refs.push(NameRef::at(written, scope, None));
            return;
        }
        for id in ids {
            self.refs
                .push(NameRef::at(written, scope.clone(), Some(id)));
        }
    }
}

/// Test targets inside `node`, searched in every scope its content may sit
/// in (see [`Index::scopes_of`]).
fn inner_test_targets(idx: &Index, node: &Node) -> Vec<NodeId> {
    let bytes = node.raw.clone();
    let mut ids = Vec::new();
    let _ = cst::with_parser(|parser| {
        if let Some(tree) = parser.parse(bytes.as_slice(), None) {
            for scope in idx.scopes_of(node) {
                scan_tests(
                    idx,
                    tree.root_node(),
                    bytes.as_slice(),
                    &scope.module,
                    scope.self_ty.as_deref(),
                    scope.cfg_test,
                    &mut ids,
                );
            }
        }
    });
    ids
}

fn scan_tests(
    idx: &Index,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    module: &str,
    self_ty: Option<&str>,
    inherited_cfg: bool,
    out: &mut Vec<NodeId>,
) {
    if node.is_extra() || node.is_missing() {
        return;
    }
    if node.kind() == "mod_item" {
        let child = field_text(node, source, "name")
            .map(|name| mod_join(module, &name))
            .unwrap_or_else(|| module.to_owned());
        let cfg = inherited_cfg || item_inner_cfg_test(node, source);
        if let Some(body) = node.child_by_field_name("body") {
            scan_list(idx, body, source, &child, self_ty, cfg, out);
        }
        return;
    }
    if node.kind() == "impl_item" {
        let ty = type_key(&field_text(node, source, "type").unwrap_or_default());
        let ty = if ty.is_empty() {
            self_ty.map(str::to_owned)
        } else {
            Some(ty)
        };
        if let Some(body) = node.child_by_field_name("body") {
            scan_list(idx, body, source, module, ty.as_deref(), inherited_cfg, out);
        }
        return;
    }
    if node.kind() == "trait_item" {
        let name = field_text(node, source, "name");
        let cfg = inherited_cfg || item_inner_cfg_test(node, source);
        if let Some(body) = node.child_by_field_name("body") {
            scan_list(
                idx,
                body,
                source,
                module,
                name.as_deref().or(self_ty),
                cfg,
                out,
            );
        }
        return;
    }
    scan_list(idx, node, source, module, self_ty, inherited_cfg, out);
}

fn scan_list(
    idx: &Index,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    module: &str,
    self_ty: Option<&str>,
    inherited_cfg: bool,
    out: &mut Vec<NodeId>,
) {
    let cfg = inherited_cfg || item_inner_cfg_test(node, source);
    let mut pending = Vec::new();
    for child in structural_children(node) {
        match child.kind() {
            "attribute_item" => pending.push(child),
            "inner_attribute_item" => {}
            _ => {
                let flags = flags_of_attrs(&pending, source);
                pending.clear();
                let testish =
                    flags.is_test || ((cfg || flags.cfg_test) && is_fn_kind(child.kind()));
                if testish && is_fn_kind(child.kind()) {
                    out.extend(refs_of_ts(idx, child, source, module, self_ty));
                }
                let next_cfg = cfg || flags.cfg_test;
                if matches!(
                    child.kind(),
                    "mod_item"
                        | "impl_item"
                        | "trait_item"
                        | "declaration_list"
                        | "block"
                        | "source_file"
                ) || child.child_by_field_name("body").is_some()
                {
                    scan_tests(idx, child, source, module, self_ty, next_cfg, out);
                }
            }
        }
    }
}

fn refs_of_ts(
    idx: &Index,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    module: &str,
    self_ty: Option<&str>,
) -> Vec<NodeId> {
    let mut walk = RefWalk {
        idx,
        source,
        module: module.to_owned(),
        self_ty: self_ty.map(str::to_owned),
        refs: Vec::new(),
    };
    walk.walk(node);
    resolved_ids(&walk.refs)
}

fn flatten_leaves<'a>(node: tree_sitter::Node<'a>, out: &mut Vec<tree_sitter::Node<'a>>) {
    if node.child_count() == 0 {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            flatten_leaves(cursor.node(), out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn is_name_leaf(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier" | "crate" | "self" | "super"
    )
}

fn is_declarator(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !RustAdapter.is_definition(&NodeKind::new(parent.kind())) {
        return false;
    }
    parent.child_by_field_name("name").is_some_and(|name| {
        name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
    })
}

/// `name` is the callee of `name(...)` or `name::<T>(...)`, not an argument.
fn is_call_callee(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(parent.kind(), "call_expression" | "generic_function") {
        return false;
    }
    parent
        .child_by_field_name("function")
        .is_some_and(|fun| fun.id() == node.id())
}

fn is_skippable_bare(name: &str) -> bool {
    name.is_empty() || name == "_" || is_keyword(name) || is_primitive(name)
}

fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "union"
            | "box"
            | "become"
            | "abstract"
            | "typeof"
            | "unsized"
            | "virtual"
            | "override"
            | "final"
            | "priv"
            | "try"
            | "yield"
            | "macro_rules"
    )
}

fn is_primitive(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
    )
}

// --- tree-sitter helpers ---------------------------------------------------

fn structural_children<'a>(node: tree_sitter::Node<'a>) -> Vec<tree_sitter::Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if !child.is_extra() && !child.is_missing() {
                out.push(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

fn node_text(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn field_text(node: tree_sitter::Node<'_>, source: &[u8], field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| node_text(n, source))
}

#[cfg(test)]
mod index_cache {
    use super::{index_builds, references};
    use hord_core::{Bytes, LangId, Node, NodeId, NodeKind, ObjectId};
    use hord_lang::ResolveCtx;

    #[test]
    fn repeated_references_build_the_index_once() {
        let mut ctx = ResolveCtx::new();
        let node = Node {
            kind: NodeKind::new("function_item"),
            lang: LangId::new("rust"),
            raw: Bytes::from(b"fn f() {}".as_slice()),
            normalized: ObjectId::from_bytes([0; 32]),
            children: Vec::new(),
            name: None,
        };
        let before = index_builds();
        let first = references(&ctx, &node);
        let second = references(&ctx, &node);
        assert_eq!(first, second);
        assert_eq!(index_builds(), before + 1);
        ctx.add_definition(
            NodeId::nil(),
            None,
            NodeKind::new("function_item"),
            "crate",
            "f",
            "",
            false,
            false,
        );
        let _ = references(&ctx, &node);
        assert_eq!(index_builds(), before + 2);
    }
}

#[cfg(test)]
mod incremental {
    use std::collections::{BTreeMap, HashSet};
    use std::convert::Infallible;
    use std::sync::Arc;

    use hord_core::{NodeId, ObjectId, RepoPath};
    use hord_lang::{IdentifiedTree, LangAdapter, ResolveCtx, Site};

    use crate::{ManifestFile, RustAdapter, RustFile};

    /// Deterministic ids per (path, site, salt), so a salt change models a
    /// carried identity that differs while the bytes stay the same.
    fn identify(path: &RepoPath, source: &str, salt: u8) -> IdentifiedTree {
        let tree = RustAdapter.parse(source.as_bytes()).unwrap();
        let mut ids = BTreeMap::new();
        fn walk(
            tree: &hord_lang::NodeTree,
            oid: ObjectId,
            site: &mut Site,
            seed: &str,
            ids: &mut BTreeMap<Site, NodeId>,
        ) {
            let node = tree.get(oid).unwrap();
            if RustAdapter.is_definition(&node.kind) {
                let h = ObjectId::of_byte_string(format!("{seed}{site:?}").as_bytes());
                let mut b = [0u8; 16];
                b.copy_from_slice(&h.as_bytes()[..16]);
                ids.insert(site.clone(), NodeId::from_u128(u128::from_le_bytes(b)));
            }
            for (i, child) in node.children.iter().enumerate() {
                site.push(u32::try_from(i).unwrap());
                walk(tree, *child, site, seed, ids);
                site.pop();
            }
        }
        if let Some(root) = tree.root() {
            walk(
                &tree,
                root,
                &mut Vec::new(),
                &format!("{path}{salt}"),
                &mut ids,
            );
        }
        IdentifiedTree::new(tree, ids)
    }

    fn rp(path: &str) -> RepoPath {
        RepoPath::new(path.split('/').map(str::to_owned).collect::<Vec<_>>())
    }

    struct Snap {
        files: BTreeMap<RepoPath, (String, u8)>,
    }

    impl Snap {
        fn set(&mut self, path: &str, source: &str) {
            let salt = self.files.get(&rp(path)).map_or(0, |f| f.1);
            self.files.insert(rp(path), (source.to_owned(), salt));
        }

        fn key(path: &RepoPath, source: &str, salt: u8) -> (ObjectId, Option<ObjectId>) {
            let blob = ObjectId::of_byte_string(source.as_bytes());
            let carried = (salt > 0).then(|| ObjectId::of_byte_string(&[salt]));
            let _ = path;
            (blob, carried)
        }

        /// Full build over every `.rs` file, and the incremental build from
        /// `prev`. Returns the incremental context and the paths it loaded.
        fn build(&self, prev: Option<&ResolveCtx>) -> (ResolveCtx, ResolveCtx, HashSet<RepoPath>) {
            let manifests: Vec<(RepoPath, &str)> = self
                .files
                .iter()
                .filter(|(p, _)| p.to_string().ends_with("Cargo.toml"))
                .map(|(p, (s, _))| (p.clone(), s.as_str()))
                .collect();
            let manifest_views: Vec<ManifestFile<'_>> = manifests
                .iter()
                .map(|(path, s)| ManifestFile {
                    path,
                    bytes: s.as_bytes(),
                })
                .collect();
            let rs: Vec<(RepoPath, IdentifiedTree)> = self
                .files
                .iter()
                .filter(|(p, _)| p.to_string().ends_with(".rs"))
                .map(|(p, (s, salt))| (p.clone(), identify(p, s, *salt)))
                .collect();
            let views: Vec<RustFile<'_>> = rs
                .iter()
                .map(|(path, t)| RustFile {
                    path,
                    tree: &t.tree,
                    ids: &t.ids,
                })
                .collect();
            let full = RustAdapter.resolve_context_with(&views, &manifest_views);

            let keys: Vec<(RepoPath, (ObjectId, Option<ObjectId>))> = self
                .files
                .iter()
                .filter(|(p, _)| p.to_string().ends_with(".rs"))
                .map(|(p, (s, salt))| (p.clone(), Self::key(p, s, *salt)))
                .collect();
            let mut loaded = HashSet::new();
            let inc = RustAdapter
                .resolve_context_incremental(prev, &keys, &manifest_views, |path| {
                    loaded.insert(path.clone());
                    let (s, salt) = &self.files[path];
                    Ok::<_, Infallible>(Some(Arc::new(identify(path, s, *salt))))
                })
                .unwrap();
            (full, inc, loaded)
        }
    }

    fn paths(list: &[&str]) -> HashSet<RepoPath> {
        list.iter().map(|p| rp(p)).collect()
    }

    fn assert_same(full: &ResolveCtx, inc: &ResolveCtx, step: &str) {
        assert!(
            full.same_contents(inc),
            "{step}: incremental context differs from a full build"
        );
        assert!(
            full.definition_count() > 0,
            "{step}: fixture indexes nothing"
        );
    }

    #[test]
    fn incremental_context_equals_full_rebuild_over_landings() {
        let mut snap = Snap {
            files: BTreeMap::new(),
        };
        snap.set(
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\"]\n[workspace.dependencies]\na = { path = \"a\" }\n",
        );
        snap.set("a/Cargo.toml", "[package]\nname = \"a\"\n");
        snap.set(
            "b/Cargo.toml",
            "[package]\nname = \"b\"\n[dependencies]\na = { workspace = true }\n",
        );
        snap.set(
            "a/src/lib.rs",
            "mod x;\npub use x::helper;\npub fn f() -> u32 { 1 }\n",
        );
        snap.set("a/src/x.rs", "pub fn helper() -> u32 { crate::f() }\n");
        snap.set(
            "b/src/lib.rs",
            "use a::f;\nuse a::*;\npub fn g() -> u32 { f() + helper() }\n",
        );
        snap.set(
            "b/tests/t.rs",
            "#[test]\nfn t() { assert_eq!(b::g(), 2); }\n",
        );

        let (full, mut prev, loaded) = snap.build(None);
        assert_same(&full, &prev, "initial");
        assert_eq!(loaded.len(), 4, "the first build loads every file");

        type Step = (&'static str, fn(&mut Snap), &'static [&'static str]);
        let steps: [Step; 8] = [
            (
                "edit a body",
                |s| s.set("a/src/x.rs", "pub fn helper() -> u32 { crate::f() + 1 }\n"),
                &["a/src/x.rs"],
            ),
            ("no change", |_| {}, &[]),
            (
                "add a mod and its file",
                |s| {
                    s.set(
                        "a/src/lib.rs",
                        "mod x;\nmod y;\npub use x::helper;\npub fn f() -> u32 { 1 }\n",
                    );
                    s.set(
                        "a/src/y.rs",
                        "pub(crate) struct Y;\nimpl Y { fn new() -> Self { Y } }\n",
                    );
                },
                &["a/src/lib.rs", "a/src/y.rs"],
            ),
            (
                "move a file to mod.rs",
                |s| {
                    let body = s.files.remove(&rp("a/src/y.rs")).unwrap().0;
                    s.set("a/src/y/mod.rs", &body);
                },
                &["a/src/y/mod.rs"],
            ),
            (
                "orphan a file (its module falls back to its path)",
                |s| {
                    s.set(
                        "a/src/lib.rs",
                        "mod x;\npub use x::helper;\npub fn f() -> u32 { 1 }\n",
                    )
                },
                // y/mod.rs is unchanged but its module moved, so it reloads.
                &["a/src/lib.rs", "a/src/y/mod.rs"],
            ),
            (
                "drop a manifest dependency",
                |s| s.set("b/Cargo.toml", "[package]\nname = \"b\"\n"),
                &[],
            ),
            (
                "carried identity changes, bytes do not",
                |s| s.files.get_mut(&rp("b/src/lib.rs")).unwrap().1 = 7,
                &["b/src/lib.rs"],
            ),
            (
                "delete a file",
                |s| {
                    s.files.remove(&rp("b/tests/t.rs"));
                },
                &[],
            ),
        ];
        for (step, edit, expect) in steps {
            edit(&mut snap);
            let (full, inc, loaded) = snap.build(Some(&prev));
            assert_same(&full, &inc, step);
            assert_eq!(loaded, paths(expect), "{step}: files re-indexed");
            prev = inc;
        }
    }

    #[test]
    fn links_change_with_manifests() {
        let mut snap = Snap {
            files: BTreeMap::new(),
        };
        snap.set("a/Cargo.toml", "[package]\nname = \"a\"\n");
        snap.set("a/src/lib.rs", "pub fn f() {}\n");
        snap.set("b/Cargo.toml", "[package]\nname = \"b\"\n");
        snap.set("b/src/lib.rs", "pub fn g() { a::f() }\n");
        let (_, prev, _) = snap.build(None);
        let mut links = 0;
        prev.for_each_link(|_, _, _| links += 1);
        assert_eq!(links, 0);
        snap.set(
            "b/Cargo.toml",
            "[package]\nname = \"b\"\n[dependencies]\na = { path = \"../a\" }\n",
        );
        let (full, inc, loaded) = snap.build(Some(&prev));
        assert!(loaded.is_empty());
        assert!(full.same_contents(&inc));
        let mut links = 0;
        inc.for_each_link(|_, _, _| links += 1);
        assert_eq!(links, 1, "a new path dependency links b to a");
    }
}
