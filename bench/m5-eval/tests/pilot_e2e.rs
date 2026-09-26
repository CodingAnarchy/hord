//! The real-model path end to end, without a model: `hord-eval-m5 run
//! --harness-cmd harness/claude.sh` on one case, with a stub `claude` that
//! makes the case's resolution (the scripted harness) and prints the
//! wrapper's canned `claude -p --output-format json` result. The usage the
//! wrapper extracts must reach the report: per-attempt cost, model, and
//! tokens (ADR 0028, ADR 0029), and the run totals.
//!
//! Needs `hord` and `hord-replay-ref` beside `hord-eval-m5` (`cargo build
//! -p hord-cli -p hord-replay-ref`, which `cargo test --workspace` does).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

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

#[test]
fn the_claude_wrappers_usage_reaches_the_report() -> TestResult {
    beside_runner("hord")?;
    beside_runner("hord-replay-ref")?;
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness = manifest.join("harness");
    let corpus = manifest.join("../../corpora/m5/cases");
    let case = corpus.join("m5-001-same-tokens.toml");
    let out = std::env::temp_dir().join(format!("hord-m5-pilot-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let stub = out.join("claude");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncat >/dev/null\n'{}' scripted --case '{}' || exit 1\ncat '{}'\n",
            env!("CARGO_BIN_EXE_hord-eval-m5"),
            case.canonicalize()?.display(),
            harness.join("testdata/claude-result.json").display()
        ),
    )?;
    Command::new("chmod").arg("+x").arg(&stub).status()?;

    let run = Command::new(env!("CARGO_BIN_EXE_hord-eval-m5"))
        .args(["run", "--only", "m5-001", "--jobs", "1", "--cost-usd", "2"])
        .arg("--corpus")
        .arg(&corpus)
        .arg("--harness-cmd")
        .arg(harness.join("claude.sh"))
        .args(["--model", "claude-sonnet-5"])
        .arg("--out")
        .arg(&out)
        .env("HORD_M5_CLAUDE", &stub)
        .output()?;
    assert!(
        run.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("report.json"))?)?;
    let case = &report["cases"][0];
    assert_eq!(case["outcome"], "resolved_by_replay", "{case:#}");
    let attempt = &case["attempts"][0];
    assert_eq!(
        attempt["tokens"],
        1520 + 2210 + 20480 + 151200,
        "{attempt:#}"
    );
    assert_eq!(attempt["cost_usd"], 0.1873, "{attempt:#}");
    assert_eq!(attempt["model"], "claude-sonnet-5", "{attempt:#}");
    let summary = &report["summary"];
    assert_eq!(summary["total_cost_usd"], 0.1873, "{summary:#}");
    assert_eq!(summary["total_tokens"], 175_410, "{summary:#}");
    assert_eq!(summary["models"]["claude-sonnet-5"], 1, "{summary:#}");
    assert!(
        summary["attempt_seconds"].as_f64().is_some_and(|s| s > 0.0),
        "{summary:#}"
    );
    let text = String::from_utf8_lossy(&run.stdout);
    assert!(text.contains("spent: $0.1873"), "{text}");
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&out).status();
    let _ = std::fs::remove_dir_all(&out);
    Ok(())
}

/// The honest exit end to end: on a contradiction (m5-089), a stub
/// `claude` changes nothing and explains why. The case parks, the attempt
/// gave up with that explanation as its reason, and the review sheet shows
/// it to the person rating the conflict summary.
#[test]
fn a_models_explanation_reaches_the_attempt_and_the_review_sheet() -> TestResult {
    beside_runner("hord")?;
    beside_runner("hord-replay-ref")?;
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness = manifest.join("harness");
    let corpus = manifest.join("../../corpora/m5/cases");
    let out = std::env::temp_dir().join(format!("hord-m5-decline-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let stub = out.join("claude");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncat >/dev/null\ncat '{}'\n",
            harness.join("testdata/claude-decline.json").display()
        ),
    )?;
    Command::new("chmod").arg("+x").arg(&stub).status()?;
    let run = Command::new(env!("CARGO_BIN_EXE_hord-eval-m5"))
        .args([
            "run",
            "--only",
            "m5-089",
            "--jobs",
            "1",
            "--max-attempts",
            "1",
        ])
        .arg("--corpus")
        .arg(&corpus)
        .arg("--harness-cmd")
        .arg(harness.join("claude.sh"))
        .arg("--out")
        .arg(&out)
        .env("HORD_M5_CLAUDE", &stub)
        .output()?;
    assert!(
        run.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("report.json"))?)?;
    let case = &report["cases"][0];
    assert_eq!(case["outcome"], "parked", "{case:#}");
    let attempt = &case["attempts"][0];
    assert_eq!(attempt["outcome"], "gave_up", "{attempt:#}");
    let explanation = "I changed no files: the intents contradict.";
    assert!(
        attempt["detail"]
            .as_str()
            .is_some_and(|d| d.starts_with(explanation)),
        "{attempt:#}"
    );
    let review = std::fs::read_to_string(out.join("review.md"))?;
    assert!(review.contains(explanation), "{review}");
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&out).status();
    let _ = std::fs::remove_dir_all(&out);
    Ok(())
}
