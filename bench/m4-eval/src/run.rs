//! Phase 3: one commit, under four selection variants built from the same
//! coverage checkpoint ([`VARIANTS`]).
//!
//! Each variant's selection is expanded into units (known tests, doctest
//! sets, name filters for tests the record does not know). One seeded fault
//! that type-checks is injected, then units run incrementally, smallest
//! variant first and the tests that ran the faulted function first, until
//! every variant either has a failing unit (detected) or has run all its
//! units (not detected). A variant whose selection contains all of (a)
//! cannot miss and is only probed. (a), every test of the affected packages, runs only
//! when some variant neither detected the fault nor contains all of (a). A
//! variant misses when (a) detects and it does not; a miss is confirmed by
//! re-running the missed failures without the fault.
//!
//! Every command runs with `LLVM_PROFILE_FILE` inside the worker's
//! directory, so an instrumented binary never writes into the caller's
//! working directory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hord_core::{EvidenceKind, NodeId, RepoPath};
use hord_verify::{ChangeFacts, Check, CoverageRecord, DefDelta, ImpactSet, TestRef};
use hord_verify_rust::{
    CargoRunner, CargoWorkspace, Fallback, RunOutput, SelectInput, Selection, select,
    select_ignoring,
};
use serde::{Deserialize, Serialize};

use crate::fault::{FaultKind, attempts, inject};
use crate::git::Checkout;
use crate::prepare::CommitFacts;

/// [`hord_verify_rust::coverage::collect`] with [`corpus_env`]: every
/// instrumented run of the cargo corpus goes through here.
pub(crate) fn collect_corpus(
    checkout: &hord_verify::Checkout,
    toolchain: &hord_verify::Toolchain,
    defs: &hord_verify_rust::DefinitionIndex,
    options: &hord_verify_rust::CoverageOptions,
) -> Result<hord_verify_rust::CoverageRun> {
    let started = Instant::now();
    match hord_verify_rust::coverage::build_suite_with_env(checkout, options, &corpus_env())? {
        Some(suite) => Ok(hord_verify_rust::coverage::run_suite(
            &suite, toolchain, defs, options, started,
        )?),
        None => Ok(hord_verify_rust::CoverageRun {
            record: CoverageRecord::new(
                checkout.snapshot,
                toolchain.id()?,
                BTreeSet::new(),
                Vec::new(),
            ),
            elapsed_ms: elapsed(started),
            command: "cargo llvm-cov (per test): nothing selected".into(),
            log: String::new(),
            lines: BTreeMap::new(),
        }),
    }
}

/// Environment for every test run of the cargo corpus: configuration of
/// cargo's own test suite, not hord logic.
///
/// `CFG_DISABLE_CROSS_TESTS=1` is the switch cargo's testsuite reads
/// (`tests/testsuite/utils/cross_compile.rs`, `disabled()`) to skip its
/// cross-compilation tests. Without it, the first cross test in each test
/// process builds a probe project for the alternate target, and on a host
/// without that target panics once per process to warn. Batched runs see
/// one such failure (`aaa_trigger_cross_compile_disabled_check`); per-test
/// processes (instrumented coverage runs) see it in every cross test.
pub(crate) fn corpus_env() -> BTreeMap<String, String> {
    [("CFG_DISABLE_CROSS_TESTS".to_owned(), "1".to_owned())]
        .into_iter()
        .collect()
}

/// Variables cargo's UI tests set on their own command and then strip if
/// the parent environment has them (`cargo_ui()` sets them before
/// `test_env()` removes every `CARGO_*` key found in the parent). Inherited,
/// they uncolor the output and every `.term.svg` snapshot fails.
const STRIPPED_BY_CARGO_UI_TESTS: [&str; 2] = ["CARGO_TERM_COLOR", "CARGO_TERM_HYPERLINKS"];

/// Refuse to run the corpus with an environment that makes its tests fail
/// for reasons unrelated to the commit. The eval cannot remove them itself
/// (`remove_var` is unsafe), so the caller has to.
pub(crate) fn check_corpus_env(vars: impl IntoIterator<Item = (String, String)>) -> Result<()> {
    let set: Vec<String> = vars
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| STRIPPED_BY_CARGO_UI_TESTS.contains(&k.as_str()))
        .collect();
    anyhow::ensure!(
        set.is_empty(),
        "unset {} before running the cargo corpus: cargo's UI tests strip inherited \
         CARGO_* variables after setting their own, so their `.term.svg` snapshots fail",
        set.join(" and ")
    );
    Ok(())
}

