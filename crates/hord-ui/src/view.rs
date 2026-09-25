//! Pages and fragments: askama templates over display-ready view models.
//!
//! View models hold strings already worded for people. Whatever feeds them
//! (the live API or a recording) maps its messages into them first, so the
//! templates carry no logic beyond loops and conditionals.

use askama::Template;
use hord_api::proto;
use hord_api::proto::evidence_kind::Kind as EvKind;
use hord_api::proto::evidence_result::Result as EvResult;

use crate::strip::{EvidenceItem, Row, Stage, Strip};

/// Shortest prefix of an id shown in lists.
const SHORT_ID: usize = 12;

/// An id cut to its first 12 characters for display.
#[must_use]
pub fn short_id(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

/// An actor in words: `human:<id>` or `agent:<id> (<model>)`.
#[must_use]
pub fn actor_label(actor: Option<&proto::Actor>) -> String {
    match actor.and_then(|a| a.kind.as_ref()) {
        Some(proto::actor::Kind::Human(h)) => format!("human:{}", h.id),
        Some(proto::actor::Kind::Agent(a)) if a.model.is_empty() => format!("agent:{}", a.id),
        Some(proto::actor::Kind::Agent(a)) => format!("agent:{} ({})", a.id, a.model),
        None => "unknown".into(),
    }
}

/// Evidence kind in policy spelling (`test`, `review`, `custom:x`), with the
/// qualifier after a colon when there is one (ADR 0026).
#[must_use]
pub fn evidence_kind_label(kind: Option<&proto::EvidenceKind>, qualifier: Option<&str>) -> String {
    let kind = match kind.and_then(|k| k.kind.as_ref()) {
        Some(EvKind::Check(_)) => "check".to_owned(),
        Some(EvKind::Test(_)) => "test".to_owned(),
        Some(EvKind::Bench(_)) => "bench".to_owned(),
        Some(EvKind::Lint(_)) => "lint".to_owned(),
        Some(EvKind::Review(_)) => "review".to_owned(),
        Some(EvKind::Custom(name)) => name.clone(),
        Some(EvKind::Rebase(_)) => "rebase".to_owned(),
        None => "evidence".to_owned(),
    };
    match qualifier {
        Some(q) => format!("{kind}:{q}"),
        None => kind,
    }
}

/// `(css class, words)` for an evidence result.
#[must_use]
pub fn evidence_result_label(result: Option<&proto::EvidenceResult>) -> (&'static str, String) {
    match result.and_then(|r| r.result.as_ref()) {
        Some(EvResult::Pass(_)) => ("pass", "pass".into()),
        Some(EvResult::Fail(why)) => ("fail", format!("fail: {why}")),
        Some(EvResult::Skipped(why)) => ("skip", format!("skipped: {why}")),
        None => ("unknown", "attached".into()),
    }
}

/// Why a change was parked, in words a newcomer can follow (M5 demo:
/// "explain from the UI alone why a given change was parked").
#[must_use]
pub fn park_reason_label(reason: proto::ParkReason) -> &'static str {
    match reason {
        proto::ParkReason::MergeConflict => {
            "parked: its edits collide with a landed change and could not be merged"
        }
        proto::ParkReason::VerificationFailed => {
            "parked: it merged, but checks failed on the merged result"
        }
        proto::ParkReason::NeedsArbitration => "parked: waiting for an arbiter to decide",
        proto::ParkReason::NeedsReview => "parked: policy requires a review before it lands",
        proto::ParkReason::Policy => "parked: policy requires evidence it does not have yet",
        proto::ParkReason::Unspecified => "parked",
    }
}

/// One piece of evidence on a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceView {
    /// ObjectId.
    pub id: String,
    /// Kind, with qualifier.
    pub kind: String,
    /// CSS class: `pass`, `fail`, `skip`, or `unknown`.
    pub class: &'static str,
    /// Result in words.
    pub result: String,
}

