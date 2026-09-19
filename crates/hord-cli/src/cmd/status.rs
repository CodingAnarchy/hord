//! `hord status [-w <ws>]`

use anyhow::Result;
use serde::Serialize;

use crate::output;
use crate::repo;

#[derive(Debug, Serialize)]
struct StatusResult {
    workspace: String,
    base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    materialization: String,
    ops: Vec<serde_json::Value>,
    read_set: Vec<String>,
    write_set: Vec<String>,
    evidence: Vec<String>,
    evidence_stale: bool,
}

pub fn run(json: bool, workspace: Option<String>) -> Result<()> {
    let store = repo::discover()?;
    let ws = repo::resolve_workspace(&store, workspace.as_deref())?;
    let head = store.head()?.map(|id| id.to_hex());
    let result = StatusResult {
        workspace: ws.id.to_string(),
        base: ws.base.to_hex(),
        head,
        materialization: ws.path.display().to_string(),
        // Access logs, ops, and evidence land with hord-txn / hord-verify (M3+).
        ops: Vec::new(),
        read_set: Vec::new(),
        write_set: Vec::new(),
        evidence: Vec::new(),
        evidence_stale: false,
    };
    if json {
        output::print_json(&result)?;
    } else {
        println!("workspace {}", result.workspace);
        println!("base {}", result.base);
        match &result.head {
            Some(head) => println!("head {head}"),
            None => println!("head (none)"),
        }
        println!("materialization {}", result.materialization);
        println!("ops (none)");
        println!("read_set (none)");
        println!("write_set (none)");
        println!("evidence (none)");
        println!(
            "evidence_stale {}",
            if result.evidence_stale {
                "true"
            } else {
                "false"
            }
        );
    }
    Ok(())
}
