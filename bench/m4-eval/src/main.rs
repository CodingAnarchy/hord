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
//! 4. **Commits.** Per commit, on `--jobs` workers: the selection under
//!    four variants from the same checkpoint (A current rules, B
//!    region-level coverage for edited functions, C narrowed non-Rust
//!    fallback, D both; see [`run`]), then one seeded fault in a function
//!    the commit wrote that type-checks, graded under every variant against
//!    (a) every test of the affected packages. A miss is a fault (a)
//!    detects and a variant's (b) does not.
//!
//! Gates (variant A, the rules in force): zero misses; median selected
//! ≤ 20% of the suite over commits with write set ≤ 5. Every phase writes its results under
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

mod disk;
mod fault;
mod fresh;
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
use hord_core::{Actor, RepoPath};
use hord_store::Store;
use hord_verify::{Checkout as VerifyCheckout, CoverageRecord, TestRef, find_coverage};
use hord_verify_rust::coverage::record_evidence;
use hord_verify_rust::{CoverageOptions, detect_toolchain};
use serde::{Deserialize, Serialize};

use crate::prepare::Prepared;
use crate::run::{CommitResult, Ctx, VARIANTS};

/// `--sizes-ignoring` output: index, commit, write set, and per variant
/// the size and fallback kinds.
type SizeRow = (usize, String, usize, BTreeMap<String, (usize, Vec<String>)>);

/// One test's executed lines of interest, as stored on disk.
type LinesRow = (TestRef, Vec<(RepoPath, Vec<u32>)>);

/// `*.profraw` files in the working directory modified since `since`: an
/// instrumented binary run without `LLVM_PROFILE_FILE`. Should be empty.
fn profraw_leaks(since: std::time::SystemTime) -> Vec<String> {
    let Ok(entries) = fs::read_dir(".") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".profraw"))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m >= since)
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

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
    /// The first-parent history to take commits from.
    #[arg(long, default_value = "HEAD")]
    head: String,
    /// Grade chain `i` of `n` (ADR 0023 amendment): the era window of
    /// `--commits-per-chain` first-parent commits ending `i ×
    /// --chain-stride` commits before `--head`. Chains are independent: each
    /// starts from its own full instrumented run. Without it, the last
    /// `--commits` commits.
    #[arg(long)]
    chain: Option<String>,
    /// Commits per chain.
    #[arg(long, default_value_t = 10)]
    commits_per_chain: usize,
    /// First-parent commits between the ends of consecutive chains.
    #[arg(long, default_value_t = 100)]
    chain_stride: usize,
    /// Faults graded per commit where (b) does not contain (a), each on its
    /// own (ADR 0023 amendment); commits where it does get one probe-graded
    /// fault.
    #[arg(long, default_value_t = 1)]
    faults_per_commit: usize,
    /// Merge the results of these chain work dirs (and sample dirs) into one
    /// report and verdict, and exit: no corpus, no runs. Exits nonzero when
    /// the merged run is complete and a gate fails.
    #[arg(long, num_args = 1..)]
    merge: Vec<PathBuf>,
    /// Run only the literal full-suite sample (ADR 0023, reported, not
    /// gated) over the commits of every chain window of `--chain _/n`, then
    /// exit. `--sample-shard k/m` runs the k-th of m slices.
    #[arg(long)]
    sample_only: bool,
    #[arg(long)]
    sample_shard: Option<String>,
    /// Test names quarantined up front (one per line; `#` comments), in
    /// addition to failures of the full-suite sample.
    #[arg(long)]
    quarantine: Option<PathBuf>,
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
    /// Run the four-variant measurement (shared coverage checkpoints every
    /// `--coverage-every` commits) instead of the lander-view grading of
    /// ADR 0022's fresh per-test coverage (the default).
    #[arg(long)]
    variants: bool,
    /// Diagnostic mode: instead of running tests, print and write (to
    /// `--json`) each commit's variant sizes with these fallback kinds
    /// turned off (comma-separated `Fallback::kind` names). Ungraded.
    #[arg(long)]
    sizes_ignoring: Option<String>,
    /// Write a Markdown summary table here.
    #[arg(long)]
    summary: Option<PathBuf>,
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
    pub(crate) commit: String,
    pub(crate) failed: Vec<String>,
    pub(crate) elapsed_ms: u64,
    pub(crate) timed_out: bool,
    /// A few failing tests' output, to diagnose environment failures.
    #[serde(default)]
    pub(crate) failure_output: Vec<(String, String)>,
}

