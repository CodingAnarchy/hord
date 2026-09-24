//! Grading ADR 0022 as amended after the 50-commit measurement, the way
//! the lander sees it (`--fresh`, the default).
//!
//! One initial instrumented run of the whole workspace gives every test a
//! coverage record. Then a chain walks the commits in landing order, as the
//! lander would:
//!
//! 1. select with the per-test records ([`hord_verify_rust::select`] with a
//!    [`Drift`] of every earlier landing, and the commit's own facts and
//!    attributable impact set);
//! 2. run the selection instrumented on the unmutated commit
//!    ([`hord_verify_rust::coverage::collect`] with a [`TestFilter`]); its
//!    wall time is the lander's verification cost for the commit;
//! 3. merge the fresh coverage into the records for the next commit.
//!
//! Graders run beside the chain, one commit each: inject one seeded fault
//! that type-checks, run the chain's selection (b), and (a) every test of
//! the affected packages when (b) did not detect it. A miss is a fault (a)
//! detects and (b) does not, confirmed by re-running the missed failures
//! without the fault. The fault never reaches the records: the chain ran
//! the commit unmutated.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hord_core::{NodeId, RepoPath};
use hord_verify::{Checkout as VerifyCheckout, CoverageRecord, DefDelta, Drift};
use hord_verify_rust::coverage::{BuiltSuite, build_suite, run_suite};
use hord_verify_rust::{
    CargoRunner, CargoWorkspace, CoverageOptions, DefinitionIndex, Fallback, SelectInput,
    Selection, TestFilter, select,
};
use serde::{Deserialize, Serialize};

use crate::fault::{attempts, inject};
use crate::prepare::{CommitFacts, Prepared};
use crate::run::{Ctx, Fault, Grader, Unit, Worker, cargo, count, elapsed, units};

/// The chain's state after a commit, persisted so a run resumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChainState {
    /// Index of the next commit to process.
    next: usize,
    ledger: CoverageRecord,
    /// The same records with one run each (no union), only to report the
    /// union rule's efficiency cost.
    single: Option<CoverageRecord>,
    drift: Drift,
    defs: DefinitionIndex,
}

/// What the lander did for one commit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ChainStep {
    pub index: usize,
    pub commit: String,
    pub write_set: usize,
    /// Tests selected (of the record) and the suite size.
    pub selected: usize,
    /// Tests the same rules would select with one run per record instead of
    /// the union of the last three (the union's efficiency cost).
    #[serde(default)]
    pub selected_single: usize,
    /// The instrumented build (overlapped with the previous commit's run)
    /// and the instrumented run of the selection, in milliseconds.
    #[serde(default)]
    pub build_ms: u64,
    #[serde(default)]
    pub run_ms: u64,
    pub suite: usize,
    pub full: bool,
    /// Fallback kinds that fired.
    pub fallbacks: Vec<String>,
    /// Tests selected because their executed code changed since their own
    /// coverage snapshot (drift), beyond those the change's writes select.
    pub drift_selected: usize,
    /// The selection as units, and (a) as units.
    pub units: Vec<Unit>,
    pub a_units: Vec<Unit>,
    pub affected: Vec<String>,
    /// Per fault target, the units that ran it (the probe).
    pub probes: Vec<(NodeId, Vec<Unit>)>,
    /// Tests the instrumented run ran, and those that failed unmutated.
    pub tests_run: usize,
    pub unmutated_failed: Vec<String>,
    /// The instrumented run's log: why each failing test failed (a test
    /// failure, a timeout, or a profile tool error).
    #[serde(default)]
    pub coverage_log: String,
    /// The lander's wall time for the commit: selection, instrumented
    /// build, instrumented run, merge.
    pub chain_ms: u64,
    pub error: Option<String>,
}

/// One graded commit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct FreshResult {
    pub step: ChainStep,
    pub fault: Option<Fault>,
    pub no_fault: Option<String>,
    /// Whether (b), the selection, detected the fault.
    pub detected: Option<bool>,
    /// Whether (a) did, when determined.
    pub detected_a: Option<bool>,
    /// The selection contains every test of (a): no miss is possible.
    pub covers_a: bool,
    /// Option 2 (approved): the selection contained all of (a), so only the
    /// probe ran; `detected` is `None` unless the probe caught the fault.
    #[serde(default)]
    pub probe_only: bool,
    pub miss: bool,
    pub confirmed_miss: bool,
    pub failed: Vec<String>,
    pub unattributed: bool,
    pub build_ms: u64,
    pub grade_ms: u64,
    pub error: Option<String>,
}

