# Hord

Git was built for people who walk away. A human edits a file, commits, and is gone. All that remains is text. When two of those commits collide, git merges *lines* because there is nobody left to ask what they meant.

Agents are different. They can be asked again.

Hord is a version control system for that world: a **transactional database over a semantic code graph**. A change is not a bag of diffs. It carries *intent* (what the work was for), *provenance* (who or what produced it), declared *read and write sets* (which definitions it depended on and which it touched), and *evidence* (the checks that showed it was valid). Landing is a serializability check plus policy, not a hope that the merge looks right.

The name is Old English for a hoard or treasury (as in *wordhord*). It is still a working title.

## Why git is the wrong primitive for agents

Hundreds of agents against one repository is a reasonable future. Git's unit of isolation is the file, and its unit of conflict is the line. Two agents editing different functions in `lib.rs` still collide. A "conflict" is a region of overlapping text, not "these two changes silently break each other's assumptions."

You can paper over that with smaller PRs and more reviewers. That does not scale to a thousand concurrent authors, most of them models.

Hord's bet is that **the stored artifact should be the code's structure**, and that **a conflicting change should be replayed**, not text-merged.

## Three ideas

1. **Code is a graph; files are a projection.** What Hord stores is a lossless syntax tree with stable identities for definitions (functions, types, impls, modules). The file you open in an editor is reconstructed from that tree, byte-identical to what went in, comments and whitespace included. Two agents editing different functions in the same file do not conflict.

2. **A change is `{intent, ops, provenance, read_set, write_set, evidence}`.** Ops are semantic (insert, replace, move, rename) and are checked against the actual tree delta. Evidence is tied to a snapshot, so it can go stale when the base moves. Policy decides what must pass before a change lands.

3. **Conflicts are resolved by replay, not merge.** First, a structural rebase on disjoint nodes. If that is not enough, rerun the recorded intent against the new base and re-verify. If that still fails, a human (or a designated arbitrator) gets a summary of *why*, not a wall of `<<<<<<<`.

Branching becomes an optional view over a transaction log, not the thing the system is made of. Workspaces are O(1) copy-on-write snapshots. History is an index you can query at the granularity of a function: who last changed this, why, and which test covers it.

## Git is the on-ramp, not the enemy

Nobody switches version control systems. Hord is designed to sit *on* git first.

Import and export are byte-exact. `export(import(repo))` must reproduce every git tree SHA. A landed Hord change can be mirrored as an ordinary git commit with structured trailers. Humans who stay on git tooling keep working; incoming git pushes become proposals and go through the same lander as agent work.

The long-term picture is that Hord is the system of record and git is a mirror. The short-term picture is that you can try Hord on a repository you already have.

## What this is not

Hord does not replace the compiler or the test runner. It invokes them and records the result.

It does not try to prove that a semantic merge is correct. Merges are heuristic and then verified.

It is not GitHub. There is a presentation layer for the lander queue, semantic changes, and arbitration, because humans still govern the system. There is no plan to grow issues, wikis, and a CI product around that.

v1 is not a peer-to-peer multi-master. Landing is serialized per repository. Offline *work* is fine; offline *landing* is not.

The first language with real semantic support is Rust, so Hord can eventually host itself. Other languages get syntax-tier or blob-tier treatment until an adapter exists.

## Status

This is early. The contract is [`docs/spec.md`](docs/spec.md). Architecture decisions live in [`docs/adr/`](docs/adr/).

**M0** is the skeleton: content-addressed objects, a local store, and a git bridge that treats every file as a blob. CI runs the test suite on Linux, macOS, and Windows (encoding goldens must agree) and the git round-trip eval on Ubuntu. Later milestones add structural Rust, identity, transactions, verification, replay, and self-hosting.

If you want the detailed argument, start at spec §0 and §2. If you want to implement, the same spec is the contract the code is written against.
