# ADR 0019: Birth NodeIds are derived from content, file, site, and base snapshot

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.1 (`NodeId`: "Assigned once, carried forward. ULID-encoded"), §3.4 rule 6 (birth), §11.1 (determinism); supersedes §3.1's ULID assignment
- **Blocks:** M5/M6 lineage and blame

## Problem

M2 and M3 replaced random ULID births with deterministic ids, because AGENTS.md requires "same inputs → same ObjectIds" and a random birth id leaks into `IdentityMap` and `ChangeRecord` hashes. The current derivation is `f(definition content id, file root id, occurrence salt)`, and no ADR records it. The review found two defects:

- **Resurrection.** Delete `foo` and land, then re-add an identical `foo`: it gets its old id back, and `node_history(foo)` joins two unrelated lifetimes.
- **Concurrent collisions.** Two changes from the same base that add the same definition to the same file get the same birth id. The rebase re-salts one of them, and ADR 0018 records the recomputed id.

## Options

1. **Random ULIDs (§3.1 as written).** No resurrection and no collisions. But ids are non-deterministic: the same change proposed twice gets different ids and different `ChangeId`s, which breaks reproducible landings and the determinism tests.
2. **Content + file + site + base snapshot.** `birth = derive("hord/birth", content id, file root id, site, base SnapshotId)`, with an occurrence salt for duplicates at one site. Deterministic for a given proposal. A re-added definition is born on a later base, so it gets a new id. Two concurrent identical births from one base still collide. The lander's rebase detects the collision and re-salts with the head snapshot. ADR 0018 records the result in the landed record.
3. **Death registry.** Keep the current derivation, record every retired id, and re-salt any birth that would reuse one. This needs a registry that every assigner can see (remote clients included), which is a new piece of replicated state.

## Decision

Option 2. A birth id is derived from the definition's content id, its file's root id, its site, and the base snapshot of the change that creates it, and it is formatted as a 128-bit NodeId. A collision found at landing is resolved by re-deriving with the head snapshot, and the landed record's `identity_deltas` carry the final id.

## Consequences

- **§3.1 superseded.** NodeIds are deterministic, not ULIDs, and there is no timestamp component. "Assigned once, carried forward" still holds: carrying rules (§3.4 rules 1–5) are unchanged, and only rule 6's new ids change.
- **A deliberate revert keeps identity only by declaration.** An agent restoring a deleted definition declares `derived_from` (§3.4 rule 5). Otherwise the restored definition is a new lifetime.
- **Assign and carry take the base snapshot.** `assign_in`/`carry_in` gain the base snapshot as an input. Existing M2 numbers are unaffected: identity stability measures carrying, not birth ids.
- Changing the derivation inputs needs a new ADR, because it changes every future id.
