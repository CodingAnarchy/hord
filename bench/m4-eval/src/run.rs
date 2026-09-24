//! Phase 3: one commit. Select tests for the unmutated commit (efficiency),
//! inject a fault that type-checks, run (b) the selection, and (a) every
//! test of the affected packages when (b) did not detect the fault.
//!
//! (b) is a subset of (a), so a fault (b) detects is detected by (a) too
//! and (a) is not run. A fault (a) detects and (b) does not is a miss; it
//! is confirmed by re-running the failing tests with and without the fault.
//!
//! To keep a commit cheap without changing what is measured: a probe runs
//! first (the tests whose coverage covers the faulted function, all in
//! (b)), and a failure there ends the commit; when (b) runs every test of
//! (a), no miss is possible and neither suite runs; and suites stop at the
//! first failing test binary, since only "did anything fail" matters.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hord_core::{EvidenceKind, RepoPath};
use hord_verify::{Check, CoverageRecord, TestRef, VerifyPolicy};
use hord_verify_rust::{
    CargoRunner, CargoWorkspace, Fallback, RunOutput, RustVerifier, SelectInput, Selection,
    select_ignoring,
};
use serde::{Deserialize, Serialize};

use crate::fault::{FaultKind, attempts, inject};
use crate::git::Checkout;
use crate::prepare::CommitFacts;

/// Where one worker builds and runs.
pub(crate) struct Worker {
    pub checkout: Checkout,
    pub target_dir: PathBuf,
}

