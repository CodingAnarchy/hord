# ADR 0018: What a rebased change record says, and who signs it

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.5 (`ChangeRecord`), §6.3–6.4 (rebase), §6.6 (`parent_intent`), §10.5.4 (signatures); amends §10.5.4
- **Blocks:** correct `node_history` today; M4 evidence; M5 signature verification and provenance trace

## Problem

When a change lands on a head other than its base, the lander writes a new record under a new `ChangeId`. It recomputes only `base`, `result`, `parents`, and `ops`, and copies the rest: `read_set`, `write_set`, `identity_deltas`, `provenance`, `evidence`, and `signature`. The landed record can then misstate what it did. In the review's repro, two changes add an identical `helper` from the same base; the rebase re-salts the second birth id, but the record still claims the old id, so `node_history` is wrong. A copied `signature` covers the submitted record, not this one, so §10.5.4's "verify on ingest" either rejects every rebased change or skips verification for them. The only link from the landed record to the submitted one is a redb queue row, which is not an object and cannot be rebuilt.

## Options

1. **Recompute sets and deltas; keep the author's signature on the submitted record; add a lander attestation.** Fields change as follows:
   - **Recomputed** from `head → result`, exactly as `propose` computes them: `ops`, `write_set`, and `identity_deltas`.
   - **Kept:** `read_set` (what the author depended on does not change when the lander moves the change), `intent`, `provenance`, and `evidence`. `provenance.actor` stays the author, because the intent and the edit are theirs.
   - **New field:** `rebased_from: Option<ChangeId>`, naming the submitted record.
   - **Cleared:** `signature`. The author's signature stays valid on the submitted record, which the landed record names.
   - **Lander attestation:** an `Evidence { kind: Rebase { submitted, landed } }` signed with the lander's key, listed in `evidence`.
2. **Land the submitted record unchanged, and keep rebase results outside it.** The log would then hold records whose `base`/`result` are not the snapshots they were applied to. That breaks §3.5's "ops reproduce result from base" for everything in the log.
3. **Put the submitted id in `intent.refs`.** No new field. But the intent is the author's text and acceptance criteria, and a lander-added ref makes the landed intent differ from what the author wrote.

## Decision

Option 1. A landed record that was rebased recomputes `ops`, `write_set`, and `identity_deltas` from `head → result`. It keeps `read_set`, `intent`, `provenance`, and `evidence`. It names the submitted record in a new `rebased_from` field, and it carries no author signature. The lander attests the rebase with signed `Rebase` evidence.

## Consequences

- **`node_history`** follows the landed record's recomputed deltas. A submitted id resolves to its landed id through `rebased_from` (the index is derived and rebuildable).
- **§10.5.4 is amended.** An author signs the submitted record. The server verifies that signature on ingest. The landed record is trusted through `rebased_from` plus the lander's `Rebase` attestation. Until M5 adds keys, the attestation is unsigned but present.
- **`parent_intent`** stays reserved for replay (§6.6).
- A record whose `rebased_from` names a missing object fails validation.
- **The rebased record is stored only when it lands** (after verification), so a failed verification leaves no orphan record.
- Recomputing the read set at landing, or re-signing as the author, needs a new ADR.

## Amendments (2026-09-23, from implementation)

- **The attestation names only the submitted record.** `Rebase { submitted, landed }` would be a hash cycle: the landed record's id covers `evidence`, which would hold an attestation that names that id. The attestation is `Evidence { kind: Rebase { submitted }, snapshot: <landed result> }`, and the landed record lists it in `evidence`. The landed record's own id and `rebased_from` link both records.
