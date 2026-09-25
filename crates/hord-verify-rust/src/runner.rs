//! Running cargo checks and recording them as [`Evidence`] (spec §3.6).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hord_core::{Actor, Evidence, EvidenceResult, ObjectId, SnapshotId, Timestamp};
use hord_verify::{Check, EvidenceFields, EvidenceIndex, Result, Toolchain, put_log};

use crate::cancel::Cancel;
use crate::libtest::TestReport;

/// Detect the Rust toolchain a checkout builds with (its
/// `rust-toolchain` file applies, since the tools run in `dir`).
///
/// Records `rustc -vV`'s first line, `cargo -V`, and `cargo llvm-cov
/// --version` when that is installed (ADR 0022: coverage-based selection
/// needs it; without it selection runs whole packages).
pub fn detect_toolchain(dir: &Path) -> Result<Toolchain> {
    let version = |program: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(program)
            .args(args)
            .current_dir(dir)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().next().map(|l| l.trim().to_owned())
    };
    let rustc = version("rustc", &["-vV"])
        .ok_or_else(|| hord_verify::Error::tool("rustc", "`rustc -vV` failed"))?;
    let cargo = version("cargo", &["-V"])
        .ok_or_else(|| hord_verify::Error::tool("cargo", "`cargo -V` failed"))?;
    let mut toolchain = Toolchain::new(hord_lang_rust::LANG)
        .with("rustc", rustc)
        .with("cargo", cargo);
    if let Some(cov) = version("cargo", &["llvm-cov", "--version"]) {
        toolchain = toolchain.with(crate::coverage::LLVM_COV, cov);
    }
    Ok(toolchain)
}

/// What one command printed and how it ended.
#[derive(Clone, Debug, Default)]
pub struct RunOutput {
    /// Exit code; `None` if killed by a signal or the timeout.
    pub code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Wall-clock milliseconds.
    pub elapsed_ms: u64,
    /// libtest totals and failures from stdout.
    pub report: TestReport,
    /// The build failed before any test ran.
    pub build_failed: bool,
    /// The timeout killed it.
    pub timed_out: bool,
    /// A [`crate::Cancel`] killed it: it says nothing about the change.
    pub cancelled: bool,
}

impl RunOutput {
    /// Whether the command succeeded.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }

    /// One-line failure summary, `None` on success.
    #[must_use]
    pub fn failure_summary(&self) -> Option<String> {
        if self.success() {
            return None;
        }
        if self.cancelled {
            return Some("cancelled: verification was stopped".to_owned());
        }
        if self.timed_out {
            // Name what the command was doing when it was killed (for
            // example cargo "Blocking waiting for file lock on ...").
            let source = if self.stderr.trim().is_empty() {
                &self.stdout
            } else {
                &self.stderr
            };
            let tail: Vec<&str> = source
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let last = &tail[tail.len().saturating_sub(5)..];
            return Some(if last.is_empty() {
                "timed out".to_owned()
            } else {
                format!("timed out; last output: {}", last.join(" | "))
            });
        }
        if self.build_failed {
            let first = self
                .stderr
                .lines()
                .find(|l| l.starts_with("error"))
                .unwrap_or("build failed");
            return Some(format!("build failed: {first}"));
        }
        if !self.report.failed.is_empty() {
            let names: Vec<&str> = self
                .report
                .failed
                .iter()
                .map(String::as_str)
                .take(20)
                .collect();
            let more = self.report.failed.len().saturating_sub(names.len());
            let tail = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            return Some(format!(
                "{} failed: {}{tail}",
                self.report.failed.len(),
                names.join(", ")
            ));
        }
        Some(format!("exit status {:?}", self.code))
    }

    /// The log stored with the evidence: stdout, then stderr.
    #[must_use]
    pub fn log(&self) -> String {
        format!("{}\n--- stderr ---\n{}", self.stdout, self.stderr)
    }
}