/// Inputs of the fresh run.
pub(crate) struct FreshRun<'a> {
    pub ctx: &'a Ctx,
    pub prepared: &'a Prepared,
    pub work: &'a Path,
    pub coverage_jobs: usize,
    /// The initial full run's record (on checkpoint 0).
    pub initial: CoverageRecord,
    pub in_shard: &'a (dyn Fn(usize) -> bool + Sync),
    pub over_budget: &'a (dyn Fn() -> bool + Sync),
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

/// Run the chain on `chain_worker` and grade on `graders` as steps arrive.
pub(crate) fn run(run: &FreshRun<'_>, chain_workers: &[Worker], graders: &[Worker]) -> Result<()> {
    let chain_dir = run.work.join("chain");
    let results_dir = run.work.join("fresh-results");
    fs::create_dir_all(&chain_dir)?;
    let (tx, rx) = mpsc::channel::<ChainStep>();
    let rx = std::sync::Mutex::new(rx);
    std::thread::scope(|scope| -> Result<()> {
        let chain_dir = &chain_dir;
        // The chain owns the sender: when it finishes, the graders' queue
        // closes after the last step.
        let chain = scope.spawn(move || chain(run, chain_workers, chain_dir, &tx));
        for worker in graders {
            let (rx, results_dir) = (&rx, &results_dir);
            scope.spawn(move || {
                loop {
                    let step = {
                        let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
                        rx.recv()
                    };
                    let Ok(step) = step else {
                        break;
                    };
                    let out = results_dir.join(format!("{}.json", step.index));
                    if out.exists() || !(run.in_shard)(step.index) || (run.over_budget)() {
                        continue;
                    }
                    let facts = &run.prepared.commits[step.index];
                    let t = Instant::now();
                    let result = match grade(run.ctx, worker, facts, &step) {
                        Ok(r) => r,
                        Err(err) => FreshResult {
                            step: step.clone(),
                            error: Some(format!("{err:#}")),
                            ..FreshResult::default()
                        },
                    };
                    eprintln!(
                        "[grade {}] {} ws={} selected {}/{} fault={} b={:?} a={:?}{} ({:.0}s)",
                        step.index,
                        &step.commit[..10],
                        step.write_set,
                        step.selected,
                        step.suite,
                        result
                            .fault
                            .as_ref()
                            .map_or("none".to_owned(), |f| format!("{:?}", f.kind)),
                        result.detected,
                        result.detected_a,
                        if result.miss { " MISS" } else { "" },
                        t.elapsed().as_secs_f64()
                    );
                    if let Err(err) = write_json(&out, &result) {
                        eprintln!("[grade {}] {err:#}", step.index);
                    }
                }
            });
        }
        chain
            .join()
            .map_err(|_| anyhow::anyhow!("chain panicked"))??;
        Ok(())
    })
}