impl SampleResult {
    /// Record one sample run, and log its failure excerpts.
    fn from_run(commit: &str, run: run::FullSuite) -> Self {
        eprintln!(
            "[sample] {}: {} failed{}",
            &commit[..10.min(commit.len())],
            run.failed.len(),
            if run.timed_out { " (timed out)" } else { "" }
        );
        for (test, output) in &run.excerpts {
            eprintln!("[sample] ---- {test} ----\n{output}");
        }
        Self {
            commit: commit.to_owned(),
            failed: run.failed,
            elapsed_ms: run.elapsed_ms,
            timed_out: run.timed_out,
            failure_output: run.excerpts,
        }
    }
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
struct VariantSummary {
    /// Median share of the suite selected over commits with write set <= 5.
    median_share_small: Option<f64>,
    /// Median over every commit.
    median_share_all: Option<f64>,
    /// Median selected test count over commits with write set <= 5.
    median_selected_small: Option<f64>,
    /// Faults this variant's (b) detected.
    detected: usize,
    /// Faults where this variant runs all of (a) (no miss possible).
    covers_a: usize,
    /// Misses: (a) detected, this variant did not.
    misses: Vec<String>,
    confirmed_misses: usize,
    /// Commits each fallback kind fired on.
    fallbacks: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct Report {
    corpus_commits: usize,
    evaluated: usize,
    errors: usize,
    complete: bool,
    prepare_secs: f64,
    quarantine: Vec<String>,
    sample: Vec<SampleResult>,
    coverage: Vec<CoverageResult>,
    faults_injected: usize,
    /// Faults (a) detected (where determined).
    faults_detected_a: usize,
    /// Faults whose (a) verdict was not needed (every variant detected).
    faults_a_undetermined: usize,
    no_fault: usize,
    small_commits: usize,
    variants: BTreeMap<String, VariantSummary>,
    mean_commit_secs: f64,
    /// `*.profraw` written into the working directory during the run.
    profraw_in_cwd: Vec<String>,
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

/// `i/n`, checked.
fn parse_part(text: &str, flag: &str) -> Result<(usize, usize)> {
    let (i, n) = text
        .split_once('/')
        .with_context(|| format!("{flag} i/n"))?;
    let (i, n): (usize, usize) = (i.parse()?, n.parse()?);
    if n == 0 || i >= n {
        bail!("{flag} {text}: need 0 <= i < n");
    }
    Ok((i, n))
}

/// Test names in a quarantine file (one per line, `#` comments).
fn read_quarantine(path: Option<&Path>) -> Result<BTreeSet<String>> {
    let Some(path) = path else {
        return Ok(BTreeSet::new());
    };
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();
    let started_at = std::time::SystemTime::now();
    let seed_quarantine = read_quarantine(args.quarantine.as_deref())?;
    if !args.merge.is_empty() {
        // Merge: one report and verdict over the chains' work dirs.
        let report = fresh::report(&args.merge, &seed_quarantine);
        let md = fresh::print(&report);
        if let Some(path) = &args.json {
            write_json(path, &report)?;
        }
        if let Some(path) = &args.summary {
            fs::write(path, md)?;
        }
        if report.complete && !(report.safety_gate && report.efficiency_gate) {
            std::process::exit(1);
        }
        return Ok(());
    }
    run::check_corpus_env(std::env::vars())?;
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

    // The commits: one chain's era window, or the last `--commits`.
    let chain = args
        .chain
        .as_deref()
        .map(|c| parse_part(c, "--chain"))
        .transpose()?;
    let window = |i: usize, n: usize| {
        git::first_parent_window(&corpus, &args.head, i * args.chain_stride, n)
    };
    if args.sample_only {
        return sample_only(&args, &corpus, &work, chain, &window, timeout, &over_budget);
    }
    // 1. Prepare.
    let commits = match chain {
        Some((i, _)) => window(i, args.commits_per_chain)?,
        None => window(0, args.commits)?,
    };
    // CBOR: the facts have maps keyed by NodeId and RepoPath.
    let prepared_path = work.join(if args.variants {
        "prepared.cbor"
    } else {
        "prepared-fresh.cbor"
    });
    // Fresh mode has one checkpoint: the initial full run.
    let every = if args.variants {
        args.coverage_every
    } else {
        usize::MAX
    };
    let cached: Option<Prepared> = fs::read(&prepared_path)
        .ok()
        .and_then(|b| hord_encoding::decode(&b).ok());
    let prepared: Prepared =
        match cached.filter(|p| p.commits.iter().map(|c| &c.commit).eq(commits.iter())) {
            Some(p) => {
                eprintln!("[prepare] reusing {}", prepared_path.display());
                p
            }
            None => {
                let p = prepare::prepare(&corpus, &work.join("hord"), &commits, every).await?;
                fs::write(&prepared_path, hord_encoding::encode(&p)?)?;
                p
            }
        };

    // Fresh mode: workers 0 and 1 are the lander chain (alternating builds),
    // the others grade.
    let worker_count = if args.variants {
        args.jobs.max(1)
    } else {
        args.jobs.max(1) + 2
    };
    let workers: Vec<run::Worker> = (0..worker_count)
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
        let sample = SampleResult::from_run(&commit, run::full_suite(worker, &commit, timeout)?);
        write_json(&sample_dir.join(format!("{commit}.json")), &sample)
    });
    let sample: Vec<SampleResult> = sample_commits
        .iter()
        .filter_map(|c| read_json(&sample_dir.join(format!("{c}.json"))))
        .collect();
    let mut quarantine_names: BTreeSet<String> = sample
        .iter()
        .flat_map(|s| s.failed.iter().cloned())
        .collect();
    quarantine_names.extend(seed_quarantine.iter().cloned());

