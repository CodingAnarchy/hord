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
//! without the fault. A failure that also happens without the fault
//! detects nothing (by (a), or by (b) when no failing test executed the
//! faulted function). Only confirmed misses fail the safety gate;
//! unconfirmed ones are listed for review. The fault never reaches the
//! records: the chain ran the commit unmutated.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use hord_core::{NodeId, RepoPath};
use hord_verify::{Checkout as VerifyCheckout, CoverageRecord, DefDelta, Drift};
use hord_verify_rust::coverage::{BuiltSuite, build_suite_with_env, run_suite};
use hord_verify_rust::{
    CargoRunner, CargoWorkspace, CoverageOptions, DefinitionIndex, Fallback, SelectInput,
    Selection, TestFilter, select,
};
use serde::{Deserialize, Serialize};

use crate::disk::{self, Monitor};
use crate::fault::{FaultKind, attempts, inject};
use crate::prepare::{CommitFacts, Prepared};
use crate::run::{
    Ctx, Fault, Grader, Unit, Worker, cargo, corpus_env, count, elapsed, runner_env, units,
};
use crate::{SampleResult, pct, read_json, write_json};

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
/// One graded fault on a commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FaultGrade {
    pub fault: Fault,
    /// Only the probe ran: the selection contains all of (a), so no miss is
    /// possible (option 2); `detected` is `None` unless the probe caught it.
    pub probe_only: bool,
    /// Whether (b), the selection, detected the fault.
    pub detected: Option<bool>,
    /// Whether (a) did, when determined.
    pub detected_a: Option<bool>,
    pub miss: bool,
    /// The missed failures pass without the fault.
    pub confirmed_miss: bool,
    /// The confirmed missed tests: (a) failures (b) did not contain that
    /// pass without the fault.
    #[serde(default)]
    pub missed: Vec<String>,
    /// Failures that also happen without the fault: not detections, by (b)
    /// or (a).
    #[serde(default)]
    pub unrelated: Vec<String>,
    /// Output of each unrelated failure, with and without the fault, to
    /// diagnose why it fails either way.
    #[serde(default)]
    pub unrelated_output: Vec<UnrelatedOutput>,
    pub failed: Vec<String>,
    /// A command hung or crashed; its units were counted as failing.
    pub unattributed: bool,
    pub build_ms: u64,
    pub grade_ms: u64,
}

/// A test that failed with and without the fault, and its output in each.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnrelatedOutput {
    pub test: String,
    pub with_fault: String,
    pub without_fault: String,
}

/// The output of each unrelated test, from the faulted and the clean runs.
fn unrelated_output(
    unrelated: &BTreeSet<Unit>,
    with_fault: &BTreeMap<String, String>,
    without_fault: &BTreeMap<String, String>,
) -> Vec<UnrelatedOutput> {
    unrelated
        .iter()
        .filter_map(|u| match u {
            Unit::Test(t) => Some(UnrelatedOutput {
                test: u.label(),
                with_fault: with_fault.get(&t.name).cloned().unwrap_or_default(),
                without_fault: without_fault.get(&t.name).cloned().unwrap_or_default(),
            }),
            _ => None,
        })
        .collect()
}

/// One graded commit: up to `--faults-per-commit` faults where (b) does not
/// contain (a), one probe-graded fault where it does (ADR 0023 amendment).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct FreshResult {
    pub step: ChainStep,
    /// The selection contains every test of (a): no miss is possible.
    pub covers_a: bool,
    pub faults: Vec<FaultGrade>,
    /// Why no fault was graded.
    pub no_fault: Option<String>,
    /// Fault attempts that did not type-check.
    pub rejected: usize,
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
    /// Disk telemetry, sampled after every chain step and grade.
    pub disk: &'a Monitor,
}

