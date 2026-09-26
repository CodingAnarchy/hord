//! The command's final message reaches the result: when it changes
//! nothing (the honest exit on a contradiction) or fails, the harness gives
//! up with that message as the reason. A stub `hord` answers `propose`, so
//! no repository or model is involved.

#![cfg(unix)]

use std::path::PathBuf;

use hord_api::proto::replay_result::Status;
use hord_api::proto::{ReplayRequest, ReplayResult};
use hord_replay_ref::{Options, replay};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> TestResult<Self> {
        let dir =
            std::env::temp_dir().join(format!("hord-replay-reason-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws"))?;
        // `hord propose` finds nothing to propose, as for a workspace the
        // command left unchanged.
        let hord = dir.join("hord");
        std::fs::write(
            &hord,
            "#!/bin/sh\necho 'error: nothing to propose: the workspace matches its base' >&2\nexit 1\n",
        )?;
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&hord)
            .status()?;
        Ok(Self(dir))
    }

    fn run(&self, cmd: &str) -> TestResult<ReplayResult> {
        let request = ReplayRequest {
            change: "ab".repeat(32),
            attempt: 1,
            workspace: "01M3000000000000000000000".into(),
            workspace_path: self.0.join("ws").display().to_string(),
            ..Default::default()
        };
        Ok(replay(
            &request,
            &Options {
                cmd: cmd.into(),
                hord: self.0.join("hord"),
                model: None,
            },
        )?)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn reason(result: &ReplayResult) -> TestResult<String> {
    match &result.status {
        Some(Status::GaveUp(g)) => Ok(g.reason.clone()),
        other => Err(format!("expected GaveUp, got {other:?}").into()),
    }
}

#[test]
fn an_unchanged_workspace_gives_up_with_the_commands_explanation() -> TestResult {
    let scratch = Scratch::new("explain")?;
    let result = scratch.run(
        "printf 'The intents contradict: LIMIT cannot be both 20 and 30.' > \"$HORD_REPLAY_MESSAGE\"",
    )?;
    assert_eq!(
        reason(&result)?,
        "The intents contradict: LIMIT cannot be both 20 and 30."
    );
    Ok(())
}

#[test]
fn a_failing_command_keeps_its_message_and_a_long_one_is_cut() -> TestResult {
    let scratch = Scratch::new("fail")?;
    let failed = scratch.run("printf 'cannot proceed' > \"$HORD_REPLAY_MESSAGE\"; exit 3")?;
    let why = reason(&failed)?;
    assert!(why.starts_with("cannot proceed"), "{why}");
    assert!(why.contains("exited"), "{why}");

    let long = scratch.run("head -c 3000 /dev/zero | tr '\\0' x > \"$HORD_REPLAY_MESSAGE\"")?;
    let why = reason(&long)?;
    assert_eq!(
        why.chars().count(),
        1001,
        "1,000 characters and an ellipsis"
    );
    assert!(why.ends_with('…'));

    let silent = scratch.run("true")?;
    assert_eq!(reason(&silent)?, "the command changed nothing");
    Ok(())
}
