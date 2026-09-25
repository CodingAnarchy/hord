//! API messages → view models: how a [`proto::ChangeView`], its history,
//! and its conflict report read on the semantic change view and the
//! arbitration workbench.

use std::collections::BTreeMap;

use hord_api::proto;
use hord_api::proto::event::Kind;

use crate::view::{
    ChangeSide, ContestedView, EvidenceView, NodeView, OpLine, RungView, actor_label,
    evidence_kind_label, evidence_result_label, park_reason_label, short_id,
};

/// Intent and ops of a change.
#[must_use]
pub fn side(view: &proto::ChangeView) -> ChangeSide {
    let intent = view.intent.clone().unwrap_or_default();
    ChangeSide {
        change: view.change.clone(),
        summary: if intent.summary.is_empty() {
            short_id(&view.change).to_owned()
        } else {
            intent.summary
        },
        body: intent.body,
        acceptance: intent.acceptance,
        actor: actor_label(view.provenance.as_ref().and_then(|p| p.actor.as_ref())),
        ops: view.ops.iter().map(OpLine::from).collect(),
    }
}

/// Evidence, each labeled with where it was found.
#[must_use]
pub fn evidence(view: &proto::ChangeView) -> Vec<EvidenceView> {
    view.evidence
        .iter()
        .map(|e| {
            let (class, result) = evidence_result_label(e.result.as_ref());
            let by = actor_label(e.produced_by.as_ref());
            EvidenceView {
                id: e.id.clone(),
                kind: evidence_kind_label(e.kind.as_ref(), e.qualifier.as_deref()),
                class,
                result: format!("{result} ({}, by {by})", e.source),
            }
        })
        .collect()
}

/// Provenance as label/value lines.
#[must_use]
pub fn provenance(view: &proto::ChangeView) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(p) = &view.provenance {
        out.push(("author".into(), actor_label(p.actor.as_ref())));
        if let Some(proto::actor::Kind::Agent(a)) = p.actor.as_ref().and_then(|a| a.kind.as_ref())
            && !a.harness.is_empty()
        {
            out.push(("harness".into(), a.harness.clone()));
        }
        out.push(("created".into(), format!("{} ms", p.created_at_ms)));
        out.push(("toolchain".into(), p.toolchain.clone()));
        if let Some(session) = &p.session {
            out.push(("session".into(), session.clone()));
        }
        if let Some(parent) = &p.parent_intent {
            out.push(("replays".into(), parent.clone()));
        }
        out.push((
            "signature".into(),
            p.signature_key.clone().unwrap_or_else(|| "unsigned".into()),
        ));
    }
    out.push(("base".into(), view.base.clone()));
    out.push(("result".into(), view.result.clone()));
    for parent in &view.parents {
        out.push(("parent".into(), parent.clone()));
    }
    if let Some(from) = &view.rebased_from {
        out.push(("rebased from".into(), from.clone()));
    }
    out
}

/// Where a queue entry is, in words.
#[must_use]
pub fn status(entry: &proto::QueueEntry) -> String {
    let words = match entry.status() {
        proto::QueueStatus::Queued => "queued".to_owned(),
        proto::QueueStatus::Landed => match &entry.landed {
            Some(landed) if *landed != entry.change => {
                format!("landed as {}", short_id(landed))
            }
            _ => "landed".to_owned(),
        },
        proto::QueueStatus::Conflicted => "parked: conflict".to_owned(),
        proto::QueueStatus::Parked => "parked: policy".to_owned(),
        proto::QueueStatus::Rejected => format!(
            "rejected: {}",
            entry.reason.as_deref().unwrap_or("no reason given")
        ),
        proto::QueueStatus::Unspecified => "unknown".to_owned(),
    };
    format!("submission #{} · {words}", entry.seq)
}

/// Why a change is parked, for the workbench headline: the latest `Parked`
/// event's reason and detail, else the queue entry's status, with the
/// report's verification failure and unmet policy spelled out.
#[must_use]
pub fn park_reason(view: &proto::ChangeView) -> String {
    let parked = view.history.iter().rev().find_map(|e| match kind(e) {
        Some(Kind::Parked(p)) => Some(p),
        _ => None,
    });
    let mut out = match parked {
        Some(p) if p.detail.is_empty() => park_reason_label(p.reason()).to_owned(),
        Some(p) => format!("{} ({})", park_reason_label(p.reason()), p.detail),
        None => view
            .queue
            .as_ref()
            .map_or_else(|| "not parked".to_owned(), status),
    };
    if let Some(report) = view.queue.as_ref().and_then(|q| q.report.as_ref()) {
        if let Some(why) = &report.verification {
            out.push_str(&format!(". Verification: {why}"));
        }
        for p in &report.policy {
            out.push_str(&format!(
                ". Policy requires {} ({})",
                p.requirement, p.evidence
            ));
        }
    }
    out
}