/// Run the chain on `chain_worker` and grade on `graders` as steps arrive.
pub(crate) fn run(run: &FreshRun<'_>, chain_workers: &[Worker], graders: &[Worker]) -> Result<()> {
    let chain_dir = run.work.join("chain");
    let results_dir = run.work.join("fresh-results");
    fs::create_dir_all(&chain_dir)?;
    let (tx, rx) = mpsc::channel::<ChainStep>();
    let rx = Mutex::new(rx);
    thread::scope(|scope| -> Result<()> {
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
                    let since = SystemTime::now();
                    let result = match grade(run.ctx, worker, facts, &step) {
                        Ok(r) => r,
                        Err(err) => FreshResult {
                            step: step.clone(),
                            error: Some(format!("{err:#}")),
                            ..FreshResult::default()
                        },
                    };
                    let faults: Vec<String> = result
                        .faults
                        .iter()
                        .map(|f| {
                            format!(
                                "{:?}:b={:?}/a={:?}{}",
                                f.fault.kind,
                                f.detected,
                                f.detected_a,
                                if f.miss { " MISS" } else { "" }
                            )
                        })
                        .collect();
                    eprintln!(
                        "[grade {}] {} ws={} selected {}/{}{} faults [{}]{} ({:.0}s)",
                        step.index,
                        &step.commit[..10],
                        step.write_set,
                        step.selected,
                        step.suite,
                        if result.covers_a { " (b covers a)" } else { "" },
                        faults.join(", "),
                        result
                            .error
                            .as_deref()
                            .map_or(String::new(), |e| format!(" error: {e}")),
                        t.elapsed().as_secs_f64()
                    );
                    if let Err(err) = write_json(&out, &result) {
                        eprintln!("[grade {}] {err:#}", step.index);
                    }
                    // Keep the grader's target to what the next commit needs.
                    let label = format!("grade {}", step.index);
                    disk::prune_and_log(&label, &worker.target_dir, since);
                    disk::clear_dir(&worker.profraw_dir());
                    run.disk.sample(&label);
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
    let targets = [coverage_target(run.work, 0), coverage_target(run.work, 1)];
    let commits = &run.prepared.commits;
    let build_options = |target: &Path| CoverageOptions {
        packages: None,
        target_dir: target.to_path_buf(),
        jobs: run.coverage_jobs,
        test_timeout: Duration::from_secs(600),
        skip: BTreeSet::new(),
        only: None,
        lines_of_interest: BTreeMap::new(),
        cancel: Default::default(),
    };
    thread::scope(|scope| -> Result<()> {
        let spawn_build = |i: usize| {
            let (worker, target) = (&workers[i % 2], &targets[i % 2]);
            let facts = &commits[i];
            let options = build_options(target);
            let since = SystemTime::now();
            let handle = scope.spawn(move || -> Result<Option<BuiltSuite>> {
                worker.checkout.checkout(&facts.commit)?;
                Ok(build_suite_with_env(
                    &VerifyCheckout {
                        root: worker.checkout.root.clone(),
                        snapshot: facts.result_snapshot,
                    },
                    &options,
                    &corpus_env(),
                )?)
            });
            (since, handle)
        };
        let mut pending = (state.next < commits.len()).then(|| spawn_build(state.next));
        while state.next < commits.len() {
            if (run.over_budget)() {
                break;
            }
            let i = state.next;
            let facts = &commits[i];
            let started = Instant::now();
            let (since, built) = match pending.take() {
                Some((since, handle)) => (
                    since,
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("build panicked"))?,
                ),
                None => (
                    SystemTime::now(),
                    Err(anyhow::anyhow!("no build for commit {i}")),
                ),
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
            // This slot builds again two commits on: keep what that build
            // needs (ADR 0022's records are already merged).
            let label = format!("lander {}", step.index);
            disk::prune_and_log(&label, &targets[i % 2], since);
            run.disk.sample(&label);
            let _ = tx.send(step);
        }
        // A build started past the budget is left to finish; its result is
        // unused.
        if let Some((_, handle)) = pending {
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
        cancel: Default::default(),
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

/// Fault attempts in the order they are tried: distinct written functions
/// first (each target's first kind), then second kinds, and so on, so the
/// faults of one commit cover as many functions as they can.
fn fault_order(attempts: Vec<(usize, FaultKind)>) -> Vec<(usize, FaultKind)> {
    let mut groups: Vec<(usize, Vec<FaultKind>)> = Vec::new();
    for (target, kind) in attempts {
        match groups.iter_mut().find(|(t, _)| *t == target) {
            Some((_, kinds)) => kinds.push(kind),
            None => groups.push((target, vec![kind])),
        }
    }
    let rounds = groups.iter().map(|(_, k)| k.len()).max().unwrap_or(0);
    let mut out = Vec::new();
    for round in 0..rounds {
        for (target, kinds) in &groups {
            if let Some(kind) = kinds.get(round) {
                out.push((*target, *kind));
            }
        }
    }
    out
}

/// Grade one commit: up to K faults (ADR 0023 amendment), each injected,
/// built, and graded on its own: (b), then (a) when needed, then a
/// confirmation of any miss.
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
        env: runner_env(worker),
        ..CargoRunner::default()
    };
    let set: BTreeSet<Unit> = step.units.iter().cloned().collect();
    let a_set: BTreeSet<Unit> = step.a_units.iter().cloned().collect();
    result.covers_a = a_set.is_subset(&set);
    let want = if result.covers_a {
        1
    } else {
        ctx.faults_per_commit.max(1)
    };
    let scope: Vec<String> = step
        .affected
        .iter()
        .flat_map(|p| ["-p".to_owned(), p.clone()])
        .collect();
    for (target, kind) in fault_order(attempts(ctx.seed, &facts.commit, &facts.targets)) {
        if result.faults.len() >= want {
            break;
        }
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
        let fault_started = Instant::now();
        let mut args = vec!["test".to_owned(), "--no-run".to_owned()];
        args.extend(scope.iter().cloned());
        let built = runner.run(root, &cargo(args))?;
        let build_ms = elapsed(fault_started);
        if !built.success() {
            worker.checkout.restore(&t.path)?;
            result.rejected += 1;
            continue;
        }
        let fault = Fault {
            path: t.path.clone(),
            name: t.name.clone(),
            kind,
            rejected: result.rejected,
            edited_lines: 0,
        };
        let graded = grade_fault(
            &runner,
            worker,
            step,
            &set,
            &a_set,
            result.covers_a,
            &t.path,
            t.node,
            fault,
        );
        // Always restore, even if grading failed.
        worker.checkout.restore(&t.path)?;
        let mut graded = graded?;
        graded.build_ms = build_ms;
        graded.grade_ms = elapsed(fault_started);
        result.faults.push(graded);
    }
    if result.faults.is_empty() {
        result.no_fault = Some(if facts.targets.is_empty() {
            "the commit writes no function".into()
        } else {
            format!("no fault type-checks ({} tried)", result.rejected)
        });
    }
    result.grade_ms = elapsed(started);
    Ok(result)
}

/// Grade one injected fault. The file is restored by the caller.
#[allow(clippy::too_many_arguments)]
fn grade_fault(
    runner: &CargoRunner,
    worker: &Worker,
    step: &ChainStep,
    set: &BTreeSet<Unit>,
    a_set: &BTreeSet<Unit>,
    covers_a: bool,
    fault_path: &str,
    fault_node: NodeId,
    fault: Fault,
) -> Result<FaultGrade> {
    let root = &worker.checkout.root;
    let probe: BTreeSet<Unit> = step
        .probes
        .iter()
        .find(|(n, _)| *n == fault_node)
        .map(|(_, u)| u.iter().cloned().collect())
        .unwrap_or_default();
    let mut grader = Grader::new(runner, root);
    let mut out = FaultGrade {
        fault,
        probe_only: false,
        detected: None,
        detected_a: None,
        miss: false,
        confirmed_miss: false,
        missed: Vec::new(),
        unrelated: Vec::new(),
        unrelated_output: Vec::new(),
        failed: Vec::new(),
        unattributed: false,
        build_ms: 0,
        grade_ms: 0,
    };
    if covers_a {
        // Option 2: (b) contains (a), so no miss is possible; run only the
        // probe to record whether it caught the fault.
        let batch: Vec<Unit> = probe.intersection(set).cloned().collect();
        grader.run(batch, &|failed| !failed.is_empty())?;
        let caught = !set.is_disjoint(&grader.failed);
        out.probe_only = true;
        out.detected = caught.then_some(true);
        out.detected_a = caught.then_some(true);
        out.failed = grader.failed.iter().map(Unit::label).collect();
        out.unattributed = grader.unattributed;
        return Ok(out);
    }
    // The faulty file, to put back after a clean re-run.
    let file = root.join(fault_path);
    let faulty = fs::read_to_string(&file)?;
    // (b): the selection, the tests that ran the faulted function first.
    let mut batch: Vec<Unit> = set.iter().cloned().collect();
    batch.sort_by_key(|u| !probe.contains(u));
    grader.run(batch, &|failed| !set.is_disjoint(failed))?;
    // Failures that also happen without the fault are not detections
    // (ADR 0023: a miss is a confirmed miss). A (b) failure by a test that
    // never executed the faulted function is checked without the fault
    // before it counts, so a test failing for other reasons cannot stand in
    // for a detection and skip (a).
    let mut unrelated: BTreeSet<Unit> = BTreeSet::new();
    let mut clean_output: BTreeMap<String, String> = BTreeMap::new();
    let b_failed: BTreeSet<Unit> = grader.failed.intersection(set).cloned().collect();
    if !b_failed.is_empty() && b_failed.is_disjoint(&probe) {
        let clean = clean_run(runner, worker, fault_path, &b_failed)?;
        out.unattributed |= clean.unattributed;
        unrelated.extend(clean.failed);
        clean_output.extend(clean.excerpts);
        fs::write(&file, &faulty)?;
    }
    let detected = grader
        .failed
        .iter()
        .any(|u| set.contains(u) && !unrelated.contains(u));
    if !detected {
        let mut batch: Vec<Unit> = a_set.difference(&grader.ran).cloned().collect();
        batch.sort_by_key(|u| !probe.contains(u));
        grader.run(batch, &|failed| {
            failed
                .iter()
                .any(|u| a_set.contains(u) && !set.contains(u) && !unrelated.contains(u))
        })?;
    }
    // (a) failures (b) did not contain: a miss, confirmed when they pass
    // without the fault.
    let missed: BTreeSet<Unit> = grader
        .failed
        .iter()
        .filter(|u| a_set.contains(u) && !set.contains(u) && !unrelated.contains(u))
        .cloned()
        .collect();
    let mut confirmed: BTreeSet<Unit> = BTreeSet::new();
    if !detected && !missed.is_empty() {
        let clean = clean_run(runner, worker, fault_path, &missed)?;
        out.unattributed |= clean.unattributed;
        confirmed = missed.difference(&clean.failed).cloned().collect();
        unrelated.extend(clean.failed);
        clean_output.extend(clean.excerpts);
    }
    let classified = classify(a_set, &grader, &unrelated, detected, &missed, &confirmed);
    out.detected = Some(detected);
    out.detected_a = classified.detected_a;
    out.miss = classified.miss;
    out.unattributed |= grader.unattributed;
    out.confirmed_miss = classified.confirmed_miss && !out.unattributed;
    out.failed = grader.failed.iter().map(Unit::label).collect();
    out.missed = confirmed.iter().map(Unit::label).collect();
    out.unrelated = unrelated.iter().map(Unit::label).collect();
    out.unrelated_output = unrelated_output(&unrelated, &grader.excerpts, &clean_output);
    Ok(out)
}

/// How one fault's runs grade, once failures without the fault are known.
#[derive(Debug, PartialEq, Eq)]
struct Classified {
    detected_a: Option<bool>,
    /// (a) failed on tests (b) did not contain, before confirmation.
    miss: bool,
    /// Some of those tests pass without the fault.
    confirmed_miss: bool,
}

/// Grade from the runs: a failure in `unrelated` (it also fails without
/// the fault) detects nothing.
fn classify(
    a_set: &BTreeSet<Unit>,
    grader: &Grader<'_>,
    unrelated: &BTreeSet<Unit>,
    detected: bool,
    missed: &BTreeSet<Unit>,
    confirmed: &BTreeSet<Unit>,
) -> Classified {
    let a_detected = grader
        .failed
        .iter()
        .any(|u| a_set.contains(u) && !unrelated.contains(u));
    Classified {
        detected_a: if a_detected {
            Some(true)
        } else if a_set.is_subset(&grader.ran) {
            Some(false)
        } else {
            None
        },
        miss: !detected && !missed.is_empty(),
        confirmed_miss: !detected && !confirmed.is_empty(),
    }
}

/// Re-run `units` with the fault removed (the caller puts it back if it
/// grades on).
fn clean_run<'a>(
    runner: &'a CargoRunner,
    worker: &'a Worker,
    fault_path: &str,
    units: &BTreeSet<Unit>,
) -> Result<Grader<'a>> {
    worker.checkout.restore(fault_path)?;
    let mut clean = Grader::new(runner, &worker.checkout.root);
    clean.run(units.iter().cloned().collect(), &|_| false)?;
    Ok(clean)
}

/// What a chain was: written to `chain/meta.json` when it starts, so a
/// merge knows each chain's window and whether it finished.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ChainMeta {
    /// `i/n`, or empty for a run without `--chain`.
    pub chain: String,
    pub head: String,
    pub stride: usize,
    pub commits: Vec<String>,
    pub faults_per_commit: usize,
}

/// One chain's (or a single run's) results, as read from its work dir.
#[derive(Debug, Serialize)]
pub(crate) struct ChainReport {
    pub chain: String,
    pub dir: String,
    /// Commits in the chain's window, chained, graded, with errors.
    pub commits: usize,
    pub chained: usize,
    pub graded: usize,
    pub errors: usize,
    pub complete: bool,
    pub small_commits: usize,
    /// Median share of the suite selected, over commits with write set <= 5.
    pub efficiency_median_small: Option<f64>,
    /// The same, with the merged quarantine left out of the selection and
    /// the suite (see [`share_quarantined`]).
    pub efficiency_median_small_quarantined: Option<f64>,
    pub efficiency_median_all: Option<f64>,
    pub counts: Counts,
    /// Commits each fallback kind fired on.
    pub fallbacks: BTreeMap<String, usize>,
    /// The lander's wall time per commit (selection + instrumented build
    /// and run + merge), in seconds.
    pub lander_secs_mean: f64,
    pub lander_secs_median: Option<f64>,
    pub lander_secs_p90: Option<f64>,
    pub lander_secs_total: f64,
    /// The chain's initial full instrumented run, in seconds.
    pub initial_secs: f64,
    /// Grading (faults, (b), (a)) per commit, in seconds: the harness's cost,
    /// not the lander's.
    pub grade_secs_mean: f64,
    /// Confirmed misses (ADR 0023): chain, commit, fault, missed tests.
    pub misses: Vec<String>,
    /// (a) failed where (b) did not, but only on tests that also fail
    /// without the fault: listed for review, not gated.
    #[serde(default)]
    pub unconfirmed_misses: Vec<String>,
    /// Peak disk use per filesystem (`chain/disk.json`), if sampled.
    pub disk: Option<disk::DiskLog>,
}

/// Per-fault and per-commit counts (ADR 0023 amendment: faults within one
/// commit are correlated, so both are reported).
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Counts {
    /// Faults graded, and those where a miss was possible ((b) does not
    /// contain (a)).
    pub faults: usize,
    pub informative_faults: usize,
    pub faults_detected_b: usize,
    pub faults_detected_a: usize,
    pub fault_misses: usize,
    pub fault_confirmed_misses: usize,
    /// Commits with at least one graded fault; informative ones; ones with a
    /// miss.
    pub commits_with_faults: usize,
    pub informative_commits: usize,
    pub commits_with_miss: usize,
}

impl Counts {
    fn add(&mut self, r: &FreshResult) {
        if r.error.is_some() || r.faults.is_empty() {
            return;
        }
        self.commits_with_faults += 1;
        if !r.covers_a {
            self.informative_commits += 1;
        }
        if r.faults.iter().any(|f| f.confirmed_miss) {
            self.commits_with_miss += 1;
        }
        for f in &r.faults {
            self.faults += 1;
            if !f.probe_only {
                self.informative_faults += 1;
            }
            self.faults_detected_b += usize::from(f.detected == Some(true));
            self.faults_detected_a += usize::from(f.detected_a == Some(true));
            self.fault_misses += usize::from(f.miss);
            self.fault_confirmed_misses += usize::from(f.confirmed_miss);
        }
    }
}

/// The report of one run, or of several chains merged (`--merge`).
#[derive(Debug, Serialize)]
pub(crate) struct FreshReport {
    pub chains: Vec<ChainReport>,
    /// Pooled over every chain.
    pub commits: usize,
    pub chained: usize,
    pub graded: usize,
    pub errors: usize,
    pub complete: bool,
    pub small_commits: usize,
    /// As selected by the chains, which ran with only the seed quarantine.
    pub efficiency_median_small: Option<f64>,
    /// Re-graded with the merged quarantine (seed and sample failures): what
    /// the efficiency gate uses, since quarantined tests are skipped in (a)
    /// and (b) alike (ADR 0023).
    pub efficiency_median_small_quarantined: Option<f64>,
    pub efficiency_median_all: Option<f64>,
    pub counts: Counts,
    pub fallbacks: BTreeMap<String, usize>,
    pub lander_secs_mean: f64,
    /// Confirmed misses: what the safety gate counts.
    pub misses: Vec<String>,
    /// Unconfirmed misses, listed for review.
    pub unconfirmed_misses: Vec<String>,
    /// "(a) failures unrelated to the fault": tests that failed with and
    /// without a fault, with how many faults they did so for.
    pub unrelated_failures: BTreeMap<String, usize>,
    pub quarantine: Vec<String>,
    pub sample: Vec<SampleResult>,
    pub profraw_in_cwd: Vec<String>,
    /// ADR 0023 gate: zero misses, and a median ≤ 20% for write sets ≤ 5.
    pub safety_gate: bool,
    pub efficiency_gate: bool,
    /// Every graded commit, with its chain.
    pub results: Vec<(String, FreshResult)>,
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

fn mean(v: &[f64]) -> f64 {
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

fn share(s: &ChainStep) -> f64 {
    s.selected as f64 / s.suite.max(1) as f64
}

/// A step's share with the quarantined tests out of both the selection and
/// the suite. `in_suite` is how many quarantined names the chain's suite has.
/// Only tests the selection names individually are taken out of it (a
/// package filter may run more), so this never understates the share.
fn share_quarantined(s: &ChainStep, quarantine: &BTreeSet<String>, in_suite: usize) -> f64 {
    let selected_quarantined = s
        .units
        .iter()
        .filter(|u| matches!(u, Unit::Test(t) if quarantine.contains(&t.name)))
        .count();
    let selected = s.selected.saturating_sub(selected_quarantined);
    selected as f64 / s.suite.saturating_sub(in_suite).max(1) as f64
}

/// The test names of a chain's suite: `chain/suite.json` (written after the
/// initial run), or for older work dirs every name its steps mention.
fn suite_names(work: &Path, steps: &[ChainStep]) -> BTreeSet<String> {
    if let Some(names) = read_json::<BTreeSet<String>>(&work.join("chain/suite.json")) {
        return names;
    }
    steps
        .iter()
        .flat_map(|s| {
            s.units
                .iter()
                .filter_map(|u| match u {
                    Unit::Test(t) => Some(t.name.clone()),
                    _ => None,
                })
                .chain(s.unmutated_failed.iter().cloned())
        })
        .collect()
}

/// Where the fresh run lists its suite's test names.
pub(crate) fn suite_path(work: &Path) -> PathBuf {
    work.join("chain/suite.json")
}

fn fallback_counts<'a>(steps: impl Iterator<Item = &'a ChainStep>) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for s in steps {
        if s.fallbacks.is_empty() {
            *out.entry("(none)".into()).or_default() += 1;
        }
        for k in &s.fallbacks {
            *out.entry(k.clone()).or_default() += 1;
        }
    }
    out
}

fn numbered<T: for<'de> Deserialize<'de>>(dir: &Path) -> Vec<(usize, T)> {
    let mut out: Vec<(usize, T)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let index: usize = name.strip_suffix(".json")?.parse().ok()?;
            Some((index, read_json(&e.path())?))
        })
        .collect();
    out.sort_by_key(|(i, _)| *i);
    out
}

