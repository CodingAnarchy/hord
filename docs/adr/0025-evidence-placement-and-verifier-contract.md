# ADR 0025: Evidence lives beside snapshots, and the verifier returns it

- **Status:** accepted
- **Date:** 2026-09-24
- **Spec:** §3.6 ("valid for a snapshot, not a change"), §4.3 (`Verifier`), §6.5 ("fresh evidence is attached before landing"), §6.7 (speculative verification), §7.1 (reuse keyed by snapshot, toolchain, command, scope), §10.5.2 (`attach_evidence`)
- **Blocks:** M4 evidence reuse, the lander running verification, and speculative verification

## Problem

`ChangeRecord.evidence` is part of the record's hash, but §6.5 says fresh evidence is attached *before landing*, after the record exists. Attaching evidence then means yet another `ChangeId` for every landed change. Evidence is also valid for a snapshot, not a change (§3.6), so a record-owned list is the wrong key for reuse. The M3 `Verifier` returns only `Pass | Fail { reason }` and cannot carry the evidence it produced. The lander prepares, verifies, and lands one change at a time, so it cannot verify N+1 against `head + N` (§6.7).

## Options

1. **Evidence inside the record (status quo).** It is simple, but every attachment creates a new record id, and reuse by snapshot needs a scan of every record.
2. **Evidence beside snapshots.** Evidence objects are stored as they are today. An index keyed by `(snapshot, toolchain, command, scope)` finds them for reuse (§7.1). `ChangeRecord.evidence` holds only what the author attached at `propose`, part of the author's claim. The lander's verification evidence is attached to the landed snapshot through the index, and listed in the `Landed` event, not written into the record. `attach_evidence(change, ev)` (§10.5.2) resolves the change to its result snapshot and indexes the evidence there.
3. **A separate "landing" object per landed change** that lists the landing's evidence and links to the record. This adds an object kind whose content duplicates the evidence index.

## Decision

Option 2. Evidence is indexed by snapshot, and landing evidence is attached to the landed snapshot, never written into a `ChangeRecord`. The `Verifier` contract returns evidence:

- `plan(snapshot, impact, policy) -> VerifyPlan`, with reuse applied: evidence already indexed for the same key is not re-run;
- `run(workspace, plan) -> Vec<Evidence>`;
- the lander's verdict is `Pass { evidence } | Fail { evidence, reason }`.

The lander keeps a window of up to K prepared candidates stacked on one another (§6.7, default K = 4). Each candidate's identity is staged in memory. A candidate that fails invalidates the ones above it, which are re-prepared.

## Consequences

- **"Resubmitting an unchanged change against an unchanged head re-runs nothing" follows directly.** The candidate snapshot is the same, so every evidence key hits.
- **Policy (§7.2) reads evidence for a snapshot from the index.** `require = ["test:selected"]` is satisfied by `Pass` evidence of that kind for the landed snapshot.
- **The ADR 0018 `Rebase` attestation stays in the record**, because it is part of what the lander claims about the record, not verification.
- **Speculative verification.** A failed candidate's evidence stays indexed for its snapshot, which is still a true fact about that tree. It becomes garbage only if nothing references that snapshot (store GC, a later ADR).
- Writing verification evidence into change records needs a new ADR.
