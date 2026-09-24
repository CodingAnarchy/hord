//! `hord status [-w <ws>]`: what `propose` would record now (spec §10.3).

use anyhow::Result;
use hord_core::Intent;
use serde::Serialize;
use serde_json::Value;

use crate::cmd::propose::reads_label;
use crate::txn::{self, Names, block_on, hex};
use crate::{output, repo};

#[derive(Debug, Serialize)]
struct StatusResult {
    workspace: String,
    base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    materialization: String,
    /// `observed`, or `unobserved` for a directory workspace (ADR 0012).
    reads: &'static str,
    ops: Vec<Value>,
    read_set: Vec<String>,
    write_set: Vec<String>,
    evidence: Vec<String>,
    evidence_stale: bool,
    /// Untracked paths `propose` leaves out: ignored by a `.gitignore` of
    /// the base or the built-in default, or not a regular file. A directory
    /// ends in `/` and stands for everything under it.
    skipped: Vec<String>,
}

pub fn run(json: bool, workspace: Option<String>, paranoid: bool) -> Result<()> {
    let store = repo::discover()?;
    let meta = repo::resolve_workspace(&store, workspace.as_deref())?;
    let repo = txn::open_store(store)?;
    let head = block_on(repo.head())?.change.map(hex);
    let mut ws = block_on(repo.open_workspace(meta.id, txn::actor(), txn::session()))?;
    ws.set_paranoid(paranoid);
    let preview = Intent {
        summary: "(status preview)".into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    };
    let record = match block_on(ws.preview(preview)) {
        Ok(proposal) => Some(proposal.record),
        Err(hord_txn::Error::NothingToPropose) => None,
        Err(err) => return Err(err.into()),
    };
    let reads = reads_label(ws.access_log().reads_observed);
    let skipped = ws
        .skipped()
        .iter()
        .map(|path| {
            let mut shown = path.to_string();
            if meta.path.join(&shown).is_dir() {
                shown.push('/');
            }
            shown
        })
        .collect();
    let result = StatusResult {
        workspace: meta.id.to_string(),
        base: hex(meta.base),
        head,
        materialization: meta.path.display().to_string(),
        reads,
        ops: record
            .iter()
            .flat_map(|r| r.ops.iter().map(txn::op_view))
            .collect(),
        read_set: record
            .iter()
            .flat_map(|r| r.read_set.iter().map(ToString::to_string))
            .collect(),
        write_set: record
            .iter()
            .flat_map(|r| r.write_set.iter().map(ToString::to_string))
            .collect(),
        evidence: Vec::new(),
        evidence_stale: false,
        skipped,
    };
    if json {
        return output::print_json(&result);
    }
    println!("workspace {}", result.workspace);
    println!("base {}", result.base);
    match &result.head {
        Some(head) => println!("head {head}"),
        None => println!("head (none)"),
    }
    println!("materialization {}", result.materialization);
    println!("reads: {reads}");
    if !result.skipped.is_empty() {
        println!("skipped (untracked, not proposed):");
        for path in &result.skipped {
            println!("  {path}");
        }
    }
    let Some(record) = record else {
        println!("no changes");
        return Ok(());
    };
    let names = Names::for_records(&repo, &[&record])?;
    println!("ops:");
    for op in &record.ops {
        if !matches!(op, hord_core::Op::Tree { .. }) {
            println!("  {}", txn::op_text(op, &names));
        }
    }
    println!("write_set:");
    for node in &record.write_set {
        println!("  {}", names.view(*node).text());
    }
    println!("read_set:");
    for node in &record.read_set {
        println!("  {}", names.view(*node).text());
    }
    println!("evidence (none)");
    Ok(())
}