/// Read one chain's results from its work dir.
fn chain_report(work: &Path) -> (ChainReport, Vec<ChainStep>, Vec<FreshResult>) {
    let meta: ChainMeta = read_json(&work.join("chain/meta.json")).unwrap_or_default();
    let steps: Vec<ChainStep> = numbered(&work.join("chain"))
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    let results: Vec<FreshResult> = numbered(&work.join("fresh-results"))
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    let ok: Vec<&ChainStep> = steps.iter().filter(|s| s.error.is_none()).collect();
    let small: Vec<&&ChainStep> = ok.iter().filter(|s| s.write_set <= 5).collect();
    let lander: Vec<f64> = steps.iter().map(|s| s.chain_ms as f64 / 1000.0).collect();
    let mut counts = Counts::default();
    for r in &results {
        counts.add(r);
    }
    let initial_secs = read_json::<(u64, usize, String)>(&work.join("chain/initial.json"))
        .map_or(0.0, |m| m.0 as f64 / 1000.0);
    let commits = if meta.commits.is_empty() {
        steps.len()
    } else {
        meta.commits.len()
    };
    let describe = |r: &FreshResult, f: &FaultGrade, tests: &[String]| {
        format!(
            "{} #{} {} {:?} in {}: {}",
            meta.chain,
            r.step.index,
            &r.step.commit[..10.min(r.step.commit.len())],
            f.fault.kind,
            f.fault.name,
            tests.join(", ")
        )
    };
    let mut misses = Vec::new();
    let mut unconfirmed_misses = Vec::new();
    for r in &results {
        for f in &r.faults {
            if f.confirmed_miss {
                misses.push(describe(r, f, &f.missed));
            } else if f.miss {
                unconfirmed_misses.push(describe(r, f, &f.unrelated));
            }
        }
    }
    let graded_secs: Vec<f64> = results.iter().map(|r| r.grade_ms as f64 / 1000.0).collect();
    let report = ChainReport {
        chain: meta.chain.clone(),
        dir: work.display().to_string(),
        commits,
        chained: steps.len(),
        graded: results.len(),
        errors: steps.len() - ok.len() + results.iter().filter(|r| r.error.is_some()).count(),
        complete: steps.len() >= commits && results.len() >= commits,
        small_commits: small.len(),
        efficiency_median_small: median(small.iter().map(|s| share(s)).collect()),
        // Filled in by `report` once the merged quarantine is known.
        efficiency_median_small_quarantined: None,
        efficiency_median_all: median(ok.iter().map(|s| share(s)).collect()),
        counts,
        fallbacks: fallback_counts(ok.iter().copied()),
        lander_secs_mean: mean(&lander),
        lander_secs_median: median(lander.clone()),
        lander_secs_p90: percentile(lander.clone(), 0.9),
        lander_secs_total: lander.iter().sum(),
        initial_secs,
        grade_secs_mean: mean(&graded_secs),
        misses,
        unconfirmed_misses,
        disk: read_json(&work.join("chain/disk.json")),
    };
    (report, steps, results)
}

