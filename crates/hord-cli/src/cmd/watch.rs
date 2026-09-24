//! `hord watch [--queue] [--change <id>] [--from <cursor>]`: tail the event
//! stream (spec §10.5.3) through the session's backend.
//!
//! Text prints one line per event; `--json` prints each `EventEnvelope` as
//! one line of JSON. A dropped stream (for example a daemon that went idle)
//! is resumed from the last cursor seen. With `--change`, only that
//! change's events are shown, and the command exits once it lands, parks,
//! or is rejected.

use std::time::Duration;

use anyhow::Result;
use hord_api::proto;
use hord_api::proto::event::Kind;
use tokio_stream::StreamExt;

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{self, block_on, short};

/// The change ids an event is about (submitted and landed).
fn changes(kind: &Kind) -> Vec<&str> {
    match kind {
        Kind::Submitted(e) => vec![&e.change],
        Kind::ConflictCheck(e) => vec![&e.change],
        Kind::Verifying(e) => vec![&e.change],
        Kind::EvidenceAttached(e) => vec![&e.change],
        Kind::Replaying(e) => vec![&e.change],
        Kind::Parked(e) => vec![&e.change],
        Kind::Arbitrated(e) => vec![&e.change, &e.result],
        Kind::Landed(e) => {
            let mut out = vec![e.change.as_str()];
            out.extend(e.submitted.as_deref());
            out
        }
        Kind::Rejected(e) => vec![&e.change],
        Kind::HeadMoved(e) => vec![&e.to],
    }
}

fn queue_kind(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Submitted(_)
            | Kind::ConflictCheck(_)
            | Kind::Parked(_)
            | Kind::Landed(_)
            | Kind::Rejected(_)
            | Kind::Arbitrated(_)
    )
}

fn text(kind: &Kind) -> String {
    match kind {
        Kind::Submitted(e) => format!("submitted {} (#{})", short(&e.change), e.submission),
        Kind::ConflictCheck(e) => format!(
            "conflict check {}: {:?}, {} set, {} merge",
            short(&e.change),
            e.result(),
            e.set_conflicts,
            e.merge_conflicts
        ),
        Kind::Verifying(e) => format!("verifying {}", short(&e.change)),
        Kind::EvidenceAttached(e) => {
            format!("evidence {} for {}", short(&e.evidence), short(&e.change))
        }
        Kind::Replaying(e) => format!("replaying {} (attempt {})", short(&e.change), e.attempt),
        Kind::Parked(e) => format!("parked {}: {}", short(&e.change), e.detail),
        Kind::Arbitrated(e) => format!("arbitrated {} -> {}", short(&e.change), short(&e.result)),
        Kind::Landed(e) => format!("landed {} at {}", short(&e.change), e.position),
        Kind::Rejected(e) => format!("rejected {}: {}", short(&e.change), e.reason),
        Kind::HeadMoved(e) => format!("head {}", short(&e.to)),
    }
}

pub fn run(
    json: bool,
    target: &Target,
    queue: bool,
    change: Option<String>,
    from: Option<u64>,
) -> Result<()> {
    if let Some(change) = &change {
        txn::parse_change(change)?;
    }
    let mut cursor = from;
    loop {
        let session = Session::open(target)?;
        let backend = session.backend();
        let mut stream = block_on(backend.events(proto::EventsRequest { from: cursor }))?;
        loop {
            let next = block_on(async {
                tokio::time::timeout(Duration::from_secs(1), stream.next()).await
            });
            let envelope = match next {
                Err(_) => continue,
                Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(envelope))) => envelope,
            };
            cursor = Some(envelope.cursor);
            let Some(kind) = envelope.event.as_ref().and_then(|e| e.kind.as_ref()) else {
                continue;
            };
            if queue && !queue_kind(kind) {
                continue;
            }
            if let Some(change) = &change
                && !changes(kind).contains(&change.as_str())
            {
                continue;
            }
            if json {
                output::print_json_line(&envelope)?;
            } else {
                println!("{:>6} {}", envelope.cursor, text(kind));
            }
            let settled = matches!(kind, Kind::Landed(_) | Kind::Parked(_) | Kind::Rejected(_));
            if change.is_some() && settled {
                return Ok(());
            }
        }
        // The stream ended: resume from the last cursor.
        std::thread::sleep(Duration::from_millis(200));
    }
}
