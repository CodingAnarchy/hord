# HORD — A Version Control System for the Agentic Age

**Status:** Draft v0.2 — 2026-09-17 (v0.2: added §10.4 presentation layer, §10.5 hosting server/API; revised M4–M6)
**Codename:** `hord` (Old English: hoard, treasury; cf. *wordhord*). Placeholder — rename freely.
**Implementation language:** Rust (edition 2024)
**First supported source language:** Rust (so the system can eventually host itself)

This document is written to be handed to a team of coding agents. It is intentionally opinionated where a decision is needed and intentionally open where the answer must be discovered by building. Sections marked **DECIDED** are not up for relitigation without an ADR. Sections marked **OPEN** are for the team to resolve, with an ADR recording the answer.

---

## 0. Executive summary

Git is a content-addressed filesystem with a commit DAG on top. It merges *text* because, by the time a conflict surfaces, the author is gone and only their bytes remain.

Hord is a **transactional database over a semantic code graph**. A change carries its *intent*, its *provenance*, its declared *read/write sets*, and *verification evidence*. Because agents are reproducible, a conflicting change is not text-merged; it is **replayed** against the new base and re-verified. Branching becomes an optional view over a transaction log rather than the primitive. Workspaces are O(1) copy-on-write snapshots. History is a queryable index at the granularity of code nodes, not a linked list of file snapshots.

Three ideas do the work; everything else is plumbing:

1. **Code is a graph; files are a projection.** The stored artifact is a lossless syntax tree with stable node identities. Two agents editing different functions in the same file never conflict.
2. **A change is `{intent, ops, provenance, read_set, write_set, evidence}`.** Landing is a serializability check plus policy evaluation, not a human decision.
3. **Conflicts are resolved by replay, not merge.** Structural rebase first; agent-driven replay of the recorded intent second; human arbitration last.

---

## 1. Goals and non-goals

### 1.1 Goals

- **Massive concurrency.** Hundreds to thousands of agents proposing changes against one repository simultaneously, with landing throughput bounded by verification cost, not by human review or lock contention.
- **Semantic conflict detection.** Conflicts are computed on graph nodes, not lines. False-negative conflicts (two changes that silently break each other) must be zero for the semantic tier; false positives should be rare.
- **Provenance and verifiability.** Every landed change records who/what produced it, under what intent, with what tooling, and what evidence showed it was valid. Evidence is re-checkable.
- **Cheap workspaces.** Creating a workspace from any snapshot is O(1). Thousands may exist concurrently.
- **Queryable history.** "Who last changed the behavior of this function, why, and which test covers it?" is an index lookup.
- **Git interoperability from day one.** Byte-exact import from and export to git. Hord must be adoptable as a layer over an existing git repository before it is adopted as a replacement.
- **Self-hosting.** By milestone M6, hord's own development happens in hord, with git as a mirror.

### 1.2 Non-goals (v1)

- Peer-to-peer multi-master landing. Landing is serialized per repository (later: per subtree). Offline *work* is supported; offline *landing* is not.
- Proving semantic merges correct. Merges are heuristic and verified, never proven.
- Replacing the build system. Hord invokes toolchains; it does not become one. (It does maintain the dependency graph a build system needs, and exposes it.)
- Full semantic support for every language. Non-Rust content gets structural (syntax-tier) or blob-tier treatment until an adapter exists.
- A general-purpose code host (issues, wikis, CI runners, permissions UI). Hord ships a **presentation layer** for its own concepts — the lander queue, semantic changes, arbitration, lineage (§10.4) — served by the hord server (§10.5). It does not try to be GitHub.

---

## 2. Design principles (DECIDED)

1. **Lossless by construction.** The file projection of any snapshot is byte-identical to the source that was ingested. Formatting, comments, trailing whitespace, and BOMs are preserved. Semantic identity is computed on a normalized view; storage is raw.
2. **Content-addressed everything.** Every stored object is identified by a BLAKE3 hash of its canonical encoding. Sharing across snapshots is automatic. Objects are immutable.
3. **Separate identity from content.** A code node has a stable `NodeId` (persists across edits) and a content `ObjectId` (changes with every edit). Read/write sets, blame, and conflict detection operate on `NodeId`s.
4. **Language-agnostic core.** The core engine knows about nodes, edges, snapshots, and transactions. It knows nothing about Rust. Language knowledge lives behind an adapter trait.
5. **Verification is data.** Test results, type-check results, benchmarks, and reviews are first-class objects attached to changes, keyed by the exact snapshot and toolchain they were produced against.
6. **Optimistic by default.** No locks. Transactions assume they will land and are checked at land time.
7. **The lander is a single writer.** One serialization point per repository advances `head`. Everything else is parallel. This is a deliberate CAP trade and should not be "fixed" in v1.
8. **Escalation is a ladder, not a wall.** Structural rebase → agent replay → human. Each rung is cheaper than the next and is tried first.
9. **Git is a peer, not an enemy.** Every hord repository can be projected to a git repository at any time. Round-trip fidelity is a tested invariant.

---

## 3. Core data model

All types below are illustrative Rust; field names are normative, exact shapes are not.

### 3.1 Identifiers

```rust
/// Content hash. BLAKE3-256 over the canonical encoding of an object.
pub struct ObjectId([u8; 32]);

/// Stable identity of a code node across edits. Assigned once, carried forward.
/// ULID-encoded; the timestamp component is informational only.
pub struct NodeId(u128);

/// A snapshot is identified by the ObjectId of its root tree object.
pub type SnapshotId = ObjectId;

/// A change record is identified by its own ObjectId.
pub type ChangeId = ObjectId;
```

### 3.2 Objects

Every persistent value is an `Object` with a canonical encoding (see §3.9). Object kinds:

| Kind | Purpose |
|---|---|
| `Blob` | Raw bytes for files with no adapter (images, lockfiles, unknown languages). |
| `Node` | One syntax-tree node, holding its exact source bytes and its children. |
| `Tree` | A directory: ordered map of name → (`Blob` \| `Tree` \| `NodeFile`). |
| `NodeFile` | The root of a parsed file: adapter id, language id, root `Node`, raw-bytes hash for round-trip checking. |
| `Snapshot` | Root `Tree` + repository-level metadata + index pointers. |
| `ChangeRecord` | See §3.5. |
| `Evidence` | See §3.6. |
| `Policy` | See §7. |
| `IdentityMap` | Per-snapshot mapping `NodeId → path-in-tree`, plus per-change `NodeId` births/deaths/derivations. |

### 3.3 Nodes and lossless trees

```rust
pub struct Node {
    pub kind: NodeKind,           // adapter-defined, e.g. "fn_item", "struct_item", "block"
    pub lang: LangId,
    pub raw: Bytes,               // leaves only; internal nodes omit this field (ADR 0008)
    pub normalized: ObjectId,     // hash of trivia-stripped canonical form (semantic identity)
    pub children: Vec<ObjectId>,  // child Node ObjectIds; projection is concat(children.raw)
    pub name: Option<QualifiedName>, // only for named definitions
}
```

