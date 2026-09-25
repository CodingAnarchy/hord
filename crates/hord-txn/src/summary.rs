//! The machine-generated conflict summary of spec §6.4 rung 3: which nodes,
//! which intents collided, and what each side changed. The replay harness
//! gets it in its request (§6.6), and the arbiter sees it with the parked
//! change (`hord conflicts`, the arbitration workbench).

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use hord_core::{Actor, ChangeId, ChangeRecord, NodeId, Op, RepoPath, TreeOpKind};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::conflict::{ConflictReport, MergeSeverity};
use crate::repo::Inner;

/// Lines of a side's changes kept in a summary; the rest are counted.
const MAX_CHANGED_LINES: usize = 24;

/// A contested definition, with its name and file where known.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SummaryNode {
    /// The definition.
    pub node: NodeId,
    /// Its qualified name, or `(file)` for a file root.
    pub name: Option<String>,
    /// The file it is in.
    pub path: Option<String>,
}

impl SummaryNode {
    /// The name, else the id.
    fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => self.node.to_string(),
        }
    }
}

/// One side of a collision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictSide {
    /// The change.
    pub change: ChangeId,
    /// Its intent summary.
    pub intent: String,
    /// Its author.
    pub actor: Actor,
    /// `true` for a landed change ("ours"), `false` for the parked change
    /// ("theirs").
    pub landed: bool,
    /// What it did to the contested definitions and files, one line each.
    pub changed: Vec<String>,
}

/// Which nodes, which intents collided, and what each side changed (spec
/// §6.4 rung 3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictSummary {
    /// The parked change.
    pub change: ChangeId,
    /// Contested definitions.
    pub nodes: Vec<SummaryNode>,
    /// Contested files.
    pub paths: Vec<RepoPath>,
    /// The parked change first, then each landed change it collided with.
    pub sides: Vec<ConflictSide>,
    /// Why it could not land: hard merge conflicts and the verification
    /// failure.
    pub reasons: Vec<String>,
    /// All of the above as text, for a prompt or a terminal.
    pub text: String,
}

/// Names of definitions: `(qualified name, file)`.
pub(crate) type NodeNames = HashMap<NodeId, (Option<String>, String)>;

impl Inner {
    /// Names of the definitions in every file `records` touched, at their
    /// base and result, and of those files' roots.
    pub(crate) fn node_names(&self, records: &[&ChangeRecord]) -> Result<NodeNames> {
        let mut known = NodeNames::new();
        for record in records {
            let paths: Vec<RepoPath> = self
                .changed_paths(record.base, record.result)?
                .into_iter()
                .map(|d| d.path)
                .collect();
            for path in &paths {
                known
                    .entry(NodeId::file_root(path))
                    .or_insert_with(|| (Some("(file)".into()), path.to_string()));
            }
            for snapshot in [record.base, record.result] {
                for path in &paths {
                    for def in self.definitions_at(snapshot, path)? {
                        known.entry(def.node).or_insert_with(|| {
                            (
                                def.name.map(|n| n.as_str().to_owned()),
                                def.path.to_string(),
                            )
                        });
                    }
                }
            }
        }
        Ok(known)
    }

    /// The landed changes `report`'s change collided with: those its sets
    /// overlapped (spec §6.3), else, for a merge conflict or a semantic
    /// conflict with no overlap, every change landed since its base.
    pub(crate) fn colliders(report: &ConflictReport) -> Vec<ChangeId> {
        let mut out = Vec::new();
        for conflict in &report.conflicts {
            if !out.contains(&conflict.landed) {
                out.push(conflict.landed);
            }
        }
        if out.is_empty() {
            out = report.checked_against.clone();
        }
        out
    }

    /// The conflict summary of the parked `change` from its `report`.
    pub(crate) fn conflict_summary(
        &self,
        change: ChangeId,
        report: &ConflictReport,
    ) -> Result<ConflictSummary> {
        let theirs = self.change_record(change)?;
        let mut landed = Vec::new();
        for id in Self::colliders(report) {
            // A collider that is not stored (a damaged store) is left out.
            if let Ok(record) = self.change_record(id) {
                landed.push((id, record));
            }
        }
        let mut records = vec![&theirs];
        records.extend(landed.iter().map(|(_, r)| r));
        let names = self.node_names(&records)?;

        let contested: BTreeSet<NodeId> = report.nodes();
        let mut paths: BTreeSet<RepoPath> = report
            .conflicts
            .iter()
            .flat_map(|c| c.paths.iter().cloned())
            .collect();
        paths.extend(
            report
                .merge
                .iter()
                .filter(|m| m.severity == MergeSeverity::Hard)
                .map(|m| m.path.clone()),
        );
        let nodes: Vec<SummaryNode> = contested
            .iter()
            .map(|node| {
                let (name, path) = names.get(node).cloned().unzip();
                SummaryNode {
                    node: *node,
                    name: name.flatten(),
                    path,
                }
            })
            .collect();
        let contested_paths: BTreeSet<String> = paths
            .iter()
            .map(ToString::to_string)
            .chain(nodes.iter().filter_map(|n| n.path.clone()))
            .collect();

        let side = |id: ChangeId, record: &ChangeRecord, is_landed: bool| ConflictSide {
            change: id,
            intent: record.intent.summary.clone(),
            actor: record.provenance.actor.clone(),
            landed: is_landed,
            changed: changed_lines(record, &names, &contested, &contested_paths),
        };
        let mut sides = vec![side(change, &theirs, false)];
        sides.extend(landed.iter().map(|(id, record)| side(*id, record, true)));

        let mut reasons: Vec<String> = report
            .merge
            .iter()
            .filter(|m| m.severity == MergeSeverity::Hard)
            .map(|m| format!("hard merge conflict in {}: {}", m.path, m.reason))
            .collect();
        if let Some(why) = &report.verification {
            reasons.push(format!("verification failed after a clean rebase: {why}"));
        }
        let text = render(&nodes, &paths, &sides, &reasons);
        Ok(ConflictSummary {
            change,
            nodes,
            paths: paths.into_iter().collect(),
            sides,
            reasons,
            text,
        })
    }
}

