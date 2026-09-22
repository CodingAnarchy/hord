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

Name presence is not a match. An auto-resolution matches the label only when the projected bytes are equal, the trivia-stripped roots are equal, or the sets of definition `normalized` hashes are equal.

A mined conflict is an auto-resolution candidate only when the merge-commit file is byte-equal to `git merge-file -p --ours`. That is git's 3-way: edits that do not overlap, plus the landing-order side of each conflict hunk. A label that introduces a line, reorders a region, or rewrites a hunk was edited by hand and is not scored. Case 0008 (`let _test` in neither parent) and case 0069 (both sides of one hunk, including a token merge) are manual. The auto-resolution of 0069 is `git merge-file --ours`, not the merge commit.

A structural projection that contains a non-blank line absent from base, ours, and theirs is not an auto-resolution. When `git merge-file --ours` re-parses losslessly, that output is used instead. List commas are not part of a field or variant's `raw`; an insert of the definition puts the source's following `,` back so the merge does not invent a comma-less line.

Edit order follows tree depth and content id. `NodeId` is a random ULID and must not change the projected bytes.

Of the 200 mined conflicts, 93 labels equal `git merge-file --ours`. A deterministic run on 2026-09-22 scored auto 91/93 and strong match 78/91 (byte 51, stripped 8, normalized 19), with 13 names-only and 2 delete-vs hard conflicts. The 13 are disjoint definition edits that git put in one conflict hunk and resolved by taking ours. Spec §5.2 rule 1 still composes those, so the bytes differ from `--ours`. ADR 0006 drops those coarse hunks from the denominator and sets the scored-case floor at 61.

## Consequences

Manual rewrites are not in the merge-gate denominator. Disjoint definition edits still compose even when `git merge-file --ours` drops one side. ADR 0006 keeps that rule and drops those coarse hunks from the denominator. Stopping the compose, so those labels byte-match, supersedes §5.2 rule 1 and needs a new ADR. Changing the same-node fallback back to a whole-file hard fail also needs a new ADR.
