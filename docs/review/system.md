# M3 system review (lane: architecture and system design)

Reviewer: **system**. Diff: `git diff 03e2310..b867df9`. Spec: `docs/spec.md`. ADRs 0012–0016.

Scratch programs are in `target/review-system/`:

- `probe/src/main.rs`: file rename, resurrection, identical births, and fields of a rebased record.
- `probe/src/bin/twins.rs`: two agents add the same definition.
- `probe/src/bin/carried.rs`: a carried rename compared with a fresh assignment.
- `blame/`: CLI blame after `land --local`.

Run a probe with `CARGO_TARGET_DIR=target/review-system/target cargo run -q [--bin X]` from `target/review-system/probe`.

M3's in-process path works. Every M3 gate in `target/m3-eval.json` is green. The design risks are at the seams M4–M6 depend on: where identity lives, what a landed record claims, how the lander talks to verification, and who may write the log. The four most severe items are reproduced. The rest are traced to file:line.

---

## Design risks, ranked

### 1. NodeIds are not a function of the snapshot's objects

**Bites:** M4 (RemoteRepo, conformance suite), and any index rebuild.

**File:line:** `crates/hord-txn/src/semantic.rs:143-150`, `crates/hord-store/src/queue.rs:97-121`, `crates/hord-txn/src/semantic.rs:44-47`.

**What is wrong.** A file's carried NodeIds come from an `IdentityIndex` object. The only thing that links a snapshot to that object is a redb row, `identity_index[snapshot] → object`. That row is written with `Durability::None`. The `IdentityIndex` object does not name its snapshot, and `rebuild_index` does not restore the row. When the row is missing, `identity_index()` silently returns `IdentityIndex::default()`, and every file falls back to a fresh content-derived assignment. So one `SnapshotId` can have two sets of NodeIds, depending on local redb state.

**Evidence** (`probe/src/bin/carried.rs`): `compute_total` is renamed to `sum_weighted` with the same body, and the rename lands. Rename detection carries the id `6G3X…MSNM`. A store holding the identical snapshot (`same snapshot id: true`) but no redb pointer gives `56DR…P9XR` for the same definition.

**Why it matters.** Spec §10.5.2 `RepoBackend` moves content with `get_objects` only. No method transfers this pointer. A remote client (M4, and M4 acceptance runs 100 remote clients) would therefore see fresh ids for every carried definition. It would build read and write sets over ids the server does not use. Read-write conflicts on renamed or carried definitions would then be missed, which is a false negative (must be 0). Write-write conflicts are usually caught later by the 3-way file merge; read-write conflicts have no second check. The same happens locally after index loss, which breaks spec §8.1 ("index is fully rebuildable from objects").

**Fix.** Make identity reachable from the snapshot's object graph. Either:

- (a) a `Snapshot` object `{ root: Tree, identity: IdentityIndexId, … }` as in spec §3.2, with `SnapshotId` defined as its id (this needs the ADR in the ADR challenges section, because §3.1 says `SnapshotId` is the root tree id); or
- (b) `NodeFile`-style tree entries that carry the `FileIdentity` id next to the blob.

In both cases, a missing identity for a snapshot the log names must be an error, never a silent fresh assignment.

**Effort:** L. It changes snapshot ids, and it should land before M4 fixes the wire format.

### 2. Two identity systems: lander-produced snapshots are invisible to blame, log --node, and query

**Bites:** now (M2 commands on an M3 repo), M4 (the `node_history`, `resolve_name`, and `edges` methods of `RepoBackend`), and M6.

**File:line:** `crates/hord-cli/src/resolve.rs:239-277`, `:320-340` (reads `store.identity_map`). The only writer of `put_identity` / `put_edge` is `bench/m2-eval/src/snapshot.rs:287`. hord-txn writes only `set_identity_index`.

**What is wrong.** M2's query path reads the store's `identity` and `edges` tables. The lander never writes them. The lander keeps its own identity in `identity_index` and computes references on the fly. `hord init --from-git` writes neither.

**Repro** (`target/review-system/blame/`): `hord init --from-git` → `ws new` → edit `beta` → `propose` → `land --local` (lands) → `hord blame beta`, `hord blame src/lib.rs:6` and `hord log --node beta` all fail with `cannot resolve … no identity map in the log`. On a store that has older M2 identity maps, the commands would instead resolve against the older snapshot's NodeIds. Before this diff those were random ULIDs. They differ from hord-txn's content-derived ids, so lineage breaks at the M3 boundary with no migration and no store format version.

