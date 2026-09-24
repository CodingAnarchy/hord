# ADR 0021: Many local processes share one repository through a per-repo daemon

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §8.1 (local store), §10.2 ("works identically against a local `.hord/`"), §10.3 (`--json` is the agent interface), §10.5.2 (local/remote symmetry)
- **Blocks:** concurrent CLI agents today; M4 local mode

## Problem

The redb index admits one process. In the review's repro, eight concurrent `hord ws new --json` calls in one repository produce seven failures: `Database already open. Cannot acquire lock.` The M3 simulation passes only because its 100 agents are tasks in one process sharing one `Repo`. Agents drive hord through the CLI (§10.3), and a human running `hord status` while an agent runs `hord propose` collides the same way.

## Options

1. **Wait for the lock.** Each CLI call opens the store, and retries with backoff until a timeout (default 30 s) if the store is locked. This is small and needs no new process. But calls serialize: a long `propose` (the first on a new head takes seconds) blocks every other agent, and caches (parse, reference context) are rebuilt by every process.
2. **Per-repo daemon.** The first CLI call starts a daemon for the repository (`hord serve --repo`, listening on a local socket: a Unix domain socket, or a named pipe on Windows). Later calls connect to it as a `RemoteRepo` client, the same client M4 uses for remote servers (§10.5.2). The daemon owns the store, the lander, and the warm caches, and exits after an idle timeout. This is the §10.2 symmetry for free, and caches are shared across calls. It needs M4's `RepoBackend` and wire types.
3. **Multi-process store.** Replace or wrap redb with a store that allows many writers (for example SQLite in WAL mode). This changes the storage decision in §8.1 and still leaves every process rebuilding its caches.

## Decision

Option 2 is the local model: the CLI talks to a per-repo daemon over the M4 `RemoteRepo` path. Until M4 ships `RepoBackend`, the CLI uses option 1 (bounded lock waiting with a clear timeout error) so concurrent agents work today.

## Consequences

- **Now:** `hord-store` open retries with backoff until `HORD_LOCK_TIMEOUT` (default 30 s), then fails with an error naming the holder where the platform allows. There is a test that runs 8 concurrent `hord ws new` calls, all of which succeed.
- **M4:**
  - `hord serve --repo` gains local-socket listening, and the CLI prefers a running daemon, starting one on demand.
  - `--no-daemon` opens the store directly, which is useful for tests and recovery.
  - The M3 simulation also runs through the daemon with 100 client processes.
- **What the daemon owns.** It is the single writer (§6.7): the lander runs inside it, which also closes the review's "single writer not enforced" finding for local use.
- Allowing several processes to write one store directly needs a new ADR.
