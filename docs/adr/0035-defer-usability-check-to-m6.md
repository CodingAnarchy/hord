# ADR 0035: The 5-person usability check moves from M5 to M6

- **Status:** accepted
- **Date:** 2026-09-26
- **Spec:** §12 M5 ("Demo": the usability check), §12 M6; ADR 0032 (TLS is M6)
- **Blocks:** nothing; it removes a human study from M5's gate

## Problem

M5's "Demo" criterion has two parts:
- The flight-recorder log plays back on the landing strip end to end. The UI test suite checks this automatically.
- A person unfamiliar with hord can explain from the UI alone why a given change was parked. This is a usability check with 5 participants.

The second part needs recruited participants and a reachable instance. Under ADR 0032, `hord serve` has no TLS until M6, so a hosted demo means a proxy or screen sharing. Every other M5 criterion is met:
- replay resolves 76% of the conflict corpus;
- 24 of 24 parked summaries were rated sufficient to resolve;
- budget enforcement, the arbitration and review round-trips, and the RPC check all pass.

## Options

1. **Keep it in M5.** M5 waits on recruiting and hosting, with no code left to write.
2. **Move it to M6.** It is run on the team-hosted server M6 delivers, with TLS, and with views 4–6. The provenance trace is the view a newcomer would use to explain a parked change.
3. **Drop it.** This loses the only check that a person new to hord can read the UI.

## Decision

Option 2. M5 is accepted without the usability check. It becomes an M6 acceptance criterion, run on the self-hosted server. Playback of the flight recording stays an M5 criterion.

## Consequences

- Spec §12: M5's "Demo" keeps the playback, and M6's "Accept" gains the usability check (5 participants, from the UI alone, against the self-hosted server).
- The check then covers views 1–6, not 1–3.
