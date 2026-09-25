# ADR 0029: One replay per attempt, judged by evidence; alternatives go to arbitration

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §6.4 (rung 2), §6.6 (replay harness), §14 OPEN #8
- **Blocks:** M5 (replay protocol, `hord-replay-ref`, conflict corpus acceptance)

## Problem

A replay re-executes a change's intent on a new base through a model-backed harness (§6.6), so two replays of the same intent can produce different results. §14 #8 asks how to compare two replays, and whether to run N and vote. The lander must decide whether a replay's result is acceptable, and the conflict corpus must be graded reproducibly (≥ 60% resolved by replay).

## Options

1. **One replay per attempt, judged by evidence.** Each attempt produces one `ChangeRecord` (with `parent_intent`) that goes through the normal lander path: conflict check, verification, and head's policy. A replay is accepted when its evidence passes. No comparison between replays is needed. Nondeterminism appears as attempts that fail and retry, up to `max_replay_attempts`.
2. **N replays and a majority vote.** Run N replays and group them by semantic equivalence. Equivalence means equal ops against the base after identity normalization; equal result `SnapshotId`s are the strict special case. Land the majority if it passes. This costs N times the budget, and model outputs rarely match exactly, so votes usually split.
3. **N replays, first passing wins.** Run N concurrently and land the first one whose evidence passes. It is faster to a result than retries, but costs up to N times the budget and hides disagreement that a reviewer would want to see.
4. **Require deterministic harnesses.** Pin seed and temperature, and compare replays by result `SnapshotId`. Hosted models do not guarantee determinism even so, and it constrains the protocol for every harness.

## Decision

Option 1. A replay is one attempt judged by the same evidence and policy as any change. When the attempts run out, the change goes to arbitration with every attempt's result attached as a candidate. Two replays are compared by their semantic ops against the base, only for display and deduplication in the arbitration workbench, never to decide landing.

## Consequences

- The replay protocol stays as §6.6 defines it: one `ReplayRequest`, one `ReplayResult`. `Budget` is enforced per attempt, and the lander kills a harness that exceeds it.
- The arbitration workbench shows each attempt's diff. Attempts with equal semantic ops collapse into one candidate, and `--pick` can land a candidate directly.
- Conflict-corpus grading counts a case as resolved by replay when any attempt within `max_replay_attempts` lands with its acceptance test passing. Runs record the harness command and model name, so a result can be traced.
- Voting (option 2) can be added later as a policy option without changing the protocol.
