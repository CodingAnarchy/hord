# M3 review: quality (correctness and robustness)

Diff: `git diff 03e2310..b867df9`. Every finding below has a failing repro in
`target/review-quality/repro/tests/` (a standalone crate with path deps on the
hord crates; run with
`CARGO_TARGET_DIR=target/review-quality/target cargo test --manifest-path target/review-quality/repro/Cargo.toml --test <name> -- --nocapture`),
or, where noted, a code trace. No source was edited.

## Findings (most severe first)

### 1. P0: A landed code change is silently reverted. `,` and `;` leaves are not writes, and the rebase fast path trusts the write sets

- **Where:** `crates/hord-txn/src/propose.rs:331` (`own_leaves` skips every `,`/`;` leaf), `crates/hord-txn/src/rebase.rs:184-210` (`reapply`), `crates/hord-diff/src/apply.rs:78` (`Replace` ignores `from`).
- **What is wrong:** `own_leaves` is meant to skip the separators *between child definitions*, so that adding a field is not a write of the struct. It skips every `,` and `;` leaf at any depth instead, along with the trivia attached to them. So a change whose only token difference is a separator, or a same-line comment after one, has an **empty write set**:
  - `vec![0; 3]` → `vec![0, 3]` (a different value),
  - `let x = 2; // two` → `// two, see issue 7`,
  - `mod other;` → `mod other;  `.

  The set check then finds nothing. When a concurrent change edits the same function, `reapply` sees no landed write on that node. It applies the concurrent change's `Replace` on head. `apply` never compares `Replace.from` with head's content, so the whole function is replaced with the concurrent change's version, and the first change's edit is gone. Both changes report `Landed` with clean reports.

  This is the exact failure ADR 0014 forbids ("never lands one side"). It is also a false negative under the M3 oracle (both changes wrote `sizes`).
- **Repro:** `tests/lost_code.rs`. A: `vec![0; 3]` → `vec![0, 3]`. B: `v` → `v.clone()` in the same function. Output:
  ```
  A write_set = {} (sizes = 2Y90WW14E430W9PHFYHSQ7WF4S)
  Landed … ([], [])
  Landed … ([], [])
  ---- head ----
  pub fn sizes() -> Vec<u32> {
      let v = vec![0; 3];      <- A's landed edit is gone
      v.clone()
  }
  ```
  `tests/lost_edit.rs` shows the same with a trailing comment after `;`. `tests/repro.rs::t5_whitespace_edits` shows `mod other;` → `mod other;  ` giving `write_set={}` with a `Replace` of the file root.
- **Fix:** do both parts.
  1. In `own_leaves`, skip a `,`/`;` only when it is a direct child of the node being walked and its previous or next sibling is a nested definition site. Keep its trivia either way (or compare the trivia-stripped token plus the trivia). All other separators count as content.
  2. Defense in depth in `reapply`: for every `Replace`/`Delete`/`Rename`, require `ours.tree.oid_at(site_of(node)) == from`, the content the op expects. For `Move`/`Insert` parents, require the parent to be present. On a mismatch, return `None` so the 3-way merge runs. This makes the fast path safe even when a write set under-reports.

  Add both repros as regression tests in `crates/hord-txn/tests/txn.rs`.
- **Effort:** S (fix 2), M (fix 1 plus tests; re-run m3-eval for the FP rate).

### 2. File creation never writes the path, so reads and declared reads of a file another change creates are missed

- **Where:** `crates/hord-txn/src/propose.rs:151` and `:282` (a new parsed file writes only its births, plus the root when its glue is non-empty), and `crates/hord-txn/src/workspace.rs:193-195` and `:208-214` (`read_file`/`read_range` return before logging the path when the file is missing).
- **What is wrong:**
  - Creating a parsed file does not put `path_node_id(path)` in the write set. `tests/repro.rs::t1` prints `A contains path id: false`.
  - A read of a missing path is not logged at all.
  - As a result, an agent that checks `src/new.rs` does not exist, or declares `reads: - path: src/new.rs` (ADR 0012), gets **no** conflict when another change creates that file.

  Two changes creating the same parsed path with different content also show no set conflict. The rebase catches that case (a hard blob-merge conflict), but the `ConflictReport` handed to replay lists no overlap.
