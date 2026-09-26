//! The pinned acceptance run (ADR 0034 amendment): before a replay lands,
//! every protected acceptance test runs from its protected source file,
//! overlaid on the replay's result, and must run and pass.
//!
//! Keeping a protected test's text is not enough: a replay can change what
//! the test exercises from elsewhere in its file (m5-090 added a `mod
//! fixture` stub that shadowed the crate), or keep the test from running
//! (a `Cargo.toml` target with `test = false`). So the lander checks the
//! replay's result out to a scratch directory, writes each protected test's
//! whole file as it is protected (head's version for the landed side's
//! tests, the original change's for its own), and runs `cargo test --tests
//! --no-fail-fast`: every test target the manifest enables, nothing
//! filtered, ignored tests not run. Each protected test must report `ok`
//! in its own target. Failed, ignored, or not run at all (a disabled
//! target, a build failure) makes the attempt `TAMPERED`. The lander's
//! usual verification still runs on the replay's result as proposed.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hord_core::{ChangeId, ChangeRecord, NodeId, RepoPath};

use crate::repo::{Inner, fs_path, now};
use crate::{Error, Result};

/// How long the pinned run may take before it counts as failed.
const LIMIT: Duration = Duration::from_secs(600);

/// How a protected test fared in the pinned run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ran {
    Passed,
    Failed,
    Ignored,
}

/// The pinned run's verdict on a replay (ADR 0034 as amended).
#[derive(Debug)]
pub(crate) enum Pinned {
    /// Every protected test ran and passed from its protected file (or
    /// nothing is protected).
    Passed,
    /// The replay changed what a protected test sees, or kept it from
    /// running: failing from its protected file but passing as the replay
    /// left it, ignored, or its target disabled. Each test with why.
    Tampered(Vec<(NodeId, String)>),
    /// The replay is simply wrong: these protected tests fail either way.
    /// An ordinary failure (the replay conflicts), not tampering.
    Fails(Vec<String>),
}

/// How one protected test fared in one run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fared {
    Passed,
    Failed,
    Ignored,
    /// Not run although the build succeeded: its target is disabled.
    Disabled,
    /// Not run: the build failed or the run timed out.
    Broken,
}

impl Inner {
    /// The pinned acceptance run of `record`, a replay of `of`: every
    /// protected test from its protected file, overlaid on the replay's
    /// result. A test that does not pass that way runs again as the replay
    /// left it, which tells tampering from a replay that is simply wrong.
    /// [`Pinned::Passed`] when nothing is protected or the result is not a
    /// Cargo package.
    pub(crate) fn pinned_check(&self, of: ChangeId, record: &ChangeRecord) -> Result<Pinned> {
        let expected = self.expected_tests(of, record.base)?;
        if expected.is_empty() {
            return Ok(Pinned::Passed);
        }
        let manifest: RepoPath = "Cargo.toml"
            .parse()
            .map_err(|_| Error::InvalidPath("Cargo.toml".into()))?;
        if self.blob_id(record.result, &manifest)?.is_none() {
            return Ok(Pinned::Passed);
        }
        let pinned = self.run_tests(record, &expected, true)?;
        let mut tampered = Vec::new();
        let mut suspects = Vec::new();
        for (test, (fared, why)) in expected.iter().zip(&pinned) {
            match fared {
                Fared::Passed => {}
                Fared::Ignored | Fared::Disabled => {
                    tampered.push((test.node, format!("{} {why}", test.name)));
                }
                Fared::Failed | Fared::Broken => suspects.push((test, why.clone())),
            }
        }
        if !suspects.is_empty() {
            let as_left = self.run_tests(record, &expected, false)?;
            let mut fails = Vec::new();
            for (test, why) in suspects {
                let passes_as_left = expected
                    .iter()
                    .zip(&as_left)
                    .any(|(t, (fared, _))| std::ptr::eq(t, test) && *fared == Fared::Passed);
                if passes_as_left {
                    tampered.push((
                        test.node,
                        format!(
                            "{} {why}, but passes as the replay left it: the replay changed what \
                             the test sees",
                            test.name
                        ),
                    ));
                } else {
                    fails.push(format!("{} {why}", test.name));
                }
            }
            if tampered.is_empty() {
                return Ok(Pinned::Fails(fails));
            }
        }
        if tampered.is_empty() {
            Ok(Pinned::Passed)
        } else {
            Ok(Pinned::Tampered(tampered))
        }
    }