/// The lander's chain (see the module docs). Emits every step, including
/// steps a resumed run already had.
///
/// Two checkouts and instrumented target directories alternate: commit
/// N+1 is checked out and built (every test binary, independent of the
/// selection) while commit N's selection runs; N+1's selection still waits
/// for N's merged records (approved speedup #4).
fn chain(
    run: &FreshRun<'_>,
    workers: &[Worker],
    dir: &Path,
    tx: &mpsc::Sender<ChainStep>,
) -> Result<()> {
    let ctx = run.ctx;
    anyhow::ensure!(workers.len() >= 2, "the chain needs two workers");
    let state_path = dir.join("state.cbor");
    let checkpoint = run.prepared.checkpoints.first().context("no checkpoint")?;
    let mut state: ChainState = fs::read(&state_path)
        .ok()
        .and_then(|b| hord_encoding::decode(&b).ok())
        .unwrap_or_else(|| ChainState {
            next: 0,
            ledger: run.initial.clone(),
            single: Some(run.initial.clone()),
            drift: Drift::new(checkpoint.snapshot),
            defs: checkpoint.defs.clone(),
        });
    for i in 0..state.next {
        if let Some(step) = read_json::<ChainStep>(&dir.join(format!("{i}.json"))) {
            let _ = tx.send(step);
        }
    }
    let toolchain = ctx.toolchain.id()?;
    let targets = [
        run.work.join("coverage-target-0"),
        run.work.join("coverage-target-1"),
    ];
    let commits = &run.prepared.commits;
    let build_options = |target: &Path| CoverageOptions {
        packages: None,
        target_dir: target.to_path_buf(),
        jobs: run.coverage_jobs,
        test_timeout: Duration::from_secs(600),
        skip: BTreeSet::new(),
        only: None,
        lines_of_interest: BTreeMap::new(),
    };
    std::thread::scope(|scope| -> Result<()> {
        let spawn_build = |i: usize| {
            let (worker, target) = (&workers[i % 2], &targets[i % 2]);
            let facts = &commits[i];
            let options = build_options(target);
            scope.spawn(move || -> Result<Option<BuiltSuite>> {
                worker.checkout.checkout(&facts.commit)?;
                Ok(build_suite(
                    &VerifyCheckout {
                        root: worker.checkout.root.clone(),
                        snapshot: facts.result_snapshot,
                    },
                    &options,
                )?)
            })
        };
        let mut pending = (state.next < commits.len()).then(|| spawn_build(state.next));
        while state.next < commits.len() {
            if (run.over_budget)() {
                break;
            }
            let i = state.next;
            let facts = &commits[i];
            let started = Instant::now();
            let built = match pending.take() {
                Some(handle) => handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("build panicked"))?,
                None => Err(anyhow::anyhow!("no build for commit {i}")),
            };
            if i + 1 < commits.len() && !(run.over_budget)() {
                pending = Some(spawn_build(i + 1));
            }
            state.defs.update(&facts.defs_delta);
            for p in &facts.defs_removed {
                state.defs.remove_file(p);
            }
            state
                .drift
                .push(facts.result_snapshot, facts.write_set.clone());
            let mut step = ChainStep {
                index: facts.index,
                commit: facts.commit.clone(),
                write_set: facts.write_set.len(),
                ..ChainStep::default()
            };
            let outcome = built.and_then(|suite| {
                chain_step(
                    run,
                    &workers[i % 2],
                    facts,
                    &state,
                    toolchain,
                    suite,
                    &mut step,
                )
            });
            match outcome {
                Ok(fresh) => {
                    state.ledger = state.ledger.merge(&fresh);
                    state.single = state.single.as_ref().map(|s| s.merge_keeping(&fresh, 1));
                }
                Err(err) => {
                    // The records cannot be refreshed; keep them (the next
                    // selection sees this commit as drift).
                    step.error = Some(format!("{err:#}"));
                }
            }
            step.chain_ms = elapsed(started);
            eprintln!(
                "[lander {}] {} ws={} selected {}/{}{} (one-run records: {}) drift+{} ran {} ({} failed unmutated) build {:.0}s run {:.0}s wall {:.0}s{}",
                step.index,
                &step.commit[..10],
                step.write_set,
                step.selected,
                step.suite,
                if step.full { " full" } else { "" },
                step.selected_single,
                step.drift_selected,
                step.tests_run,
                step.unmutated_failed.len(),
                step.build_ms as f64 / 1000.0,
                step.run_ms as f64 / 1000.0,
                step.chain_ms as f64 / 1000.0,
                step.error
                    .as_deref()
                    .map_or(String::new(), |e| format!(" error: {e}"))
            );
            write_json(&dir.join(format!("{}.json", step.index)), &step)?;
            state.next += 1;
            fs::write(&state_path, hord_encoding::encode(&state)?)?;
            let _ = tx.send(step);
        }
        // A build started past the budget is left to finish; its result is
        // unused.
        if let Some(handle) = pending {
            let _ = handle.join();
        }
        Ok(())
    })
}

