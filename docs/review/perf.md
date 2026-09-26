# M3 review: performance and scaling (perf)

Diff: `03e2310..b867df9`. Machine: Apple Silicon, 10 cores, APFS, release builds. Three other reviewers were building and running on the same machine, so absolute numbers carry about ±15 % noise. Every ratio below was measured within a single run.

Harness: `target/review-perf/bench` is a standalone crate with path deps on the hord crates. It uses the cargo corpus at HEAD `8814ead1` (2,950 files, 1,372 `.rs`, 10.5 MB of Rust). It has these modes:
`longrun N` (begin at head → edit 1–3 fns → propose → submit → land, repeated), `queue N` (N changes on one base), `remote N` (propose in one `Repo`, land from a freshly opened one), `alt N` (proposes that alternate between heads), `parse`, `mem`, `cbor`, `dir N`, `stat`. Profiles come from `/usr/bin/sample` and are aggregated with `target/review-perf/{tree,self,inc}.py`. Raw output is in `target/review-perf/*.txt|csv`.

## Baseline (bench/m3-eval, release)

| | measured |
|---|---|
| sim throughput | 26.8 changes/s (100 in 3.73 s), second run 23.6/s, third 26.1/s |
| per landing | ~37 ms: `prepare` ≈ 25 ms (rebase ≈ 22.8 ms wall, `validate_except` 2.4 ms), `finish` ≈ 8.4 ms (redb commits) |
| rebase worker CPU | 82 % of it is `hord_lang_rust::cst::parse` (the re-parse of the merged/applied file) |
| peak RSS | 2.47 GB (0.69 GB after bootstrap+listing → 1.6 GB after 100 concurrent proposes → 2.43 GB after the 1,000-workspace phase) |

Because every agent in the sim begins at the same base, the gate measures the best case for the caches (see M4 section).

---

## Findings (most severe first)

### 1. The reference context is rebuilt from scratch for every new head, so the first propose after each landing costs 2.8 s. In a long-running lander that is every propose.