impl From<&EvidenceItem> for EvidenceView {
    fn from(item: &EvidenceItem) -> Self {
        let (class, result) = evidence_result_label(item.result.as_ref());
        Self {
            id: item.id.clone(),
            kind: evidence_kind_label(item.kind.as_ref(), item.qualifier.as_deref()),
            class,
            result,
        }
    }
}

/// One row of the landing strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    /// DOM id: stable per submitted change, so a live update replaces it.
    pub dom_id: String,
    /// The change as submitted (links to the semantic change view).
    pub change: String,
    /// Shortened id.
    pub change_short: String,
    /// Intent summary, or the short id when not known.
    pub label: String,
    /// Author in words.
    pub actor: String,
    /// CSS class of the stage: `proposed`, `verifying`, `replaying`,
    /// `arbitration`, `review`, `landed`, `rejected`, `arbitrated`.
    pub stage_class: &'static str,
    /// Stage in words.
    pub stage: String,
    /// Conflict-check outcome in words, once run.
    pub conflict: Option<String>,
    /// Evidence in arrival order.
    pub evidence: Vec<EvidenceView>,
    /// Whether the row links to the arbitration workbench.
    pub parked: bool,
}

impl From<&Row> for RowView {
    fn from(row: &Row) -> Self {
        let (stage_class, stage) = stage_label(&row.stage);
        Self {
            dom_id: format!("row-{}", row.change),
            change: row.change.clone(),
            change_short: short_id(&row.change).to_owned(),
            label: row
                .summary
                .clone()
                .unwrap_or_else(|| short_id(&row.change).to_owned()),
            actor: actor_label(row.actor.as_ref()),
            stage_class,
            stage,
            conflict: row.conflict.as_ref().map(conflict_label),
            evidence: row.evidence.iter().map(EvidenceView::from).collect(),
            parked: matches!(row.stage, Stage::Parked { .. }),
        }
    }
}

fn stage_label(stage: &Stage) -> (&'static str, String) {
    match stage {
        Stage::Proposed => ("proposed", "proposed".into()),
        Stage::Verifying => ("verifying", "verifying".into()),
        Stage::Replaying { attempt, harness } => (
            "replaying",
            format!("replaying (attempt {attempt}, {harness})"),
        ),
        Stage::Parked { reason, detail } => {
            let class = match reason {
                proto::ParkReason::NeedsReview | proto::ParkReason::Policy => "review",
                _ => "arbitration",
            };
            let words = park_reason_label(*reason);
            if detail.is_empty() {
                (class, words.into())
            } else {
                (class, format!("{words} ({detail})"))
            }
        }
        Stage::Arbitrated { result } => {
            ("arbitrated", format!("arbitrated → {}", short_id(result)))
        }
        Stage::Landed {
            position: Some(position),
        } => ("landed", format!("landed at #{position}")),
        Stage::Landed { position: None } => ("landed", "landed".into()),
        Stage::Rejected { reason } => ("rejected", format!("rejected: {reason}")),
    }
}

fn conflict_label(check: &proto::ConflictCheck) -> String {
    let outcome = match check.result() {
        proto::ConflictOutcome::Clean => "clean",
        proto::ConflictOutcome::Overlap => "overlap, merged",
        proto::ConflictOutcome::Hard => "hard conflict",
        proto::ConflictOutcome::Unspecified => "checked",
    };
    if check.set_conflicts == 0 && check.merge_conflicts == 0 {
        outcome.into()
    } else {
        format!(
            "{outcome} ({} set, {} merge)",
            check.set_conflicts, check.merge_conflicts
        )
    }
}

/// Every row of a strip, in order.
#[must_use]
pub fn rows(strip: &Strip) -> Vec<RowView> {
    strip.rows().iter().map(RowView::from).collect()
}

/// The strip's rows alone: the live region, re-rendered on update.
#[derive(Template, Debug)]
#[template(path = "strip_rows.html")]
pub struct StripRows {
    /// URL prefix of this repository's pages (`""` or `/r/<name>`).
    pub base: String,
    /// Rows, in order.
    pub rows: Vec<RowView>,
}

