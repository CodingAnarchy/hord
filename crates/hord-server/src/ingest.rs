//! What the server checks when a change or evidence enters the lander or
//! the evidence index (spec §10.5.4): provenance is the token's actor, not
//! what the client reports, and signatures verify with a key bound to that
//! actor. Without auth (a repository's own daemon) there is no token, and a
//! signature that is present must still verify.

use std::sync::Arc;

use hord_api::auth::Scope;
use hord_api::{RepoBackend, proto, wire};
use hord_core::sign::{self, PublicKey, SignError};
use hord_core::{Actor, ChangeRecord, Evidence, EvidenceKind, IntentRef, Signature};
use tonic::Status;

use crate::auth::{AuthStore, Principal, same_actor};

/// Check the change `change` names before it is submitted: with a
/// principal, authored by the token's actor and signed with a key bound to
/// it, or vouched for by a `bridge` token ([`bridge_vouched`], ADR 0037).
/// A change that cannot be read is left for the backend to report.
pub(crate) async fn check_submit(
    backend: &dyn RepoBackend,
    auth: Option<Arc<AuthStore>>,
    principal: Option<Principal>,
    change: &str,
) -> Result<(), Status> {
    let Ok(id) = wire::object_id("change", change) else {
        return Ok(());
    };
    let Ok(reply) = backend
        .get_objects(proto::GetObjectsRequest {
            ids: vec![wire::id(id)],
        })
        .await
    else {
        return Ok(());
    };
    let Some(object) = reply.objects.first() else {
        return Ok(());
    };
    let record: ChangeRecord = hord_encoding::decode(&object.cbor)
        .map_err(|err| Status::invalid_argument(format!("{change} is not a change: {err}")))?;
    // Looking a key up may read the auth file.
    blocking(move || {
        let verify = |key: &PublicKey| sign::verify_change(&record, key);
        match (auth, principal) {
            (Some(auth), Some(principal)) => {
                // A token that may propose submits its own changes; a
                // voucher, or a token that may only bridge, is the bridge's.
                if record.provenance.voucher.is_some() || !principal.has(&Scope::Propose) {
                    return bridge_vouched(&auth, &principal, &record);
                }
                claimed_by(&record.provenance.actor, &principal, "a change authored by")?;
                signed_by(&auth, &principal, record.signature.as_ref(), verify)
            }
            _ => self_consistent(record.signature.as_ref(), verify),
        }
    })
    .await
}

/// Check an arbiter's decision before the lander acts on it (spec §6.4
/// rung 3, §10.5.4): with a principal, the arbiter is the token's actor
/// (set here when the request leaves it out) and the decision is signed
/// with a key bound to it. Without auth, a signature that is present must
/// verify. A malformed request is left for the backend to report.
pub(crate) async fn check_arbitration(
    auth: Option<Arc<AuthStore>>,
    principal: Option<Principal>,
    request: &mut proto::ArbitrateRequest,
) -> Result<(), Status> {
    if let (Some(_), Some(principal)) = (&auth, &principal) {
        match &request.arbiter {
            Some(claimed) => {
                let claimed = wire::actor_from("arbiter", claimed)
                    .map_err(|err| Status::invalid_argument(err.to_string()))?;
                claimed_by(&claimed, principal, "a decision by")?;
            }
            None => request.arbiter = Some(wire::actor(&principal.actor)),
        }
    }
    let Ok((change, action, arbiter)) = hord_txn::arbitrate_request(request) else {
        return Ok(());
    };
    blocking(move || {
        let message = hord_txn::arbitration_message(change, &action)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        let verify = |key: &PublicKey| {
            key.verify(
                hord_txn::ARBITRATION_DOMAIN,
                message.as_bytes(),
                arbiter.signature.as_ref().ok_or(SignError::Unsigned)?,
            )
        };
        match (auth, principal) {
            (Some(auth), Some(principal)) => {
                signed_by(&auth, &principal, arbiter.signature.as_ref(), verify)
            }
            _ => self_consistent(arbiter.signature.as_ref(), verify),
        }
    })
    .await
}

/// [`check_evidence`] on the blocking pool (it may read the auth file).
pub(crate) async fn check_evidence_async(
    auth: Option<Arc<AuthStore>>,
    principal: Option<Principal>,
    evidence: Evidence,
) -> Result<(), Status> {
    blocking(move || check_evidence(auth.as_deref(), principal.as_ref(), &evidence)).await
}

async fn blocking(f: impl FnOnce() -> Result<(), Status> + Send + 'static) -> Result<(), Status> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| Status::internal(err.to_string()))?
}