/// The report over the work dirs of one or more chains (`--merge`, or a
/// single run's own dir), with the sample results found in any of them.
pub(crate) fn report(dirs: &[PathBuf], quarantine: &BTreeSet<String>) -> FreshReport {
    let mut chains = Vec::new();
    let mut steps = Vec::new();
    let mut chain_steps: Vec<(PathBuf, Vec<ChainStep>)> = Vec::new();
    let mut results = Vec::new();
    let mut sample: Vec<SampleResult> = Vec::new();
    for dir in dirs {
        if dir.join("chain").is_dir() {
            let (c, s, r) = chain_report(dir);
            chain_steps.push((dir.clone(), s.clone()));
            steps.extend(s);
            results.extend(r.into_iter().map(|r| (c.chain.clone(), r)));
            chains.push(c);
        }
        if let Ok(entries) = fs::read_dir(dir.join("sample")) {
            for e in entries.flatten() {
                if let Some(r) = read_json::<SampleResult>(&e.path()) {
                    sample.push(r);
                }
            }
        }
    }
    sample.sort_by(|a, b| a.commit.cmp(&b.commit));
    sample.dedup_by(|a, b| a.commit == b.commit);
    let mut quarantine: BTreeSet<String> = quarantine.clone();
    quarantine.extend(sample.iter().flat_map(|s| s.failed.iter().cloned()));
    // Re-grade each chain's small commits with the merged quarantine.
    let mut small_quarantined = Vec::new();
    for (chain, (dir, chain_steps)) in chains.iter_mut().zip(&chain_steps) {
        let in_suite = suite_names(dir, chain_steps)
            .intersection(&quarantine)
            .count();
        let shares: Vec<f64> = chain_steps
            .iter()
            .filter(|s| s.error.is_none() && s.write_set <= 5)
            .map(|s| share_quarantined(s, &quarantine, in_suite))
            .collect();
        chain.efficiency_median_small_quarantined = median(shares.clone());
        small_quarantined.extend(shares);
    }
    let efficiency_median_small_quarantined = median(small_quarantined);
    let ok: Vec<&ChainStep> = steps.iter().filter(|s| s.error.is_none()).collect();
    let small: Vec<&&ChainStep> = ok.iter().filter(|s| s.write_set <= 5).collect();
    let mut counts = Counts::default();
    for (_, r) in &results {
        counts.add(r);
    }
    let misses: Vec<String> = chains
        .iter()
        .flat_map(|c| c.misses.iter().cloned())
        .collect();
    let unconfirmed_misses: Vec<String> = chains
        .iter()
        .flat_map(|c| c.unconfirmed_misses.iter().cloned())
        .collect();
    let mut unrelated_failures: BTreeMap<String, usize> = BTreeMap::new();
    for f in results.iter().flat_map(|(_, r)| &r.faults) {
        for t in &f.unrelated {
            *unrelated_failures.entry(t.clone()).or_default() += 1;
        }
    }
    let efficiency_median_small = median(small.iter().map(|s| share(s)).collect());
    let lander: Vec<f64> = steps.iter().map(|s| s.chain_ms as f64 / 1000.0).collect();
    FreshReport {
        commits: chains.iter().map(|c| c.commits).sum(),
        chained: steps.len(),
        graded: results.len(),
        errors: chains.iter().map(|c| c.errors).sum(),
        complete: !chains.is_empty() && chains.iter().all(|c| c.complete),
        small_commits: small.len(),
        efficiency_median_small,
        efficiency_median_small_quarantined,
        efficiency_median_all: median(ok.iter().map(|s| share(s)).collect()),
        fallbacks: fallback_counts(ok.iter().copied()),
        lander_secs_mean: mean(&lander),
        safety_gate: counts.fault_confirmed_misses == 0,
        efficiency_gate: efficiency_median_small_quarantined.is_some_and(|m| m <= 0.20),
        counts,
        misses,
        unconfirmed_misses,
        unrelated_failures,
        quarantine: quarantine.into_iter().collect(),
        sample,
        profraw_in_cwd: Vec::new(),
        chains,
        results,
    }
}