/// One row alone, for a live update of that row.
#[derive(Template, Debug)]
#[template(path = "strip_row.html")]
pub struct StripRow {
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The row.
    pub row: RowView,
}

/// View 1: the landing strip page.
#[derive(Template, Debug)]
#[template(path = "strip.html")]
pub struct StripPage {
    /// Page title (the repository name).
    pub title: String,
    /// URL prefix of this repository's pages (`""` or `/r/<name>`).
    pub base: String,
    /// Rows at render time.
    pub rows: Vec<RowView>,
    /// URL of the live region's event source, when live.
    pub live: Option<String>,
    /// Head at render time.
    pub head: Option<String>,
}

/// Flight-recorder playback: the strip plus transport controls.
#[derive(Template, Debug)]
#[template(path = "playback.html")]
pub struct PlaybackPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// Recording id (a Blob's ObjectId).
    pub recording: String,
    /// What was recorded, in words.
    pub description: String,
    /// Events in the recording: the scrubber's range.
    pub total: usize,
    /// Scrubber position at render time.
    pub position: usize,
    /// Rows at that position.
    pub rows: Vec<RowView>,
}

/// A definition id with its name and file, when resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    /// NodeId.
    pub id: String,
    /// Name, else the short id.
    pub label: String,
    /// File, when known.
    pub path: Option<String>,
}

impl From<&proto::NodeRef> for NodeView {
    fn from(node: &proto::NodeRef) -> Self {
        Self {
            id: node.id.clone(),
            label: node
                .name
                .clone()
                .unwrap_or_else(|| short_id(&node.id).to_owned()),
            path: node.path.clone(),
        }
    }
}

/// One op, worded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpLine {
    /// `insert`, `delete`, `replace`, `move`, `rename`, `blob`, or `tree`.
    pub op: String,
    /// One line, with names where known.
    pub text: String,
}

impl From<&proto::OpView> for OpLine {
    fn from(op: &proto::OpView) -> Self {
        Self {
            op: op.op.clone(),
            text: op.text.clone(),
        }
    }
}

/// One side of a change as the views show it: intent first, then ops.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSide {
    /// ChangeId.
    pub change: String,
    /// Intent summary.
    pub summary: String,
    /// Intent body (task description, prompt, spec excerpt).
    pub body: String,
    /// Acceptance criteria.
    pub acceptance: Vec<String>,
    /// Author in words.
    pub actor: String,
    /// Ops on named definitions.
    pub ops: Vec<OpLine>,
}

/// View 2: one change.
#[derive(Template, Debug)]
#[template(path = "change.html")]
pub struct ChangePage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The change: intent, ops.
    pub side: ChangeSide,
    /// Where it is in the lander, in words, when queued.
    pub status: Option<String>,
    /// Evidence for its result snapshot (ADR 0025) and the author's.
    pub evidence: Vec<EvidenceView>,
    /// Provenance lines: toolchain, harness, created, parents, rebased from.
    pub provenance: Vec<(String, String)>,
    /// Read set.
    pub reads: Vec<NodeView>,
    /// Write set.
    pub writes: Vec<NodeView>,
    /// Text diff (the secondary tab), when available.
    pub diff: Option<String>,
    /// Its path through the lander and the ladder, oldest first.
    pub history: Vec<RungView>,
    /// Whether the review form is shown.
    pub reviewable: bool,
    /// Why the review form is not shown, when it is not.
    pub review_note: Option<String>,
    /// Outcome of a review just submitted, in words.
    pub flash: Option<String>,
}

/// One rung of the escalation ladder (spec §6.4) in the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RungView {
    /// When it happened.
    pub at: String,
    /// What was tried: `rebase`, `replay #n (harness)`, `park`, `arbitrate`.
    pub rung: String,
    /// What came of it.
    pub outcome: String,
}