/// Shared, read-only inputs of phase 3.
pub(crate) struct Ctx {
    pub toolchain: hord_verify::Toolchain,
    pub seed: u64,
    pub quarantine: BTreeSet<TestRef>,
    pub timeout: Duration,
    /// Kill a command that prints nothing this long (a hang, which the
    /// fault can cause; it counts as the run failing).
    pub idle: Duration,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SelectionSummary {
    pub full: bool,
    pub packages: Vec<String>,
    pub exact: usize,
    pub filters: usize,
    pub doc: Vec<String>,
    pub fallbacks: Vec<String>,
    /// Tests of the record the selection runs.
    pub selected: usize,
    /// Tests in the record (the suite).
    pub suite: usize,
    /// Per fallback kind that fired: tests selected with that kind turned
    /// off (its cost is `selected` minus this).
    #[serde(default)]
    pub without: BTreeMap<String, usize>,
    /// Tests selected with every fallback turned off (coverage and static
    /// edges only).
    #[serde(default)]
    pub without_any: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Fault {
    pub path: String,
    pub name: String,
    pub kind: FaultKind,
    /// Attempts that did not type-check before this one.
    pub rejected: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CommitResult {
    pub index: usize,
    pub commit: String,
    pub write_set: usize,
    pub selection: SelectionSummary,
    pub affected: Vec<String>,
    pub fault: Option<Fault>,
    /// Why no fault was injected.
    pub no_fault: Option<String>,
    pub detected_b: bool,
    /// The probe (the tests whose coverage covers the faulted function,
    /// all in (b)) detected it; (b) and (a) were not run further.
    #[serde(default)]
    pub probe_detected: bool,
    /// (b) runs every test of (a) (a full or package-level selection of
    /// every affected package), so a miss is impossible; neither was run
    /// past the probe, and whether (a) detects the fault is unknown.
    #[serde(default)]
    pub b_covers_a: bool,
    /// `None` when (a) was not run because (b) detected the fault.
    pub detected_a: Option<bool>,
    pub failed_b: Vec<String>,
    pub failed_a: Vec<String>,
    pub timed_out: bool,
    pub miss: bool,
    /// The miss reproduced: the (a) failures fail with the fault and pass
    /// without it.
    pub confirmed_miss: bool,
    pub build_ms: u64,
    pub b_ms: u64,
    pub a_ms: u64,
    pub total_ms: u64,
    pub error: Option<String>,
}

/// Tests of `record` that `selection` runs, and the suite size, both
/// without quarantined tests. Doctests are counted in neither.
pub(crate) fn count(
    selection: &Selection,
    record: &CoverageRecord,
    quarantine: &BTreeSet<TestRef>,
) -> (usize, usize) {
    let suite: Vec<&TestRef> = record
        .tests
        .iter()
        .map(|t| &t.test)
        .filter(|t| !quarantine.contains(*t))
        .collect();
    if selection.full {
        return (suite.len(), suite.len());
    }
    let mut selected = 0;
    let mut matched_filters: BTreeSet<(&String, &String)> = BTreeSet::new();
    for t in &suite {
        let exact = selection
            .exact
            .get(&(t.package.clone(), t.target.clone()))
            .is_some_and(|names| names.contains(&t.name));
        let filtered = selection
            .filters
            .get(&t.package)
            .and_then(|fs| fs.iter().find(|f| t.name.contains(f.as_str())));
        if let Some(f) = filtered {
            matched_filters.insert((&t.package, f));
        }
        if selection.packages.contains(&t.package) || exact || filtered.is_some() {
            selected += 1;
        }
    }
    // A filter for a test the record does not know yet selects that test.
    for (pkg, filters) in &selection.filters {
        selected += filters
            .iter()
            .filter(|f| !matched_filters.contains(&(pkg, *f)))
            .count();
    }
    (selected, suite.len())
}

fn cargo(args: Vec<String>) -> Check {
    Check {
        requirement: "m4-eval".into(),
        kind: EvidenceKind::Test,
        qualifier: None,
        program: "cargo".into(),
        args,
        env: BTreeMap::new(),
        dir: RepoPath::default(),
        scope: None,
    }
}

/// Failing tests of `out`, without quarantined names.
fn failures(out: &RunOutput, quarantine: &BTreeSet<String>) -> Vec<String> {
    out.report
        .failed
        .iter()
        .filter(|n| !quarantine.contains(*n))
        .cloned()
        .collect()
}

/// A run detects the fault when a test fails, or it times out or crashes
/// after building (a hang or abort is observable too).
fn detects(out: &RunOutput, quarantine: &BTreeSet<String>) -> bool {
    !failures(out, quarantine).is_empty() || (!out.success() && !out.build_failed)
}

pub(crate) fn evaluate(
    ctx: &Ctx,
    worker: &Worker,
    facts: &CommitFacts,
    record: Arc<CoverageRecord>,
) -> Result<CommitResult> {
    let started = Instant::now();
    let root = &worker.checkout.root;
    worker.checkout.checkout(&facts.commit)?;
    let workspace = CargoWorkspace::load(root)?;
    let mut verifier = RustVerifier::new(ctx.toolchain.clone(), workspace.clone())?
        .with_coverage(Some(Arc::clone(&record)));
    verifier.quarantine = ctx.quarantine.clone();
    let runner = CargoRunner {
        target_dir: Some(worker.target_dir.clone()),
        timeout: Some(ctx.timeout),
        idle_timeout: Some(ctx.idle),
        ..CargoRunner::default()
    };
    verifier.runner = runner.clone();
    let quarantined: BTreeSet<String> = ctx.quarantine.iter().map(|t| t.name.clone()).collect();

    let policy = VerifyPolicy::default().requiring(["test:selected"]);
    let selection = verifier.selection(&facts.impact, &policy);
    let (selected, suite) = count(&selection, &record, &ctx.quarantine);
    // What each fallback kind costs (ADR 0022 amendments: measured, not
    // tuned): the same selection with that kind turned off.
    let input = SelectInput {
        coverage: Some(&record),
        toolchain: ctx.toolchain.id()?,
        workspace: &workspace,
        impact: &facts.impact,
        max_impact: policy.max_impact,
    };
    let kinds: BTreeSet<&str> = selection.fallbacks.iter().map(Fallback::kind).collect();
    let mut without = BTreeMap::new();
    for kind in &kinds {
        let alt = select_ignoring(input, &[*kind].into_iter().collect());
        without.insert((*kind).to_owned(), count(&alt, &record, &ctx.quarantine).0);
    }
    let without_any = if kinds.is_empty() {
        selected
    } else {
        count(&select_ignoring(input, &kinds), &record, &ctx.quarantine).0
    };
    let mut result = CommitResult {
        index: facts.index,
        commit: facts.commit.clone(),
        write_set: facts.write_set.len(),
        selection: SelectionSummary {
            full: selection.full,
            packages: selection.packages.iter().cloned().collect(),
            exact: selection.exact.values().map(BTreeSet::len).sum(),
            filters: selection.filters.values().map(BTreeSet::len).sum(),
            doc: selection.doc.iter().cloned().collect(),
            fallbacks: selection
                .fallbacks
                .iter()
                .map(ToString::to_string)
                .collect(),
            selected,
            suite,
            without,
            without_any,
        },
        ..CommitResult::default()
    };

    // (a)'s packages: those the commit changed, and their reverse deps.
    let touched: BTreeSet<String> = facts
        .changed_paths
        .iter()
        .filter_map(|p| p.parse::<RepoPath>().ok())
        .filter_map(|p| workspace.package_of(&p).map(|p| p.name.clone()))
        .collect();
    let affected = workspace.with_reverse_deps(&touched);
    result.affected = affected.iter().cloned().collect();
    let scope: Vec<String> = affected
        .iter()
        .flat_map(|p| ["-p".to_owned(), p.clone()])
        .collect();

    // Inject the first fault that type-checks.
    let mut injected: Option<(String, hord_core::NodeId)> = None;
    let mut rejected = 0;
    for (target, kind) in attempts(ctx.seed, &facts.commit, &facts.targets) {
        let t = &facts.targets[target];
        let file = root.join(&t.path);
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some(def_text) = text.get(t.span.clone()) else {
            continue;
        };
        let Some(faulty) = inject(kind, def_text) else {
            continue;
        };
        let new_text = format!("{}{faulty}{}", &text[..t.span.start], &text[t.span.end..]);
        std::fs::write(&file, new_text)?;
        let build_start = Instant::now();
        let mut args = vec!["test".to_owned(), "--no-run".to_owned()];
        args.extend(scope.iter().cloned());
        let built = runner.run(root, &cargo(args))?;
        result.build_ms += elapsed(build_start);
        if built.success() {
            result.fault = Some(Fault {
                path: t.path.clone(),
                name: t.name.clone(),
                kind,
                rejected,
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
        result.total_ms = elapsed(started);
        return Ok(result);
    };

    // Probe: the tests whose coverage covers the faulted function. They are
    // in (b) (they cover a write-set node, or sit in a fallback package),
    // so a failure here is detection by (b), and by (a) ⊇ (b).
    let b_start = Instant::now();
    let mut probe: BTreeMap<(String, hord_verify::TestTarget), BTreeSet<String>> = BTreeMap::new();
    for t in record.tests_covering(&[fault_node].into_iter().collect()) {
        if !ctx.quarantine.contains(t) {
            probe
                .entry((t.package.clone(), t.target.clone()))
                .or_default()
                .insert(t.name.clone());
        }
    }
    let probe = Selection {
        exact: probe,
        ..Selection::default()
    };
    for mut check in verifier.test_checks(&probe, "test:selected", None) {
        check.args.retain(|a| a != "--no-fail-fast");
        let out = runner.run(root, &check)?;
        result.timed_out |= out.timed_out;
        result.failed_b.extend(failures(&out, &quarantined));
        result.probe_detected |= detects(&out, &quarantined);
    }
    if result.probe_detected {
        result.detected_b = true;
        result.b_ms = elapsed(b_start);
        worker.checkout.restore(&fault_path)?;
        result.total_ms = elapsed(started);
        return Ok(result);
    }
    if selection.full || affected.is_subset(&selection.packages) {
        result.b_covers_a = true;
        result.b_ms = elapsed(b_start);
        worker.checkout.restore(&fault_path)?;
        result.total_ms = elapsed(started);
        return Ok(result);
    }

    // (b): the selection. Stops at the first failing test binary.
    let mut b_detects = false;
    for mut check in verifier.test_checks(&selection, "test:selected", None) {
        check.args.retain(|a| a != "--no-fail-fast");
        let out = runner.run(root, &check)?;
        result.timed_out |= out.timed_out;
        result.failed_b.extend(failures(&out, &quarantined));
        b_detects |= detects(&out, &quarantined);
        if b_detects {
            break;
        }
    }
    result.b_ms = elapsed(b_start);
    result.detected_b = b_detects;

    // (a): every test of the affected packages, unless (b) already detected.
    if !b_detects {
        let a_start = Instant::now();
        let mut a_detects = false;
        for package in &affected {
            // Default fail-fast: only "did anything fail" matters.
            let mut args = vec!["test".to_owned(), "-p".to_owned(), package.clone()];
            let skips: Vec<String> = ctx
                .quarantine
                .iter()
                .filter(|t| &t.package == package)
                .flat_map(|t| ["--skip".to_owned(), t.name.clone()])
                .collect();
            if !skips.is_empty() {
                args.push("--".into());
                args.push("--exact".into());
                args.extend(skips);
            }
            let out = runner.run(root, &cargo(args))?;
            result.timed_out |= out.timed_out;
            result.failed_a.extend(failures(&out, &quarantined));
            a_detects |= detects(&out, &quarantined);
            if a_detects {
                break;
            }
        }
        result.a_ms = elapsed(a_start);
        result.detected_a = Some(a_detects);
        if a_detects {
            result.miss = true;
            result.confirmed_miss =
                confirm(&runner, worker, &fault_path, &affected, &result.failed_a)?;
        }
    }
    worker.checkout.restore(&fault_path)?;
    result.total_ms = elapsed(started);
    Ok(result)
}

/// Re-run `failing` with the fault (they must fail again) and without it
/// (they must pass): a real miss, not a flaky or environment failure.
fn confirm(
    runner: &CargoRunner,
    worker: &Worker,
    fault_path: &str,
    affected: &BTreeSet<String>,
    failing: &[String],
) -> Result<bool> {
    if failing.is_empty() {
        // Only a timeout or crash: not attributable to named tests.
        return Ok(false);
    }
    let root = &worker.checkout.root;
    let run = || -> Result<RunOutput> {
        let mut args = vec!["test".to_owned()];
        for p in affected {
            args.push("-p".into());
            args.push(p.clone());
        }
        args.extend(["--no-fail-fast".into(), "--".into(), "--exact".into()]);
        args.extend(failing.iter().cloned());
        Ok(runner.run(root, &cargo(args))?)
    };
    let with_fault = run()?;
    worker.checkout.restore(fault_path)?;
    let without = run()?;
    let none = BTreeSet::new();
    Ok(!failures(&with_fault, &none).is_empty() && without.success())
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Run the literal full suite on an unmutated commit (ADR 0023 sample) and
/// return the failing tests and the elapsed milliseconds.
pub(crate) fn full_suite(
    worker: &Worker,
    commit: &str,
    timeout: Duration,
) -> Result<(Vec<String>, u64, bool)> {
    worker.checkout.checkout(commit)?;
    let runner = CargoRunner {
        target_dir: Some(worker.target_dir.clone()),
        timeout: Some(timeout),
        ..CargoRunner::default()
    };
    let start = Instant::now();
    let out = runner
        .run(
            &worker.checkout.root,
            &cargo(vec![
                "test".into(),
                "--workspace".into(),
                "--no-fail-fast".into(),
            ]),
        )
        .context("full cargo test")?;
    Ok((
        out.report.failed.into_iter().collect(),
        elapsed(start),
        out.timed_out,
    ))
}

pub(crate) fn worker(root: &Path, corpus: &Path, id: usize) -> Result<Worker> {
    Ok(Worker {
        checkout: Checkout::open(corpus, &root.join(format!("w{id}/src")))?,
        target_dir: root.join(format!("w{id}/target")),
    })
}

#[cfg(test)]
mod tests {
    use hord_core::{NodeId, ObjectId};
    use hord_verify::TestTarget;

    use super::*;

    #[test]
    fn counts_selected_tests_against_the_suite() {
        let t = |pkg: &str, name: &str| TestRef {
            package: pkg.into(),
            target: TestTarget {
                kind: "test".into(),
                name: "s".into(),
            },
            name: name.into(),
        };
        let record = CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            ObjectId::from_bytes([2; 32]),
            BTreeSet::new(),
            vec![
                (
                    t("a", "x::one"),
                    None,
                    [NodeId::from_u128(1)].into_iter().collect(),
                    false,
                ),
                (t("a", "x::two"), None, BTreeSet::new(), false),
                (t("b", "y::three"), None, BTreeSet::new(), false),
                (t("b", "y::four"), None, BTreeSet::new(), false),
            ],
        );
        let mut sel = Selection::default();
        sel.exact
            .entry((
                "a".into(),
                TestTarget {
                    kind: "test".into(),
                    name: "s".into(),
                },
            ))
            .or_default()
            .insert("x::one".into());
        sel.filters
            .entry("a".into())
            .or_default()
            .insert("brand_new".into());
        sel.filters
            .entry("b".into())
            .or_default()
            .insert("thr".into());
        let quarantine: BTreeSet<TestRef> = [t("b", "y::four")].into_iter().collect();
        // one exact + "thr" matches y::three + one unknown new test.
        assert_eq!(count(&sel, &record, &quarantine), (3, 3));
        sel.packages.insert("b".into());
        sel.filters.clear();
        assert_eq!(count(&sel, &record, &BTreeSet::new()), (3, 4));
        assert_eq!(
            count(
                &Selection {
                    full: true,
                    ..Selection::default()
                },
                &record,
                &BTreeSet::new()
            ),
            (4, 4)
        );
    }
}
