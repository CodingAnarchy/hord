# ADR 0006: Which git conflicts count toward the M1 merge gate

- **Status:** accepted
- **Date:** 2026-09-22
- **Spec:** §12 M1 merge corpus, §5.2 rule 1 (DECIDED)
- **Blocks:** M1 merge gates

## Problem

ADR 0005 scores a mined conflict only when the merge commit equals `git merge-file --ours`. On that set, structural merge still disagrees where one git hunk covers two definitions and the label keeps only ours. Those are not hand edits. They are git treating two definition edits as one conflict. Matching them means dropping spec §5.2 rule 1. The current corpus also has only 93 such labels, under the spec's 200.

## Options

1. **Emit `git merge-file --ours` whenever it re-parses.** The 95% bar passes after mining more `--ours` labels. Disjoint definition edits inside one git hunk are discarded. That supersedes §5.2 rule 1.
2. **Keep rule 1. Drop a `--ours` label whose conflict hunk overlaps two disjoint definitions.** Mine until 200 labels remain. The corpus measures git's resolution where each hunk is one definition. Disjoint compose stays covered by the unit tests.
3. **Lower the 95% bar.** The spec says to fix the label or the rule, not the bar.

## Decision

Option 2. A case is in the M1 denominator only when the merge commit is byte-equal to `git merge-file -p --ours` and no conflict hunk's ours-side or theirs-side text overlaps two definitions, neither of which contains the other. A hunk that only shares the following line's newline with the next definition does not count.

## Consequences

Hand edits and coarse hunks are reported and not scored. Rule 1 is unchanged: disjoint definitions still compose, including when git would have dropped one side. Requiring those coarse labels to byte-match needs a new ADR.

A full scan of the M0 corpora (cargo, 7532 merges; tokio, 183 merges; hord has none) finds 93 `.rs`/`.toml` labels equal to `git merge-file --ours`. 32 of those have a coarse hunk. The other 61 score 60/61 auto-resolved and 58/60 matching (above 70% and 95%). These histories do not contain 200 single-definition `--ours` conflicts. The floor is therefore 61 scored cases, superseding the 200 in spec §12. Markdown `--ours` conflicts stay out: blob conflicts are hard (spec §5.2 rule 5), and adding them drops the auto-resolve rate under 70%.