    if !args.variants {
        return run_fresh(
            &args,
            &work,
            &prepared,
            &workers,
            &toolchain,
            &quarantine_names,
            timeout,
            started_at,
            &in_shard,
            &over_budget,
        );
    }

    // 3 and 4, checkpoint by checkpoint.
    let results_dir = work.join("results");
    let coverage_dir = work.join("coverage");
    let mut coverage_results = Vec::new();
    let mut diagnostic: Vec<SizeRow> = Vec::new();
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
                in_shard(c.index)
                    && (args.sizes_ignoring.is_some()
                        || !results_dir.join(format!("{}.json", c.index)).exists())
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
            let saved_lines = coverage_dir.join(format!("{j}.lines.cbor"));
            let tc = toolchain.id()?;
            let mut found = find_coverage(&store, [checkpoint.snapshot], tc)?.map(|f| f.1);
            // Variant B needs this checkpoint's line data too.
            if !saved_lines.exists() {
                found = None;
            }
            if found.is_none() && saved_lines.exists() {
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
                    let run = run::collect_corpus(
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
                            only: None,
                            lines_of_interest: checkpoint.interest.clone(),
                            cancel: Default::default(),
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
                    let flat: Vec<LinesRow> = run
                        .lines
                        .iter()
                        .map(|(t, l)| {
                            (
                                t.clone(),
                                l.iter()
                                    .map(|(p, n)| (p.clone(), n.iter().copied().collect()))
                                    .collect(),
                            )
                        })
                        .collect();
                    fs::write(&saved_lines, hord_encoding::encode(&flat)?)?;
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
        let lines: BTreeMap<TestRef, BTreeMap<RepoPath, BTreeSet<u32>>> =
            fs::read(coverage_dir.join(format!("{j}.lines.cbor")))
                .ok()
                .and_then(|b| hord_encoding::decode::<Vec<LinesRow>>(&b).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(t, l)| {
                    (
                        t,
                        l.into_iter()
                            .map(|(p, n)| (p, n.into_iter().collect()))
                            .collect(),
                    )
                })
                .collect();
        if let Some(kinds) = &args.sizes_ignoring {
            let ignore: BTreeSet<&str> = kinds.split(',').map(str::trim).collect();
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
                faults_per_commit: args.faults_per_commit,
            };
            for facts in &group {
                let input = run::CommitInput {
                    facts,
                    record: &record,
                    lines: &lines,
                };
                let sizes = run::sizes_ignoring(&ctx, &workers[0], &input, &ignore)?;
                println!(
                    "{}\t{}\t{}\t{}",
                    facts.index,
                    &facts.commit[..10],
                    facts.write_set.len(),
                    VARIANTS
                        .iter()
                        .map(|v| format!("{v}={} {:?}", sizes[*v].0, sizes[*v].1))
                        .collect::<Vec<_>>()
                        .join("\t")
                );
                diagnostic.push((
                    facts.index,
                    facts.commit.clone(),
                    facts.write_set.len(),
                    sizes,
                ));
            }
            continue;
        }
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
            faults_per_commit: args.faults_per_commit,
        };
        run_parallel(&workers, todo, &over_budget, |worker, facts| {
            let t = Instant::now();
            let input = run::CommitInput {
                facts,
                record: &record,
                lines: &lines,
            };
            let result = match run::evaluate(&ctx, worker, &input) {
                Ok(r) => r,
                Err(err) => CommitResult {
                    index: facts.index,
                    commit: facts.commit.clone(),
                    error: Some(format!("{err:#}")),
                    ..CommitResult::default()
                },
            };
            let cell = |v: &str| {
                result.variants.get(v).map_or("-".to_owned(), |r| {
                    let d = match r.detected {
                        Some(true) => "+",
                        Some(false) if r.miss => "MISS",
                        Some(false) => "-",
                        None => "",
                    };
                    format!("{v}={}{d}", r.selected)
                })
            };
            eprintln!(
                "[commit {}] {} ws={} {} {} {} {} fault={} a={:?} ({:.0}s)",
                facts.index,
                &facts.commit[..10],
                result.write_set,
                cell("A"),
                cell("B"),
                cell("C"),
                cell("D"),
                result
                    .fault
                    .as_ref()
                    .map_or("none".to_owned(), |f| format!("{:?}", f.kind)),
                result.detected_a,
                t.elapsed().as_secs_f64()
            );
            if let Some(e) = &result.error {
                eprintln!("[commit {}] error: {e}", facts.index);
            }
            write_json(&results_dir.join(format!("{}.json", facts.index)), &result)
        });
    }

