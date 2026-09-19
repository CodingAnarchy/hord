# ADR (open): Bulk git import writes packs directly

- **Status:** open (not yet decided)
- **Date:** 2026-09-18
- **Spec:** §8.1 (DECIDED: "Loose objects for recently written data, packed by a background job")
- **Blocks:** nothing (M0 is green at ~1000 commits/s vs. the 200 target)

## Problem

On the cargo corpus, ~65 % of import wall clock is `open`/`write`/`close`
creating ~209k loose object files (one per object, 256 shard directories).
Nothing else on the path is above 10 %. The remaining 2× is only reachable
by not creating one file per object during bulk ingest.

## Options

1. **Import writes a pack directly.** `import_git*` opens a `PackWriter`
   and `put` appends objects to it; the redb `objects` table is populated at
   the end. Loose objects stay the path for interactive `put`. Fastest;
   the store gains a "bulk sink" mode and `get` during import must read from
   the in-progress pack (or an in-memory index of it).
2. **Write-behind pool.** `put` hashes and queues; N threads create the
   loose files. `get` must consult the queue. Keeps the loose layout; gains
   are bounded by how well the filesystem parallelizes directory inserts
   (not measured here).
3. **Do nothing.** 1000 commits/s already exceeds the M0 target 5×.

## Why this is an ADR and not a patch

Option 1 changes the meaning of "recently written data is loose"; option 2
adds threads and a queue to a store that is otherwise synchronous. Both are
architecture, not a hot-path fix. Decide when import throughput becomes a
real constraint (large monorepo import, M6 sync).