- **Repro:** `tests/phantom.rs`. With `declare=true` it prints `conflicts = []` and fails the assertion. `tests/repro.rs::t1_add_add_same_rust_path` prints `Conflicted Some(([], [MergeConflict … blob 3-way line merge conflict]))`: an empty set-conflict list.
- **Fix:**
  - In `propose`, when `base_blob.is_none()`, insert `path_node_id(path)` into `write_set` (creation writes the root, consistent with ADR 0015's "glue edits write the root").
  - In `read_file`/`read_range`, insert into `read_paths` before the `None` early return.
- **Effort:** S.

### 3. Directory `propose` includes build output and editor droppings (`target/`, `.DS_Store`)

- **Where:** `crates/hord-txn/src/workspace.rs:571-600` (`directory_changes` uses `walk_files`, `crates/hord-txn/src/materialize.rs:316-334`, with no ignore rules).
- **What is wrong:** every regular file under the checkout that is not in the base becomes a created file in the proposal. ADR 0016 option 4 rejects sparse checkouts because agents run `cargo build`/`cargo test` inside the workspace. That puts `target/` (thousands of binaries) into the next `propose` as `Op::Blob` creations, and they land.
- **Repro:** `tests/repro.rs::t3_directory_build_output_is_proposed`: `blob ops: [".DS_Store", "target/.rustc_info.json", "target/debug/fixture"]`.
- **Fix:** honor ignore rules when walking a checkout: the base snapshot's `.gitignore` files plus a built-in `.hord/`-style default. The `ignore` crate's `gitignore` matcher needs a one-line ADR log entry. At minimum, skip untracked paths that match the base's `.gitignore`, and report skipped untracked files in `hord status`.
- **Effort:** M.

### 4. A tracked file replaced by a symlink is proposed as a deletion

- **Where:** `crates/hord-txn/src/materialize.rs:325-331`. `walk_files` uses `symlink_metadata` and skips anything that is not `is_file()`/`is_dir()`. Base files that are not seen are then deleted (`workspace.rs:597-599`).
- **What is wrong:** `mv README.md docs-README.md && ln -s docs-README.md README.md` (or a tool that writes through a symlink swap) proposes `Blob{README.md, to: None}` plus `Tree Delete`. The file is still readable at that path. Symlinked directories drop their whole subtree the same way. Nothing warns.
- **Repro:** `tests/repro.rs::t4_directory_symlink_is_a_deletion` prints the `Delete` ops.
- **Fix:** when a path that is a base file is now a symlink, fail `propose` with a typed error (`Error::UnsupportedFileType(path)`) rather than deleting it, because the tree has no symlink mode. Or follow the link for files inside the checkout.
- **Effort:** S.

### 5. A failed `finish` leaves the in-process head cache stale, and the next landing reverts the previous one

- **Where:** `crates/hord-txn/src/lander.rs:380-407`.
- **What is wrong:** `finish` calls `set_head` (durable), then `index_change(landed_id)?`, then `footprint_of(...)?`, and only then `set_head_cache`. If `index_change` or `footprint_of` fails (a redb or pack I/O error), `land_local` returns `Err`, but `Inner::head` (`repo.rs`, cached in `self.head`) still returns the *previous* head. A long-lived `Repo` (the M4 server lander) that calls `land_local` again rebases the next change onto the old snapshot (`prepare`, lander.rs:326-328). It writes a record whose `result` omits the change that just landed, then `set_head`s it, so that change's content drops out of head.
- **Evidence:** code trace only (needs injected I/O failure). Lines: `set_head` at 398, `index_change` `?` at 399, `set_head_cache` at 402.
- **Fix:** call `set_head_cache` immediately after `self.store.set_head(landed_id)?`, before any fallible post-landing work. Treat `index_change` failure as a warning, since the index is rebuildable with `rebuild_index`.
- **Effort:** S.

### 6. Any `Err` from `prepare` wedges the queue on that entry

- **Where:** `crates/hord-txn/src/lander.rs:184-185` and `:303-331`, `crates/hord-txn/src/rebase.rs` (`finish_parsed`: `check_reproduces(...)?`, `carry_in(...)?`).
- **What is wrong:** `run` sets `state.cursor = Some(entry.seq)` and then `prepare(entry)?`. Missing records and invalid ops are parked as `Rejected`. Anything else that fails deterministically for that record, such as a `check_reproduces` mismatch on a 3-way-merged file (`OpsDoNotReproduce`) or an identity error, propagates out of `land_local` and leaves the entry `Queued`. Every later `land_local` picks the same entry first and fails the same way. One poison change blocks every change behind it (head-of-line blocking), with no operator-visible status.
- **Evidence:** code trace. `rebase` → `merge_file` → `finish_parsed` → `check_reproduces(...)?` (rebase.rs `finish_parsed`) → `results … result?` → `prepare` `?` (lander.rs:328) → `run` `?` (lander.rs:185).
- **Fix:** in `prepare`, map errors other than I/O and store errors from `rebase`/`validate_except` to `park(entry, Rejected { reason }, Some(report))`, as `validate` already is. Keep propagating only transient store I/O. Conversely, `validate` currently parks *every* error as `Rejected` (lander.rs:317-325), including transient I/O, which permanently rejects a valid change. Classify errors in one helper used by both paths.
- **Effort:** S.

### 7. Resubmitting a landed change lands an empty duplicate

- **Where:** `crates/hord-txn/src/lander.rs:207-215` (`submit` dedups only against `Queued` entries).
- **What is wrong:** `hord submit <landed id>` or `hord land --local <landed id>` queues it again. The lander rebases it onto a head that already contains it. Every file hits `ours == file.to`, so the result equals head, and a new record (`base == result`, 0 ops, same intent) is appended to the log. `hord log` shows the intent twice.
- **Repro:** `tests/resubmit.rs`: `log 3 -> 4; last: base==result true ops 0`.
- **Fix:** in `submit`, return the existing entry when one `names(change)` with status `Landed`. In `prepare`, also treat an empty rebase (`rebased.result == head.snapshot`) as "already applied", with no log append.
- **Effort:** S.

### 8. Copy-mode stat index is racy on filesystems with coarse mtimes (Linux)

- **Where:** `crates/hord-txn/src/materialize.rs:180` (`index_of(dest)` right after the copy) and `workspace.rs:586-590`.
- **What is wrong:** git's "racily clean" problem. A file written within the same mtime tick as the index, with the same size, is treated as unchanged. Linux inode timestamps come from the coarse clock (a jiffy, about 1–10 ms) on kernels without multigrain timestamps. An agent that edits a file within milliseconds of `hord ws new --materialize=copy` can therefore have the edit missed at `propose`. This is different from ADR 0016's accepted "tool preserves mtime" case: nothing here preserves anything.
- **Evidence:** code trace, not reproduced. APFS has ns mtimes, and macOS `fs::copy` clones and keeps the source mtime.
- **Fix:** record the index's own write time. As git does, treat any entry whose `mtime_ns >= index_time - granularity` as dirty (re-hash it once).
- **Effort:** S.

## Claims in PR #8 without a regression test

- **Lander crash recovery** ("an entry marked landed whose change is not in the log is queued again", lander.rs module doc): no test anywhere. `tests/recovery.rs` forges that state through the public `Store::queue_set` and shows `recover` works. That file can be moved into `crates/hord-txn/tests/txn.rs` as-is.
- **Fast-path safety** ("ops on definitions no landed change wrote re-apply on head"): no test covers a landed edit that the write set under-reports. Finding 1's repro is that test.
- **Deterministic landing across processes:** no test lands the same sequence twice and compares ids. `tests/determinism.rs` does (it passes, see below) and is cheap to adopt.
- **ADR 0012 declared `path` reads:** tested only for names (`declarations_join_the_read_set_and_unknown_names_fail`). The file-creation case (finding 2) is untested.

## ADR challenges

None. Findings 1 and 2 are implementation gaps under ADRs 0012, 0014, and 0015 as written.

## Not a problem (checked and cleared)

- **Queue durability ordering.** `queue_set` (non-durable) → buffered `append_log` → `set_head` (Immediate, persists log and head in one redb commit). A crash at any point leaves either an entry that re-processes or an entry marked landed but missing from the log, which `recover` re-queues (verified, `tests/recovery.rs`).
- **Determinism.** Two fresh repos landing the same three changes (glue edits, births, nested impl, a 3-way merge) produce identical head snapshots and NodeIds (`tests/determinism.rs`). The `thread::scope` results are folded through a `BTreeMap` in path order, and every `HashMap`/`HashSet` in hord-txn is used for lookups only, never iterated into output. `created_at` enters `ChangeId` by spec (§3.5). A rebased record keeps the submitted provenance.
- **Rename vs. edit.** A file moved away while another change edits a definition in it gives a write-write conflict on the dead definition, then a hard "deleted on one side" merge conflict. It does not land (`tests/repro.rs::t7`).
- **Blob-tier add/add** gives a write-write conflict on the path id plus a hard line-merge conflict (`t2`).
- **`Cargo.lock` merge on hostile input.** Empty base, non-UTF-8, malformed packages, and unresolvable dependency spellings return `Conflict`/`Unsupported`, never a panic (`tests/lockfuzz.rs`). `Unsupported` falls back to the blob line merge.
- **Panics reachable from input.** No `unwrap`/`expect`/`panic!` outside tests in hord-txn, the new CLI modules, `cargo_lock.rs`, or `hord-lang-toml/src/merge.rs`, apart from one guarded `expect("checked")` (merge.rs:549). `read_range` clamps. The hex slicing in `print_entry` is on fixed 64-char ids.
- **Intent parsing.** A BOM, CRLF, a `...` closing fence, numeric or bool scalars (`summary: 2024` → `"2024"`), unknown keys (rejected), and an empty summary (`summary: #12` → rejected) all behave. `summary: hord-txn: fix x` is a YAML error, reported with context, not swallowed. That is YAML, not a bug.
- **CLI JSON.** Errors under `--json` go to stderr as `{"error": …}`. NodeIds print as ULIDs everywhere, and `reads: - node:` accepts the same form. `status`/`propose` report `reads: unobserved` for directory workspaces.
- **Stat index for clones.** The pristine index is reused, and the clone keeps mtimes. New files, deletes, and in-place edits are all detected (the repo's `directory.rs` plus `t3`/`t4` above exercise the walk).