/// One contested definition with both sides' ops on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContestedView {
    /// The definition.
    pub node: NodeView,
    /// Why it is contested (the rule or merge reason).
    pub why: String,
    /// Ours (landed) ops on it.
    pub ours: Vec<OpLine>,
    /// Theirs (parked) ops on it.
    pub theirs: Vec<OpLine>,
}

/// View 3: the arbitration workbench for one parked change.
#[derive(Template, Debug)]
#[template(path = "arbitration.html")]
pub struct ArbitrationPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// Why it was parked, in words.
    pub reason: String,
    /// Ours: the landed changes it collided with.
    pub ours: Vec<ChangeSide>,
    /// Theirs: the parked change.
    pub theirs: ChangeSide,
    /// Contested definitions.
    pub contested: Vec<ContestedView>,
    /// Files in contention with no definition (set conflicts on paths).
    pub paths: Vec<String>,
    /// Ladder history, oldest first.
    pub ladder: Vec<RungView>,
    /// Where the parked change is now, in words.
    pub status: String,
    /// Whether it can be arbitrated now (conflicted or out of replays).
    pub open: bool,
    /// Replay attempts, oldest first (spec §6.4 rung 2).
    pub attempts: Vec<AttemptView>,
    /// Replay results offered to the arbiter, one per distinct result
    /// (ADR 0029).
    pub candidates: Vec<CandidateView>,
    /// The machine summary's reasons it could not land.
    pub reasons: Vec<String>,
    /// What each side changed, from the machine summary.
    pub sides: Vec<SideChange>,
    /// A resolution submitted and not landed yet, or why the last one did
    /// not help.
    pub pending: Option<String>,
    /// Head at render time, for the workspace instructions.
    pub head: Option<String>,
    /// Whether to show how to resolve in a workspace.
    pub show_workspace: bool,
    /// Outcome of an action just taken, in words.
    pub flash: Option<String>,
}

/// One replay attempt on the workbench.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptView {
    /// Attempt number.
    pub attempt: u32,
    /// Harness.
    pub harness: String,
    /// CSS class: `pass` (proposed), `fail`, or `unknown` (running).
    pub class: &'static str,
    /// How it ended, with the harness's detail.
    pub outcome: String,
    /// The replay change it proposed.
    pub change: Option<String>,
    /// Time, tokens, cost, model.
    pub spent: String,
    /// The arbiter's note it ran with.
    pub note: Option<String>,
}

/// One arbitration candidate: a distinct replay result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateView {
    /// The replay change (picked with `resolved`).
    pub change: String,
    /// Shortened.
    pub short: String,
    /// The attempts that produced it: `#1, #3`.
    pub attempts: String,
    /// Number of ops.
    pub ops: u32,
    /// How it settled in the lander.
    pub settled: String,
}

/// One side of the collision as the machine summary words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideChange {
    /// `ours (landed)` or `theirs (parked)`, with the intent.
    pub label: String,
    /// The side's change.
    pub change: String,
    /// What it did to the contested definitions and files.
    pub changed: Vec<String>,
}

/// One registered recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingRow {
    /// Blob ObjectId.
    pub id: String,
    /// What was recorded.
    pub description: String,
    /// Repository it came from.
    pub repo: String,
    /// Number of events.
    pub events: u64,
}

/// The recordings under `.hord/recordings/`.
#[derive(Template, Debug)]
#[template(path = "recordings.html")]
pub struct RecordingsPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// Recordings, in id order.
    pub recordings: Vec<RecordingRow>,
}

/// An error page.
#[derive(Template, Debug)]
#[template(path = "error.html")]
pub struct ErrorPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// What went wrong.
    pub message: String,
    /// Whether signing in would help (the call needed a token).
    pub sign_in: bool,
}

/// The sign-in page: a bearer token kept in a cookie.
#[derive(Template, Debug)]
#[template(path = "login.html")]
pub struct LoginPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
}

