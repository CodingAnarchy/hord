# ADR 0023: Grade selection safety with seeded faults on real commits

- **Status:** accepted
- **Date:** 2026-09-24
- **Spec:** §12 M4 selection safety and efficiency (supersedes how "run (a) full `cargo test`" is measured), §7.1, ADR 0022
- **Blocks:** M4 selection gates

## Problem

§12 M4 says: on 500 real cargo commits, run (a) the full `cargo test` and (b) the hord-selected tests, with zero cases where (a) fails and (b) passes. Two things make that unworkable as written:

- **Cost.** cargo's full test suite takes on the order of 10–20 minutes on a laptop. 500 runs is 80–160 hours, before the coverage runs ADR 0022 needs.
- **Weak signal.** Commits on cargo's main branch passed CI, so (a) almost never fails. With (a) passing, a zero-miss result says nothing about selection. The gate would pass even with a selector that runs no tests.

## Options

1. **Literal gate.** 500 full runs plus selected runs. Days of compute for a result that is nearly vacuous.
2. **Seeded faults on real commits (mutation-based regression test selection evaluation).** For each of the 500 commits, apply the commit, then inject one deterministic fault into one definition the commit changed. The fault is one of: the body panics, returns `Default::default()`, or flips a boolean/comparison, whichever parses and type-checks. Then run:
   - **(a)** every test in the packages affected by the commit. Cargo's package dependency graph is a sound superset for Rust, because a test outside the affected packages cannot link the changed code.
   - **(b)** hord's selection.

   A **miss** is a fault that (a) detects and (b) does not. The gate is zero misses over the faults that (a) detects. To stay within a laptop-scale budget (target ≤ 12 h per full run), (a) runs per affected package with `cargo test` caching and `--no-fail-fast`, and a fault that nothing in (a) detects is recorded and excluded.
3. **A smaller corpus.** Use a crate with a fast suite instead of cargo. It is cheaper, but it does not exercise subprocess-driven tests, which are the hard case in ADR 0022.

## Decision

Option 2. Selection safety is graded on 500 real cargo commits with one seeded fault per commit in a changed definition. The gate is zero faults detected by the affected packages' full tests but not by hord's selection. Efficiency (median selected ≤ 20% of the suite for write sets ≤ 5) is measured on the unmutated commits, as §12 says.

## Consequences

- **The harness lives in `bench/m4-eval`.** It shares the fault injector with a unit test that proves each fault kind is caught by a test that calls the faulted function.
- **The literal "full `cargo test`" run is reported for a sample of commits (default 20), not gated.** It is also where flaky tests are found and quarantined; a quarantined test is listed in the report.
- **Faults are deterministic** (seeded per commit), so a miss reproduces.
- Changing the fault kinds, the budget, or the (a) superset rule needs a new ADR.

## Amendments (2026-09-24, from implementation)

- **150 commits, several faults per informative commit.** A single commit's grading costs minutes of instrumented cargo tests, and in the 50-commit baseline only 10 of 32 single faults landed where a miss was possible, because selection (b) did not contain all of (a).
  - The gate grades 150 real cargo commits, as independent chains of consecutive first-parent commits taken from different periods of history. Each chain starts from its own full instrumented run and is graded exactly as the lander sees it.
  - On each commit where (b) does not contain (a), up to 5 separate seeded faults are graded: different written functions, or different fault kinds, each built and graded on its own. That is about 300 informative faults, against about 150 from 500 × 1.
  - Commits where (b) contains (a) keep one probe-graded fault.
  - The report gives per-fault and per-commit counts, because faults within one commit are correlated. The gate stays zero misses.
- **It runs on CI, not on a developer machine.** It uses standard GitHub runners, split into enough chains (about 15 chains of 10 commits) that each job stays well under the 6-hour job limit. It runs nightly or on demand, and a merge job combines the chains into one report and verdict.
- Efficiency is unchanged: the median selected share for write sets ≤ 5, from the chains' selections.

## Amendments (2026-09-25, after the first CI gate run)

- **Chain windows start after cargo `89e13501a` (2026-01-03).** Before it, cargo's test support names every test's scratch directory `cit/t0` in a fresh process, so the per-test processes of instrumented runs collide; 15 chains 35 first-parent commits apart (525 commits) fit between it and the pinned corpus head.
