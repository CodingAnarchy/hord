# ADR (open): How the web UI reaches the API

- **Status:** open (not yet decided)
- **Date:** 2026-09-25
- **Spec:** §10.4 (governing rule and stack, both DECIDED), §10.5.2 (route sketch), §11 (`hord-ui` depends only on `hord-api`), §12 M5 (the "no unlisted request" check); ADR 0024 ("the web UI (M5) uses gRPC-Web"; Events replace SSE)
- **Blocks:** M5 web UI views 1–3 and flight-recorder playback (the data path only; templates, static assets, and the landing-strip state machine are scaffolded meanwhile)

## Problem

Two decided texts point different ways.

- **Spec §10.4 (DECIDED stack).** Server-rendered `askama` HTML, server-sent events for live regions, minimal vanilla JS, and no frontend build pipeline. The rationale is one language and one binary, and agents can change the UI without a Node toolchain.
- **Spec §10.4 (DECIDED governing rule).** The UI consumes only the public API that `hord --json` and agents use. It has no privileged data path.
- **ADR 0024.** `hord.proto` is the one schema. `hord serve` serves gRPC and gRPC-Web, and "the web UI (M5) uses gRPC-Web". Events are a server-streaming RPC, replacing SSE. The M5 check becomes "the UI calls no RPC outside `hord.proto`".

Read literally, ADR 0024 puts a gRPC-Web client in the browser. That needs more than the "minimal vanilla JS" of §10.4:

1. **Protobuf in JS.** gRPC-Web frames carry binary protobuf (or base64 of it). Decoding it needs a protobuf runtime (such as a vendored `protobuf.js` loading `hord.proto` at runtime) or generated code (a build step, which §10.4 rules out). Writing the varint and framing decoder by hand is the hand-rolled serialization that AGENTS.md forbids.
2. **CBOR in JS.** The semantic-change and arbitration views show a `ChangeRecord`: its intent, ops, read and write sets, and provenance. The API carries records only as `Object.bytes`, which is canonical CBOR. The browser would also need a CBOR decoder and a JS copy of `hord-core`'s object model, which is a second implementation of the data model in a second language.

§11 already says `hord-ui` is "askama templates plus static assets" and "depends only on hord-api", where `RepoBackend` lives. That reads as a server-side client of the API.

## Options

1. **Server-side client of `RepoBackend`, events relayed as SSE (recommended).**
   - `hord-ui` is an axum router that `hord-server` mounts at `/` (and `/r/<name>/`). Each page handler holds an `Arc<dyn RepoBackend>` and calls only its methods. Every trait method is one RPC of `hord.proto`, method for method (ADR 0024).
   - `hord serve` hands it the same backend the gRPC service serves. Tests can hand it a `RemoteRepo` instead, so the UI is shown to work over the wire as well.
   - Live regions (the landing strip) use one SSE endpoint. It relays `RepoBackend::events`, rendered as HTML fragments or as the canonical JSON mapping of `EventEnvelope`, with the event cursor as the SSE `id` for resume.
   - Actions (review, arbitrate) are HTML form POSTs. The handler turns each into one RPC (`AttachEvidence`, `Arbitrate`, and `auth`'s review RPC).
   - The M5 check is a recording `RepoBackend` wrapper in the UI test suite. It logs every call by RPC name and fails on any name that is not a method of `hord.v1.RepoBackend` in the descriptor set. It also checks that the UI touches no other state: `hord-ui` has no store or `hord-txn` dependency.
   - What this changes: ADR 0024's sentence "the web UI uses gRPC-Web" is amended to "the web UI is a client of the gRPC API; gRPC-Web stays available to browser clients other than hord's own UI". The §10.4 stack stands as written.
   - Cost: the browser sees HTML, not the API. The API boundary sits between the UI server and the backend, not between the browser and the server.
2. **Browser gRPC-Web client, literal ADR 0024.**
   - Pages are static shells, and vendored JS (a protobuf runtime plus a gRPC-Web transport) calls `RepoBackend` from the browser.
   - For the change views, the API also needs RPCs that return decoded views (see "API gaps"), because decoding CBOR in JS is ruled out above.
   - Needs an ADR that supersedes the §10.4 stack: this is not minimal JS, and the vendored runtime is a frontend dependency. The M5 check becomes a browser-level proxy, such as a headless browser test.
3. **Hybrid.** Pages are server-rendered as in option 1, and only the live landing strip subscribes to `Events` over gRPC-Web in the browser.
   - This keeps one JS protobuf runtime for events, which carry only strings and ids, not CBOR.
   - It still needs the vendored protobuf runtime and a partial supersession of §10.4 (no SSE), and it splits the UI across two data paths.

## Recommendation

Option 1. It is the only option that satisfies both DECIDED §10.4 rules and AGENTS.md as written. It matches §11's "depends only on hord-api". It makes the M5 check a precise unit-level assertion over RPC names, not a browser proxy. ADR 0024's goal, one schema and no second hand-written API, still holds, because the UI has no API of its own: its routes return only HTML and an SSE relay of `EventEnvelope`.

## API gaps (independent of the option above)

Whichever option is chosen, views 2 and 3 need data that `RepoBackend` does not return in usable form today. The spec's own route sketch (§10.5.2: `GET /changes/{id}` returns "ChangeRecord + resolved names + evidence", plus `/changes/{id}/diff?format=ops|text` and `/recordings/{id}`) had these, but ADR 0024's `RepoBackend` has none of them:

| Need | Today | Gap |
|---|---|---|
| Intent, ops, read/write sets, provenance | `GetObjects` returns the record as CBOR | Decoding it needs `hord-core`. Names for op `NodeId`s need a walk of the snapshot's trees. |
| Evidence for a change's result snapshot (ADR 0025) | Only in events (`Landed.evidence`, `EvidenceAttached`) and the author's `ChangeRecord.evidence` | No RPC lists a snapshot's indexed evidence. |
| Text diff (secondary tab) | None | Needs `hord-diff`, which `hord-ui` must not depend on. |
| Recordings "under `.hord/recordings/`" | The M3/M4 eval stores a `Blob` in the store and prints its id | No way to list recordings. Fetching one by id works (`GetObjects` returns the Blob's CBOR). |

Options:

- **(a) A read-only `Changes` service in `hord.proto`, owned by the ui slice (recommended).**
  - `GetChange(change)` returns the decoded view: intent, `OpView`s with names, read and write sets as `NodeRef`s, provenance, the snapshot's evidence, and the queue entry.
  - `ChangeDiff(change, format = text)`.
  - `ListRecordings` and `GetRecording(id)`.
  - `hord-server` implements it over `LocalRepo`, and `RemoteRepo` gets the client.
  - It is a separate service, like ADR 0024's `Workspaces` amendment, so `RepoBackend` stays §10.5.2 method for method. The M5 check allows `hord.v1.RepoBackend` and `hord.v1.Changes`.
- **(b) Add the same methods to `RepoBackend`.** This amends §10.5.2's trait. Every implementation and the conformance suite grow.
- **(c) `hord-ui` decodes objects itself** with `hord-core` and walks trees via `GetObjects`. There is no evidence listing and no text diff unless `hord-ui` also depends on `hord-diff` and the index, which breaks §11's "depends only on hord-api".

## What I will do on each answer

- **1 + (a):** `hord-ui` as described. The `Changes` service goes into `hord.proto` in one contiguous block. Recordings are listed from what `ListRecordings` returns.
- **2 or 3:** stop and write the supersession ADR for §10.4's stack first.
