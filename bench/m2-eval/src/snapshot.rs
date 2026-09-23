//! Cargo HEAD snapshot: reference recall and warm blame lookups.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use hord_core::{
    Actor, ChangeRecord, IdentityMap, Intent, Node, NodeId, NodePath, ObjectId, Provenance,
    RepoPath, Timestamp,
};
use hord_lang::{IdentifiedTree, LangAdapter, NodeTree, default_identify};
use hord_lang_rust::{ManifestFile, RustAdapter, RustFile};
use hord_store::Store;

use crate::git;

const MAX_BYTES: usize = 500_000;

pub(crate) struct FileSnap {
    pub path: RepoPath,
    pub tree: NodeTree,
    /// Definition site → id.
    pub ids: BTreeMap<Vec<u32>, NodeId>,
    /// Content id → id of its first site in preorder (for oracles that name
    /// definitions by content).
    pub by_oid: std::collections::HashMap<ObjectId, NodeId>,
}

impl FileSnap {
    /// Under cargo's `src/` or `crates/`, the scope of the §12 samples.
    pub(crate) fn in_scope(&self) -> bool {
        matches!(
            self.path.components().first().map(String::as_str),
            Some("src" | "crates")
        )
    }
}

pub(crate) struct ReferenceReport {
    pub labeled: usize,
    pub expected: usize,
    pub hit: usize,
    /// The labeled functions, in sample order.
    pub sample: Vec<Sampled>,
}

/// One labeled function definition.
pub(crate) struct Sampled {
    pub path: String,
    pub oid: ObjectId,
    pub qname: String,
}

pub(crate) struct BlameReport {
    pub defs: usize,
    pub max_ms: f64,
}

/// Every `.rs` file at HEAD. Samples use [`FileSnap::in_scope`] files; the
/// rust-analyzer oracle resolves over all of them, since its references may
/// sit in `tests/` or other workspace members.
pub(crate) fn load_head(git_dir: &Path) -> Result<Vec<FileSnap>> {
    let adapter = RustAdapter;
    let mut files = Vec::new();
    for path in git::head_rust_files(git_dir)? {
        let bytes = git::blob(git_dir, "HEAD", &path).unwrap_or_default();
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            continue;
        }
        let Ok(tree) = adapter.parse(&bytes) else {
            continue;
        };
        let empty = IdentifiedTree::default();
        let mapping = default_identify(&adapter, &empty, &tree);
        let Ok(repo_path) = path.parse::<RepoPath>() else {
            continue;
        };
        let by_oid = first_site_ids(&tree, &mapping.nodes);
        files.push(FileSnap {
            path: repo_path,
            tree,
            ids: mapping.nodes,
            by_oid,
        });
    }
    Ok(files)
}

pub(crate) struct ManifestSnap {
    pub path: RepoPath,
    pub bytes: Vec<u8>,
}

pub(crate) fn load_manifests(git_dir: &Path) -> Result<Vec<ManifestSnap>> {
    let raw = git::git(git_dir, &["ls-tree", "-r", "--name-only", "HEAD"])?;
    let mut out = Vec::new();
    for path in String::from_utf8(raw)?.lines() {
        if !path.ends_with("Cargo.toml") {
            continue;
        }
        let Ok(bytes) = git::blob(git_dir, "HEAD", path) else {
            continue;
        };
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            continue;
        }
        let Ok(repo_path) = path.parse::<RepoPath>() else {
            continue;
        };
        out.push(ManifestSnap {
            path: repo_path,
            bytes,
        });
    }
    Ok(out)
}

