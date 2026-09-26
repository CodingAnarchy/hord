# ADR 0038: What `hord audit` trusts, and how

- **Status:** accepted
- **Date:** 2026-09-26
- **Spec:** §3.6, §10.5.4, §12 M6 (acceptance by audit over a 30-day window); amends ADR 0018 (the lander's attestation), ADR 0032 (key revocation), ADR 0036 and ADR 0037 (bridge checks)
- **Blocks:** trusting `hord audit`'s verdict on the M6 window

## Problem

The M6 quality review found four ways the audit's verdict can be wrong or unreachable:

1. **The lander signs nothing.** ADR 0018 says its `Rebase` attestation is signed with the lander's key, but the lander has no key. Signing the attestation would also make landed ids depend on which repository's key landed them, which breaks "same inputs, same ids".
2. **The audit judges as of now.** It reads evidence and policy as they stand at audit time, so evidence attached after a landing can clear a bad landing or fail a good one.
3. **Revocation reaches back.** Revoking a key fails every signature it made earlier, so no window can pass after a revocation.
4. **Some signatures can't be re-checked.** `Arbitrated` doesn't carry what the arbiter signed, so the audit can check only that the key is bound. `BridgeChecked` doesn't record who recorded it.

## Decision

1. **The lander signs outside the id.**
   - Attestations and landed ids stay deterministic and unsigned.
   - Each repository has a lander key. Its public id is listed in the auth file as `[[lander]] id = "ed25519:…"`, and old ids stay listed after a rotation.
   - The lander signs each landed id in its `Landed` event.
   - The audit verifies that every landing in the window carries a valid lander signature by a listed key, so "nothing landed outside the lander" is cryptographic.
2. **Record the evidence that was counted.** `Landed` lists exactly the evidence ids the lander counted when it judged the change, and the audit re-judges that decision alone. For history without the list, the audit counts only evidence whose `EvidenceAttached` came before the `Landed` event, plus `Landed.evidence`, and says it did so.
3. **Revoke with a time.** A revoked key stays in the auth file with `revoked_at`. A signature verifies if its event (`Submitted`, `EvidenceAttached` or `Arbitrated`) is before that time. `hord token`, `hord key` and the runbook revoke this way, never by deletion.
4. **Carry what was signed.**
   - `Arbitrated` carries the signed action and note, and the audit verifies the signature itself.
   - `BridgeChecked` carries the recorder's key id, and the audit counts only checks recorded by a key bound to a `bridge` token.

## Consequences

- The M6 window can pass after a key is revoked, and it can't be cleared by evidence attached later.
- Nothing that isn't in the log can pass an audit: every landing, arbitration and bridge check carries a verifiable signature.
- All proto changes are additive. Events recorded earlier are audited with the fallbacks above, and the audit report says which fallback applied.

## Amendment (2026-09-26): bridge checks are signed

A recorder key id alone can be borrowed: on `hord serve`'s local endpoint, which takes no tokens, any process of the `hord` OS user could record a passing check naming the bridge's key. So the bridge now signs each `BridgeChecked` with that key (`signature`, domain `hord.bridge-check`, over every other field).

- **Server:** with tokens, it refuses a check whose signature does not verify against the recorder key, or whose recorder is not bound to the token's actor. Without tokens, a signature that is present must verify.
- **Audit:** with an auth file, it counts only checks that are signed by their recorder and whose key belonged to a `bridge` token's actor at the time. An unsigned check, including one recorded before this amendment, is left out and counted in the report's notes.
