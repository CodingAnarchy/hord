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
use hord_verify_rust::coverage::{BuiltSuite, build_suite_with_env, run_suite};
use hord_verify_rust::{
    CargoRunner, CargoWorkspace, CoverageOptions, DefinitionIndex, Fallback, SelectInput,
    Selection, TestFilter, select,
};
use serde::{Deserialize, Serialize};

use crate::fault::{FaultKind, attempts, inject};
use crate::prepare::{CommitFacts, Prepared};
use crate::run::{
    Ctx, Fault, Grader, Unit, Worker, cargo, corpus_env, count, elapsed, runner_env, units,
};

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
    pub failed: Vec<String>,
    /// A command hung or crashed; its units were counted as failing.
    pub unattributed: bool,
    pub build_ms: u64,
    pub grade_ms: u64,
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
        cancel: Default::default(),
    };
    std::thread::scope(|scope| -> Result<()> {
        let spawn_build = |i: usize| {
            let (worker, target) = (&workers[i % 2], &targets[i % 2]);
            let facts = &commits[i];
            let options = build_options(target);
            scope.spawn(move || -> Result<Option<BuiltSuite>> {
                worker.checkout.checkout(&facts.commit)?;
                Ok(build_suite_with_env(
                    &VerifyCheckout {
                        root: worker.checkout.root.clone(),
                        snapshot: facts.result_snapshot,
                    },
                    &options,
                    &corpus_env(),
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
    let mut grader = Grader {
        runner,
        root,
        ran: BTreeSet::new(),
        failed: BTreeSet::new(),
        unattributed: false,
        timed_out: false,
    };
    let mut out = FaultGrade {
        fault,
        probe_only: false,
        detected: None,
        detected_a: None,
        miss: false,
        confirmed_miss: false,
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
    // (b): the selection, the tests that ran the faulted function first.
    let mut batch: Vec<Unit> = set.iter().cloned().collect();
    batch.sort_by_key(|u| !probe.contains(u));
    grader.run(batch, &|failed| !set.is_disjoint(failed))?;
    let detected = !set.is_disjoint(&grader.failed);
    if !detected {
        let mut batch: Vec<Unit> = a_set.difference(&grader.ran).cloned().collect();
        batch.sort_by_key(|u| !probe.contains(u));
        grader.run(batch, &|failed| !a_set.is_disjoint(failed))?;
    }
    out.detected = Some(detected);
    out.detected_a = if !a_set.is_disjoint(&grader.failed) {
        Some(true)
    } else if a_set.is_subset(&grader.ran) {
        Some(false)
    } else {
        None
    };
    out.miss = out.detected_a == Some(true) && !detected;
    out.failed = grader.failed.iter().map(Unit::label).collect();
    out.unattributed = grader.unattributed;
    if out.miss {
        // The missed failures must pass without the fault.
        worker.checkout.restore(fault_path)?;
        let missed: Vec<Unit> = grader
            .failed
            .iter()
            .filter(|u| a_set.contains(u) && !set.contains(u))
            .cloned()
            .collect();
        let mut clean = Grader {
            runner,
            root,
            ran: BTreeSet::new(),
            failed: BTreeSet::new(),
            unattributed: false,
            timed_out: false,
        };
        clean.run(missed.clone(), &|_| false)?;
        out.confirmed_miss = !missed.is_empty() && clean.failed.is_empty() && !grader.unattributed;
    }
    Ok(out)
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
    pub misses: Vec<String>,
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
        if r.faults.iter().any(|f| f.miss) {
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
    pub efficiency_median_small: Option<f64>,
    pub efficiency_median_all: Option<f64>,
    pub counts: Counts,
    pub fallbacks: BTreeMap<String, usize>,
    pub lander_secs_mean: f64,
    pub misses: Vec<String>,
    pub quarantine: Vec<String>,
    pub sample: Vec<crate::SampleResult>,
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
    let misses = results
        .iter()
        .filter(|r| r.faults.iter().any(|f| f.miss))
        .map(|r| {
            format!(
                "{} {} {}",
                meta.chain,
                r.step.index,
                &r.step.commit[..10.min(r.step.commit.len())]
            )
        })
        .collect();
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
    };
    (report, steps, results)
}

/// The report over the work dirs of one or more chains (`--merge`, or a
/// single run's own dir), with the sample results found in any of them.
pub(crate) fn report(dirs: &[PathBuf], quarantine: &BTreeSet<String>) -> FreshReport {
    let mut chains = Vec::new();
    let mut steps = Vec::new();
    let mut results = Vec::new();
    let mut sample: Vec<crate::SampleResult> = Vec::new();
    for dir in dirs {
        if dir.join("chain").is_dir() {
            let (c, s, r) = chain_report(dir);
            steps.extend(s);
            results.extend(r.into_iter().map(|r| (c.chain.clone(), r)));
            chains.push(c);
        }
        if let Ok(entries) = fs::read_dir(dir.join("sample")) {
            for e in entries.flatten() {
                if let Some(r) = read_json::<crate::SampleResult>(&e.path()) {
                    sample.push(r);
                }
            }
        }
    }
    sample.sort_by(|a, b| a.commit.cmp(&b.commit));
    sample.dedup_by(|a, b| a.commit == b.commit);
    let mut quarantine: BTreeSet<String> = quarantine.clone();
    quarantine.extend(sample.iter().flat_map(|s| s.failed.iter().cloned()));
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
        efficiency_median_all: median(ok.iter().map(|s| share(s)).collect()),
        fallbacks: fallback_counts(ok.iter().copied()),
        lander_secs_mean: mean(&lander),
        safety_gate: counts.fault_misses == 0,
        efficiency_gate: efficiency_median_small.is_some_and(|m| m <= 0.20),
        counts,
        misses,
        quarantine: quarantine.into_iter().collect(),
        sample,
        profraw_in_cwd: Vec::new(),
        chains,
        results,
    }
}

fn pct(m: Option<f64>) -> String {
    m.map_or("n/a".to_owned(), |m| format!("{:.1}%", m * 100.0))
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
                "safety {} (zero misses), efficiency {} (median <= 20% for write sets <= 5){}",
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
            "Efficiency, median share (write set <= 5)",
            format!("{} (n={})", pct(r.efficiency_median_small), r.small_commits),
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
            "Misses per fault (confirmed)",
            format!("{} ({})", c.fault_misses, c.fault_confirmed_misses),
        ),
        (
            "Commits: with faults / informative / with a miss",
            format!(
                "{} / {} / {}",
                c.commits_with_faults, c.informative_commits, c.commits_with_miss
            ),
        ),
        ("Misses", format!("{:?}", r.misses)),
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
    md.push_str("\n## Chains\n\n| Chain | Commits | Median share (ws <= 5) | Faults (informative) | Misses | Lander s/commit (mean, p90) | Initial run | Grading s/commit |\n|---|---|---|---|---|---|---|---|\n");
    for ch in &r.chains {
        let line = format!(
            "| {} | {}/{}{} | {} | {} ({}) | {} | {:.0}, {} | {:.1} min | {:.0} |",
            if ch.chain.is_empty() { "-" } else { &ch.chain },
            ch.chained,
            ch.commits,
            if ch.complete { "" } else { " (incomplete)" },
            pct(ch.efficiency_median_small),
            ch.counts.faults,
            ch.counts.informative_faults,
            ch.counts.fault_misses,
            ch.lander_secs_mean,
            ch.lander_secs_p90
                .map_or("n/a".into(), |v| format!("{v:.0}")),
            ch.initial_secs / 60.0,
            ch.grade_secs_mean,
        );
        println!("  chain {line}");
        md.push_str(&line);
        md.push('\n');
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
