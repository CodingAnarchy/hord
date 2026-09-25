//! The reference replay harness (spec §6.6, §11): one attempt of the replay
//! protocol, shelling out to a configurable command.
//!
//! It reads one [`ReplayRequest`] line from stdin, runs the command in the
//! request's workspace with a prompt built from the change's intent and the
//! conflict summary, proposes what the command left there with `hord
//! propose`, and writes one [`ReplayResult`] line to stdout. Both lines are
//! the canonical protobuf JSON mapping of `hord.proto` (ADR 0024).
//!
//! The command runs through the platform shell, in the workspace directory,
//! with the prompt on stdin and in these variables:
//!
//! | variable | value |
//! |---|---|
//! | `HORD_REPLAY_PROMPT_FILE` | a file holding the prompt |
//! | `HORD_REPLAY_USAGE` | where it may write `{"tokens": N, "cost_usd": X, "model": "…"}` |
//! | `HORD_REPLAY_CHANGE` | the change being replayed |
//! | `HORD_REPLAY_ATTEMPT` | the attempt number |
//! | `HORD_WORKSPACE`, `HORD_WORKSPACE_PATH` | the workspace id and directory |
//! | `HORD_REPO` | the repository root |
//!
//! Its stdout goes to this process's stderr, so the protocol line stays
//! alone on stdout. A command that exits non-zero, or leaves nothing to
//! propose, gives up. Budget enforcement is the lander's (ADR 0028): it
//! kills this process when the attempt runs out of time, and rejects usage
//! over budget.
//!
//! Nothing in Hord depends on this crate; any process that speaks the
//! protocol is a harness.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use hord_api::proto::{self, ReplayRequest, ReplayResult, replay_result::Status};
use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};

/// Why an attempt could not run at all (as opposed to giving up).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a file, or starting a process, failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// The failure.
        source: std::io::Error,
    },
    /// A protocol line or a `hord` answer did not decode.
    #[error("{what}: {source}")]
    Json {
        /// What was being decoded.
        what: String,
        /// The failure.
        source: serde_json::Error,
    },
    /// The intent file could not be written as YAML.
    #[error("intent front matter: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// How to run an attempt.
#[derive(Clone, Debug)]
pub struct Options {
    /// The command, run by the platform shell in the workspace.
    pub cmd: String,
    /// The `hord` binary that proposes.
    pub hord: PathBuf,
    /// A model name to report when the command reports none (ADR 0029).
    pub model: Option<String>,
}

/// What the command may report in `HORD_REPLAY_USAGE`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    tokens: Option<u64>,
    cost_usd: Option<f64>,
    model: Option<String>,
}

/// The prompt for `request`: the intent to re-execute, its acceptance
/// criteria, what collided and why, and an arbiter's note.
#[must_use]
pub fn prompt(request: &ReplayRequest) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Replay a change on a new base (hord replay, attempt {}).",
        request.attempt
    );
    let _ = writeln!(
        text,
        "The change could not land as it was written, because other changes landed first. \
         Re-do its task in this directory, which is a checkout of the new base, keeping what \
         landed. Leave the result in the files; hord proposes it."
    );
    if let Some(intent) = &request.intent {
        let _ = writeln!(text, "\n## Task\n\n{}", intent.summary);
        if !intent.body.trim().is_empty() {
            let _ = writeln!(text, "\n{}", intent.body.trim());
        }
        if !intent.acceptance.is_empty() {
            let _ = writeln!(text, "\n## Acceptance");
            for item in &intent.acceptance {
                let _ = writeln!(text, "- {}: {}", item.kind, item.value);
            }
        }
    }
    if let Some(summary) = &request.summary {
        let _ = writeln!(text, "\n## What collided\n\n{}", summary.text.trim_end());
    } else if let Some(report) = &request.conflict {
        let _ = writeln!(text, "\n## What collided");
        for merge in &report.merge {
            let _ = writeln!(text, "- {}: {}", merge.path, merge.reason);
        }
        if let Some(why) = &report.verification {
            let _ = writeln!(text, "- verification failed: {why}");
        }
    }
    if let Some(note) = &request.note {
        let _ = writeln!(text, "\n## Note from the arbiter\n\n{note}");
    }
    text
}

