//! Shared helpers for `hord-lang-rust` integration tests.

#![allow(dead_code)]

use std::process::Command;

/// `git`, isolated from the user's and the system's configuration (a
/// global `commit.gpgsign`, hooks, a default branch name), so tests behave
/// the same on every machine and on CI.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    command
}

/// Isolate `command`, and any `git` it runs, from the user's and the
/// system's git configuration. Git (for Windows too) reads `/dev/null` as
/// an empty file.
pub fn isolate_git(command: &mut Command) -> &mut Command {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}