    /// Run the tests of `record`'s result, with each protected test's file
    /// as protected when `overlay`, else as the replay left it. How each of
    /// `expected` fared, with why, in order.
    fn run_tests(
        &self,
        record: &ChangeRecord,
        expected: &[crate::escalation::Expected],
        overlay: bool,
    ) -> Result<Vec<(Fared, String)>> {
        let scratch = self.store.hord_dir().join("pinned").join(format!(
            "{}-{}-{}",
            record.result.to_hex(),
            if overlay { "pinned" } else { "as-left" },
            now().as_millis()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let outcome = self.run_tests_in(record, expected, overlay, &scratch);
        let _ = std::fs::remove_dir_all(&scratch);
        outcome
    }

    fn run_tests_in(
        &self,
        record: &ChangeRecord,
        expected: &[crate::escalation::Expected],
        overlay: bool,
        scratch: &Path,
    ) -> Result<Vec<(Fared, String)>> {
        let tree = scratch.join("tree");
        self.checkout(record.result, &tree)?;
        if overlay {
            // Each protected test's file, as it is protected.
            let mut overlaid: BTreeMap<&RepoPath, ()> = BTreeMap::new();
            for test in expected {
                if overlaid.insert(&test.path, ()).is_some() {
                    continue;
                }
                if let Some(bytes) = self.file_bytes(test.source, &test.path)? {
                    let target = fs_path(&tree, &test.path);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes.as_slice())?;
                }
            }
        }
        let log_path = scratch.join("cargo-test.log");
        let log = File::create(&log_path)?;
        let mut child = Command::new("cargo")
            .args(["test", "--tests", "--no-fail-fast"])
            .current_dir(&tree)
            .env(
                "CARGO_TARGET_DIR",
                self.store.hord_dir().join("pinned-target"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        let deadline = Instant::now() + LIMIT;
        let timed_out = loop {
            if child.try_wait()?.is_some() {
                break false;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let output = std::fs::read_to_string(&log_path).unwrap_or_default();
        let build_failed = output.contains("error: could not compile");
        let results = parse(&output);
        let whence = if overlay {
            "from its protected file"
        } else {
            "as the replay left it"
        };
        Ok(expected
            .iter()
            .map(|test| {
                let found = results.iter().find(|(target, name, _)| {
                    in_target(target, &test.path)
                        && (name == &test.name || name.ends_with(&format!("::{}", test.name)))
                });
                match found.map(|(_, _, ran)| *ran) {
                    Some(Ran::Passed) => (Fared::Passed, String::new()),
                    Some(Ran::Failed) => (Fared::Failed, format!("fails when run {whence}")),
                    Some(Ran::Ignored) => (Fared::Ignored, "is ignored".to_owned()),
                    None if timed_out => (
                        Fared::Broken,
                        format!("did not finish within {}s", LIMIT.as_secs()),
                    ),
                    None if build_failed => (
                        Fared::Broken,
                        format!("does not build {whence}: {}", last_error(&output)),
                    ),
                    None => (
                        Fared::Disabled,
                        "did not run: its test target is disabled".to_owned(),
                    ),
                }
            })
            .collect())
    }
}

/// Whether the cargo target `target` (as `Running <target> (...)` names
/// it) is the one `path`'s tests run in: `tests/x.rs` for an integration
/// test file, the crate's unit tests for a file under `src/`.
fn in_target(target: &str, path: &RepoPath) -> bool {
    let path = path.to_string();
    if path.starts_with("tests/") {
        target == path
    } else {
        target.starts_with("unittests ")
    }
}

/// `(target, test, result)` for every libtest result line in `output`,
/// under the `Running <target> (...)` line cargo printed before it.
fn parse(output: &str) -> Vec<(String, String, Ran)> {
    let mut out = Vec::new();
    let mut target = String::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Running ") {
            target = rest
                .rsplit_once(" (")
                .map_or(rest, |(name, _)| name)
                .to_owned();
            continue;
        }
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, result)) = rest.split_once(" ... ") else {
            continue;
        };
        let ran = if result == "ok" {
            Ran::Passed
        } else if result.starts_with("ignored") {
            Ran::Ignored
        } else if result.starts_with("FAILED") {
            Ran::Failed
        } else {
            continue;
        };
        out.push((target.clone(), name.to_owned(), ran));
    }
    out
}

/// The first `error` line of cargo's output, for a build that failed.
fn last_error(output: &str) -> String {
    output
        .lines()
        .find(|l| l.trim_start().starts_with("error"))
        .map_or_else(|| "no test output".to_owned(), |l| l.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libtest_output_is_read_per_target() {
        let output = "   Compiling fixture v0.1.0\n     Running unittests src/lib.rs (target/debug/deps/fixture-1)\n\nrunning 0 tests\n     Running tests/m5_a.rs (target/debug/deps/m5_a-2)\ntest beta_at_least_20 ... ok\n     Running tests/m5_b.rs (target/debug/deps/m5_b-3)\ntest beta_is_21 ... FAILED\ntest slow ... ignored, needs a network\n";
        assert_eq!(
            parse(output),
            vec![
                (
                    "tests/m5_a.rs".into(),
                    "beta_at_least_20".into(),
                    Ran::Passed
                ),
                ("tests/m5_b.rs".into(), "beta_is_21".into(), Ran::Failed),
                ("tests/m5_b.rs".into(), "slow".into(), Ran::Ignored),
            ]
        );
        let path: RepoPath = "tests/m5_b.rs".parse().expect("parse a literal path");
        assert!(in_target("tests/m5_b.rs", &path));
        assert!(!in_target("tests/m5_a.rs", &path));
    }
}