/// Print the report, and return its Markdown summary.
pub(crate) fn print(r: &FreshReport) -> String {
    let c = &r.counts;
    let mut md = String::new();
    md.push_str(&format!(
        "# M4 selection gate (ADR 0023): {} chain(s), {} of {} commits chained{}\n\n",
        r.chains.len(),
        r.chained,
        r.commits,
        if r.complete { "" } else { " (incomplete)" }
    ));
    let rows = [
        (
            "Verdict",
            format!(
                "safety {} (zero confirmed misses), efficiency {} (median <= 20% for write sets <= 5){}",
                if r.safety_gate { "PASS" } else { "FAIL" },
                if r.efficiency_gate { "PASS" } else { "FAIL" },
                if r.complete {
                    ""
                } else {
                    "; not gated: incomplete"
                }
            ),
        ),
        (
            "Efficiency, median share (write set <= 5), merged quarantine out (gated)",
            format!(
                "{} (n={}, {} quarantined)",
                pct(r.efficiency_median_small_quarantined),
                r.small_commits,
                r.quarantine.len()
            ),
        ),
        (
            "Efficiency, median share (write set <= 5), as selected",
            pct(r.efficiency_median_small),
        ),
        (
            "Efficiency, median share (all)",
            pct(r.efficiency_median_all),
        ),
        (
            "Faults: graded / informative / (b) detected / (a) detected",
            format!(
                "{} / {} / {} / {}",
                c.faults, c.informative_faults, c.faults_detected_b, c.faults_detected_a
            ),
        ),
        (
            "Misses per fault: confirmed (gated) / unconfirmed (review)",
            format!(
                "{} / {}",
                c.fault_confirmed_misses,
                c.fault_misses - c.fault_confirmed_misses
            ),
        ),
        (
            "Commits: with faults / informative / with a miss",
            format!(
                "{} / {} / {}",
                c.commits_with_faults, c.informative_commits, c.commits_with_miss
            ),
        ),
        ("Confirmed misses", format!("{:?}", r.misses)),
        (
            "Lander wall time per commit (mean)",
            format!("{:.0} s", r.lander_secs_mean),
        ),
        ("Quarantine", format!("{:?}", r.quarantine)),
        (
            "profraw in the working directory",
            format!("{:?}", r.profraw_in_cwd),
        ),
    ];
    md.push_str("| Measure | Value |\n|---|---|\n");
    for (k, v) in rows {
        println!("  {k}: {v}");
        md.push_str(&format!("| {k} | {v} |\n"));
    }
    println!("  fallbacks (commits): {:?}", r.fallbacks);
    md.push_str("\n## Chains\n\n| Chain | Commits | Median share (ws <= 5; quarantine out, as selected) | Faults (informative) | Misses | Lander s/commit (mean, p90) | Initial run | Grading s/commit | Peak disk used (least free) |\n|---|---|---|---|---|---|---|---|---|\n");
    for ch in &r.chains {
        let line = format!(
            "| {} | {}/{}{} | {}, {} | {} ({}) | {} | {:.0}, {} | {:.1} min | {:.0} | {} |",
            if ch.chain.is_empty() { "-" } else { &ch.chain },
            ch.chained,
            ch.commits,
            if ch.complete { "" } else { " (incomplete)" },
            pct(ch.efficiency_median_small_quarantined),
            pct(ch.efficiency_median_small),
            ch.counts.faults,
            ch.counts.informative_faults,
            ch.counts.fault_confirmed_misses,
            ch.lander_secs_mean,
            ch.lander_secs_p90
                .map_or("n/a".into(), |v| format!("{v:.0}")),
            ch.initial_secs / 60.0,
            ch.grade_secs_mean,
            ch.disk.as_ref().map_or("n/a".into(), peak_disk),
        );
        println!("  chain {line}");
        md.push_str(&line);
        md.push('\n');
    }
    if !r.unconfirmed_misses.is_empty() {
        md.push_str(
            "\n## Unconfirmed misses (review; the listed tests also fail without the fault)\n\n",
        );
        for m in &r.unconfirmed_misses {
            md.push_str(&format!("- {m}\n"));
        }
    }
    if !r.unrelated_failures.is_empty() {
        md.push_str("\n## (a) failures unrelated to the fault\n\n| Test | Faults |\n|---|---|\n");
        for (t, n) in &r.unrelated_failures {
            md.push_str(&format!("| `{t}` | {n} |\n"));
        }
        // One pair of outputs per test: why it fails either way.
        let mut shown = BTreeSet::new();
        for o in r
            .results
            .iter()
            .flat_map(|(_, res)| &res.faults)
            .flat_map(|f| &f.unrelated_output)
        {
            if shown.insert(o.test.clone()) {
                md.push_str(&format!(
                    "\n<details><summary>`{}`</summary>\n\nWith the fault:\n\n```text\n{}\n```\n\nWithout the fault:\n\n```text\n{}\n```\n\n</details>\n",
                    o.test, o.with_fault, o.without_fault
                ));
            }
        }
    }
    if !r.sample.is_empty() {
        md.push_str("\n## Full-suite sample (literal `cargo test`, not gated)\n\n| Commit | Failed | Minutes |\n|---|---|---|\n");
        for s in &r.sample {
            md.push_str(&format!(
                "| {} | {}{} | {:.0} |\n",
                &s.commit[..10.min(s.commit.len())],
                s.failed.len(),
                if s.timed_out { " (timed out)" } else { "" },
                s.elapsed_ms as f64 / 60_000.0
            ));
        }
        // One commit's failure excerpts: enough to diagnose an environment.
        if let Some(s) = r.sample.iter().find(|s| !s.failure_output.is_empty()) {
            md.push_str(&format!(
                "\n<details><summary>Failure output, {}</summary>\n\n",
                &s.commit[..10.min(s.commit.len())]
            ));
            for (test, output) in &s.failure_output {
                md.push_str(&format!("`{test}`\n\n```text\n{output}\n```\n\n"));
            }
            md.push_str("</details>\n");
        }
    }
    md.push_str("\n## Fallback kinds (commits fired)\n\n| Kind | Commits |\n|---|---|\n");
    for (k, n) in &r.fallbacks {
        md.push_str(&format!("| {k} | {n} |\n"));
    }
    md.push_str("\n## Commits\n\n| Chain | # | Commit | ws | Selected | Fallbacks | Drift + | Lander s | Faults |\n|---|---|---|---|---|---|---|---|---|\n");
    for (chain, x) in &r.results {
        let s = &x.step;
        let faults: Vec<String> = x
            .faults
            .iter()
            .map(|f| {
                let d = if f.miss {
                    "MISS"
                } else if f.probe_only {
                    "probe"
                } else if f.detected == Some(true) {
                    "caught"
                } else {
                    "not caught by (a)"
                };
                format!("{:?} `{}`: {d}", f.fault.kind, f.fault.name)
            })
            .collect();
        md.push_str(&format!(
            "| {} | {} | {} | {} | {}/{} | {} | {} | {:.0} | {} |\n",
            if chain.is_empty() { "-" } else { chain },
            s.index,
            &s.commit[..10.min(s.commit.len())],
            s.write_set,
            s.selected,
            s.suite,
            s.fallbacks.join(", "),
            s.drift_selected,
            s.chain_ms as f64 / 1000.0,
            if faults.is_empty() {
                x.no_fault
                    .clone()
                    .or_else(|| x.error.clone())
                    .unwrap_or_default()
            } else {
                faults.join("; ")
            },
        ));
    }
    md
}

