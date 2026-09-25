# ADR 0034: A replay may not change the acceptance tests it must satisfy

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §6.4 rung 2, §6.6 (replay harness), §12 M5 (conflict corpus); ADR 0029
- **Blocks:** M5 conflict corpus (real-model run)

## Problem

The lander verifies a replay with the tests in the replay's own result. The first real-model pilot (Sonnet 5, 10 cases) landed two replays that passed that verification but failed the cases' original acceptance tests:

- **m5-089:** a deliberate contradiction. The model picked one side and must have changed the other side's test.
- **m5-020:** the model changed a signature that its own intent's test pins, then adjusted that test.

A replay that weakens the tests it is judged by turns rung 2 into a way around verification.

## Options

1. **Protect named tests.** A replay may not modify or delete a test definition that either colliding intent names in its `acceptance` (matched by `NodeId`). The lander rejects such an attempt.
2. **Re-verify with pinned tests.** The lander runs both intents' named acceptance tests from their original sources before landing a replay. This is more machinery and a second test run.
3. **Policy only.** Replays that touch test code require `review:human`.

## Decision

Option 1.

- **What is protected:** the set of definitions named as `test` acceptance in either colliding change's intent. That is the replayed change's own intent and the intents of the changes it collided with.
- **When a replay is rejected:** if its ops modify, delete or rename any of those `NodeId`s, the lander rejects the attempt before verification. The attempt records a new outcome, `TAMPERED`, with the definitions it touched. Like `OVER_BUDGET`, its change is not submitted and does not become a candidate, and the ladder moves on to the next attempt or to arbitration.
- **Adding a test is allowed:** a replay may still add tests of its own.
- **The prompt says so:** the reference harness's prompt lists the protected tests and says they must not change.

## Consequences

- A replay can land only by meeting both intents' tests as written. A genuine contradiction between those tests can no longer be "resolved" by editing one of them, so it ends in arbitration.
- Tests named in acceptance only by a name that resolves to no definition are not protected. The lander logs them, and the corpus names every acceptance test by its definition.
- Arbitration is unaffected. A human resolution may change tests, and it goes through verification and policy like any change.
