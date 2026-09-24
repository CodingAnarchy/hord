//! M4 selection safety and efficiency (spec §12, ADR 0023).
//!
//! On the last `--commits` first-parent commits of the cargo corpus:
//!
//! 1. **Prepare.** Land every commit in a hord repository (so NodeIds are
//!    carried by hord), and record each commit's write set, impact set, the
//!    change facts since its coverage checkpoint, and the functions it wrote.
//! 2. **Full-suite sample.** Run the literal `cargo test --workspace` on
//!    `--full-sample` unmutated commits; tests that fail there are
//!    quarantined (and listed in the report).
//! 3. **Coverage.** Per checkpoint (every `--coverage-every` commits, at the
//!    base of its first commit), per-test coverage with `cargo-llvm-cov`,
//!    stored as coverage evidence in the hord store (ADR 0022).
//! 4. **Commits.** Per commit, on `--jobs` workers: hord's selection on the
//!    unmutated commit (efficiency), then one seeded fault in a function the
//!    commit wrote that type-checks; (b) the selection and, unless (b)
//!    detected it, (a) every test of the affected packages. A miss is a
//!    fault (a) detects and (b) does not.
//!
//! Gates: zero misses; median selected ≤ 20% of the suite over unmutated
//! commits with write set ≤ 5. Every phase writes its results under
//! `--work`, so a rerun resumes; `--budget` stops starting new work once
//! spent.
//!
//! Do not start it with a shell's `&`: a background job ignores SIGINT,
//! the ignore is inherited by every test process, and cargo's
//! `death::ctrl_c_kills_everyone` then hangs until the command timeout.
//!
//! ```text
//! cargo run -p hord-eval-m4 --release --offline -- --commits 10 --coverage-every 10 --full-sample 1
//! cargo run -p hord-eval-m4 --release --offline -- --budget 12h --json target/m4-eval.json
//! ```

#![forbid(unsafe_code)]

mod fault;
mod git;
mod prepare;
mod run;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use hord_core::Actor;
use hord_store::Store;
use hord_verify::{Checkout as VerifyCheckout, CoverageRecord, TestRef, find_coverage};
use hord_verify_rust::coverage::{collect, record_evidence};
use hord_verify_rust::{CoverageOptions, detect_toolchain};
use serde::{Deserialize, Serialize};

use crate::prepare::Prepared;
use crate::run::{CommitResult, Ctx};

#[derive(Debug, Parser)]
#[command(
    name = "hord-eval-m4",
    about = "M4 selection safety and efficiency (spec §12, ADR 0023)"
)]
struct Args {
    /// Bare corpus directory (default: $HORD_CORPORA or ~/.cache/hord/corpora).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Working directory (checkouts, target dirs, results).
    #[arg(long, default_value = "target/m4-eval")]
    work: PathBuf,
    /// Commits to evaluate: the last N first-parent commits.
    #[arg(long, default_value_t = 500)]
    commits: usize,
    /// Seed for fault placement and the sample.
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Parallel workers (each has its own checkout and target dir).
    #[arg(long, default_value_t = 2)]
    jobs: usize,
    /// Tests run at once during a coverage run.
    #[arg(long, default_value_t = 10)]
    coverage_jobs: usize,
    /// Commits per coverage checkpoint.
    #[arg(long, default_value_t = 50)]
    coverage_every: usize,
    /// Unmutated commits whose literal full `cargo test` is run.
    #[arg(long, default_value_t = 20)]
    full_sample: usize,
    /// Stop starting new work after this long (`90m`, `12h`, `600s`).
    #[arg(long)]
    budget: Option<String>,
    /// Kill one cargo command after this many minutes.
    #[arg(long, default_value_t = 30)]
    command_timeout_mins: u64,
    /// Kill a cargo command that prints nothing for this many minutes.
    #[arg(long, default_value_t = 5)]
    idle_timeout_mins: u64,
    /// Evaluate only commits whose index is `i` modulo `n` (`--shard 0/4`),
    /// so CI machines split one run; the sample and coverage run only where
    /// a shard needs them. Merge by pointing `--work` at the union of the
    /// `results/` directories and rerunning (finished commits are skipped).
    #[arg(long)]
    shard: Option<String>,
    /// Print commit N's write set, impact set, touched definitions, and
    /// the tests each one selects, then exit (after prepare and coverage).
    #[arg(long)]
    explain: Option<usize>,
    /// Write the report as JSON here.
    #[arg(long)]
    json: Option<PathBuf>,
}

