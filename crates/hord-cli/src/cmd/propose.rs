//! `hord propose [-w <ws>] --intent <file>` (spec §6.2, §10.2).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::txn::{self, block_on, hex};
use crate::{intent, output, repo};

#[derive(Debug, Serialize)]
struct ProposeResult {
    change: String,
    workspace: String,
    base: String,
    result: String,
    parents: Vec<String>,
    summary: String,
    ops: usize,
    write_set: Vec<String>,
    read_set: Vec<String>,
    reads: &'static str,
}

pub fn run(json: bool, workspace: Option<String>, intent_path: PathBuf) -> Result<()> {
    let text = std::fs::read_to_string(&intent_path)
        .with_context(|| format!("read intent file {}", intent_path.display()))?;
    let file = intent::parse(&text)
        .with_context(|| format!("parse intent file {}", intent_path.display()))?;
    let store = repo::discover()?;
    let meta = repo::resolve_workspace(&store, workspace.as_deref())?;
    let repo = txn::open_store(store)?;
    let mut ws = block_on(repo.open_workspace(meta.id, txn::actor(), txn::session()))?;
    for read in file.reads {
        ws.declare_read(read);
    }
    let proposal = block_on(ws.propose(file.intent))?;
    let record = &proposal.record;
    let result = ProposeResult {
        change: hex(proposal.change),
        workspace: meta.id.to_string(),
        base: hex(record.base),
        result: hex(record.result),
        parents: record.parents.iter().map(|p| hex(*p)).collect(),
        summary: record.intent.summary.clone(),
        ops: record.ops.len(),
        write_set: record.write_set.iter().map(ToString::to_string).collect(),
        read_set: record.read_set.iter().map(ToString::to_string).collect(),
        reads: reads_label(ws.access_log().reads_observed),
    };
    if json {
        output::print_json(&result)?;
    } else {
        println!("{}", result.change);
    }
    Ok(())
}

/// ADR 0012: a `Directory` workspace cannot see reads made by other tools.
pub fn reads_label(observed: bool) -> &'static str {
    if observed { "observed" } else { "unobserved" }
}
