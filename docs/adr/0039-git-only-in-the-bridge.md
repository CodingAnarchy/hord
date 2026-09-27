# ADR 0039: Git is confined to the git bridge

- **Status:** accepted
- **Date:** 2026-09-26
- **Spec:** §5.2 rule 5 (blob merge), §9 (the git bridge), §11 (crate layout); ADR 0006 (merge corpus labels), ADR 0036
- **Blocks:** nothing; it sets a boundary

## Problem

Hord's merges don't need git. The lander's merge mode uses hord's own line and token 3-way merge, and blob-tier files use `diffy`. But `hord-diff`, a core library, still spawns `git merge-file --ours` in `MergeMode::Corpus`, the mode the M1 merge-corpus evaluation uses to match ADR 0006's labels. So a core crate depends on a `git` binary being installed, and the rule that git belongs only to the bridge is nowhere written down or enforced.

## Options

1. **Keep git for corpus mode.** It only runs in an eval, but a core crate still spawns git, and the boundary stays implicit.
2. **Move corpus mode's git call into `bench/m1-eval`.** Git leaves `hord-diff`, but the eval measures git's merge rather than hord's in that step.
3. **Implement "ours on conflict" in hord's own 3-way merge, and confine git to the bridge.**

## Decision

Option 3.

- **Corpus mode** resolves conflict hunks to the landing (ours) side with `hord-diff`'s own line 3-way merge, and the `git merge-file` subprocess is removed.
- **The M1 corpus must still meet its targets** (spec §12 M1): ≥ 70% of scored cases auto-resolved, 100% of auto-resolutions parsing, and ≥ 95% matching the labels. Any case whose outcome changes is reviewed, and a label or rule is fixed per ADR 0006 rather than accepted blindly.
- **Git runs only in the git bridge:** `hord-git` (import, export, `hord git sync`) and the CLI's `hord git …` commands, which call it. Corpus mining scripts in `bench/` may use git, since the corpus is mined from git history. No other crate spawns `git` or links a git library. A test enforces this.
- **Spec §5.2 rule 5** reads "a diff3-style 3-way line merge (`diffy` or similar)", with no reference to git.

## Consequences

- `hord` runs with no `git` installed, except for the bridge commands.
- Adding git to any other crate needs a new ADR.