fn parse_budget(text: &str) -> Result<Duration> {
    let (num, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let n: u64 = num.parse().with_context(|| format!("budget {text:?}"))?;
    Ok(Duration::from_secs(match unit {
        "h" => n * 3600,
        "m" => n * 60,
        "s" | "" => n,
        _ => bail!("budget unit in {text:?}: use h, m, or s"),
    }))
}

fn corpus_dir(cache: Option<PathBuf>) -> Result<PathBuf> {
    let base = match cache {
        Some(p) => p,
        None => match std::env::var_os("HORD_CORPORA") {
            Some(p) => PathBuf::from(p),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache/hord/corpora")
            }
        },
    };
    let dir = base.join("cargo.git");
    if !dir.exists() {
        bail!(
            "no corpus at {} (run hord-eval once to fetch it)",
            dir.display()
        );
    }
    Ok(dir)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct SampleResult {
    commit: String,
    failed: Vec<String>,
    elapsed_ms: u64,
    timed_out: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CoverageResult {
    checkpoint: usize,
    commit: String,
    elapsed_ms: u64,
    tests: usize,
    failed_tests: usize,
    instrumented: usize,
    log: String,
}

#[derive(Debug, Default, Serialize)]
struct FallbackCost {
    /// Commits it fired on.
    commits: usize,
    /// Of those, with write set <= 5.
    small_commits: usize,
    /// Median extra tests it selected, over the commits it fired on.
    median_extra_tests: Option<f64>,
    /// Median selected share on those commits, with and without it.
    median_share: Option<f64>,
    median_share_without: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Report {
    corpus_commits: usize,
    evaluated: usize,
    complete: bool,
    prepare_secs: f64,
    quarantine: Vec<String>,
    sample: Vec<SampleResult>,
    coverage: Vec<CoverageResult>,
    faults_injected: usize,
    faults_detected: usize,
    /// Of those, detected by the probe alone.
    probe_detected: usize,
    /// Faults where (b) runs all of (a): no miss possible, not run.
    b_covers_a: usize,
    no_fault: usize,
    misses: Vec<CommitResult>,
    confirmed_misses: usize,
    efficiency_commits: usize,
    efficiency_median: Option<f64>,
    efficiency_median_all: Option<f64>,
    fallbacks: BTreeMap<String, usize>,
    /// Per fallback kind: how often it fires and what it costs.
    fallback_costs: BTreeMap<String, FallbackCost>,
    /// Median selected share with every fallback off (coverage only), over
    /// write set <= 5: what the fallbacks cost in total.
    efficiency_median_without_fallbacks: Option<f64>,
    mean_commit_secs: f64,
    projected_full_run_hours: f64,
    safety_gate: bool,
    efficiency_gate: bool,
    commits: Vec<CommitResult>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();
    let budget = args.budget.as_deref().map(parse_budget).transpose()?;
    let over_budget = || budget.is_some_and(|b| started.elapsed() > b);
    let corpus = corpus_dir(args.cache.clone())?;
    fs::create_dir_all(&args.work)?;
    let work = args.work.canonicalize()?;
    let timeout = Duration::from_secs(args.command_timeout_mins * 60);
    let (shard, shards) = match &args.shard {
        None => (0, 1),
        Some(text) => {
            let (i, n) = text.split_once('/').context("--shard i/n")?;
            let (i, n): (usize, usize) = (i.parse()?, n.parse()?);
            if n == 0 || i >= n {
                bail!("--shard {text}: need 0 <= i < n");
            }
            (i, n)
        }
    };
    let in_shard = |index: usize| index % shards == shard;

    // 1. Prepare.
    let commits = git::first_parent_commits(&corpus, args.commits)?;
    // CBOR: the facts have maps keyed by NodeId and RepoPath.
    let prepared_path = work.join("prepared.cbor");
    let cached: Option<Prepared> = fs::read(&prepared_path)
        .ok()
        .and_then(|b| hord_encoding::decode(&b).ok());
    let prepared: Prepared = match cached
        .filter(|p| p.commits.iter().map(|c| &c.commit).eq(commits.iter()))
    {
        Some(p) => {
            eprintln!("[prepare] reusing {}", prepared_path.display());
            p
        }
        None => {
            let p = prepare::prepare(&corpus, &work.join("hord"), &commits, args.coverage_every)
                .await?;
            fs::write(&prepared_path, hord_encoding::encode(&p)?)?;
            p
        }
    };

    let workers: Vec<run::Worker> = (0..args.jobs.max(1))
        .map(|i| run::worker(&work, &corpus, i))
        .collect::<Result<_>>()?;
    let toolchain = detect_toolchain(&workers[0].checkout.root)?;
    if !toolchain.has(hord_verify_rust::coverage::LLVM_COV) {
        bail!("cargo-llvm-cov is required (ADR 0022)");
    }

    // 2. Full-suite sample, in parallel over the workers.
    let mut sample_rng = fault::Rng::new(args.seed, "full-sample");
    let mut order: Vec<usize> = (0..prepared.commits.len()).collect();
    sample_rng.shuffle(&mut order);
    let sample_commits: Vec<String> = order
        .iter()
        .take(args.full_sample)
        .map(|i| prepared.commits[*i].commit.clone())
        .collect();
    let sample_dir = work.join("sample");
    let pending: Vec<String> = sample_commits
        .iter()
        .enumerate()
        .filter(|(i, _)| in_shard(*i))
        .map(|(_, c)| c)
        .filter(|c| !sample_dir.join(format!("{c}.json")).exists())
        .cloned()
        .collect();
    run_parallel(&workers, pending, &over_budget, |worker, commit| {
        eprintln!("[sample] full cargo test on {}", &commit[..10]);
        let (failed, elapsed_ms, timed_out) = run::full_suite(worker, &commit, timeout)?;
        write_json(
            &sample_dir.join(format!("{commit}.json")),
            &SampleResult {
                commit: commit.clone(),
                failed,
                elapsed_ms,
                timed_out,
            },
        )
    });
    let sample: Vec<SampleResult> = sample_commits
        .iter()
        .filter_map(|c| read_json(&sample_dir.join(format!("{c}.json"))))
        .collect();
    let quarantine_names: BTreeSet<String> = sample
        .iter()
        .flat_map(|s| s.failed.iter().cloned())
        .collect();

    // 3 and 4, checkpoint by checkpoint.
    let results_dir = work.join("results");
    let coverage_dir = work.join("coverage");
    let mut coverage_results = Vec::new();
    for (j, checkpoint) in prepared.checkpoints.iter().enumerate() {
        let group: Vec<&prepare::CommitFacts> = prepared
            .commits
            .iter()
            .filter(|c| c.checkpoint == j)
            .collect();
        let todo: Vec<&prepare::CommitFacts> = group
            .iter()
            .copied()
            .filter(|c| {
                in_shard(c.index) && !results_dir.join(format!("{}.json", c.index)).exists()
            })
            .collect();
        let cov_path = coverage_dir.join(format!("{j}.json"));
        if todo.is_empty() || over_budget() {
            if let Some(r) = read_json(&cov_path) {
                coverage_results.push(r);
            }
            continue;
        }
        let record = {
            let store = Store::open(work.join("hord"))?;
            // The record is also kept as a file: prepare re-creates the store.
            let saved = coverage_dir.join(format!("{j}.cbor"));
            let tc = toolchain.id()?;
            let mut found = find_coverage(&store, [checkpoint.snapshot], tc)?.map(|f| f.1);
            if found.is_none() {
                found = fs::read(&saved)
                    .ok()
                    .and_then(|b| hord_encoding::decode::<CoverageRecord>(&b).ok())
                    .filter(|r| r.snapshot == checkpoint.snapshot && r.toolchain == tc);
            }
            match found {
                Some(record) => record,
                None => {
                    eprintln!("[coverage] checkpoint {j} at {}", &checkpoint.commit[..10]);
                    let w = &workers[0];
                    w.checkout.checkout(&checkpoint.commit)?;
                    let run = collect(
                        &VerifyCheckout {
                            root: w.checkout.root.clone(),
                            snapshot: checkpoint.snapshot,
                        },
                        &toolchain,
                        &checkpoint.defs,
                        &CoverageOptions {
                            packages: None,
                            target_dir: work.join("coverage-target"),
                            jobs: args.coverage_jobs,
                            test_timeout: Duration::from_secs(600),
                            skip: BTreeSet::new(),
                        },
                    )?;
                    record_evidence(
                        &run,
                        &store,
                        Actor::Human {
                            id: "hord-eval-m4".into(),
                        },
                    )?;
                    fs::create_dir_all(&coverage_dir)?;
                    fs::write(&saved, hord_encoding::encode(&run.record)?)?;
                    let r = &run.record;
                    write_json(
                        &cov_path,
                        &CoverageResult {
                            checkpoint: j,
                            commit: checkpoint.commit.clone(),
                            elapsed_ms: run.elapsed_ms,
                            tests: r.tests.len(),
                            failed_tests: r.tests.iter().filter(|t| t.failed).count(),
                            instrumented: r.defs.len(),
                            log: run.log.clone(),
                        },
                    )?;
                    eprintln!(
                        "[coverage] {} tests, {} definitions instrumented, {:.1} min",
                        r.tests.len(),
                        r.defs.len(),
                        run.elapsed_ms as f64 / 60_000.0
                    );
                    run.record
                }
            }
        };
        if let Some(r) = read_json(&cov_path) {
            coverage_results.push(r);
        }
        let record = Arc::new(record);
        if let Some(n) = args.explain {
            if let Some(facts) = group.iter().find(|c| c.index == n) {
                explain(facts, &record);
            }
            continue;
        }
        let quarantine: BTreeSet<TestRef> = record
            .tests
            .iter()
            .filter(|t| quarantine_names.contains(&t.test.name))
            .map(|t| t.test.clone())
            .collect();
        let ctx = Ctx {
            toolchain: toolchain.clone(),
            seed: args.seed,
            quarantine,
            timeout,
            idle: Duration::from_secs(args.idle_timeout_mins * 60),
        };
        run_parallel(&workers, todo, &over_budget, |worker, facts| {
            let t = Instant::now();
            let result = match run::evaluate(&ctx, worker, facts, Arc::clone(&record)) {
                Ok(r) => r,
                Err(err) => CommitResult {
                    index: facts.index,
                    commit: facts.commit.clone(),
                    error: Some(format!("{err:#}")),
                    ..CommitResult::default()
                },
            };
            eprintln!(
                "[commit {}] {} ws={} selected {}/{} fault={} b={} a={:?} miss={} ({:.0}s)",
                facts.index,
                &facts.commit[..10],
                result.write_set,
                result.selection.selected,
                result.selection.suite,
                result
                    .fault
                    .as_ref()
                    .map_or("none".to_owned(), |f| format!("{:?}", f.kind)),
                result.detected_b,
                result.detected_a,
                result.miss,
                t.elapsed().as_secs_f64()
            );
            if let Some(e) = &result.error {
                eprintln!("[commit {}] error: {e}", facts.index);
            }
            write_json(&results_dir.join(format!("{}.json", facts.index)), &result)
        });
    }

    // Report.
    let results: Vec<CommitResult> = prepared
        .commits
        .iter()
        .filter_map(|c| read_json(&results_dir.join(format!("{}.json", c.index))))
        .collect();
    let report = report(
        &args,
        &prepared,
        results,
        sample,
        coverage_results,
        &quarantine_names,
    );
    print_report(&report);
    if let Some(path) = &args.json {
        write_json(path, &report)?;
    }
    if report.complete && !(report.safety_gate && report.efficiency_gate) {
        std::process::exit(1);
    }
    Ok(())
}

/// Run `job` over `items` on the workers, one item per worker at a time,
/// until the items or the budget run out. A failing job is logged.
fn run_parallel<T: Send>(
    workers: &[run::Worker],
    items: Vec<T>,
    over_budget: &(dyn Fn() -> bool + Sync),
    job: impl Fn(&run::Worker, T) -> Result<()> + Sync,
) {
    let queue = Mutex::new(items.into_iter());
    std::thread::scope(|scope| {
        for worker in workers {
            let (queue, job) = (&queue, &job);
            scope.spawn(move || {
                loop {
                    if over_budget() {
                        break;
                    }
                    let Some(item) = queue.lock().unwrap_or_else(|e| e.into_inner()).next() else {
                        break;
                    };
                    if let Err(err) = job(worker, item) {
                        eprintln!("[job] {err:#}");
                    }
                }
            });
        }
    });
}

fn explain(facts: &prepare::CommitFacts, record: &CoverageRecord) {
    let name = |n: &hord_core::NodeId| facts.names.get(n).cloned().unwrap_or_else(|| n.to_string());
    let tests = |n: &hord_core::NodeId| record.tests_covering(&[*n].into_iter().collect()).len();
    println!("commit {} {}", facts.index, facts.commit);
    for n in &facts.write_set {
        println!("  write  {} ({} tests)", name(n), tests(n));
    }
    for (n, hop) in &facts.impact.nodes {
        if *hop > 0 {
            println!("  hop {hop}  {} ({} tests)", name(n), tests(n));
        }
    }
    for t in &facts.impact.facts.touched {
        println!(
            "  touched {:?} {} {} attrs={} test={} dispatch={} instrumented={} ({} tests)",
            t.delta,
            t.kind,
            name(&t.node),
            t.attributes_changed,
            t.test,
            t.dispatch,
            record.is_instrumented(t.node),
            tests(&t.node)
        );
    }
    for p in &facts.impact.facts.paths {
        println!("  path {p}");
    }
}

fn report(
    args: &Args,
    prepared: &Prepared,
    results: Vec<CommitResult>,
    sample: Vec<SampleResult>,
    coverage: Vec<CoverageResult>,
    quarantine: &BTreeSet<String>,
) -> Report {
    let ok: Vec<&CommitResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let faulted: Vec<&&CommitResult> = ok.iter().filter(|r| r.fault.is_some()).collect();
    let detected = faulted
        .iter()
        .filter(|r| r.detected_b || r.detected_a == Some(true))
        .count();
    let b_covers_a = faulted.iter().filter(|r| r.b_covers_a).count();
    let probe_detected = faulted.iter().filter(|r| r.probe_detected).count();
    let misses: Vec<CommitResult> = ok.iter().filter(|r| r.miss).map(|r| (*r).clone()).collect();
    let ratio = |r: &CommitResult| r.selection.selected as f64 / r.selection.suite.max(1) as f64;
    let small: Vec<f64> = ok
        .iter()
        .filter(|r| r.write_set <= 5)
        .map(|r| ratio(r))
        .collect();
    let all: Vec<f64> = ok.iter().map(|r| ratio(r)).collect();
    // kind -> (commits, small commits, extra tests, share, share without)
    type Tally = (usize, usize, Vec<f64>, Vec<f64>, Vec<f64>);
    let mut costs: BTreeMap<String, Tally> = BTreeMap::new();
    for r in &ok {
        let suite = r.selection.suite.max(1) as f64;
        for (kind, without) in &r.selection.without {
            let e = costs.entry(kind.clone()).or_default();
            e.0 += 1;
            if r.write_set <= 5 {
                e.1 += 1;
            }
            e.2.push(r.selection.selected.saturating_sub(*without) as f64);
            e.3.push(r.selection.selected as f64 / suite);
            e.4.push(*without as f64 / suite);
        }
    }
    let fallback_costs = costs
        .into_iter()
        .map(
            |(k, (commits, small_commits, extra, share, share_without))| {
                (
                    k,
                    FallbackCost {
                        commits,
                        small_commits,
                        median_extra_tests: median(extra),
                        median_share: median(share),
                        median_share_without: median(share_without),
                    },
                )
            },
        )
        .collect();
    let without_any: Vec<f64> = ok
        .iter()
        .filter(|r| r.write_set <= 5)
        .map(|r| r.selection.without_any as f64 / r.selection.suite.max(1) as f64)
        .collect();
    let mut fallbacks: BTreeMap<String, usize> = BTreeMap::new();
    for r in &ok {
        for k in r.selection.without.keys().cloned() {
            *fallbacks.entry(k).or_default() += 1;
        }
        if r.selection.fallbacks.is_empty() {
            *fallbacks.entry("(none)".into()).or_default() += 1;
        }
    }
    let mean_commit_secs = if ok.is_empty() {
        0.0
    } else {
        ok.iter().map(|r| r.total_ms as f64 / 1000.0).sum::<f64>() / ok.len() as f64
    };
    let sample_secs = if sample.is_empty() {
        0.0
    } else {
        sample
            .iter()
            .map(|s| s.elapsed_ms as f64 / 1000.0)
            .sum::<f64>()
            / sample.len() as f64
    };
    let coverage_secs = if coverage.is_empty() {
        0.0
    } else {
        coverage
            .iter()
            .map(|c| c.elapsed_ms as f64 / 1000.0)
            .sum::<f64>()
            / coverage.len() as f64
    };
    let jobs = args.jobs.max(1) as f64;
    let checkpoints = 500usize.div_ceil(args.coverage_every.max(1)) as f64;
    let projected =
        (500.0 * mean_commit_secs / jobs + 20.0 * sample_secs / jobs + checkpoints * coverage_secs)
            / 3600.0;
    let efficiency_median = median(small.clone());
    let complete = results.len() == prepared.commits.len() && prepared.commits.len() >= 500;
    Report {
        corpus_commits: prepared.commits.len(),
        evaluated: results.len(),
        complete,
        prepare_secs: prepared.prepare_ms as f64 / 1000.0,
        quarantine: quarantine.iter().cloned().collect(),
        sample,
        coverage,
        faults_injected: faulted.len(),
        faults_detected: detected,
        probe_detected,
        b_covers_a,
        no_fault: ok.iter().filter(|r| r.fault.is_none()).count(),
        confirmed_misses: misses.iter().filter(|m| m.confirmed_miss).count(),
        safety_gate: misses.is_empty(),
        misses,
        efficiency_commits: small.len(),
        efficiency_gate: efficiency_median.is_some_and(|m| m <= 0.20),
        efficiency_median,
        efficiency_median_all: median(all),
        fallbacks,
        fallback_costs,
        efficiency_median_without_fallbacks: median(without_any),
        mean_commit_secs,
        projected_full_run_hours: projected,
        commits: results,
    }
}

fn print_report(r: &Report) {
    println!(
        "M4 selection eval ({} of {} commits evaluated{})",
        r.evaluated,
        r.corpus_commits,
        if r.complete { "" } else { ", incomplete" }
    );
    println!("  prepare: {:.1}s", r.prepare_secs);
    for s in &r.sample {
        println!(
            "  full-suite sample {}: {} failed, {:.1} min",
            &s.commit[..10],
            s.failed.len(),
            s.elapsed_ms as f64 / 60_000.0
        );
    }
    println!("  quarantined: {:?}", r.quarantine);
    for c in &r.coverage {
        println!(
            "  coverage checkpoint {} ({}): {} tests ({} failed), {} defs, {:.1} min",
            c.checkpoint,
            &c.commit[..10],
            c.tests,
            c.failed_tests,
            c.instrumented,
            c.elapsed_ms as f64 / 60_000.0
        );
    }
    println!(
        "  safety: {} faults injected, {} detected ({} by the probe), {} where (b) runs all of (a), {} misses ({} confirmed), {} commits without a fault",
        r.faults_injected,
        r.faults_detected,
        r.probe_detected,
        r.b_covers_a,
        r.misses.len(),
        r.confirmed_misses,
        r.no_fault
    );
    for m in &r.misses {
        println!(
            "    MISS {} {:?} failed_a={:?}",
            m.commit, m.fault, m.failed_a
        );
    }
    let pct = |m: Option<f64>| m.map_or("n/a".to_owned(), |m| format!("{:.1}%", m * 100.0));
    println!(
        "  efficiency: median selected {} of the suite over {} commits with write set <= 5 (all commits: {})",
        pct(r.efficiency_median),
        r.efficiency_commits,
        pct(r.efficiency_median_all)
    );
    println!("  fallbacks (commits): {:?}", r.fallbacks);
    println!(
        "  with every fallback off (coverage only): median {} for write set <= 5",
        pct(r.efficiency_median_without_fallbacks)
    );
    for (kind, c) in &r.fallback_costs {
        println!(
            "    {kind}: fired on {} commits ({} with write set <= 5), median +{} tests, median share {} -> {} without it",
            c.commits,
            c.small_commits,
            c.median_extra_tests
                .map_or("n/a".into(), |v| format!("{v:.0}")),
            pct(c.median_share),
            pct(c.median_share_without)
        );
    }
    println!(
        "  mean {:.0}s per commit; projected full run (500 commits): {:.1} h",
        r.mean_commit_secs, r.projected_full_run_hours
    );
    println!(
        "  gates: safety {} efficiency {}{}",
        if r.safety_gate { "PASS" } else { "FAIL" },
        if r.efficiency_gate { "PASS" } else { "FAIL" },
        if r.complete {
            ""
        } else {
            " (not gated: incomplete run)"
        }
    );
}
