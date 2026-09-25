//! `--remote-submit`: the concurrency simulation's changes, submitted to and
//! landed by a repository that did not propose them (the M4 remote case).
//!
//! The agents of [`crate::sim`] propose on one [`Repo`], which is then
//! closed without submitting anything. A freshly opened [`Repo`] on the same
//! store submits every change in a seeded order and drains the queue with
//! `land_local`. That lander has none of the proposer's in-memory state, so
//! it must validate each change itself (spec §3.5). Gate: throughput over
//! `land_local` ≥ 20 changes/s, the same measure as the in-process run.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hord_core::{ChangeId, RepoPath};
use hord_txn::{QueueStatus, Repo};
use serde::Serialize;

use crate::sim;

/// Outcome of the remote-submit run.
#[derive(Debug, Serialize)]
pub(crate) struct RemoteReport {
    pub seed: u64,
    pub agents: usize,
    pub propose_secs: f64,
    pub submit_secs: f64,
    pub land_secs: f64,
    pub throughput: f64,
    pub landed: usize,
    pub conflicted: usize,
    pub rejected: usize,
    pub pass: bool,
}

pub(crate) async fn run(
    dir: &Path,
    files: Vec<(RepoPath, Vec<u8>)>,
    corpus_files: &[(String, Vec<u8>)],
    config: &sim::SimConfig,
) -> Result<RemoteReport> {
    let store = hord_store::Store::create(dir).context("create store")?;
    let proposer = Repo::from_store(store, sim::repo_options(config.strict_reads, false)).await?;
    let base = proposer
        .bootstrap(files, sim::intent("import cargo"), sim::actor("m3-seed"))
        .await?;
    let snapshot = sim::load_snapshot(&proposer, corpus_files).await?;
    let plan = sim::plan(
        &snapshot,
        config.agents,
        config.overlap_percent,
        config.seed,
    )?;
    eprintln!(
        "[remote] {} agents propose on one repo; another submits and lands",
        config.agents
    );
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(config.agents);
    for agent in plan.agents.iter().cloned() {
        let repo = proposer.clone();
        let seed = config.seed;
        tasks.push(tokio::spawn(async move {
            sim::agent_work(&repo, base, agent, seed).await
        }));
    }
    let mut changes: Vec<ChangeId> = Vec::with_capacity(config.agents);
    for task in tasks {
        changes.push(task.await.context("agent task")??.1);
    }
    let propose_secs = started.elapsed().as_secs_f64();
    drop(proposer);

    let lander = Repo::open_with(dir, sim::repo_options(config.strict_reads, false)).await?;
    let mut order: Vec<usize> = (0..changes.len()).collect();
    sim::Rng::new(config.seed.rotate_left(17)).shuffle(&mut order);
    let started = Instant::now();
    for i in order {
        lander.submit(changes[i]).await?;
    }
    let submit_secs = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let done = lander.land_local().await?;
    let land_secs = started.elapsed().as_secs_f64();
    if done.len() != changes.len() {
        bail!(
            "land_local processed {} of {} changes",
            done.len(),
            changes.len()
        );
    }
    let count = |f: fn(&QueueStatus) -> bool| done.iter().filter(|e| f(&e.status)).count();
    let throughput = changes.len() as f64 / land_secs;
    Ok(RemoteReport {
        seed: config.seed,
        agents: changes.len(),
        propose_secs,
        submit_secs,
        land_secs,
        throughput,
        landed: count(|s| matches!(s, QueueStatus::Landed { .. })),
        conflicted: count(|s| matches!(s, QueueStatus::Conflicted)),
        rejected: count(|s| matches!(s, QueueStatus::Rejected { .. })),
        pass: throughput >= 20.0,
    })
}
