//! Workspace creation latency and 1,000 live workspaces (spec §6.1, §12 M3).
//!
//! On the landed cargo snapshot: `n` InMemory workspaces are begun one after
//! another and all kept alive (latency over a growing live set). Then every
//! workspace, each in its own task, reads one function and edits it. Then
//! 100 more are begun with all `n` live and holding overlays. Every 100th
//! workspace proposes, to show late proposals are no slower.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hord_core::RepoPath;
use hord_txn::{BeginOptions, Repo, Workspace};
use serde::Serialize;

use crate::rust;
use crate::sim::{actor, intent};

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceReport {
    pub live: usize,
    pub begin_p50_ms: f64,
    pub begin_p99_ms: f64,
    pub begin_max_ms: f64,
    pub first_100_mean_ms: f64,
    pub last_100_mean_ms: f64,
    pub loaded_p50_ms: f64,
    pub loaded_p99_ms: f64,
    pub usable: usize,
    pub edit_mean_ms: f64,
    pub propose_ms: Vec<f64>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn mean(xs: &[Duration]) -> Duration {
    if xs.is_empty() {
        return Duration::ZERO;
    }
    xs.iter().sum::<Duration>() / u32::try_from(xs.len()).unwrap_or(u32::MAX)
}

async fn begin(repo: &Repo, who: String) -> Result<(Workspace, Duration)> {
    let started = Instant::now();
    let ws = repo.begin(BeginOptions::at_head(actor(&who))).await?;
    Ok((ws, started.elapsed()))
}

/// Read one function in one of `files` and insert a statement in it.
async fn edit(mut ws: Workspace, files: Vec<RepoPath>, i: usize) -> (Workspace, bool) {
    let path = &files[i % files.len()];
    let ok = async {
        let defs = ws.definitions(path).await?;
        let funcs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind.as_str() == "function_item")
            .collect();
        for k in 0..funcs.len() {
            let def = funcs[(i / files.len() + k) % funcs.len()];
            let text = ws.read_definition(path, def.node).await?;
            let stmt = format!("let _live_{i} = {i}_u64;");
            if let Some(edited) = rust::insert_stmt(text.as_slice(), &stmt) {
                ws.write_definition(path, def.node, edited).await?;
                return Ok::<bool, hord_txn::Error>(true);
            }
        }
        Ok(false)
    }
    .await
    .unwrap_or(false);
    (ws, ok)
}

pub(crate) async fn run(repo: &Repo, n: usize, files: &[RepoPath]) -> Result<WorkspaceReport> {
    // The first begin after the head moves loads it; every later one is a
    // pointer copy.
    begin(repo, "m3-ws-warm".into()).await?;
    let mut live = Vec::with_capacity(n);
    let mut latencies = Vec::with_capacity(n);
    for i in 0..n {
        let (ws, took) = begin(repo, format!("m3-ws-{i}")).await?;
        latencies.push(took);
        live.push(ws);
    }
    let window = 100.min(n);
    let first = mean(&latencies[..window]);
    let last = mean(&latencies[n - window..]);
    let mut sorted = latencies.clone();
    sorted.sort();

    let started = Instant::now();
    let mut tasks = Vec::with_capacity(n);
    for (i, ws) in live.into_iter().enumerate() {
        tasks.push(tokio::spawn(edit(ws, files.to_vec(), i)));
    }
    let mut live = Vec::with_capacity(n);
    let mut usable = 0;
    for task in tasks {
        let (ws, ok) = task.await.context("workspace task")?;
        usable += usize::from(ok);
        live.push(ws);
    }
    let edit_mean = started.elapsed() / u32::try_from(n.max(1)).unwrap_or(u32::MAX);

    let mut loaded = Vec::with_capacity(100);
    let mut extra = Vec::with_capacity(100);
    for i in 0..100 {
        let (ws, took) = begin(repo, format!("m3-ws-loaded-{i}")).await?;
        loaded.push(took);
        extra.push(ws);
    }
    loaded.sort();

    let mut propose_ms = Vec::new();
    for ws in live.iter_mut().step_by(100) {
        let started = Instant::now();
        ws.propose(intent("m3 live workspace edit")).await?;
        propose_ms.push(ms(started.elapsed()));
    }
    let report = WorkspaceReport {
        live: live.len(),
        begin_p50_ms: ms(percentile(&sorted, 50.0)),
        begin_p99_ms: ms(percentile(&sorted, 99.0)),
        begin_max_ms: ms(sorted.last().copied().unwrap_or_default()),
        first_100_mean_ms: ms(first),
        last_100_mean_ms: ms(last),
        loaded_p50_ms: ms(percentile(&loaded, 50.0)),
        loaded_p99_ms: ms(percentile(&loaded, 99.0)),
        usable,
        edit_mean_ms: ms(edit_mean),
        propose_ms,
    };
    drop(extra);
    drop(live);
    Ok(report)
}

/// Gate: p99 begin < 5 ms with 0 → n live and with n loaded, the last 100
/// no slower than 3× the first 100 (+ 0.2 ms timer noise), every workspace
/// usable.
pub(crate) fn ok(report: &WorkspaceReport, n: usize) -> bool {
    report.live == n
        && report.begin_p99_ms < 5.0
        && report.loaded_p99_ms < 5.0
        && report.last_100_mean_ms <= report.first_100_mean_ms * 3.0 + 0.2
        && report.usable == n
}