- **Where:** `crates/hord-txn/src/propose.rs:379` (`inner.rust_ctx(base)`), `crates/hord-txn/src/semantic.rs:262-295` (`build_rust_ctx` lists and views every file), `crates/hord-lang-rust/src/resolve.rs:93-106` (`resolve_context_with`), `resolve.rs:194` (`assign_modules` → `parse_mods` at `:404`, a tree-sitter parse per file), `resolve.rs:585` (`collect_ts`, a second tree-sitter parse per file).
- **What is wrong:** `RustCtx` is keyed by `SnapshotId` and built over the whole snapshot. Nothing carries over from the previous head's context, even though a landing changes 1–5 files. Every Rust file is parsed by tree-sitter twice more (independently of the cached hord parse) and walked, all on one thread.
- **Measured:** `longrun 150` begins each workspace at head and lands between proposes. Propose took min 2,803 ms, p50 2,850 ms, max 4,539 ms (first, cold) on **150/150** cycles. The lander itself stays at 9.8 ms/landing. The 8-entry cache hit rate for these first proposes is **0 %**, and it can only rise when two or more agents share a base (the sim, where all 100 share one base, hits 99/100). `queue 1000` then one late propose on the new head: 3,068 ms.
- **Where the time goes** (sample of 6 builds, ≈2.8 s each): `resolve_context_with` 85 %: `walk_ts` 32 %, `assign_modules` 26 % (almost all tree-sitter), a second tree-sitter parse 23 %, `walk_nt` 9 %, `install_links` 9 % (64 % of that is `Display::fmt` string building). `Index::build` is 14 % (see #2). The hord parse of files is <1 % once cached.
- **Fix:** (a) Cache per-file facts: `parse_mods` result keyed by blob, and `collect_ts`/`index_file` output keyed by (blob, module path). A new head's context then re-walks only the changed files. (b) Better: derive `ctx(head')` from `ctx(head)` plus `changed_paths(head, head')`, falling back to a full build only when a `mod` item, `#[path]`, or `Cargo.toml` changed. (c) Replace the `format!`/`Display` keys in `install_links` with interned or borrowed keys.
- **Expected gain:** (a) removes ~80 % of the build (per-file tree-sitter and walks): 2.8 s → ~0.3–0.5 s. With #2 it drops to ~0.1–0.2 s. (b) brings it to tens of ms. This is the biggest scaling limit found. See the M4 section.
- **Effort:** M for (a), L for (b).

### 2. The Rust name `Index` sits behind a process-global single-slot cache, so proposes on two heads thrash it (0.42 s per propose instead of 13 ms), and `Index::build` is quadratic in imports.

- **Where:** `crates/hord-lang-rust/src/resolve.rs:150-167` (`static CACHE: Mutex<Option<(u64, Arc<Index>)>>`, rebuilt under the lock), `resolve.rs:1478-1487` and the similar scans at `:1510`, `:1532`, `:1569` (`ident_status` linearly scans all `self.imps` for every (module, name) query during the import fixpoint).
- **What is wrong:** there is one `Index` per process, not one per `ResolveCtx`. Any reference query on a different context evicts it. The build runs while the global mutex is held, so concurrent proposes on different heads serialize on it and evict each other.
- **Measured (`alt 12`):** with both contexts already built and cached, proposes that alternate heads A/B/A took **422, 420, 454 ms**. Repeating head A took **13.4, 13.3 ms**. `Index::build` is ~0.38 s per build on cargo, and 96 % of it is `ident_status`.
- **Fix:** store the index in the context (`OnceLock<Arc<Index>>` inside `ResolveCtx` or `RustCtx`) and drop the global slot. Index `imps` by `(module, local)`, and globs by module, in `HashMap`s built once.
- **Expected gain:** alternating or concurrent proposes 420 ms → ~13 ms. `Index::build` 380 ms → low tens of ms. The global lock stops serializing all proposers.
- **Effort:** S.

### 3. Parse trees hold 42× their source bytes and are cached by count (4,096), not by size. RSS grows ~2.2 MB per landing to 3.8 GB, then clear-all causes a cliff.

- **Where:** `crates/hord-lang/src/tree.rs:214-256` (`intern_branch` concatenates the children's `raw` **and** `stripped` into new buffers at every internal node, so bytes are stored once per depth level), `crates/hord-txn/src/semantic.rs:227`, `rebase.rs:231,400,569`, `workspace.rs:557`, `propose.rs:147` (`IdentifiedTree::new((*tree).clone(), …)` deep-copies the `NodeTree` map for every identified view), `crates/hord-txn/src/repo.rs:22,246-266` (the `parsed` and `identified` caches are capped at 4,096 entries each and `clear()` everything on overflow), `snapshot.rs:44,264` (same pattern for trees).
- **Measured:**
  - `mem`: parsing all 1,372 cargo `.rs` files (10.5 MB) holds **420 MB (42× source)**. One `NodeTree` clone adds **199 MB (19×)**. `parse`: stored `raw` alone is 9.8–11× file size (for example 55 KB → 554 KB).
  - `queue 1000`: RSS 1.58 GB after proposes → **3.81 GB** after landing 994 changes (~2.2 MB per landing: one new merged-file tree plus one identified clone each).
  - m3-eval's 1,000 edited workspaces add ~0.8 GB (1.6 → 2.43 GB), about 0.8 MB each. Each edit parses a new blob into the shared cache.
  - `longrun 150`: RSS saw-tooths 480–850 MB as `clear()` fires. After a clear, a context rebuild takes 4.4 s instead of 3.0 s (`alt`: `evicted-first` 4,416 ms).
- **Fix:** (a) An internal node's `raw` is an optional cache (ADR 0008; it is not hashed). Make `raw`/`stripped` of internal nodes a range into one per-file `Arc<[u8]>` source, or compute them lazily in `project`. (b) Have `IdentifiedTree` hold `Arc<NodeTree>` instead of cloning. (c) Replace count caps with clear-all by a byte-weighted LRU (`raw` length is a good weight).
- **Expected gain:** (a)+(b) cut resident parse memory ~10× (420 MB → ~40–60 MB for all of cargo), bringing the 994-landing run to <1 GB and 1,000 edited workspaces to well under 100 MB of trees. (c) removes the 1.4 s rebuild cliff and the saw-tooth. (a) also removes the per-level `memmove`/`malloc` in parse (about 10 % of parse self time).
- **Effort:** M (a), S (b), S (c).

### 4. The per-node `ObjectId` uses cbor2's canonical `Value` path. That is 42 % of hord parse time, and the parse is 80 % of rebase time.

- **Where:** `crates/hord-lang/src/tree.rs:151` (`ObjectId::of(&node)` for every node occurrence, before dedup) → `crates/hord-encoding/src/lib.rs:35` (`cbor2::to_canonical_vec`), which buffers a `cbor2::Value`, allocates key strings, and sorts. Also `normalized_of_children` (`tree.rs:141`).
- **Claim checked:** "13.8 ms per 55 KB vs 1.7 ms tree-sitter, BLAKE3 + CBOR per node." The ratio holds: `crates/cargo-test-support/src/lib.rs` (55,086 B) parses in 18.1 ms with hord and 2.25 ms with tree-sitter (8×) on this loaded machine. Inclusive attribution inside `cst::parse`: `ObjectId::of` **55 %** (cbor2 **42 %**, BLAKE3 **12 %**), tree-sitter 19 %, the rest interning, maps, and allocation. The cost is CBOR more than BLAKE3.
- **Prototype (`cbor`):** a `Serialize` wrapper that emits `Node`'s fields in RFC 8949 §4.2.1 bytewise key order (`raw, kind, lang, name, children, normalized`), encoded with `cbor2::to_writer` (no `Value`, no sort) into a reused buffer. It is **byte-identical to `hord_encoding::encode` on all 658,157 unique cargo nodes**. Speed: 146 ms vs 485 ms for `ObjectId::of`, **3.3× faster**. It stays within ADR 0001 (still cbor2, still canonical bytes). The guard is a proptest asserting equality with `to_canonical_vec`.
- **Fix:** add `hord_encoding::encode_ordered` (or a `CanonicalOrder` marker) for map-free types whose `Serialize` already emits canonical key order. Use it for `Node` and the normalized-children array, streaming into `blake3::Hasher`. Add the equality proptest.
- **Expected gain:** hord parse −35–40 % (18 → ~11 ms per 55 KB). That lands in every rebase re-parse (rebase is ~22 ms of the ~37 ms landing), in cold context builds, in `validate`, and in propose. Lander sim throughput is estimated at ~27 → ~32 changes/s. Fresh-process landing (#5) gains more.
- **Effort:** S–M.

### 5. When changes come from another process (M4 remote), `validate` re-parses base and result serially on the lander thread, and throughput drops below the 20/s gate.

- **Where:** `crates/hord-txn/src/lander.rs:316-317` (`validate` unless `proposed` contains the id; `proposed` is per-process memory, `repo.rs:150`), `crates/hord-txn/src/files.rs:57-85` (`file_changes` → `file_view` for base and result of each file, one after another).
- **Measured (`remote 100`):** same kind of workload as the sim, landed by a freshly opened `Repo`: **19.7/s and 18.8/s** (two runs), against 26.8/s in-process. In `prepare`, `validate_except` is 74 % of samples (≈24.5 ms/landing), and 93 % of that is `cst::parse` from `file_view`.
- **Fix:** validation depends only on the record, not on head. Run it off the critical path: at `submit`, or in a prefetch task a few entries ahead of the lander cursor. Record the result durably so a restart does not redo it. Also parse files in parallel, as `rebase` already does (`rebase.rs:62`). #4 shrinks what remains.
- **Expected gain:** removes ~24 ms of serial work per landing: ~19 → ~30+/s for remote submissions.
- **Effort:** M.

### 6. Each landing does two fsync'd redb commits and two more non-durable commits. `finish` is 22 % of a landing.

- **Where:** `crates/hord-txn/src/lander.rs:392-399`: `put_identity_index` → `Store::set_identity_index` (commit, `Durability::None`, `crates/hord-store/src/queue.rs:97`), `put_entry` → `queue_set` (commit, None, `queue.rs:55`), `set_head` (commit, **Immediate**, `store.rs:305`), `index_change` → `insert_history` (commit, default **Immediate**, `crates/hord-store/src/index.rs:370-380`). Plus `submit` → `queue_push` (Immediate, `queue.rs:34`).
- **Measured:** `finish` ≈ 8.4 ms/landing (841 samples over 100 landings): `set_head` commit 4.7 ms, `insert_history` commit 3.5 ms. `submit` p50 is 5.9 ms (`longrun`), nearly all of it the fsync.
- **Fix:** one write transaction per landing that writes the identity-index pointer, the queue entry, the log entry, `head`, and the `node_history` rows, committed once with `Immediate`. That needs a `Store` batch API such as `Store::land(LandBatch)`. Crash recovery gets simpler too, because the queue status and head move atomically. For M4, group-commit submits: batch `queue_push` for N ms, or across concurrent callers.
- **Expected gain:** −3.5–4 ms per landing (one fsync fewer): ~37 → ~33 ms, about +12 % throughput at today's numbers and a bigger share once #4 and #5 land. Submit could reach ~1,000+/s with group commit, up from ~170/s.
- **Effort:** M.

### 7. `submit` and `status` decode the whole queue on every call. `landed_since` clones the whole log on every landing.

- **Where:** `crates/hord-txn/src/lander.rs:207-214` (`submit` → `queue_entries()`, looking for an existing Queued entry), `:237-241` (`queue_status`), `:486-487` and `:516` (`self.store.log()` returns a `Vec` clone of the full landing log; `position_of_snapshot` walks the log in reverse, calling `footprint()` per entry). `repo.rs:148` (`footprints`) is unbounded too, with one entry per landed change forever.
- **Measured (`queue 1000`):** `status()` at 994 entries: **16.9 ms**. The late `submit`: 17.9 ms, against a ~4.5 ms early-queue mean. Growth is linear, so a 100k-entry queue would cost ~1.7 s per `status` or `submit`. At 1,000 entries the log clone and footprint map are still negligible, but both are O(landings) per landing.
- **Fix:** add a redb table `change → seq` (and `landed → seq`) written in the same transaction as `queue_push` and `queue_set`, so `submit` and `status` become point lookups. Keep an in-memory `ChangeId → log position` map, which `Store` already has as `first_pos`, and expose a `log_since(pos)` slice instead of `log()`. Bound `footprints` to changes after the oldest live base.
- **Expected gain:** `status` and `submit` go from O(queue) to O(log n): 17 ms → <1 ms at 1k entries, and flat afterwards. That matters for 100 polling clients.
- **Effort:** S.

### 8. A `Directory` propose is floored by an O(files) serial `lstat` walk (~45 ms on cargo), and each clone re-encodes the pristine stat index.

- **Where:** `crates/hord-txn/src/workspace.rs:571-604` (`directory_changes`: `list_files(base)` into a `BTreeMap`, a full `walk_files`, and a per-file map lookup), `crates/hord-txn/src/materialize.rs:175-182` (every clone decodes the pristine's `.stat` and re-encodes it through the canonical `Value` path).
- **Measured (`dir 200`):** `begin_directory` p50 47.1 ms, p99 55.9 ms, which confirms ADR 0016's 48.7/71.6 ms. The first one on a base takes 348 ms (pristine write). A `Directory` propose after one edit takes 69–148 ms (warm context). `stat`: the `lstat` walk of 2,950 files takes **45.5 ms**. Stat index (244 KB) decode is 2.5 ms and canonical encode is 1.5 ms. So propose is roughly 45 ms walk + ~15 ms in-memory propose + index work. ADR 0016's 39 ms figure was taken on an unloaded machine.
- **Fix:** walk directories in parallel (per top-level dir, with `std::thread::scope`), and compare `(dev, ino)`-free stats against the index without building `base_files` for unchanged paths. At clone time, clone the pristine `.stat` file with `reflink` instead of decode and re-encode. Longer term, FSEvents/inotify dirty tracking per workspace, or the M7 VFS.
- **Expected gain:** a parallel walk gives about 3× on APFS (45 → ~15 ms). Cloning the stat file saves ~4 ms per `begin_directory`.
- **Effort:** S.

---

## M4 (spec §12) needs that today's design makes expensive

1. **100 remote clients (a remote simulation that produces a flight-recorder log).** Clients that begin at a moving head each need a fresh context (#1): 100 × 2.8 CPU-s, while head moves every ~30–40 ms. Context builds can never catch up. The 8-slot cache is smaller than the number of distinct bases, so it thrashes, and each cold rebuild takes ~4.4 s. The global `Index` mutex (#2) serializes all of them. Remote-submitted changes also lose the `proposed` fast path and pay full `validate` (#5, below 20/s). `status` polling is O(queue) (#7). Fix #1 (incremental context) and #2 before the server work, or the remote sim will measure the context builder rather than the lander.
2. **Speculative parallel verification.** `run` (`lander.rs:176-205`) runs prepare → verify → finish strictly in sequence. `prepare` rebases onto `self.head()`, and `landed_since` reads the durable log. Nothing can rebase onto a *speculative* head (head plus k pending, not-yet-verified landings). The identity index for a result snapshot is written only in `finish`, so speculative results have no index for the next speculative rebase to read. Needed: rebase/set-check parameterized by an explicit `(head snapshot, landed-since footprints, identity index)` chain held in memory, plus invalidation when a speculative predecessor fails verification. Also, `rebase` spawns one OS thread per file per landing (`rebase.rs:62`). With k speculative rebases in flight that becomes unbounded thread creation, so move it to a bounded pool (rayon or tokio blocking with a semaphore).
3. **Evidence reuse and impact sets** will want the reference graph per snapshot. The per-head full context rebuild (#1) is the wrong substrate for that. Incremental context is a prerequisite.
4. **Durability throughput:** one fsync per submit plus two per landing (#6), all through redb's single writer, will contend with the server's object and query services on the same store.

## ADR challenges

- **None of the accepted M3 ADRs is wrong on performance.** One observation about the M3 acceptance *harness* (not an ADR): all 100 sim agents begin at the same bootstrap change (`bench/m3-eval/src/sim.rs:430`, `Base::Change(base)`). So the throughput gate runs with one context build, 99 % cache hits, and no rebase against a moving head for proposes. A realistic "begin at head, land, repeat" run (`longrun`) shows proposes at 2.8 s each (#1). Recommend adding a moving-head variant to the M3 or M4 harness before the M4 remote simulation, so #1 regressions are gated.

## Not a problem (checked and cleared)

- **`begin` (InMemory):** p50 0.1 µs, p99 0.2 µs, max 46 µs over 1,000 live workspaces; no degradation from the first 100 to the last 100 (m3-eval).
- **Lander at base == head:** 9.8 ms/landing, flat over 150 landings (`longrun`); no per-landing growth in land time up to 1,000 queued (`queue 1000`: 36.4/s for 1-edit changes).
- **Context memory:** ~8–13 MB RSS per cached context (`alt`), so the 8-entry cap is ~100 MB. The context's cost is time, not memory.
- **BLAKE3 portable backend:** `blake3::portable::compress_in_place` appears in profiles, but every input is single-chunk (<1 KiB), where NEON gives nothing. Not worth a feature flag.
- **`rebase`'s `ObjectId::of(record)` (`rebase.rs:47`) and tree update:** <1 ms per landing in total.
- **Log/ref batching in `Store`:** `append_log` is buffered and folded into `set_head`'s commit. `index_change`'s `flush()` is a no-op after `set_head` (nothing is pending).
- **Directory clone:** `clonefile` dominates creation, as ADR 0016 intends. `du` reports ~26 MB per clone, but that counts shared APFS blocks, so it is not real disk use.
- **Conflict set check (`conflict::check`)** at 1,000 landings since base: not visible in profiles.