**Fix.**
- Pick one identity source of truth, the per-snapshot identity from item 1.
- Have the lander (and git import) write it. Derive the `identity` and `edges` tables from it.
- Move `resolve.rs`'s name, line, and blame resolution out of `hord-cli`, which uses anyhow, into a library `Query` type (spec §10.1 names `Query`).
- Add an integration test: land through `hord land --local`, then run `hord blame`.

**Effort:** M.

### 3. A rebased record copies the submitted record's sets, deltas, provenance, and signature, so it can misstate what it did

**Bites:** now (`node_history` is wrong), M4 (evidence), M5 (signatures, provenance trace).

**File:line:** `crates/hord-txn/src/lander.rs:343-352` (`ChangeRecord { base, result, parents, ops, ..record }`).

**What is wrong.** When a change lands on a head other than its base, the lander writes a new record and lands it under a new `ChangeId`. Only `base`, `result`, `parents`, and `ops` are recomputed. `write_set`, `read_set`, `identity_deltas`, `provenance`, `evidence`, and `signature` are copied. `probe` case 4 prints `same provenance/read_set/write_set/deltas: true` and `parent_intent: None`.

**Evidence** (`probe/src/bin/twins.rs`): two agents add the same `pub fn helper` at different places in `a.rs`. Content-derived birth ids (item 9) give both births the same id, `62G8…JK0`. The second change hits a write-write conflict, rebases cleanly (no merge conflict at all) and lands. In the landed snapshot, the second `helper` was re-salted to `62G8…MDKN`. But the landed record still says `Birth { 62G8…JK0 }` and `write_set` contains `…JK0`. Results: `node_history(…JK0)` has 2 changes, `node_history(…MDKN)` has 0, and the landed snapshot has two `helper` definitions (E0428) with no flag in `report.merge`.

**Further consequences:**
- A copied `signature` (§10.5.4) covers the submitted record, not this one, so M5's "verify on ingest" rejects every rebased change, or skips verification for them.
- `provenance.actor` claims the agent authored ops the lander computed.
- The only submitted → landed link is the redb queue row. That row is not an object and cannot be rebuilt.

**Fix.**
- Recompute `write_set` and `identity_deltas` for the rebased record from `head → result`, the same way propose does.
- Clear `signature` and add a lander attestation: either a lander-signed field, or `Evidence{kind: Custom("rebase")}` naming the submitted id.
- Record the submitted id in the object. `parent_intent` does not fit (§6.6 reserves it for replay), so this needs a field or `IntentRef`, decided in an ADR.

**Effort:** M.

### 4. The lander cannot pipeline or speculate, and the Verifier API cannot return evidence

**Bites:** M4.

**File:line:** `crates/hord-txn/src/lander.rs:175-205` (serial loop), `:136-142` (the doc claims an implementation "may run checks concurrently or speculatively"), `:343-352` (landed record `put_object` before verify), `:111-120` (`Verdict` has no evidence).

**What is wrong:**
- `run` prepares change N, awaits `verify(N)`, finishes N, and only then prepares N+1. A `Verifier` sees one request at a time, so it cannot speculate on its own, and the doc comment is wrong.
- Spec §6.7 requires verifying N+1 against `head + N` before N lands. Preparing N+1 on a candidate snapshot would call `identity_index(candidate)`. No pointer is stored for that snapshot until `finish`, so N+1 would silently get fresh ids (item 1).
- Spec §6.5 says "fresh evidence is attached before landing", but `ChangeRecord.evidence` is part of the hash, and the landed record is already stored before verification. Attaching evidence means yet another id, or evidence kept outside the record (`evidence_by_snapshot`, `RepoBackend::attach_evidence`), and nothing decides which.
- A failed verification leaves the rebased record stored as an orphan.

**Fix.**
- Split the lander into `prepare(entry, on: Candidate|Head) → Candidate` and `commit(candidate)`. Keep a window of K candidates stacked on each other, and put each candidate's `IdentityIndex` into the in-memory cache when it is prepared.
- Change the verifier signature to `Verdict::Pass { evidence: Vec<ObjectId> } | Fail { evidence, reason }`.
- Write an ADR on where evidence lives: per snapshot, out of the record hash. That is consistent with §3.6 "Evidence is valid for a snapshot, not a change".
- Store the landed record only on commit.