    if args.sizes_ignoring.is_some() {
        if let Some(path) = &args.json {
            write_json(path, &diagnostic)?;
        }
        return Ok(());
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
    let mut report = report;
    report.profraw_in_cwd = profraw_leaks(started_at);
    print_report(&report);
    if let Some(path) = &args.json {
        write_json(path, &report)?;
    }
    if let Some(path) = &args.summary {
        fs::write(path, summary(&report))?;
    }
    if report.complete && !(report.safety_gate && report.efficiency_gate) {
        std::process::exit(1);
    }
    Ok(())
}

/// `--sample-only`: the literal full `cargo test` on a seeded sample of the
/// commits of every chain window (ADR 0023: reported, not gated; its failures
/// become the quarantine). Results go to `<work>/sample/`.
fn sample_only(
    args: &Args,
    corpus: &Path,
    work: &Path,
    chain: Option<(usize, usize)>,
    window: &dyn Fn(usize, usize) -> Result<Vec<String>>,
    timeout: Duration,
    over_budget: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    let chains = chain.map_or(1, |(_, n)| n);
    let per = if chain.is_some() {
        args.commits_per_chain
    } else {
        args.commits
    };
    let mut candidates = Vec::new();
    for i in 0..chains {
        candidates.extend(window(i, per)?);
    }
    let mut rng = fault::Rng::new(args.seed, "full-sample");
    rng.shuffle(&mut candidates);
    candidates.truncate(args.full_sample);
    let (k, m) = match &args.sample_shard {
        Some(s) => parse_part(s, "--sample-shard")?,
        None => (0, 1),
    };
    let sample_dir = work.join("sample");
    let mine: Vec<String> = candidates
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % m == k)
        .map(|(_, c)| c)
        .filter(|c| !sample_dir.join(format!("{c}.json")).exists())
        .collect();
    let workers: Vec<run::Worker> = (0..args.jobs.max(1))
        .map(|i| run::worker(work, corpus, i))
        .collect::<Result<_>>()?;
    let monitor = disk::Monitor::new(work, None);
    monitor.sample("start");
    run_parallel(&workers, mine, over_budget, |worker, commit| {
        eprintln!("[sample] full cargo test on {}", &commit[..10]);
        let since = std::time::SystemTime::now();
        let sample = SampleResult::from_run(&commit, run::full_suite(worker, &commit, timeout)?);
        let written = write_json(&sample_dir.join(format!("{commit}.json")), &sample);
        let label = format!("sample {}", &commit[..10]);
        disk::prune_and_log(&label, &worker.target_dir, since);
        disk::clear_dir(&worker.profraw_dir());
        monitor.sample(&label);
        written
    });
    Ok(())
}

