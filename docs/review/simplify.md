# M3 review: simplify (simplification and reuse)

Diff reviewed: `git diff 03e2310..b867df9`. Read-only. Scratch work is in `target/review-simplify/`.

Verification done for this report:

- `cargo test -q -p hord-diff -p hord-identity`: all green (the tests cited below as coverage).
- `target/review-simplify/scratch`: a program that runs `assign_in(path, t)` and `carry_in(path, ∅, t, [])` over all 139 `.rs` files under `crates/` (4,149 definitions). The two `IdentityMapping`s are equal (`nodes`, `deltas`, `moves`, `renames`) for every file.
- Caller counts come from `grep -rnw` over `crates/` and `bench/`, tests included.

Line counts are approximate deletions, before any new helper is added.

---

## 1. One file-root id under four names; `glue_node_id` survives even though ADR 0015 removes it

**Where:** `crates/hord-txn/src/ids.rs:20-27`, `crates/hord-txn/src/lib.rs:39-40`, `crates/hord-diff/src/defs.rs:8-19`, `crates/hord-diff/Cargo.toml:14`, `crates/hord-identity/src/root.rs:13`

**What is wrong:** The same function goes by four names:

| Name | Crate | What it is |
|---|---|---|
| `file_root_id(path)` | hord-identity | the implementation |
| `file_parent(path)` | hord-diff | `hord_identity::file_root_id(path)` |
| `path_node_id(path)` | hord-txn | `hord_diff::file_parent(path)` |
| `glue_node_id(path)` | hord-txn, `#[deprecated]` | `path_node_id(path)` |

- ADR 0015 (accepted), Consequences: "hord-txn's separate `glue_node_id` is removed." It is still `pub` and re-exported with `#[allow(deprecated)]`. Its only caller is its own unit test (`ids.rs:41`).
- hord-diff takes on a whole crate dependency (`hord-identity`) for this one 10-line function. It uses nothing else from hord-identity.
- Callers use a mix of names: `file_parent` has 28 references in 11 files (hord-txn `propose.rs`, `files.rs` and `rebase.rs` among them); `path_node_id` has 26 in 8 files. A reader has to follow two hops to learn they are the same id.

**Fix:**

1. Delete `glue_node_id` and its re-export. This alone is required by ADR 0015.
2. Move `file_root_id` into hord-core, next to `NodeId` and `RepoPath` (e.g. `NodeId::file_root(&RepoPath)`). It only needs `ObjectId::of`, which hord-core already has.
3. Delete the `file_parent` and `path_node_id` wrappers, or keep one of them as a plain `pub use`.
4. Drop `hord-identity` from `hord-diff/Cargo.toml`.

**Deletes:** `ids.rs` (44 lines, whole file), `defs.rs:8-19` (12), `lib.rs` re-exports (4), one Cargo dependency. The call-site renames are mechanical.

**Behaviour kept:** The id derivation is unchanged. Covered by `hord-identity` `root::tests::root_ids_are_stable_distinct_and_not_nil`, `hord-txn` `ids::tests::path_ids_are_the_file_roots`, and `hord-diff/tests/apply_identity.rs`.

**Effort:** S

---

## 2. hord-txn repairs `hord_diff::apply`'s output ids from outside (`union_ids`)

**Where:** `crates/hord-txn/src/rebase.rs:245-370` (`union_ids`, its nested `check` walk, `copy_subtree`). It is called from `reapply` at `rebase.rs:224`.

**What is wrong:** `hord_diff::apply` has an internal id policy:

- `apply_replace` drops every id under the replaced node (`apply.rs:262-264`).
- `apply_insert` gives inserted definitions `NodeId::generate()` (`apply.rs:288`).

`reapply` calls `apply`, then `union_ids` undoes that policy. It re-copies subtree ids from the change's result for each `Replace`. It re-pairs each `Insert`'s content with its sites in the change's result by scanning `ids` for matching content. It then walks the whole tree again (`check`) to prove that none of `apply`'s generated ids survived.

So hord-txn is coded against hord-diff internals it does not own. It knows which ids `apply` drops, which it invents, and that the invented ones are unknown. About 125 lines in hord-txn exist only because `apply` throws away information it had at the moment of each edit: the site it wrote and the op that wrote it.

**Fix:** Give hord-diff an entry point that takes the ids too: `apply_identified(path, base, ops, store: &IdentifiedTree)`.

