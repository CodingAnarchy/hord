//! `hord key show` and `hord key verify <object> [--key <key-id>]` (spec
//! §10.5.4): this user's key id, and checking a signed change or evidence
//! (such as a review) against a public key. Exits 1 when it does not
//! verify.

use std::process;

use anyhow::{Context, Result, anyhow, bail};
use hord_api::{RepoBackend, proto, wire};
use hord_core::sign::{self, PublicKey};
use hord_core::{ChangeRecord, Evidence, ObjectId, Signature};

use crate::session::{Session, Target};
use crate::txn::{self, block_on};
use crate::{identity, output};

pub fn run_show(json: bool, name: Option<String>) -> Result<()> {
    let name = name.unwrap_or_else(|| txn::actor().id().to_owned());
    let path = identity::key_path(&name)?;
    let key = identity::load_or_create_key(&path)?;
    let key_id = key.public().key_id();
    if json {
        output::print_json(&serde_json::json!({
            "keyId": key_id,
            "keyFile": path.display().to_string(),
        }))
    } else {
        println!("{key_id} ({})", path.display());
        Ok(())
    }
}

/// A signed object.
enum Signed {
    Change(Box<ChangeRecord>),
    Evidence(Box<Evidence>),
}

impl Signed {
    fn kind(&self) -> &'static str {
        match self {
            Self::Change(_) => "change",
            Self::Evidence(_) => "evidence",
        }
    }

    fn signature(&self) -> Option<&Signature> {
        match self {
            Self::Change(c) => c.signature.as_ref(),
            Self::Evidence(e) => e.signature.as_ref(),
        }
    }

    fn verify(&self, key: &PublicKey) -> Result<(), sign::SignError> {
        match self {
            Self::Change(c) => sign::verify_change(c, key),
            Self::Evidence(e) => sign::verify_evidence(e, key),
        }
    }
}

fn load(backend: &dyn RepoBackend, id: ObjectId) -> Result<Signed> {
    let reply = block_on(backend.get_objects(proto::GetObjectsRequest {
        ids: vec![wire::id(id)],
    }))
    .with_context(|| format!("load {}", id.to_hex()))?;
    let Some(object) = reply.objects.first() else {
        bail!("no object {}", id.to_hex());
    };
    if let Ok(evidence) = hord_encoding::decode::<Evidence>(&object.cbor) {
        return Ok(Signed::Evidence(Box::new(evidence)));
    }
    if let Ok(record) = hord_encoding::decode::<ChangeRecord>(&object.cbor) {
        return Ok(Signed::Change(Box::new(record)));
    }
    bail!("{} is neither a change nor evidence", id.to_hex())
}

pub fn run_verify(json: bool, target: &Target, object: String, key: Option<String>) -> Result<()> {
    let id: ObjectId = object
        .parse()
        .map_err(|_| anyhow!("{object:?} is not an object id (64 hex digits)"))?;
    let session = Session::open(target)?;
    let signed = load(session.backend().as_ref(), id)?;
    // Without --key, the key the signature names: that proves only that
    // it signed, so name the expected signer's key to check who.
    let key_id = match (key, signed.signature()) {
        (Some(key), _) => key,
        (None, Some(signature)) => signature.key_id.clone(),
        (None, None) => String::new(),
    };
    let outcome = if key_id.is_empty() {
        Err(sign::SignError::Unsigned)
    } else {
        PublicKey::from_key_id(&key_id).and_then(|key| signed.verify(&key))
    };
    let actor = match &session {
        Session::Remote { remote, .. } if !key_id.is_empty() => {
            block_on(remote.auth().get_key(&key_id))
                .ok()
                .and_then(|k| k.actor)
        }
        _ => None,
    };
    let result = proto::KeyVerifyResult {
        object: wire::id(id),
        kind: signed.kind().to_owned(),
        key_id,
        verified: outcome.is_ok(),
        reason: outcome.err().map(|e| e.to_string()),
        actor,
    };
    if json {
        output::print_json(&result)?;
    } else if result.verified {
        let who = result
            .actor
            .as_ref()
            .map(|a| format!(" ({})", wire::actor_id(a)))
            .unwrap_or_default();
        println!(
            "verified: {} {} signed by {}{who}",
            result.kind,
            txn::short(&result.object),
            result.key_id
        );
    } else {
        println!(
            "not verified: {} {}: {}",
            result.kind,
            txn::short(&result.object),
            result.reason.as_deref().unwrap_or("unknown")
        );
    }
    if !result.verified {
        process::exit(1);
    }
    Ok(())
}