/// A chain's peak disk use: `/ 61.2G of 72.0G (10.8G) at lander 3`, per
/// filesystem, and the one holding the work directory.
fn peak_disk(log: &disk::DiskLog) -> String {
    let peaks: Vec<String> = log
        .peaks
        .iter()
        .map(|p| {
            format!(
                "`{}` {} of {} ({}) at {}",
                p.mount,
                disk::gb(p.peak_used),
                disk::gb(p.size),
                disk::gb(p.min_free),
                p.at
            )
        })
        .collect();
    format!("{}; work on `{}`", peaks.join("; "), log.work_mount)
}

/// The chain's instrumented target directory for `slot` (0 or 1). The
/// initial full run builds in slot 0, so the chain's first build there is
/// incremental and no third instrumented target exists.
pub(crate) fn coverage_target(work: &Path, slot: usize) -> PathBuf {
    work.join(format!("coverage-target-{slot}"))
}

/// Where the fresh run keeps its initial record.
pub(crate) fn initial_path(work: &Path) -> PathBuf {
    work.join("chain/initial.cbor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_cover_distinct_functions_before_second_kinds() {
        use FaultKind::{Default as D, Flip as F, Panic as P};
        let order = fault_order(vec![(2, P), (2, D), (2, F), (0, F), (0, P), (1, D)]);
        assert_eq!(order, vec![(2, P), (0, F), (1, D), (2, D), (0, P), (2, F)]);
        assert!(fault_order(Vec::new()).is_empty());
    }

    #[test]
    fn quarantined_tests_leave_both_the_selection_and_the_suite() {
        let test = |name: &str| {
            Unit::Test(hord_verify::TestRef {
                package: "cargo".into(),
                target: hord_verify::TestTarget {
                    kind: "test".into(),
                    name: "testsuite".into(),
                },
                name: name.into(),
            })
        };
        let step = ChainStep {
            selected: 4,
            suite: 10,
            units: vec![test("a"), test("q1"), test("q2"), test("b")],
            ..ChainStep::default()
        };
        let quarantine: BTreeSet<String> = ["q1", "q2", "q3"].map(String::from).into();
        // Three quarantined tests in the suite, two of them selected.
        assert!((share_quarantined(&step, &quarantine, 3) - 2.0 / 7.0).abs() < 1e-9);
        assert!((share_quarantined(&step, &BTreeSet::new(), 0) - 0.4).abs() < 1e-9);
        // Without chain/suite.json, the suite is every name the steps mention.
        let dir = std::env::temp_dir().join(format!("hord-m4-suite-{}", std::process::id()));
        let names = suite_names(&dir, std::slice::from_ref(&step));
        assert_eq!(names.len(), 4);
    }

    fn unit(name: &str) -> Unit {
        Unit::Test(hord_verify::TestRef {
            package: "cargo".into(),
            target: hord_verify::TestTarget {
                kind: "test".into(),
                name: "testsuite".into(),
            },
            name: name.into(),
        })
    }

    fn units_of(names: &[&str]) -> BTreeSet<Unit> {
        names.iter().map(|n| unit(n)).collect()
    }

    #[test]
    fn failures_without_the_fault_detect_nothing() {
        let runner = CargoRunner::default();
        let root = Path::new("/w");
        // (b) = {x}; (a) = {x, env, y}. Everything in (a) ran.
        let a_set = units_of(&["x", "env", "y"]);
        let mut grader = Grader::new(&runner, root);
        grader.ran = a_set.clone();

        // Gate run 36189604585: (a) failed only on a test that also fails
        // without the fault. Not a detection, and not a confirmed miss.
        grader.failed = units_of(&["env"]);
        let env = units_of(&["env"]);
        let c = classify(&a_set, &grader, &env, false, &env, &BTreeSet::new());
        assert_eq!(
            c,
            Classified {
                detected_a: Some(false),
                miss: true,
                confirmed_miss: false,
            }
        );

        // A real miss: y fails with the fault and passes without it.
        grader.failed = units_of(&["env", "y"]);
        let missed = units_of(&["env", "y"]);
        let c = classify(&a_set, &grader, &env, false, &missed, &units_of(&["y"]));
        assert_eq!(
            (c.detected_a, c.miss, c.confirmed_miss),
            (Some(true), true, true)
        );

        // (b) detected it: no miss, whatever (a) did.
        grader.failed = units_of(&["x"]);
        let c = classify(
            &a_set,
            &grader,
            &BTreeSet::new(),
            true,
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(
            (c.detected_a, c.miss, c.confirmed_miss),
            (Some(true), false, false)
        );

        // (a) did not finish and nothing related failed: undetermined.
        grader.ran = units_of(&["x"]);
        grader.failed = BTreeSet::new();
        let c = classify(
            &a_set,
            &grader,
            &BTreeSet::new(),
            false,
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(c.detected_a, None);
    }

    #[test]
    fn unrelated_failures_keep_their_output_with_and_without_the_fault() {
        let with: BTreeMap<String, String> = [
            (
                "registry::readonly".to_owned(),
                "denied (faulted)".to_owned(),
            ),
            ("x".to_owned(), "the fault".to_owned()),
        ]
        .into();
        let without: BTreeMap<String, String> =
            [("registry::readonly".to_owned(), "denied (clean)".to_owned())].into();
        let mut unrelated = units_of(&["registry::readonly"]);
        unrelated.insert(Unit::Doc("cargo".into()));
        let out = unrelated_output(&unrelated, &with, &without);
        assert_eq!(
            out,
            vec![UnrelatedOutput {
                test: "cargo/testsuite:registry::readonly".into(),
                with_fault: "denied (faulted)".into(),
                without_fault: "denied (clean)".into(),
            }],
            "doctest sets have no per-test output"
        );
    }

    #[test]
    fn the_safety_gate_counts_confirmed_misses_and_lists_the_rest() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("hord-m4-confirmed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let step = ChainStep {
            index: 0,
            commit: "a07c49a989d565727725e5bb5a8038ff402006a8".into(),
            write_set: 1,
            selected: 1,
            suite: 100,
            ..ChainStep::default()
        };
        write_json(
            &dir.join("chain/meta.json"),
            &ChainMeta {
                chain: "2/15".into(),
                commits: vec![step.commit.clone()],
                ..ChainMeta::default()
            },
        )?;
        write_json(&dir.join("chain/0.json"), &step)?;
        let fault = |name: &str, confirmed: bool| FaultGrade {
            fault: Fault {
                path: "src/lib.rs".into(),
                name: name.into(),
                kind: FaultKind::Panic,
                rejected: 0,
                edited_lines: 0,
            },
            probe_only: false,
            detected: Some(false),
            detected_a: Some(confirmed),
            miss: true,
            confirmed_miss: confirmed,
            missed: if confirmed {
                vec!["cargo/testsuite:y".into()]
            } else {
                Vec::new()
            },
            unrelated: vec!["cargo/testsuite:registry::readonly".into()],
            unrelated_output: vec![UnrelatedOutput {
                test: "cargo/testsuite:registry::readonly".into(),
                with_fault: "Permission denied".into(),
                without_fault: "Permission denied (clean)".into(),
            }],
            failed: Vec::new(),
            unattributed: false,
            build_ms: 0,
            grade_ms: 0,
        };
        let result = |faults| FreshResult {
            step: step.clone(),
            faults,
            ..FreshResult::default()
        };
        write_json(
            &dir.join("fresh-results/0.json"),
            &result(vec![fault("f", false)]),
        )?;
        let r = report(std::slice::from_ref(&dir), &BTreeSet::new());
        assert!(r.safety_gate, "an unconfirmed miss is not gated");
        assert_eq!(r.misses.len(), 0);
        assert_eq!(r.unconfirmed_misses.len(), 1);
        assert!(
            r.unconfirmed_misses[0].contains("registry::readonly"),
            "{:?}",
            r.unconfirmed_misses
        );
        assert_eq!(
            r.unrelated_failures
                .get("cargo/testsuite:registry::readonly"),
            Some(&1)
        );
        let md = print(&r);
        assert!(md.contains("(a) failures unrelated to the fault"));
        assert!(md.contains("Permission denied (clean)"), "{md}");

        write_json(
            &dir.join("fresh-results/0.json"),
            &result(vec![fault("f", false), fault("g", true)]),
        )?;
        let r = report(std::slice::from_ref(&dir), &BTreeSet::new());
        assert!(!r.safety_gate, "a confirmed miss fails the gate");
        assert_eq!(r.misses.len(), 1);
        assert!(r.misses[0].contains("cargo/testsuite:y"), "{:?}", r.misses);
        assert_eq!(r.counts.commits_with_miss, 1);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn counts_are_per_fault_and_per_commit() {
        let grade = |probe_only: bool, detected: Option<bool>, miss: bool| FaultGrade {
            fault: Fault {
                path: "src/lib.rs".into(),
                name: "f".into(),
                kind: FaultKind::Panic,
                rejected: 0,
                edited_lines: 0,
            },
            probe_only,
            detected,
            detected_a: detected.or(Some(miss)),
            miss,
            confirmed_miss: miss,
            missed: Vec::new(),
            unrelated: Vec::new(),
            unrelated_output: Vec::new(),
            failed: Vec::new(),
            unattributed: false,
            build_ms: 0,
            grade_ms: 0,
        };
        let mut c = Counts::default();
        c.add(&FreshResult {
            covers_a: false,
            faults: vec![
                grade(false, Some(true), false),
                grade(false, Some(false), true),
            ],
            ..FreshResult::default()
        });
        c.add(&FreshResult {
            covers_a: true,
            faults: vec![grade(true, None, false)],
            ..FreshResult::default()
        });
        c.add(&FreshResult::default());
        assert_eq!((c.faults, c.informative_faults, c.fault_misses), (3, 2, 1));
        assert_eq!(
            (
                c.commits_with_faults,
                c.informative_commits,
                c.commits_with_miss
            ),
            (2, 1, 1)
        );
        assert_eq!(c.faults_detected_b, 1);
    }
}