/// Runs [`Check`]s with cargo in a checkout.
#[derive(Clone, Debug)]
pub struct CargoRunner {
    /// Extra environment for every command.
    pub env: BTreeMap<String, String>,
    /// `CARGO_TARGET_DIR`, shared across checkouts so builds are
    /// incremental; `None` uses the checkout's own `target/`.
    pub target_dir: Option<PathBuf>,
    /// Kill a command that runs longer than this.
    pub timeout: Option<Duration>,
    /// Kill a command that prints nothing for this long (a hung test:
    /// libtest prints a line per finished test).
    pub idle_timeout: Option<Duration>,
    /// Who produces the evidence.
    pub actor: Actor,
    /// Kills the running command when set (the repository is shutting
    /// down).
    pub cancel: Cancel,
}

/// Default limit on one verification command: a stuck command must fail
/// the change, never wedge the lander.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Default limit on a command printing nothing (libtest prints a line per
/// finished test, cargo a line per compiled crate).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

impl Default for CargoRunner {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            target_dir: None,
            timeout: Some(DEFAULT_TIMEOUT),
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            actor: Actor::Agent {
                id: "hord-verify-rust".into(),
                model: String::new(),
                model_hash: hord_core::Bytes::default(),
                harness: "hord".into(),
            },
            cancel: Cancel::default(),
        }
    }
}