/// Up to `max_tests` failing tests' output from libtest's `failures:`
/// section, one per top-level module first, each cut to `max_lines` lines:
/// enough to diagnose environment failures from a CI log.
pub(crate) fn failure_excerpts(
    stdout: &str,
    max_tests: usize,
    max_lines: usize,
) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in stdout.lines() {
        let header = line
            .strip_prefix("---- ")
            .and_then(|l| l.strip_suffix(" stdout ----"));
        if let Some(name) = header {
            blocks.extend(current.take());
            current = Some((name.to_owned(), Vec::new()));
        } else if line == "failures:" || line.starts_with("test result:") {
            blocks.extend(current.take());
        } else if let Some((_, lines)) = &mut current {
            lines.push(line);
        }
    }
    blocks.extend(current);
    let module = |name: &str| name.split("::").next().unwrap_or_default().to_owned();
    let mut picked: Vec<usize> = Vec::new();
    let mut modules = BTreeSet::new();
    for (i, (name, _)) in blocks.iter().enumerate() {
        if picked.len() < max_tests && modules.insert(module(name)) {
            picked.push(i);
        }
    }
    for i in 0..blocks.len() {
        if picked.len() < max_tests && !picked.contains(&i) {
            picked.push(i);
        }
    }
    picked.sort_unstable();
    picked
        .into_iter()
        .map(|i| {
            let (name, lines) = &blocks[i];
            let mut text: Vec<&str> = lines.iter().take(max_lines).copied().collect();
            if lines.len() > max_lines {
                text.push("[...]");
            }
            (name.clone(), text.join("\n"))
        })
        .collect()
}

/// Where one worker builds and runs.
pub(crate) struct Worker {
    pub checkout: Checkout,
    pub target_dir: PathBuf,
}

/// The environment of every cargo command a worker runs: the corpus
/// configuration, and `LLVM_PROFILE_FILE` inside the worker's directory.
pub(crate) fn runner_env(worker: &Worker) -> BTreeMap<String, String> {
    let mut env = corpus_env();
    env.insert("LLVM_PROFILE_FILE".to_owned(), worker.profraw_pattern());
    env
}

impl Worker {
    /// `LLVM_PROFILE_FILE` for everything this worker runs: inside its own
    /// directory, never the working directory.
    pub(crate) fn profraw_pattern(&self) -> String {
        self.profraw_dir()
            .join("%p-%m.profraw")
            .display()
            .to_string()
    }

    /// The directory of [`Worker::profraw_pattern`].
    pub(crate) fn profraw_dir(&self) -> PathBuf {
        self.target_dir.with_file_name("profraw")
    }
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
    /// Faults graded per commit where (b) does not contain (a) (ADR 0023
    /// amendment); commits where it does get one probe-graded fault.
    pub faults_per_commit: usize,
}

