//! Views 4–6 (spec §10.4, M6): node lineage, provenance trace, and the
//! repository browser with its graph view. View models and templates, and
//! the mapping from the `Changes` messages that feed them.

use askama::Template;
use hord_api::proto;

use crate::view::{
    EvidenceView, NodeView, OpLine, actor_label, evidence_kind_label, evidence_result_label,
    park_reason_label, short_id,
};

/// Most neighbours drawn per side of the graph view; the lists below it
/// show every one.
pub const GRAPH_SIDE: usize = 12;

/// Longest label drawn in the graph view, in characters.
const GRAPH_LABEL: usize = 30;

/// One definition as a link target: its id, name, and file.
fn node_view(node: Option<&proto::NodeRef>) -> NodeView {
    node.map_or_else(
        || NodeView {
            id: String::new(),
            label: "(unknown)".into(),
            path: None,
        },
        NodeView::from,
    )
}

// ------------------------------------------------------------ lineage

/// One change in a definition's lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageRow {
    /// Landing position.
    pub position: u64,
    /// The landed change.
    pub change: String,
    /// Shortened.
    pub short: String,
    /// Intent summary.
    pub summary: String,
    /// Author in words.
    pub actor: String,
    /// Its result snapshot, for the browser.
    pub result: String,
    /// What happened to the definition: born, renamed, moved, edited,
    /// retired, split, merged, derived.
    pub what: Vec<String>,
    /// Name and file before, when it was there.
    pub before: Option<String>,
    /// Name and file after, when it is still there.
    pub after: Option<String>,
    /// File to open at the result, when it is there.
    pub file: Option<String>,
    /// The change's ops on it.
    pub ops: Vec<OpLine>,
    /// Other definitions its identity deltas name (the split's parts, what
    /// it was derived or merged from), each linking to its own lineage.
    pub related: Vec<NodeView>,
    /// Evidence on the change and its result.
    pub evidence: Vec<EvidenceView>,
}

fn located(node: &proto::NodeRef) -> String {
    match (&node.name, &node.path) {
        (Some(name), Some(path)) => format!("{name} in {path}"),
        (None, Some(path)) => format!("{} in {path}", short_id(&node.id)),
        _ => short_id(&node.id).to_owned(),
    }
}

/// What a lineage entry did to the definition, in words.
fn what_happened(entry: &proto::LineageEntry) -> Vec<String> {
    let mut out = Vec::new();
    for delta in &entry.identity {
        out.push(match delta.kind.as_str() {
            "birth" => "born".to_owned(),
            "death" => "retired".to_owned(),
            _ => delta.text.clone(),
        });
    }
    match (&entry.before, &entry.after) {
        (Some(before), Some(after)) => {
            if before.name != after.name {
                out.push(format!(
                    "renamed {} → {}",
                    before.name.as_deref().unwrap_or("?"),
                    after.name.as_deref().unwrap_or("?")
                ));
            }
            if before.path != after.path {
                out.push(format!(
                    "moved {} → {}",
                    before.path.as_deref().unwrap_or("?"),
                    after.path.as_deref().unwrap_or("?")
                ));
            }
        }
        (None, Some(_)) if !out.iter().any(|w| w == "born") => out.push("appeared".into()),
        (Some(_), None) if !out.iter().any(|w| w == "retired") => out.push("removed".into()),
        _ => {}
    }
    if entry.ops.iter().any(|o| o.op == "replace") {
        out.push("edited".into());
    }
    if out.is_empty() {
        out.push("touched".into());
    }
    out
}

/// A lineage entry for the page.
#[must_use]
pub fn lineage_row(entry: &proto::LineageEntry) -> LineageRow {
    let change = entry.change.clone().unwrap_or_default();
    let mut related: Vec<NodeView> = Vec::new();
    for delta in &entry.identity {
        for node in delta.node.iter().chain(&delta.others) {
            let own = entry
                .after
                .as_ref()
                .or(entry.before.as_ref())
                .is_some_and(|n| n.id == node.id);
            if !own && !related.iter().any(|r| r.id == node.id) {
                related.push(NodeView::from(node));
            }
        }
    }
    LineageRow {
        position: change.position,
        short: short_id(&change.change).to_owned(),
        summary: if change.summary.is_empty() {
            short_id(&change.change).to_owned()
        } else {
            change.summary.clone()
        },
        actor: actor_label(change.actor.as_ref()),
        result: change.result.clone(),
        change: change.change,
        what: what_happened(entry),
        before: entry.before.as_ref().map(located),
        after: entry.after.as_ref().map(located),
        file: entry.after.as_ref().and_then(|n| n.path.clone()),
        ops: entry.ops.iter().map(OpLine::from).collect(),
        related,
        evidence: entry
            .evidence
            .iter()
            .map(|e| {
                let (class, result) = evidence_result_label(e.result.as_ref());
                EvidenceView {
                    id: e.id.clone(),
                    kind: evidence_kind_label(e.kind.as_ref(), e.qualifier.as_deref()),
                    class,
                    result,
                }
            })
            .collect(),
    }
}