fn kind(envelope: &proto::EventEnvelope) -> Option<&Kind> {
    envelope.event.as_ref()?.kind.as_ref()
}

/// The events of a change's history as rungs of the escalation ladder
/// (spec §6.4): conflict check, verification, replay attempts and their
/// outcomes, parking, arbitration, landing. Times are seconds since the
/// first event.
#[must_use]
pub fn rungs(history: &[proto::EventEnvelope]) -> Vec<RungView> {
    let start = history.first().map_or(0, |e| e.at_ms);
    let mut out: Vec<RungView> = Vec::new();
    for envelope in history {
        let at = format!(
            "+{:.1}s",
            envelope.at_ms.saturating_sub(start) as f64 / 1000.0
        );
        let (rung, outcome) = match kind(envelope) {
            Some(Kind::Submitted(s)) => (
                "submitted".to_owned(),
                format!("submission #{}", s.submission),
            ),
            Some(Kind::ConflictCheck(c)) => (
                "conflict check".to_owned(),
                format!(
                    "{} ({} set, {} merge)",
                    match c.result() {
                        proto::ConflictOutcome::Clean => "clean",
                        proto::ConflictOutcome::Overlap => "overlap, rebased",
                        proto::ConflictOutcome::Hard => "hard conflict",
                        proto::ConflictOutcome::Unspecified => "checked",
                    },
                    c.set_conflicts,
                    c.merge_conflicts
                ),
            ),
            Some(Kind::Verifying(v)) => (
                "verify".to_owned(),
                v.plan.as_ref().map_or_else(
                    || "started".to_owned(),
                    |p| {
                        format!(
                            "{} commands, {} selected tests, {} reused",
                            p.commands.len(),
                            p.selected_tests,
                            p.reused
                        )
                    },
                ),
            ),
            Some(Kind::EvidenceAttached(e)) => (
                evidence_kind_label(e.kind.as_ref(), e.qualifier.as_deref()),
                evidence_result_label(e.result.as_ref()).1,
            ),
            Some(Kind::Replaying(r)) => {
                // A replay's outcome is whatever the change did next.
                (
                    format!("replay #{} ({})", r.attempt, r.harness),
                    "running".to_owned(),
                )
            }
            Some(Kind::Parked(p)) => ("park".to_owned(), park_reason_label(p.reason()).to_owned()),
            Some(Kind::Arbitrated(a)) => (
                "arbitrate".to_owned(),
                format!(
                    "{} resolved it as {}",
                    actor_label(a.by.as_ref()),
                    short_id(&a.result)
                ),
            ),
            Some(Kind::Landed(l)) => ("land".to_owned(), format!("landed at #{}", l.position)),
            Some(Kind::Rejected(r)) => ("reject".to_owned(), r.reason.clone()),
            Some(Kind::HeadMoved(_)) | None => continue,
        };
        // Close the previous replay with this event's outcome.
        if let Some(prev) = out.last_mut()
            && prev.rung.starts_with("replay #")
            && prev.outcome == "running"
        {
            prev.outcome = format!("then {rung}: {outcome}");
        }
        out.push(RungView { at, rung, outcome });
    }
    out
}

