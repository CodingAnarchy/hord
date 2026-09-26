//! Stopping `hord-eval-m5` stops what it started: SIGTERM while a replay
//! harness is running leaves no `hord serve`, `hord-replay-ref`, or harness
//! process behind (they used to keep running, and a model kept spending).
//!
//! Needs `hord` and `hord-replay-ref` beside `hord-eval-m5`, and `pgrep`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn beside_runner(name: &str) -> TestResult<PathBuf> {
    let runner = Path::new(env!("CARGO_BIN_EXE_hord-eval-m5"));
    let path = runner.parent().ok_or("the runner's directory")?.join(name);
    if !path.exists() {
        return Err(format!(
            "{} is missing: cargo build -p hord-cli -p hord-replay-ref",
            path.display()
        )
        .into());
    }
    Ok(path)
}

/// Pids of processes whose command line mentions `marker`.
fn processes_mentioning(marker: &str) -> TestResult<Vec<String>> {
    let out = Command::new("pgrep").args(["-f", marker]).output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

#[test]
fn stopping_the_runner_stops_its_servers_and_harnesses() -> TestResult {
    beside_runner("hord")?;
    beside_runner("hord-replay-ref")?;
    let dir = std::env::temp_dir().join(format!("hord-m5-cleanup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus)?;
    // m5-007's first attempt sleeps past any budget: the harness is
    // running when the runner is stopped. Its command line names this copy
    // of the case, so leftovers are found by the directory's path.
    let case = "m5-007-same-tokens.toml";
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpora/m5/cases")
            .join(case),
        corpus.join(case),
    )?;
    // The runner passes canonical paths (on macOS the temp dir is a symlink).
    let marker = dir
        .canonicalize()?
        .to_str()
        .ok_or("UTF-8 temp path")?
        .to_owned();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_hord-eval-m5"))
        .args([
            "run",
            "--only",
            "m5-007",
            "--jobs",
            "1",
            "--wall-time-secs",
            "300",
        ])
        .arg("--corpus")
        .arg(&corpus)
        .arg("--out")
        .arg(dir.join("out"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Wait for the sleeping harness (`hord-eval-m5 scripted --case <copy>`).
    let deadline = Instant::now() + Duration::from_secs(120);
    let scripted = format!("scripted --case {marker}");
    while processes_mentioning(&scripted)?.is_empty() {
        if Instant::now() > deadline || runner.try_wait()?.is_some() {
            // Stop it the way under test, so a failure leaves nothing behind.
            let _ = Command::new("kill")
                .args(["-TERM", &runner.id().to_string()])
                .status();
            let _ = runner.wait();
            return Err("the harness never started".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!processes_mentioning(&marker)?.is_empty());
    Command::new("kill")
        .args(["-TERM", &runner.id().to_string()])
        .status()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while runner.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = runner.kill();
            return Err("the runner did not stop on SIGTERM".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Everything it started is gone (give the kills a moment to land).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut left = processes_mentioning(&marker)?;
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        left = processes_mentioning(&marker)?;
    }
    let listing = Command::new("ps")
        .args(["-o", "pid,command", "-p"])
        .arg(left.join(","))
        .output();
    assert!(
        left.is_empty(),
        "left running: {}",
        listing
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    );
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&dir).status();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