/// View 4: one definition's history (semantic blame).
#[derive(Template, Debug)]
#[template(path = "lineage.html")]
pub struct LineagePage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The definition, where it is now or was last.
    pub node: NodeView,
    /// Whether head still has it.
    pub live: bool,
    /// Changes that touched it, newest first.
    pub rows: Vec<LineageRow>,
}

/// The lineage page for a `NodeLineage` reply.
#[must_use]
pub fn lineage_page(base: &str, reply: &proto::NodeLineageResponse) -> LineagePage {
    let node = node_view(reply.node.as_ref());
    LineagePage {
        title: format!("Lineage: {}", node.label),
        base: base.to_owned(),
        live: reply.live,
        rows: reply.entries.iter().rev().map(lineage_row).collect(),
        node,
    }
}

// ------------------------------------------------------------ trace

/// One step of a provenance trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepView {
    /// Seconds since the first step.
    pub at: String,
    /// Stage in words; its CSS class is `stage-<class>`.
    pub stage: &'static str,
    /// CSS class of the stage.
    pub class: &'static str,
    /// What happened.
    pub text: String,
    /// CSS class of its result: `pass`, `fail`, `skip`, or `unknown`.
    pub outcome: &'static str,
    /// Who acted, when it was not the author.
    pub actor: Option<String>,
    /// A change it names, to link to.
    pub change: Option<String>,
}

fn stage_words(stage: proto::TraceStage) -> (&'static str, &'static str) {
    match stage {
        proto::TraceStage::Intent => ("intent written", "proposed"),
        proto::TraceStage::Proposed => ("proposed", "proposed"),
        proto::TraceStage::Submitted => ("submitted", "proposed"),
        proto::TraceStage::Conflict => ("conflict check", "verifying"),
        proto::TraceStage::Parked => ("parked", "arbitration"),
        proto::TraceStage::Replay => ("replay", "replaying"),
        proto::TraceStage::Verify => ("verification", "verifying"),
        proto::TraceStage::Evidence => ("evidence", "verifying"),
        proto::TraceStage::Review => ("review", "review"),
        proto::TraceStage::Arbitrated => ("arbitration", "arbitrated"),
        proto::TraceStage::Landed => ("landed", "landed"),
        proto::TraceStage::Rejected => ("rejected", "rejected"),
        proto::TraceStage::Unspecified => ("step", "proposed"),
    }
}

/// A trace step for the page. A parked step says why in the words the
/// strip and the workbench use.
#[must_use]
pub fn step_view(step: &proto::TraceStep, start: u64) -> StepView {
    let (stage, class) = stage_words(step.stage());
    let text = match step.park.and_then(|p| proto::ParkReason::try_from(p).ok()) {
        Some(reason) if step.text.is_empty() => park_reason_label(reason).to_owned(),
        Some(reason) => format!("{} ({})", park_reason_label(reason), step.text),
        None => step.text.clone(),
    };
    StepView {
        at: format!("+{:.1}s", step.at_ms.saturating_sub(start) as f64 / 1000.0),
        stage,
        class,
        text,
        outcome: match step.outcome.as_deref() {
            Some("pass") => "pass",
            Some("fail") => "fail",
            Some("skip") => "skip",
            _ => "unknown",
        },
        actor: step.actor.as_ref().map(|a| actor_label(Some(a))),
        change: step.change.clone(),
    }
}

/// One message the trace is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceView {
    /// What it is: `submitted`, `landed as`, `replay proposal`.
    pub role: &'static str,
    /// Its change.
    pub change: String,
    /// Its intent summary.
    pub summary: String,
}

/// View 5: one change's story.
#[derive(Template, Debug)]
#[template(path = "trace.html")]
pub struct TracePage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The change asked for.
    pub change: String,
    /// Its intent summary.
    pub summary: String,
    /// Its author.
    pub actor: String,
    /// Where it is in the lander now, in words.
    pub status: Option<String>,
    /// Whether it is parked for an arbiter now (links the workbench).
    pub parked: bool,
    /// The timeline.
    pub steps: Vec<StepView>,
    /// The messages the trace is built from.
    pub sources: Vec<SourceView>,
}

