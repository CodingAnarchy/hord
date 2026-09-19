//! `hord ws new [--base <snap|ref>]`

use anyhow::Result;
use serde::Serialize;

use crate::output;
use crate::repo;

#[derive(Debug, Serialize)]
struct WsNewResult {
    id: String,
    base: String,
    materialization: String,
}

pub fn run_new(json: bool, base: Option<String>) -> Result<()> {
    let store = repo::discover()?;
    let base_id = repo::resolve_base(&store, base.as_deref())?;
    let ws = store.create_workspace(base_id)?;
    repo::set_current_workspace(&store, ws.id)?;
    let result = WsNewResult {
        id: ws.id.to_string(),
        base: ws.base.to_hex(),
        materialization: ws.path.display().to_string(),
    };
    if json {
        output::print_json(&result)?;
    } else {
        println!("workspace {}", result.id);
        println!("{}", result.materialization);
    }
    Ok(())
}
