//! `hord submit`, `hord queue`, `hord land --local`, `hord conflicts`
//! (spec §6.7, §10.2), over the session's [`hord_api::RepoBackend`].

use std::time::Duration;

use anyhow::{Result, bail};
use hord_api::proto::event::Kind;
use hord_api::{RepoBackend, proto, wire};
use tokio_stream::StreamExt;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on, node_text, short};

fn status_name(entry: &proto::QueueEntry) -> &'static str {
    match entry.status() {
        proto::QueueStatus::Queued => "queued",
        proto::QueueStatus::Landed => "landed",
        proto::QueueStatus::Conflicted => "conflicted",
        proto::QueueStatus::Rejected => "rejected",
        proto::QueueStatus::Parked => "parked",
        proto::QueueStatus::Replaying => "replaying",
        proto::QueueStatus::NeedsArbitration => "arbitration",
        proto::QueueStatus::Replayed => "replayed",
        proto::QueueStatus::Arbitrated => "arbitrated",
        proto::QueueStatus::Unspecified => "unknown",
    }
}

/// One line for a queue entry.
fn print_entry(entry: &proto::QueueEntry) {
    let mut line = format!(
        "{:>4} {} {:<10} {}",
        entry.seq,
        short(&entry.change),
        status_name(entry),
        entry.summary
    );
    if let Some(landed) = &entry.landed
        && landed != &entry.change
    {
        line.push_str(&format!(" (landed as {})", short(landed)));
    }
    if entry.conflicts > 0 {
        line.push_str(&format!(
            " [{} conflict{}{}]",
            entry.conflicts,
            if entry.conflicts == 1 { "" } else { "s" },
            if entry.hard { ", hard" } else { "" }
        ));
    }
    if let Some(reason) = &entry.reason {
        line.push_str(&format!(" ({reason})"));
    }
    println!("{line}");
}

pub fn run_submit(json: bool, target: &Target, change: String) -> Result<()> {
    txn::parse_change(&change)?;
    let session = Session::open(target)?;
    let reply = block_on(session.backend().submit(proto::SubmitRequest { change }))?;
    if json {
        output::print_json(&reply)?;
    } else if let Some(entry) = &reply.entry {
        println!("queued {} at position {}", entry.change, entry.seq);
    }
    Ok(())
}

pub fn run_queue(json: bool, target: &Target, mine: bool) -> Result<()> {
    let session = Session::open(target)?;
    let me = mine.then(|| txn::actor().id().to_owned());
    let reply = block_on(session.backend().queue(proto::QueueQuery {
        actor: me,
        ..Default::default()
    }))?;
    if json {
        output::print_json(&reply)?;
    } else if reply.entries.is_empty() {
        println!("queue is empty");
    } else {
        for entry in &reply.entries {
            print_entry(entry);
        }
    }
    Ok(())
}