/// The intent file `hord propose` gets: the original intent, with a ref to
/// the change being replayed (hord records `parent_intent` itself).
pub fn intent_file(request: &ReplayRequest) -> Result<String, Error> {
    let intent = request.intent.clone().unwrap_or_default();
    let tagged = |kind: &str, value: &str| {
        let mut map = Mapping::new();
        map.insert(Value::from(kind), Value::from(value));
        Value::Mapping(map)
    };
    let mut refs: Vec<Value> = intent
        .refs
        .iter()
        .map(|r| tagged(&r.kind, &r.value))
        .collect();
    refs.push(tagged("change", &request.change));
    let acceptance: Vec<Value> = intent
        .acceptance
        .iter()
        .map(|a| match a.kind.as_str() {
            "invariant" => Value::from(a.value.as_str()),
            kind => tagged(kind, &a.value),
        })
        .collect();
    let summary = if intent.summary.trim().is_empty() {
        format!("Replay of {}", request.change)
    } else {
        intent.summary.clone()
    };
    let mut front = Mapping::new();
    front.insert(Value::from("summary"), Value::from(summary));
    front.insert(Value::from("refs"), Value::Sequence(refs));
    front.insert(Value::from("acceptance"), Value::Sequence(acceptance));
    let yaml = serde_yaml_ng::to_string(&Value::Mapping(front))?;
    Ok(format!("---\n{yaml}---\n{}\n", intent.body))
}

/// A result that gives up for `reason`.
#[must_use]
pub fn gave_up(reason: impl Into<String>) -> ReplayResult {
    ReplayResult {
        status: Some(Status::GaveUp(proto::ReplayGaveUp {
            reason: reason.into(),
        })),
        ..Default::default()
    }
}

/// A scratch directory for one attempt, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, Error> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path =
            std::env::temp_dir().join(format!("hord-replay-ref-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(io(format!("create {}", path.display())))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shell(cmd: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", cmd]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", cmd]);
        command
    }
}

/// Run one attempt of `request`.
///
/// Returns the result to write: `Proposed` with the change `hord propose`
/// printed, or `GaveUp` when the command failed or changed nothing. An
/// error is a failure to run at all.
pub fn replay(request: &ReplayRequest, options: &Options) -> Result<ReplayResult, Error> {
    let scratch = Scratch::new()?;
    let prompt_text = prompt(request);
    let prompt_file = scratch.0.join("prompt.md");
    let usage_file = scratch.0.join("usage.json");
    std::fs::write(&prompt_file, &prompt_text).map_err(io("write the prompt"))?;
    let workspace = Path::new(&request.workspace_path);
    let mut child = shell(&options.cmd)
        .current_dir(workspace)
        .env("HORD_REPLAY_PROMPT_FILE", &prompt_file)
        .env("HORD_REPLAY_USAGE", &usage_file)
        .env("HORD_REPLAY_CHANGE", &request.change)
        .env("HORD_REPLAY_ATTEMPT", request.attempt.to_string())
        .env("HORD_WORKSPACE", &request.workspace)
        .env("HORD_WORKSPACE_PATH", &request.workspace_path)
        .env("HORD_REPO", &request.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(io(format!("start {:?}", options.cmd)))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that does not read its prompt closes the pipe early.
        let _ = stdin.write_all(prompt_text.as_bytes());
    }
    let status = child.wait().map_err(io("wait for the command"))?;
    let usage: Usage = match std::fs::read_to_string(&usage_file) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| Error::Json {
            what: "the command's usage report".into(),
            source,
        })?,
        Err(_) => Usage::default(),
    };
    let with_usage = |mut result: ReplayResult| {
        result.tokens = usage.tokens;
        result.cost_micros = usage
            .cost_usd
            .filter(|usd| usd.is_finite() && *usd >= 0.0)
            // Non-negative and finite: checked just above.
            .map(|usd| (usd * 1_000_000.0).round() as u64);
        result.model = usage.model.clone().or_else(|| options.model.clone());
        result
    };
    if !status.success() {
        return Ok(with_usage(gave_up(format!(
            "the command exited with {status}"
        ))));
    }
    let intent_path = scratch.0.join("intent.md");
    std::fs::write(&intent_path, intent_file(request)?).map_err(io("write the intent file"))?;
    let out = Command::new(&options.hord)
        .args(["propose", "-w", &request.workspace, "--intent"])
        .arg(&intent_path)
        .arg("--json")
        .current_dir(if request.repo.is_empty() {
            workspace
        } else {
            Path::new(&request.repo)
        })
        .stdin(Stdio::null())
        .output()
        .map_err(io(format!("run {}", options.hord.display())))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Ok(with_usage(gave_up(format!("hord propose failed: {why}"))));
    }
    let proposed: proto::ProposeResponse =
        serde_json::from_slice(&out.stdout).map_err(|source| Error::Json {
            what: "hord propose --json".into(),
            source,
        })?;
    Ok(with_usage(ReplayResult {
        status: Some(Status::Proposed(proto::ReplayProposed {
            change: proposed.change,
        })),
        ..Default::default()
    }))
}
