//! Identity stability and rename precision on consecutive cargo commits.
//!
//! A labeled pair is the same function when its qualified name is unchanged
//! in the file, including across a git rename of the path. That is the
//! stability sample.
//!
//! Rename precision uses a different labeler than ADR 0007: a hord rename is
//! precise when the stripped bodies still share at least half their tokens.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use hord_core::{Node, ObjectId, QualifiedName};
use hord_lang::{IdentifiedTree, LangAdapter, NodeTree, default_identify};
use hord_lang_rust::RustAdapter;

use crate::git;

const MAX_BYTES: usize = 500_000;
/// Independent of ADR 0007's 0.8 tree-edit ratio. A hord rename counts as
/// precise when the stripped bodies still share this much token mass.
const RENAME_JACCARD: f64 = 0.5;

pub(crate) struct IdentityReport {
    pub commits: usize,
    pub labeled: usize,
    pub stable: usize,
    pub hord_renames: usize,
    pub precise_renames: usize,
}

struct Site {
    oid: ObjectId,
    kind: String,
    qname: String,
    tokens: BTreeSet<String>,
}

pub(crate) fn measure(git_dir: &Path, sample: usize, max_commits: usize) -> Result<IdentityReport> {
    let adapter = RustAdapter;
    let commits = git::rev_list(git_dir)?;
    let mut report = IdentityReport {
        commits: 0,
        labeled: 0,
        stable: 0,
        hord_renames: 0,
        precise_renames: 0,
    };
    for commit in commits {
        if report.commits >= max_commits {
            break;
        }
        if report.labeled >= sample && report.hord_renames >= 20 {
            break;
        }
        let Some(parent) = git::parent(git_dir, &commit)? else {
            continue;
        };
        report.commits += 1;
        if report.commits.is_multiple_of(25) {
            eprintln!(
                "[identity] scanned {} commits, labeled {}",
                report.commits, report.labeled
            );
        }
        for (old_path, new_path) in git::changed_rust(git_dir, &parent, &commit)? {
            if report.labeled >= sample && report.hord_renames >= 20 {
                break;
            }
            let old_bytes = git::blob(git_dir, &parent, &old_path).unwrap_or_default();
            let new_bytes = git::blob(git_dir, &commit, &new_path).unwrap_or_default();
            if old_bytes.is_empty()
                || new_bytes.is_empty()
                || old_bytes.len() > MAX_BYTES
                || new_bytes.len() > MAX_BYTES
            {
                continue;
            }
            score_file(&adapter, &old_bytes, &new_bytes, sample, &mut report);
        }
    }
    Ok(report)
}

fn score_file(
    adapter: &RustAdapter,
    old_bytes: &[u8],
    new_bytes: &[u8],
    sample: usize,
    report: &mut IdentityReport,
) {
    let Ok(old_tree) = adapter.parse(old_bytes) else {
        return;
    };
    let Ok(new_tree) = adapter.parse(new_bytes) else {
        return;
    };
    let empty = IdentifiedTree::default();
    let base_map = default_identify(adapter, &empty, &old_tree);
    let base = IdentifiedTree::new(old_tree, base_map.nodes.clone());
    let result_map = default_identify(adapter, &base, &new_tree);
    let old_sites = sites(adapter, &base.tree);
    let new_sites = sites(adapter, &new_tree);
    let mut used = BTreeSet::new();
    for new_site in &new_sites {
        if report.labeled >= sample {
            break;
        }
        if !is_function(&new_site.kind) {
            continue;
        }
        let Some(oi) = old_sites.iter().position(|old| {
            !used.contains(&old.oid) && old.kind == new_site.kind && old.qname == new_site.qname
        }) else {
            continue;
        };
        used.insert(old_sites[oi].oid);
        let old_id = base.ids.get(&old_sites[oi].oid).copied();
        let new_id = result_map.nodes.get(&new_site.oid).copied();
        if old_id.is_some() && old_id == new_id {
            report.stable += 1;
        }
        report.labeled += 1;
    }
    for new_site in &new_sites {
        if !is_function(&new_site.kind) {
            continue;
        }
        let Some(new_id) = result_map.nodes.get(&new_site.oid).copied() else {
            continue;
        };
        let Some(old_site) = old_sites.iter().find(|old| {
            is_function(&old.kind)
                && base.ids.get(&old.oid).copied() == Some(new_id)
                && old.qname != new_site.qname
        }) else {
            continue;
        };
        report.hord_renames += 1;
        if jaccard(&old_site.tokens, &new_site.tokens) >= RENAME_JACCARD {
            report.precise_renames += 1;
        }
    }
}

fn is_function(kind: &str) -> bool {
    kind == "function_item" || kind == "function_signature_item"
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

fn sites(adapter: &RustAdapter, tree: &NodeTree) -> Vec<Site> {
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
    out: &mut Vec<Site>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    if adapter.is_definition(&node.kind)
        && let Some(qname) = qname(adapter, tree, ancestors, node)
    {
        let simple = qname.as_str().rsplit("::").next().unwrap_or(qname.as_str());
        let stripped = tree.stripped(oid).unwrap_or_default();
        let text = String::from_utf8_lossy(stripped);
        out.push(Site {
            oid,
            kind: node.kind.as_str().to_string(),
            qname: qname.as_str().to_string(),
            tokens: tokens(&text, simple),
        });
    }
    ancestors.push(oid);
    for child in &node.children {
        walk(adapter, tree, *child, ancestors, out);
    }
    ancestors.pop();
}

fn qname(
    adapter: &RustAdapter,
    tree: &NodeTree,
    ancestors: &[ObjectId],
    node: &Node,
) -> Option<QualifiedName> {
    if let Some(name) = &node.name {
        return Some(name.clone());
    }
    let nodes: Vec<&Node> = ancestors.iter().filter_map(|id| tree.get(*id)).collect();
    adapter.qualified_name(&nodes, node)
}

fn tokens(text: &str, simple: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| tok.len() >= 2 && !tok.eq_ignore_ascii_case(simple))
        .map(|tok| tok.to_ascii_lowercase())
        .collect()
}

pub(crate) fn identity_ok(report: &IdentityReport, sample: usize) -> bool {
    if report.labeled < sample {
        return false;
    }
    let stable = report.stable * 100 >= report.labeled * 97;
    let precision =
        report.hord_renames > 0 && report.precise_renames * 100 >= report.hord_renames * 95;
    stable && precision
}