/// The selection variants measured side by side (orchestrator request,
/// after the smoke run): A = current rules; B = region-level coverage for
/// edited functions; C = narrowed non-Rust fallback; D = B and C.
pub(crate) const VARIANTS: [&str; 4] = ["A", "B", "C", "D"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct VariantResult {
    /// Tests of the record the selection runs (plus tests it adds that the
    /// record does not know); doctests excluded.
    pub selected: usize,
    /// Tests in the record (the suite).
    pub suite: usize,
    /// Fallback kinds that fired.
    pub fallbacks: Vec<String>,
    /// Whether (b) under this variant detected the fault; `None` without a
    /// fault.
    pub detected: Option<bool>,
    /// This variant's selection includes every test of (a).
    pub covers_a: bool,
    /// (a) detected the fault and this variant's (b) did not.
    pub miss: bool,
    /// The miss reproduced: the missed failures pass without the fault.
    pub confirmed_miss: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Fault {
    pub path: String,
    pub name: String,
    pub kind: FaultKind,
    /// Attempts that did not type-check before this one.
    pub rejected: usize,
    /// Edited lines of the faulted function (0: born, or no line diff);
    /// variant B filters by them.
    pub edited_lines: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CommitResult {
    pub index: usize,
    pub commit: String,
    pub write_set: usize,
    pub affected: Vec<String>,
    pub fault: Option<Fault>,
    /// Why no fault was injected.
    pub no_fault: Option<String>,
    pub variants: BTreeMap<String, VariantResult>,
    /// Whether (a) detected the fault, when determined (it is not run when
    /// every variant detected it).
    pub detected_a: Option<bool>,
    /// Units observed failing with the fault.
    pub failed: Vec<String>,
    /// A command hung or crashed; its units were counted as failing.
    pub unattributed: bool,
    pub build_ms: u64,
    pub run_ms: u64,
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

pub(crate) fn cargo(args: Vec<String>) -> Check {
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
pub(crate) fn failures(out: &RunOutput, quarantine: &BTreeSet<String>) -> Vec<String> {
    out.report
        .failed
        .iter()
        .filter(|n| !quarantine.contains(*n))
        .cloned()
        .collect()
}

/// A run detects the fault when a test fails, or it times out or crashes
/// after building (a hang or abort is observable too).
pub(crate) fn detects(out: &RunOutput, quarantine: &BTreeSet<String>) -> bool {
    !failures(out, quarantine).is_empty() || (!out.success() && !out.build_failed)
}

/// One thing (b) or (a) can run.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum Unit {
    /// A test the coverage record knows.
    Test(TestRef),
    /// A package's doctests.
    Doc(String),
    /// Tests of a package whose names contain the filter (a test the
    /// record does not know yet).
    Filter(String, String),
}

impl Unit {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Test(t) => format!("{}/{}:{}", t.package, t.target.name, t.name),
            Self::Doc(p) => format!("{p}/doc"),
            Self::Filter(p, f) => format!("{p}/*{f}*"),
        }
    }
}

/// The units `selection` runs. A package-level or full selection runs the
/// package's known tests, its doctests, and `new_tests` (tests the change
/// adds that the record does not know) in it.
pub(crate) fn units(
    selection: &Selection,
    record: &CoverageRecord,
    quarantine: &BTreeSet<TestRef>,
    ws: &CargoWorkspace,
    new_tests: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<Unit> {
    let whole: BTreeSet<String> = if selection.full {
        ws.packages.keys().cloned().collect()
    } else {
        selection.packages.clone()
    };
    let mut out = BTreeSet::new();
    for t in record.tests.iter().map(|t| &t.test) {
        if quarantine.contains(t) {
            continue;
        }
        let exact = selection
            .exact
            .get(&(t.package.clone(), t.target.clone()))
            .is_some_and(|n| n.contains(&t.name));
        let filtered = selection
            .filters
            .get(&t.package)
            .is_some_and(|fs| fs.iter().any(|f| t.name.contains(f.as_str())));
        if whole.contains(&t.package) || exact || filtered {
            out.insert(Unit::Test(t.clone()));
        }
    }
    let known = |pkg: &str, f: &str| {
        record
            .tests
            .iter()
            .any(|t| t.test.package == pkg && t.test.name.contains(f))
    };
    for (pkg, fs) in &selection.filters {
        for f in fs {
            if !known(pkg, f) {
                out.insert(Unit::Filter(pkg.clone(), f.clone()));
            }
        }
    }
    for p in &whole {
        if ws.packages.get(p).is_some_and(|p| p.has_doctests()) {
            out.insert(Unit::Doc(p.clone()));
        }
        for f in new_tests.get(p).into_iter().flatten() {
            if !known(p, f) {
                out.insert(Unit::Filter(p.clone(), f.clone()));
            }
        }
    }
    for p in &selection.doc {
        out.insert(Unit::Doc(p.clone()));
    }
    out
}

/// Variant B's record: a test keeps its edge to an edited function only if
/// it executed one of that function's edited lines. Everything else (other
/// edges, drift, births, fallbacks) is unchanged.
pub(crate) fn region_record(
    record: &CoverageRecord,
    facts: &ChangeFacts,
    edited: &BTreeMap<NodeId, (RepoPath, BTreeSet<u32>)>,
    lines: &BTreeMap<TestRef, BTreeMap<RepoPath, BTreeSet<u32>>>,
) -> CoverageRecord {
    let targets: Vec<(NodeId, &RepoPath, &BTreeSet<u32>)> = facts
        .touched
        .iter()
        .filter(|t| t.kind.as_str() == "function_item" && t.delta == DefDelta::Edited)
        .filter_map(|t| edited.get(&t.node).map(|(p, l)| (t.node, p, l)))
        .collect();
    if targets.is_empty() {
        return record.clone();
    }
    let tests = record
        .tests
        .iter()
        .map(|tc| {
            let mut covers: BTreeSet<NodeId> = record.covered_by(tc).collect();
            for (node, path, want) in &targets {
                if covers.contains(node) {
                    let hit = lines
                        .get(&tc.test)
                        .and_then(|l| l.get(*path))
                        .is_some_and(|l| !l.is_disjoint(want));
                    if !hit {
                        covers.remove(node);
                    }
                }
            }
            (tc.test.clone(), tc.node, covers, tc.failed)
        })
        .collect();
    CoverageRecord::new(
        record.snapshot,
        record.toolchain,
        record.defs.iter().copied().collect(),
        tests,
    )
}

/// String literal contents of Rust source (escapes left as written).
fn string_literals(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\'' => {
                // A char literal ('"', '\'') or a lifetime: skip the quote
                // and what it may hide.
                if bytes.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'\\') {
                    i += 4;
                } else {
                    i += 1;
                }
            }
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if let Some(lit) = text.get(start..i.min(bytes.len())) {
                    out.push(lit);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether `literal` names `path` by suffix: its components, without
/// leading `.` and `..`, end `path` (at least the file name).
fn names_path(literal: &str, path: &RepoPath) -> bool {
    let parts: Vec<&str> = literal
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    if parts.is_empty() || parts.len() > path.components().len() {
        return false;
    }
    let tail = &path.components()[path.components().len() - parts.len()..];
    tail.iter().zip(&parts).all(|(a, b)| a == b)
}

/// Variant C's facts: a changed non-Rust file stays (and can trigger the
/// fallback) only if it is inside a build target's source directory, or a
/// string literal in its package's Rust files names it by path suffix
/// (`include_str!`, `include_bytes!`, `file!`, or any other literal).
/// Manifests, lockfiles, build scripts, and workspace files are kept.
pub(crate) fn narrow_non_rust(
    facts: &ChangeFacts,
    ws: &CargoWorkspace,
    root: &Path,
) -> ChangeFacts {
    let mut literals: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = facts.clone();
    out.paths.retain(|path| {
        let name = path.components().last().map_or("", String::as_str);
        if name.ends_with(".rs")
            || name == "Cargo.toml"
            || name == "Cargo.lock"
            || ws.is_workspace_wide(path)
            || ws.is_build_script(path)
        {
            return true;
        }
        let Some(pkg) = ws.package_of(path) else {
            return true;
        };
        let in_sources = pkg.targets.iter().any(|t| {
            if t.kind.iter().any(|k| k == "custom-build") {
                return false;
            }
            let mut dir = t.src_path.components().to_vec();
            dir.pop();
            dir.len() > pkg.dir.components().len() && path.components().starts_with(&dir)
        });
        if in_sources {
            return true;
        }
        let lits = literals.entry(pkg.name.clone()).or_insert_with(|| {
            let mut all = Vec::new();
            let base = root.join(pkg.dir.to_string());
            let nested: Vec<std::path::PathBuf> = ws
                .packages
                .values()
                .filter(|p| {
                    p.dir != pkg.dir && p.dir.components().starts_with(pkg.dir.components())
                })
                .map(|p| root.join(p.dir.to_string()))
                .collect();
            let mut stack = vec![base];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for e in entries.flatten() {
                    let p = e.path();
                    let fname = e.file_name().to_string_lossy().into_owned();
                    if p.is_dir() {
                        if fname != "target" && !fname.starts_with('.') && !nested.contains(&p) {
                            stack.push(p);
                        }
                    } else if fname.ends_with(".rs")
                        && let Ok(text) = std::fs::read_to_string(&p)
                    {
                        all.extend(string_literals(&text).into_iter().map(str::to_owned));
                    }
                }
            }
            all
        });
        lits.iter().any(|l| names_path(l, path))
    });
    out
}

/// Runs units, recording which ran and which failed.
pub(crate) struct Grader<'a> {
    pub runner: &'a CargoRunner,
    pub root: &'a Path,
    pub ran: BTreeSet<Unit>,
    pub failed: BTreeSet<Unit>,
    pub unattributed: bool,
    pub timed_out: bool,
}

impl<'a> Grader<'a> {
    /// A grader that has run nothing yet.
    pub(crate) fn new(runner: &'a CargoRunner, root: &'a Path) -> Self {
        Self {
            runner,
            root,
            ran: BTreeSet::new(),
            failed: BTreeSet::new(),
            unattributed: false,
            timed_out: false,
        }
    }

    /// Run `batch` (in order, one command per test binary, doctest set, or
    /// package filter) until `stop` holds after a command.
    pub(crate) fn run(
        &mut self,
        batch: Vec<Unit>,
        stop: &dyn Fn(&BTreeSet<Unit>) -> bool,
    ) -> Result<()> {
        // A read-only test that failed in an earlier command left its
        // scratch tree unwritable; open it up so this command's runs of the
        // same tests start clean (and a clean re-run means something).
        if let Some(target) = &self.runner.target_dir {
            crate::disk::restore_permissions(&crate::disk::scratch_dir(target));
        }
        // Group, keeping the batch's order of first appearance.
        let mut groups: Vec<(String, Vec<Unit>)> = Vec::new();
        for u in batch {
            let key = match &u {
                Unit::Test(t) => format!("t {} {} {}", t.package, t.target.kind, t.target.name),
                Unit::Doc(p) => format!("d {p}"),
                Unit::Filter(p, _) => format!("f {p}"),
            };
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(u),
                None => groups.push((key, vec![u])),
            }
        }
        for (_, group) in groups {
            let mut args = vec!["test".to_owned(), "-p".to_owned()];
            match &group[0] {
                Unit::Test(t) => {
                    args.push(t.package.clone());
                    match t.target.kind.as_str() {
                        "lib" => args.push("--lib".into()),
                        kind => {
                            args.push(format!("--{kind}"));
                            args.push(t.target.name.clone());
                        }
                    }
                    args.extend(["--".into(), "--exact".into()]);
                    for u in &group {
                        if let Unit::Test(t) = u {
                            args.push(t.name.clone());
                        }
                    }
                }
                Unit::Doc(p) => {
                    args.push(p.clone());
                    args.push("--doc".into());
                }
                Unit::Filter(p, _) => {
                    args.push(p.clone());
                    args.push("--".into());
                    for u in &group {
                        if let Unit::Filter(_, f) = u {
                            args.push(f.clone());
                        }
                    }
                }
            }
            let out = self.runner.run(self.root, &cargo(args))?;
            self.timed_out |= out.timed_out;
            let names = failures(&out, &BTreeSet::new());
            let mut attributed = false;
            for u in &group {
                self.ran.insert(u.clone());
                let hit = match u {
                    Unit::Test(t) => names.contains(&t.name),
                    Unit::Doc(_) => detects(&out, &BTreeSet::new()),
                    Unit::Filter(_, f) => names.iter().any(|n| n.contains(f.as_str())),
                };
                if hit {
                    attributed = true;
                    self.failed.insert(u.clone());
                }
            }
            if detects(&out, &BTreeSet::new()) && !attributed {
                // A hang or crash with no named failure: count the whole
                // command as failing, and say so in the result.
                self.unattributed = true;
                self.failed.extend(group.iter().cloned());
            }
            if stop(&self.failed) {
                break;
            }
        }
        Ok(())
    }
}

/// Everything one commit's evaluation needs besides the worker.
pub(crate) struct CommitInput<'a> {
    pub facts: &'a CommitFacts,
    pub record: &'a CoverageRecord,
    /// Per test, the lines of interest it executed (variant B).
    pub lines: &'a BTreeMap<TestRef, BTreeMap<RepoPath, BTreeSet<u32>>>,
}

