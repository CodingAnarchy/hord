# ADR 0017: A snapshot is a `Snapshot` object that names its identity

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.1 (`SnapshotId`), §3.2 (`Snapshot`, `IdentityMap`), §8.1 (index rebuildable from objects), §10.5.2 (`RepoBackend`); supersedes §3.1's definition of `SnapshotId`
- **Blocks:** M4 wire format and conformance suite; `hord blame`/`log --node`/`query` on snapshots the lander produces

## Problem

§3.1 says a `SnapshotId` is the root tree's `ObjectId`. §3.2 says a `Snapshot` object is the "root `Tree` + repository-level metadata + index pointers". The code follows §3.1, so no object links a snapshot to its NodeIds. A file's carried NodeIds live in an `IdentityIndex` object, and only a redb row points from the snapshot to it. The M3 review fix binds that row to a content-addressed object so `rebuild_index` can restore it, but the binding is still a sidecar that `get_objects` does not carry. The consequences:

- A remote client (M4) that fetches a snapshot's objects cannot learn its NodeIds, so it builds read and write sets over ids the server does not use. Missed read-write conflicts are false negatives.
- Two identity systems exist. M2's `blame`, `log --node`, and `query` read the `identity` and `edges` tables, which only `bench/m2-eval` writes. The lander writes `identity_index`. On a repository that uses `hord land --local`, `hord blame` fails ("no identity map in the log").
- One `IdentityIndex` per snapshot is a flat list of every parsed file (about 194 KB on cargo), and each landing stores one or two. That is about 3.9 GB per 10,000 changes.

NodeIds depend on history, not only on content: the same bytes reached by two histories can carry different ids. So identity cannot be derived from the tree alone. It has to be stored, and stored where the snapshot's object graph reaches it.

## Options

1. **`Snapshot` object (§3.2 wins).** `Snapshot { root: TreeId, identity: IdentityTreeId, meta }`, with `SnapshotId` defined as its `ObjectId`. The identity tree is Merkle-shaped like `Tree`: one node per directory, and one `FileIdentity` object per parsed file (the site → NodeId list). Unchanged subtrees share objects. `ChangeRecord.base`/`result` name `Snapshot`s. The root tree id stays a pure function of file content, which the git bridge maps to git trees.
2. **Identity in tree entries.** `Tree` entries for parsed files become `NodeFile { blob, identity: FileIdentityId }`, and `SnapshotId` stays the root tree id. There's no new object kind. But tree ids then depend on history, so two snapshots with identical files have different trees, "same tree" stops meaning "same content", and the git bridge needs a second, content-only tree hash to compare against git.
3. **Keep the sidecar (status quo after the review fix).** The binding object plus redb row, and a `RepoBackend` method to fetch it. It works locally, but a snapshot's objects are not self-describing: every consumer (sync, replication, GC reachability) must know about the sidecar.

## Decision

Option 1. A snapshot is a `Snapshot` object whose id is the `SnapshotId`. It names the content root (`root`) and a Merkle identity tree (`identity`). The lander and git import write it, and every identity reader (propose, rebase, blame, log, query, edges) reads NodeIds from it. The `identity`, `edges`, and `identity_index` redb tables become indexes derived from `Snapshot` objects, and `rebuild_index` rebuilds them.

## Consequences

- **§3.1 superseded.** `SnapshotId` is the `Snapshot` object's id, not the root tree's id. "Same content" is `a.root == b.root`.
- **Store format change.** Snapshot ids change for every stored snapshot. Existing stores are test and bench repositories created per run, so there is no migration. The store gets a format version, and an older store is refused with a clear message.
- **Git bridge.** It maps `Snapshot.root` to git trees. The M0 round-trip invariant (tree SHAs) is unchanged, because it is about `root`.
- **`FileIdentity` sharing.** A `FileIdentity` equal to the fresh deterministic assignment (ADR 0019) is omitted from the identity tree, and readers fall back to the assignment. This keeps git-imported snapshots small.
- **Query moves to the library.** M2's query path moves out of `hord-cli` into a library `Query` type (§10.1) over `Snapshot` identity. Integration test: land through `hord land --local`, then `hord blame` and `hord log --node` resolve.
- **Missing identity is an error.** A snapshot the log names but whose identity tree is missing is `MissingIdentity`, never a fresh assignment.
- Putting identity anywhere else again needs a new ADR.
