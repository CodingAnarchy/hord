//! `hord review <change> --as <kind> --approve|--reject -m <msg>` (spec
//! §7.2, §10.2, ADR 0026): sign `Evidence { kind: Review }` against the
//! change's exact result snapshot and attach it.
//!
//! The qualifier is `--as` (ADR 0026), so it meets `review:<kind>`
//! requirements. Against a remote with a stored token, the reviewer is the
//! logged-in actor and the server allows it only with the `review:<kind>`
//! scope; otherwise the reviewer is this process's actor with its key in
//! `~/.hord/keys/`. A change parked for missing evidence is submitted again
//! by the server once the review is attached. `hord key verify <evidence>`
//! checks the signature.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hord_api::auth::Scope;
use hord_api::{proto, wire};
use hord_core::sign;
use hord_core::{Evidence, EvidenceKind, EvidenceResult, Timestamp};

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on};

pub struct Review {
    pub change: String,
    pub kind: String,
    pub approve: bool,
    pub message: String,
}

pub fn run(json: bool, target: &Target, review: Review) -> Result<()> {
    // The qualifier must be one a `review:<kind>` scope can name.
    format!("review:{}", review.kind)
        .parse::<Scope>()
        .with_context(|| format!("--as {:?}", review.kind))?;
    let change = txn::parse_change(&review.change)?;
    let session = Session::open(target)?;
    let backend = session.backend();
    let record = txn::backend_change(backend.as_ref(), change)?;
    let signer = session.signer()?;
    let verdict = if review.approve { "approve" } else { "reject" };
    let mut evidence = Evidence {
        kind: EvidenceKind::Review,
        qualifier: Some(review.kind.clone()),
        snapshot: record.result,
        toolchain: record.provenance.toolchain,
        command: format!(
            "hord review --as {} --{verdict} -m {:?}",
            review.kind, review.message
        ),
        scope: None,
        result: if review.approve {
            EvidenceResult::Pass
        } else {
            EvidenceResult::Fail {
                summary: review.message.clone(),
            }
        },
        log: None,
        cost_ms: 0,
        produced_by: signer.actor.clone(),
        produced_at: now(),
        signature: None,
    };
    sign::sign_evidence(&mut evidence, &signer.key)?;
    let reply = block_on(backend.attach_evidence(proto::AttachEvidenceRequest {
        change: wire::id(change),
        evidence: hord_encoding::encode(&evidence)?,
    }))
    .context("attach the review")?;
    let result = proto::ReviewResult {
        change: wire::id(change),
        snapshot: wire::id(record.result),
        evidence: reply.evidence,
        qualifier: review.kind,
        approved: review.approve,
        key_id: signer.key.public().key_id(),
    };
    if json {
        output::print_json(&result)
    } else {
        println!(
            "{} review:{} {} ({})",
            if result.approved {
                "approved"
            } else {
                "rejected"
            },
            result.qualifier,
            txn::short(&result.change),
            result.evidence
        );
        Ok(())
    }
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    Timestamp::from_millis(ms)
}