/// Diagnostic only: each variant's selection size with the fallback kinds
/// in `ignore` turned off ([`hord_verify_rust::select_ignoring`]). Nothing
/// runs and nothing is graded; this is not a safe selection.
pub(crate) fn sizes_ignoring(
    ctx: &Ctx,
    worker: &Worker,
    input: &CommitInput<'_>,
    ignore: &BTreeSet<&str>,
) -> Result<BTreeMap<String, (usize, Vec<String>)>> {
    let facts = input.facts;
    let root = &worker.checkout.root;
    worker.checkout.checkout(&facts.commit)?;
    let workspace = CargoWorkspace::load(root)?;
    let toolchain = ctx.toolchain.id()?;
    let record_b = region_record(
        input.record,
        &facts.impact.facts,
        &facts.edited_lines,
        input.lines,
    );
    let impact_c = ImpactSet {
        facts: narrow_non_rust(&facts.impact.facts, &workspace, root),
        ..facts.impact.clone()
    };
    let mut out = BTreeMap::new();
    for (v, rec, impact) in [
        ("A", input.record, &facts.impact),
        ("B", &record_b, &facts.impact),
        ("C", input.record, &impact_c),
        ("D", &record_b, &impact_c),
    ] {
        let sel = select_ignoring(
            SelectInput {
                coverage: Some(rec),
                toolchain,
                workspace: &workspace,
                impact,
                max_impact: None,
                drift: None,
            },
            ignore,
        );
        let kinds: BTreeSet<&str> = sel.fallbacks.iter().map(Fallback::kind).collect();
        out.insert(
            v.to_owned(),
            (
                count(&sel, input.record, &ctx.quarantine).0,
                kinds.into_iter().map(str::to_owned).collect(),
            ),
        );
    }
    Ok(out)
}

