//! M3 validation (spec §12): the lander under concurrent agents.
//!
//! - **Concurrency simulation.** Cargo HEAD is imported from the corpus
//!   (every regular file; symlinks skipped) as one bootstrap change. 100
//!   synthetic agents, one tokio task and one InMemory workspace each, all
//!   on that change, read and then edit 1–5 functions (a new first statement
//!   in the body) and submit. A fixed seed picks targets so 80% of agents are
//!   disjoint and 20% overlap in pairs (write-write, read-write, or naming).
//!   `land_local` drains the queue with the stub verifier. The oracle
//!   ([`sim`], ADR 0012) knows the true overlap from what each agent did,
//!   not from the lander's sets. Gates: throughput ≥ 20 changes/s over
//!   `land_local`; false negatives = 0; false-positive changes ≤ 10%; every
//!   change that overlaps nothing landed before it lands (structural rebase,
//!   no replay).
//! - **Workspaces.** `begin` p50/p99 on the cargo snapshot while 1,000
//!   workspaces are kept live and then edited. Gate: p99 < 5 ms, no
//!   degradation from the first 100 to the last 100 ([`workspaces`]).
//! - **`Cargo.lock`.** Two concurrent dependency additions to cargo's own
//!   lockfile both land and give Cargo's canonical file ([`lock`]).
//!
//! The process exits nonzero if any gate fails.
//!
//! ```text
//! cargo run -p hord-eval-m3 --release --offline
//! cargo run -p hord-eval-m3 --release --offline -- --json target/m3-eval.json
//! cargo run -p hord-eval-m3 --release --offline -- --seed 7 --strict-reads
//! ```
//!
//! The corpus is `cargo.git` under `--cache`, `$HORD_CORPORA`, or
//! `~/.cache/hord/corpora` (the bare clone `bench/m0-eval` fetches).

#![forbid(unsafe_code)]

mod corpus;
mod lock;
mod rust;
mod sim;
mod workspaces;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use hord_core::RepoPath;
use hord_txn::{Repo, RepoConfig, RepoOptions, StubVerifier};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "hord-eval-m3", about = "M3 validation (spec §12)")]
struct Args {
    /// Bare corpus directory (default: $HORD_CORPORA or ~/.cache/hord/corpora).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Seed for target selection and edits.
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Synthetic agents in the simulation.
    #[arg(long, default_value_t = 100)]
    agents: usize,
    /// Share of agents that overlap another, in percent (rounded down to pairs).
    #[arg(long, default_value_t = 20)]
    overlap_percent: usize,
    /// Live workspaces in the workspace run.
    #[arg(long, default_value_t = 1_000)]
    workspaces: usize,
    /// Run the lander with `strict_reads` (write-read conflicts reported and
    /// counted as true overlaps for the false-negative gate).
    #[arg(long)]
    strict_reads: bool,
    /// Submit each change as soon as its agent proposes, so landing order
    /// depends on timing. By default agents work concurrently and submit in
    /// a seeded order.
    #[arg(long)]
    racy_submit: bool,
    /// Also write the full report as JSON to this path.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Report {
    corpus_commit: String,
    files: usize,
    symlinks_skipped: usize,
    bootstrap_secs: f64,
    snapshot_defs: usize,
    sim: sim::SimReport,
    workspaces: workspaces::WorkspaceReport,
    cargo_lock: Vec<lock::LockCase>,
    gates: Gates,
}

#[derive(Debug, Serialize)]
struct Gates {
    throughput: bool,
    false_negatives: bool,
    false_positive_rate: bool,
    disjoint_landed: bool,
    workspaces: bool,
    cargo_lock: bool,
    all: bool,
}

/// Scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() {
    let args = Args::parse();
    match run(&args) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(err) => {
            eprintln!("m3 eval error: {err:#}");
            std::process::exit(2);
        }
    }
}

