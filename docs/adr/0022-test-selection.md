# ADR 0022: Select tests by coverage-derived `Tests` edges, with conservative fallbacks

- **Status:** accepted
- **Date:** 2026-09-24
- **Spec:** §4.2 (Tier 3, "finer selection is OPEN"), §6.5 (impact set), §7.1 (selection mechanisms), §14 item 5 (OPEN); §12 M4 selection safety and efficiency
- **Blocks:** M4 selection efficiency (median ≤ 20% of the suite for write sets ≤ 5)

## Problem

Spec §4.2 starts with crate granularity (`cargo test -p <crate> <filter>`) and leaves finer selection open. Crate granularity cannot meet the M4 efficiency gate on cargo: nearly all of cargo's tests are in one integration test binary (`tests/testsuite`), in the same package as the code under change. Reference edges cannot do it soundly either. `testsuite` tests drive the built `cargo` binary as a subprocess (`cargo_process("build")`), so they never name the library functions they exercise, and a selection by `References`/`Tests` edges would drop tests that fail. Selection safety (zero misses) is the highest-severity requirement (§7.1).

## Options

1. **Crate granularity (§4.2 as written).** `cargo test -p` for every package whose code, or whose dependencies' code, is in the impact set. This is sound, and it can never pass the efficiency gate on cargo.
2. **Static edges only.** Select test functions whose `Tests`/`References` closure reaches the impact set, and run them by exact name. This is efficient, and unsound for subprocess-driven tests, macro-generated tests, and trait dispatch.
3. **Coverage-derived edges, with static edges and fallbacks.** A coverage run (`cargo llvm-cov`, which collects profiles from child processes through `LLVM_PROFILE_FILE`) records, per test, the definitions it executed. Those are stored as `Tests(t, def)` edges keyed by NodeId, so they carry across snapshots through identity (ADRs 0017–0020). Selection for a change:
   - every test whose recorded coverage, or static `Tests`/`References` edges, reaches the impact set;
   - every test the change adds or edits;
   - **fallback to whole packages**, for the package and its reverse dependencies, when the change touches anything coverage cannot attribute: `build.rs`, `Cargo.toml`, `Cargo.lock`, file-level glue (`use`, `mod`, attributes), `macro_rules!` definitions, `static`/`const` initializers, trait impls that were born or died (dynamic dispatch can pick them up without a covered call site), or a definition with no coverage record;
   - **fallback to everything** when there is no coverage record for the base snapshot's toolchain, or the impact set exceeds the policy threshold (§7.1 item 4).

   Coverage records are `Evidence { kind: Custom("coverage") }` with the per-test edge list as the log object. They are refreshed on a schedule or when fallback rates rise, not on every landing.

## Decision

Option 3. Test selection runs individual tests (`--exact` per test binary) chosen from coverage-derived `Tests` edges plus static edges, with package-level fallback for changes coverage cannot attribute and full-suite fallback without a coverage record. Static-only selection is never used on its own.

## Consequences

- **`cargo-llvm-cov`** becomes a toolchain requirement for selection, recorded in `Toolchain`. Without it, verification runs the full affected packages (option 1). It is safe, just slower.
- **Coverage runs cost one instrumented full suite per refresh.** The M4 harness measures how stale coverage may get before fallback rates erase the gains.
- **Fallback triggers are part of the safety argument, not tuning.** Removing one needs evidence from the ADR 0023 harness and a new ADR.
- **Selection accuracy is tracked** (§13 "selection safety"). A miss is a P0 bug in the fallback rules.