/// `hord land --local`: in this process with `--no-daemon`; otherwise the
/// daemon's (or remote's) lander runs on its own, so this submits the
/// change, if given, and waits until nothing is queued.
pub fn run_land(json: bool, target: &Target, local: bool, change: Option<String>) -> Result<()> {
    if !local {
        bail!("pass --local: `hord land` lands in this repository (see `hord submit` for remotes)");
    }
    let change = change.as_deref().map(txn::parse_change).transpose()?;
    let session = Session::open(target)?;
    let result = match &session {
        Session::Direct { repo } => {
            if let Some(change) = change {
                block_on(repo.submit(change))?;
            }
            let processed: Vec<proto::QueueEntry> = block_on(repo.land_local())?
                .iter()
                .map(|entry| {
                    let record = block_on(repo.change(entry.change)).ok();
                    hord_txn::queue_entry_message(entry, record.as_ref())
                })
                .collect();
            let target = match change {
                Some(change) => Some(hord_txn::queue_entry_message(
                    &block_on(repo.status(change))?,
                    block_on(repo.change(change)).ok().as_ref(),
                )),
                None => None,
            };
            proto::LandResult {
                processed,
                change: target,
                head: block_on(repo.head())?.change.map(txn::hex),
            }
        }
        _ => wait_for_lander(session.backend().as_ref(), change)?,
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

/// Submit `change` (if any) and wait until the backend's queue is empty.
/// `processed` is every entry that settled meanwhile.
fn wait_for_lander(
    backend: &dyn RepoBackend,
    change: Option<hord_core::ChangeId>,
) -> Result<proto::LandResult> {
    let pending = |backend: &dyn RepoBackend| -> Result<Vec<proto::QueueEntry>> {
        Ok(block_on(backend.queue(proto::QueueQuery {
            pending_only: true,
            ..Default::default()
        }))?
        .entries)
    };
    let mut events = block_on(backend.events(proto::EventsRequest { from: None }))?;
    if let Some(change) = change {
        block_on(backend.submit(proto::SubmitRequest {
            change: wire::id(change),
        }))?;
    }
    let mut waiting: Vec<String> = pending(backend)?.into_iter().map(|e| e.change).collect();
    let mut settled = Vec::new();
    while !waiting.is_empty() {
        let next =
            block_on(async { tokio::time::timeout(Duration::from_secs(1), events.next()).await });
        match next {
            Ok(Some(Ok(envelope))) => {
                let id = match envelope.event.and_then(|e| e.kind) {
                    Some(Kind::Landed(l)) => l.submitted.unwrap_or(l.change),
                    Some(Kind::Parked(p)) => p.change,
                    Some(Kind::Rejected(r)) => r.change,
                    _ => continue,
                };
                if waiting.contains(&id) {
                    settled.push(id);
                }
            }
            Ok(Some(Err(err))) => return Err(err.into()),
            Ok(None) => bail!("the lander's event stream ended"),
            // Re-check the queue now and then, in case an event was missed.
            Err(_) => {}
        }
        waiting = pending(backend)?.into_iter().map(|e| e.change).collect();
    }
    let all = block_on(backend.queue(proto::QueueQuery::default()))?.entries;
    let processed = settled
        .iter()
        .filter_map(|id| all.iter().rev().find(|e| &e.change == id).cloned())
        .collect();
    let target = change.and_then(|change| {
        let id = wire::id(change);
        all.iter()
            .rev()
            .find(|e| e.change == id || e.landed.as_deref() == Some(id.as_str()))
            .cloned()
    });
    let head = block_on(backend.head(proto::HeadRequest {}))?.change;
    Ok(proto::LandResult {
        processed,
        change: target,
        head,
    })
}

pub fn run_conflicts(json: bool, target: &Target, change: String) -> Result<()> {
    txn::parse_change(&change)?;
    let session = Session::open(target)?;
    let entries = block_on(session.backend().queue(proto::QueueQuery {
        change: Some(change.clone()),
        ..Default::default()
    }))?
    .entries;
    let entry = entries.last().cloned();
    let report = match entry.as_ref().and_then(|e| e.report.clone()) {
        Some(report) => report,
        // Not processed yet: the set check against head, computed where the
        // store is.
        None => match &session {
            Session::Direct { repo } => {
                let id = txn::parse_change(&change)?;
                let report = block_on(repo.conflicts(id))?;
                hord_txn::conflict_report_message(&report)
            }
            _ => bail!(
                "change {change} has not been processed by the lander; `hord conflicts` \
                 explains a change once it has (see `hord queue`)"
            ),
        },
    };
    let result = proto::ConflictsResult {
        report: Some(report),
        entry,
    };
    if json {
        output::print_json(&result)?;
    } else {
        print_report(&result);
    }
    Ok(())
}

/// The conflict summary, replay attempts, and arbitration candidates.
fn print_escalation(escalation: &proto::Escalation) {
    if let Some(summary) = &escalation.summary {
        println!();
        for line in summary.text.lines() {
            println!("  {line}");
        }
        println!();
    }
    for attempt in &escalation.attempts {
        let outcome = match attempt.outcome() {
            proto::ReplayOutcome::Running => "running",
            proto::ReplayOutcome::Proposed => "proposed",
            proto::ReplayOutcome::GaveUp => "gave up",
            proto::ReplayOutcome::Killed => "killed (over its time budget)",
            proto::ReplayOutcome::OverBudget => "over budget",
            proto::ReplayOutcome::Failed => "failed",
            proto::ReplayOutcome::Unspecified => "unknown",
        };
        let mut line = format!(
            "replay {} by {}: {outcome}",
            attempt.attempt, attempt.harness
        );
        if let Some(change) = &attempt.change {
            line.push_str(&format!(" {}", short(change)));
        }
        if let Some(detail) = &attempt.detail {
            line.push_str(&format!(" ({detail})"));
        }
        println!("{line}");
    }
    for candidate in &escalation.candidates {
        let attempts: Vec<String> = candidate.attempts.iter().map(ToString::to_string).collect();
        println!(
            "candidate {} from attempt{} {} ({} ops)",
            candidate.change,
            if attempts.len() == 1 { "" } else { "s" },
            attempts.join(", "),
            candidate.ops
        );
    }
    if let Some(resolution) = &escalation.resolution {
        println!("resolution {} is in the queue", short(resolution));
    }
    if let Some(note) = &escalation.note {
        println!("note: {note}");
    }
}

fn kind_name(kind: proto::ConflictKind) -> &'static str {
    match kind {
        proto::ConflictKind::WriteWrite => "write-write",
        proto::ConflictKind::ReadWrite => "read-write",
        proto::ConflictKind::WriteRead => "write-read",
        proto::ConflictKind::Unspecified => "conflict",
    }
}

fn print_report(result: &proto::ConflictsResult) {
    let Some(report) = &result.report else {
        return;
    };
    let status = result.entry.as_ref().map_or("not submitted", status_name);
    println!("change {} ({status})", short(&report.change));
    let n = report.checked_against.len();
    println!(
        "checked against {n} landed change{} since its base",
        if n == 1 { "" } else { "s" }
    );
    if report.clean {
        println!("no conflicts");
    }
    for c in &report.conflicts {
        let mut what: Vec<String> = c.nodes.iter().map(node_text).collect();
        what.extend(c.paths.iter().map(|p| format!("file {p}")));
        println!(
            "{} with {} \"{}\": {}",
            kind_name(c.kind()),
            short(&c.landed),
            c.landed_summary,
            what.join(", ")
        );
    }
    for m in &report.merge {
        let nodes: Vec<String> = m.nodes.iter().map(node_text).collect();
        let nodes = if nodes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", nodes.join(", "))
        };
        let severity = match m.severity() {
            proto::MergeSeverity::Hard => "hard",
            _ => "soft",
        };
        println!("merge {severity} {}: {}{nodes}", m.path, m.reason);
    }
    if !report.adapter_merged.is_empty() {
        let paths: Vec<&str> = report
            .adapter_merged
            .iter()
            .map(|m| m.path.as_str())
            .collect();
        println!("merged by adapter: {}", paths.join(", "));
    }
    if let Some(reason) = &report.verification {
        println!("verification failed: {reason}");
    }
    for violation in &report.policy {
        let source = match &violation.rule {
            Some(rule) => format!("rule {rule:?}"),
            None => format!("[land] {}", violation.source),
        };
        println!(
            "policy: {source} requires {} ({})",
            violation.requirement, violation.evidence
        );
        for trigger in &violation.triggers {
            println!("  because {trigger}");
        }
    }
    if let Some(escalation) = result.entry.as_ref().and_then(|e| e.escalation.as_ref()) {
        print_escalation(escalation);
    }
    match result.entry.as_ref().map(status_name) {
        Some("conflicted") => println!(
            "parked: no replay harness ran; resolve with `hord arbitrate {}` or `hord replay`",
            report.change
        ),
        Some("replaying") => println!("replaying (spec §6.4 rung 2)"),
        Some("arbitration") => println!(
            "parked for arbitration: `hord arbitrate {} --pick ours|theirs|<candidate>`",
            report.change
        ),
        Some("parked") => println!("parked: attach the evidence policy requires and submit again"),
        Some("landed") if !report.clean => println!("landed, flagged for re-verification"),
        _ => {}
    }
}
