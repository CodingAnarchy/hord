//! `harness/claude.sh`, the model command for a real-model pilot, without a
//! model: its usage extraction against a canned `claude -p --output-format
//! json` result, and the flags it passes, through a stub `claude`.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn harness_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("harness")
}

fn sample() -> TestResult<String> {
    Ok(std::fs::read_to_string(
        harness_dir().join("testdata/claude-result.json"),
    )?)
}

/// `claude.sh --extract-usage` on `input`, with or without jq.
fn extract(input: &str, jq: bool) -> TestResult<Output> {
    let mut command = Command::new("sh");
    command
        .arg(harness_dir().join("claude.sh"))
        .arg("--extract-usage")
        .env("HORD_M5_MODEL", "claude-sonnet-5")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !jq {
        command.env("HORD_M5_NO_JQ", "1");
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(input.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn has_jq() -> bool {
    Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// With jq when it is installed, and always with the fallback parser.
fn modes() -> Vec<bool> {
    let mut modes = vec![false];
    if has_jq() {
        modes.push(true);
    }
    modes
}

/// tokens = input + output + cache creation + cache reads; cost is
/// `total_cost_usd`; the model is the one that ran.
#[test]
fn usage_is_extracted_from_a_canned_result() -> TestResult {
    let want_tokens = 1520 + 2210 + 20480 + 151200;
    for jq in modes() {
        let out = extract(&sample()?, jq)?;
        assert!(out.status.success(), "jq {jq}: {out:?}");
        let usage: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        assert_eq!(usage["tokens"], want_tokens, "jq {jq}: {usage}");
        assert_eq!(usage["cost_usd"], 0.1873, "jq {jq}: {usage}");
        assert_eq!(usage["model"], "claude-sonnet-5", "jq {jq}: {usage}");
    }
    Ok(())
}

/// A result without usage fails loudly instead of reporting nothing.
#[test]
fn a_result_without_usage_fails_loudly() -> TestResult {
    let bare = r#"{"type":"result","is_error":false,"result":"done"}"#;
    for jq in modes() {
        let out = extract(bare, jq)?;
        assert!(!out.status.success(), "jq {jq}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("lacks usage"),
            "jq {jq}: {out:?}"
        );
    }
    Ok(())
}

/// Through a stub `claude`: the flags (no permission bypass, Bash only for
/// cargo, the attempt's cost cap), the prompt on stdin, the usage file
/// written, and a failing CLI failing the attempt.
#[test]
fn the_wrapper_runs_claude_with_limited_tools_and_writes_usage() -> TestResult {
    let dir = std::env::temp_dir().join(format!("hord-m5-claude-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let args_file = dir.join("args");
    let prompt_file = dir.join("prompt");
    let stub = dir.join("claude");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\ncat '{}'\nexit \"${{STUB_EXIT:-0}}\"\n",
            args_file.display(),
            prompt_file.display(),
            harness_dir().join("testdata/claude-result.json").display()
        ),
    )?;
    Command::new("chmod").arg("+x").arg(&stub).status()?;
    let usage_file = dir.join("usage.json");
    let run = |exit: &str| -> TestResult<Output> {
        let mut child = Command::new("sh")
            .arg(harness_dir().join("claude.sh"))
            .current_dir(&dir)
            .env("HORD_M5_CLAUDE", &stub)
            .env("HORD_REPLAY_USAGE", &usage_file)
            .env("HORD_REPLAY_COST_USD", "2.000000")
            .env("STUB_EXIT", exit)
            .env_remove("HORD_M5_MAX_TURNS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(b"Replay a change on a new base.")?;
        Ok(child.wait_with_output()?)
    };
    let out = run("0")?;
    assert!(out.status.success(), "{out:?}");
    let args: Vec<String> = std::fs::read_to_string(&args_file)?
        .lines()
        .map(str::to_owned)
        .collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };
    assert!(has("-p"), "{args:?}");
    assert_eq!(after("--model").as_deref(), Some("claude-sonnet-5"));
    assert_eq!(after("--output-format").as_deref(), Some("json"));
    assert_eq!(after("--permission-mode").as_deref(), Some("dontAsk"));
    assert_eq!(after("--max-budget-usd").as_deref(), Some("2.000000"));
    assert!(has("--restricted"), "{args:?}");
    assert!(!args.iter().any(|a| a.contains("dangerously")), "{args:?}");
    assert!(!has("--max-turns"), "only when HORD_M5_MAX_TURNS is set");
    let bash: Vec<&String> = args.iter().filter(|a| a.starts_with("Bash(")).collect();
    assert_eq!(
        bash,
        [
            "Bash(cargo check *)",
            "Bash(cargo test *)",
            "Bash(cargo build *)"
        ],
        "{args:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&prompt_file)?,
        "Replay a change on a new base."
    );
    let usage: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&usage_file)?)?;
    assert_eq!(usage["cost_usd"], 0.1873);

    let failed = run("3")?;
    assert!(!failed.status.success(), "{failed:?}");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// `claude.sh --extract-message`: the JSON result's final text, at most
/// 1,000 characters, with and without jq.
fn extract_message(input: &str, jq: bool) -> TestResult<Output> {
    let mut command = Command::new("sh");
    command
        .arg(harness_dir().join("claude.sh"))
        .arg("--extract-message")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !jq {
        command.env("HORD_M5_NO_JQ", "1");
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(input.as_bytes())?;
    Ok(child.wait_with_output()?)
}

#[test]
fn the_final_message_is_extracted_and_capped() -> TestResult {
    let long = "x".repeat(3000);
    let result = format!(
        r#"{{"type":"result","is_error":false,"result":"{long}","total_cost_usd":0.1,"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
    );
    for jq in modes() {
        let out = extract_message(&sample()?, jq)?;
        assert!(out.status.success(), "jq {jq}: {out:?}");
        assert_eq!(
            String::from_utf8(out.stdout)?.trim_end(),
            "Updated scale_plus_one to call scale(x, 2) and ran cargo test.",
            "jq {jq}"
        );
        let out = extract_message(&result, jq)?;
        assert_eq!(
            String::from_utf8(out.stdout)?.trim_end().chars().count(),
            1000,
            "jq {jq}"
        );
    }
    Ok(())
}
