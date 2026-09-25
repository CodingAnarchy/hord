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

## Amendments (2026-09-24, from implementation)

Every rule below takes the reading that runs more tests, never fewer.

- **Coverage drift counts as part of the change.** A coverage record is usually older than a change's base. Everything that changed between the coverage snapshot and the snapshot being verified is looked up in coverage, and can trigger fallbacks, exactly like the change's own edits. As coverage ages, efficiency degrades toward the package fallback.
- **Doctests.** Per-test coverage cannot attribute doctests on stable. A selection also runs `cargo test -p P --doc` for every package the change touches, and for their reverse dependencies.
- **Files no adapter parses.** A changed file with no adapter inside a package (docs, fixtures, SVGs) falls back to that package and its reverse dependencies. Tests can read files by computed path at runtime, which cannot be detected, so this stays conservative for M4. `bench/m4-eval` reports how often it fires and what it costs. Narrowing it needs data and a new ADR.
- **"No coverage record" applies to functions.** Coverage attributes code only to functions, so the fallback applies to `function_item`s: those born in the change, and edited ones coverage never instrumented. Type definitions reach tests through the impact set's `References` dependents and the other triggers.
- **The impact bound.** Expansion stops at 2 hops or at the crate boundary, whichever comes first. Both are configurable.
- **Size threshold (§7.1 item 4).** An optional `max_impact` key in `.hord-policy.toml`'s `[land]` table (ADR 0026). Unset, which is the default, means no size fallback.

## Amendments (2026-09-24, after the 50-commit measurement)

The measurement (`bench/m4-eval`, 50 cargo commits, four variants) selected 100% of tests under every variant, with 0 misses. The cause was coverage drift. Counting every change since a shared coverage checkpoint as part of the change meant one `Cargo.lock` or root `Cargo.toml` commit sent every later commit in its checkpoint group to the whole-workspace fallback. Where drift was small, the impact set's dependents, not the edited functions, set the selection size. These amendments replace the first bullet of the earlier amendments ("coverage drift counts as part of the change") and the impact-set rule in Option 3.

- **Coverage is fresh per test.** Every verification run is instrumented, and it refreshes the coverage record of each test it ran. Each test's record names the snapshot it was taken on. Drift matters to a test only if a definition that test executed changed between its record's snapshot and the snapshot being verified; such a test is selected. A test whose executed code did not change cannot have changed behavior through drift.
- **Global triggers are narrow in time.** A change to `Cargo.toml`, `Cargo.lock`, a `build.rs`, or anything else that falls back for a package still runs that package in full for that change, instrumented. That run refreshes those tests, so later changes start from fresh records.
- **Select on the functions the change wrote.** A test is selected when its coverage includes a function the change wrote (edited or deleted). Dynamic coverage makes this sound: a test that never ran the changed code cannot observe it. The impact set's `References` dependents (§6.5) are used only for writes coverage cannot attribute: types, fields, consts, statics, trait items, and declarations. Births, glue, attributes, and the other fallback triggers are unchanged.
- **Narrowed non-Rust fallback.** A changed file with no adapter triggers a package fallback only if it lies inside a build target's source directories, or if its path (by suffix) is named in an `include_str!`, `include_bytes!` or `file!` invocation, or in a string literal, in the package. Otherwise it selects no tests. If the ADR 0023 harness finds a miss caused by this rule, the rule is reverted.
- The ADR 0023 gate is re-run under these rules. Efficiency is measured on commits whose records reflect every earlier landing, as the lander would see them.
- **A record is the union of a test's last three runs (2026-09-24).** Repeated instrumented runs of one test entered different function sets in 7 of 30 cargo tests: retries, timeouts, and other timing-dependent paths. One run's coverage can therefore miss functions another run executes. A test's record is the union of its last three instrumented runs, and drift for that test is measured from the snapshot of the oldest run in the union. This over-approximates, so it is safe, at a small efficiency cost.
- **Coverage runs one test per process.** Tests sharing a process share lazily initialized state (`OnceLock`, harness setup): only the first test's coverage shows the initializer, although later tests depend on what it built. Attributing coverage per test inside a shared process would therefore miss them. Per-test records come from isolated per-test processes, the way a selected test actually runs.
- **Staleness is checked run by run (2026-09-24, supersedes "drift … measured from the snapshot of the oldest run in the union").** A test is stale, and so selected, when for some run r among its last three, a function r executed has changed since r's own snapshot. Coverage selection still uses the union of all three runs' functions. This still catches a nondeterministic path seen only in an older run, since that path is checked against every change since that run. It drops only a function that changed *before* the run that executed it, and that run already exercised the changed code. Measuring the whole union from its oldest run made fallback refreshes age every record two fallbacks back: 26.2% median selection for write sets ≤ 5, against 2.6% for the same selections run by run.
