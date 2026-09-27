# ADR 0040: M6 is accepted by hord-first development, not a 30-day window

- **Status:** accepted
- **Date:** 2026-09-27
- **Spec:** §12 M6 (acceptance); ADR 0035 (usability check), ADR 0036 (bridge), ADR 0038 (audit)
- **Blocks:** M6 acceptance

## Problem

M6's acceptance asks for thirty consecutive days of development on a team host with zero manual git operations. That measures time spent, not whether hord can carry real development. What matters is whether the agents building hord can move to hord first and keep adding real functionality through the lander, without regressions, with the agentic workflow (propose, verify, replay, arbitrate, review) handling it.

## Options

1. **Keep the 30-day window.**
2. **Hord-first development with a feature programme.** Hord's own agents work only through a hord server run locally. Acceptance is a set of substantive features delivered through that workflow with no regressions, measured by `hord audit` and the acceptance harnesses rather than by elapsed time.
3. **Hord-first with no feature bar.** Switching over is enough. It proves the plumbing, not that hord can carry development.

## Decision

Option 2.

- **Hord-first.**
  - Hord's repository is served by `hord serve` running locally on the developer's machine, with TLS and auth on. The team host (§12 M6) is deferred until it's needed.
  - Every change by an agent working on hord is proposed, verified, submitted and landed through that server. Agents use `hord ws`, `propose`, `submit` and `watch`, never `git commit` or `git push`.
  - `hord git sync` mirrors the log to GitHub's `main`, and humans contribute through pull requests (ADR 0036).
- **The feature programme.** M6 is accepted once at least one substantive feature in each of four areas has landed end to end through the hord-first workflow. The exact features are chosen when the programme starts. The areas:
  - **language coverage and controls**, for example a second Tier 2 adapter (TypeScript or Python);
  - **a new capability**;
  - **a reliability improvement**;
  - **the forge**: the first slice of ADR 0041's issues, discussions and proposals.
- **No regressions.** Throughout the programme:
  - the M0–M5 acceptance harnesses stay green on every landing, which CI runs on the mirror;
  - `hord audit --require-bridge` over the programme's window passes: every landing has intent, provenance and passing evidence, and is signed by the lander; every review and arbitration came through the API; the bridge never diverged;
  - the M4 selection gate and the M5 conflict corpus pass when re-run at the end.
- **What is reported:** the landings, the replays and arbitrations, the human interventions (all through the UI or CLI), the regressions found after landing, and the time from intent to landed.
- The usability check (ADR 0035) stays in M6, run against the local server.

## Consequences

- M6 no longer waits on a calendar or a provisioned host.
- The dogfooding produces real evidence about the agentic workflow: how often replays resolve, what needs a human, and what regresses.
- Features in the programme are real work: they get their own ADRs and specs as usual.