/// Select on the records, run the selection instrumented on `suite`, and
/// return the fresh coverage.
fn chain_step(
    run: &FreshRun<'_>,
    worker: &Worker,
    facts: &CommitFacts,
    state: &ChainState,
    toolchain: hord_core::ObjectId,
    suite: Option<BuiltSuite>,
    step: &mut ChainStep,
) -> Result<CoverageRecord> {
    let ctx = run.ctx;
    let root = &worker.checkout.root;
    let workspace = CargoWorkspace::load(root)?;
    let ledger = &state.ledger;
    let input = SelectInput {
        coverage: Some(ledger),
        toolchain,
        workspace: &workspace,
        impact: &facts.own_impact,
        max_impact: None,
        drift: Some(&state.drift),
    };
    let selection = select(input);
    let without_drift = select(SelectInput {
        drift: None,
        ..input
    });
    let (selected, suite_size) = count(&selection, ledger, &ctx.quarantine);
    step.selected = selected;
    step.suite = suite_size;
    step.full = selection.full;
    step.drift_selected = selected.saturating_sub(count(&without_drift, ledger, &ctx.quarantine).0);
    if let Some(single) = &state.single {
        let one = select(SelectInput {
            coverage: Some(single),
            ..input
        });
        step.selected_single = count(&one, single, &ctx.quarantine).0;
    }
    let kinds: BTreeSet<&str> = selection.fallbacks.iter().map(Fallback::kind).collect();
    step.fallbacks = kinds.into_iter().map(str::to_owned).collect();

    // Units for the graders: the selection, (a), and the probes.
    let mut new_tests: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &facts.own_impact.facts.touched {
        if t.test
            && t.delta != DefDelta::Died
            && ledger.tests_at(t.node).next().is_none()
            && let (Some(p), Some(name)) = (
                workspace.package_of(&t.path),
                t.name
                    .as_ref()
                    .and_then(|n| n.as_str().rsplit("::").next().map(str::to_owned)),
            )
        {
            new_tests.entry(p.name.clone()).or_default().insert(name);
        }
    }
    step.units = units(&selection, ledger, &ctx.quarantine, &workspace, &new_tests)
        .into_iter()
        .collect();
    let touched: BTreeSet<String> = facts
        .changed_paths
        .iter()
        .filter_map(|p| p.parse::<RepoPath>().ok())
        .filter_map(|p| workspace.package_of(&p).map(|p| p.name.clone()))
        .collect();
    let affected = workspace.with_reverse_deps(&touched);
    step.affected = affected.iter().cloned().collect();
    let a = Selection {
        packages: affected,
        ..Selection::default()
    };
    step.a_units = units(&a, ledger, &ctx.quarantine, &workspace, &new_tests)
        .into_iter()
        .collect();
    step.probes = facts
        .targets
        .iter()
        .map(|t| {
            let probe = ledger
                .tests_covering(&[t.node].into_iter().collect())
                .into_iter()
                .filter(|r| !ctx.quarantine.contains(*r))
                .map(|r| Unit::Test(r.clone()))
                .collect();
            (t.node, probe)
        })
        .collect();

    // The lander's instrumented run of the selection, unmutated.
    let filter = TestFilter::from_selection(&selection);
    let options = CoverageOptions {
        packages: None,
        target_dir: PathBuf::new(),
        jobs: run.coverage_jobs,
        test_timeout: Duration::from_secs(600),
        skip: ctx.quarantine.clone(),
        only: Some(filter.clone()),
        lines_of_interest: BTreeMap::new(),
    };
    let empty = || {
        CoverageRecord::new(
            facts.result_snapshot,
            toolchain,
            BTreeSet::new(),
            Vec::new(),
        )
    };
    let (fresh, build_ms) = match suite {
        _ if filter.is_empty() => (empty(), 0),
        None => (empty(), 0),
        Some(suite) => {
            let started = Instant::now();
            let run = run_suite(&suite, &ctx.toolchain, &state.defs, &options, started)?;
            step.run_ms = elapsed(started);
            step.coverage_log = run.log.chars().take(200_000).collect();
            (run.record, suite.build_ms)
        }
    };
    step.build_ms = build_ms;
    step.tests_run = fresh.tests.len();
    step.unmutated_failed = fresh
        .tests
        .iter()
        .filter(|t| t.failed)
        .map(|t| t.test.name.clone())
        .collect();
    Ok(fresh)
}

