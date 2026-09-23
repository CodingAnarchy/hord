//! `hord blame <name|path:line>`
//!
//! Resolution and history come from [`hord_txn::Query`], which reads NodeIds
//! from the snapshots' identity (ADR 0017), so changes landed with `hord
//! land --local` resolve. A NodeId argument does not scan anything;
//! `path:line` reads one file at `head`. Evidence is the change's object ids
//! only.

use anyhow::{Context, Result};
use hord_core::ChangeRecord;
use serde::Serialize;

use crate::output;
use crate::resolve;
use crate::txn::{self, block_on};

#[derive(Debug, Serialize)]
struct BlameResult {
    node: String,
    history: Vec<BlameEntry>,
}

#[derive(Debug, Serialize)]
struct BlameEntry {
    change: String,
    intent: String,
    actor: String,
    evidence: Vec<String>,
}

pub fn run(json: bool, target: String) -> Result<()> {
    let repo = txn::open()?;
    let query = repo.query();
    let node = resolve::resolve_blame_target(&query, &target)?;
    let history = block_on(query.node_history(node))?;
    let mut entries = Vec::with_capacity(history.len());
    for id in history {
        let change: ChangeRecord =
            block_on(repo.change(id)).with_context(|| format!("change {id} in node history"))?;
        entries.push(BlameEntry {
            change: id.to_hex(),
            intent: change.intent.summary,
            actor: resolve::actor_id(&change.provenance.actor).to_owned(),
            evidence: change
                .evidence
                .iter()
                .map(hord_core::ObjectId::to_hex)
                .collect(),
        });
    }
    let result = BlameResult {
        node: node.to_string(),
        history: entries,
    };
    if json {
        output::print_json(&result)?;
    } else if result.history.is_empty() {
        println!("node {}", result.node);
        println!("(no history)");
    } else {
        println!("node {}", result.node);
        for (index, entry) in result.history.iter().enumerate() {
            if index > 0 {
                println!();
            }
            println!("{}", entry.change);
            println!("  intent: {}", entry.intent);
            println!("  actor: {}", entry.actor);
            if entry.evidence.is_empty() {
                println!("  evidence: (none)");
            } else {
                println!("  evidence: {}", entry.evidence.join(" "));
            }
        }
    }
    Ok(())
}
