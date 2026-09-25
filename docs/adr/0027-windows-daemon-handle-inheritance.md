# ADR 0027: The CLI may use `unsafe` to keep its stdio out of the Windows daemon

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §11.1 (`#![forbid(unsafe_code)]` except `hord-vfs`, DECIDED), ADR 0021 / ADR 0024 (the per-repo daemon)
- **Blocks:** M4 (the daemon on Windows)

## Problem

On Windows, `CreateProcess` gives a child every inheritable handle of its parent, and Rust's `Command` always asks it to. When the CLI starts the per-repo daemon, the daemon inherits the CLI's own stdout and stderr. If the caller captured them through a pipe (an agent, a test, `hord ws new | ...`), the pipe stays open until the daemon exits, so the caller waits for the daemon's whole idle timeout (300 s by default) before it sees the command finish. On Unix, std opens every handle close-on-exec, so this does not happen.

Stable Rust cannot turn inheritance off: `CommandExt::inherit_handles` is unstable (rust-lang/rust#146407). The Win32 call that can, `SetHandleInformation`, needs `unsafe`, which §11.1 forbids outside `hord-vfs`. `hord-vfs` does not exist yet, and starting processes is not its job.

## Options

1. **One `unsafe` call in `hord-cli`.** Before spawning the daemon, clear `HANDLE_FLAG_INHERIT` on the CLI's three std handles with `SetHandleInformation` (`windows-sys`). This is Windows-only, a single function, and the handles are the process's own. Child stdio set up through `Stdio` still works, because std duplicates those handles as inheritable itself.
2. **Start the daemon through PowerShell `Start-Process`**, which does not pass on handles. No `unsafe`, but every daemon start costs about a second, and it depends on PowerShell and its execution policy.
3. **Wait for `inherit_handles` to stabilize** and skip the daemon tests on Windows until then. Windows users who capture output wait for the full idle timeout in the meantime.

## Decision

Option 1. `hord-cli` goes from `forbid` to `deny(unsafe_code)`, with a single `#[allow(unsafe_code)]` function in `daemon.rs` that is compiled only on Windows. Move it to `inherit_handles` once that is stable.

## Consequences

- Capturing a command's output on Windows no longer waits for the daemon's idle timeout.
- §11.1's rule becomes: `forbid(unsafe_code)` everywhere except `hord-vfs`, plus the one Windows function in `hord-cli` named here. Any other `unsafe` still needs its own ADR.
- `windows-sys` becomes a Windows-only dependency of `hord-cli`. It is already in the tree through tokio and others.