/// Grade one commit: fault, (b), (a) if needed, confirm.
fn grade(ctx: &Ctx, worker: &Worker, facts: &CommitFacts, step: &ChainStep) -> Result<FreshResult> {
    let started = Instant::now();
    let mut result = FreshResult {
        step: step.clone(),
        ..FreshResult::default()
    };
    if let Some(err) = &step.error {
        result.error = Some(format!("chain: {err}"));
        return Ok(result);
    }
    let root = &worker.checkout.root;
    worker.checkout.checkout(&facts.commit)?;
    let runner = CargoRunner {
        target_dir: Some(worker.target_dir.clone()),
        timeout: Some(ctx.timeout),
        idle_timeout: Some(ctx.idle),
        env: [("LLVM_PROFILE_FILE".to_owned(), worker.profraw_pattern())]
            .into_iter()
            .collect(),
        ..CargoRunner::default()
    };
    let set: BTreeSet<Unit> = step.units.iter().cloned().collect();
    let a_set: BTreeSet<Unit> = step.a_units.iter().cloned().collect();
    result.covers_a = a_set.is_subset(&set);
    let scope: Vec<String> = step
        .affected
        .iter()
        .flat_map(|p| ["-p".to_owned(), p.clone()])
        .collect();

    let mut injected: Option<(String, NodeId)> = None;
    let mut rejected = 0;
    for (target, kind) in attempts(ctx.seed, &facts.commit, &facts.targets) {
        let t = &facts.targets[target];
        let file = root.join(&t.path);
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let Some(def_text) = text.get(t.span.clone()) else {
            continue;
        };
        let Some(faulty) = inject(kind, def_text) else {
            continue;
        };
        fs::write(
            &file,
            format!("{}{faulty}{}", &text[..t.span.start], &text[t.span.end..]),
        )?;
        let b = Instant::now();
        let mut args = vec!["test".to_owned(), "--no-run".to_owned()];
        args.extend(scope.iter().cloned());
        let built = runner.run(root, &cargo(args))?;
        result.build_ms += elapsed(b);
        if built.success() {
            result.fault = Some(Fault {
                path: t.path.clone(),
                name: t.name.clone(),
                kind,
                rejected,
                edited_lines: 0,
            });
            injected = Some((t.path.clone(), t.node));
            break;
        }
        worker.checkout.restore(&t.path)?;
        rejected += 1;
    }
    let Some((fault_path, fault_node)) = injected else {
        result.no_fault = Some(if facts.targets.is_empty() {
            "the commit writes no function".into()
        } else {
            format!("no fault type-checks ({rejected} tried)")
        });
        result.grade_ms = elapsed(started);
        return Ok(result);
    };

    let probe: BTreeSet<Unit> = step
        .probes
        .iter()
        .find(|(n, _)| *n == fault_node)
        .map(|(_, u)| u.iter().cloned().collect())
        .unwrap_or_default();
    let mut grader = Grader {
        runner: &runner,
        root,
        ran: BTreeSet::new(),
        failed: BTreeSet::new(),
        unattributed: false,
        timed_out: false,
    };
    // Option 2: when (b) contains all of (a), no miss is possible; run only
    // the probe (the tests that ran the faulted function) to record whether
    // it caught the fault.
    if result.covers_a {
        let batch: Vec<Unit> = probe.intersection(&set).cloned().collect();
        grader.run(batch, &|failed| !failed.is_empty())?;
        let caught = !set.is_disjoint(&grader.failed);
        result.probe_only = true;
        result.detected = caught.then_some(true);
        result.detected_a = caught.then_some(true);
        result.failed = grader.failed.iter().map(Unit::label).collect();
        result.unattributed = grader.unattributed;
        worker.checkout.restore(&fault_path)?;
        result.grade_ms = elapsed(started);
        return Ok(result);
    }
    // (b): the selection, the tests that ran the faulted function first.
    let mut batch: Vec<Unit> = set.iter().cloned().collect();
    batch.sort_by_key(|u| !probe.contains(u));
    grader.run(batch, &|failed| !set.is_disjoint(failed))?;
    let detected = !set.is_disjoint(&grader.failed);
    // (a): only when (b) did not detect and does not contain all of (a).
    if !detected && !result.covers_a {
        let mut batch: Vec<Unit> = a_set.difference(&grader.ran).cloned().collect();
        batch.sort_by_key(|u| !probe.contains(u));
        grader.run(batch, &|failed| !a_set.is_disjoint(failed))?;
    }
    result.detected = Some(detected);
    result.detected_a = if !a_set.is_disjoint(&grader.failed) {
        Some(true)
    } else if a_set.is_subset(&grader.ran) {
        Some(false)
    } else {
        None
    };
    result.miss = result.detected_a == Some(true) && !detected;
    result.failed = grader.failed.iter().map(Unit::label).collect();
    result.unattributed = grader.unattributed;
    worker.checkout.restore(&fault_path)?;
    if result.miss {
        let missed: Vec<Unit> = grader
            .failed
            .iter()
            .filter(|u| a_set.contains(u) && !set.contains(u))
            .cloned()
            .collect();
        let mut clean = Grader {
            runner: &runner,
            root,
            ran: BTreeSet::new(),
            failed: BTreeSet::new(),
            unattributed: false,
            timed_out: false,
        };
        clean.run(missed.clone(), &|_| false)?;
        result.confirmed_miss =
            !missed.is_empty() && clean.failed.is_empty() && !grader.unattributed;
    }
    result.grade_ms = elapsed(started);
    Ok(result)
}

