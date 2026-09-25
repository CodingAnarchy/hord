
## Amendments (2026-09-25, after the first CI gate run)

Gate run 36145004665: 0 misses in 104 faults, but a median selected share of 86.1% for write sets ≤ 5. Most of that came from measurement defects fixed in the harness (chain windows before cargo `89e13501a`, where per-test scratch directories collide under per-test processes, and UI snapshot tests failing only in the CI environment). Two rules are narrowed as well:

- **Staleness ends once the test re-ran on the change (R1; refines "Staleness is checked run by run").** Test t is stale when some run r among its last three executed a function f that changed after r's snapshot, *and no later run of t has a snapshot at or after that change*. Coverage selection still uses the union of the three runs. This keeps the nondeterministic-path case whenever the change came after t's latest run. It drops only the case where t already passed on a snapshot containing the change. A later change to f still selects t through the union.
- **Non-function edits in an integration-test file select that file's tests (R2; narrows the package fallback).** For glue, declaration, initializer and attribute writes in a file under `tests/`, select the tests defined in that file's module subtree, plus the tests covering any function the change wrote, instead of the whole package. A shared helper module keeps the package fallback: a module whose items are referenced from another test module (hord's `References` edges), or a module under a `utils`/`support`-style path that other test modules import. A non-function item in a test module compiles only into that module's tests.

Both are checked by the ADR 0023 harness. A miss caused by either reverts it, as was done for the non-Rust rule.