pub(crate) fn evaluate(
    ctx: &Ctx,
    worker: &Worker,
    input: &CommitInput<'_>,
) -> Result<CommitResult> {
    let started = Instant::now();
    let facts = input.facts;
    let record = input.record;
    let root = &worker.checkout.root;
    worker.checkout.checkout(&facts.commit)?;
    let workspace = CargoWorkspace::load(root)?;
    let runner = CargoRunner {
        target_dir: Some(worker.target_dir.clone()),
        timeout: Some(ctx.timeout),
        idle_timeout: Some(ctx.idle),
        env: runner_env(worker),
        ..CargoRunner::default()
    };
    let toolchain = ctx.toolchain.id()?;

    // The four selections, from the same checkpoint.
    let record_b = region_record(
        record,
        &facts.impact.facts,
        &facts.edited_lines,
        input.lines,
    );
    let facts_c = narrow_non_rust(&facts.impact.facts, &workspace, root);
    let impact_c = ImpactSet {
        facts: facts_c,
        ..facts.impact.clone()
    };
    let select_with = |rec: &CoverageRecord, impact: &ImpactSet| {
        select(SelectInput {
            coverage: Some(rec),
            toolchain,
            workspace: &workspace,
            impact,
            max_impact: None,
            drift: None,
        })
    };
    let selections: BTreeMap<&str, Selection> = [
        ("A", select_with(record, &facts.impact)),
        ("B", select_with(&record_b, &facts.impact)),
        ("C", select_with(record, &impact_c)),
        ("D", select_with(&record_b, &impact_c)),
    ]
    .into_iter()
    .collect();
    // Tests the change adds that the record does not know, per package.
    let mut new_tests: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &facts.impact.facts.touched {
        if t.test
            && t.delta != DefDelta::Died
            && record.tests_at(t.node).next().is_none()
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
    let sets: BTreeMap<&str, BTreeSet<Unit>> = selections
        .iter()
        .map(|(v, s)| {
            (
                *v,
                units(s, record, &ctx.quarantine, &workspace, &new_tests),
            )
        })
        .collect();
    let mut result = CommitResult {
        index: facts.index,
        commit: facts.commit.clone(),
        write_set: facts.write_set.len(),
        ..CommitResult::default()
    };
    for (v, s) in &selections {
        let (selected, suite) = count(s, record, &ctx.quarantine);
        let kinds: BTreeSet<&str> = s.fallbacks.iter().map(Fallback::kind).collect();
        result.variants.insert(
            (*v).to_owned(),
            VariantResult {
                selected,
                suite,
                fallbacks: kinds.into_iter().map(str::to_owned).collect(),
                ..VariantResult::default()
            },
        );
    }

    // (a)'s packages: those the commit changed, and their reverse deps.
    let touched: BTreeSet<String> = facts
        .changed_paths
        .iter()
        .filter_map(|p| p.parse::<RepoPath>().ok())
        .filter_map(|p| workspace.package_of(&p).map(|p| p.name.clone()))
        .collect();
    let affected = workspace.with_reverse_deps(&touched);
    result.affected = affected.iter().cloned().collect();
    let a_selection = Selection {
        packages: affected.clone(),
        ..Selection::default()
    };
    let a_set = units(
        &a_selection,
        record,
        &ctx.quarantine,
        &workspace,
        &new_tests,
    );
    for (v, r) in result.variants.iter_mut() {
        r.covers_a = a_set.is_subset(&sets[v.as_str()]);
    }
    let scope: Vec<String> = affected
        .iter()
        .flat_map(|p| ["-p".to_owned(), p.clone()])
        .collect();

    // Inject the first fault that type-checks.
    let mut injected: Option<(String, NodeId)> = None;
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
        std::fs::write(
            &file,
            format!("{}{faulty}{}", &text[..t.span.start], &text[t.span.end..]),
        )?;
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
                edited_lines: facts.edited_lines.get(&t.node).map_or(0, |(_, l)| l.len()),
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

    // Grade every variant: run the smallest undecided variant's remaining
    // units (the tests that ran the faulted function first) until each
    // variant has a failure or has run all its units.
    let run_start = Instant::now();
    let probe: BTreeSet<Unit> = record
        .tests_covering(&[fault_node].into_iter().collect())
        .into_iter()
        .map(|t| Unit::Test(t.clone()))
        .collect();
    let mut order: Vec<&str> = VARIANTS.to_vec();
    order.sort_by_key(|v| (sets[v].len(), *v));
    let mut grader = Grader::new(&runner, root);
    // The probe runs first for everyone. A variant whose selection holds
    // all of (a) cannot miss, so it is not graded further.
    let probe_batch: Vec<Unit> = probe.intersection(&a_set).cloned().collect();
    grader.run(probe_batch, &|_| false)?;
    loop {
        let next = order.iter().find(|v| {
            let set = &sets[**v];
            !a_set.is_subset(set) && set.is_disjoint(&grader.failed) && !set.is_subset(&grader.ran)
        });
        let Some(v) = next else {
            break;
        };
        let set = &sets[*v];
        let mut batch: Vec<Unit> = set.difference(&grader.ran).cloned().collect();
        batch.sort_by_key(|u| !probe.contains(u));
        grader.run(batch, &|failed| !set.is_disjoint(failed))?;
    }
    // (a): only when some variant neither detected nor runs all of (a).
    let need_a = VARIANTS
        .iter()
        .any(|v| sets[v].is_disjoint(&grader.failed) && !a_set.is_subset(&sets[v]));
    if need_a && a_set.is_disjoint(&grader.failed) {
        let mut batch: Vec<Unit> = a_set.difference(&grader.ran).cloned().collect();
        batch.sort_by_key(|u| !probe.contains(u));
        grader.run(batch, &|failed| !a_set.is_disjoint(failed))?;
    }
    result.detected_a = if !a_set.is_disjoint(&grader.failed) {
        Some(true)
    } else if a_set.is_subset(&grader.ran) {
        Some(false)
    } else {
        None
    };
    for (v, r) in result.variants.iter_mut() {
        let set = &sets[v.as_str()];
        let detected = !set.is_disjoint(&grader.failed);
        // A covering variant that was not seen failing: undetermined (it
        // detects exactly when (a) does), unless all of it ran.
        r.detected = if detected || set.is_subset(&grader.ran) || !r.covers_a {
            Some(detected)
        } else {
            result.detected_a
        };
        r.miss = result.detected_a == Some(true) && !detected;
    }
    result.failed = grader.failed.iter().map(Unit::label).collect();
    result.unattributed = grader.unattributed;
    let _ = grader.timed_out;
    worker.checkout.restore(&fault_path)?;

    // Confirm misses: the (a) failures a missing variant did not run must
    // pass without the fault.
    for v in VARIANTS {
        if !result.variants[v].miss {
            continue;
        }
        let missed: Vec<Unit> = grader
            .failed
            .iter()
            .filter(|u| a_set.contains(u) && !sets[v].contains(u))
            .cloned()
            .collect();
        let mut clean = Grader::new(&runner, root);
        clean.run(missed.clone(), &|_| false)?;
        let confirmed = !missed.is_empty() && clean.failed.is_empty() && !grader.unattributed;
        if let Some(r) = result.variants.get_mut(v) {
            r.confirmed_miss = confirmed;
        }
    }
    result.run_ms = elapsed(run_start);
    result.total_ms = elapsed(started);
    Ok(result)
}

pub(crate) fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The literal full suite on one unmutated commit.
pub(crate) struct FullSuite {
    pub failed: Vec<String>,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    /// A few failing tests' output (see [`failure_excerpts`]).
    pub excerpts: Vec<(String, String)>,
}

/// Failing tests whose output a sample keeps.
const SAMPLE_EXCERPTS: usize = 5;
/// Lines kept per excerpt.
const SAMPLE_EXCERPT_LINES: usize = 60;

/// Run the literal full suite on an unmutated commit (ADR 0023 sample).
pub(crate) fn full_suite(worker: &Worker, commit: &str, timeout: Duration) -> Result<FullSuite> {
    worker.checkout.checkout(commit)?;
    let runner = CargoRunner {
        target_dir: Some(worker.target_dir.clone()),
        timeout: Some(timeout),
        env: runner_env(worker),
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
    Ok(FullSuite {
        failed: out.report.failed.into_iter().collect(),
        elapsed_ms: elapsed(start),
        timed_out: out.timed_out,
        excerpts: failure_excerpts(&out.stdout, SAMPLE_EXCERPTS, SAMPLE_EXCERPT_LINES),
    })
}

pub(crate) fn worker(root: &Path, corpus: &Path, id: usize) -> Result<Worker> {
    Ok(Worker {
        checkout: Checkout::open(corpus, &root.join(format!("w{id}/src")))?,
        target_dir: root.join(format!("w{id}/target")),
    })
}

#[cfg(test)]
mod tests {
    use hord_core::ObjectId;
    use hord_verify::TestTarget;

    use super::*;

    fn tref(pkg: &str, name: &str) -> TestRef {
        TestRef {
            package: pkg.into(),
            target: TestTarget {
                kind: "test".into(),
                name: "s".into(),
            },
            name: name.into(),
        }
    }

    #[test]
    fn corpus_env_refuses_what_cargo_ui_tests_strip() {
        let vars = |keys: &[&str]| -> Vec<(String, String)> {
            keys.iter()
                .map(|k| ((*k).to_owned(), "x".to_owned()))
                .collect()
        };
        assert!(check_corpus_env(vars(&["CARGO_HOME", "CI", "TERM"])).is_ok());
        let err = check_corpus_env(vars(&["CARGO_TERM_COLOR", "CARGO_TERM_HYPERLINKS"]))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.contains("CARGO_TERM_COLOR and CARGO_TERM_HYPERLINKS"),
            "{err}"
        );
    }

    #[test]
    fn failure_excerpts_prefer_distinct_modules_and_cut_long_output() {
        let stdout = "running 4 tests\n\
            test a::one ... FAILED\n\n\
            failures:\n\n\
            ---- a::one stdout ----\nline 1\nline 2\nline 3\n\n\
            ---- a::two stdout ----\nother\n\n\
            ---- b::three stdout ----\nb out\n\n\
            failures:\n    a::one\n    a::two\n    b::three\n\n\
            test result: FAILED. 1 passed; 3 failed\n";
        let two = failure_excerpts(stdout, 2, 2);
        let names: Vec<&str> = two.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a::one", "b::three"], "one per module first");
        assert_eq!(two[0].1, "line 1\nline 2\n[...]");
        assert_eq!(two[1].1, "b out\n");
        let all = failure_excerpts(stdout, 5, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].0, "a::two");
        assert!(failure_excerpts("test result: ok.\n", 5, 10).is_empty());
    }

    fn workspace() -> CargoWorkspace {
        let json = r#"{"workspace_root": "/w", "workspace_members": ["a 0.1"],
          "packages": [{"id": "a 0.1", "name": "a", "manifest_path": "/w/Cargo.toml",
            "targets": [{"name": "a", "kind": ["lib"], "src_path": "/w/src/lib.rs"},
                        {"name": "s", "kind": ["test"], "src_path": "/w/tests/s/main.rs", "doctest": false},
                        {"name": "build-script-build", "kind": ["custom-build"], "src_path": "/w/build.rs"}],
            "dependencies": []}]}"#;
        CargoWorkspace::from_metadata_json(json.as_bytes()).expect("from metadata json")
    }

    #[test]
    fn units_expand_packages_exact_filters_and_doctests() {
        let record = CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            ObjectId::from_bytes([2; 32]),
            BTreeSet::new(),
            vec![
                (tref("a", "one"), None, BTreeSet::new(), false),
                (tref("a", "two"), None, BTreeSet::new(), false),
            ],
        );
        let ws = workspace();
        let new_tests: BTreeMap<String, BTreeSet<String>> =
            [("a".to_owned(), ["fresh".to_owned()].into_iter().collect())]
                .into_iter()
                .collect();
        let mut sel = Selection::default();
        sel.exact
            .entry(("a".into(), tref("a", "one").target))
            .or_default()
            .insert("one".into());
        let u = units(&sel, &record, &BTreeSet::new(), &ws, &new_tests);
        assert_eq!(u, [Unit::Test(tref("a", "one"))].into_iter().collect());
        sel.packages.insert("a".into());
        let u = units(
            &sel,
            &record,
            &[tref("a", "two")].into_iter().collect(),
            &ws,
            &new_tests,
        );
        assert_eq!(
            u,
            [
                Unit::Test(tref("a", "one")),
                Unit::Doc("a".into()),
                Unit::Filter("a".into(), "fresh".into())
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn region_record_keeps_edges_of_tests_that_ran_an_edited_line() {
        use hord_verify::TouchedDef;
        let f = NodeId::from_u128(7);
        let path: RepoPath = "src/lib.rs".parse().expect("parse `src/lib.rs`");
        let record = CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            ObjectId::from_bytes([2; 32]),
            [f].into_iter().collect(),
            vec![
                (tref("a", "hits"), None, [f].into_iter().collect(), false),
                (tref("a", "misses"), None, [f].into_iter().collect(), false),
            ],
        );
        let mut facts = ChangeFacts::default();
        facts.touched.push(TouchedDef {
            node: f,
            path: path.clone(),
            kind: hord_core::NodeKind::new("function_item"),
            name: None,
            delta: DefDelta::Edited,
            attributes_changed: false,
            test: false,
            dispatch: false,
        });
        let edited = [(f, (path.clone(), [10].into_iter().collect()))]
            .into_iter()
            .collect();
        let lines = [
            (
                tref("a", "hits"),
                [(path.clone(), [9, 10].into_iter().collect())]
                    .into_iter()
                    .collect(),
            ),
            (
                tref("a", "misses"),
                [(path.clone(), [9].into_iter().collect())]
                    .into_iter()
                    .collect(),
            ),
        ]
        .into_iter()
        .collect();
        let b = region_record(&record, &facts, &edited, &lines);
        let hit: Vec<String> = b
            .tests_covering(&[f].into_iter().collect())
            .into_iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(hit, vec!["hits"]);
        assert!(
            b.is_instrumented(f),
            "the definition keeps its coverage record"
        );
    }

    #[test]
    fn narrowing_keeps_sources_named_files_and_manifests() {
        let root = std::env::temp_dir().join(format!("hord-m4-narrow-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("create dir all");
        std::fs::write(
            root.join("src/lib.rs"),
            "const R: &str = include_str!(\"../docs/used.md\"); // \"docs/comment.md\"\nfn f() { let c = '\"'; }\n",
        )
        .expect("write a fixture file");
        let ws = workspace();
        let mut facts = ChangeFacts::default();
        for p in [
            "src/data.json",
            "docs/used.md",
            "docs/unused.md",
            "docs/comment.md",
            "triagebot.toml",
            "Cargo.toml",
            "build.rs",
            "tests/s/fixture.svg",
        ] {
            facts.paths.insert(p.parse().expect("parse test path"));
        }
        let kept: Vec<String> = narrow_non_rust(&facts, &ws, &root)
            .paths
            .iter()
            .map(ToString::to_string)
            .collect();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            kept,
            vec![
                "Cargo.toml",
                "build.rs",
                "docs/used.md",
                "src/data.json",
                "tests/s/fixture.svg"
            ]
        );
    }

    #[test]
    fn literals_and_path_suffixes() {
        assert_eq!(
            string_literals(r#"a("x/y.md", 'c', "z\"q") // "no""#),
            vec!["x/y.md", r#"z\"q"#]
        );
        let p: RepoPath = "tests/testsuite/foo/stderr.term.svg"
            .parse()
            .expect("parse `tests/testsuite/foo/stderr.term.svg`");
        assert!(names_path("stderr.term.svg", &p));
        assert!(names_path("../foo/stderr.term.svg", &p));
        assert!(!names_path("bar/stderr.term.svg", &p));
        assert!(!names_path("./", &p));
    }

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