/// The lander-view grading of ADR 0022 as amended ([`fresh`]).
#[allow(clippy::too_many_arguments)]
fn run_fresh(
    args: &Args,
    work: &Path,
    prepared: &Prepared,
    workers: &[run::Worker],
    toolchain: &hord_verify::Toolchain,
    quarantine_names: &BTreeSet<String>,
    timeout: Duration,
    started_at: std::time::SystemTime,
    in_shard: &(dyn Fn(usize) -> bool + Sync),
    over_budget: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    let checkpoint = prepared.checkpoints.first().context("no checkpoint")?;
    let monitor = disk::Monitor::new(work, Some(work.join("chain/disk.json")));
    monitor.sample("start");
    let initial_path = fresh::initial_path(work);
    let initial_meta = work.join("chain/initial.json");
    let initial: CoverageRecord = match fs::read(&initial_path)
        .ok()
        .and_then(|b| hord_encoding::decode::<CoverageRecord>(&b).ok())
        .filter(|r| r.snapshot == checkpoint.snapshot)
    {
        Some(r) => r,
        None => {
            eprintln!(
                "[coverage] initial full run at {}",
                &checkpoint.commit[..10]
            );
            let w = &workers[0];
            w.checkout.checkout(&checkpoint.commit)?;
            let target_dir = fresh::coverage_target(work, 0);
            let since = std::time::SystemTime::now();
            let run = run::collect_corpus(
                &VerifyCheckout {
                    root: w.checkout.root.clone(),
                    snapshot: checkpoint.snapshot,
                },
                toolchain,
                &checkpoint.defs,
                &CoverageOptions {
                    packages: None,
                    target_dir: target_dir.clone(),
                    jobs: args.coverage_jobs,
                    test_timeout: Duration::from_secs(600),
                    skip: BTreeSet::new(),
                    only: None,
                    lines_of_interest: BTreeMap::new(),
                    cancel: Default::default(),
                },
            )?;
            disk::prune_and_log("initial", &target_dir, since);
            let names: BTreeSet<&str> = run
                .record
                .tests
                .iter()
                .map(|t| t.test.name.as_str())
                .collect();
            write_json(&fresh::suite_path(work), &names)?;
            monitor.sample("initial");
            fs::create_dir_all(work.join("chain"))?;
            fs::write(&initial_path, hord_encoding::encode(&run.record)?)?;
            write_json(
                &initial_meta,
                &(run.elapsed_ms, run.record.tests.len(), run.log.clone()),
            )?;
            eprintln!(
                "[coverage] {} tests, {} definitions, {:.1} min",
                run.record.tests.len(),
                run.record.defs.len(),
                run.elapsed_ms as f64 / 60_000.0
            );
            run.record
        }
    };
    write_json(
        &work.join("chain/meta.json"),
        &fresh::ChainMeta {
            chain: args.chain.clone().unwrap_or_default(),
            head: args.head.clone(),
            stride: args.chain_stride,
            commits: prepared.commits.iter().map(|c| c.commit.clone()).collect(),
            faults_per_commit: args.faults_per_commit,
        },
    )?;
    let quarantine: BTreeSet<TestRef> = initial
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
        faults_per_commit: args.faults_per_commit,
    };
    anyhow::ensure!(
        workers.len() >= 3,
        "fresh mode needs two chain workers and a grader"
    );
    let (chain_workers, graders) = workers.split_at(2);
    fresh::run(
        &fresh::FreshRun {
            ctx: &ctx,
            prepared,
            work,
            coverage_jobs: args.coverage_jobs,
            initial,
            in_shard,
            over_budget,
            disk: &monitor,
        },
        chain_workers,
        graders,
    )?;
    monitor.sample("end");
    let mut report = fresh::report(&[work.to_path_buf()], quarantine_names);
    report.profraw_in_cwd = profraw_leaks(started_at);
    let md = fresh::print(&report);
    if let Some(path) = &args.json {
        write_json(path, &report)?;
    }
    if let Some(path) = &args.summary {
        fs::write(path, md)?;
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
    _args: &Args,
    prepared: &Prepared,
    results: Vec<CommitResult>,
    sample: Vec<SampleResult>,
    coverage: Vec<CoverageResult>,
    quarantine: &BTreeSet<String>,
) -> Report {
    let ok: Vec<&CommitResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let faulted: Vec<&&CommitResult> = ok.iter().filter(|r| r.fault.is_some()).collect();
    let mut variants = BTreeMap::new();
    for v in VARIANTS {
        let share = |r: &CommitResult| {
            r.variants
                .get(v)
                .map(|x| x.selected as f64 / x.suite.max(1) as f64)
        };
        let small: Vec<&&CommitResult> = ok.iter().filter(|r| r.write_set <= 5).collect();
        let mut fallbacks: BTreeMap<String, usize> = BTreeMap::new();
        for r in &ok {
            if let Some(x) = r.variants.get(v) {
                if x.fallbacks.is_empty() {
                    *fallbacks.entry("(none)".into()).or_default() += 1;
                }
                for k in &x.fallbacks {
                    *fallbacks.entry(k.clone()).or_default() += 1;
                }
            }
        }
        let get = |r: &CommitResult| r.variants.get(v).cloned().unwrap_or_default();
        variants.insert(
            v.to_owned(),
            VariantSummary {
                median_share_small: median(small.iter().filter_map(|r| share(r)).collect()),
                median_share_all: median(ok.iter().filter_map(|r| share(r)).collect()),
                median_selected_small: median(
                    small
                        .iter()
                        .filter_map(|r| r.variants.get(v))
                        .map(|x| x.selected as f64)
                        .collect(),
                ),
                detected: faulted
                    .iter()
                    .filter(|r| get(r).detected == Some(true))
                    .count(),
                covers_a: faulted.iter().filter(|r| get(r).covers_a).count(),
                misses: faulted
                    .iter()
                    .filter(|r| get(r).miss)
                    .map(|r| format!("{} {}", r.index, &r.commit[..10]))
                    .collect(),
                confirmed_misses: faulted.iter().filter(|r| get(r).confirmed_miss).count(),
                fallbacks,
            },
        );
    }
    let mean_commit_secs = if ok.is_empty() {
        0.0
    } else {
        ok.iter().map(|r| r.total_ms as f64 / 1000.0).sum::<f64>() / ok.len() as f64
    };
    let complete = results.len() == prepared.commits.len() && prepared.commits.len() >= 500;
    let a = &variants["A"];
    Report {
        corpus_commits: prepared.commits.len(),
        evaluated: results.len(),
        errors: results.len() - ok.len(),
        complete,
        prepare_secs: prepared.prepare_ms as f64 / 1000.0,
        quarantine: quarantine.iter().cloned().collect(),
        sample,
        coverage,
        faults_injected: faulted.len(),
        faults_detected_a: faulted
            .iter()
            .filter(|r| r.detected_a == Some(true))
            .count(),
        faults_a_undetermined: faulted.iter().filter(|r| r.detected_a.is_none()).count(),
        no_fault: ok.iter().filter(|r| r.fault.is_none()).count(),
        small_commits: ok.iter().filter(|r| r.write_set <= 5).count(),
        safety_gate: a.misses.is_empty(),
        efficiency_gate: a.median_share_small.is_some_and(|m| m <= 0.20),
        variants,
        mean_commit_secs,
        profraw_in_cwd: Vec::new(),
        commits: results,
    }
}