/// Contested definitions with both sides' ops on each, and contested files
/// that name no definition, from the parked change's conflict report.
#[must_use]
pub fn contested(
    theirs: &proto::ChangeView,
    ours: &[proto::ChangeView],
) -> (Vec<ContestedView>, Vec<String>) {
    let mut nodes: BTreeMap<String, (proto::NodeRef, Vec<String>)> = BTreeMap::new();
    let mut paths = Vec::new();
    if let Some(report) = theirs.queue.as_ref().and_then(|q| q.report.as_ref()) {
        for c in &report.conflicts {
            let why = format!(
                "{} with {}",
                match c.kind() {
                    proto::ConflictKind::WriteWrite => "both wrote it",
                    proto::ConflictKind::ReadWrite => "it read what the other wrote",
                    proto::ConflictKind::WriteRead => "it wrote what the other read",
                    proto::ConflictKind::Unspecified => "overlap",
                },
                if c.landed_summary.is_empty() {
                    short_id(&c.landed).to_owned()
                } else {
                    c.landed_summary.clone()
                }
            );
            for node in &c.nodes {
                nodes
                    .entry(node.id.clone())
                    .or_insert_with(|| (node.clone(), Vec::new()))
                    .1
                    .push(why.clone());
            }
            paths.extend(c.paths.iter().cloned());
        }
        for m in &report.merge {
            let why = format!(
                "{} merge conflict: {}",
                match m.severity() {
                    proto::MergeSeverity::Hard => "hard",
                    proto::MergeSeverity::Soft => "soft",
                    proto::MergeSeverity::Unspecified => "a",
                },
                m.reason
            );
            for node in &m.nodes {
                nodes
                    .entry(node.id.clone())
                    .or_insert_with(|| (node.clone(), Vec::new()))
                    .1
                    .push(why.clone());
            }
            if m.nodes.is_empty() {
                paths.push(m.path.clone());
            }
        }
    }
    paths.sort();
    paths.dedup();
    let ops_on = |view: &proto::ChangeView, id: &str| -> Vec<OpLine> {
        view.ops
            .iter()
            .filter(|op| op.node.as_deref() == Some(id) || op.parent.as_deref() == Some(id))
            .map(OpLine::from)
            .collect()
    };
    let contested = nodes
        .into_iter()
        .map(|(id, (node, whys))| ContestedView {
            node: NodeView::from(&node),
            why: whys.join("; "),
            ours: ours.iter().flat_map(|o| ops_on(o, &id)).collect(),
            theirs: ops_on(theirs, &id),
        })
        .collect();
    (contested, paths)
}

/// Landed changes a parked change's report names, in order, without
/// duplicates.
#[must_use]
pub fn landed_against(view: &proto::ChangeView) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(report) = view.queue.as_ref().and_then(|q| q.report.as_ref()) {
        for c in &report.conflicts {
            if !out.contains(&c.landed) {
                out.push(c.landed.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use hord_api::wire;

    use super::*;

    fn env(cursor: u64, at_ms: u64, kind: Kind) -> proto::EventEnvelope {
        proto::EventEnvelope {
            cursor,
            at_ms,
            event: Some(wire::event(kind)),
        }
    }

    fn node(id: &str, name: &str) -> proto::NodeRef {
        proto::NodeRef {
            id: id.into(),
            name: Some(name.into()),
            path: Some("src/lib.rs".into()),
        }
    }

    fn op(node: &str, text: &str) -> proto::OpView {
        proto::OpView {
            op: "replace".into(),
            node: Some(node.into()),
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn replay_attempts_show_what_came_of_them() {
        let rungs = rungs(&[
            env(
                1,
                1_000,
                Kind::Parked(proto::Parked {
                    change: "c".into(),
                    reason: proto::ParkReason::MergeConflict.into(),
                    detail: String::new(),
                }),
            ),
            env(
                2,
                2_500,
                Kind::Replaying(proto::Replaying {
                    change: "c".into(),
                    attempt: 1,
                    harness: "ref".into(),
                }),
            ),
            env(
                3,
                4_000,
                Kind::Rejected(proto::Rejected {
                    change: "c".into(),
                    reason: "acceptance test failed".into(),
                }),
            ),
        ]);
        assert_eq!(rungs.len(), 3);
        assert_eq!(rungs[1].rung, "replay #1 (ref)");
        assert_eq!(rungs[1].outcome, "then reject: acceptance test failed");
        assert_eq!(rungs[1].at, "+1.5s");
    }

    #[test]
    fn contested_definitions_pair_both_sides_ops() {
        let theirs = proto::ChangeView {
            change: "t".into(),
            ops: vec![op("n1", "replace parse"), op("n2", "replace other")],
            queue: Some(proto::QueueEntry {
                report: Some(proto::ConflictReport {
                    conflicts: vec![proto::SetConflict {
                        kind: proto::ConflictKind::WriteWrite.into(),
                        landed: "o".into(),
                        nodes: vec![node("n1", "parse")],
                        paths: vec!["Cargo.lock".into()],
                        landed_summary: "Speed up parse".into(),
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ours = proto::ChangeView {
            change: "o".into(),
            ops: vec![op("n1", "replace parse (ours)")],
            ..Default::default()
        };
        assert_eq!(landed_against(&theirs), ["o"]);
        let (contested, paths) = contested(&theirs, &[ours]);
        assert_eq!(contested.len(), 1);
        assert_eq!(contested[0].node.label, "parse");
        assert_eq!(contested[0].why, "both wrote it with Speed up parse");
        assert_eq!(contested[0].ours[0].text, "replace parse (ours)");
        assert_eq!(contested[0].theirs.len(), 1);
        assert_eq!(paths, ["Cargo.lock"]);
    }
}
