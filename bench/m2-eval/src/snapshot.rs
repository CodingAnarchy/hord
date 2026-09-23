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
use hord_lang_rust::{RustAdapter, RustFile};
use hord_store::Store;

use crate::git;

const MAX_BYTES: usize = 500_000;

pub(crate) struct FileSnap {
    path: RepoPath,
    tree: NodeTree,
    ids: BTreeMap<ObjectId, NodeId>,
}

pub(crate) struct ReferenceReport {
    pub labeled: usize,
    pub expected: usize,
    pub hit: usize,
}

pub(crate) struct BlameReport {
    pub defs: usize,
    pub max_ms: f64,
}

pub(crate) fn load_head(git_dir: &Path) -> Result<Vec<FileSnap>> {
    let adapter = RustAdapter;
    let mut files = Vec::new();
    for path in git::head_rust_files(git_dir)? {
        if !(path.starts_with("src/") || path.starts_with("crates/")) {
            continue;
        }
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
        files.push(FileSnap {
            path: repo_path,
            tree,
            ids: mapping.nodes,
        });
    }
    Ok(files)
}

pub(crate) fn references(files: &[FileSnap], sample: usize) -> ReferenceReport {
    let adapter = RustAdapter;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|file| RustFile {
            path: &file.path,
            tree: &file.tree,
            ids: &file.ids,
        })
        .collect();
    let ctx = adapter.resolve_context(&views);
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
    };
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
            let stripped = file.tree.stripped(site.oid).unwrap_or_default();
            let text = String::from_utf8_lossy(stripped);
            let own = site.qname.rsplit("::").next().unwrap_or("");
            let expected: BTreeSet<&str> = tokens(&text)
                .into_iter()
                .filter(|tok| *tok != own && simple_names.contains(*tok))
                .collect();
            if expected.is_empty() {
                continue;
            }
            let hord: BTreeSet<String> = adapter
                .references(&ctx, node)
                .into_iter()
                .map(|name| name.name.as_str().to_string())
                .collect();
            for name in expected {
                report.expected += 1;
                if covers(&hord, name) {
                    report.hit += 1;
                }
            }
            report.labeled += 1;
        }
    }
    report
}

pub(crate) fn references_ok(report: &ReferenceReport, sample: usize) -> bool {
    report.labeled >= sample && report.expected > 0 && report.hit * 100 >= report.expected * 95
}

pub(crate) fn blame(files: &[FileSnap]) -> Result<BlameReport> {
    let dir = std::env::temp_dir().join(format!("hord-m2-blame-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("blame temp dir")?;
    let store = Store::create(&dir).context("create store")?;
    let snapshot = ObjectId::from_bytes([2; 32]);
    let mut map = IdentityMap::default();
    let mut write_set = std::collections::BTreeSet::new();
    for file in files {
        for (oid, node_id) in &file.ids {
            let Some(pointer) = pointer(&file.tree, *oid) else {
                continue;
            };
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

fn tokens(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| tok.len() >= 2)
        .collect()
}

fn covers(hord: &BTreeSet<String>, simple: &str) -> bool {
    hord.iter()
        .any(|name| name == simple || name.rsplit("::").next().is_some_and(|tail| tail == simple))
}

fn pointer(tree: &NodeTree, target: ObjectId) -> Option<Vec<u32>> {
    let root = tree.root()?;
    let mut path = Vec::new();
    if find(tree, root, target, &mut path) {
        Some(path)
    } else {
        None
    }
}

fn find(tree: &NodeTree, oid: ObjectId, target: ObjectId, path: &mut Vec<u32>) -> bool {
    if oid == target {
        return true;
    }
    let Some(node) = tree.get(oid) else {
        return false;
    };
    for (index, child) in node.children.iter().enumerate() {
        path.push(index as u32);
        if find(tree, *child, target, path) {
            return true;
        }
        path.pop();
    }
    false
}