fn pct(m: Option<f64>) -> String {
    m.map_or("n/a".to_owned(), |m| format!("{:.1}%", m * 100.0))
}

fn print_report(r: &Report) {
    println!(
        "M4 selection eval: {} of {} commits evaluated ({} errors){}",
        r.evaluated,
        r.corpus_commits,
        r.errors,
        if r.complete { "" } else { ", incomplete" }
    );
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
    println!("  quarantined: {:?}", r.quarantine);
    println!(
        "  faults: {} injected, (a) detected {} ({} not needed), {} commits without a fault",
        r.faults_injected, r.faults_detected_a, r.faults_a_undetermined, r.no_fault
    );
    for (v, s) in &r.variants {
        println!(
            "  {v}: median {} (write set <= 5, n={}), all {}; detected {}, covers (a) {}, misses {} ({} confirmed) {:?}",
            pct(s.median_share_small),
            r.small_commits,
            pct(s.median_share_all),
            s.detected,
            s.covers_a,
            s.misses.len(),
            s.confirmed_misses,
            s.misses
        );
        println!("     fallbacks: {:?}", s.fallbacks);
    }
    println!("  mean {:.0}s per commit", r.mean_commit_secs);
    println!(
        "  profraw written to the working directory: {:?}",
        r.profraw_in_cwd
    );
    println!(
        "  gates (A): safety {} efficiency {}{}",
        if r.safety_gate { "PASS" } else { "FAIL" },
        if r.efficiency_gate { "PASS" } else { "FAIL" },
        if r.complete {
            ""
        } else {
            " (not gated: incomplete run)"
        }
    );
}

