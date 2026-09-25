//! `hord arbitrate <change> --pick ours|theirs|<change> | --edit | --replay`
//! (spec §6.4 rung 3, §10.2): resolve a parked change through the
//! backend's `Arbitrate`.

use anyhow::Result;
use hord_api::proto::arbitration::Action;
use hord_api::{proto, wire};

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on, short};
use crate::workspaces::this_caller;

pub fn run(
    json: bool,
    target: &Target,
    change: String,
    pick: Option<String>,
    edit: bool,
    replay: bool,
    note: Option<String>,
) -> Result<()> {
    txn::parse_change(&change)?;
    let session = Session::open(target)?;
    if edit {
        return open_workspace(json, &session, &change);
    }
    let action = match pick.as_deref() {
        Some("ours") => Action::PickOurs(true),
        Some("theirs") => Action::PickTheirs(true),
        Some(other) => Action::Resolved(wire::id(txn::parse_change(other)?)),
        None if replay => Action::Replay(true),
        None => unreachable!("clap requires --pick, --edit, or --replay"),
    };
    let reply = block_on(session.backend().arbitrate(proto::ArbitrateRequest {
        change: change.clone(),
        action: Some(proto::Arbitration {
            action: Some(action.clone()),
        }),
        arbiter: Some(wire::actor(&txn::actor())),
        note,
        key_id: None,
        signature: None,
    }))?;
    if json {
        output::print_json(&reply)?;
    } else if matches!(action, Action::Replay(_)) {
        println!("replaying {} with the repository's harness", short(&change));
    } else {
        println!(
            "queued resolution {} of {}; it lands with both as parents",
            reply.change,
            short(&change)
        );
    }
    Ok(())
}

/// `--edit`: a workspace on head to resolve the conflict by hand.
fn open_workspace(json: bool, session: &Session, change: &str) -> Result<()> {
    let ws = block_on(session.workspaces().ws_new(proto::WsNewRequest {
        caller: Some(this_caller()),
        base: None,
        materialize: proto::Materialize::Clone.into(),
    }))?;
    if json {
        output::print_json(&ws)?;
    } else {
        println!("workspace {} on head, at", ws.id);
        println!("{}", ws.materialization);
        println!("see `hord conflicts {change}` for what collided; then");
        println!("  hord propose -w {} --intent <file>", ws.id);
        println!("  hord arbitrate {change} --pick <proposed change>");
    }
    Ok(())
}