- In `apply_replace`, copy `store`'s ids under the replaced node's site in `store`.
- In `apply_insert`, copy ids at the inserted site `at`, which `apply_insert` already computes, from the store-side site of `node`.
- Never generate an id.

Then `reapply` uses `applied.ids` directly, and `union_ids`, `copy_subtree` and the `check` walk go away. The existing `apply` can stay as `apply_identified` with an id-less store, for merge and corpus callers.

**Deletes:** about 125 lines from hord-txn. Adds about 40 to hord-diff, where the sites are already in hand.

**Behaviour kept:** hord-txn `txn.rs::rebase_of_disjoint_changes_on_the_same_file_composes` and `rebased_records_carry_structural_ops_and_land_validated`; the M3 simulation (`hord-eval-m3`). The fallback when ids cannot be placed (`finish_parsed`, which re-carries) is unchanged.

**Effort:** M

---

## 3. The "definition sites with parents" walk exists four times; the "leaves outside nested definitions" walk twice

**Where:**

- Definition sites and their parent ids:
  - `hord-diff/src/defs.rs:84-137`: `DefSite`, `collect_sites`, `walk`, `sites_by_id`
  - `hord-identity/src/carry.rs:480-519`: `site_index`, `sites`, `walk_sites`, `Placement`
  - `hord-txn/src/semantic.rs:429-452`: `by_node`, and `enclosing`, which finds the parent by prefix search instead of a walk
  - `hord-lang/src/identify.rs:399-419`: `walk_defs`
- Leaves outside nested definitions:
  - `hord-diff/src/defs.rs:143-227`: `root_glue`, `local_glue`, `walk_outside_defs`
  - `hord-txn/src/propose.rs:306-347`: `root_glue`, `own_leaves`

**What is wrong:**

- `collect_sites` and `walk_sites` are the same preorder walk that tracks the parent. They differ only in how they spell "no parent": the nil sentinel (`NodeId::nil()`) in one, `None` in the other.
- A consequence: hord-diff's `mapping_between` move recovery (`defs.rs:275-296`) re-implements hord-identity's `recompute_moves` (`carry.rs:453-478`). Both skip moves where either parent is the file root.
- hord-txn's `own_leaves` is hord-diff's `local_glue` with two changes: it pushes the leaf `oid` instead of `normalized`, and it takes `&IdentifiedTree`. This is how hord-txn learns "which definitions changed". ADR 0015's amendment requires the write set to come from content, not from ops, so the logic has to exist, but it should not be a second copy.

**Fix:** In hord-lang, next to `IdentifiedTree` (which all three crates already depend on), add:

- `IdentifiedTree::def_sites() -> Vec<DefSite { node, oid, site, parent: Option<NodeId>, index }>`
- `leaves_outside_defs(tree, site, visit)`

Point `collect_sites`/`sites`/`enclosing` and `local_glue`/`own_leaves` at them. `mapping_between`'s move loop can then call a shared `moves_between(base, side)`.

**Deletes:** about 55 lines in hord-diff, 45 in hord-identity, 40 in hord-txn. Adds about 50 in hord-lang.

**Behaviour kept:** hord-diff `apply_identity.rs`, `merge.rs` and `proptest.rs` (`apply(diff) == result`); hord-identity `carry.rs::moved_carry_keeps_id_and_emits_move` and `derived_from_across_parents_emits_move`; hord-txn `duplicate_content.rs` and `txn.rs::comment_edits_conflict_only_with_edits_of_the_same_text` (write-set content comparison).

**Effort:** M

---

## 4. The six-way `match` over `Op`'s `NodeId` fields is hand-written six times

**Where:**

- `hord-diff/src/defs.rs:28-40` (`mentions`)
- `hord-diff/src/defs.rs:43-90` (`swap_root`, 48 lines)
- `hord-diff/src/merge.rs:499-507` (`primary_node`)
- `hord-txn/src/files.rs:123-129` (`owner_of`)
- `hord-txn/src/files.rs:220-235` (`coarse_paths`)
- `hord-txn/src/rebase.rs:183-196` (`names_landed`)

**What is wrong:** Every site spells out `Insert{parent}`, `Delete{node}`, `Replace{node}`, `Rename{node}`, `Move{node, from_parent, to_parent}`, and `Blob`/`Tree` → none. When a variant is added to the spec §3.5 `Op`, all six have to change together.