/// The Markdown summary: one row per variant, then one per commit.
fn summary(r: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# M4 selection variants ({} commits, {} with write set <= 5, {} faults)

",
        r.evaluated, r.small_commits, r.faults_injected
    ));
    out.push_str("| Variant | Median share (ws <= 5) | Median tests (ws <= 5) | Median share (all) | Detected | Covers (a) | Misses (confirmed) |\n|---|---|---|---|---|---|---|\n");
    let names = [
        ("A", "current rules"),
        ("B", "region-level coverage"),
        ("C", "narrowed non-Rust fallback"),
        ("D", "B + C"),
    ];
    for (v, label) in names {
        let s = &r.variants[v];
        out.push_str(&format!(
            "| {v} {label} | {} | {} | {} | {} | {} | {} ({}) |\n",
            pct(s.median_share_small),
            s.median_selected_small
                .map_or("n/a".into(), |m| format!("{m:.0}")),
            pct(s.median_share_all),
            s.detected,
            s.covers_a,
            s.misses.len(),
            s.confirmed_misses
        ));
    }
    out.push_str("\n## Fallback kinds (commits fired)\n\n| Kind |");
    for (v, _) in names {
        out.push_str(&format!(" {v} |"));
    }
    out.push_str("\n|---|---|---|---|---|\n");
    let kinds: BTreeSet<&String> = r
        .variants
        .values()
        .flat_map(|s| s.fallbacks.keys())
        .collect();
    for k in kinds {
        out.push_str(&format!("| {k} |"));
        for (v, _) in names {
            out.push_str(&format!(
                " {} |",
                r.variants[v].fallbacks.get(k).copied().unwrap_or(0)
            ));
        }
        out.push('\n');
    }
    out.push_str("\n## Commits\n\n| # | Commit | ws | A | B | C | D | Fault | (a) |\n|---|---|---|---|---|---|---|---|---|\n");
    for c in &r.commits {
        let cell = |v: &str| {
            c.variants.get(v).map_or("-".into(), |x| {
                let d = match (x.detected, x.miss) {
                    (_, true) => " MISS",
                    (Some(true), _) => " ✓",
                    _ => "",
                };
                format!("{}{d}", x.selected)
            })
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            c.index,
            &c.commit[..10],
            c.write_set,
            cell("A"),
            cell("B"),
            cell("C"),
            cell("D"),
            c.fault.as_ref().map_or_else(
                || c.error.clone().map_or("none".into(), |_| "error".into()),
                |f| format!("{:?} `{}`", f.kind, f.name)
            ),
            c.detected_a.map_or("-".into(), |d| d.to_string())
        ));
    }
    out.push_str(&format!(
        "\nprofraw written to the working directory: {:?}\n",
        r.profraw_in_cwd
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_parts_and_quarantine_files() -> Result<()> {
        assert_eq!(parse_part("2/15", "--chain")?, (2, 15));
        assert!(parse_part("15/15", "--chain").is_err());
        assert!(parse_part("1/0", "--chain").is_err());
        assert!(parse_part("x", "--chain").is_err());
        let dir = std::env::temp_dir().join(format!("hord-m4-quarantine-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let file = dir.join("q.txt");
        fs::write(&file, "# known flaky\na::b\n\n  c::d  # env\n")?;
        let q = read_quarantine(Some(&file))?;
        fs::remove_dir_all(&dir)?;
        assert_eq!(
            q,
            ["a::b".to_owned(), "c::d".to_owned()].into_iter().collect()
        );
        assert!(read_quarantine(None)?.is_empty());
        Ok(())
    }
}
