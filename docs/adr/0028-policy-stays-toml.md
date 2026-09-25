# ADR 0028: Policy stays declarative TOML through M5

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §7.2 (OPEN: a richer policy language if TOML proves insufficient by M5), §14 OPEN #6
- **Blocks:** M5 (review round-trip, replay attempts and budgets in policy)

## Problem

§14 #6 asks whether policy needs a richer language than TOML (Rhai, Starlark, or WASM), to be decided by M5. M5 adds the policy inputs that exercise the escalation ladder: `review:human` blocking a landing (the review round-trip), `max_replay_attempts`, and a replay cost budget. If TOML cannot express these, M5 needs an embedded interpreter. If it can, an interpreter adds a sandbox, a new attack surface on a hosted server, and nondeterminism risk for no benefit yet.

## Options

1. **TOML only, extended by keys.** Add `[replay] budget` (tokens, wall time, cost) and keep `[land]` and `[[rule]]` as ADR 0026 defines them. Every M5 acceptance criterion is a requirement on evidence kinds or a numeric limit, which TOML expresses. Unknown keys stay errors, so a policy written for a later schema fails loudly instead of being ignored.
2. **TOML plus a Rhai escape hatch.** A rule may carry `script = "..."` evaluated against the change. Rhai is pure Rust and sandboxable, but scripts can loop, read wall time, or depend on iteration order unless restricted, and every hosted repository then runs user code in the lander.
3. **Starlark (`starlark-rust`).** Deterministic by design and hermetic. It is a heavier dependency with a second configuration language to document, and nothing in M5 needs it.
4. **WASM policies.** Most general and most isolated. It needs a host ABI, fuel metering, and a toolchain for policy authors. That is out of proportion for M5.

## Decision

Option 1. Policy stays declarative TOML through M5. A later ADR revisits this when a real rule cannot be written as evidence requirements plus predicates, and Starlark (option 3) is the first candidate then.

## Consequences

- M5 adds `[replay]` to `.hord-policy.toml`: `budget` (the §6.6 `Budget` fields) alongside the existing `[land] max_replay_attempts`. Unknown keys remain errors.
- The lander stays deterministic: judging a change runs no user code.
- The trigger for revisiting is concrete: a rule that needs a computation over the change (not a predicate on touched definitions, actor, paths, or write-set size).