#[cfg(test)]
mod tests {
    use hord_api::proto::event::Kind;
    use hord_api::wire;

    use super::*;

    #[test]
    fn a_parked_row_says_why_and_links_to_the_workbench() -> askama::Result<()> {
        let mut strip = Strip::new();
        strip.apply(&proto::EventEnvelope {
            cursor: 1,
            at_ms: 0,
            event: Some(wire::event(Kind::Parked(proto::Parked {
                change: "abcdef0123456789".into(),
                reason: proto::ParkReason::MergeConflict.into(),
                detail: "src/lib.rs".into(),
            }))),
        });
        strip.set_summary("abcdef0123456789", "Rename <parse> & friends");
        let html = StripRows {
            base: String::new(),
            rows: rows(&strip),
        }
        .render()?;
        assert!(html.contains("collide with a landed change"), "{html}");
        assert!(
            html.contains("Rename &#60;parse&#62; &#38; friends"),
            "{html}"
        );
        assert!(html.contains("/arbitrate/abcdef0123456789"), "{html}");
        assert!(html.contains("id=\"row-abcdef0123456789\""), "{html}");
        Ok(())
    }

    #[test]
    fn pages_render() -> askama::Result<()> {
        let side = ChangeSide {
            change: "c".into(),
            summary: "Do it".into(),
            ops: vec![OpLine {
                op: "replace".into(),
                text: "replace fn parse".into(),
            }],
            ..Default::default()
        };
        let change = ChangePage {
            title: "t".into(),
            base: String::new(),
            side: side.clone(),
            status: None,
            evidence: Vec::new(),
            provenance: vec![("harness".into(), "h".into())],
            reads: Vec::new(),
            writes: Vec::new(),
            diff: Some("-a\n+b".into()),
            history: Vec::new(),
            reviewable: true,
            review_note: None,
            flash: None,
        }
        .render()?;
        assert!(change.contains("replace fn parse"));
        assert!(change.contains("name=\"verdict\""));
        let intent = change.find("Do it").unwrap_or(usize::MAX);
        let diff = change.find("+b").unwrap_or(0);
        assert!(intent < diff, "intent comes before the text diff");
        let arb = ArbitrationPage {
            title: "t".into(),
            base: "/r/x".into(),
            reason: "parked".into(),
            ours: vec![side.clone()],
            theirs: side,
            contested: Vec::new(),
            paths: vec!["Cargo.lock".into()],
            ladder: vec![RungView {
                at: "0".into(),
                rung: "replay #1 (ref)".into(),
                outcome: "failed".into(),
            }],
            status: "parked".into(),
            open: true,
            attempts: vec![AttemptView {
                attempt: 1,
                harness: "ref".into(),
                class: "fail",
                outcome: "gave up".into(),
                change: None,
                spent: "1.0s".into(),
                note: Some("keep both".into()),
            }],
            candidates: vec![CandidateView {
                change: "cand".into(),
                short: "cand".into(),
                attempts: "#2".into(),
                ops: 1,
                settled: "conflicted".into(),
            }],
            reasons: vec!["both replaced two".into()],
            sides: Vec::new(),
            pending: None,
            head: Some("h".into()),
            show_workspace: true,
            flash: None,
        }
        .render()?;
        for action in [
            "pick_ours",
            "pick_theirs",
            "replay",
            "workspace",
            "resolved",
        ] {
            assert!(arb.contains(&format!("value=\"{action}\"")), "{action}");
        }
        assert!(arb.contains("/r/x/arbitrate/c"));
        StripPage {
            title: "t".into(),
            base: String::new(),
            rows: Vec::new(),
            live: Some("/events".into()),
            head: None,
        }
        .render()?;
        PlaybackPage {
            title: "t".into(),
            base: String::new(),
            recording: "r".into(),
            description: "d".into(),
            total: 3,
            position: 0,
            rows: Vec::new(),
        }
        .render()?;
        Ok(())
    }
}
