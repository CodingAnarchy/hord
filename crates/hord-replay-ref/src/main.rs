//! `hord-replay-ref`: the reference replay harness (spec §6.6). See the
//! library docs for the protocol and the command's environment.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use hord_api::proto::ReplayRequest;
use hord_replay_ref::{Options, gave_up, replay};

/// Reference replay harness: reads a ReplayRequest line on stdin, runs a
/// command in the replay workspace, proposes through hord, and writes a
/// ReplayResult line on stdout.
#[derive(Debug, Parser)]
#[command(name = "hord-replay-ref", version)]
struct Cli {
    /// The command to run in the workspace (by the platform shell), with
    /// the prompt on stdin. Also `HORD_REPLAY_CMD`.
    #[arg(long, env = "HORD_REPLAY_CMD", value_name = "CMD")]
    cmd: String,
    /// The `hord` binary to propose with. Also `HORD_BIN`.
    #[arg(long, env = "HORD_BIN", value_name = "PATH", default_value = "hord")]
    hord: PathBuf,
    /// Model name to report when the command reports none.
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut line = String::new();
    if let Err(err) = std::io::stdin().lock().read_line(&mut line) {
        eprintln!("hord-replay-ref: read the request: {err}");
        return ExitCode::FAILURE;
    }
    let request: ReplayRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(err) => {
            eprintln!("hord-replay-ref: the request is not a ReplayRequest: {err}");
            return ExitCode::FAILURE;
        }
    };
    let options = Options {
        cmd: cli.cmd,
        hord: cli.hord,
        model: cli.model,
    };
    let result = replay(&request, &options).unwrap_or_else(|err| gave_up(err.to_string()));
    let Ok(text) = serde_json::to_string(&result) else {
        eprintln!("hord-replay-ref: encode the result");
        return ExitCode::FAILURE;
    };
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
