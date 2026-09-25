//! `hord propose [-w <ws>] --intent <file>` (spec §6.2, §10.2). Against a
//! remote, the proposal's new objects are pushed to it (§10.5.5).
//!
//! The record is signed with the actor's key (spec §10.5.4). Against a
//! remote, the workspace service signs it before pushing. Through the
//! per-repo daemon or `--no-daemon`, the daemon builds it unsigned and this
//! command signs it with the user's key from `~/.hord/keys/` (created on
//! first use), stores the signed record, and prints its id; the daemon
//! verifies the signature when the change is submitted.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use hord_api::{proto, wire};
use hord_core::sign;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on};
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
    let mut result = block_on(session.workspaces().propose(proto::ProposeRequest {
        caller: Some(this_caller()),
        workspace,
        intent: text,
        intent_path: intent_path.display().to_string(),
    }))?;
    if !matches!(session, Session::Remote { .. }) {
        result.change = sign_stored(&session, &result.change)?;
    }
    if json {
        output::print_json(&result)?;
    } else {
        println!("{}", result.change);
    }
    Ok(())
}

/// Sign the stored record `change` as this process's actor, store the
/// signed record, and return its id.
fn sign_stored(session: &Session, change: &str) -> Result<String> {
    let backend = session.backend();
    let id = txn::parse_change(change)?;
    let mut record = txn::backend_change(backend.as_ref(), id)?;
    let signer = session.signer()?;
    if record.provenance.actor != signer.actor {
        bail!(
            "change {change} is authored by {}, not {}: not signing it",
            record.provenance.actor.id(),
            signer.actor.id()
        );
    }
    sign::sign_change(&mut record, &signer.key)?;
    let object = wire::object(hord_encoding::encode(&record)?);
    let signed = wire::verified_object(&object)?;
    block_on(backend.put_objects(proto::PutObjectsRequest {
        objects: vec![object],
    }))
    .context("store the signed change")?;
    Ok(wire::id(signed))
}
