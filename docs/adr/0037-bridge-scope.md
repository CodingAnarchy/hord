# ADR 0037: A narrow `bridge` scope submits pull requests on behalf of their git authors

- **Status:** accepted
- **Date:** 2026-09-26
- **Spec:** §9 (Sync), §10.5.4 (provenance set by the server from the token; scopes); amends ADR 0032's scope list; ADR 0036
- **Blocks:** `hord git sync` against an authenticated `hord serve`

## Problem

ADR 0036 makes a pull request's git author the actor of its proposal. §10.5.4 and the server's ingest check require a submitted change to be authored by the token's actor, and signed with a key bound to that actor. The bridge holds one token, and git authors have neither a token nor a key on the server, so every pull request would be refused.

## Options

1. **A narrow `bridge` scope** that may submit on behalf of git authors, with the bridge recorded as the voucher.
2. **The bridge is the actor,** and the git author is recorded only in the intent.
3. **The bridge runs only inside `hord serve`,** where no token is involved.
4. **Server accounts and keys for every git author.**

## Decision

Option 1.

- **What the scope allows:** a token with the new `bridge` scope may submit an unsigned change whose actor is `Human`, only when its intent carries an `IntentRef::GitCommit` for the pull request's head.
- **What the server records:** the bridge's key id, as the party that vouched for the change, on the `Submitted` event and in the change's provenance trace.
- **What the scope does not allow:** submitting as an agent, submitting without a git ref, reviewing, or arbitrating.
- **Divergence checks:** `RecordBridgeCheck` requires `bridge`.

## Consequences

- `Hord-Actor` on `main` names the human who wrote the change, as ADR 0036 intends. The trace shows which bridge vouched for it.
- The server still decides who may claim what. The claim is narrow and auditable: `hord audit` counts bridge-vouched changes separately from signed ones.
- Minting a `bridge` token is an admin action (`hord token mint --scope bridge`).
