//! `hord ws new [--base <snap|ref|remote/ref>] [--materialize clone|copy]`,
//! `hord ws list`, `hord ws rm`, `hord ws gc` (the `Workspaces` service).

use anyhow::Result;
use hord_api::proto;

use crate::cli::Materialize;
use crate::output;
use crate::remotes::Remotes;
use crate::session::{Session, Target};
use crate::txn::{block_on, short};
use crate::workspaces::this_caller;

/// Create the workspace and check its base out into `.hord/ws/<id>/`.
///
/// `--base <remote>/<ref>` resolves the base on that remote and works
/// against it (ADR 0024 amendment).
pub fn run_new(
    json: bool,
    target: &Target,
    base: Option<String>,
    materialize: Materialize,
) -> Result<()> {
    let (target, base) = split_remote_base(target, base)?;
    let session = Session::open(&target)?;
    let response = block_on(
        session.workspaces().ws_new(proto::WsNewRequest {
            caller: Some(this_caller()),
            base,
            materialize: match materialize {
                Materialize::Clone => proto::Materialize::Clone,
                Materialize::Copy => proto::Materialize::Copy,
            }
            .into(),
        }),
    )?;
    if json {
        output::print_json(&response)?;
    } else {
        println!("workspace {} ({})", response.id, response.materialize);
        println!("{}", response.materialization);
    }
    Ok(())
}

/// `<remote>/<ref>` when `<remote>` is a configured remote: that remote,
/// and the ref alone.
fn split_remote_base(target: &Target, base: Option<String>) -> Result<(Target, Option<String>)> {
    let Some(spec) = &base else {
        return Ok((target.clone(), base));
    };
    let Some((name, rest)) = spec.split_once('/') else {
        return Ok((target.clone(), base));
    };
    let Ok(root) = crate::repo::discover_root() else {
        return Ok((target.clone(), base));
    };
    let remotes = Remotes::load(&root.join(hord_store::HORD_DIR))?;
    if !remotes.remotes.contains_key(name) {
        return Ok((target.clone(), base));
    }
    let target = Target {
        remote: Some(name.to_owned()),
        ..target.clone()
    };
    Ok((target, Some(rest.to_owned())))
}

pub fn run_list(json: bool, target: &Target) -> Result<()> {
    let session = Session::open(target)?;
    let response = block_on(session.workspaces().ws_list(proto::WsListRequest {}))?;
    if json {
        output::print_json(&response)?;
    } else if response.workspaces.is_empty() {
        println!("no workspaces");
    } else {
        for ws in &response.workspaces {
            let mark = if response.current.as_deref() == Some(ws.id.as_str()) {
                "*"
            } else {
                " "
            };
            println!(
                "{mark} {} {} {}",
                ws.id,
                short(&ws.base),
                ws.materialization
            );
        }
    }
    Ok(())
}

pub fn run_rm(json: bool, target: &Target, id: String) -> Result<()> {
    let session = Session::open(target)?;
    let response = block_on(session.workspaces().ws_rm(proto::WsRmRequest { id }))?;
    if json {
        output::print_json(&response)?;
    } else {
        println!("removed workspace {}", response.id);
    }
    Ok(())
}

pub fn run_gc(json: bool, target: &Target) -> Result<()> {
    let session = Session::open(target)?;
    let response = block_on(session.workspaces().ws_gc(proto::WsGcRequest {}))?;
    if json {
        output::print_json(&response)?;
    } else if response.removed_pristine.is_empty() {
        println!("nothing to collect");
    } else {
        for snapshot in &response.removed_pristine {
            println!("removed pristine {snapshot}");
        }
    }
    Ok(())
}
