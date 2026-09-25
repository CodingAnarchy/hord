//! `hord verify [-w <ws>] [--plan-only]` (spec §10.2, §10.3): the checks
//! head's policy requires for the workspace's proposal, planned with reuse,
//! run, and attached as evidence to the proposal's result snapshot, where
//! the lander finds and reuses them (ADR 0025). `--plan-only` tells an
//! agent which tests will run so it can run them itself first. Exits 1 when
//! a check fails.

use anyhow::Result;
use hord_api::proto;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::block_on;
use crate::workspaces::this_caller;

pub fn run(json: bool, target: &Target, workspace: Option<String>, plan_only: bool) -> Result<()> {
    let session = Session::open(target)?;
    let result = block_on(session.workspaces().verify(proto::WsVerifyRequest {
        caller: Some(this_caller()),
        workspace,
        plan_only,
    }))?;
    if json {
        output::print_json(&result)?;
    } else {
        println!("workspace {}", result.workspace);
        println!(
            "snapshot {} (policy: {})",
            result.snapshot, result.policy_source
        );
        if result.requirements.is_empty() {
            println!("nothing required");
        } else {
            println!("requires {}", result.requirements.join(", "));
        }
        for check in &result.reused {
            let outcome = if check.passed { "pass" } else { "FAIL" };
            println!("  reused {outcome}: {}", check.command);
        }
        for check in &result.checks {
            let verb = if plan_only { "would run" } else { "ran" };
            println!("  {verb}: {}", check.command);
        }
        for note in &result.notes {
            println!("  note: {note}");
        }
        match (result.passed, &result.reason) {
            (Some(true), _) => println!("pass ({} evidence)", result.evidence.len()),
            (Some(false), Some(reason)) => println!("FAIL\n{reason}"),
            (Some(false), None) => println!("FAIL"),
            (None, _) => {}
        }
    }
    if result.passed == Some(false) {
        std::process::exit(1);
    }
    Ok(())
}
