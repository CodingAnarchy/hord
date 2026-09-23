# ADR 0020: Moving or renaming a file carries its definitions' NodeIds

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.4 rules 3–4 (Moved, Renamed), §3.5 (`Op::Tree` rename, `Op::Move`), ADR 0007 (rename similarity), ADR 0015 (file roots)
- **Blocks:** §13 identity churn over real history; M6 blame

## Problem

`propose` carries identity per path: each file's result is matched only against the same path in the base. Deleting `src/a.rs` and writing the same bytes to `src/b.rs` produces two deaths, two births, and `[Delete, CreateFile]`. Every NodeId in the file is lost. ADR 0015 already says "a rename is `Op::Tree Rename`, and its definitions `Move` from the old root to the new one", and ADR 0007 says cross-file renames are found only when both files are in the compared pair of trees. Neither is implemented, so the code and the accepted ADRs disagree.

## Options

1. **Pair deleted and created files in `propose`.** Within one change, pair each deleted file with a created file:
   - by blob equality first;
   - then, for the files still unpaired, by the fraction of definitions whose `normalized` hash appears in both, taking the best pair at or above ADR 0007's threshold.

   Carry identity across each pair (§3.4 rules 1–4 with the old file as base), and emit `Op::Tree { kind: Rename { to } }` plus `Move` from the old root to the new one. An unpaired file stays a death or a birth.
2. **Amend ADR 0015: file moves are birth plus death.** Simple, and honest about today's behavior. `git mv` then resets blame for every definition in the file.
3. **Carry across all files in the snapshot.** Match every born definition against every dead one repository-wide. It catches moves between existing files, but it is quadratic in definitions per change and prone to false carries between unrelated files.

## Decision

Option 1. `propose` pairs the files a change deleted with the files it created, by blob equality and then by definition overlap at ADR 0007's threshold. Each pair carries identity and is recorded as a file rename plus definition moves. Definitions that move between two files that both exist before and after the change are births and deaths, unless the change declares them (§3.4 rule 5).

## Consequences

- **`git mv`** keeps every NodeId. A rename that also edits the file keeps the ids of the definitions §3.4 still matches.
- **Conflict checks** see both the old and new path ids (ADR 0015). A concurrent edit to the old path conflicts with the move.
- **Git import** gets the same pairing for free, since it proposes through the same path.
- Carrying definitions between existing files without a declaration needs a new ADR.
