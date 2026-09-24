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
    let scope = scope_args(&options.packages);
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let built = run_captured(build, None, None)?;
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
        .env("HORD_CAPTURE_DIR", work.join("capture"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let listed = run_captured(list, None, None)?;
    if !listed.success() {
        return Err(Error::tool("cargo test --list", listed.stderr));
    }
    let mut jobs: Vec<(usize, TestRef, BTreeMap<String, String>, PathBuf)> = Vec::new();
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
            if options.skip.contains(&test_ref) {
                continue;
            }
            crate_of.insert(test_ref.clone(), exe.krate.clone());
            let mut env = run_env.clone();
            env.insert("__HORD_CWD".into(), cwd.display().to_string());
            jobs.push((jobs.len(), test_ref, env, exe.path.clone()));
        }
    }
    log.push_str(&format!(
        "{} test binaries, {} tests\n",
        exes.len(),
        jobs.len()
    ));

    // 3. Run each test alone.
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
        let (work, bin, timeout) = (work.clone(), bin.clone(), options.test_timeout);
        handles.push(thread::spawn(move || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((id, test, env, exe)) = jobs.get(i) else {
                    break;
                };
                let outcome = run_one(&work, &bin, *id, test, env, exe, timeout, &names);
                match outcome {
                    Ok((covered, passed, profile)) => {
                        if let Some(profile) = profile {
                            let mut slot = lock(&export_profile);
                            if slot.is_none() {
                                *slot = Some(profile);
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
    let profile = lock(&export_profile)
        .clone()
        .ok_or_else(|| Error::tool("llvm-profdata", "no test produced a profile"))?;
    let functions = export_functions(&bin, &profile, &objects)?;
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
        let Some(path) = repo_path(&root, &checkout.root, file) else {
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
    let _ = fs::remove_dir_all(&work);
    let record = CoverageRecord::new(checkout.snapshot, toolchain.id()?, instrumented, tests);
    Ok(CoverageRun {
        command: format!(
            "cargo llvm-cov (per test) cargo test {} --tests -- --exact",
            scope.join(" ")
        ),
        record,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        log,
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
    names: &Mutex<Interner>,
) -> Result<(BTreeSet<u32>, bool, Option<PathBuf>)> {
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
    let out = run_captured(cmd, Some(timeout), None)?;
    let raws: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
        .collect();
    if raws.is_empty() {
        let _ = fs::remove_dir_all(&dir);
        return Ok((BTreeSet::new(), out.success(), None));
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
    Ok((covered, out.success(), Some(merged)))
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
    fn unquotes_shell_values() {
        assert_eq!(unquote("'a b'"), "a b");
        assert_eq!(unquote("'a'\\''b'"), "a'b");
        assert_eq!(unquote("plain"), "plain");
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
        let path = RepoPath::from_str("src/lib.rs").unwrap();
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
        assert_eq!(p, Some(RepoPath::from_str("src/a.rs").unwrap()));
        assert_eq!(repo_path(Path::new("/a"), Path::new("/a"), "/b/c.rs"), None);
    }
}
