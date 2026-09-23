//! `hord submit`, `hord queue`, `hord land --local`, `hord conflicts`
//! (spec §6.7, §10.2).

use anyhow::{Result, bail};
use serde::Serialize;

use crate::output;
use crate::txn::{self, EntryView, block_on, entry_view, print_entry};

fn view(repo: &hord_txn::Repo, entry: &hord_txn::QueueEntry) -> EntryView {
    let record = block_on(repo.change(entry.change)).ok();
    entry_view(entry, record.as_ref())
}

pub fn run_submit(json: bool, change: String) -> Result<()> {
    let change = txn::parse_change(&change)?;
    let repo = txn::open()?;
    let entry = block_on(repo.submit(change))?;
    let view = view(&repo, &entry);
    if json {
        output::print_json(&view)?;
    } else {
        println!("queued {} at position {}", view.change, view.seq);
    }
    Ok(())
}

pub fn run_queue(json: bool, mine: bool) -> Result<()> {
    let repo = txn::open()?;
    let me = txn::actor();
    let me = crate::resolve::actor_id(&me).to_owned();
    let entries: Vec<EntryView> = block_on(repo.queue())?
        .iter()
        .map(|entry| view(&repo, entry))
        .filter(|view| !mine || view.actor == me)
        .collect();
    if json {
        output::print_json(&entries)?;
    } else if entries.is_empty() {
        println!("queue is empty");
    } else {
        for entry in &entries {
            print_entry(entry);
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct LandResult {
    processed: Vec<EntryView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<EntryView>,
    head: Option<String>,
}

pub fn run_land(json: bool, local: bool, change: Option<String>) -> Result<()> {
    if !local {
        bail!("only `hord land --local` is available: no remote lander is configured");
    }
    let change = change.as_deref().map(txn::parse_change).transpose()?;
    let repo = txn::open()?;
    if let Some(change) = change {
        block_on(repo.submit(change))?;
    }
    let processed: Vec<EntryView> = block_on(repo.land_local())?
        .iter()
        .map(|entry| view(&repo, entry))
        .collect();
    let target = match change {
        Some(change) => Some(view(&repo, &block_on(repo.status(change))?)),
        None => None,
    };
    let head = block_on(repo.head())?.change.map(txn::hex);
    let result = LandResult {
        processed,
        change: target,
        head,
    };
    if json {
        output::print_json(&result)?;
    } else {
        if result.processed.is_empty() {
            println!("nothing queued");
        }
        for entry in &result.processed {
            print_entry(entry);
        }
        match &result.head {
            Some(head) => println!("head {head}"),
            None => println!("head (none)"),
        }
    }
    Ok(())
}

pub fn run_conflicts(json: bool, change: String) -> Result<()> {
    let change = txn::parse_change(&change)?;
    let repo = txn::open()?;
    let report = block_on(repo.conflicts(change))?;
    let entry = block_on(repo.status(change)).ok();
    let view = txn::report_view(&repo, &report, entry.as_ref())?;
    if json {
        output::print_json(&view)?;
    } else {
        txn::print_report(&view);
    }
    Ok(())
}
