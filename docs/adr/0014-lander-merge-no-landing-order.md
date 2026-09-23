# ADR 0014: The lander never merges by landing order

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §5.2 rule 2, §6.3, §6.4 rung 1, §12 M3; amends ADR 0005
- **Blocks:** M3 (false negatives must be 0)

## Problem

ADR 0005 says a same-node `Replace` pair tries a positional CST merge, then a line merge. If both fail, it keeps ours (landing order) and adds only theirs' new child definitions. That fits the M1 corpus, whose labels equal `git merge-file --ours` (ADR 0005, ADR 0006). In the lander, "ours" is the landed head and "theirs" is the submitted change. So the fallback reports a clean rebase that silently discards the submitted edit. The change lands under its intent without its content. That is a false negative, and M3 requires 0.

hord-txn currently detects this after the fact: a definition that both sides edited, where the merged bytes equal one side's. That check has to exclude §5.2 rule 2 (both sides made the same edit), and it breaks when merge internals change.

## Options

1. **Detect in the lander (status quo).** No hord-diff change. The check is a heuristic over merge output and is a second copy of merge rules outside hord-diff.
2. **Merge mode in hord-diff.** `merge` takes a mode. `Corpus` (the default today) keeps ADR 0005's landing-order fallback. `Lander` turns a failed same-node merge into a hard `Conflict` naming the node. The CST merge and line merge run unchanged in both modes, as do equal-`normalized` composition and disjoint composition.
3. **Drop the fallback everywhere.** One behavior. It supersedes ADR 0005 and drops the M1 auto-resolve rate on cases git itself resolved with `--ours`.

## Decision

Option 2. `hord_diff::merge` (and `merge_ops`) take a `MergeMode`. hord-txn always uses `MergeMode::Lander`, and removes its after-the-fact lost-edit check. M1 scores and `bench/m1-eval` use `MergeMode::Corpus` and are unchanged.

## Consequences

- A same-node edit that the CST merge and line merge cannot combine parks the change at rung 1 (a hard conflict, then replay in M5). It never lands one side.
- The M1 merge-gate numbers describe `Corpus` mode only. They are not a claim about lander auto-merge rate (§13). The lander's rate is measured by the M3 simulation.
- Any future fallback that keeps one side's bytes must be off in `Lander` mode. Enabling it there needs a new ADR.

## Amendments (2026-09-23, from implementation)

- **When a compose invents a line.** In `Corpus` mode, a structural compose that produces a non-blank line absent from base, ours, and theirs is replaced by `git merge-file --ours` (ADR 0005). In `Lander` mode it is replaced only by a line merge that keeps both sides. The result must re-parse, and it is flagged as a soft conflict. If no such merge exists, it is a hard conflict. `git merge-file --ours` is never used in `Lander` mode.