**Fix:** Add `Op::node_ids(&self) -> impl Iterator<Item = NodeId>` and `Op::map_node_ids(self, f)` to hord-core. This adds methods and does not change the §3.5 shape. Then:

- `mentions(op, id)` becomes `op.node_ids().any(|n| n == id)`.
- `swap_root` becomes a single `map_node_ids`.
- `coarse_paths` becomes `flat_map(Op::node_ids)`. This also includes `Move.node`, which is a definition id; it is compared only against `file_parent(path)`, so the result is the same.

`primary_node` and `owner_of` pick one role-specific field, so they stay as they are.

**Deletes:** about 60 lines. Adds about 30 in hord-core.

**Behaviour kept:** hord-diff `apply_identity.rs` and `merge.rs`; hord-txn `txn.rs::blob_tier_files_conflict_by_path_and_line_merge` and `new_and_deleted_files` (coarse paths).

**Effort:** S

---

## 5. `assign`/`carry` without a path are test-only, and `assign_in` equals `carry_in` on an empty base

**Where:** `crates/hord-identity/src/assign.rs:10-47`, `crates/hord-identity/src/carry.rs:85-114`, `crates/hord-identity/src/lib.rs:25-26`

**What is wrong:**

- **No production callers.** The path-free `assign` and `carry` (salt `0`) are not called anywhere outside `hord-identity/tests/carry.rs`. Production code uses only `assign_in` (`semantic.rs:225`) and `carry_in` (4 call sites).
- **They cause the bug M3 fixed.** The path-free forms give identical files in two paths the same ids. That is the collision `assign_in` exists to prevent (`tests/carry.rs:767`) and the one behind the `duplicate_content.rs` regression. Keeping them public invites it back.
- **`assign_in` is redundant.** `assign_in(a, p, t)` is `carry_in(a, p, &IdentifiedTree::default(), t, &[])`. Verified: equal `IdentityMapping` on all 139 `crates/**/*.rs` files, 4,149 definitions. `tests/carry.rs:785` already asserts the `nodes` half.

**Fix:**

1. Delete `assign` and path-free `carry`.
2. Make `assign_in` a one-line call to `carry_in` with an empty base, or delete it and call `carry_in` at `semantic.rs:225`.
3. Rename `carry_in` to `carry` once the path-free form is gone.
4. Move the roughly 25 test call sites to a fixed test path. `tests/carry.rs` already has `fn path()` at line 122.

**Deletes:** about 45 lines in src, including the `scope == 0` comment in `path_salt`. The test edits are mechanical.

**Behaviour kept:** Every `hord-identity` test, run through the path form. `assign_in_keeps_identical_files_apart` still covers per-path separation.

**Effort:** S

---

## 6. `hord_diff::merge_ops` is public API with no caller, and its test duplicates another

**Where:** `crates/hord-diff/src/merge.rs:357-395`, `crates/hord-diff/src/lib.rs:20,49`, `crates/hord-diff/tests/merge.rs:230-251`

**What is wrong:**

- **No caller.** hord-txn, hord-cli and the benches call only `merge`, `merge_blob`, `diff` and `apply`. The one caller of `merge_ops` is the test `merge_ops_delete_vs_replace`.
- **The test is a duplicate.** `delete_vs_replace_is_hard_conflict` (`tests/merge.rs:147`) checks the same rule-3 outcome (Hard) through `merge`, and it also asserts that the conflict names nodes.
- **It adds a third nil guard.** The wrapper exists only to swap the root and guard against nil at a public boundary nobody uses (see finding 7).

**Fix:** Delete `merge_ops`, its re-export, its doc mention in `lib.rs`, and the test. `merge_ops_internal` stays, because `merge` uses it. Also update the ADR 0014 text, which mentions "`hord_diff::merge` (and `merge_ops`)". That is an informational change; the decision is unaffected.

**Deletes:** about 40 src lines and 22 test lines.

**Behaviour kept:** `delete_vs_replace_is_hard_conflict`, `disjoint_ops_compose` and `apply_of_diff_used_by_merge_round_trip`.

**Effort:** S

---

## 7. The nil sentinel is kept only because `merge()` has no path

**Where:**