impl CargoRunner {
    /// Run `check` in `root` (its `dir` is relative to it).
    pub fn run(&self, root: &Path, check: &Check) -> Result<RunOutput> {
        let mut cmd = Command::new(&check.program);
        cmd.args(&check.args)
            .current_dir(root.join(check.dir.to_string()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &self.target_dir {
            cmd.env("CARGO_TARGET_DIR", dir);
        }
        cmd.envs(&self.env).envs(&check.env);
        run_captured(cmd, self.timeout, self.idle_timeout, &self.cancel)
    }

    /// The evidence for `check`'s `output`, with its log stored in `logs`.
    pub fn evidence(
        &self,
        snapshot: SnapshotId,
        toolchain: ObjectId,
        check: &Check,
        output: &RunOutput,
        logs: &dyn EvidenceIndex,
    ) -> Result<Evidence> {
        // A killed command says nothing about the change: no evidence.
        if output.cancelled {
            return Err(hord_verify::Error::Cancelled);
        }
        let log = put_log(logs, output.log().as_bytes())?;
        Ok(EvidenceFields {
            kind: check.kind.clone(),
            qualifier: check.qualifier.clone(),
            snapshot,
            toolchain,
            command: check.command(),
            scope: check.scope.clone(),
            result: match output.failure_summary() {
                None => EvidenceResult::Pass,
                Some(summary) => EvidenceResult::Fail { summary },
            },
            log: Some(log),
            cost_ms: output.elapsed_ms,
            produced_by: self.actor.clone(),
            produced_at: now(),
        }
        .build())
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Kill `child` and everything it started: its process group on Unix, its
/// process tree on Windows (which has no process groups), so a hung
/// grandchild cannot keep holding the output pipes.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    signal_group(child.id());
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// `kill` arguments that send SIGKILL to the process group `pgid`.
///
/// The `--` is required: without it, Linux's `kill` (procps) parses
/// `-2844` as options and signals process group `2`, and a PID starting
/// with `1` becomes `kill(-1, SIGKILL)`, which kills every process the
/// user owns. On CI that killed the GitHub runner itself.
#[cfg(unix)]
fn group_kill_args(pgid: u32) -> [String; 3] {
    ["-9".to_owned(), "--".to_owned(), format!("-{pgid}")]
}

/// SIGKILL the process group `pgid` (a child spawned as a group leader).
#[cfg(unix)]
fn signal_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(group_kill_args(pgid))
        .stderr(Stdio::null())
        .status();
}

pub(crate) fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    Timestamp::from_millis(ms)
}

/// Spawn `cmd` with piped output, read both pipes, and kill it after
/// `timeout` in all, or `idle` without output.
///
/// On Unix the command runs in its own process group and a kill takes the
/// whole group, so a hung grandchild (a test binary under `cargo test`)
/// dies too instead of holding the pipes open.
/// How long the output readers get after the command exits or is killed.
/// A descendant that escaped the kill (on Windows, one whose parent chain
/// `taskkill /T` cannot follow) can hold the pipes open indefinitely; the
/// run keeps what was read by then instead of waiting for it.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// Output read from one of the command's pipes, on its own thread.
struct PipeReader {
    buf: Arc<Mutex<Vec<u8>>>,
    finished: Receiver<()>,
}

impl PipeReader {
    /// The output, once the pipe closes or `deadline` passes.
    fn collect(self, deadline: Instant) -> String {
        let _ = self
            .finished
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

pub(crate) fn run_captured(
    mut cmd: Command,
    timeout: Option<Duration>,
    idle: Option<Duration>,
    cancel: &Cancel,
) -> Result<RunOutput> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let last_output = Arc::new(AtomicU64::new(0));
    let reader = |pipe: Option<Box<dyn Read + Send>>, last: Arc<AtomicU64>| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let shared = Arc::clone(&buf);
        thread::spawn(move || {
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = p.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    shared
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&chunk[..n]);
                    last.store(millis(start.elapsed()), Ordering::Relaxed);
                }
            }
            let _ = done.send(());
        });
        PipeReader { buf, finished }
    };
    let out_reader = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        Arc::clone(&last_output),
    );
    let err_reader = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        Arc::clone(&last_output),
    );
    let mut timed_out = false;
    let mut cancelled = false;
    let status: ExitStatus = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.is_cancelled() {
            cancelled = true;
            kill_group(&mut child);
            break child.wait()?;
        }
        let quiet = millis(start.elapsed()).saturating_sub(last_output.load(Ordering::Relaxed));
        if timeout.is_some_and(|t| start.elapsed() > t)
            || idle.is_some_and(|t| u128::from(quiet) > t.as_millis())
        {
            timed_out = true;
            kill_group(&mut child);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // The group leader exited: stop anything it left behind that still
    // holds the pipes.
    if !timed_out && !cancelled {
        #[cfg(unix)]
        signal_group(child.id());
    }
    let drained = Instant::now() + PIPE_GRACE;
    let stdout = out_reader.collect(drained);
    let stderr = err_reader.collect(drained);
    let report = TestReport::parse(&stdout);
    let code = if timed_out || cancelled {
        None
    } else {
        status.code()
    };
    let build_failed = code != Some(0)
        && report.binaries == 0
        && (stderr.contains("error: could not compile") || stderr.contains("error[E"));
    Ok(RunOutput {
        code,
        stdout,
        stderr,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        report,
        build_failed,
        timed_out,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
    use hord_core::{EvidenceKind, RepoPath};
    use hord_verify::MemoryIndex;

    use super::*;

    fn sh(script: &str) -> Check {
        Check {
            requirement: "test:selected".into(),
            kind: EvidenceKind::Test,
            qualifier: Some("selected".into()),
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            dir: RepoPath::default(),
            scope: None,
        }
    }

    #[test]
    fn a_cancel_kills_the_running_command_and_records_nothing() {
        let runner = CargoRunner {
            timeout: None,
            idle_timeout: None,
            ..CargoRunner::default()
        };
        let cancel = runner.cancel.clone();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        });
        let started = Instant::now();
        // A quiet grandchild (the backgrounded sleep) goes with its group.
        let check = sh("sleep 60 & wait");
        let out = runner
            .run(&std::env::temp_dir(), &check)
            .expect("run the shell check");
        stopper.join().expect("the stopper thread ends");
        assert!(out.cancelled && !out.success(), "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let index = MemoryIndex::new();
        let err = runner
            .evidence(
                ObjectId::from_bytes([1; 32]),
                ObjectId::from_bytes([2; 32]),
                &check,
                &out,
                &index,
            )
            .expect_err("a cancelled command is no evidence");
        assert!(matches!(err, hord_verify::Error::Cancelled), "{err}");
        // A command started after the cancel is killed at once.
        let again = runner
            .run(&std::env::temp_dir(), &sh("sleep 60"))
            .expect("run");
        assert!(again.cancelled);
    }

    #[test]
    fn records_pass_fail_and_timeouts() {
        let runner = CargoRunner {
            timeout: Some(Duration::from_millis(500)),
            ..CargoRunner::default()
        };
        let root = std::env::temp_dir();
        let index = MemoryIndex::new();
        let snap = ObjectId::from_bytes([1; 32]);
        let tc = ObjectId::from_bytes([2; 32]);

        let ok = sh("echo 'test result: ok. 2 passed; 0 failed; 0 ignored;'");
        let out = runner.run(&root, &ok).expect("run the shell check");
        assert!(out.success() && out.report.passed == 2);
        let ev = runner
            .evidence(snap, tc, &ok, &out, &index)
            .expect("record the evidence");
        assert_eq!(ev.result, EvidenceResult::Pass);
        assert_eq!(ev.command, ok.command());
        let log =
            hord_verify::get_log(&index, ev.log.expect("evidence carries a log")).expect("get log");
        assert!(
            String::from_utf8(log)
                .expect("output is UTF-8")
                .contains("2 passed")
        );

        let bad = sh(
            "echo 'test x::y ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed;'; exit 101",
        );
        let out = runner.run(&root, &bad).expect("run the shell check");
        let ev = runner
            .evidence(snap, tc, &bad, &out, &index)
            .expect("record the evidence");
        assert_eq!(
            ev.result,
            EvidenceResult::Fail {
                summary: "1 failed: x::y".into()
            }
        );

        let build = sh(
            "echo 'error[E0308]: mismatched types' >&2; echo 'error: could not compile `x`' >&2; exit 101",
        );
        let out = runner.run(&root, &build).expect("run the shell check");
        assert!(out.build_failed);
        assert!(
            out.failure_summary()
                .expect("failure summary")
                .starts_with("build failed: error[E0308]")
        );

        let idle = CargoRunner {
            idle_timeout: Some(Duration::from_millis(300)),
            ..CargoRunner::default()
        };
        let chatty = sh("for i in 1 2 3 4 5 6; do echo $i; sleep 0.1; done");
        assert!(
            idle.run(&root, &chatty)
                .expect("run the shell check")
                .success()
        );
        // A quiet grandchild holding the pipes is killed with its group.
        let hung = sh("echo start; sh -c 'sleep 30' ; echo never");
        let started = Instant::now();
        let out = idle.run(&root, &hung).expect("run the shell check");
        assert!(out.timed_out && started.elapsed() < Duration::from_secs(10));

        let slow = sh("sleep 5");
        let out = runner.run(&root, &slow).expect("run the shell check");
        assert!(out.timed_out && !out.success());
        assert_eq!(out.failure_summary().as_deref(), Some("timed out"));
        // A killed command that printed something names its last output.
        let noisy = sh("echo Blocking waiting for file lock; sleep 5");
        let out = runner.run(&root, &noisy).expect("run the shell check");
        assert_eq!(
            out.failure_summary().as_deref(),
            Some("timed out; last output: Blocking waiting for file lock")
        );
    }

    #[test]
    fn detects_this_toolchain() {
        let tc = detect_toolchain(Path::new(env!("CARGO_MANIFEST_DIR"))).expect("detect toolchain");
        assert!(tc.components["rustc"].starts_with("rustc "));
        assert!(tc.components["cargo"].starts_with("cargo "));
    }

    /// Regression: without `--`, procps `kill -9 -12345` signalled group 1,
    /// that is every process the user owns, and killed the CI runner.
    #[cfg(unix)]
    #[test]
    fn group_kill_separates_the_negative_pgid_from_options() {
        assert_eq!(group_kill_args(12345), ["-9", "--", "-12345"]);
        // A real group dies and nothing else is signalled.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30 & wait"]);
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().expect("spawn a process group");
        std::thread::sleep(Duration::from_millis(200));
        signal_group(child.id());
        let status = child.wait().expect("wait for the killed group leader");
        assert!(!status.success(), "the group leader was killed");
    }
}