/// What `record` did, one line per op, restricted to the contested nodes
/// and files when it touched any of them.
fn changed_lines(
    record: &ChangeRecord,
    names: &NodeNames,
    contested: &BTreeSet<NodeId>,
    contested_paths: &BTreeSet<String>,
) -> Vec<String> {
    let described: Vec<(bool, String)> = record
        .ops
        .iter()
        .map(|op| {
            let (node, path) = op_subject(op);
            let file = node
                .and_then(|n| names.get(&n))
                .map(|(_, file)| file.clone())
                .or_else(|| path.map(ToString::to_string));
            let hit = node.is_some_and(|n| contested.contains(&n))
                || file.as_ref().is_some_and(|f| contested_paths.contains(f));
            (hit, describe(op, names))
        })
        .collect();
    let any_hit = described.iter().any(|(hit, _)| *hit);
    let mut lines: Vec<String> = described
        .into_iter()
        .filter(|(hit, _)| *hit || !any_hit)
        .map(|(_, line)| line)
        .collect();
    lines.dedup();
    if lines.len() > MAX_CHANGED_LINES {
        let more = lines.len() - MAX_CHANGED_LINES;
        lines.truncate(MAX_CHANGED_LINES);
        lines.push(format!("… and {more} more"));
    }
    lines
}

/// The definition or file an op is about.
fn op_subject(op: &Op) -> (Option<NodeId>, Option<&RepoPath>) {
    match op {
        Op::Insert { parent, .. } => (Some(*parent), None),
        Op::Delete { node }
        | Op::Replace { node, .. }
        | Op::Move { node, .. }
        | Op::Rename { node, .. } => (Some(*node), None),
        Op::Blob { path, .. } | Op::Tree { path, .. } => (None, Some(path)),
    }
}

/// `node`'s name and file, else its id.
fn named(node: NodeId, names: &NodeNames) -> String {
    match names.get(&node) {
        Some((Some(name), file)) if name != "(file)" => format!("{name} ({file})"),
        Some((_, file)) => format!("file {file}"),
        None => node.to_string(),
    }
}

fn describe(op: &Op, names: &NodeNames) -> String {
    match op {
        Op::Insert { parent, .. } => format!("added a definition in {}", named(*parent, names)),
        Op::Delete { node } => format!("deleted {}", named(*node, names)),
        Op::Replace { node, .. } => format!("rewrote {}", named(*node, names)),
        Op::Move { node, .. } => format!("moved {}", named(*node, names)),
        Op::Rename { node, from, to } => format!(
            "renamed {} to {} ({})",
            from.as_str(),
            to.as_str(),
            named(*node, names)
        ),
        Op::Blob { path, from, to } => match (from, to) {
            (None, Some(_)) => format!("created file {path}"),
            (Some(_), None) => format!("deleted file {path}"),
            _ => format!("edited file {path}"),
        },
        Op::Tree { path, kind } => match kind {
            TreeOpKind::CreateFile => format!("created file {path}"),
            TreeOpKind::CreateDir => format!("created directory {path}"),
            TreeOpKind::Delete => format!("deleted {path}"),
            TreeOpKind::Rename { to } => format!("renamed {path} to {to}"),
        },
    }
}

fn render(
    nodes: &[SummaryNode],
    paths: &BTreeSet<RepoPath>,
    sides: &[ConflictSide],
    reasons: &[String],
) -> String {
    let mut text = String::new();
    if let Some(theirs) = sides.first() {
        let _ = writeln!(
            text,
            "Parked change {} \"{}\" by {}",
            theirs.change,
            theirs.intent,
            theirs.actor.id()
        );
    }
    let landed: Vec<&ConflictSide> = sides.iter().filter(|s| s.landed).collect();
    let _ = writeln!(
        text,
        "collided with {} landed change{}.",
        landed.len(),
        if landed.len() == 1 { "" } else { "s" }
    );
    if !nodes.is_empty() {
        let list: Vec<String> = nodes.iter().map(SummaryNode::label).collect();
        let _ = writeln!(text, "Contested definitions: {}", list.join(", "));
    }
    if !paths.is_empty() {
        let list: Vec<String> = paths.iter().map(ToString::to_string).collect();
        let _ = writeln!(text, "Contested files: {}", list.join(", "));
    }
    for reason in reasons {
        let _ = writeln!(text, "Why: {reason}");
    }
    for side in sides {
        let which = if side.landed {
            "Landed (ours)"
        } else {
            "Parked (theirs)"
        };
        let _ = writeln!(
            text,
            "\n{which} {} \"{}\" by {}:",
            side.change,
            side.intent,
            side.actor.id()
        );
        for line in &side.changed {
            let _ = writeln!(text, "  - {line}");
        }
    }
    text
}