pub(crate) fn references(
    files: &[&FileSnap],
    manifests: &[ManifestSnap],
    sample: usize,
) -> ReferenceReport {
    let adapter = RustAdapter;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|file| RustFile {
            path: &file.path,
            tree: &file.tree,
            ids: &file.ids,
        })
        .collect();
    let manifest_views: Vec<ManifestFile<'_>> = manifests
        .iter()
        .map(|manifest| ManifestFile {
            path: &manifest.path,
            bytes: &manifest.bytes,
        })
        .collect();
    let ctx = adapter.resolve_context_with(&views, &manifest_views);
    let mut counts = BTreeMap::<String, usize>::new();
    for file in files {
        for site in defs(&adapter, &file.tree) {
            if !is_named_item(&site.kind) {
                continue;
            }
            if let Some(simple) = site.qname.rsplit("::").next()
                && simple.len() >= 4
            {
                *counts.entry(simple.to_string()).or_insert(0) += 1;
            }
        }
    }
    let simple_names: BTreeSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n == 1)
        .map(|(name, _)| name)
        .collect();
    let mut report = ReferenceReport {
        labeled: 0,
        expected: 0,
        hit: 0,
        sample: Vec::new(),
    };
    let mut miss_parents = BTreeMap::<String, usize>::new();
    let mut miss_names = BTreeMap::<String, usize>::new();
    let mut miss_examples = 0usize;
    for file in files {
        if report.labeled >= sample {
            break;
        }
        for site in defs(&adapter, &file.tree) {
            if report.labeled >= sample {
                break;
            }
            if site.kind != "function_item" {
                continue;
            }
            let Some(node) = file.tree.get(site.oid) else {
                continue;
            };
            let own = site.qname.rsplit("::").next().unwrap_or("");
            let leaves = identifier_leaves(&file.tree, site.oid);
            let mut expected: Vec<(&str, &str)> = Vec::new();
            let mut seen = BTreeSet::new();
            for leaf in &leaves {
                if leaf.text == own || leaf.text.len() < 4 || !simple_names.contains(leaf.text) {
                    continue;
                }
                if seen.insert(leaf.text) {
                    expected.push((leaf.text, leaf.parent));
                }
            }
            if expected.is_empty() {
                continue;
            }
            let hord: BTreeSet<String> = adapter
                .references(&ctx, node)
                .into_iter()
                .map(|name| name.name.as_str().to_string())
                .collect();
            let mut missed_here = Vec::new();
            for (name, parent) in &expected {
                report.expected += 1;
                if covers(&hord, name) {
                    report.hit += 1;
                } else {
                    *miss_parents.entry((*parent).to_string()).or_insert(0) += 1;
                    *miss_names.entry((*name).to_string()).or_insert(0) += 1;
                    if missed_here.len() < 8 {
                        missed_here.push((*name, *parent));
                    }
                }
            }
            if !missed_here.is_empty() && miss_examples < 12 {
                eprintln!(
                    "[references] miss {} expected {} hit-gap {} e.g. {}",
                    site.qname,
                    expected.len(),
                    missed_here.len(),
                    missed_here
                        .iter()
                        .map(|(name, parent)| format!("{name}@{parent}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                miss_examples += 1;
            }
            report.labeled += 1;
            report.sample.push(Sampled {
                path: file.path.to_string(),
                oid: site.oid,
                qname: site.qname.clone(),
            });
        }
    }
    if !miss_parents.is_empty() {
        let mut parents: Vec<_> = miss_parents.into_iter().collect();
        parents.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let shown = parents
            .iter()
            .take(12)
            .map(|(kind, n)| format!("{kind}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("[references] miss parents {shown}");
        let mut names: Vec<_> = miss_names.into_iter().collect();
        names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let shown = names
            .iter()
            .take(12)
            .map(|(name, n)| format!("{name}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("[references] miss names {shown}");
    }
    report
}

pub(crate) fn references_ok(report: &ReferenceReport, sample: usize) -> bool {
    report.labeled >= sample && report.expected > 0 && report.hit * 100 >= report.expected * 95
}

pub(crate) fn blame(files: &[&FileSnap]) -> Result<BlameReport> {
    let dir = std::env::temp_dir().join(format!("hord-m2-blame-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("blame temp dir")?;
    let store = Store::create(&dir).context("create store")?;
    let snapshot = ObjectId::from_bytes([2; 32]);
    let mut map = IdentityMap::default();
    let mut write_set = std::collections::BTreeSet::new();
    for file in files {
        for (site, node_id) in &file.ids {
            let pointer = site.clone();
            let path = NodePath {
                file: file.path.clone(),
                pointer,
            };
            if map.nodes.values().any(|existing| existing == &path) {
                continue;
            }
            map.nodes.insert(*node_id, path);
            write_set.insert(*node_id);
        }
    }
    let defs = write_set.len();
    store.put_identity(snapshot, &map).context("put identity")?;
    let record = ChangeRecord {
        base: ObjectId::from_bytes([1; 32]),
        result: snapshot,
        parents: Vec::new(),
        ops: Vec::new(),
        intent: Intent {
            summary: "m2 blame sample".into(),
            body: String::new(),
            refs: Vec::new(),
            acceptance: Vec::new(),
        },
        provenance: Provenance {
            actor: Actor::Human {
                id: "m2-eval".into(),
            },
            toolchain: ObjectId::from_bytes([3; 32]),
            created_at: Timestamp::from_millis(0),
            session: None,
            parent_intent: None,
        },
        read_set: std::collections::BTreeSet::new(),
        write_set: write_set.clone(),
        identity_deltas: Vec::new(),
        evidence: Vec::new(),
        signature: None,
    };
    let change = store.put_object(&record).context("put change")?;
    store.append_log(change).context("append log")?;
    store.index_change(change).context("index change")?;
    let Some(first) = write_set.iter().next().copied() else {
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(BlameReport {
            defs: 0,
            max_ms: 0.0,
        });
    };
    store.node_history(first).context("warm lookup")?;
    let mut max_ms = 0.0_f64;
    for node in &write_set {
        let started = Instant::now();
        store.node_history(*node).context("blame lookup")?;
        max_ms = max_ms.max(started.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(BlameReport { defs, max_ms })
}

pub(crate) fn blame_ok(report: &BlameReport) -> bool {
    report.defs > 0 && report.max_ms < 50.0
}

struct Def {
    oid: ObjectId,
    kind: String,
    qname: String,
}

fn defs(adapter: &RustAdapter, tree: &NodeTree) -> Vec<Def> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ancestors = Vec::new();
    walk(adapter, tree, root, &mut ancestors, &mut out);
    out
}

fn walk(
    adapter: &RustAdapter,
    tree: &NodeTree,
    oid: ObjectId,
    ancestors: &mut Vec<ObjectId>,
    out: &mut Vec<Def>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    if adapter.is_definition(&node.kind)
        && let Some(qname) = name_of(adapter, tree, ancestors, node)
    {
        out.push(Def {
            oid,
            kind: node.kind.as_str().to_string(),
            qname,
        });
    }
    ancestors.push(oid);
    for child in &node.children {
        walk(adapter, tree, *child, ancestors, out);
    }
    ancestors.pop();
}

fn name_of(
    adapter: &RustAdapter,
    tree: &NodeTree,
    ancestors: &[ObjectId],
    node: &Node,
) -> Option<String> {
    if let Some(name) = &node.name {
        return Some(name.as_str().to_string());
    }
    let nodes: Vec<&Node> = ancestors.iter().filter_map(|id| tree.get(*id)).collect();
    adapter
        .qualified_name(&nodes, node)
        .map(|name| name.as_str().to_string())
}

fn is_named_item(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
    )
}

struct IdentLeaf<'a> {
    text: &'a str,
    parent: &'a str,
}

/// Identifier, type, and field leaves under `root`. Strings and comments are
/// not leaves of these kinds, so they are not expected references.
fn identifier_leaves<'a>(tree: &'a NodeTree, root: ObjectId) -> Vec<IdentLeaf<'a>> {
    let mut out = Vec::new();
    collect_idents(tree, root, "", &mut out);
    out
}

fn collect_idents<'a>(
    tree: &'a NodeTree,
    oid: ObjectId,
    parent: &'a str,
    out: &mut Vec<IdentLeaf<'a>>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let kind = node.kind.as_str();
    if node.children.is_empty()
        && matches!(kind, "identifier" | "type_identifier" | "field_identifier")
        && let Some(bytes) = tree.stripped(oid)
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        out.push(IdentLeaf { text, parent });
        return;
    }
    for child in &node.children {
        collect_idents(tree, *child, kind, out);
    }
}

fn covers(hord: &BTreeSet<String>, simple: &str) -> bool {
    hord.iter().any(|name| {
        let name = name.strip_prefix('.').unwrap_or(name);
        // A path reference emits every segment (`Platform::new` references
        // `Platform`). Method calls are stored as `.name`.
        name.split("::").any(|segment| segment == simple)
    })
}

/// Content id → id of its first definition site in preorder.
pub(crate) fn first_site_ids(
    tree: &NodeTree,
    ids: &BTreeMap<Vec<u32>, NodeId>,
) -> std::collections::HashMap<ObjectId, NodeId> {
    let mut out = std::collections::HashMap::new();
    for (site, id) in ids {
        if let Some(oid) = hord_lang::oid_at(tree, site) {
            out.entry(oid).or_insert(*id);
        }
    }
    out
}