**Invariants:**

- Leaves store `raw` (the token plus attached trivia). An internal node's stored object has no `raw` (ADR 0008). Its projection is `concat(children.raw)`, which keeps the file walk lossless.
- A leaf `ObjectId` is `blake3(canonical(node))` and covers `raw`. An internal `ObjectId` is `blake3(canonical(stored object))` over `kind`, `lang`, `normalized`, `children`, and `name`. Identical child sequences share one stored node.
- A leaf `normalized` hashes its stripped token text. An internal `normalized` hashes the canonical CBOR array of its children's `normalized` ids, in order. A whitespace-only change alters `ObjectId`s up the path and no `normalized` id.

**Trivia attachment rule (DECIDED):** leading trivia (comments, blank lines) attaches to the following token; trailing trivia on the same line attaches to the preceding token. This makes "doc comment moves with the function" the default.

### 3.4 Node identity

`NodeId` is the answer to "is this the same function as before?" It is assigned at birth and carried forward by the adapter's `identify` step (§4.3), which maps nodes in a new tree to nodes in the base tree.

Carrying rules, applied in order:

1. **Exact:** same `normalized` hash and same parent `NodeId` → same `NodeId`.
2. **Named:** same `QualifiedName` under the same parent → same `NodeId` (body changed).
3. **Moved:** same `normalized` hash under a different parent → same `NodeId`, emit `Op::Move`.
4. **Renamed:** unmatched definition in old and new with `normalized` body similarity above threshold (OPEN: threshold and metric; start with tree-edit-distance ratio ≥ 0.8) → same `NodeId`, emit `Op::Rename`.
5. **Declared:** an agent may explicitly declare identity relations in the change (`derived_from`, `split_into`, `merged_from`). Declarations override heuristics.
6. Otherwise: new `NodeId` (birth) or retired `NodeId` (death).

Only **definition-bearing** nodes get durable `NodeId`s (functions, types, impls, modules, consts, statics, macros, and their equivalents). Statements and expressions inside a body are identified positionally within their enclosing definition; edits to them are edits to the definition. This bounds the identity map to roughly the number of definitions in the repository.

### 3.5 Change records

```rust
pub struct ChangeRecord {
    pub base: SnapshotId,
    pub result: SnapshotId,
    pub parents: Vec<ChangeId>,          // usually one; the change this was landed after
    pub ops: Vec<Op>,                    // semantic operations, base -> result
    pub intent: Intent,
    pub provenance: Provenance,
    pub read_set: BTreeSet<NodeId>,
    pub write_set: BTreeSet<NodeId>,
    pub identity_deltas: Vec<IdentityDelta>, // births, deaths, derivations
    pub evidence: Vec<ObjectId>,         // Evidence objects
    pub signature: Option<Signature>,
}

pub enum Op {
    Insert  { parent: NodeId, index: u32, node: ObjectId },
    Delete  { node: NodeId },
    Replace { node: NodeId, from: ObjectId, to: ObjectId },
    Move    { node: NodeId, from_parent: NodeId, to_parent: NodeId, index: u32 },
    Rename  { node: NodeId, from: QualifiedName, to: QualifiedName },
    Blob    { path: RepoPath, from: Option<ObjectId>, to: Option<ObjectId> },
    Tree    { path: RepoPath, kind: TreeOpKind },   // file/dir create, delete, rename
}

pub struct Intent {
    pub summary: String,                 // one line, human-readable
    pub body: String,                    // full task description / prompt / spec excerpt
    pub refs: Vec<IntentRef>,            // issue ids, spec URLs, parent change ids
    pub acceptance: Vec<Acceptance>,     // machine-checkable: test names, check commands, invariants
}

pub struct Provenance {
    pub actor: Actor,                    // Human { id } | Agent { id, model, model_hash, harness }
    pub toolchain: ObjectId,             // hash of a Toolchain object (rustc, cargo, tree-sitter versions...)
    pub created_at: Timestamp,
    pub session: Option<String>,         // opaque harness session id
    pub parent_intent: Option<ChangeId>, // if this change was a replay of another
}
```

`Op`s are derived by the diff engine (§5) and **also** checked against the actual `base → result` tree delta; a `ChangeRecord` whose ops do not reproduce `result` from `base` is rejected at landing.

The `read_set` is the set of `NodeId`s whose content the author *depended on*. It is collected from (a) VFS/API access logs during the transaction, (b) adapter-derived reference edges from written nodes, and (c) explicit declarations. It is a superset by construction; over-declaring is safe (may cause spurious conflicts), under-declaring is a correctness bug.

### 3.6 Evidence

```rust
pub struct Evidence {
    pub kind: EvidenceKind,      // Check | Test | Bench | Lint | Review | Custom(String)
    pub snapshot: SnapshotId,    // exactly what was verified
    pub toolchain: ObjectId,
    pub command: String,
    pub scope: Option<BTreeSet<NodeId>>, // what this evidence is claimed to cover
    pub result: EvidenceResult,  // Pass | Fail { summary } | Skipped { reason }
    pub log: Option<ObjectId>,   // Blob of captured output
    pub cost_ms: u64,
    pub produced_by: Actor,
    pub produced_at: Timestamp,
}
```

Evidence is valid for a snapshot, not a change. When a change lands on a base different from the one its evidence was produced against, the evidence is **stale** and the verification engine (§6) decides what must re-run.

### 3.7 Snapshots and the log

A `Snapshot` is a Merkle tree. The repository maintains:

- `head: ChangeId` — the latest landed change.
- `log` — append-only sequence of landed `ChangeId`s, in landing order. This is the total order; the DAG of `parents` is derived, not primary.
- `index` — see §8.

Named refs (`main`, `release/1.2`) are pointers into the log and are optional. A "branch" in the git sense is a workspace whose base is not `head` and which has not landed; it needs no name.

### 3.8 Edges

Edges are derived from adapter analysis and stored per snapshot in the index, not in the Merkle tree (they are recomputable).

| Edge | Meaning |
|---|---|
| `Contains(a, b)` | structural parent/child (definitions only) |
| `References(a, b)` | `a`'s body names `b` |
| `Depends(a, b)` | build/type dependency, coarser than References (crate/module level) |
| `Tests(t, a)` | test `t` exercises `a` (from adapter heuristics, refined by coverage evidence) |
| `DerivedFrom(a, b)` | identity delta: `a` was split from / renamed from / copied from `b` |

### 3.9 Canonical encoding (DECIDED)

Objects are encoded with a deterministic binary encoding for hashing and storage. Requirements: canonical (one encoding per value), schema-evolvable, no floats in hashed fields, sorted map keys. Use **canonical CBOR (RFC 8949 §4.2.1)** via a Rust crate with a canonicalization mode, wrapped in a thin `hord-encoding` crate with golden test vectors checked into the repo. Do not hand-roll a format.

---

## 4. Language support: tiers and adapters

### 4.1 The question of per-language implementations

Storing syntax trees does **not** require writing a parser or a full analyzer per language. Language support is layered, and each layer has a different cost:

| Tier | Provides | Per-language cost | Source |
|---|---|---|---|
| **0 — Blob** | bytes, line diff, git-equivalent behavior | none | built in |
| **1 — Syntax** | lossless CST, structural diff/merge, node-granular conflicts | ~zero: a grammar crate + a 50-line `NodeKind` mapping | tree-sitter grammars (100+ languages exist) |
| **2 — Semantic** | definitions, qualified names, references, dependency edges, stable identity | an adapter: typically 1–3k lines | hand-written per language, or bridged from an existing analyzer |
| **3 — Verified** | type-check, test, lint as evidence | a toolchain runner: ~200 lines | invoke the language's own toolchain |

Tier 1 is nearly universal and is where most of the conflict-reduction value lives. Tier 2 is what enables read/write sets, semantic blame, and impact analysis. Tier 3 is orchestration, not analysis.

So: **yes, Tier 2 is a distinct implementation per language, but it is a thin adapter over a uniform interface, not a reimplementation of the language.** The core engine never sees language-specific types.

### 4.2 Rust adapter strategy (DECIDED for M1–M2, OPEN beyond)

- **Tier 1:** `tree-sitter-rust`. Lossless CST with trivia attachment per §3.3.
- **Tier 2:** Start with a tree-sitter-based resolver: module tree from `mod` items and file layout, `use` resolution, definition extraction, reference edges by name resolution within the crate. A name whose target package is in the snapshot and linked by a manifest also resolves (ADR 0010). For Rust that link is a path dependency, including one inherited with `workspace = true`, not workspace membership alone. This is deliberately approximate (no trait resolution, no type inference, no feature selection). It must be **sound for write sets** (every edited definition is identified) and **conservative for read sets** (over-approximate references).
- **Tier 2 backend (ADR 0009, ADR 0011):** do not depend on `ra_ap_syntax`, `ra_ap_hir`, or `ra_ap_ide_db`. The resolver stays tree-sitter. The §12 pass/fail recall check is a syntactic name walk. rust-analyzer find-references may be printed and does not authorize embedding those crates.
- **Macros:** `macro_rules!` and proc-macro *invocations* are opaque `Node`s at Tier 1/2. Their expansion is not stored. Reference edges from a macro invocation are the identifiers lexically present in the invocation tokens (conservative). `#[test]` and `#[cfg(test)]` are recognized by attribute inspection.
- **Tier 3:** `cargo check`, `cargo test`, `cargo clippy`, `cargo bench` via `hord-verify-rust`. Test selection uses `cargo test -p <crate> <filter>` initially; finer selection is OPEN.

### 4.3 Adapter trait

```rust
pub trait LangAdapter: Send + Sync {
    fn lang(&self) -> LangId;
    fn tier(&self) -> Tier;
    fn matches(&self, path: &RepoPath, head: &[u8]) -> bool;

    /// Tier 1. Must be lossless: `project(parse(bytes)) == bytes`.
    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError>;
    fn project(&self, tree: &NodeTree) -> Bytes;

    /// Tier 1. Which node kinds bear durable identity.
    fn is_definition(&self, kind: &NodeKind) -> bool;

    /// Tier 2. Qualified name of a definition node, given its ancestor chain.
    fn qualified_name(&self, path: &[&Node], node: &Node) -> Option<QualifiedName>;

    /// Tier 2. Outgoing references from a node's body (names, not resolved targets).
    fn references(&self, ctx: &ResolveCtx, node: &Node) -> Vec<NameRef>;

    /// Tier 2. Resolve a NameRef to a definition NodeId within a snapshot, if possible.
    fn resolve(&self, ctx: &ResolveCtx, r: &NameRef) -> Option<NodeId>;

    /// Tier 2. Test heuristics: which definitions does this test likely exercise?
    fn test_targets(&self, ctx: &ResolveCtx, test: &Node) -> Vec<NodeId>;

    /// Tier 2. Identity carrying between base and result trees. Default impl applies §3.4.
    fn identify(&self, base: &IdentifiedTree, result: &NodeTree) -> IdentityMapping {
        default_identify(self, base, result)
    }
}

pub trait Verifier: Send + Sync {
    fn lang(&self) -> LangId;
    fn toolchain(&self) -> Toolchain;
    /// Plan the minimal set of checks for an impact set. May return "everything".
    fn plan(&self, snap: &Snapshot, impact: &BTreeSet<NodeId>, policy: &Policy) -> VerifyPlan;
    fn run(&self, ws: &Workspace, plan: &VerifyPlan) -> Vec<Evidence>;
}
```

Adapters are compiled in for v1 (a registry in `hord-lang`). **OPEN:** WASM-loaded adapters for M7+.

### 4.4 Non-code content

Files without an adapter are `Blob`s with git semantics (line-based 3-way merge for text, no merge for binary). `Cargo.lock` gets a tiny purpose-built adapter early (M3) because lockfile conflicts are a top source of spurious agent conflicts. `Cargo.toml` is TOML → Tier 1 via `tree-sitter-toml`.

---

## 5. Diff and structural merge

### 5.1 Structural diff

Input: base `NodeTree`, result `NodeTree`, `IdentityMapping`. Output: `Vec<Op>` minimal under a cost model (insert/delete/replace/move/rename weighted). Algorithm: **OPEN**, but start with GumTree-style top-down/bottom-up matching using `normalized` hashes as anchors, restricted to definition granularity for `Op`s (sub-definition edits collapse to `Replace` on the definition). Correctness requirement: `apply(base, ops) == result` byte-for-byte after projection. Optimality is a quality metric, not a correctness requirement.

### 5.2 Structural 3-way merge (rebase)

Input: `base`, `ours` (landed head), `theirs` (proposed change ops). Output: either a merged tree or a `Conflict` listing the `NodeId`s in contention.

Rules:

1. Ops on disjoint definition `NodeId`s compose. Order-dependent ops on the same parent (two inserts at the same index) are resolved by a deterministic tie-break (landing order first, then `NodeId`) and **flagged as soft conflicts** for the verifier to re-check.
2. `Replace` on the same `NodeId` from both sides is a hard conflict, **unless** `normalized` results are equal (both made the same edit) — then it composes.
3. `Delete` vs any other op on the same `NodeId` is a hard conflict.
4. `Rename` composes with `Replace` (rename applied, then body replaced; references updated by re-running the adapter's resolver).
5. Blob-tier files use git's 3-way line merge (`diffy` or similar). Binary conflicts are hard conflicts.

Every merge result must re-parse cleanly under the adapter. A merge that produces an unparseable file is a hard conflict regardless of rules above.

---

## 6. Transactions and landing

### 6.1 Workspace

```rust
pub struct Workspace {
    pub id: WorkspaceId,
    pub base: SnapshotId,
    pub actor: Actor,
    pub materialization: Materialization,  // Directory { path } | Vfs { mount } | InMemory
    pub access_log: AccessLog,             // reads and writes at node granularity
}
```

- `begin(base) -> Workspace` is O(1): a snapshot pointer plus an empty overlay.
- Materialization is lazy. `Directory` mode writes files on demand and watches for changes; `Vfs` mode (M7) serves a FUSE mount. `InMemory` is for agents that use the library API directly and never touch a filesystem.
- The `access_log` records every node read (via file read → mapped to node spans, or via API) and every file/node written. This is the raw material for `read_set`/`write_set`.

### 6.2 Lifecycle

```
begin(base)
  → [agent works: reads, writes, runs checks locally]
  → propose()      : diff, identify, build ChangeRecord, attach local Evidence
  → submit()       : hand ChangeRecord to the lander
  → lander: conflict check → verify → policy → land | replay | escalate
```

### 6.3 Conflict check (serializability)

Let `L` be the set of changes landed after `change.base` up to current `head`. `change` conflicts if any of:

- `change.write_set ∩ L.write_set ≠ ∅` (write-write)
- `change.read_set ∩ L.write_set ≠ ∅` (read-write: something the author relied on changed)
- `change.write_set ∩ L.read_set ≠ ∅` (write-read: only if policy is `strict`; default is `off`, since landed changes were already verified against their own reads)

Read/write sets are compared on `NodeId`. Blob-tier files use path as the identity.

No conflict → the change is **rebased trivially** (its ops are re-applied on `head`; guaranteed to compose by disjointness) and proceeds to verification.

### 6.4 Escalation ladder

1. **Structural rebase (§5.2).** Cheap, deterministic. If it succeeds with no hard conflicts, proceed to verification with the soft conflicts flagged.
2. **Replay.** The lander invokes the **replay harness** (§6.6) with `change.intent`, `change.provenance`, the new base, and the conflict report. The harness produces a new `ChangeRecord` with `parent_intent = original`. Bounded: policy sets max replay attempts (default 2) and a cost budget.
3. **Arbitration.** The change is parked in a `needs_arbitration` queue with a machine-generated conflict summary (which nodes, which intents collided, what each side changed). A human or a designated arbiter agent resolves it. Arbitration produces a new change whose `parents` include both colliding changes.

### 6.5 Verification at landing

After rebase, the verification engine (§7) computes the **impact set**: `write_set ∪ transitive References-dependents(write_set)` bounded by policy (default: 2 hops, or crate boundary). Evidence attached to the change is accepted if it was produced against the exact post-rebase snapshot; otherwise it is stale and the verifier's `plan` decides what to re-run. Fresh evidence is attached before landing.

Verification failure after a clean rebase is treated as a **semantic conflict** and enters the escalation ladder at step 2 with the failure attached to the replay context.

### 6.6 Replay harness

Hord does not embed a model. It defines a **replay protocol**: a stdin/stdout JSON-lines contract that any agent harness can implement.

```
→ ReplayRequest { intent, provenance, base: SnapshotId, workspace: WorkspaceId,
                  conflict: ConflictReport, budget: Budget }
← ReplayResult  { status: Proposed { change: ChangeId } | GaveUp { reason } }
```

The harness gets a workspace on the new base, does whatever it does, and calls `propose()`. Hord records `parent_intent` so the history shows the change was a replay. **The reference harness ships as a separate crate (`hord-replay-ref`) that shells out to a configurable command**; it is not part of the core.

### 6.7 The lander

Single process per repository. Consumes a queue of submitted changes. For each change it runs §6.3–§6.5 and appends to the log. Verification is the expensive step and is **parallel across changes** with a speculative model: change *N+1* is verified against `head + N` before *N* lands; if *N* fails, *N+1* is re-checked. This is the classic merge-queue design (Bors, Zuul, GitHub merge queue) and is well understood.

**OPEN (M7):** partitioned landers per subtree with a two-phase commit for cross-partition changes.

---

## 7. Verification and policy

### 7.1 Incremental verification

Goal: the cost of verifying a change is proportional to its impact set, not to the repository. Mechanisms, in priority order:

1. **Evidence reuse.** Evidence keyed by `(snapshot, toolchain, command, scope)`. Identical inputs → identical outputs; skip.
2. **Test selection.** `Tests(t, a)` edges select tests whose targets intersect the impact set. Initially from adapter heuristics (a test references `foo` → it tests `foo`). Refined by **coverage evidence**: when tests run with coverage instrumentation (`cargo llvm-cov`, OPEN), the observed edges replace the heuristic ones.
3. **Build-graph pruning.** `Depends` edges at crate level restrict `cargo check` to affected packages.
4. **Fallback.** If the impact set exceeds a policy threshold or the adapter is Tier ≤ 1, run everything.

Selection accuracy is a tracked metric (§10). A test-selection miss (a selected-out test would have failed) is a bug of the highest severity.

### 7.2 Policy

Policies are stored in the repository at `.hord/policy.toml` and versioned like everything else. A policy is evaluated at landing against the change and its evidence.

```toml
[land]
require = ["check", "test:selected", "lint"]   # evidence kinds that must be Pass
strict_reads = false                            # write-read conflicts (§6.3)
max_write_set = 200                             # definitions; larger changes need review
max_replay_attempts = 2

[[rule]]
name = "unsafe requires human"
when = { touches_kind = "unsafe_block" }
require = ["review:human"]

[[rule]]
name = "public API"
when = { touches_visibility = "pub", paths = ["crates/hord-core/**"] }
require = ["review:human", "test:full"]

[[rule]]
name = "agent-authored large changes"
when = { actor = "agent", write_set_gt = 50 }
require = ["review:agent-reviewer", "bench:no-regression"]
```

`review:*` evidence is produced by a reviewer (human or agent) signing an `Evidence { kind: Review }` object against the exact snapshot. **OPEN:** a richer policy language (Rhai, Starlark, or WASM) if TOML proves insufficient by M5.

---

## 8. Storage, index, and distribution

### 8.1 Local store

- **Objects:** content-addressed, packed into append-only pack files with an offset index. Zstd-compressed with per-kind dictionaries. Loose objects for recently written data, packed by a background job.
- **Index:** an embedded KV store (`redb`, pure Rust, ACID). Tables: `node_history: NodeId → Vec<ChangeId>`, `edges_<snapshot>`, `identity_<snapshot>`, `evidence_by_snapshot`, `log`, `refs`. The index is fully rebuildable from objects; corruption is recoverable.
- **Locality:** the store is a directory (`.hord/`) alongside a git repository when bridged, or standalone.

### 8.2 Remote

v1 remote is a **hord server** (§10.5) hosting one or more repositories, each with its own lander. It exposes:

- Object fetch/push (batched, content-addressed, lazy — clients fetch what they materialize).
- The lander queue (submit, status, arbitration).
- Index queries (history, blame, edges) so thin clients need not rebuild indexes.
- An event stream (landing, verification, conflict, arbitration events) for live clients.
- The web presentation layer (§10.4).

Protocol: HTTP/2. Object transfer uses canonical CBOR (§3.9); everything else is JSON, so the web UI and `hord --json` share one schema (§10.5.2).

### 8.3 Partial and lazy checkout

A workspace never needs the whole repository. Materialization pulls trees on access. Agents operating on a 10M-line monorepo touch a few hundred nodes. This is the EdenFS/CitC model and is the core performance story.

### 8.4 Distribution (M7+, OPEN)

Objects are content-addressed and signed, so any node can serve them; the only centralized component is the lander. Design space for later: mirrored landers with leader election; subtree-partitioned landers; offline-first clients that queue submissions. Not v1.

---

## 9. Git bridge

Required from M0. Two directions:

- **Import:** walk git history; each commit → a `Snapshot` via adapters (Tier 0 for everything at M0; Tier 1/2 for Rust from M1/M2). Each commit → a `ChangeRecord` with `intent.summary = commit subject`, `intent.body = commit body`, `provenance.actor = Human { git author }`, empty evidence, and read/write sets derived from the diff. Merge commits get multiple `parents`. The git SHA is recorded as an `IntentRef::GitCommit`.
- **Export:** project a `Snapshot` to a git tree; a landed change becomes a commit with a structured trailer block (`Hord-Change: <id>`, `Hord-Intent: <summary>`, `Hord-Actor: ...`). Exported commit trees are byte-identical to the projection.
- **Sync:** a bridge daemon keeps a git remote as a mirror of the log. Incoming git pushes to the mirror are imported as proposals (Tier 0 read/write sets) and go through the lander like any other change — this is how humans on git tooling coexist with hord during adoption.

Round-trip invariant, tested in CI on real repositories (`rust-lang/cargo`, `tokio-rs/tokio`, hord itself): `export(import(repo))` reproduces every tree SHA.

---

## 10. Interfaces

### 10.1 Library API (`hord` crate)

The CLI, server, and any agent harness use the same API. Primary types: `Repo`, `Workspace`, `ChangeRecord`, `Lander`, `Query`. Every operation is async and cancellable. No global state.

### 10.2 CLI (`hord-cli`)

```
hord init [--from-git <path>]         create a repository (optionally importing git history)
hord ws new [--base <snap|ref>]        create workspace, print id and materialization path
hord ws list | rm | gc
hord status [-w <ws>]                  ops, read/write sets, staleness of evidence
hord verify [-w <ws>] [--plan-only]    run the verifier plan locally, attach evidence
hord propose [-w <ws>] --intent <file> build ChangeRecord
hord submit <change>                   send to lander
hord queue [--mine]                    lander status
hord land --local <change>             single-user mode: run the lander inline
hord log [--node <NodeId|name>] [--path <p>] [--actor <a>] [--since <t>]
hord blame <name|path:line>            semantic blame: change, intent, actor, evidence
hord show <change|snapshot|node>
hord query <edge> <node>               references, dependents, tests-of
hord conflicts <change>                explain a conflict report
hord git export <ref> | import <ref> | sync
hord policy check [-w <ws>]            dry-run policy against current workspace
hord replay <change> --harness <cmd>   run the replay protocol manually
hord review <change> --as <kind> [--approve|--reject] [-m <msg>]
                                       sign Review evidence against the change's snapshot
hord arbitrate <change> --pick ours|theirs | --edit | --replay
                                       resolve a parked conflict
hord remote add|rm|list <name> <url>   configure a hord server as a remote
hord login <remote>                    obtain and store a scoped token
hord watch [--queue|--change <id>]     tail the event stream (SSE) in the terminal
hord serve [--repo <path>|--root <dir>] [--bind <addr>]
                                       run the hord server: lander, API, web UI
```

All commands support `--json` for agent consumption. Human-oriented output is secondary. Every command that reads or mutates repository state works identically against a local `.hord/` store or a configured remote; the CLI is a thin client over the same API the server exposes (§10.5).

### 10.3 Agent-facing conventions

- `hord status --json` is the canonical "what am I about to propose" call.
- `hord verify --plan-only --json` tells an agent which tests will run so it can run them itself first.
- Intent files are Markdown with a YAML front matter block (`summary`, `refs`, `acceptance`). Agents should write intent **before** editing; the harness may enforce this.

### 10.4 Presentation layer (human in the loop)

Hord is agent-native and **human-governed**. Humans sit at three structural points — policy-required review (§7.2), arbitration (§6.4), and git-side collaboration via the bridge (§9) — and the presentation layer is where they stand to do it. It is also how the system is demonstrated.

**Governing rule (DECIDED):** the UI consumes only the public JSON API (§10.5.2), the same one `hord --json` and agent harnesses use. It has no privileged data path. This keeps the UI honest about what agents can see and makes it a living test of the API.

**Views**, in order of demo value:

| View | What it shows | Primary user action |
|---|---|---|
| **Landing strip** | Live lander queue: proposed → verifying → landed / replaying / arbitration. Rows are labeled by intent summary and actor. Verification progress and evidence appear as they arrive. | Watch; click through to a change. |
| **Semantic change** | A change as `intent` + `ops` on named definitions + `evidence` + provenance. The text diff is a secondary tab, never the default. Read/write sets are visible. | Review: sign `Review` evidence (approve/reject with message). |
| **Arbitration workbench** | The conflict report: both intents, both sides' ops on the contested `NodeId`s, the ladder history (which rungs were tried, replay attempts and their outcomes). | Pick ours / pick theirs / open a workspace to edit / re-run replay with a note. |
| **Node lineage** | One definition's history: every change that touched it, with intent, actor, evidence, renames, splits, moves. Semantic blame. | Navigate; jump to any change or snapshot. |
| **Provenance trace** | One change's story as a timeline: intent written → proposed → conflict → replay(s) → verification → landed. | Audit; export as JSON. |
| **Repository browser** | Snapshot tree with file projection, plus the graph view: definitions and their `References`/`Tests` edges. | Navigate; open a node's lineage. |

**Flight recorder.** The lander emits an append-only event log (the same events as the stream in §10.5.3). The M3 concurrency simulation writes one; the UI can replay any recorded log at any speed with a scrubber. This is the demo mechanism: a recorded run of 100 agents landing changes, with conflicts resolving on the landing strip, replayable without live agents. Recordings are stored as `Blob`s in the repository under `.hord/recordings/` so they ship with the code.

**Human actions are evidence, not side channels.** Approving, rejecting, and arbitrating all produce signed `Evidence` or `ChangeRecord` objects through the API. The UI never mutates state except by creating objects any CLI user could create.

**Stack (DECIDED for M5, ADR before M7):** server-rendered HTML (`askama` templates) with server-sent events for live regions and minimal vanilla JS. No frontend build pipeline. Rationale: one language, one binary, agents can modify it without a Node toolchain. A richer client (Leptos/Dioxus or TypeScript) is an ADR once the views stabilize.

**Not in the UI:** issues, discussion threads, permissions administration, CI dashboards. Link out; do not build.

### 10.5 Hosting server and API

#### 10.5.1 Shape

`hord serve` is a subcommand of the single `hord` binary, backed by the `hord-server` crate. It embeds the same `hord` library the CLI uses; there is no separate server codebase. It runs:

- one **lander** per hosted repository (§6.7), as a tokio task with its own queue;
- the **object service** (fetch/push, lazy);
- the **query service** (index-backed: history, blame, edges, snapshots);
- the **event stream** (§10.5.3);
- the **web UI** (§10.4), served from `/`;
- the **git bridge daemon** (§9) if configured.

Modes: `hord serve --repo <path>` hosts one repository; `hord serve --root <dir>` hosts every `.hord/` under a directory, namespaced as `/r/<name>/`. State lives in each repository's `.hord/`; the server is stateless beyond that plus a small `server.toml` (bind address, auth config, remotes). **OPEN (M7):** an object-storage backend (S3-compatible) for the pack store.

#### 10.5.2 Local/remote symmetry

The library exposes one trait:

```rust
#[async_trait]
pub trait RepoBackend: Send + Sync {
    // objects
    async fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<Object>>;
    async fn put_objects(&self, objs: Vec<Object>) -> Result<()>;
    async fn has(&self, ids: &[ObjectId]) -> Result<Vec<bool>>;
    // log & refs
    async fn head(&self) -> Result<ChangeId>;
    async fn log(&self, q: LogQuery) -> Result<Page<ChangeSummary>>;
    async fn refs(&self) -> Result<BTreeMap<String, ChangeId>>;
    // lander
    async fn submit(&self, change: ChangeId) -> Result<SubmissionId>;
    async fn queue(&self, q: QueueQuery) -> Result<Vec<QueueEntry>>;
    async fn arbitrate(&self, change: ChangeId, action: Arbitration) -> Result<ChangeId>;
    // queries
    async fn node_history(&self, node: NodeId) -> Result<Vec<ChangeSummary>>;
    async fn edges(&self, snap: SnapshotId, node: NodeId, kind: EdgeKind) -> Result<Vec<NodeId>>;
    async fn resolve_name(&self, snap: SnapshotId, name: &str) -> Result<Vec<NodeId>>;
    // evidence
    async fn attach_evidence(&self, change: ChangeId, ev: Evidence) -> Result<ObjectId>;
    // events
    async fn events(&self, from: Option<EventCursor>) -> Result<EventStream>;
}
```

`LocalRepo` implements it against `.hord/` directly; `RemoteRepo` implements it as an HTTP client. The CLI holds a `Box<dyn RepoBackend>` and does not know which it has. The server is `axum` routes that call `LocalRepo`. Consequence: every CLI command is automatically a remote command, the API is defined by the trait, and the trait is tested once against both implementations.

**Wire format:** `/api/v1/**` is JSON with `serde` types shared in `hord-api`; `/objects/**` is canonical CBOR (batched `get`/`put`/`has`). All JSON types derive `schemars::JsonSchema`; the server publishes the schema at `/api/v1/schema.json` for agent harnesses.

Route sketch (per repository, prefixed `/r/<name>` in multi-repo mode):

```
GET  /api/v1/head
GET  /api/v1/log?after=&actor=&node=&path=&limit=
GET  /api/v1/changes/{id}                     ChangeRecord + resolved names + evidence
GET  /api/v1/changes/{id}/diff?format=ops|text
POST /api/v1/changes/{id}/evidence            attach Review or other evidence
POST /api/v1/submit                            { change: ChangeId }
GET  /api/v1/queue
GET  /api/v1/queue/{submission}
POST /api/v1/arbitrate/{change}                { action: pick_ours|pick_theirs|replay|resolved:<ChangeId> }
GET  /api/v1/nodes/{id}/history
GET  /api/v1/nodes/{id}/edges?kind=
GET  /api/v1/snapshots/{id}/tree?path=
GET  /api/v1/snapshots/{id}/file?path=         projection
GET  /api/v1/resolve?snapshot=&name=
GET  /api/v1/events?from=                      SSE
GET  /api/v1/recordings/{id}                   flight-recorder event log
POST /objects/get | /objects/put | /objects/has   CBOR
GET  /                                          web UI (§10.4)
```

#### 10.5.3 Event stream

The lander and verifier emit typed events; the server fans them out over SSE. This one stream feeds the terminal (`hord watch`), the landing strip, the flight recorder, and webhooks.

```rust
pub enum Event {
    Submitted   { submission: SubmissionId, change: ChangeId, actor: Actor },
    ConflictCheck { change: ChangeId, result: ConflictOutcome },
    Verifying   { change: ChangeId, plan: VerifyPlanSummary },
    EvidenceAttached { change: ChangeId, evidence: ObjectId, kind: EvidenceKind, result: EvidenceResult },
    Replaying   { change: ChangeId, attempt: u32, harness: String },
    Parked      { change: ChangeId, reason: ParkReason },          // needs arbitration / review
    Arbitrated  { change: ChangeId, by: Actor, result: ChangeId },
    Landed      { change: ChangeId, position: u64 },
    Rejected    { change: ChangeId, reason: String },
    HeadMoved   { from: ChangeId, to: ChangeId },
}
```

Events carry an `EventCursor` (monotonic per repository) so clients resume after disconnect. **Webhooks:** `server.toml` may list URLs to POST events to, filtered by kind — this is how chat notifications ("your change is parked") and external CI hook in without hord growing a notification system.

#### 10.5.4 Identity and authorization

- **Actors** authenticate with bearer tokens. Humans obtain them via `hord login` (OIDC against a configured provider, or a local user table for single-team deployments). Agents receive tokens minted by an operator, bound to an `Actor::Agent { id, model, harness }` so provenance is set by the server from the token, not self-reported.
- **Scopes:** `read`, `propose` (put objects, submit), `review:<kind>`, `arbitrate`, `admin`. A typical agent token is `read + propose`. A reviewer-agent token is `read + review:agent-reviewer`. Policy rules (§7.2) reference the same review kinds, so "who may sign what" is enforced at the API, not in the UI.
- **Signatures:** `ChangeRecord.signature` and `Evidence` are signed with the actor's key (Ed25519). The server verifies on ingest and records the key id in provenance. Keys for agents are generated at token mint time; keys for humans live in `~/.hord/keys/`.
- Authorization is coarse in v1. Path-level permissions are handled by policy rules (`require review:human` on a path), not by ACLs. **OPEN (M7):** per-subtree ownership as a first-class concept.

#### 10.5.5 How a CLI session flows through the server

```
hord remote add origin https://hord.example/r/hord
hord login origin
hord ws new --base origin/main        # fetches root tree, lazy objects on access
  ... edit ...
hord verify                           # local evidence, attached to the (not yet submitted) change
hord propose --intent intent.md       # builds ChangeRecord locally; pushes new objects to /objects/put
hord submit <change>                  # POST /api/v1/submit; returns submission id
hord watch --change <change>          # SSE: ConflictCheck → Verifying → Landed | Parked
```

If parked, a human opens the arbitration workbench (or runs `hord arbitrate`), and the resolution lands as a new change with both parents. Nothing in this flow is different for an agent except that `hord watch` is replaced by polling `/queue/{submission}` or subscribing to `/events`.

---

## 11. Crate layout

```
hord/
  Cargo.toml                 workspace
  crates/
    hord-encoding/           canonical CBOR, ObjectId, golden vectors
    hord-core/               Node, Tree, Snapshot, ChangeRecord, Op, Evidence, Policy types
    hord-store/              packfiles, redb index, GC
    hord-lang/               LangAdapter/Verifier traits, registry, tree-sitter generic Tier 1
    hord-lang-rust/          Rust Tier 2 adapter
    hord-lang-toml/          TOML Tier 1 + Cargo.lock adapter
    hord-diff/               structural diff, 3-way merge, blob merge
    hord-identity/           NodeId assignment, identify() default, IdentityMap
    hord-txn/                Workspace, AccessLog, propose, conflict check, Lander
    hord-verify/             evidence, impact set, plan/reuse, Verifier orchestration
    hord-verify-rust/        cargo check/test/clippy/bench runner
    hord-policy/             policy parsing and evaluation
    hord-git/                import/export/sync via gitoxide (`gix`)
    hord-vfs/                directory materializer + watcher; FUSE later (only crate allowed `unsafe`)
    hord-replay-ref/         reference replay harness (shells out)
    hord-api/                RepoBackend trait, JSON wire types, Event, JSON Schema export
    hord-remote/             RemoteRepo: HTTP client implementing RepoBackend
    hord-server/             axum routes over LocalRepo, lander tasks, SSE, webhooks, auth
    hord-ui/                 askama templates + static assets for §10.4; depends only on hord-api
    hord-cli/                CLI (includes `hord serve`)
    hord/                    facade crate re-exporting the public API
  docs/
    adr/                     architecture decision records, numbered
    spec.md                  this document
  bench/                     corpora and benchmark harnesses
  corpora/                   merge-conflict test cases (see §12)
```

### 11.1 Engineering conventions (DECIDED)

- Rust 2024 edition. MSRV = current stable at project start; bump freely.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` gate every landing.
- `#![forbid(unsafe_code)]` in every crate except `hord-vfs`.
- Property tests (`proptest`) for: encoding round-trip, parse/project losslessness, `apply(base, diff(base, result)) == result`, merge commutativity where specified.
- Every public type documented. `cargo doc` warnings are errors.
- Errors: `thiserror` in libraries, `anyhow` only in `hord-cli`.
- Async: `tokio`. No blocking I/O in async contexts.
- One ADR per DECIDED/OPEN item resolved. ADRs are short (problem, options, decision, consequences).
- No hand-rolled parsers, hashes, or serialization formats. Use `tree-sitter`, `blake3`, canonical CBOR.
- Deterministic everything: same inputs → same `ObjectId`s across machines and runs. Non-determinism is a P0 bug.

Dependencies to prefer: `tree-sitter`, `tree-sitter-rust`, `tree-sitter-toml`, `blake3`, `redb`, `zstd`, `gix`, `tokio`, `hyper`/`axum`, `proptest`, `clap`, `serde`, `ulid`, `diffy`. Additions require a one-line justification in the ADR log.

---

## 12. Milestones

Each milestone has acceptance criteria that are **executable**. A milestone is done when its acceptance suite passes in CI, not when the code "exists."

### M0 — Skeleton and git round-trip (Tier 0)

Deliver: workspace layout, `hord-encoding`, `hord-core` types, `hord-store`, `hord-git` import/export at Tier 0 (all files as blobs), `hord-cli` with `init`, `ws new`, `status`, `log`, `git export/import`.

Accept:
- `export(import(repo))` reproduces every tree SHA for: hord itself, `rust-lang/cargo` (full history), `tokio-rs/tokio` (last 2,000 commits).
- Import throughput ≥ 200 commits/s on cargo on a developer laptop.
- Golden test vectors for encoding pass; `ObjectId`s are identical on Linux/macOS/Windows.

### M1 — Structural tier for Rust (Tier 1)

Deliver: `hord-lang` generic tree-sitter adapter, `hord-lang-rust` Tier 1, `hord-lang-toml`, `hord-diff` structural diff and 3-way merge, trivia attachment.

Accept:
- Losslessness: `project(parse(f)) == f` for every `.rs` and `.toml` file in the M0 corpora. Any failure is a blocker.
- `apply(base, diff(base, result)) == result` under `proptest` and on all consecutive commit pairs in the corpora.
- **Merge corpus:** real git conflicts mined into `corpora/merges/`. A case is scored only when the merge commit equals `git merge-file --ours` and no conflict hunk overlaps two disjoint definitions (ADR 0006). Cargo and tokio contain 61 such cases; that count is the floor (the original 200 included hand edits and coarse hunks). Target: hord auto-resolves ≥ 70% of scored cases, with 100% of auto-resolutions parsing and ≥ 95% matching the labeled resolution. Cases where hord resolves differently from the label are reviewed and either the label or the merge rules are fixed via ADR.

### M2 — Identity and semantics (Tier 2)

Deliver: `hord-identity`, Rust Tier 2 adapter (definitions, qualified names, references, module resolution, test heuristics), edge index, `hord blame`, `hord query`, `hord log --node`.

Accept:
- **Identity stability:** over cargo's history, ≥ 97% of definitions that a human would call "the same function" across consecutive commits keep their `NodeId` (measured on a 500-definition labeled sample). Rename detection precision ≥ 95%.
- Reference edges: on a labeled sample of 300 definitions, recall ≥ 95% against a syntactic name walk (ADR 0011). A site counts when a path, an unresolved call or selector, or an immediately preceding attribute names the definition without types or macro expansion. Precision is secondary; over-approximation is acceptable. rust-analyzer find-references is printed and is not the pass/fail bar.
- `hord blame` on any definition in cargo answers in < 50 ms from a warm index.
- ADR 0009: do not embed rust-analyzer crates. ADR 0011: the recall gate is the syntactic walk, not find-references.

### M3 — Transactions and the lander

Deliver: `hord-txn` (workspaces, access logging, propose, conflict check, structural rebase, single-process lander), `Cargo.lock` adapter, `hord submit/queue/land --local/conflicts`.

Accept:
- **Concurrency simulation:** a harness spawns 100 synthetic agents against a snapshot of cargo, each making a randomized edit to 1–5 definitions (chosen to be 80% disjoint, 20% overlapping) and submitting. Measure: landing throughput (target ≥ 20 changes/s with verification stubbed), false-negative conflicts (must be 0 — checked by an oracle that knows the true overlap), false-positive rate (target ≤ 10%).
- Structural rebase resolves every disjoint-set change without invoking replay.
- Workspace creation < 5 ms; 1,000 live workspaces on one machine without degradation.
- `Cargo.lock` concurrent dependency additions merge cleanly.

### M4 — Verification and policy

Deliver: `hord-verify`, `hord-verify-rust`, evidence store, impact sets, test selection, evidence reuse, `hord-policy`, `hord verify`, `hord policy check`. Lander now runs verification and policy.

Accept:
- **Selection safety:** on 500 real cargo commits, run (a) full `cargo test` and (b) hord-selected tests. Zero cases where (a) fails and (b) passes.
- **Selection efficiency:** median selected test count ≤ 20% of the full suite for changes with write_set ≤ 5.
- Evidence reuse: resubmitting an unchanged change against an unchanged head re-runs nothing.
- Policy examples in §7.2 are enforced in the concurrency simulation; violations are rejected with a machine-readable reason.
- **Server foundation:** `hord-api` trait and wire types, `LocalRepo`, `RemoteRepo`, `hord serve` with object service, query service, submit/queue, and the SSE event stream. The `RepoBackend` conformance suite passes against both implementations. The concurrency simulation from M3 runs against a remote server with 100 clients and produces a flight-recorder log.

### M5 — Replay, arbitration, and the human loop

Deliver: replay protocol, `hord-replay-ref`, arbitration queue, conflict summaries, `hord replay/review/arbitrate/watch`; token auth and scopes (§10.5.4); web UI views 1–3 (landing strip, semantic change, arbitration workbench) plus flight-recorder playback; webhooks.

Accept:
- **Conflict corpus:** 100 intent-bearing conflict cases (two synthetic agent tasks with overlapping write sets, each with a natural-language intent and an acceptance test). With a reference harness backed by a configurable model, ≥ 60% resolve through replay with acceptance tests passing; the remainder land in arbitration with a summary a human rates as "sufficient to resolve" ≥ 90% of the time.
- Replay budget enforcement: no replay exceeds its cost budget.
- **Arbitration round-trip:** every parked case in the conflict corpus can be resolved from the workbench, and the resolution lands as a change with both parents and a signed `Arbitrated` event.
- **Review round-trip:** a policy rule requiring `review:human` blocks landing until a human signs from the UI or `hord review`; the resulting `Evidence` is verifiable with the signer's public key.
- **Demo:** the M4 flight-recorder log plays back on the landing strip end-to-end, and a person unfamiliar with hord can explain from the UI alone why a given change was parked (usability check, 5 participants).
- The UI issues no request that is not in `/api/v1/schema.json` (enforced by a proxy in the UI test suite).

### M6 — Self-hosting

Deliver: hord's own repository is a hord repository, served by `hord serve` on a team host. The git mirror (`hord git sync`) is what GitHub sees. All contributions — human and agent — go through the lander. Web UI views 4–6 (lineage, provenance trace, repository browser).

Accept:
- Thirty consecutive days of development with zero manual git operations by the core team.
- Every change in that window has an intent, provenance, and passing evidence.
- Every human review and arbitration in that window was performed through the UI or CLI against the server, not by editing the store.
- Bridge sync never diverges (checked hourly).

### M7 — Scale and breadth (OPEN, sequenced by need)

Candidates: FUSE VFS; partitioned landers; second Tier 2 adapter (TypeScript or Python) to validate the adapter abstraction; WASM adapters; rust-analyzer-precision references; coverage-refined test edges; mirrored landers.

---

## 13. Metrics to track from M1 onward

| Metric | Definition | Why |
|---|---|---|
| Auto-merge rate | conflicts resolved at ladder rung 1 / total conflicts | core value proposition |
| False-negative conflicts | changes that landed and broke a concurrent change's assumptions | correctness; must be 0 |
| Identity churn | definitions whose `NodeId` changed without a rename/move op | blame quality |
| Selection safety | selected-out tests that would have failed | correctness |
| Verification cost ratio | selected work / full work | performance |
| Land latency | submit → land, p50/p95 | agent throughput |
| Replay success | rung-2 resolutions with passing acceptance / attempts | agentic value |
| Round-trip drift | git export mismatches | interop |

---

## 14. Open questions

Each should become an ADR. Listed roughly in the order they will block progress.

1. Identity heuristics: similarity metric and threshold for rename detection (§3.4). Blocks M2.
2. Diff algorithm choice and cost model (§5.1). Blocks M1.
3. rust-analyzer as Tier 2 backend: decided by ADR 0009 (do not embed the crates).
4. Read-set collection fidelity: how much to trust access logs vs. adapter references vs. declarations; whether to require declarations from agents. Blocks M3.
5. Test selection for Rust below crate granularity (§4.2). Blocks M4 efficiency target.
6. Policy language beyond TOML (§7.2). Decide by M5.
7. Remote protocol (§8.2). Decide by M4.
8. Replay nondeterminism: how to compare two replays of the same intent; whether to run N replays and vote. Blocks M5.
9. Macro handling beyond opacity: whether stored expansions ever pay for themselves. M7.
10. Large generated files (bindings, protobuf output): store as blobs, or parse and pay the cost? Decide at M1 with a size cutoff.
11. Comments as first-class nodes with their own identity (for doc-blame)? M7.
12. Lander partitioning and cross-partition commits (§6.7). M7.
13. Richer UI client vs. server-rendered HTML (§10.4). ADR before M7.
14. Object-storage pack backend and multi-tenant hosting (§10.5.1). M7.
15. Per-subtree ownership as a first-class authorization concept vs. policy rules only (§10.5.4). M7.

---

## 15. Risks

- **Semantic merge is undecidable.** Mitigation: never land an unverified merge; the ladder is the design, not a fallback.
- **Read-set under-declaration.** An agent that reads through side channels (grep output it didn't route through hord) will under-declare. Mitigation: adapter-derived references are always included; VFS mode captures all reads; `strict_reads` policy for sensitive subtrees.
- **Adapter drift.** tree-sitter grammar updates change `NodeKind`s and break identity. Mitigation: pin grammar versions per snapshot in the `Toolchain` object; migration tooling re-identifies on upgrade.
- **Verification cost dominates.** If selection accuracy is poor, the lander is just a slow merge queue. Mitigation: M4 efficiency target is a gate; coverage-refined edges are the escape hatch.
- **Adoption.** No one switches VCS. Mitigation: the git bridge means hord is a layer first. M6 proves it on hord itself.
- **Scope creep toward a build system.** Mitigation: §1.2 non-goal; hord exposes the graph and invokes toolchains, nothing more.

---

## 16. Glossary

- **Node** — one syntax-tree element; definition-bearing nodes carry `NodeId`s.
- **Snapshot** — an immutable Merkle tree of the whole repository.
- **Change / ChangeRecord** — base → result with intent, provenance, read/write sets, evidence.
- **Workspace** — a mutable overlay on a snapshot; where work happens.
- **Land** — append a change to the log after conflict check, verification, and policy.
- **Lander** — the single-writer process that lands changes.
- **Replay** — re-executing a change's intent against a new base.
- **Arbitration** — human or designated-agent resolution of a conflict the ladder could not resolve.
- **Evidence** — a verification result keyed to an exact snapshot and toolchain.
- **Impact set** — write set plus its dependents, bounded by policy.
- **Tier** — level of language support: 0 blob, 1 syntax, 2 semantic, 3 verified.
- **Projection** — rendering a snapshot (or subtree) back to bytes on disk.