fn run(args: &Args) -> Result<bool> {
    let git_dir = corpus::cache_dir(args.cache.as_deref())?.join("cargo.git");
    if !git_dir.is_dir() {
        anyhow::bail!("missing cargo corpus at {}", git_dir.display());
    }
    let corpus = corpus::load_head(&git_dir)?;
    eprintln!(
        "[corpus] cargo {} files {} (symlinks skipped {})",
        &corpus.commit[..12.min(corpus.commit.len())],
        corpus.files.len(),
        corpus.symlinks
    );
    let scratch =
        Scratch(std::env::temp_dir().join(format!("hord-m3-eval-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    std::fs::create_dir_all(&scratch.0).context("scratch dir")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let report = runtime.block_on(evaluate(args, &corpus, &scratch.0))?;
    print(&report, args);
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("write {}", path.display()))?;
        eprintln!("[report] wrote {}", path.display());
    }
    Ok(report.gates.all)
}

async fn evaluate(args: &Args, corpus: &corpus::Corpus, scratch: &Path) -> Result<Report> {
    let store = hord_store::Store::create(scratch.join("sim")).context("create store")?;
    let repo = Repo::from_store(
        store,
        RepoOptions {
            config: RepoConfig {
                strict_reads: args.strict_reads,
            },
            // Spec §12 M3: verification stubbed. Overlaps that rebase cleanly
            // land flagged; the product default parks them.
            verifier: Some(Arc::new(StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await?;
    let files: Vec<(RepoPath, Vec<u8>)> = corpus
        .files
        .iter()
        .filter_map(|(p, b)| p.parse::<RepoPath>().ok().map(|p| (p, b.clone())))
        .collect();
    let file_count = files.len();
    let started = Instant::now();
    let base = repo
        .bootstrap(
            files,
            sim::intent(&format!("import cargo {}", corpus.commit)),
            sim::actor("m3-seed"),
        )
        .await?;
    let bootstrap_secs = started.elapsed().as_secs_f64();
    eprintln!("[sim] bootstrap {file_count} files in {bootstrap_secs:.2}s; listing definitions");
    let snapshot = sim::load_snapshot(&repo, &corpus.files).await?;
    eprintln!(
        "[sim] {} definitions; running {} agents",
        snapshot.defs.len(),
        args.agents
    );
    let sim = sim::run(
        &repo,
        base,
        &snapshot,
        &sim::SimConfig {
            agents: args.agents,
            overlap_percent: args.overlap_percent,
            seed: args.seed,
            strict_reads: args.strict_reads,
            racy_submit: args.racy_submit,
        },
    )
    .await?;

    eprintln!("[workspaces] {} live", args.workspaces);
    let workspaces = workspaces::run(&repo, args.workspaces, &sim.target_files).await?;
    drop(repo);

    eprintln!("[cargo-lock] two concurrent dependency additions");
    let manifest = corpus.file("Cargo.toml").context("no Cargo.toml")?;
    let lockfile = corpus.file("Cargo.lock").context("no Cargo.lock")?;
    let cargo_lock = lock::run(scratch, manifest, lockfile).await?;

    let gates = {
        let throughput = sim.throughput >= 20.0;
        let false_negatives = sim.false_negatives.is_empty();
        let false_positive_rate = sim.false_positive_rate <= 0.10;
        let disjoint_landed = sim.disjoint_not_landed.is_empty() && sim.disjoint > 0;
        let workspaces = workspaces::ok(&workspaces, args.workspaces);
        let cargo_lock = cargo_lock.iter().all(|c| c.pass);
        Gates {
            throughput,
            false_negatives,
            false_positive_rate,
            disjoint_landed,
            workspaces,
            cargo_lock,
            all: throughput
                && false_negatives
                && false_positive_rate
                && disjoint_landed
                && workspaces
                && cargo_lock,
        }
    };
    Ok(Report {
        corpus_commit: corpus.commit.clone(),
        files: file_count,
        symlinks_skipped: corpus.symlinks,
        bootstrap_secs,
        snapshot_defs: snapshot.defs.len(),
        sim,
        workspaces,
        cargo_lock,
        gates,
    })
}

fn pass_fail(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

fn print(report: &Report, args: &Args) {
    let sim = &report.sim;
    let gates = &report.gates;
    let kinds = |kind: sim::PairKind| sim.pairs.iter().filter(|p| p.2 == kind).count();
    println!(
        "[sim] seed {} agents {} (disjoint {}, overlapping {}: pairs write-write {} read-write {} name {}) target pool {} strict_reads {} racy_submit {}",
        sim.seed,
        sim.agents,
        sim.planned_disjoint,
        sim.planned_overlapping,
        kinds(sim::PairKind::WriteWrite),
        kinds(sim::PairKind::ReadWrite),
        kinds(sim::PairKind::Name),
        sim.pool,
        sim.strict_reads,
        sim.racy_submit,
    );
    println!(
        "[sim] agents propose+submit {:.2}s (begin max {:.3} ms); landed {} conflicted {} rejected {} flagged {}",
        sim.agents_secs,
        sim.agent_begin_max_ms,
        sim.landed,
        sim.conflicted,
        sim.rejected,
        sim.flagged
    );
    println!(
        "[sim] throughput {:.1} changes/s ({} in {:.3}s; target ≥ 20) {}",
        sim.throughput,
        sim.agents,
        sim.land_secs,
        pass_fail(gates.throughput)
    );
    for f in &sim.false_negatives {
        println!(
            "[sim] false negative: agent {} landed after agent {} with no conflict (oracle {:?})",
            f.later, f.earlier, f.oracle
        );
        for line in &f.detail {
            eprintln!("[sim]   {line}");
        }
    }
    println!(
        "[sim] false negatives {} (must be 0) {}",
        sim.false_negatives.len(),
        pass_fail(gates.false_negatives)
    );
    for f in &sim.false_positive_pairs {
        println!(
            "[sim] false positive: agent {} vs landed agent {} reported {:?}, oracle disjoint",
            f.later, f.earlier, f.reported
        );
        for line in &f.detail {
            eprintln!("[sim]   {line}");
        }
    }
    for row in sim
        .changes
        .iter()
        .filter(|r| r.false_positive && r.set_conflicts.is_empty())
    {
        println!(
            "[sim] false positive: agent {} {} with no oracle overlap: {:?}",
            row.agent, row.status, row.merge_conflicts
        );
    }
    println!(
        "[sim] false positives {}/{} = {:.1}% (target ≤ 10%) {}",
        sim.false_positive_changes,
        sim.agents,
        sim.false_positive_rate * 100.0,
        pass_fail(gates.false_positive_rate)
    );
    for loss in &sim.identity_loss {
        println!(
            "[sim] identity loss: agent {} write_set lacks {:?}, has instead {:?}",
            loss.agent, loss.missing, loss.extra
        );
    }
    println!(
        "[sim] false positives not explained by identity loss {}/{} (diagnostic)",
        sim.false_positive_changes_clean_identity, sim.agents
    );
    println!(
        "[sim] structural rebase landed {}/{} oracle-disjoint changes without replay {}",
        sim.disjoint_landed,
        sim.disjoint,
        pass_fail(gates.disjoint_landed)
    );
    if !sim.disjoint_not_landed.is_empty() {
        println!(
            "[sim] disjoint but not landed: agents {:?}",
            sim.disjoint_not_landed
        );
    }
    if sim.unknown_landed_refs > 0 {
        println!(
            "[sim] {} set conflicts named a change outside the run",
            sim.unknown_landed_refs
        );
    }
    let ws = &report.workspaces;
    println!(
        "[workspaces] begin p50 {:.1} µs p99 {:.1} µs max {:.1} µs over {} (target < 5 ms)",
        ws.begin_p50_ms * 1000.0,
        ws.begin_p99_ms * 1000.0,
        ws.begin_max_ms * 1000.0,
        ws.live
    );
    println!(
        "[workspaces] {} live: begin mean first 100 {:.1} µs, last 100 {:.1} µs; with {} edited and live p50 {:.1} µs p99 {:.1} µs; usable {}/{}; edit wall/ws {:.3} ms; propose ms {:?} {}",
        ws.live,
        ws.first_100_mean_ms * 1000.0,
        ws.last_100_mean_ms * 1000.0,
        ws.live,
        ws.loaded_p50_ms * 1000.0,
        ws.loaded_p99_ms * 1000.0,
        ws.usable,
        args.workspaces,
        ws.edit_mean_ms,
        ws.propose_ms
            .iter()
            .map(|m| format!("{m:.0}"))
            .collect::<Vec<_>>(),
        pass_fail(gates.workspaces)
    );
    for case in &report.cargo_lock {
        println!(
            "[cargo-lock] {} (+{}, +{}): {:?}; head {}{}; conflicts {:?} {}",
            case.case,
            case.deps[0],
            case.deps[1],
            case.statuses,
            if case.head_matches {
                "matches cargo's order"
            } else {
                "differs"
            },
            case.first_difference
                .as_ref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default(),
            case.conflicts,
            pass_fail(case.pass)
        );
    }
    println!("m3 {}", pass_fail(gates.all));
}