- `hord-diff/src/defs.rs:21-25` (`root_sentinel`)
- `swap_root` at `diff.rs:30`, `apply.rs:42` and `merge.rs:385-392`
- the three "an op names the nil NodeId" guards at `apply.rs:33-41` and `merge.rs:373-383`
- `merge.rs:134-137`
- 20 `root_sentinel()` comparisons
- `hord-txn/src/propose.rs:200,288`: `write_set.remove(&NodeId::nil())`

**What is wrong:**

- **Why the sentinel lives on.** Since ADR 0015 every public op names `file_parent(path)`. Inside hord-diff, though, the root is still `NodeId::nil()`, swapped in and out at each boundary. The only public entry point without a path is `merge` (`merge.rs:111`). Its two production callers both have one: `rebase.rs:485` has `path` in scope, and `bench/m1-eval/src/main.rs:690` already builds a `RepoPath` for `apply` (line 1124).
- **Dead nil removals in hord-txn.** The two `write_set` nil removals cannot fire:
  - Write-set ids come from carried or stabilized ids. `stabilize_births` never returns nil (`assign.rs:92`).
  - Base ids come from the same source.
  - `path_node_id` is never nil (`root.rs:20`).
  - `carry_in` is always called with `&[]` declarations.

  The `read_set` removal (`propose.rs:209`) is different. It guards user-supplied `node:` reads, so keep it.

**Fix:**

1. Add `path: &RepoPath` to `merge`.
2. Pass `root = file_parent(path)` into `DiffCtx`, `collect_sites` and `apply_internal`, and use it where `root_sentinel()` is used today. `site_of`'s special case becomes `id == root`.
3. Delete `root_sentinel`, `swap_root`, `mentions` (if finding 4 does not keep it), the three nil guards and the remap closure. A nil op then fails like any unknown id (`Error::MissingId`).
4. Delete the two `write_set` nil removals.

**Deletes:** about 95 lines. Adds about 15 for the threaded `root`.

**Behaviour change to note:** `merge` conflicts would name the file root id instead of dropping it (`merge.rs:136`). hord-txn's `MergeConflict.nodes` would then show `(file)` in `hord conflicts`. That is arguably clearer, but it is a visible change.

**Behaviour kept:** `hord-diff/tests/proptest.rs` (`apply(diff) == result`), `apply_identity.rs` and `merge.rs`; M1 eval scores (run `hord-eval-m1`).

**Effort:** M

---

## 8. Path-less `ResolveCtx::add_file` / `for_each_file` are dead, and `FileRoot.path` is needlessly optional

**Where:** `crates/hord-lang/src/adapter.rs:68-74, 210-221, 281-286`, `crates/hord-lang-rust/src/resolve.rs:1353-1355`

**What is wrong:** M3 added `add_file_at` and `for_each_file_at` and marked the old forms "prefer `_at`". Neither `add_file` nor `for_each_file` has a caller anywhere in `crates/` or `bench/`, tests included. Because they still exist, `FileRoot.path` is `Option<RepoPath>`, and the resolver carries an `if let Some(path)` branch for a `None` that can no longer happen.

**Fix:** Delete `add_file` and `for_each_file`. Make `FileRoot.path: RepoPath`, rename the `_at` forms to the plain names, and drop the `Option` from the `for_each_file` callback.

**Deletes:** about 22 lines.

**Behaviour kept:** `hord-lang-rust/tests/tier2.rs` and the M2 eval (the resolver is the only consumer).

**Effort:** S

---

## 9. Duplicate tests

**`txn.rs::cargo_lock_goes_through_the_lockfile_merge_call_site`** (`crates/hord-txn/tests/txn.rs:261-288`)

- **What is wrong:** `concurrent_cargo_lock_dependency_additions_both_land` (`txn.rs:554`) covers the same path and more: two concurrent `Cargo.lock` package additions, `land_local`, both land, both packages present. On a real v4 lockfile, it also asserts structural ops rather than a Blob, the write-write overlap, no hard conflict, and the merged dependency list. The smaller test adds no assertion the bigger one lacks.
- **Fix:** delete it (28 lines).

**`duplicate_content.rs` re-implements the `tests/common` harness** (`crates/hord-txn/tests/duplicate_content.rs:34-62`)

- **What is wrong:** It has its own `actor()`, `intent()`, temp dir, `Repo::create` and `bootstrap`, and it looks up a definition by name, which `common::def` already does. Its temp dir is keyed only by pid and is removed partway through the test, so a panic leaks it.
- **Fix:** `mod common;` plus `common::repo(&[("src/lib.rs", SRC)])` and `common::def`. That deletes about 30 lines and gets `TempRepo`'s drop cleanup.

