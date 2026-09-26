# Open questions from the M6 quality review

Found while reviewing `m6`. Each one needs a decision (an ADR or an amendment) before code changes, so none was implemented. Recommendations are the reviewer's.

## 1. The lander's `Rebase` attestation is unsigned, though ADR 0018 says it is signed with the lander's key

**Where.** `crates/hord-txn/src/lander.rs`, `rebase_attestation`: `signature: None`, and the doc comment says "Unsigned until M5". ADR 0018 says "Until M5 adds keys, the attestation is unsigned but present". M5 added author and arbiter keys, but no lander key. `hord audit` treats attestations as not being checks, and never verifies them.

**Why it is not a straightforward change.** The attestation is deliberately a pure function of the two records: its `produced_at` is the submitted record's `created_at`, so "the same landing gives the same landed id anywhere". The landed record's `evidence` lists the attestation's id, and that id covers its signature. So:

- A signature by a per-repository key makes the landed `ChangeId` depend on which repository landed the change. That breaks that determinism (AGENTS.md: "same inputs → same ObjectIds"). Ed25519 is deterministic for one key, so the same repository always gives the same id, but a clone or a restored host with a new key does not.
- The key's home is also open: beside the store (`.hord/`), or with the server's secrets (`/etc/hord`, HORD_HOME). Both a daemon and `hord serve` run landers on the same store.
- Rotation is open too. Old attestations must stay verifiable, so either the key is never rotated, or old key ids stay listed.
- How audit trusts the key is open. Today key trust is "bound to an actor in the auth file", and the lander is not an actor anyone logs in as.

**Options.**

1. **Sign outside the id.** Keep the attestation, and the landed id, deterministic and unsigned. Record the lander's signature over the landed id in the `Landed` event instead, or in a separate object that references the landed id. Audit verifies it against a lander key id listed in the auth file (`[[lander]] id = "ed25519:…"`).
2. **Sign the attestation with a per-repository key** in `.hord/lander.key`, created with the store and bound in the auth file as `Actor::Agent { id: "hord-lander" }`. Accept that landed ids are per-repository. Needs an amendment to the determinism rule, and to ADR 0018's "same landed id anywhere".
3. **Leave it unsigned** and amend ADR 0018 to say so. The landed record is trusted through `rebased_from` plus the lander being the only writer of the log. That is what `hord audit`'s `Landed`-event check measures.

**Recommendation.** Option 1. It keeps ids deterministic. It puts the lander's claim where audit already reads (the event log), and it makes "nothing landed outside the lander" cryptographic rather than a property of who can write the store. Rotation becomes listing more than one lander key id.

## 2. `hord audit` judges evidence and policy as of the audit, not as of the landing

**Where.** `crates/hord-server/src/audit.rs`: `landed_evidence` reads `evidence_at(result)` now, and `Repo::judge_landed` evaluates policy over the current evidence index.

**What goes wrong.**

- Passing evidence attached to a landed snapshot after it landed makes a change that landed with none audit clean.
- A failing check attached later, such as a flaky nightly run on an old snapshot, fails a window that passed.
- A revert whose result equals an earlier snapshot shares that snapshot's evidence.

**Options.**

1. Count only evidence whose `EvidenceAttached` event is at or before the change's `Landed` event, plus the ids `Landed.evidence` lists.
2. Record, on `Landed`, exactly the evidence ids the lander counted. The audit then judges those alone.
3. Keep "as of now", and document it as intended (evidence accrues).

**Recommendation.** Option 2. The lander already knows the set it counted, so the audit then re-judges the decision the lander actually made. Option 1 is the fallback for history recorded before the change.

## 3. Revoking a key fails every signature it made in the audit window

**Where.** `audit.rs` `bound()` checks every signature against the auth file as it is now. `docs/hosting.md` says to revoke by deleting the `[[key]]` entry, and the server now reloads the file within a second.

**What goes wrong.** After a key leaks and is revoked, each change and review it signed legitimately earlier fails "not bound to any actor". A thirty-day M6 window then cannot pass.

**Options.**

1. Keep revoked keys with `revoked_at`. A signature verifies if it was made before the revocation (by the `Submitted` or `EvidenceAttached` event time).
2. Record the binding the server verified at ingest (key id and actor) on `Submitted`, `EvidenceAttached` and `Arbitrated`. The audit trusts the event log for history, and checks the auth file only for events without it.

**Recommendation.** Option 1. It is a small change to the auth file format, it keeps the auth file the one source of trust, and it gives revocation a time.

## 4. The audit cannot re-verify an arbitration's signature

**Where.** `Arbitrated` (hord.proto) carries `key_id` and `signature`, but not the action or note the signature covers (`hord_txn::arbitration_message`). The audit can only check that the key is bound, and accepts any signature bytes.

**Options.**

1. Add the signed action and note to `Arbitrated`, and verify the signature in the audit.
2. Keep trusting ingest, which verified the signature. Rename the criterion to say "bound key".

**Recommendation.** Option 1. It is additive in the proto.

Related: `BridgeChecked` events do not record who recorded them. On `hord serve`'s local endpoint, which takes no tokens, any process of the `hord` OS user can record passing checks. Recording the recorder's key id on the event (the bridge's voucher key) would let the audit count only checks by a bound key.
