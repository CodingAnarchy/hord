//! Per-test coverage with `cargo-llvm-cov` (ADR 0022).
//!
//! 1. Build every test binary with the instrumentation `cargo llvm-cov
//!    show-env` sets up (its `RUSTC_WRAPPER` instruments workspace crates
//!    only), into a separate target directory.
//! 2. List each binary's tests and capture the environment cargo runs it
//!    with, through a `runner` script, so step 3 can run the binary
//!    directly exactly as `cargo test` would.
//! 3. Run every test alone (`--exact`), in parallel, each with its own
//!    `LLVM_PROFILE_FILE` directory. Child processes inherit it, so a test
//!    that drives a built binary as a subprocess (cargo's `testsuite`) is
//!    attributed the code that subprocess ran. `llvm-profdata merge` and
//!    `show --all-functions` give the functions it entered.
//! 4. `llvm-cov export` maps every instrumented function to its source
//!    lines once; the innermost definition containing them is the
//!    function's [`NodeId`].
//!
//! The result is a [`CoverageRecord`] keyed by NodeId, stored as
//! `Evidence { kind: Custom("coverage") }` ([`record_evidence`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hord_core::{Actor, EvidenceKind, EvidenceResult, NodeId, ObjectId, RepoPath};
use hord_verify::{
    COVERAGE_KIND, Checkout, CoverageRecord, Definition, Error, EvidenceFields, EvidenceIndex,
    Result, TestRef, TestTarget, Toolchain, line_range,
};
use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
use crate::cargo::CargoWorkspace;
use crate::libtest::parse_list;
use crate::runner::{now, run_captured};

/// [`Toolchain`] component name of `cargo-llvm-cov`.
pub const LLVM_COV: &str = "cargo-llvm-cov";

/// Definitions of a snapshot by file and line, to attribute coverage.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DefinitionIndex {
    files: BTreeMap<RepoPath, Vec<(Range<u32>, usize, NodeId)>>,
}

impl DefinitionIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the definitions of one file, whose contents are `bytes`.
    pub fn add_file(&mut self, path: &RepoPath, bytes: &[u8], defs: &[Definition]) {
        let entry = self.files.entry(path.clone()).or_default();
        for def in defs {
            entry.push((line_range(bytes, &def.span), def.span.len(), def.node));
        }
    }

    /// Replace the definitions of one file (after a change rewrote it).
    pub fn set_file(&mut self, path: &RepoPath, bytes: &[u8], defs: &[Definition]) {
        self.files.remove(path);
        self.add_file(path, bytes, defs);
    }

    /// Forget a file (deleted, or no longer parsed).
    pub fn remove_file(&mut self, path: &RepoPath) {
        self.files.remove(path);
    }

    /// Replace every file `other` indexes with its entries.
    pub fn update(&mut self, other: &DefinitionIndex) {
        for (path, entries) in &other.files {
            self.files.insert(path.clone(), entries.clone());
        }
    }

    /// The smallest definition of `path` whose lines contain `lines`.
    #[must_use]
    pub fn innermost(&self, path: &RepoPath, lines: &Range<u32>) -> Option<NodeId> {
        self.files
            .get(path)?
            .iter()
            .filter(|(r, _, _)| r.start <= lines.start && lines.end <= r.end)
            .min_by_key(|(_, len, node)| (*len, *node))
            .map(|(_, _, node)| *node)
    }

    /// Number of files indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no file is indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// How to collect coverage.
#[derive(Clone, Debug)]
pub struct CoverageOptions {
    /// Packages whose tests run; `None` is the whole workspace.
    pub packages: Option<BTreeSet<String>>,
    /// Target directory for the instrumented build (kept apart from
    /// ordinary builds, whose flags differ).
    pub target_dir: PathBuf,
    /// Tests run at once.
    pub jobs: usize,
    /// Kill a single test after this long; it is recorded as failed.
    pub test_timeout: Duration,
    /// Tests not to run (quarantined).
    pub skip: BTreeSet<TestRef>,
    /// Run only these tests (an instrumented verification run of a
    /// selection, ADR 0022); `None` runs every test in scope.
    pub only: Option<TestFilter>,
    /// Source lines (1-based, per file) whose per-test execution to report
    /// in [`CoverageRun::lines`]: region-level data for chosen lines, such
    /// as the lines a change edits. Empty: none (the default; cheaper).
    pub lines_of_interest: BTreeMap<RepoPath, BTreeSet<u32>>,
    /// Kills the running build or test when set.
    pub cancel: Cancel,
}

impl Default for CoverageOptions {
    /// Whole workspace, `target/hord-coverage`, one job per available CPU,
    /// a 10-minute test timeout, nothing skipped or filtered.
    fn default() -> Self {
        Self {
            packages: None,
            target_dir: PathBuf::from("target/hord-coverage"),
            jobs: std::thread::available_parallelism().map_or(1, usize::from),
            test_timeout: Duration::from_secs(600),
            skip: BTreeSet::new(),
            only: None,
            lines_of_interest: BTreeMap::new(),
            cancel: Cancel::default(),
        }
    }
}