/// The report of a fresh run.
#[derive(Debug, Serialize)]
pub(crate) struct FreshReport {
    pub commits: usize,
    pub chained: usize,
    pub graded: usize,
    pub errors: usize,
    pub small_commits: usize,
    /// Median share of the suite selected, over commits with write set <= 5.
    pub efficiency_median_small: Option<f64>,
    pub efficiency_median_all: Option<f64>,
    /// The same median with one run per record instead of the union of the
    /// last three: the union rule's efficiency cost is the difference.
    pub efficiency_median_small_single_run: Option<f64>,
    /// Mean instrumented build (overlapped with the previous run) and run
    /// time per commit, in seconds.
    pub build_secs_mean: f64,
    pub run_secs_mean: f64,
    pub median_selected_small: Option<f64>,
    pub faults_injected: usize,
    pub detected_b: usize,
    pub detected_a: usize,
    pub covers_a: usize,
    /// Faults graded by the probe only (option 2).
    pub probe_only: usize,
    pub misses: Vec<String>,
    pub confirmed_misses: usize,
    /// Commits each fallback kind fired on.
    pub fallbacks: BTreeMap<String, usize>,
    /// Commits where drift selected extra tests, and their median count.
    pub drift_commits: usize,
    pub drift_median_extra: Option<f64>,
    /// The lander's wall time per commit (selection + instrumented build
    /// and run + merge), in seconds.
    pub lander_secs_mean: f64,
    pub lander_secs_median: Option<f64>,
    pub lander_secs_p90: Option<f64>,
    pub lander_secs_total: f64,
    /// The initial full instrumented run, in seconds.
    pub initial_secs: f64,
    /// Projected lander time for 500 commits: initial + 500 × mean.
    pub projected_500_hours: f64,
    /// Grading (fault, (b), (a)) is the harness's cost, not the lander's.
    pub grade_secs_mean: f64,
    pub profraw_in_cwd: Vec<String>,
    pub safety_gate: bool,
    pub efficiency_gate: bool,
    pub results: Vec<FreshResult>,
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

fn mean(v: Vec<f64>) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn percentile(mut v: Vec<f64>, p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v.get(i).copied()
}

/// Collect the chain steps and grades under `work` into a report.
pub(crate) fn report(work: &Path, prepared: &Prepared, initial_secs: f64) -> FreshReport {
    let steps: Vec<ChainStep> = prepared
        .commits
        .iter()
        .filter_map(|c| read_json(&work.join(format!("chain/{}.json", c.index))))
        .collect();
    let results: Vec<FreshResult> = prepared
        .commits
        .iter()
        .filter_map(|c| read_json(&work.join(format!("fresh-results/{}.json", c.index))))
        .collect();
    let ok_steps: Vec<&ChainStep> = steps.iter().filter(|s| s.error.is_none()).collect();
    let share = |s: &ChainStep| s.selected as f64 / s.suite.max(1) as f64;
    let small: Vec<&&ChainStep> = ok_steps.iter().filter(|s| s.write_set <= 5).collect();
    let mut fallbacks: BTreeMap<String, usize> = BTreeMap::new();
    for s in &ok_steps {
        if s.fallbacks.is_empty() {
            *fallbacks.entry("(none)".into()).or_default() += 1;
        }
        for k in &s.fallbacks {
            *fallbacks.entry(k.clone()).or_default() += 1;
        }
    }
    let lander: Vec<f64> = steps.iter().map(|s| s.chain_ms as f64 / 1000.0).collect();
    let lander_mean = if lander.is_empty() {
        0.0
    } else {
        lander.iter().sum::<f64>() / lander.len() as f64
    };
    let graded: Vec<&FreshResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let faulted: Vec<&&FreshResult> = graded.iter().filter(|r| r.fault.is_some()).collect();
    let misses: Vec<String> = faulted
        .iter()
        .filter(|r| r.miss)
        .map(|r| format!("{} {}", r.step.index, &r.step.commit[..10]))
        .collect();
    let drift: Vec<f64> = ok_steps
        .iter()
        .filter(|s| s.drift_selected > 0)
        .map(|s| s.drift_selected as f64)
        .collect();
    let efficiency_median_small = median(small.iter().map(|s| share(s)).collect());
    FreshReport {
        commits: prepared.commits.len(),
        chained: steps.len(),
        graded: results.len(),
        errors: steps.len() - ok_steps.len() + results.iter().filter(|r| r.error.is_some()).count(),
        small_commits: small.len(),
        efficiency_median_small,
        efficiency_median_all: median(ok_steps.iter().map(|s| share(s)).collect()),
        efficiency_median_small_single_run: median(
            small
                .iter()
                .map(|s| s.selected_single as f64 / s.suite.max(1) as f64)
                .collect(),
        ),
        build_secs_mean: mean(
            ok_steps
                .iter()
                .map(|s| s.build_ms as f64 / 1000.0)
                .collect(),
        ),
        run_secs_mean: mean(ok_steps.iter().map(|s| s.run_ms as f64 / 1000.0).collect()),
        median_selected_small: median(small.iter().map(|s| s.selected as f64).collect()),
        faults_injected: faulted.len(),
        detected_b: faulted.iter().filter(|r| r.detected == Some(true)).count(),
        detected_a: faulted
            .iter()
            .filter(|r| r.detected_a == Some(true))
            .count(),
        covers_a: faulted.iter().filter(|r| r.covers_a).count(),
        probe_only: faulted.iter().filter(|r| r.probe_only).count(),
        confirmed_misses: faulted.iter().filter(|r| r.confirmed_miss).count(),
        safety_gate: misses.is_empty(),
        misses,
        fallbacks,
        drift_commits: drift.len(),
        drift_median_extra: median(drift),
        lander_secs_mean: lander_mean,
        lander_secs_median: median(lander.clone()),
        lander_secs_p90: percentile(lander.clone(), 0.9),
        lander_secs_total: lander.iter().sum(),
        initial_secs,
        projected_500_hours: (initial_secs + 500.0 * lander_mean) / 3600.0,
        grade_secs_mean: if graded.is_empty() {
            0.0
        } else {
            graded
                .iter()
                .map(|r| r.grade_ms as f64 / 1000.0)
                .sum::<f64>()
                / graded.len() as f64
        },
        profraw_in_cwd: Vec::new(),
        efficiency_gate: efficiency_median_small.is_some_and(|m| m <= 0.20),
        results,
    }
}

fn pct(m: Option<f64>) -> String {
    m.map_or("n/a".to_owned(), |m| format!("{:.1}%", m * 100.0))
}

/// Print the report, and return its Markdown summary.
pub(crate) fn print(r: &FreshReport) -> String {
    let secs = |v: Option<f64>| v.map_or("n/a".to_owned(), |v| format!("{v:.0} s"));
    let mut md = String::new();
    md.push_str(&format!(
        "# M4 fresh per-test coverage (lander view): {} commits\n\n",
        r.chained
    ));
    md.push_str("| Measure | Value |\n|---|---|\n");
    let rows = [
        (
            "Commits chained / graded / errors",
            format!("{} / {} / {}", r.chained, r.graded, r.errors),
        ),
        (
            "Efficiency, median share (write set <= 5)",
            format!(
                "{} (n={}, median {} tests)",
                pct(r.efficiency_median_small),
                r.small_commits,
                r.median_selected_small
                    .map_or("n/a".into(), |m| format!("{m:.0}"))
            ),
        ),
        (
            "Efficiency, median share (all)",
            pct(r.efficiency_median_all),
        ),
        (
            "Union rule's cost: median share (ws <= 5) with one run per record",
            pct(r.efficiency_median_small_single_run),
        ),
        (
            "Instrumented build (overlapped) / run, mean per commit",
            format!("{:.0} s / {:.0} s", r.build_secs_mean, r.run_secs_mean),
        ),
        (
            "Faults injected / (b) detected / (a) detected / (b) covers (a) / probe only",
            format!(
                "{} / {} / {} / {} / {}",
                r.faults_injected, r.detected_b, r.detected_a, r.covers_a, r.probe_only
            ),
        ),
        (
            "Misses (confirmed)",
            format!("{} ({}) {:?}", r.misses.len(), r.confirmed_misses, r.misses),
        ),
        (
            "Commits where drift added tests (median extra)",
            format!(
                "{} ({})",
                r.drift_commits,
                r.drift_median_extra
                    .map_or("n/a".into(), |m| format!("{m:.0}"))
            ),
        ),
        (
            "Lander wall time per commit: mean / median / p90",
            format!(
                "{:.0} s / {} / {}",
                r.lander_secs_mean,
                secs(r.lander_secs_median),
                secs(r.lander_secs_p90)
            ),
        ),
        (
            "Initial full instrumented run",
            format!("{:.1} min", r.initial_secs / 60.0),
        ),
        (
            "Projected 500 commits (initial + 500 × mean)",
            format!("{:.1} h", r.projected_500_hours),
        ),
        (
            "Grading per commit (harness, not lander)",
            format!("{:.0} s", r.grade_secs_mean),
        ),
        (
            "profraw in the working directory",
            format!("{:?}", r.profraw_in_cwd),
        ),
        (
            "Gates",
            format!(
                "safety {}, efficiency {}",
                if r.safety_gate { "PASS" } else { "FAIL" },
                if r.efficiency_gate { "PASS" } else { "FAIL" }
            ),
        ),
    ];
    for (k, v) in rows {
        println!("  {k}: {v}");
        md.push_str(&format!("| {k} | {v} |\n"));
    }
    println!("  fallbacks (commits): {:?}", r.fallbacks);
    md.push_str("\n## Fallback kinds (commits fired)\n\n| Kind | Commits |\n|---|---|\n");
    for (k, n) in &r.fallbacks {
        md.push_str(&format!("| {k} | {n} |\n"));
    }
    md.push_str("\n## Commits\n\n| # | Commit | ws | Selected | Fallbacks | Drift + | Lander s | Fault | (b) | (a) |\n|---|---|---|---|---|---|---|---|---|---|\n");
    for x in &r.results {
        let s = &x.step;
        md.push_str(&format!(
            "| {} | {} | {} | {}/{} | {} | {} | {:.0} | {} | {} | {} |\n",
            s.index,
            &s.commit[..10.min(s.commit.len())],
            s.write_set,
            s.selected,
            s.suite,
            s.fallbacks.join(", "),
            s.drift_selected,
            s.chain_ms as f64 / 1000.0,
            x.fault.as_ref().map_or_else(
                || x.error.clone().unwrap_or_else(|| "none".into()),
                |f| format!("{:?} `{}`", f.kind, f.name)
            ),
            x.detected.map_or("-".into(), |d| if x.miss {
                "MISS".into()
            } else {
                d.to_string()
            }),
            x.detected_a.map_or("-".into(), |d| d.to_string()),
        ));
    }
    md
}

/// Where the fresh run keeps its initial record.
pub(crate) fn initial_path(work: &Path) -> PathBuf {
    work.join("chain/initial.cbor")
}