/// Check evidence before it is attached: with a principal, the token may
/// sign this kind (`review:<kind>` for a review, else `propose`), it is
/// produced by the token's actor, and signed with a key bound to it.
/// Undecodable evidence is left for the backend to report.
fn check_evidence(
    auth: Option<&AuthStore>,
    principal: Option<&Principal>,
    evidence: &Evidence,
) -> Result<(), Status> {
    let verify = |key: &PublicKey| sign::verify_evidence(evidence, key);
    let (Some(auth), Some(principal)) = (auth, principal) else {
        return self_consistent(evidence.signature.as_ref(), verify);
    };
    let needed = match (&evidence.kind, &evidence.qualifier) {
        (EvidenceKind::Review, Some(kind)) => Scope::Review(kind.clone()),
        (EvidenceKind::Review, None) => {
            return Err(Status::invalid_argument(
                "review evidence names its reviewer kind (`hord review --as <kind>`)",
            ));
        }
        (EvidenceKind::Rebase { .. }, _) => {
            return Err(Status::permission_denied(
                "rebase attestations are the lander's own",
            ));
        }
        _ => Scope::Propose,
    };
    if !principal.has(&needed) {
        return Err(Status::permission_denied(format!(
            "attaching this evidence requires scope {needed}; the token of {} lacks it",
            principal.actor.id()
        )));
    }
    claimed_by(&evidence.produced_by, principal, "evidence produced by")?;
    signed_by(auth, principal, evidence.signature.as_ref(), verify)
}

/// What a `bridge` token may submit (ADR 0037): a pull request's
/// unsigned change whose actor is a human (its git author), whose intent
/// names a git commit (the pull request's head), and whose provenance names
/// as voucher a key bound to the token's actor. Nothing else.
fn bridge_vouched(
    auth: &AuthStore,
    principal: &Principal,
    record: &ChangeRecord,
) -> Result<(), Status> {
    let who = describe(&principal.actor);
    if !principal.has(&Scope::Bridge) {
        return Err(Status::permission_denied(format!(
            "only a bridge token may submit a change vouched for on another's behalf; \
             the token of {who} lacks scope bridge"
        )));
    }
    let provenance = &record.provenance;
    if !matches!(provenance.actor, Actor::Human { .. }) {
        return Err(Status::permission_denied(format!(
            "the bridge token of {who} may submit only a git author's change, not {}",
            describe(&provenance.actor)
        )));
    }
    if record.signature.is_some() {
        return Err(Status::permission_denied(
            "a change the bridge vouches for is unsigned; a signed change is submitted by its author",
        ));
    }
    if !record
        .intent
        .refs
        .iter()
        .any(|r| matches!(r, IntentRef::GitCommit { .. }))
    {
        return Err(Status::permission_denied(
            "a change the bridge vouches for names its pull request's head (a git commit ref)",
        ));
    }
    let Some(voucher) = &provenance.voucher else {
        return Err(Status::permission_denied(
            "a change the bridge submits names the bridge's key as its voucher",
        ));
    };
    let bound = auth
        .key_actor(voucher)
        .map_err(|err| Status::internal(err.to_string()))?;
    match bound {
        Some(actor) if same_actor(&actor, &principal.actor) => Ok(()),
        Some(actor) => Err(Status::permission_denied(format!(
            "voucher key {voucher} is bound to {}, not {who}",
            describe(&actor)
        ))),
        None => Err(Status::permission_denied(format!(
            "voucher key {voucher} is not bound to any actor on this server"
        ))),
    }
}

/// The claimed actor must be the token's.
fn claimed_by(claimed: &Actor, principal: &Principal, what: &str) -> Result<(), Status> {
    if same_actor(claimed, &principal.actor) {
        Ok(())
    } else {
        Err(Status::permission_denied(format!(
            "the token of {} cannot submit {what} {}",
            describe(&principal.actor),
            describe(claimed)
        )))
    }
}

/// A signature by a key bound to the token's actor, that verifies.
fn signed_by(
    auth: &AuthStore,
    principal: &Principal,
    signature: Option<&Signature>,
    verify: impl FnOnce(&PublicKey) -> Result<(), SignError>,
) -> Result<(), Status> {
    let signature = signature.ok_or_else(|| {
        Status::permission_denied("unsigned: this server requires the actor's signature")
    })?;
    let bound = auth
        .key_actor(&signature.key_id)
        .map_err(|err| Status::internal(err.to_string()))?;
    match bound {
        Some(actor) if same_actor(&actor, &principal.actor) => {}
        Some(actor) => {
            return Err(Status::permission_denied(format!(
                "key {} is bound to {}, not {}",
                signature.key_id,
                describe(&actor),
                describe(&principal.actor)
            )));
        }
        None => {
            return Err(Status::permission_denied(format!(
                "key {} is not bound to any actor on this server",
                signature.key_id
            )));
        }
    }
    let key = sign::signer(signature).map_err(|err| Status::invalid_argument(err.to_string()))?;
    verify(&key).map_err(|err| Status::invalid_argument(format!("bad signature: {err}")))
}

/// Without auth: a signature, if present, verifies with the key it names.
fn self_consistent(
    signature: Option<&Signature>,
    verify: impl FnOnce(&PublicKey) -> Result<(), SignError>,
) -> Result<(), Status> {
    let Some(signature) = signature else {
        return Ok(());
    };
    let key = sign::signer(signature).map_err(|err| Status::invalid_argument(err.to_string()))?;
    verify(&key).map_err(|err| Status::invalid_argument(format!("bad signature: {err}")))
}

fn describe(actor: &Actor) -> String {
    match actor {
        Actor::Human { id } => format!("human {id}"),
        Actor::Agent {
            id, model, harness, ..
        } => format!("agent {id} ({model}, {harness})"),
    }
}
