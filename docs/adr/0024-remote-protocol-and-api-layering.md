# ADR 0024: gRPC remote protocol with the `.proto` as the one schema, and API layering

- **Status:** accepted
- **Date:** 2026-09-24
- **Spec:** §8.2 (remote), §8.3 (lazy checkout), §10.1 (library API), §10.5.1–10.5.3 (server, `RepoBackend`, events), §14 item 7 (OPEN); amends §8.2 and §10.5.2's wire format; ADR 0021 (local daemon)
- **Blocks:** M4 server foundation and conformance suite

## Problem

§8.2 sketches the protocol: HTTP/2, canonical CBOR for objects, JSON elsewhere, SSE events, and §14 leaves it OPEN. The M3 system review (item 6) found that today's API matches §10.1 in names only:

- `Repo` is concrete over a local `Store`.
- `Workspace` reaches the local store directly.
- The lander is a drain call, `land_local`, not a task that emits events.
- The `--json` shapes are defined ad hoc in `hord-cli`.

`RepoBackend` cannot be implemented by a remote client without restructuring. ADR 0021 also puts the local CLI behind a per-repo daemon over the same remote path in M4. §10.5.2 requires the web UI and `hord --json` to share one schema.

## Options

For the transport:

1. **HTTP/2 with JSON and CBOR.** `axum`/`hyper`, batched canonical CBOR at `/objects`, JSON with `schemars` at `/api/v1`, and SSE events. This matches the spec's sketch, but it hand-maintains a route table and its types in two places, the client and the server.
2. **gRPC (`tonic`).** One `.proto` defines every service, message, and stream. Clients and servers are generated, and streaming is native. Browsers cannot speak raw gRPC, so the UI needs gRPC-Web. Protobuf adds a code generator and `protoc` at build time.
3. **An extended git smart protocol.** Familiar to operators, but shaped around packfiles and refs, not per-object lazy fetch and a lander queue.

## Decision

Option 2. It has five parts.

- **Schema.** `crates/hord-api/proto/hord.proto` is the single source of truth for the remote API, the web UI, and `hord --json`.
  - `tonic-build` generates code, with `protoc` vendored (`protoc-bin-vendored`), so building needs no system install.
  - Objects travel as `bytes` fields holding their canonical CBOR (§3.9). Hashing and object encoding are unchanged.
- **Serving.** `hord serve` serves gRPC plus gRPC-Web (`tonic-web`) on one port. The web UI (M5) uses gRPC-Web.
  - Object transfer is batched RPCs (`GetObjects`, `PutObjects`, `Has`, at most 1,000 ids or 16 MiB per message) and streams for large sets.
  - Events are a server-streaming RPC resumable from an `EventCursor`, replacing SSE.
- **JSON.** `hord --json` prints the canonical protobuf JSON mapping of the same messages.
  - The server publishes the file descriptor set, plus a JSON Schema generated from it, at a well-known RPC and at `GET /schema.json`.
  - This supersedes `/api/v1/schema.json`. The M5 check "the UI issues no request that is not in the schema" becomes "the UI calls no RPC outside `hord.proto`".
- **Layering: `hord-api` and `RepoBackend`.** `hord-api` owns the `.proto`, the generated types, and `RepoBackend` (§10.5.2, method for method), expressed in the generated types.
- **Layering: the rest.**
  - `hord-txn` gains an object-source trait (`get_objects`, `has`, snapshot identity). `LocalRepo` implements it over `Store`, and `RemoteRepo` (`hord-remote`) over the gRPC client with a local object cache.
  - `Workspace` is generic over that source, so a remote workspace fetches only what it touches (§8.3).
  - The lander becomes a long-running task (`Lander::spawn(repo, cancel) -> (JoinHandle, EventStream)`) and is the only writer of the log and head (§6.7). `land_local` is a thin wrapper around it.
  - The CLI holds `Box<dyn RepoBackend>`. It prefers a running per-repo daemon (`hord serve --repo`, on a Unix socket, or a named pipe on Windows), starts one on demand, and has `--no-daemon` to open the store directly (ADR 0021).

## Consequences

- **The `--json` output shape changes once in M4,** to the protobuf JSON mapping. Agents that parse today's shapes see one documented change.
- **One conformance suite.** It is written once against `&dyn RepoBackend` and runs against `LocalRepo` and against `RemoteRepo`↔`hord serve` in-process.
- **Remote pristine checkouts (ADR 0016) are lazy.** A remote `Directory` workspace's pristine checkout fetches blobs on first materialization, not at `ws new`. This amends ADR 0016 for remote bases only.
- **Webhooks (§10.5.3)** stay HTTP POSTs of the JSON-mapped `Event`. That is outbound only and does not change the API.
- **Auth is M5** (§10.5.4). In M4, `hord serve` binds loopback only and refuses another address without `--insecure-bind`. M5's bearer tokens go in gRPC metadata.
- **New dependencies,** justified in the ADR log: `tonic`, `tonic-build`, `tonic-web`, `prost`, `prost-types`, `protoc-bin-vendored`, `pbjson` (for the canonical JSON mapping), `async-trait`, `tokio-stream`.
- Choosing a different transport, keeping a second hand-written API surface, or letting a second writer touch the log needs a new ADR.

## Amendments (2026-09-24, from implementation)

- **A local-only `Workspaces` service.** `RepoBackend` (§10.5.2) has no workspace operations, and while a daemon holds the store no CLI process can open it. `hord.proto` gains a `Workspaces` service (`WsNew`, `WsList`, `WsRm`, `WsGc`, `Status`, `Propose`, `PolicyCheck`) that the per-repo daemon serves. The daemon therefore parses, carries identity, and proposes with warm, shared caches (ADR 0021). `hord serve` for true remotes does not serve it in M4. Against a true remote, the CLI runs these commands client-side over a local cache store, with `RemoteRepo` as the object source (§10.5.5: "builds ChangeRecord locally"). `RepoBackend` stays §10.5.2, method for method.
- **Choosing a remote.** A global `--remote <name>` flag targets a configured remote. Each clone may set a default upstream (`hord remote set-default <name>`), used when `--remote` is absent and the command is not local-only. `ws new --base <remote>/<ref>` resolves the base from that remote (§10.5.5).
- **When a remote directory workspace fetches.** Without a VFS, a `Directory` workspace's first materialization is `ws new`. A remote `Directory` workspace fetches only its base's blobs, batched and skipping cached objects, when it writes the pristine checkout at `ws new`, and nothing earlier. `InMemory` workspaces stay fully lazy. This replaces consequence 3's "not at `ws new`".