fn source(role: &'static str, view: &proto::ChangeView) -> SourceView {
    SourceView {
        role,
        change: view.change.clone(),
        summary: view
            .intent
            .as_ref()
            .map(|i| i.summary.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| short_id(&view.change).to_owned()),
    }
}

/// The trace page for a `ChangeTrace` reply.
#[must_use]
pub fn trace_page(base: &str, reply: &proto::ChangeTraceResponse) -> TracePage {
    let submitted = reply.submitted.clone().unwrap_or_default();
    let start = reply.steps.first().map_or(0, |s| s.at_ms);
    let mut sources = vec![source("submitted", &submitted)];
    if let Some(landed) = &reply.landed {
        sources.push(source("landed as", landed));
    }
    for replay in &reply.replays {
        sources.push(source("replay proposal", replay));
    }
    let head = source("submitted", &submitted);
    TracePage {
        title: format!("Trace: {}", head.summary),
        base: base.to_owned(),
        change: reply.change.clone(),
        summary: head.summary,
        actor: actor_label(submitted.provenance.as_ref().and_then(|p| p.actor.as_ref())),
        status: submitted.queue.as_ref().map(crate::present::status),
        parked: crate::present::arbitrable(submitted.queue.as_ref()),
        steps: reply.steps.iter().map(|s| step_view(s, start)).collect(),
        sources,
    }
}

// ------------------------------------------------------------ browser

/// One step of a path, for breadcrumbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    /// Component name.
    pub name: String,
    /// Path up to and including it.
    pub path: String,
}

/// Breadcrumbs for `path`.
#[must_use]
pub fn crumbs(path: &str) -> Vec<Crumb> {
    let mut out = Vec::new();
    let mut at = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(part);
        out.push(Crumb {
            name: part.to_owned(),
            path: at.clone(),
        });
    }
    out
}

/// View 6: one directory of a snapshot.
#[derive(Template, Debug)]
#[template(path = "tree.html")]
pub struct TreePage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The snapshot.
    pub snapshot: String,
    /// Breadcrumbs to the directory.
    pub crumbs: Vec<Crumb>,
    /// Its entries: directories first.
    pub entries: Vec<proto::TreeItem>,
}

/// A definition in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionRow {
    /// The definition.
    pub node: NodeView,
    /// Adapter kind.
    pub kind: String,
    /// First line.
    pub start: u32,
    /// Last line.
    pub end: u32,
    /// Nesting depth, for indentation.
    pub depth: usize,
}

/// One source line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLine {
    /// Line number, from 1.
    pub number: usize,
    /// Its text.
    pub text: String,
}

/// View 6: one file of a snapshot, with its definitions.
#[derive(Template, Debug)]
#[template(path = "file.html")]
pub struct FilePage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The snapshot.
    pub snapshot: String,
    /// Breadcrumbs to the file.
    pub crumbs: Vec<Crumb>,
    /// Its Blob.
    pub blob: String,
    /// Its size in bytes.
    pub size: u64,
    /// Whether it is not text.
    pub binary: bool,
    /// Definitions, in source order.
    pub definitions: Vec<DefinitionRow>,
    /// Its lines.
    pub lines: Vec<SourceLine>,
}

/// The file page for a `GetFile` reply.
#[must_use]
pub fn file_page(base: &str, reply: &proto::GetFileResponse) -> FilePage {
    let mut depth_of: Vec<(String, usize)> = Vec::new();
    let definitions = reply
        .definitions
        .iter()
        .map(|d| {
            let depth = d
                .parent
                .as_ref()
                .and_then(|p| depth_of.iter().find(|(id, _)| id == p))
                .map_or(0, |(_, depth)| depth + 1);
            let node = node_view(d.node.as_ref());
            depth_of.push((node.id.clone(), depth));
            DefinitionRow {
                node,
                kind: d.kind.clone(),
                start: d.start_line,
                end: d.end_line,
                depth,
            }
        })
        .collect();
    FilePage {
        title: reply.path.clone(),
        base: base.to_owned(),
        snapshot: reply.snapshot.clone(),
        crumbs: crumbs(&reply.path),
        blob: reply.blob.clone(),
        size: reply.size,
        binary: reply.binary,
        definitions,
        lines: reply
            .text
            .lines()
            .enumerate()
            .map(|(i, text)| SourceLine {
                number: i + 1,
                text: text.to_owned(),
            })
            .collect(),
    }
}