**Effort:** M (before M4 verification lands).

### 5. The local store admits one process, so concurrent CLI agents fail

**Bites:** now, for Directory and CLI agents, and in M4's local mode (spec §10.2: "works identically against a local `.hord/`").

**File:line:** `crates/hord-txn/src/repo.rs:117-121` (doc: "the redb index admits one process"). `hord-store` has no retry or wait.

**Repro:** 8 concurrent `hord ws new --json` in one repo → 7 exit 1 with `index: Database already open. Cannot acquire lock.`

**What is wrong.** Spec §10.3 makes `hord --json` the agent interface. The M3 simulation passes only because its 100 agents are tokio tasks in one process sharing one `Repo`. A human running `hord status` while an agent runs `hord propose` collides the same way.

**Fix.** Decide the local concurrency model in an ADR. Recommended: local mode is a per-repo daemon (`hord serve --repo` on a Unix socket, started on demand), and the CLI talks to it through the M4 `RemoteRepo` path. That gives the local/remote symmetry of §10.5.2 for free. Short of that, wait with a timeout on the redb lock.

**Effort:** M, since it rides on M4's `RemoteRepo`.

### 6. The public API matches §10.1 in names only and will not survive `RepoBackend` without restructuring

**Bites:** M4.

**File:line:** `crates/hord-txn/src/repo.rs:118-146` (a concrete `Repo` over `Store`, and `pub fn store()` at `:409`), `workspace.rs:73` (`Workspace { repo: Repo }` calls `Inner` and the local `Store` directly), `crates/hord-cli/src/txn.rs:87-386` (`EntryView`, `ReportView`, and `op_view` are the de facto JSON wire schema), `crates/hord-cli/src/resolve.rs` (the query logic).

**What is wrong:**
- **Present:** `Repo`, `Workspace`, `ChangeRecord`, and async methods.
- **Missing:** a `Lander` type. The lander is `Repo::land_local`, which drains and returns. It is not a long-running task that wakes on submit and emits §10.5.3 events.
- **Missing:** a `Query` type. Queries live in the CLI.
- **Wire types:** the `--json` shapes are defined ad hoc in `hord-cli`, while §10.5.2 requires one `hord-api` schema shared with the UI. Moving them in M4 changes what agents already parse.
- **Workspaces:** `Workspace` needs local objects and local redb. A remote workspace (§10.5.5 `ws new --base origin/main`, lazy) cannot reuse it without an object-source abstraction.
- **Pristine checkouts** (ADR 0016) need every blob of the base locally, which contradicts §8.3's lazy fetch for remote clients.
- **Cancellation:** all work runs in `spawn_blocking`. Dropping a future discards the result but does not stop the work, for example a whole-snapshot `RustCtx` build.
- **Global state:** none in hord-txn. The exception is item 7's global static.

**Fix.** Before M4 code:

- Split `Inner` into an object/identity reader trait (`get_objects`, `identity(snapshot)`) that both `LocalRepo` and `RemoteRepo` implement, plus a `LocalRepo`-only lander.
- Make `Workspace` generic over the reader, or hold it as `Arc<dyn …>`.
- Add a `Lander` handle (`spawn(cancel) → JoinHandle`, `subscribe() → EventStream`), with `land_local` as a thin wrapper.
- Move the wire views into `hord-api` now, so the `--json` shape changes once.
- Write an ADR for remote materialization: a sparse pristine checkout, or fetching all blobs on first use.

**Effort:** L.

### 7. Language-specific logic in hord-txn, and a process-global name index

**Bites:** M4 (`hord-verify-rust` needs the same context), M7 (a second Tier 2 adapter).

**File:line:**
- `crates/hord-txn/Cargo.toml` depends on `hord-lang-rust` and `hord-lang-toml`.
- `semantic.rs:33,262-290` builds a Rust-only `RustCtx` from `RustFile` and `ManifestFile`.
- `semantic.rs:300-316` hardcodes `RustAdapter`.
- `propose.rs:157,179` sets the `rust:` flag, so the reference hop runs only for Rust.
- `rebase.rs:458-462` dispatches `is_cargo_lock` and `merge_cargo_lock` by path.
- `crates/hord-lang-rust/src/resolve.rs:150-165`: `static CACHE: Mutex<Option<(u64, Arc<Index>)>>`.

