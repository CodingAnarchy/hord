# ADR 0041: A hord-first forge: issues, discussions and proposals as hord objects

- **Status:** accepted
- **Date:** 2026-09-27
- **Spec:** §10.4 (supersedes "Not in the UI: issues, discussion threads … Link out; do not build" for issues, discussions and proposals); §10.5 (API); ADR 0024, ADR 0030, ADR 0040
- **Blocks:** the forge feature in M6's programme (ADR 0040)

## Problem

§10.4 keeps issues and discussions out of hord: link out to another tool, and don't build them. Hord-first development by agents (ADR 0040) needs a place where agents and humans raise problems, discuss designs and propose work, tied to the changes, intents and definitions hord already knows. On GitHub those live apart from hord's semantic history, and an agent can reach them only through a second API.

## Options

1. **Keep linking out** to GitHub issues and discussions. They are a separate system, with no link to `NodeId`s, intents or evidence, and agents need a second API.
2. **A hord-first forge.** Issues, discussions and proposals become hord objects, reached through `hord.proto` and rendered by `hord-ui`.
3. **Mirror GitHub's.** The bridge syncs GitHub issues into hord. That keeps GitHub as the source of truth, which is the opposite of hord-first.

## Decision

Option 2, in slices. §10.4's "Not in the UI" line now covers only permissions administration and CI dashboards.

- **Threads are records in the store.**
  - An issue, discussion or proposal is a *thread*: a content-addressed record with a kind, a title, a body, an author (`Actor`, with provenance set by the server from the token, §10.5.4) and optional links. A link points to a change, an intent, a `NodeId`, a snapshot or another thread.
  - Replies and state changes (open, closed, accepted, declined) are signed events appended to the thread. They are never edited in place.
- **The API is part of `hord.proto`.**
  - A `Threads` service lists, opens, replies to, links, and changes the state of threads. It is scoped like the rest: `read`, and a new `discuss` scope for writing.
  - Thread events join the event stream, so agents watch and react as they do to lander events.
- **Proposals connect to the lander.** A proposal can be accepted into work. Changes that implement it link back through their intent, and the proposal's page shows their status.
- **UI:**
  - thread lists and pages in `hord-ui`, server-rendered as in ADR 0030;
  - links from change, lineage and trace pages to the threads that mention them;
  - `hord issue`, `hord discuss` and `hord propose-work` on the CLI (exact names are left to the slice).
- **The first slice**, the M6 programme's forge feature, is issues with replies, links to changes and definitions, the API, the CLI, and the UI pages. Discussions and proposals follow as further slices.

## Consequences

- Agents and humans raise and track work in the same system that lands it. A thread can point at the exact definitions and changes it's about, and a lineage page shows the threads that mention a definition.
- Hord takes on forge features it previously left to others, so they need tests and audits like the rest. `hord audit` counts thread writes as API writes.
- Mirroring threads to or from GitHub issues is not in scope. The bridge stays a code mirror (ADR 0036).
