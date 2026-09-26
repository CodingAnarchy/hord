# ADR 0030: The web UI is a server-side client of the gRPC API, plus a read-only `Changes` service

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §10.4 (governing rule and stack, DECIDED), §10.5.2 (route sketch), §11 (`hord-ui` depends only on `hord-api`), §12 M5 (the "no unlisted request" check); amends ADR 0024
- **Blocks:** M5 web UI views 1–3 and flight-recorder playback

## Problem

Two decided texts disagree about the UI's data path.

- Spec §10.4 fixes the stack: server-rendered `askama` HTML, SSE for live regions, minimal vanilla JS, and no frontend build pipeline.
- ADR 0024 says "the web UI (M5) uses gRPC-Web". Read literally, that puts a protobuf runtime in the browser, since gRPC-Web frames are binary protobuf. It also needs a CBOR decoder and a JS copy of the object model, because records travel as canonical CBOR.

Separately, views 2 and 3 need data that `RepoBackend` does not return in usable form: decoded change views with names, a snapshot's indexed evidence (ADR 0025), a text diff, and a list of recordings.

## Options

1. **Server-side client of `RepoBackend`, with events relayed as SSE.** `hord-ui` renders HTML on the server. Every call it makes is one RPC of `hord.proto`.
2. **A gRPC-Web client in the browser**, as ADR 0024 literally reads. It supersedes the §10.4 stack with a vendored JS protobuf runtime and decoded-view RPCs.
3. **Hybrid.** Server-rendered pages, with only the landing strip on gRPC-Web in the browser. This means two data paths.

For the missing data:

- (a) a new read-only `Changes` service;
- (b) the same methods added to `RepoBackend`;
- (c) `hord-ui` decodes objects itself, which breaks §11's "depends only on hord-api".

## Decision

Option 1 with (a).

- **Server.** `hord-ui` is an axum router that `hord serve` mounts. Its handlers hold a `RepoBackend` (and a `Changes` client) and call nothing else.
- **Live regions.** The landing strip gets one SSE endpoint that relays `Events`, with the event cursor as the SSE `id`.
- **Actions.** Review and arbitrate are form POSTs, each turned into exactly one RPC.
- **The `Changes` service** is a separate read-only service in `hord.proto`, like the `Workspaces` amendment, with four RPCs:
  - `GetChange`: intent, named ops, read and write sets, provenance, the snapshot's evidence, and the queue entry;
  - `ChangeDiff`;
  - `ListRecordings`;
  - `GetRecording`.

ADR 0024's line "the web UI uses gRPC-Web" now reads: "the web UI is a client of the gRPC API; gRPC-Web stays available to other browser clients."

## Consequences

- The §10.4 stack stands as written. There is no JS protobuf runtime, no CBOR decoding in JS, and no build pipeline.
- **The M5 check** is a recording wrapper in the UI test suite. It fails on any call that is not an RPC of `hord.v1.RepoBackend`, `hord.v1.Changes`, or the RPCs of other slices that the UI's actions use (review and `Arbitrate`). `hord-ui` has no store, `hord-txn`, or `hord-diff` dependency.
- The API boundary sits between the UI server and the backend, not between the browser and the server. Tests also run the UI over a `RemoteRepo`, to show it works over the wire.
- `RepoBackend` stays §10.5.2 method for method. `hord-server` implements `Changes` over `LocalRepo`, and `hord-remote` gets its client.

**Amended (2026-09-26, M6):** `Changes` gains five read-only RPCs for web UI views 4–6 (`NodeLineage`, `ChangeTrace`, `ListTree`, `GetFile`, `NodeEdges`), each requiring the `read` scope.