**Behaviour kept:** the remaining assertions are unchanged.

**Effort:** S

---

## 10. Small copies across crates

**Where:** `hord-lang-toml/src/merge.rs:382-395` (`pick3`) vs `hord-diff/src/cst_merge.rs:59-72` (`prefer_unchanged`); `hord-lang-toml/src/merge.rs:304-310` vs `hord-lang-rust/src/cargo_lock.rs:149-155` (`join`)

**What is wrong:**

- `pick3` over `Option<&T>` is `prefer_unchanged` with `T = Option<&T>`: the same three-way rule, character for character.
- `join` is an identical copy.

**Fix:** Put `pick3` in hord-lang, which both crates depend on, and use it in both. Make toml's `join` `pub(crate)`-and-exported, or just inline `.join("; ")`.

**Deletes:** about 20 lines.

**Behaviour kept:** hord-lang-toml's merge unit tests (commutativity), `hord-diff/tests/merge.rs`, and `hord-lang-rust/tests/cargo_lock.rs`.

**Effort:** S

---

## 11. Snapshot tree diff re-implemented in hord-txn (low)

**Where:** `crates/hord-txn/src/snapshot.rs:127-190` vs `crates/hord-git/src/import.rs:418-461` (pre-M3)

**What is wrong:** Both walk two store trees, take the union of entry names, skip equal entries by id, and recurse into subtrees. One emits `Op`s; the other emits `FileDelta`s. hord-txn does not depend on hord-git, and the outputs differ, so this is the weakest finding here.

**Fix (optional):** Add `hord_store::changed_files(store, from, to) -> Vec<FileDelta>`. hord-txn uses it as is. hord-git import maps each delta to Blob/Tree ops, which is where it does its `CreateDir`/`Delete` bookkeeping.

**Deletes:** about 60 lines net.

**Effort:** M. Do this only when one of the two is touched next.

---

## ADR challenges

None. Finding 1 is code that falls short of an accepted ADR (0015), not a disagreement with one.

## Out of lane (for the quality and system reviewers)

These are noted, not verified further:

- `hord_diff::apply` still uses `NodeId::generate()` (a random ULID) for inserted definitions (`apply.rs:288`). In hord-txn, `union_ids`'s `check` keeps these ids out of stored identity. Any other `apply` caller that keeps `applied.ids` gets ids that differ from run to run, and AGENTS.md treats non-determinism as P0.
- `mapping_between` (`defs.rs:287`) drops every move into or out of the file root, so a definition moved from top level into a `mod` is not a `Move`. hord-identity's `recompute_moves` does the same.

## Not a problem

Checked and cleared:

- **`MergeSeverity` (hord-txn) vs `hord_diff::ConflictKind`:** the same two variants, but the hord-txn type is serde'd for `hord conflicts --json`, and the hord-diff one is not. A separate type at the serialization boundary is fine.
- **`walk_files` in `snapshot.rs` and `materialize.rs`:** one walks store trees, the other the filesystem. Different sources, so this is not duplication.
- **PENDING / TODO / FIXME markers:** none in `crates/`, `bench/`, `docs/`, or the skills (only in vendored `corpora/`).
- **`hord_lang_toml::merge::merge_toml`:** its only callers are its own unit tests, but it is the generic TOML entry point ADR 0013 describes, and `merge_cargo_lock` needs the lower-level `merge_docs`. Keep it.
- **`read_set.remove(&NodeId::nil())` (`propose.rs:209`):** reachable through a user `node:` read declaration. Keep it.
- **Intent parsing (`hord-cli/src/intent.rs`):** uses `serde_yaml_ng`, with only the `---` fences split by hand. That is not a hand-rolled parser.
- **`bench/m3-eval/src/lock.rs` vs `hord-lang-rust/tests/cargo_lock_corpus.rs`:** the first is an end-to-end lander scenario, the second a per-commit re-emit check. They are not duplicates.
- **`status_name` in both `bench/m3-eval/src/sim.rs` and `hord-cli/src/txn.rs`:** four lines each, in a standalone bench. Not worth a shared API.
- **`hord-cli/src/txn.rs` view and formatting helpers:** CLI presentation only. They do not re-implement hord-txn logic.