/// Which tests an instrumented run runs: a selection, as
/// [`TestFilter::from_selection`] maps it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestFilter {
    /// These tests.
    pub tests: BTreeSet<TestRef>,
    /// Every test of these packages.
    pub packages: BTreeSet<String>,
    /// Tests of a package whose names contain a filter (tests the record
    /// does not know yet).
    pub names: BTreeMap<String, BTreeSet<String>>,
    /// Every test.
    pub all: bool,
}

impl TestFilter {
    /// The tests `selection` runs (its doctests are not instrumented and
    /// run separately).
    #[must_use]
    pub fn from_selection(selection: &crate::Selection) -> Self {
        let mut tests = BTreeSet::new();
        for ((package, target), names) in &selection.exact {
            for name in names {
                tests.insert(TestRef {
                    package: package.clone(),
                    target: target.clone(),
                    name: name.clone(),
                });
            }
        }
        Self {
            tests,
            packages: selection.packages.clone(),
            names: selection.filters.clone(),
            all: selection.full,
        }
    }

    /// Whether it selects `test`.
    #[must_use]
    pub fn contains(&self, test: &TestRef) -> bool {
        self.all
            || self.packages.contains(&test.package)
            || self.tests.contains(test)
            || self
                .names
                .get(&test.package)
                .is_some_and(|fs| fs.iter().any(|f| test.name.contains(f.as_str())))
    }

    /// Whether it selects nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.all && self.tests.is_empty() && self.packages.is_empty() && self.names.is_empty()
    }

    /// The packages whose test binaries must be built.
    #[must_use]
    pub fn packages(&self) -> Option<BTreeSet<String>> {
        if self.all {
            return None;
        }
        let mut out = self.packages.clone();
        out.extend(self.tests.iter().map(|t| t.package.clone()));
        out.extend(self.names.keys().cloned());
        Some(out)
    }
}

/// A finished coverage run.
#[derive(Clone, Debug)]
pub struct CoverageRun {
    /// The record.
    pub record: CoverageRecord,
    /// Wall-clock milliseconds, build included.
    pub elapsed_ms: u64,
    /// The command recorded as the evidence's `command`.
    pub command: String,
    /// Progress notes and failures, for the evidence log.
    pub log: String,
    /// Per test, the [`CoverageOptions::lines_of_interest`] it executed
    /// (a line counts when `llvm-cov` gives it a nonzero count). Tests
    /// that executed none are absent.
    pub lines: BTreeMap<TestRef, BTreeMap<RepoPath, BTreeSet<u32>>>,
}

/// Executed lines of one test, restricted to the lines of interest.
type TestLines = BTreeMap<RepoPath, BTreeSet<u32>>;

/// What [`run_one`] needs to report lines of interest.
struct LineQuery {
    /// Every instrumented executable, for `llvm-cov export`.
    objects: Vec<PathBuf>,
    /// Absolute source path, its repository path, and the lines wanted.
    files: Vec<(PathBuf, RepoPath, BTreeSet<u32>)>,
    /// Some test already kept an indexed profile for the export mapping.
    have_export: std::sync::atomic::AtomicBool,
}

/// One test binary from the instrumented build.
#[derive(Clone, Debug)]
struct TestExe {
    path: PathBuf,
    package: String,
    target: TestTarget,
    /// Crate name inside the binary (`-` → `_`).
    krate: String,
}

/// The environment `cargo llvm-cov show-env` prints (`KEY=VALUE`, values
/// single-quoted when needed).
fn llvm_cov_env(root: &Path) -> Result<BTreeMap<String, String>> {
    let out = Command::new("cargo")
        .args(["llvm-cov", "show-env"])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(Error::tool(
            LLVM_COV,
            "`cargo llvm-cov show-env` failed; is it installed?",
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_owned(), unquote(value.trim())))
        })
        .collect())
}

