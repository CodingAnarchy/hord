# ADR 0026: The policy is a tracked root file read from head, and evidence carries a qualifier

- **Status:** accepted
- **Date:** 2026-09-24
- **Spec:** §7.2 (policy location and requirements), §3.6 (`Evidence`), §6.3–6.4 (defaults), §10.2 (`review --as`); supersedes §7.2's path `.hord/policy.toml`
- **Blocks:** M4 policy enforcement at landing

## Problem

§7.2 says the policy is "stored in the repository at `.hord/policy.toml` and versioned like everything else". But `.hord/` is the local store directory, and snapshots and directory workspaces leave it out (ADR 0016 follow-up), so the two halves of the sentence conflict. It is also unstated which version of the policy judges a change: if a change's own result supplies the policy, a change can weaken the rules it is judged by. Separately, requirements such as `test:selected`, `review:human`, and `bench:no-regression` carry qualifiers that `EvidenceKind` has no place for.

## Options

The implementing slice listed these in `docs/adr/_open-policy.md`:

1. `.hord/policy.toml` in the store, unversioned.
2. Carve `.hord/policy.toml` out of the store exclusion.
3. A tracked file at the repository root.
4. A `Policy` object behind a named ref.

For qualifiers, two options: an optional field on `Evidence`, or encoding qualified kinds as `EvidenceKind::Custom("kind:qual")`.

## Decision

- **Location.** The policy is the tracked file `.hord-policy.toml` at the repository root: versioned, diffable, and exported to git. A change is judged by the policy in its **landing base (head)**, never by its own result. A change that edits `.hord-policy.toml` is judged by head's policy like any other.
- **Qualifiers.** `Evidence` gains `qualifier: Option<String>`, omitted from the canonical encoding when `None`, so existing evidence ids are unchanged.
  - A requirement `kind:qual` is met by `Pass` evidence of that kind with that qualifier.
  - A bare `kind` is met by any qualifier.
  - `test:full` also satisfies `test:selected`, because a full run is a superset. The reverse does not hold.
  - Review evidence takes its qualifier from `hord review --as <kind>`.
- **`[land]` defaults.** Every `[land]` key is optional:
  - `require = []`;
  - `strict_reads = false` (§6.3);
  - `max_write_set` unset (no limit);
  - `max_replay_attempts = 2` (§6.4).

  Unknown keys are errors. A write set larger than `max_write_set` requires `Pass` evidence of kind `review` with any qualifier.
- **Rule predicates.** Every predicate in a rule's `when` must hold, and the definition predicates (`touches_kind`, `touches_visibility`, `paths`) must hold on the same touched definition. `touches_visibility` compares the written modifier exactly.

## Consequences

- **No policy file in head means no requirements.** The lander applies only `[land]` defaults, and the conflict check and verification still run.
- **Adapters report two new facts** for a changed definition: the node kinds inside it (for `touches_kind`) and its visibility (for `touches_visibility`). This is a `LangAdapter` method with an empty default, implemented for Rust.
- **Git export** writes `.hord-policy.toml` like any other file.
- Reading policy from anywhere other than head, or changing the qualifier rules, needs a new ADR.

## Amendments (2026-09-24, from implementation)

- **A policy that does not parse never lands.** Head's policy judges every change, and an unreadable head policy parks everything, including the change that would fix it. So the lander parses the `.hord-policy.toml` in each change's result, and rejects the change if it does not parse. `propose` and `hord policy check` report the same parse error early. A repository can still reach an unparseable head policy only through bootstrap or git import, and recovery from that is administrative.