**What is wrong:**
- AGENTS.md and ADR 0013 say language-specific logic belongs only in the adapter crate. A second Tier 2 adapter would get no ADR 0012 reference hop at all. Its read sets would silently lack references, and that under-declares.
- `Cargo.lock` merging is chosen by path in hord-txn, not by the adapter.
- The name index is one global slot keyed by context stamp. Two snapshots resolved alternately rebuild the whole index on every switch: proposes on different bases, the lander on head, or several repositories under `hord serve --root`. This breaks §10.1's "no global state". The global dates from M2, but M3 is its first multi-base caller.

**Fix.**
- Add `LangAdapter::snapshot_ctx(files) → Box<dyn Any>` (or a `SemanticCtx` trait) and `LangAdapter::merge_file(base, ours, theirs) → Option<MergeOutcome>`. Then hord-txn iterates adapters, never names Rust, and depends on `hord-lang-*` only in `default_adapters` (better in the `hord` facade).
- Move the name index into the context object, so it is per snapshot and per `Repo`.

**Effort:** M.

### 8. Renaming or moving a file loses every NodeId in it, contradicting ADR 0015

**Bites:** M2 metrics over real history (identity churn, §13) and M6 blame.

**File:line:** `crates/hord-txn/src/propose.rs:84-200` (`carry_in(adapter, path, base_ref, …)` runs per path, and `base_ref` is the same path's base only). `TreeOpKind::Rename` is never emitted (`grep Rename crates/hord-txn/src` finds only consumers).

**Evidence** (`probe` case 1): delete `src/a.rs` and write the same bytes to `src/b.rs`. Result: 2 deaths, 2 births, tree ops `[a.rs:Delete, b.rs:CreateFile]`, 0 moves. `foo` goes from `6EAW…` to `76X7…`.

ADR 0015's consequence says: "A rename is `Op::Tree Rename`, and its definitions `Move` from the old root to the new one." ADR 0007 says cross-file rename works only "when both files are in that pair of trees", and hord-txn never pairs two paths. Spec §3.4 rule 3 (Moved) therefore never fires across files. The path salt in birth ids guarantees that the new id differs.

**Fix.** In propose, pair files that were deleted and created in the same change, by blob equality first and then by definition-hash overlap. Carry across the pair, and emit `Tree Rename` plus `Move`. Or amend ADR 0015 to say file moves are birth plus death until an identified-move ADR. Either way, code and ADR must agree.

**Effort:** M.

### 9. Content-derived birth ids have no ADR, and they resurrect dead ids

**Bites:** M5 and M6 (lineage and blame).

**File:line:** `crates/hord-identity/src/assign.rs` (`stabilize_births`, `stable_birth_id`, and `assign_in`'s path salt), added in this range.

**What is wrong.** Spec §3.1 says a NodeId is "Assigned once, carried forward. ULID-encoded; the timestamp component is informational only." Births are now `f(content ObjectId, salt, path)`. The only ids reserved against reuse are ids live or dying in the same mapping.

**Evidence** (`probe` case 2): delete `foo`, land. Re-add an identical `foo`, land. It gets its old id, `6EAW…`, back, so `node_history(foo)` joins two unrelated lifetimes. Item 3 shows the concurrent version of the same problem: two independent births collide on one id.

Determinism is the reason for the change (AGENTS.md: "same inputs → same ObjectIds"). It is a real trade-off, but no ADR records it.

**Fix.** Write an ADR: content-derived birth ids, salted by path and by the change's base snapshot (or by a death registry), so that a definition that died can never be reborn under the same id. Note the §3.1 supersession in it.

**Effort:** S (ADR) plus S (salt).

### 10. The single-writer invariant is not enforced; the lander's head cache can drop a git sync

**Bites:** M6 (the git bridge daemon runs inside `hord serve`, §10.5.1).

**File:line:**
- `crates/hord-txn/src/repo.rs:216-236`: `Inner::head` is cached forever once set.
- `crates/hord-git/src/import.rs:111,319` calls `store.append_log` and `store.set_head` directly.
- `repo.rs:409` exposes `pub fn store()`.

**What is wrong.** Spec §6.7 makes the lander the single writer of the log. Suppose git import or sync appends to the log through the store while a `Repo` is open:

- `set_check` reads the fresh log.
- `prepare` rebases onto the cached, stale `head.snapshot`.
- `finish` calls `set_head(landed)` with `parents = [stale head]`.

The imported change stays in the log, but its content is missing from head's tree.

**Fix.** Make every log writer, git sync included, go through `Repo::submit` as a Tier 0 change. Make `Store::append_log` and `set_head` reachable only through the lander. Alternatively, have the lander check that `store.head()` matches its cache inside the same redb write transaction.

**Effort:** S–M.

### 11. The queue and `ConflictReport` are not enough for M5's replay and arbitration

**Bites:** M5.

**File:line:** `crates/hord-txn/src/lander.rs:35-52` (`QueueStatus`), `crates/hord-txn/src/conflict.rs:61-78` (`ConflictReport`), and `lander.rs:70-77` (the report lives only in the redb queue row).

**What is adequate.** For a rung-2 `ReplayRequest` the report covers most of what is needed: `change`, `base`, `head`, `checked_against`, and per-rule `SetConflict{kind, landed, nodes, paths}`. The colliding intents and ops can be looked up from the landed ids.

**What is missing:**
- `Conflicted` is terminal. There is no `Replaying{attempt}`, `NeedsArbitration`, or `Parked{reason}` (the §10.5.3 `ParkReason`), no attempt counter or budget for "max replay attempts default 2", and no link from a replayed change (`parent_intent`) back to the parked entry.
- `verification` is a `String`, not `Evidence` ids. §6.5 says "failure attached to the replay context".
- A hard `MergeConflict.nodes` can be empty ("when the merge names them"), and the arbitration workbench needs the contested NodeIds.
- The report is not an object. It cannot be fetched through `/objects`, cited by a replay request, or rebuilt, and the provenance trace view (§10.4) needs exactly that history.

**Fix.**
- Store `ConflictReport` as a content-addressed object, with the queue row pointing at it.
- Add ladder states and an attempts counter to `QueueStatus`.
- Make `verification: Vec<ObjectId>` (evidence).
- Require hard merge conflicts to name nodes. Fall back to the file root id.

**Effort:** M.

### 12. Storage grows without GC, and the identity index is flat per snapshot

**Bites:** M6 (30 days of self-hosting) and a long-running `hord serve` (M4).

**File:line:** `propose.rs:203` and `lander.rs:392` each store a full `IdentityIndex` per snapshot. `semantic.rs:44-66` is a flat `Vec<(RepoPath, ObjectId)>` that never shrinks: `propose.rs:153` calls `set(path, Some(..))` for every written parsed file, even when its ids equal the fresh assignment.

**Numbers.** On cargo HEAD, 2,087 files are parsed (`.rs`, `.toml`, `Cargo.lock`). Once every file has been written once, each `IdentityIndex` is about 194 KB of canonical CBOR (my estimate: the path encoding plus 34 B per id, computed over `git ls-tree`). A rebased landing stores two of them (the proposal's result and the landed result), so about 390 KB per landed change. That is about 3.9 GB per 10,000 changes, a third of it incompressible hashes.

**Other unbounded growth:**
- Two `ChangeRecord`s per rebased landing.
- The rebased record is stored before verify, and stays as an orphan when verification fails (item 4).
- The queue table keeps every entry and report forever.
- In memory, `Inner::footprints` (`repo.rs:141`) and `Inner::proposed` (`:143`) are never evicted.
- `hord-store` has no object GC, although spec §11 gives the store a GC.

**Fix.**
- Make the identity index Merkle-shaped: per directory, like `Tree`, so unchanged subtrees share objects. Or fold it into the snapshot's tree entries (item 1).
- Skip storing a `FileIdentity` whose ids equal the fresh assignment.
- Add a reachability GC rooted at the log, refs, live workspaces, and queued entries.
- Bound the two in-memory maps with an LRU.

**Effort:** M.

### 13. The default verifier lands unverified overlaps (§15 "never land an unverified merge")

**Bites:** now. `hord land --local` is usable before M4.

**File:line:** `crates/hord-cli/src/txn.rs:29-31` (`RepoOptions::default()` gives a `StubVerifier`) and `lander.rs:144-151`.

**Evidence.** `target/m3-eval.json`: `flagged 10, conflicted 0`. All 10 read-write and write-write overlap pairs landed. The item 3 repro landed a file with a duplicate `pub fn helper`. Spec §12 allows a stubbed verifier for the simulation's throughput number, not as the product default.

**Fix.** Make the default verifier fail closed: `Verdict::Fail` when `report.conflicts` or soft merges are present. The M3 harness opts into `StubVerifier` explicitly.

**Effort:** S.

---

## Crate boundaries (§11)

| Item | Current place | Verdict |
|---|---|---|
| Directory materializer (clone, pristine, stat index) | `hord-txn/src/materialize.rs` | §11 assigns it to `hord-vfs` ("directory materializer + watcher; FUSE later"). ADR 0016 decides the mechanism but not the placement. **hord-vfs is not needed now**: the code is safe Rust, and `reflink-copy` keeps it free of `unsafe`. Amend ADR 0016 to record the placement for now. Move it to `hord-vfs` behind a `Materializer` trait when M4's remote lazy materialization needs a second implementation (item 6). Effort S (ADR) now, M later. |
| Query and blame resolution | `hord-cli/src/resolve.rs` (anyhow) | Wrong crate. `RepoBackend` needs it in a library (item 2). |
| JSON wire views | `hord-cli/src/txn.rs` | Belong in `hord-api` (item 6). |
| Rust context, `Cargo.lock` dispatch | `hord-txn` | Belong behind the adapter trait (item 7). |
| `lander_queue`, `identity_index` tables | `hord-store/src/queue.rs` | Fine as opaque bytes. `identity_index` should become an object reference (item 1). |
| `Cargo.lock` bridge | `hord-lang-rust::cargo_lock` | Consistent with ADR 0013. |

## ADRs 0012–0016: consistency and gaps

- **0015 against the code.** Consequence 2 (a file rename is a `Tree Rename` plus `Move`) is not implemented (item 8). Amendment 2 (a Tier 0 blob-only change is a coarse write) holds (`conflict.rs` `coarse`).
- **0015 against 0007.** 0007 limits cross-file renames to a pair of trees. 0015 assumes moves across roots. hord-txn never builds such a pair.
- **0013 and AGENTS.md against hord-txn.** "Rust-specific logic lives only in `hord-lang-rust`" is broken by `rebase.rs:458` and `semantic.rs:262-316` (item 7).
- **0012.** It promises the one-hop reference read set for written definitions in general. The implementation runs it only for Rust (`propose.rs:157`). The ADR should say "adapters that provide references", or the code should become adapter-generic.
- **0016.** The amendment moves the §12 M3 "< 5 ms" gate to `begin` only, and gives `Directory` a new 100 ms p99 target. That is a change to an acceptance criterion made in an amendment, with spec §12 unedited (see ADR challenges). Placement against §11 `hord-vfs` is not addressed.
- **0014.** Consistent. hord-txn only uses `MergeMode::Lander` (`rebase.rs:490`, no `Corpus`). Gap: two births with the same qualified name in one scope do not conflict at §6.3 and do not produce a merge flag (item 3's repro). This relies entirely on M4 verification and should be noted in the ADR.
- **Decisions with no ADR:** content-derived birth ids (item 9), new ids for rebased changes and what they carry (item 3), identity kept outside the snapshot's objects (item 1).

## ADRs to write next

1. **Where snapshot identity lives.** A snapshot object with an identity pointer, or identity in tree entries. It resolves the §3.1/§3.2 ambiguity. Blocks M4's wire format.
2. **Rebased change records.** Which fields are recomputed, who signs, how the landed record names the submitted one, and how `node_history` treats both ids.
3. **Birth id derivation.** Content, path, and base salt; no rebirth of dead ids; the supersession of §3.1's "ULID, assigned once".
4. **Evidence placement and the Verifier contract.** Per-snapshot evidence outside the record hash, `Verdict` carrying evidence, and the speculative window (§6.5, §6.7).
5. **Local concurrency model.** A per-repo daemon with the CLI as a `RemoteRepo` client, or lock waiting (item 5).
6. **Remote materialization.** A pristine checkout against lazy fetch (§8.3, ADR 0016's full-checkout requirement).
7. **Lander queue states and the conflict report object**, for M5's ladder (item 11).
8. **Adapter hooks for semantic context and file merges**, to take Rust and `Cargo.lock` out of hord-txn (item 7).
9. **Store GC and retention**, for superseded records, orphans, queue rows, and identity indexes (item 12).
10. **File move identity**: pairing deleted and created files in propose (item 8), or an amendment to ADR 0015.

## §13 metrics: what can be measured today

| Metric | Measurable now? | Notes |
|---|---|---|
| Auto-merge rate (rung 1 / conflicts) | Partly | Only in the M3 simulation (`flagged 10`, `conflicted 0`). The simulation's overlapping edits all insert a new first statement, which the line merge always keeps both of, so it never produces a hard conflict and the rate is trivially 100%. Queue reports are in redb, not objects, so the rate cannot be computed from history. |
| False-negative conflicts | Only against an oracle | The M3 simulation oracle (ADR 0012 definition) reports 0. There is no production measure until M4 verification. Item 1 is a path to false negatives that the in-process simulation cannot exercise. |
| Identity churn | In harnesses only | M2 eval and the simulation's `identity_loss`. It is not computed over landed history. File moves (item 8), git-imported snapshots (no identity), and identity-pointer loss (item 1) all cause churn that nothing measures. |
| Selection safety | No | M4. |
| Verification cost ratio | No | M4. The verifier is a stub. |
| Land latency p50/p95 | Data exists, metric does not | `QueueEntry.submitted_at` and `updated_at` are recorded, but the simulation submits every change before draining the queue, so the latency would measure queue depth. Nothing reports it. |
| Replay success | No | M5. |
| Round-trip drift | Yes (M0 harness) | Lander snapshots keep the Tier 0 tree shape (`snapshot.rs:1-6`), so export applies. There is a new `hord-git/tests/round_trip.rs`. |

## ADR challenges

These are not counted as bugs.

- **Spec §3.1 against §3.2.** §3.1 says `SnapshotId` is the root tree's id. §3.2 says a `Snapshot` object holds the "root Tree + repository-level metadata + index pointers". The implementation took §3.1, which left no content-addressed place for identity, and item 1 follows from that. An ADR should settle which one wins.
- **ADR 0016 amendment against spec §12 M3.** "Workspace creation < 5 ms" was narrowed to `begin`, and `Directory` got a 100 ms p99 target. The measurements support the split (48.7 ms mean; the clone costs most of it). The acceptance text in §12 should be edited, or the ADR should say it supersedes that line, so the gate is not re-argued at M4.
- **Spec §10.5.4 against §6.3.** An author's `ChangeRecord.signature` cannot survive the lander rebuilding the record. The spec should say signatures cover the submitted record, and that the landed record carries a lander attestation (item 3).

## Not a problem (checked and cleared)

- **Crash ordering in `finish`.** The identity pointer and the queue row are written with `Durability::None` before `set_head`. `set_head` flushes the buffered log entries in the same durable write (`store.rs:304-308`). Recovery re-queues entries marked landed that are missing from the log (`lander.rs:262-281`). This is consistent.
- **Cancelling `land_local`.** Each mutation is one `spawn_blocking` closure that runs to completion. If the future is dropped between `prepare` and `finish`, the entry stays `Queued`, and at worst an orphan record is left (item 12).
- **`begin` is O(1)** for `InMemory` workspaces. The simulation measures p99 of 0.29 µs over 1,000 live workspaces.
- **Merge mode (ADR 0014).** hord-txn uses only `MergeMode::Lander`, and there is no after-the-fact lost-edit check left.
- **`landed_since`** finds `L` by the first parent, then by the base snapshot. That is correct for a single-parent log. A two-parent arbitration change (M5) whose first parent is the landed side also works.
- **Global state in hord-txn.** All caches are per `Repo` (`Inner`). The `AtomicU64` in `materialize.rs:124` only names temporary directories. The one real global is in `hord-lang-rust` (item 7).
- **`Cargo.lock` placement** matches ADR 0013, and the adapter reports its own language id, `cargo-lock`.
- **The write set comes from content comparison** (ADR 0015 amendment), and `tests/duplicate_content.rs` covers the under-declaration regression.
