//! `hord propose [-w <ws>] --intent <file>` (spec §6.2, §10.2). Against a
//! remote, the proposal's new objects are pushed to it (§10.5.5).

use std::path::PathBuf;

use anyhow::{Context, Result};
use hord_api::proto;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::block_on;
use crate::workspaces::this_caller;

pub fn run(
    json: bool,
    target: &Target,
    workspace: Option<String>,
    intent_path: PathBuf,
) -> Result<()> {
    let text = std::fs::read_to_string(&intent_path)
        .with_context(|| format!("read intent file {}", intent_path.display()))?;
    // Checked here too, so a malformed file fails before any work.
    crate::intent::parse(&text)
        .with_context(|| format!("parse intent file {}", intent_path.display()))?;
    let session = Session::open(target)?;
    let result = block_on(session.workspaces().propose(proto::ProposeRequest {
        caller: Some(this_caller()),
        workspace,
        intent: text,
        intent_path: intent_path.display().to_string(),
    }))?;
    if json {
        output::print_json(&result)?;
    } else {
        println!("{}", result.change);
    }
    Ok(())
}