/// One neighbour drawn in the graph view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// NodeId (its graph page).
    pub id: String,
    /// Label, cut to fit.
    pub label: String,
    /// Full name and file, for the tooltip.
    pub title: String,
    /// Label position.
    pub x: u32,
    /// Label baseline.
    pub y: u32,
    /// `start` or `end`: which way the label runs from `x`.
    pub anchor: &'static str,
    /// Edge endpoint on the label's side.
    pub edge_x: u32,
    /// CSS class of the edge: `ref`, `test`, or `contains`.
    pub class: &'static str,
}

/// One labeled group of neighbours, listed below the drawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeGroup {
    /// `references`, `referenced by`, …
    pub label: &'static str,
    /// The neighbours.
    pub nodes: Vec<NodeView>,
}

/// View 6: a definition's neighbourhood.
#[derive(Template, Debug)]
#[template(path = "graph.html")]
pub struct GraphPage {
    /// Page title.
    pub title: String,
    /// URL prefix of this repository's pages.
    pub base: String,
    /// The snapshot.
    pub snapshot: String,
    /// The definition.
    pub node: NodeView,
    /// Its adapter kind.
    pub kind: String,
    /// SVG width.
    pub width: u32,
    /// SVG height.
    pub height: u32,
    /// Vertical centre of the definition's box.
    pub centre: u32,
    /// The definition's label, cut to fit.
    pub centre_label: String,
    /// Neighbours drawn.
    pub drawn: Vec<GraphNode>,
    /// Neighbours not drawn (more than [`GRAPH_SIDE`] on a side).
    pub hidden: usize,
    /// Every neighbour, by edge.
    pub groups: Vec<EdgeGroup>,
    /// Tests coverage names that map to no definition.
    pub unmapped: Vec<String>,
}

const WIDTH: u32 = 760;
const ROW: u32 = 26;
const LEFT_EDGE: u32 = 250;
const RIGHT_EDGE: u32 = 510;

fn cut(label: &str) -> String {
    if label.chars().count() <= GRAPH_LABEL {
        label.to_owned()
    } else {
        let head: String = label.chars().take(GRAPH_LABEL - 1).collect();
        format!("{head}…")
    }
}

