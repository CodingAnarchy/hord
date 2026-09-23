//! `hord ws new [--base <snap|ref>] [--materialize clone|copy]`, `hord ws rm`,
//! `hord ws gc`.

use anyhow::{Result, anyhow};
use hord_store::WorkspaceId;
use hord_txn::{Base, BeginOptions, Materialization, MaterializeMode};
use serde::Serialize;

use crate::cli::Materialize;
use crate::txn::{self, block_on};
use crate::{output, repo};

#[derive(Debug, Serialize)]
struct WsNewResult {
    id: String,
    base: String,
    materialization: String,
    /// Mode actually used: `clone` or `copy` (ADR 0016).
    materialize: &'static str,
}

/// Create the workspace and check its base out into `.hord/ws/<id>/`.
pub fn run_new(json: bool, base: Option<String>, materialize: Materialize) -> Result<()> {
    let store = repo::discover()?;
    let base_id = repo::resolve_base(&store, base.as_deref())?;
    let repo_handle = txn::open_store(store)?;
    let mode = match materialize {
        Materialize::Clone => MaterializeMode::Clone,
        Materialize::Copy => MaterializeMode::Copy,
    };
    let ws = block_on(repo_handle.begin_directory_with(
        BeginOptions {
            base: Base::Snapshot(base_id),
            actor: txn::actor(),
            session: txn::session(),
        },
        mode,
    ))?;
    repo::set_current_workspace(repo_handle.store(), ws.id())?;
    let path = match ws.materialization() {
        Materialization::Directory { path } => path.display().to_string(),
        Materialization::InMemory => String::new(),
    };
    let result = WsNewResult {
        id: ws.id().to_string(),
        base: ws.base().to_hex(),
        materialization: path,
        materialize: ws.materialized_as().unwrap_or(mode).as_str(),
    };
    if json {
        output::print_json(&result)?;
    } else {
        println!("workspace {} ({})", result.id, result.materialize);
        println!("{}", result.materialization);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct WsRmResult {
    id: String,
    removed: bool,
}

pub fn run_rm(json: bool, id: String) -> Result<()> {
    let parsed: WorkspaceId = id
        .parse()
        .map_err(|_| anyhow!("invalid workspace id {id:?}"))?;
    let repo_handle = txn::open()?;
    let removed = block_on(repo_handle.remove_workspace(parsed))?;
    if !removed {
        return Err(anyhow!("unknown workspace {id}"));
    }
    let result = WsRmResult { id, removed };
    if json {
        output::print_json(&result)?;
    } else {
        println!("removed workspace {}", result.id);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct WsGcResult {
    removed_pristine: Vec<String>,
}

pub fn run_gc(json: bool) -> Result<()> {
    let repo_handle = txn::open()?;
    let removed = block_on(repo_handle.gc_pristine())?;
    let result = WsGcResult {
        removed_pristine: removed.iter().map(|s| s.to_hex()).collect(),
    };
    if json {
        output::print_json(&result)?;
    } else if result.removed_pristine.is_empty() {
        println!("nothing to collect");
    } else {
        for snapshot in &result.removed_pristine {
            println!("removed pristine {snapshot}");
        }
    }
    Ok(())
}
