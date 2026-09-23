# ADR 0016: Directory workspaces are copy-on-write clones of a pristine checkout

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §6.1 (`begin` is O(1), materialization is lazy), §8.3, §11 (`hord-vfs`), §12 M3 (workspace creation < 5 ms, 1,000 live workspaces)
- **Blocks:** M3 workspace gates for `Directory` materialization

## Problem

§6.1 says `begin(base)` is O(1) and materialization is lazy. hord-txn's `InMemory` workspaces meet that (7 µs). `Directory` workspaces (`hord ws new`) write a full checkout at begin: every file's bytes, O(files) of I/O and disk. `propose` then reads and hashes every file to find writes. Without a VFS (M7), a plain directory cannot serve a file it has not written. So "defer everything until the first write" is not available for a real directory. The M3 gates are met only by the `InMemory` path today.

## Options

1. **Full checkout (status quo).** Simple and portable. O(files × bytes) per workspace, and 1,000 cargo checkouts is about 10 GB.
2. **Hard links to a pristine checkout.** Near-free. An editor or tool that writes in place (not by rename) corrupts the pristine copy and every other workspace. Rejected.
3. **Copy-on-write clone of a pristine checkout.** Keep one read-only pristine checkout per base snapshot under `.hord/pristine/<snapshot>/`, written once. Each workspace is a filesystem clone of it:
   - APFS: `clonefile(2)` on the directory, a single call.
   - btrfs/XFS: per-file `FICLONE`.
   - Other filesystems: fall back to a copy.

   Data blocks are shared until a file is written, so the byte cost is paid only for files the agent changes. At clone time, record a stat index (path → size, mtime, inode), git-index style. `propose` re-reads only files whose stat changed, so propose is O(files) `stat` calls, not O(bytes) reads.
4. **Sparse checkout.** `hord ws new --paths <globs>` writes only matching files, and `hord ws add <path>` adds more later. Files that were never written count as unchanged. This defers cost for real, but `cargo build`/`cargo test` inside the workspace needs the whole crate graph, so it only helps agents that edit without building.
5. **VFS.** Serve reads on demand and log them (this also closes ADR 0012's "reads unobserved" gap). This is `hord-vfs` (FUSE/FSKit), sequenced for M7.

## Decision

Option 3, plus the stat index for `propose`, behind `--materialize=clone` (default) with `--materialize=copy` as the fallback. Clone through the `reflink-copy` crate so no hord crate needs `unsafe` (§11.1). Option 4 is not in M3. Option 5 stays M7.

## Consequences

- On APFS, creating a workspace is one metadata clone plus the stat index. It must be measured on cargo, and the < 5 ms gate is reported separately for `InMemory` and `Directory`. Disk use for 1,000 live workspaces is roughly the size of the files agents change.
- The first workspace on a new base pays one full pristine checkout. Pristine directories are read-only (mode bits) and garbage-collected by `hord ws gc` when no live workspace references that base.
- On a filesystem without reflink support, behavior falls back to today's copy. `hord ws new --json` reports which mode was used.
- A tool that preserves mtime and size while changing content defeats the stat index, the same limitation git has. `hord status --paranoid` re-hashes everything.
- New dependency: `reflink-copy` (safe `clonefile`/`FICLONE` wrapper; avoids `unsafe` outside `hord-vfs`).
- Reads in `Directory` workspaces stay unobserved until option 5.

## Amendments (2026-09-23, from implementation)

- **Protection.** Only the pristine's top directory (`.hord/pristine/<snapshot>/`) is read-only. Files and subdirectories keep normal modes. `clonefile` copies mode bits, so read-only files forced an O(files) `chmod` pass on every clone (the dominant cost). A copy-on-write clone cannot write through to the pristine, so file-level protection guarded only against hard links, which option 2 already rejects.
- **Stat index.** The key is path → (size, mtime in ns), with no inode: clones get new inodes. The index is computed once, when the pristine is written, and every clone reuses it. A copied checkout is walked once for its own index.
- **Creation target.** The < 5 ms gate is `begin` (§6.1) and is met by `InMemory` (about 0.1 µs). `Directory` creation (clone mode) must be under 100 ms at p99 on a cargo-sized checkout, with 1,000 live workspaces. Measured on cargo HEAD (2,950 files, 17.9 MB, APFS, release): mean 48.7 ms, p50 47.8 ms, p99 71.6 ms, max 86.0 ms, with no degradation from the first 100 to the last 100. The first workspace on a base writes the pristine (316 ms). Copy mode is a fallback and has no target (392 ms mean). `propose` after one edit takes 39 ms with the stat index, against 268 ms with `--paranoid`.
- **Removal.** `hord ws rm <id>` removes a workspace. `hord ws gc` removes pristine checkouts no live workspace uses as its base.
- Linux `FICLONE` is untested so far. Any clone failure falls back to a copy.
