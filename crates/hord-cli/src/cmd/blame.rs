//! `hord blame <name|path:line|NodeId>`
//!
//! A NodeId or a qualified name (resolved at head) goes through the
//! session's backend (`resolve_name`, `node_history`). `path:line` parses
//! one file at head, which needs the store: it runs in this process (a
//! running daemon is asked to stop first), and not against a remote.
//! Evidence is the change's object ids only.

use anyhow::{Result, bail};
use hord_api::proto;

use crate::output;
use crate::resolve::{self, BlameTarget};
use crate::session::{Session, Target};
use crate::txn::{self, block_on};

pub fn run(json: bool, target: &Target, spec: String) -> Result<()> {
    let parsed = resolve::parse_blame_target(&spec)?;
    let session = match parsed {
        BlameTarget::Line { .. } => {
            if target.remote.is_some() {
                bail!("blame by path:line needs the local store; not supported against a remote");
            }
            Session::direct(&crate::repo::discover_root()?)?
        }
        _ => Session::open(target)?,
    };
    let backend = session.backend();
    let node = match parsed {
        BlameTarget::Node(id) => id,
        BlameTarget::Name(name) => txn::backend_resolve_node(backend.as_ref(), &name)?,
        BlameTarget::Line { path, line } => match &session {
            Session::Direct { repo } => block_on(repo.query().resolve_line(path, line))?,
            _ => unreachable!("path:line opens the store"),
        },
    };
    let history = block_on(backend.node_history(proto::NodeHistoryRequest {
        node: node.to_string(),
    }))?;
    let mut entries = Vec::with_capacity(history.changes.len());
    for summary in &history.changes {
        let id = hord_api::wire::object_id("change", &summary.change)?;
        let record = txn::backend_change(backend.as_ref(), id)?;
        entries.push(proto::BlameEntry {
            change: summary.change.clone(),
            intent: record.intent.summary,
            actor: resolve::actor_id(&record.provenance.actor).to_owned(),
            evidence: record.evidence.iter().map(|e| e.to_hex()).collect(),
        });
    }
    let result = proto::BlameResult {
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
