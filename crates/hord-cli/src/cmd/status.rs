//! `hord status [-w <ws>]`: what `propose` would record now (spec §10.3).

use anyhow::Result;
use hord_api::proto;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{block_on, node_text};
use crate::workspaces::this_caller;

pub fn run(json: bool, target: &Target, workspace: Option<String>, paranoid: bool) -> Result<()> {
    let session = Session::open(target)?;
    let result = block_on(session.workspaces().status(proto::StatusRequest {
        caller: Some(this_caller()),
        workspace,
        paranoid,
    }))?;
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
    println!("reads: {}", result.reads);
    if !result.skipped.is_empty() {
        println!("skipped (untracked, not proposed):");
        for path in &result.skipped {
            println!("  {path}");
        }
    }
    if !result.changes {
        println!("no changes");
        return Ok(());
    }
    println!("ops:");
    for op in result.ops.iter().filter(|op| op.op != "tree") {
        println!("  {}", op.text);
    }
    println!("write_set:");
    for node in &result.write_set {
        println!("  {}", node_text(node));
    }
    println!("read_set:");
    for node in &result.read_set {
        println!("  {}", node_text(node));
    }
    println!("evidence (none)");
    Ok(())
}