/// The graph page for a `NodeEdges` reply: what names it and the tests
/// that exercise it on the left, what it names, tests, and contains on
/// the right.
#[must_use]
pub fn graph_page(base: &str, reply: &proto::NodeEdgesResponse) -> GraphPage {
    let node = node_view(reply.node.as_ref());
    let left: Vec<(&proto::NodeRef, &'static str)> = reply
        .referenced_by
        .iter()
        .map(|n| (n, "ref"))
        .chain(reply.tested_by.iter().map(|n| (n, "test")))
        .collect();
    let right: Vec<(&proto::NodeRef, &'static str)> = reply
        .references
        .iter()
        .map(|n| (n, "ref"))
        .chain(reply.tests.iter().map(|n| (n, "test")))
        .chain(reply.contains.iter().map(|n| (n, "contains")))
        .collect();
    let rows = left
        .len()
        .min(GRAPH_SIDE)
        .max(right.len().min(GRAPH_SIDE))
        .max(1) as u32;
    let height = rows * ROW + 20;
    let mut drawn = Vec::new();
    let mut place = |side: &[(&proto::NodeRef, &'static str)], left: bool| {
        let shown = side.len().min(GRAPH_SIDE) as u32;
        let top = (height - shown * ROW) / 2 + ROW / 2 + 4;
        for (i, (n, class)) in side.iter().take(GRAPH_SIDE).enumerate() {
            let view = NodeView::from(*n);
            drawn.push(GraphNode {
                title: match &view.path {
                    Some(p) => format!("{} ({p})", view.label),
                    None => view.label.clone(),
                },
                label: cut(&view.label),
                id: view.id,
                x: if left { LEFT_EDGE - 6 } else { RIGHT_EDGE + 6 },
                y: top + i as u32 * ROW,
                anchor: if left { "end" } else { "start" },
                edge_x: if left { LEFT_EDGE } else { RIGHT_EDGE },
                class,
            });
        }
    };
    place(&left, true);
    place(&right, false);
    let hidden = left.len().saturating_sub(GRAPH_SIDE) + right.len().saturating_sub(GRAPH_SIDE);
    let group = |label, nodes: &[proto::NodeRef]| EdgeGroup {
        label,
        nodes: nodes.iter().map(NodeView::from).collect(),
    };
    GraphPage {
        title: format!("Graph: {}", node.label),
        base: base.to_owned(),
        snapshot: reply.snapshot.clone(),
        kind: reply.kind.clone().unwrap_or_default(),
        width: WIDTH,
        height,
        centre: height / 2,
        centre_label: cut(&node.label),
        node,
        drawn,
        hidden,
        groups: vec![
            group("references", &reply.references),
            group("referenced by", &reply.referenced_by),
            group("tested by", &reply.tested_by),
            group("tests", &reply.tests),
            group("contains", &reply.contains),
        ],
        unmapped: reply.covered_by_tests.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, path: &str) -> proto::NodeRef {
        proto::NodeRef {
            id: id.into(),
            name: Some(name.into()),
            path: Some(path.into()),
        }
    }

    #[test]
    fn a_lineage_entry_says_it_was_renamed_and_moved() {
        let entry = proto::LineageEntry {
            change: Some(proto::ChangeSummary {
                change: "c0ffee0123456789".into(),
                summary: "Move parse to util".into(),
                ..Default::default()
            }),
            before: Some(node("n", "parse", "src/lib.rs")),
            after: Some(node("n", "parse_all", "src/util.rs")),
            ..Default::default()
        };
        let row = lineage_row(&entry);
        assert_eq!(
            row.what,
            [
                "renamed parse → parse_all",
                "moved src/lib.rs → src/util.rs"
            ]
        );
        assert_eq!(row.after.as_deref(), Some("parse_all in src/util.rs"));
        assert_eq!(row.file.as_deref(), Some("src/util.rs"));
    }

    #[test]
    fn a_split_links_to_its_parts() {
        let entry = proto::LineageEntry {
            before: Some(node("n", "big", "src/lib.rs")),
            identity: vec![proto::IdentityDeltaView {
                kind: "split_into".into(),
                node: Some(node("n", "big", "src/lib.rs")),
                others: vec![
                    node("a", "left", "src/lib.rs"),
                    node("b", "right", "src/lib.rs"),
                ],
                text: "big split into left, right".into(),
            }],
            ..Default::default()
        };
        let row = lineage_row(&entry);
        assert_eq!(row.what[0], "big split into left, right");
        let related: Vec<&str> = row.related.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(related, ["left", "right"]);
    }

    #[test]
    fn a_parked_step_says_why_in_plain_words() {
        let step = proto::TraceStep {
            at_ms: 2_500,
            stage: proto::TraceStage::Parked.into(),
            park: Some(proto::ParkReason::MergeConflict.into()),
            text: "src/lib.rs".into(),
            outcome: Some("fail".into()),
            ..Default::default()
        };
        let view = step_view(&step, 1_000);
        assert_eq!(view.at, "+1.5s");
        assert_eq!(view.stage, "parked");
        assert!(
            view.text.contains("collide with a landed change"),
            "{}",
            view.text
        );
        assert!(view.text.ends_with("(src/lib.rs)"));
        assert_eq!(view.outcome, "fail");
    }

    #[test]
    fn crumbs_walk_the_path() {
        let c = crumbs("src/a/b.rs");
        assert_eq!(c.len(), 3);
        assert_eq!(c[1].path, "src/a");
        assert!(crumbs("").is_empty());
    }

    #[test]
    fn the_graph_draws_both_sides_and_caps_each() -> askama::Result<()> {
        let many: Vec<proto::NodeRef> = (0..20)
            .map(|i| node(&format!("r{i}"), &format!("caller_{i}"), "src/lib.rs"))
            .collect();
        let reply = proto::NodeEdgesResponse {
            snapshot: "s".into(),
            node: Some(node("n", "parse", "src/lib.rs")),
            kind: Some("function_item".into()),
            referenced_by: many,
            references: vec![node("t", "Token", "src/lib.rs")],
            tested_by: vec![node("x", "tests::parses", "src/lib.rs")],
            ..Default::default()
        };
        let page = graph_page("", &reply);
        assert_eq!(page.drawn.len(), GRAPH_SIDE + 1);
        assert_eq!(page.hidden, 21 - GRAPH_SIDE);
        assert!(page.drawn.iter().all(|n| n.y < page.height));
        let html = page.render()?;
        assert!(html.contains("<svg"), "{html}");
        assert!(html.contains("/graph/s/t"), "{html}");
        assert!(html.contains("/nodes/n"), "{html}");
        assert!(html.contains("tests::parses"), "{html}");
        Ok(())
    }
}
