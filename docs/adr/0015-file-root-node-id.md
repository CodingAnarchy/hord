# ADR 0015: File roots have a path-derived NodeId

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.3, §3.5 (`Op`, "ops reproduce result"), §5.1, §6.3
- **Blocks:** M3 landing validation

## Problem

`hord_diff::file_parent()` is `NodeId::nil` in every file. A top-level `Insert { parent, index, node }` cannot say which file it belongs to, and `Replace` on the nil id means "the glue of some file". hord-txn works around this by putting an `Op::Blob { path, from, to }` before each changed file's structural ops, so that an op's meaning depends on the `Blob` before it. `Blob` carries the whole result file, so the §3.5 check that ops reproduce `result` passes on the `Blob` alone. The structural ops of parsed files are never checked.

## Options

1. **`Blob` op as a file header (status quo).** No type change. Ops are position-dependent, and validation of parsed files is vacuous.
2. **Add a `path` field to structural ops.** Explicit, but changes the `Op` shape in spec §3.5 and repeats the path in every op.
3. **Path-derived file-root `NodeId`.** Each file's CST root gets `NodeId = derive("hord/file", path)` from canonical CBOR of the path components (hord-txn's `path_node_id` today). Top-level `Insert`/`Move` use it as `parent`/`to_parent`. `Replace` on it replaces that file's glue. `IdentityMap` locates it at the file with an empty child path.

## Decision

Option 3. `file_parent(path)` returns the path-derived root id, and the nil id is no longer used as a parent. `Op::Blob` is emitted only for files without an adapter, for unparseable results, and for file create/delete content. Landing validation applies structural ops to parsed files and never uses a `Blob` as a stand-in for them.

## Consequences

- Ops are position-independent, and `apply(base, ops) == result` checks the structural ops of every parsed file (spec §3.5, §12 M1 property).
- The root id changes when a file is renamed. A rename is `Op::Tree Rename`, and its definitions `Move` from the old root to the new one. Conflict checks see both paths. Giving file roots carried identity across renames needs a new ADR.
- Glue edits write the root id. Two changes that both touch glue in one file (for example, `use` lines) conflict write-write and go to structural rebase. The M3 simulation reports how much this adds to the false-positive rate.
- hord-txn's separate `glue_node_id` is removed. The root id is both the glue id and the whole-file id for parsed files.

## Amendments (2026-09-23, from implementation)

- **Write sets do not come from ops.** A record's `write_set` is every carried definition whose content changed between base and result (by content comparison after identity carrying), plus births, deaths, file roots whose glue changed, and coarse paths. Ops are one representation of the change and are validated separately (§3.5). Deriving the write set from ops under-declares whenever the diff falls back to a whole-file root `Replace`.
- **Tier 0 blob-only changes stay valid.** A change that modifies a parsed file with only `Op::Blob` (git import, git sync) is a coarse write to that file: it conflicts with any change that touches the file. Validation accepts a `Blob` for a parsed file only when that file has no structural ops.
- **A write is not a read of the root.** Writing a parsed file does not add its root id to the read set; otherwise every glue edit would be a read-write conflict with every change to the file. Glue edits conflict with glue edits (write-write on the root id) and with coarse writes.
- **Ops are assigned to files by NodeId.** A record's changed files come from the base → result tree diff. Each structural op belongs to the file whose tree holds its NodeId (the root id, or a definition id in that file's base or result).
- **Identity-index row durability (2026-09-23).** The snapshot → `IdentityIndex` row is written without an fsync. It becomes durable with the next ordered durable commit (`set_head` when landing, `queue_push` on submit). The binding object is content-addressed, so `rebuild_index` restores the row, and a missing row is `MissingIdentity`, never a fresh assignment.
- **Merge conflicts name the file root (2026-09-23).** `hord_diff::merge` takes the file's path and uses `file_parent(path)` as the root, the same as `diff` and `apply`, so the nil id is not used anywhere in hord-diff. Contention at file level, in either a soft or a hard conflict, names the file root id, which `hord conflicts` shows as the file. A hard conflict is never reported with an empty node list.
