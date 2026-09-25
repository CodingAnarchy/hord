# ADR 0033: A definition moved unchanged to another file keeps its identity

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §3.4 rule 3 ("Moved: same `normalized` hash under a different parent → same `NodeId`, emit `Op::Move`"); amends ADR 0020
- **Blocks:** M5 arbitration ("take theirs" on move vs edit), blame and history across file moves

## Problem

ADR 0020 pairs deleted files with created files, and treats definitions that move between files existing before and after as births and deaths unless declared. A definition moved out of a file that stays into a new file (`parse` from `src/lib.rs` to a new `src/util.rs`) matches neither rule, so it is recorded as `Death(old)` + `Birth(new)`. This breaks three things:
- "Take theirs" cannot follow the definition, so it duplicates it.
- Blame and history lose the definition at the move.
- The rung-1 merge cannot combine a move with an edit.

## Options

1. **Exact-body moves keep their id across files.** This applies §3.4 rule 3 across file boundaries: a definition whose `normalized` hash is unchanged keeps its `NodeId` and emits `Op::Move`, whichever files it leaves and enters.
2. **Also near-equal bodies** (ADR 0007's threshold, same kind and name). This catches a move plus an edit in one change, but it is a broader heuristic with more false-match risk.
3. **A heuristic local to "take theirs".** Identity is unchanged, and blame and history still break.
4. **Leave it to the arbiter.**

## Decision

Option 1. `identify` applies rule 3 across files for definitions with an equal `normalized` hash, among the definitions a change removed from one file and added to another. It covers both new and existing target files, and supersedes ADR 0020's "births and deaths between existing files" clause for exact bodies.

## Consequences

- A move keeps its identity even when the target file already existed.
- A definition moved and edited in the same change is still a birth and a death unless the change declares it (§3.4 rule 5). Near-equal matching (option 2) needs its own ADR.
- If several candidates share one `normalized` hash (duplicated bodies), ambiguous matches are not carried: they stay births and deaths, deterministically.
- "Take theirs" splices at head's current location of the `NodeId`, and the structural merge can combine an edit with a cross-file move.
- The M2 identity corpus is rerun, and its results must not regress.
