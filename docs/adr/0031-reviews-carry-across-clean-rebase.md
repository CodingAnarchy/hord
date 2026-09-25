# ADR 0031: A review carries across a clean structural rebase

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §3.6 (evidence is valid for a snapshot), §7.2 (`review:*` evidence signed against the exact snapshot), §6.4 rung 1; amends ADR 0025 for reviews; ADR 0018 (rebased records)
- **Blocks:** M5 review round-trip when head moves between proposal and review

## Problem

`hord review <change>` signs `Evidence { kind: Review }` against the change's `result` (ADR 0025). If head has moved, the lander judges a rebased result instead (ADR 0018). That snapshot is stored only when the change lands, so no reviewer can see or sign it. A change that needs `review:human` then stays parked no matter how many humans approve it.

## Options

1. **Carry a review across a clean rebase.** Policy also counts `review:*` evidence indexed for the submitted record's `result`, provided the lander's rebase had no hard merge and no adapter-merged files.
2. **Review the candidate.** The lander stores the rebased candidate when it parks, and reviewers sign that. It is exact, but every move of head forces a new review.
3. **Attach reviews to the change id.** This is simple, but it breaks §3.6 and would carry a review across a merge nobody saw.

## Decision

Option 1. When judging a rebased candidate, the lander's policy facts also count `review:*` evidence on the submitted record's `result`, but only when the rebase report has no `merge` and no `adapter_merged` files. The `Rebase` attestation already links the two snapshots.

## Consequences

- Reviewers approve the author's edits once. A clean rebase does not change those edits, so the review stands.
- Machine evidence (`check`, `test`, `lint`, `bench`) still counts only for the exact snapshot and re-runs as before. The exception covers reviews only.
- A rebase that merged anything, structurally or through an adapter, needs a fresh review of the candidate. That path needs a reviewable candidate id, and it is left to arbitration and replay (rungs 2–3) until a later ADR.
