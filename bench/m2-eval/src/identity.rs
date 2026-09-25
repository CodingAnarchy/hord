//! Identity stability and rename precision on consecutive cargo commits.
//!
//! A labeled pair is the same function when its qualified name is unchanged
//! in the file, including across a git rename of the path. That is the
//! stability sample.
//!
//! Rename precision uses a different labeler than ADR 0007: a hord rename is
//! precise when the CST leaves still share at least half their tokens.
//! Leaves are split one at a time. Concatenating the stripped body first
//! glues adjacent tokens (`pub` + `fn` becomes `pubfn`) and under-counts.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use hord_core::ObjectId;
use hord_lang::{IdentifiedTree, LangAdapter, NodeTree, default_identify};
use hord_lang_rust::RustAdapter;

use crate::{git, snapshot};

const MAX_BYTES: usize = 500_000;

pub(crate) struct IdentityReport {
    pub commits: usize,
    pub labeled: usize,
    pub stable: usize,
    pub hord_renames: usize,
    pub precise_renames: usize,
}

struct Site {
    oid: ObjectId,
    /// Child-index path from the file root.
    at: Vec<u32>,
    kind: String,
    qname: String,
    /// Alphanumeric runs of each CST leaf, lowercased, excluding the
    /// function's own simple name.
    leaves: BTreeSet<String>,
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
            score_file(
                &adapter,
                &old_path,
                &new_path,
                &old_bytes,
                &new_bytes,
                sample,
                &mut report,
            );
        }
    }
    Ok(report)
}

fn score_file(
    adapter: &RustAdapter,
    old_path: &str,
    new_path: &str,
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
        let old_id = base.ids.get(&old_sites[oi].at).copied();
        let new_id = result_map.nodes.get(&new_site.at).copied();
        if old_id.is_some() && old_id == new_id {
            report.stable += 1;
        }
        report.labeled += 1;
    }
    for new_site in &new_sites {
        if !is_function(&new_site.kind) {
            continue;
        }
        let Some(new_id) = result_map.nodes.get(&new_site.at).copied() else {
            continue;
        };
        let Some(old_site) = old_sites.iter().find(|old| {
            is_function(&old.kind)
                && base.ids.get(&old.at).copied() == Some(new_id)
                && old.qname != new_site.qname
        }) else {
            continue;
        };
        report.hord_renames += 1;
        if shares_half(&old_site.leaves, &new_site.leaves) {
            report.precise_renames += 1;
        } else {
            let overlap = jaccard(&old_site.leaves, &new_site.leaves);
            let only_old = preview(old_site.leaves.difference(&new_site.leaves));
            let only_new = preview(new_site.leaves.difference(&old_site.leaves));
            eprintln!(
                "[identity] imprecise rename {old_path} {} -> {new_path} {} overlap {overlap:.2} only-old [{only_old}] only-new [{only_new}]",
                old_site.qname, new_site.qname
            );
        }
    }
}

fn is_function(kind: &str) -> bool {
    kind == "function_item" || kind == "function_signature_item"
}

fn preview<'a>(tokens: impl Iterator<Item = &'a String>) -> String {
    tokens
        .take(8)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `true` when `|intersection| / |union| >= 1/2`. Empty sets do not match.
fn shares_half(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    let union = a.union(b).count();
    union > 0 && a.intersection(b).count() * 2 >= union
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
    let mut out = Vec::new();
    snapshot::named_defs(adapter, tree, |at, oid, node, qname| {
        let simple = qname.as_str().rsplit("::").next().unwrap_or(qname.as_str());
        let mut leaves = BTreeSet::new();
        collect_leaf_tokens(tree, oid, simple, &mut leaves);
        out.push(Site {
            oid,
            at: at.to_vec(),
            kind: node.kind.as_str().to_string(),
            qname: qname.as_str().to_string(),
            leaves,
        });
    });
    out
}

fn tokens(text: &str, simple: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| tok.len() >= 2 && !tok.eq_ignore_ascii_case(simple))
        .map(|tok| tok.to_ascii_lowercase())
        .collect()
}

fn collect_leaf_tokens(tree: &NodeTree, oid: ObjectId, simple: &str, out: &mut BTreeSet<String>) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    if node.children.is_empty() {
        if let Some(bytes) = tree.stripped(oid)
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            out.extend(tokens(text, simple));
        }
        return;
    }
    for child in &node.children {
        collect_leaf_tokens(tree, *child, simple, out);
    }
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
