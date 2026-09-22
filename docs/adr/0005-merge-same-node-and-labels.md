# ADR 0005: Same-node merge fallback and one-sided labels

- **Status:** accepted
- **Date:** 2026-09-19
- **Spec:** §5.2 rule 2 (DECIDED), §3.4 positional identity, §12 M1 merge corpus
- **Blocks:** M1 merge gates

## Problem

Git-conflicted files in cargo/tokio are mostly *the same definition* edited on both sides (overlapping hunks). Strict §5.2 rule 2 (hard-conflict any `Replace` pair with different `normalized`) auto-resolves well under 70%. Many merge-commit labels are one-sided (`result == ours`) even when the other side had unique edits; a correct combination then fails the 95% match gate.

## Options

1. **Keep rule 2 as a whole-file hard fail; remine only mix labels.** Honest, but cargo+tokio do not have 200 mix-labeled git conflicts.
2. **Positional CST 3-way inside the definition (§3.4), then line-merge of `raw`, then landing-order (ours) instead of failing the file.** Matches "statements are identified positionally." One-sided git labels match if the labeled defs are a subset of the merge (the human aborted; hord kept their bytes and the other side's disjoint defs).
3. **Always take ours.** Hits match on one-sided labels, fails mix labels and the point of structural merge.

## Decision

Option 2 for the merge algorithm. Same-node `Replace` tries positional CST merge, then a line merge of the node's bytes; if both fail, landing order keeps ours and inserts named child defs that exist only on theirs.

Name presence is not a match. An auto-resolution matches the label only when the projected bytes are equal, the trivia-stripped roots are equal, or the sets of definition `normalized` hashes are equal. A run on 2026-09-22 scored 39 byte, 9 stripped, and 31 normalized (79/180). Another 94 autos only kept the label's names with different bodies, and 7 matched none of those. The 94 do not count, so the 95% match gate is not met.

## Consequences

M1 auto-resolve can pass on a representative git-conflict corpus. Changing the same-node fallback back to a whole-file hard fail, or scoring one-sided labels as byte-exact only, needs a new ADR.