/// POSIX single-quote unquoting (`'a'\''b'` → `a'b`).
fn unquote(value: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => quoted = !quoted,
            '\\' if !quoted => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Where `llvm-profdata` and `llvm-cov` of the active toolchain live.
fn llvm_bin(root: &Path) -> Result<PathBuf> {
    let text = |args: &[&str]| -> Result<String> {
        let out = Command::new("rustc")
            .args(args)
            .current_dir(root)
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let sysroot = text(&["--print", "sysroot"])?;
    let host = text(&["-vV"])?
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| Error::tool("rustc", "no host in `rustc -vV`"))?;
    let bin = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin");
    if !bin.join("llvm-profdata").exists() && !bin.join("llvm-profdata.exe").exists() {
        return Err(Error::tool(
            "llvm-tools",
            format!(
                "no llvm-profdata in {}; `rustup component add llvm-tools`",
                bin.display()
            ),
        ));
    }
    Ok(bin)
}

fn scope_args(packages: &Option<BTreeSet<String>>) -> Vec<String> {
    match packages {
        None => vec!["--workspace".into()],
        Some(names) => names
            .iter()
            .flat_map(|n| ["-p".to_owned(), n.clone()])
            .collect(),
    }
}

#[derive(Deserialize)]
struct Artifact {
    reason: String,
    #[serde(default)]
    executable: Option<String>,
    #[serde(default)]
    manifest_path: Option<String>,
    #[serde(default)]
    target: Option<ArtifactTarget>,
    #[serde(default)]
    profile: Option<ArtifactProfile>,
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct ArtifactProfile {
    test: bool,
}

/// Collect per-test coverage of `checkout` (see the module docs).
///
/// `defs` must hold the definitions of the checkout's snapshot.
pub fn collect(
    checkout: &Checkout,
    toolchain: &Toolchain,
    defs: &DefinitionIndex,
    options: &CoverageOptions,
) -> Result<CoverageRun> {
    let start = Instant::now();
    match build_suite(checkout, options)? {
        Some(suite) => run_suite(&suite, toolchain, defs, options, start),
        None => Ok(CoverageRun {
            record: CoverageRecord::new(
                checkout.snapshot,
                toolchain.id()?,
                BTreeSet::new(),
                Vec::new(),
            ),
            elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            command: "cargo llvm-cov (per test): nothing selected".into(),
            log: String::new(),
            lines: BTreeMap::new(),
        }),
    }
}

/// An instrumented build of a checkout's test binaries, with every test
/// listed and its run environment captured: steps 1 and 2 of [`collect`].
/// Building does not depend on which tests will run, so a caller can build
/// the next snapshot while [`run_suite`] runs the current one (they need
/// separate [`CoverageOptions::target_dir`]s).
#[derive(Debug)]
pub struct BuiltSuite {
    snapshot: hord_core::SnapshotId,
    /// The checkout root as given, and canonicalized.
    checkout_root: PathBuf,
    root: PathBuf,
    bin: PathBuf,
    work: PathBuf,
    objects: Vec<PathBuf>,
    binaries: usize,
    /// Every non-ignored test: its ref, environment, and binary.
    tests: Vec<(TestRef, BTreeMap<String, String>, PathBuf)>,
    crate_of: HashMap<TestRef, String>,
    scope: Vec<String>,
    log: String,
    /// Milliseconds the build and listing took.
    pub build_ms: u64,
}

impl BuiltSuite {
    /// Number of tests listed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tests.len()
    }

    /// Whether no test was listed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }
}

/// Steps 1 and 2 of [`collect`] for `checkout`: build the test binaries of
/// [`CoverageOptions::packages`] (narrowed to what `options.only` needs)
/// instrumented, list their tests, and capture the environment each runs
/// in. `None` when nothing is selected, so nothing is built.
pub fn build_suite(checkout: &Checkout, options: &CoverageOptions) -> Result<Option<BuiltSuite>> {
    build_suite_with_env(checkout, options, &BTreeMap::new())
}

/// [`build_suite`] with `extra_env` set for the build and for every test
/// process the suite later runs: configuration of the repository's own test
/// suite (a switch its tests read), not hord's.
pub fn build_suite_with_env(
    checkout: &Checkout,
    options: &CoverageOptions,
    extra_env: &BTreeMap<String, String>,
) -> Result<Option<BuiltSuite>> {
    let start = Instant::now();
    let root = checkout.root.canonicalize()?;
    let workspace = CargoWorkspace::load(&root)?;
    let bin = llvm_bin(&root)?;
    let target_dir = &options.target_dir;
    fs::create_dir_all(target_dir)?;
    let work = target_dir.join("hord-coverage");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(work.join("trash"))?;
    fs::create_dir_all(work.join("capture"))?;
    let mut env = llvm_cov_env(&root)?;
    for key in [
        "CARGO_TARGET_DIR",
        "CARGO_LLVM_COV_TARGET_DIR",
        "CARGO_LLVM_COV_BUILD_DIR",
    ] {
        env.insert(key.into(), target_dir.display().to_string());
    }
    env.insert(
        "LLVM_PROFILE_FILE".into(),
        work.join("trash/%p-%m.profraw").display().to_string(),
    );
    let mut log = String::new();

    // 1. Build.
    let packages = match &options.only {
        Some(filter) if filter.is_empty() => {
            // Nothing selected: nothing to build or run.
            return Ok(None);
        }
        Some(filter) => match (filter.packages(), &options.packages) {
            (Some(needed), Some(limit)) => Some(needed.intersection(limit).cloned().collect()),
            (Some(needed), None) => Some(needed),
            (None, limit) => limit.clone(),
        },
        None => options.packages.clone(),
    };
    // Tests of packages the workspace no longer has cannot run.
    let packages = packages.map(|ps| {
        ps.into_iter()
            .filter(|p| workspace.packages.contains_key(p))
            .collect::<BTreeSet<_>>()
    });
    if packages.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(None);
    }
    let scope = scope_args(&packages);
    let mut build = Command::new("cargo");
    build
        .arg("test")
        .args(&scope)
        .args([
            "--tests",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&root)
        .envs(&env)
        .envs(extra_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let built = run_captured(build, None, None, &options.cancel)?;
    if built.cancelled {
        return Err(Error::Cancelled);
    }
    if !built.success() {
        return Err(Error::tool("cargo test --no-run", built.stderr));
    }
    let mut exes = Vec::new();
    let mut objects = Vec::new();
    for line in built.stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else {
            continue;
        };
        let (Some(exe), Some(target), Some(profile), Some(manifest)) =
            (a.executable, a.target, a.profile, a.manifest_path)
        else {
            continue;
        };
        if a.reason != "compiler-artifact" {
            continue;
        }
        objects.push(PathBuf::from(&exe));
        if !profile.test {
            continue;
        }
        let manifest_dir = Path::new(&manifest).parent().unwrap_or(Path::new(""));
        let rel = manifest_dir.strip_prefix(&root).unwrap_or(manifest_dir);
        let Some(package) = workspace
            .packages
            .values()
            .find(|p| Path::new(&p.dir.to_string()) == rel)
        else {
            continue;
        };
        let kind = target.kind.first().cloned().unwrap_or_default();
        let kind = if kind.ends_with("lib") || kind == "proc-macro" {
            "lib".to_owned()
        } else {
            kind
        };
        exes.push(TestExe {
            path: PathBuf::from(&exe),
            package: package.name.clone(),
            krate: target.name.replace('-', "_"),
            target: TestTarget {
                kind,
                name: target.name,
            },
        });
    }
    objects.sort();
    objects.dedup();

    // 2. Environment and test lists, through a runner script.
    let script = work.join("capture.sh");
    fs::write(
        &script,
        "#!/bin/sh\nexe=\"$1\"; shift\nname=$(basename \"$exe\")\n\
         env > \"$HORD_CAPTURE_DIR/$name.env\"\n\
         \"$exe\" --list --format terse > \"$HORD_CAPTURE_DIR/$name.list\" 2>/dev/null\n\
         \"$exe\" --list --format terse --ignored > \"$HORD_CAPTURE_DIR/$name.ignored\" 2>/dev/null\n\
         exit 0\n",
    )?;
    make_executable(&script)?;
    let mut list = Command::new("cargo");
    list.arg("test")
        .args(&scope)
        .args(["--tests", "--no-fail-fast", "--config"])
        .arg(format!(
            "target.'cfg(all())'.runner = ['{}']",
            script.display()
        ))
        .current_dir(&root)
        .envs(&env)
        .envs(extra_env)
        .env("HORD_CAPTURE_DIR", work.join("capture"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let listed = run_captured(list, None, None, &options.cancel)?;
    if listed.cancelled {
        return Err(Error::Cancelled);
    }
    if !listed.success() {
        return Err(Error::tool("cargo test --list", listed.stderr));
    }
    let mut tests_out: Vec<(TestRef, BTreeMap<String, String>, PathBuf)> = Vec::new();
    let mut crate_of: HashMap<TestRef, String> = HashMap::new();
    for exe in &exes {
        let name = exe
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let capture = work.join("capture");
        let Ok(env_text) = fs::read_to_string(capture.join(format!("{name}.env"))) else {
            log.push_str(&format!(
                "no environment captured for {}\n",
                exe.path.display()
            ));
            continue;
        };
        let mut run_env: BTreeMap<String, String> = BTreeMap::new();
        let mut last: Option<String> = None;
        for line in env_text.lines() {
            match line.split_once('=') {
                Some((k, v))
                    if !k.is_empty()
                        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
                {
                    run_env.insert(k.to_owned(), v.to_owned());
                    last = Some(k.to_owned());
                }
                _ => {
                    if let Some(k) = &last
                        && let Some(v) = run_env.get_mut(k)
                    {
                        v.push('\n');
                        v.push_str(line);
                    }
                }
            }
        }
        // The instrumented build's own settings must not reach the tests:
        // cargo's tests run nested cargo builds, which would pick up the
        // coverage `RUSTC_WRAPPER` and target directory.
        for key in env.keys() {
            if key != "LLVM_PROFILE_FILE" {
                run_env.remove(key);
            }
        }
        for key in [
            "HORD_CAPTURE_DIR",
            "LLVM_PROFILE_FILE",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
        ] {
            run_env.remove(key);
        }
        let cwd = run_env
            .get("CARGO_MANIFEST_DIR")
            .map_or_else(|| root.clone(), PathBuf::from);
        let ignored: BTreeSet<String> = fs::read_to_string(capture.join(format!("{name}.ignored")))
            .map(|t| parse_list(&t).into_iter().collect())
            .unwrap_or_default();
        let tests = fs::read_to_string(capture.join(format!("{name}.list")))
            .map(|t| parse_list(&t))
            .unwrap_or_default();
        for test in tests {
            if ignored.contains(&test) {
                continue;
            }
            let test_ref = TestRef {
                package: exe.package.clone(),
                target: exe.target.clone(),
                name: test,
            };
            crate_of.insert(test_ref.clone(), exe.krate.clone());
            let mut env = run_env.clone();
            env.insert("__HORD_CWD".into(), cwd.display().to_string());
            tests_out.push((test_ref, env, exe.path.clone()));
        }
    }
    Ok(Some(BuiltSuite {
        snapshot: checkout.snapshot,
        checkout_root: checkout.root.clone(),
        root,
        bin,
        work,
        objects,
        binaries: exes.len(),
        tests: tests_out,
        crate_of,
        scope,
        log,
        build_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

/// Steps 3 and 4 of [`collect`]: run the tests of `suite` that `options`
/// selects (`only`, `skip`), each alone under coverage, and map what they
/// ran to `defs` (the definitions of the suite's snapshot). `start` is when
/// the caller's run began, for [`CoverageRun::elapsed_ms`].
pub fn run_suite(
    suite: &BuiltSuite,
    toolchain: &Toolchain,
    defs: &DefinitionIndex,
    options: &CoverageOptions,
    start: Instant,
) -> Result<CoverageRun> {
    let (root, bin, work, objects, scope) = (
        &suite.root,
        &suite.bin,
        &suite.work,
        &suite.objects,
        &suite.scope,
    );
    let mut log = suite.log.clone();
    let crate_of = &suite.crate_of;
    let mut jobs: Vec<(usize, TestRef, BTreeMap<String, String>, PathBuf)> = Vec::new();
    for (test_ref, env, exe) in &suite.tests {
        if options.skip.contains(test_ref)
            || options.only.as_ref().is_some_and(|f| !f.contains(test_ref))
        {
            continue;
        }
        jobs.push((jobs.len(), test_ref.clone(), env.clone(), exe.clone()));
    }
    log.push_str(&format!(
        "{} test binaries, {} tests\n",
        suite.binaries,
        jobs.len()
    ));

    // 3. Run each test alone.
    let query = Arc::new(LineQuery {
        objects: objects.clone(),
        files: options
            .lines_of_interest
            .iter()
            .map(|(p, l)| (root.join(p.to_string()), p.clone(), l.clone()))
            .collect(),
        have_export: std::sync::atomic::AtomicBool::new(false),
    });
    let lines: Arc<Mutex<Vec<Option<TestLines>>>> = Arc::new(Mutex::new(vec![None; jobs.len()]));
    let names = Arc::new(Mutex::new(Interner::default()));
    let results: Arc<Mutex<Vec<Option<TestOutcome>>>> =
        Arc::new(Mutex::new(vec![None; jobs.len()]));
    let export_profile: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let next = Arc::new(AtomicUsize::new(0));
    let jobs = Arc::new(jobs);
    let failures = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..options.jobs.max(1) {
        let (jobs, names, results, next, failures) = (
            Arc::clone(&jobs),
            Arc::clone(&names),
            Arc::clone(&results),
            Arc::clone(&next),
            Arc::clone(&failures),
        );
        let export_profile = Arc::clone(&export_profile);
        let (query, lines) = (Arc::clone(&query), Arc::clone(&lines));
        let (work, bin, timeout) = (work.clone(), bin.clone(), options.test_timeout);
        let cancel = options.cancel.clone();
        handles.push(thread::spawn(move || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((id, test, env, exe)) = jobs.get(i) else {
                    break;
                };
                if cancel.is_cancelled() {
                    break;
                }
                let outcome = run_one(
                    &work, &bin, *id, test, env, exe, timeout, &cancel, &names, &query,
                );
                match outcome {
                    Ok((covered, passed, profile, hit)) => {
                        if !hit.is_empty() {
                            lock(&lines)[*id] = Some(hit);
                        }
                        if let Some(profile) = profile {
                            let mut slot = lock(&export_profile);
                            if slot.is_none() {
                                *slot = Some(profile);
                                query.have_export.store(true, Ordering::Relaxed);
                            } else {
                                let _ = fs::remove_file(profile);
                            }
                        }
                        if !passed {
                            lock(&failures).push(format!("{}: failed under coverage", test.name));
                        }
                        lock(&results)[*id] = Some((covered, !passed));
                    }
                    Err(err) => {
                        lock(&failures).push(format!("{}: {err}", test.name));
                        lock(&results)[*id] = Some((BTreeSet::new(), true));
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    for f in lock(&failures).iter() {
        log.push_str(f);
        log.push('\n');
    }

    // 4. Map functions to definitions.
    // No test ran (an empty selection) or none left a profile: no function
    // mapping, and every test's coverage is empty.
    let functions = match lock(&export_profile).clone() {
        Some(profile) => export_functions(bin, &profile, objects)?,
        None if jobs.is_empty() => Vec::new(),
        None => return Err(Error::tool("llvm-profdata", "no test produced a profile")),
    };
    let mut node_of_name: HashMap<String, NodeId> = HashMap::new();
    let mut node_of_path: HashMap<String, NodeId> = HashMap::new();
    let mut instrumented = BTreeSet::new();
    for f in &functions {
        let Some(region) = f.regions.first() else {
            continue;
        };
        let (Some(start), Some(end), Some(file)) = (region.first(), region.get(2), region.get(5))
        else {
            continue;
        };
        let Some(file) = usize::try_from(*file).ok().and_then(|i| f.filenames.get(i)) else {
            continue;
        };
        let Some(path) = repo_path(root, &suite.checkout_root, file) else {
            continue;
        };
        let lines = u32::try_from(*start).unwrap_or(0)..u32::try_from(*end).unwrap_or(0) + 1;
        let Some(node) = defs.innermost(&path, &lines) else {
            continue;
        };
        instrumented.insert(node);
        let name = profile_name(&f.name);
        node_of_name.insert(name.to_owned(), node);
        let demangled = format!("{:#}", rustc_demangle::demangle(name));
        node_of_path.entry(demangled).or_insert(node);
    }
    let names = lock(&names);
    let results = lock(&results);
    let lines = lock(&lines);
    let per_test_lines: BTreeMap<TestRef, TestLines> = jobs
        .iter()
        .filter_map(|(id, test, _, _)| lines[*id].clone().map(|l| (test.clone(), l)))
        .collect();
    let mut tests = Vec::new();
    for (id, test, _, _) in jobs.iter() {
        let (covered, failed) = results[*id].clone().unwrap_or_default();
        let covers: BTreeSet<NodeId> = covered
            .iter()
            .filter_map(|n| names.names.get(*n as usize))
            .filter_map(|name| node_of_name.get(name.as_str()).copied())
            .collect();
        let own = crate_of
            .get(test)
            .and_then(|k| node_of_path.get(&format!("{k}::{}", test.name)).copied());
        tests.push((test.clone(), own, covers, failed));
    }
    let _ = fs::remove_dir_all(work);
    let record = CoverageRecord::new(suite.snapshot, toolchain.id()?, instrumented, tests);
    // Tests a cancel killed (or never started) say nothing: no run.
    if options.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(CoverageRun {
        command: format!(
            "cargo llvm-cov (per test) cargo test {} --tests -- --exact",
            scope.join(" ")
        ),
        record,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        log,
        lines: per_test_lines,
    })
}

/// Store `run`'s record and its `Custom("coverage")` evidence; return the
/// evidence id.
pub fn record_evidence(
    run: &CoverageRun,
    index: &dyn EvidenceIndex,
    actor: Actor,
) -> Result<ObjectId> {
    let log = run.record.put(index)?;
    let evidence = EvidenceFields {
        kind: EvidenceKind::Custom(COVERAGE_KIND.to_owned()),
        qualifier: None,
        snapshot: run.record.snapshot,
        toolchain: run.record.toolchain,
        command: run.command.clone(),
        scope: None,
        result: EvidenceResult::Pass,
        log: Some(log),
        cost_ms: run.elapsed_ms,
        produced_by: actor,
        produced_at: now(),
    }
    .build();
    index.put_evidence(&evidence)
}

/// Interned names of the functions a test entered, and whether it failed.
type TestOutcome = (BTreeSet<u32>, bool);

#[derive(Default)]
struct Interner {
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(name.to_owned());
        self.ids.insert(name.to_owned(), id);
        id
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run one test with its own profile directory and return the functions
/// it entered, whether it passed, and its merged profile (kept for the
/// export step) when it produced one.
#[allow(clippy::too_many_arguments)]
fn run_one(
    work: &Path,
    bin: &Path,
    id: usize,
    test: &TestRef,
    env: &BTreeMap<String, String>,
    exe: &Path,
    timeout: Duration,
    cancel: &Cancel,
    names: &Mutex<Interner>,
    query: &LineQuery,
) -> Result<(BTreeSet<u32>, bool, Option<PathBuf>, TestLines)> {
    let dir = work.join(format!("t{id}"));
    fs::create_dir_all(&dir)?;
    let mut env = env.clone();
    let cwd = env
        .remove("__HORD_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| work.to_path_buf());
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", &test.name, "--test-threads=1", "-q"])
        .env_clear()
        .envs(&env)
        .env("LLVM_PROFILE_FILE", dir.join("%p-%m.profraw"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = run_captured(cmd, Some(timeout), None, cancel)?;
    let raws: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
        .collect();
    if raws.is_empty() {
        let _ = fs::remove_dir_all(&dir);
        return Ok((BTreeSet::new(), out.success(), None, TestLines::new()));
    }
    // Usually one `llvm-profdata merge --text` gives the entered functions
    // (one process per test instead of merge + show). An indexed profile is
    // made only when `llvm-cov` needs one: the export mapping (until a test
    // has provided it) and lines of interest.
    if query.files.is_empty() && query.have_export.load(Ordering::Relaxed) {
        let text = Command::new(bin.join("llvm-profdata"))
            .args(["merge", "-sparse", "--text", "-o", "-"])
            .args(&raws)
            .stderr(Stdio::null())
            .output()?;
        let _ = fs::remove_dir_all(&dir);
        if !text.status.success() {
            return Err(Error::tool(
                "llvm-profdata merge --text",
                format!("failed for {}", test.name),
            ));
        }
        let mut covered = BTreeSet::new();
        {
            let text = String::from_utf8_lossy(&text.stdout);
            let mut names = lock(names);
            for name in entered_functions_text(&text) {
                covered.insert(names.intern(name));
            }
        }
        return Ok((covered, out.success(), None, TestLines::new()));
    }
    let merged = work.join(format!("t{id}.profdata"));
    let status = Command::new(bin.join("llvm-profdata"))
        .args(["merge", "-sparse", "-o"])
        .arg(&merged)
        .args(&raws)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let _ = fs::remove_dir_all(&dir);
    if !status.success() {
        return Err(Error::tool(
            "llvm-profdata merge",
            format!("failed for {}", test.name),
        ));
    }
    let show = Command::new(bin.join("llvm-profdata"))
        .args(["show", "--all-functions"])
        .arg(&merged)
        .stderr(Stdio::null())
        .output()?;
    let text = String::from_utf8_lossy(&show.stdout);
    let mut covered = BTreeSet::new();
    {
        let mut names = lock(names);
        for name in entered_functions(&text) {
            covered.insert(names.intern(name));
        }
    }
    let hit = if query.files.is_empty() {
        TestLines::new()
    } else {
        executed_lines(bin, &merged, query)?
    };
    Ok((covered, out.success(), Some(merged), hit))
}

/// The lines of interest a profile executed (`llvm-cov export
/// -format=lcov`, restricted to the files of interest).
fn executed_lines(bin: &Path, profile: &Path, query: &LineQuery) -> Result<TestLines> {
    let Some((first, rest)) = query.objects.split_first() else {
        return Ok(TestLines::new());
    };
    let mut cmd = Command::new(bin.join("llvm-cov"));
    cmd.args([
        "export",
        "-format=lcov",
        "-skip-expansions",
        "-instr-profile",
    ])
    .arg(profile)
    .arg(first);
    for o in rest {
        cmd.arg("-object").arg(o);
    }
    for (abs, _, _) in &query.files {
        cmd.arg(abs);
    }
    let out = cmd.stderr(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::tool("llvm-cov export -format=lcov", "failed"));
    }
    Ok(parse_lcov(
        &String::from_utf8_lossy(&out.stdout),
        &query.files,
    ))
}

/// `DA:line,count` records with a nonzero count, per `SF:` file, kept when
/// the line is of interest.
fn parse_lcov(lcov: &str, files: &[(PathBuf, RepoPath, BTreeSet<u32>)]) -> TestLines {
    let mut out = TestLines::new();
    let mut current: Option<&(PathBuf, RepoPath, BTreeSet<u32>)> = None;
    for line in lcov.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            let path = Path::new(path);
            current = files.iter().find(|(abs, _, _)| {
                abs == path || abs.canonicalize().ok().as_deref() == Some(path)
            });
        } else if let Some(rest) = line.strip_prefix("DA:")
            && let Some((file, rel, wanted)) = current
        {
            let _ = file;
            let mut parts = rest.split(',');
            let (Some(n), Some(count)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(n), Ok(count)) = (n.parse::<u32>(), count.parse::<u64>()) else {
                continue;
            };
            if count > 0 && wanted.contains(&n) {
                out.entry(rel.clone()).or_default().insert(n);
            }
        } else if line == "end_of_record" {
            current = None;
        }
    }
    out
}

/// Functions with a nonzero entry count (first counter) in the text profile
/// format `llvm-profdata merge --text` prints: blocks of a name line, then
/// `# Func Hash:`, `# Num Counters:`, `# Counter Values:` sections.
fn entered_functions_text(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut name: Option<&str> = None;
    let mut want_count = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            name = None;
            want_count = false;
        } else if line == "# Counter Values:" {
            want_count = true;
        } else if line.starts_with('#') || line.starts_with(':') {
            want_count = false;
        } else if want_count {
            if let Some(n) = name.take()
                && line.parse::<u64>().is_ok_and(|c| c > 0)
            {
                out.push(n);
            }
            want_count = false;
        } else if name.is_none() && line.parse::<u64>().is_err() {
            name = Some(line);
        }
    }
    out
}

/// Functions with a nonzero entry count in `llvm-profdata show
/// --all-functions` output.
fn entered_functions(show: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut current: Option<&str> = None;
    for line in show.lines() {
        if let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
            && !name.starts_with(' ')
        {
            current = Some(name);
        } else if let Some(count) = line.trim().strip_prefix("Function count: ")
            && let Some(name) = current.take()
            && count.trim().parse::<u64>().is_ok_and(|c| c > 0)
        {
            out.push(name);
        }
    }
    out
}

/// A profile function name without the `file;` prefix local symbols get.
fn profile_name(name: &str) -> &str {
    name.rsplit(';').next().unwrap_or(name)
}

#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}

#[derive(Deserialize)]
struct ExportData {
    functions: Vec<ExportFunction>,
}

#[derive(Deserialize)]
struct ExportFunction {
    name: String,
    filenames: Vec<String>,
    regions: Vec<Vec<i64>>,
}

/// Every instrumented function of `objects`, with its regions.
fn export_functions(
    bin: &Path,
    profile: &Path,
    objects: &[PathBuf],
) -> Result<Vec<ExportFunction>> {
    let Some((first, rest)) = objects.split_first() else {
        return Ok(Vec::new());
    };
    let mut cmd = Command::new(bin.join("llvm-cov"));
    cmd.args([
        "export",
        "-format=text",
        "-skip-expansions",
        "-instr-profile",
    ])
    .arg(profile)
    .arg(first);
    for o in rest {
        cmd.arg("-object").arg(o);
    }
    let out = cmd.stderr(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::tool("llvm-cov export", "failed"));
    }
    let export: Export = serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::tool("llvm-cov export", e.to_string()))?;
    Ok(export.data.into_iter().flat_map(|d| d.functions).collect())
}

/// `file` relative to the checkout, whichever spelling of the root it uses.
fn repo_path(canonical_root: &Path, root: &Path, file: &str) -> Option<RepoPath> {
    let file = Path::new(file);
    let rel = file
        .strip_prefix(canonical_root)
        .or_else(|_| file.strip_prefix(root))
        .ok()?;
    let text = rel.to_string_lossy().replace('\\', "/");
    text.parse().ok()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Err(Error::tool(
        LLVM_COV,
        "per-test coverage needs a POSIX shell runner",
    ))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::NodeKind;

    use super::*;

    #[test]
    fn reads_executed_lines_of_interest_from_lcov() {
        let rel = RepoPath::from_str("src/lib.rs").expect("parse test path");
        let files = vec![(
            PathBuf::from("/w/src/lib.rs"),
            rel.clone(),
            [2, 3, 9].into_iter().collect(),
        )];
        let lcov = "SF:/w/src/lib.rs\nDA:2,5\nDA:3,0\nDA:4,1\nDA:9,1\nend_of_record\n\
                    SF:/w/src/other.rs\nDA:2,7\nend_of_record\n";
        let got = parse_lcov(lcov, &files);
        assert_eq!(got[&rel], [2, 9].into_iter().collect());
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn unquotes_shell_values() {
        assert_eq!(unquote("'a b'"), "a b");
        assert_eq!(unquote("'a'\\''b'"), "a'b");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn reads_entered_functions_from_the_text_profile() {
        let text = "_RNvA\n# Func Hash:\n12\n# Num Counters:\n2\n# Counter Values:\n5\n0\n\n\
                    src/x.rs;_RNvB\n# Func Hash:\n7\n# Num Counters:\n1\n# Counter Values:\n0\n\n\
                    _RNvC\n# Func Hash:\n9\n# Num Counters:\n3\n# Counter Values:\n1\n0\n4\n";
        assert_eq!(entered_functions_text(text), vec!["_RNvA", "_RNvC"]);
    }

    #[test]
    fn reads_entered_functions() {
        let show = "Counters:\n  _RNvA:\n    Hash: 0x1\n    Counters: 2\n    Function count: 3\n  \
                    _RNvB:\n    Hash: 0x2\n    Counters: 1\n    Function count: 0\n  src/x.rs;_RNvC:\n    \
                    Hash: 0x3\n    Counters: 1\n    Function count: 1\nInstrumentation level: Front-end\n";
        assert_eq!(entered_functions(show), vec!["_RNvA", "src/x.rs;_RNvC"]);
        assert_eq!(profile_name("src/x.rs;_RNvC"), "_RNvC");
    }

    #[test]
    fn innermost_definition_by_lines() {
        let path = RepoPath::from_str("src/lib.rs").expect("parse test path");
        let src = b"mod m {\n    fn f() {\n        1;\n    }\n}\n";
        let def = |node: u128, span: Range<usize>| Definition {
            node: NodeId::from_u128(node),
            path: path.clone(),
            kind: NodeKind::new("function_item"),
            name: None,
            span,
            parent: None,
        };
        let mut index = DefinitionIndex::new();
        index.add_file(&path, src, &[def(1, 0..45), def(2, 12..43)]);
        assert_eq!(index.innermost(&path, &(2..5)), Some(NodeId::from_u128(2)));
        assert_eq!(index.innermost(&path, &(1..6)), Some(NodeId::from_u128(1)));
        assert_eq!(index.innermost(&path, &(9..10)), None);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn repo_paths_strip_either_root() {
        let p = repo_path(
            Path::new("/private/tmp/x"),
            Path::new("/tmp/x"),
            "/tmp/x/src/a.rs",
        );
        assert_eq!(
            p,
            Some(RepoPath::from_str("src/a.rs").expect("parse test path"))
        );
        assert_eq!(repo_path(Path::new("/a"), Path::new("/a"), "/b/c.rs"), None);
    }
}
