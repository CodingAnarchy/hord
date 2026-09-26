//! The machine-generated conflict summary of spec §6.4 rung 3: which nodes,
//! which intents collided, and what each side changed. The replay harness
//! gets it in its request (§6.6), and the arbiter sees it with the parked
//! change (`hord conflicts`, the arbitration workbench).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use hord_core::{Actor, ChangeId, ChangeRecord, NodeId, ObjectId, RepoPath, SnapshotId};
use hord_lang::{IdentifiedTree, NodeTree};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::conflict::{ConflictReport, MergeConflict, MergeSeverity};
use crate::repo::Inner;

/// Lines of a side's changes kept in a summary; the rest are counted.
const MAX_CHANGED_LINES: usize = 24;
/// Characters of an id shown in prose.
const SHORT_ID: usize = 12;
/// Longest definition, on one line, quoted inline.
const MAX_QUOTED: usize = 90;
/// Assertions quoted per test; the rest are counted.
const MAX_ASSERTIONS: usize = 3;

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
    /// `name (file)`, the file for a file root, else the short id.
    fn label(&self) -> String {
        match (&self.name, &self.path) {
            (Some(name), Some(path)) if name != "(file)" => format!("{name} ({path})"),
            (_, Some(path)) => path.clone(),
            (Some(name), None) => name.clone(),
            (None, None) => short(&self.node.to_string()).to_owned(),
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
    /// What it changed, one line each: definitions first (contested ones
    /// leading, tests by name with what they assert), then files.
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
    /// Why it could not land, in plain words: each hard merge conflict and
    /// each failing test with what it asserts. The report keeps the raw
    /// reasons, evidence ids included.
    pub reasons: Vec<String>,
    /// All of the above as text, for a prompt, a terminal, or a reviewer
    /// reading it cold: ids cut to 12 characters.
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

        let mut sides = Vec::new();
        let mut tests = Vec::new();
        let all = std::iter::once((change, &theirs, false))
            .chain(landed.iter().map(|(id, record)| (*id, record, true)));
        for (id, record, is_landed) in all {
            let (changed, defs) = self.side_changes(record, &contested)?;
            tests.extend(
                defs.into_values()
                    .filter(|d| d.asserts.is_some())
                    .map(|d| (d, is_landed)),
            );
            sides.push(ConflictSide {
                change: id,
                intent: record.intent.summary.clone(),
                actor: record.provenance.actor.clone(),
                landed: is_landed,
                changed,
            });
        }

        let mut reasons: Vec<String> = report
            .merge
            .iter()
            .filter(|m| m.severity == MergeSeverity::Hard)
            .map(|m| hard_reason(m, &names))
            .collect();
        if let Some(why) = &report.verification {
            reasons.extend(verification_reasons(why, &tests));
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

    /// The definitions of `path` in `snapshot`, with their text, and what
    /// each test asserts.
    fn defs_at(&self, snapshot: SnapshotId, path: &RepoPath) -> Result<HashMap<NodeId, Def>> {
        let mut out = HashMap::new();
        let Some(bytes) = self.file_bytes(snapshot, path)? else {
            return Ok(out);
        };
        let parsed = self.parsed_at(snapshot, path)?;
        for def in self.definitions_at(snapshot, path)? {
            let text = bytes
                .as_slice()
                .get(def.span.clone())
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let kind = def.kind.as_str().to_owned();
            let asserts = is_test(&kind, &text).then(|| {
                parsed
                    .as_ref()
                    .map(|p| assertions(&p.tree, def.node))
                    .unwrap_or_default()
            });
            out.insert(
                def.node,
                Def {
                    name: def.name.map(|n| n.as_str().to_owned()),
                    kind,
                    path: def.path,
                    start: def.span.start,
                    parent: def.parent,
                    text,
                    asserts,
                },
            );
        }
        Ok(out)
    }

    /// What `record` changed, one line each: definitions first (the
    /// contested ones leading, tests by name with what they assert), then
    /// the files no definition line covers. Also its definitions after the
    /// change, by id.
    fn side_changes(
        &self,
        record: &ChangeRecord,
        contested: &BTreeSet<NodeId>,
    ) -> Result<(Vec<String>, HashMap<NodeId, Def>)> {
        let deltas = self.changed_paths(record.base, record.result)?;
        let mut before = HashMap::new();
        let mut after = HashMap::new();
        for delta in &deltas {
            before.extend(self.defs_at(record.base, &delta.path)?);
            after.extend(self.defs_at(record.result, &delta.path)?);
        }
        let mut edits: Vec<(NodeId, Edit)> = Vec::new();
        for (id, now) in &after {
            match before.get(id) {
                None => edits.push((*id, Edit::Added)),
                Some(was) if was.path != now.path => edits.push((*id, Edit::Moved)),
                Some(was) if squash(&was.text) != squash(&now.text) => {
                    edits.push((*id, Edit::Changed));
                }
                Some(_) => {}
            }
        }
        edits.extend(
            before
                .keys()
                .filter(|id| !after.contains_key(id))
                .map(|id| (*id, Edit::Removed)),
        );
        // A definition that changed only because something inside it did
        // is left to that inner line.
        let mut enclosing = HashSet::new();
        for (id, edit) in &edits {
            let defs = if *edit == Edit::Removed {
                &before
            } else {
                &after
            };
            let mut parent = defs.get(id).and_then(|d| d.parent);
            while let Some(p) = parent {
                if !enclosing.insert(p) {
                    break;
                }
                parent = defs.get(&p).and_then(|d| d.parent);
            }
        }
        edits.retain(|(id, _)| !enclosing.contains(id));
        let def_of = |id: &NodeId, edit: &Edit| {
            if *edit == Edit::Removed {
                before.get(id)
            } else {
                after.get(id)
            }
        };
        edits.sort_by_key(|(id, edit)| {
            let def = def_of(id, edit);
            (
                !contested.contains(id),
                def.map(|d| d.path.clone()),
                def.map_or(0, |d| d.start),
            )
        });

        let created: HashSet<&RepoPath> = deltas
            .iter()
            .filter(|d| d.from.is_none())
            .map(|d| &d.path)
            .collect();
        let deleted: HashSet<&RepoPath> = deltas
            .iter()
            .filter(|d| d.to.is_none())
            .map(|d| &d.path)
            .collect();
        let mut covered: HashSet<&RepoPath> = HashSet::new();
        let mut lines = Vec::new();
        for (id, edit) in &edits {
            let Some(def) = def_of(id, edit) else {
                continue;
            };
            covered.insert(&def.path);
            let place = if created.contains(&def.path) {
                format!("{}, new file", def.path)
            } else if deleted.contains(&def.path) {
                format!("{}, file deleted", def.path)
            } else {
                def.path.to_string()
            };
            let what = def.label();
            lines.push(match edit {
                Edit::Added => match &def.asserts {
                    Some(asserts) => {
                        format!("added {what} ({place}){}", asserts_clause(asserts, ""))
                    }
                    None => match quoted(&def.text) {
                        Some(text) => format!("added {what} ({place}): {text}"),
                        None => format!("added {what} ({place})"),
                    },
                },
                Edit::Removed => format!("removed {what} ({place})"),
                Edit::Moved => {
                    let from = before.get(id).map(|d| d.path.to_string());
                    covered.extend(before.get(id).map(|d| &d.path));
                    format!("moved {what} from {} to {place}", from.unwrap_or_default())
                }
                Edit::Changed => {
                    let was = before.get(id);
                    let renamed = was
                        .and_then(|w| w.name.as_ref())
                        .filter(|old| Some(*old) != def.name.as_ref());
                    let what = match renamed {
                        Some(old) => format!("{what} (renamed from {old})"),
                        None => what,
                    };
                    match (
                        &def.asserts,
                        was.and_then(|w| quoted(&w.text)),
                        quoted(&def.text),
                    ) {
                        (Some(asserts), _, _) => {
                            format!(
                                "changed {what} ({place}){}",
                                asserts_clause(asserts, "now ")
                            )
                        }
                        (None, Some(old), Some(new)) if old == new => {
                            format!("changed only comments or layout of {what} ({place})")
                        }
                        (None, Some(old), Some(new)) => {
                            format!("changed {what} ({place}): now {new}, was {old}")
                        }
                        (None, _, _) => format!("changed {what} ({place})"),
                    }
                }
            });
        }
        for delta in &deltas {
            if covered.contains(&delta.path) {
                continue;
            }
            lines.push(match (delta.from, delta.to) {
                (None, _) => format!("created {}", delta.path),
                (_, None) => format!("deleted {}", delta.path),
                _ => format!("edited {}", delta.path),
            });
        }
        lines.dedup();
        if lines.len() > MAX_CHANGED_LINES {
            let more = lines.len() - MAX_CHANGED_LINES;
            lines.truncate(MAX_CHANGED_LINES);
            lines.push(format!("… and {more} more"));
        }
        Ok((lines, after))
    }
}

/// How a change touched a definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edit {
    Added,
    Removed,
    Changed,
    Moved,
}

/// A definition on one side of a change.
#[derive(Clone, Debug)]
struct Def {
    name: Option<String>,
    /// Adapter kind, e.g. `function_item`.
    kind: String,
    path: RepoPath,
    /// Where it starts in its file, for ordering.
    start: usize,
    parent: Option<NodeId>,
    /// Its source, attached comments and attributes included.
    text: String,
    /// For a test, what it asserts; `None` for anything else.
    asserts: Option<Vec<String>>,
}

impl Def {
    /// `test name`, `fn name`, `const NAME`, or the bare name or kind.
    fn label(&self) -> String {
        let kind = if self.asserts.is_some() {
            "test"
        } else {
            kind_word(&self.kind)
        };
        match (kind, &self.name) {
            ("", Some(name)) => name.clone(),
            (kind, Some(name)) => format!("{kind} {name}"),
            ("", None) => self.kind.clone(),
            (kind, None) => format!("an {kind}"),
        }
    }
}

/// The Rust keyword for an adapter kind; empty when there is none.
fn kind_word(kind: &str) -> &'static str {
    match kind {
        "function_item" | "function_signature_item" => "fn",
        "const_item" => "const",
        "static_item" => "static",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        "trait_item" => "trait",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "type_item" => "type",
        "macro_definition" => "macro",
        "enum_variant" => "variant",
        "field_declaration" => "field",
        _ => "",
    }
}

/// Whether a definition is a test: a function with a `#[test]` (or
/// `#[<runtime>::test]`) attribute.
fn is_test(kind: &str, text: &str) -> bool {
    kind == "function_item"
        && text.lines().map(str::trim).any(|line| {
            line.starts_with("#[test") || (line.starts_with("#[") && line.contains("::test"))
        })
}

/// `: asserts a and b`, or nothing when a test asserts nothing.
fn asserts_clause(asserts: &[String], now: &str) -> String {
    if asserts.is_empty() {
        return String::new();
    }
    let mut shown: Vec<&str> = asserts
        .iter()
        .take(MAX_ASSERTIONS)
        .map(String::as_str)
        .collect();
    let more = asserts.len() - shown.len();
    let tail = format!("{more} more");
    if more > 0 {
        shown.push(&tail);
    }
    format!(": {now}asserts {}", and_list(&shown))
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// A definition's code on one line, without its comments, in backticks,
/// when it is short enough to read inline.
fn quoted(text: &str) -> Option<String> {
    let code: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    let code = squash(&code.join("\n"));
    (!code.is_empty() && code.chars().count() <= MAX_QUOTED).then(|| format!("`{code}`"))
}

/// `text` with every run of whitespace made one space, trimmed.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What the test `node` asserts, in words, from its syntax tree: each
/// `assert_eq!`, `assert_ne!`, and `assert!` in it, in order.
fn assertions(tree: &IdentifiedTree, node: NodeId) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(root) = tree.site_of(node).and_then(|site| tree.oid_at(site)) {
        collect_assertions(&tree.tree, root, &mut out);
    }
    out
}

fn collect_assertions(tree: &NodeTree, id: ObjectId, out: &mut Vec<String>) {
    let Some(node) = tree.get(id) else {
        return;
    };
    if node.kind.as_str() == "macro_invocation"
        && let Some(assertion) = assertion(tree, &node.children)
    {
        out.push(assertion);
        return;
    }
    for child in &node.children {
        collect_assertions(tree, *child, out);
    }
}

/// An assertion macro's meaning: `` `a` equals `b` `` for `assert_eq!(a,
/// b)`, and so on. `None` for a macro that is not an assertion.
fn assertion(tree: &NodeTree, children: &[ObjectId]) -> Option<String> {
    let (name, rest) = children.split_first()?;
    let name = squash(&source(tree, *name));
    let args = rest
        .iter()
        .find_map(|c| tree.get(*c).filter(|n| n.kind.as_str() == "token_tree"))?;
    // The token tree's first and last children are its delimiters.
    let inner = args.children.get(1..args.children.len().checked_sub(1)?)?;
    let mut parts = vec![String::new()];
    for child in inner {
        if tree.get(*child).is_some_and(|n| n.kind.as_str() == ",") {
            parts.push(String::new());
        } else if let Some(part) = parts.last_mut() {
            part.push_str(&source(tree, *child));
        }
    }
    let parts: Vec<String> = parts
        .iter()
        .map(|p| squash(p))
        .filter(|p| !p.is_empty())
        .collect();
    match (name.as_str(), parts.as_slice()) {
        ("assert_eq" | "debug_assert_eq", [a, b, ..]) => Some(format!("`{a}` equals `{b}`")),
        ("assert_ne" | "debug_assert_ne", [a, b, ..]) => {
            Some(format!("`{a}` does not equal `{b}`"))
        }
        ("assert" | "debug_assert", [condition, ..]) => Some(format!("`{condition}` is true")),
        (other, _) if other.contains("assert") => Some(format!("`{other}!({})`", parts.join(", "))),
        _ => None,
    }
}

/// The source text of the subtree `id`: its leaves' bytes in order.
fn source(tree: &NodeTree, id: ObjectId) -> String {
    fn leaves(tree: &NodeTree, id: ObjectId, out: &mut Vec<u8>) {
        let Some(node) = tree.get(id) else {
            return;
        };
        if node.children.is_empty() {
            out.extend_from_slice(node.raw.as_slice());
        }
        for child in &node.children {
            leaves(tree, *child, out);
        }
    }
    let mut out = Vec::new();
    leaves(tree, id, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `node`'s name and file, else its short id.
fn named(node: NodeId, names: &NodeNames) -> String {
    match names.get(&node) {
        Some((Some(name), file)) if name != "(file)" => format!("{name} ({file})"),
        Some((_, file)) => file.clone(),
        None => short(&node.to_string()).to_owned(),
    }
}

/// A hard merge conflict in plain words.
fn hard_reason(merge: &MergeConflict, names: &NodeNames) -> String {
    let what = if merge.nodes.is_empty() {
        merge.path.to_string()
    } else {
        let named: Vec<String> = merge.nodes.iter().map(|n| named(*n, names)).collect();
        and_list(&named.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let reason = merge.reason.as_str();
    let path = &merge.path;
    if reason.starts_with("both sides replaced this definition") {
        format!("Both changes rewrote {what}, and the two rewrites do not combine.")
    } else if reason.starts_with("Delete vs any other op") {
        format!("One change deleted {what} and the other changed it.")
    } else if reason.starts_with("one side moved this definition") {
        format!("One change moved {what} and the other edited it where it was.")
    } else if reason.starts_with("blob 3-way line merge conflict") {
        format!("Both changes edited the same lines of {path}.")
    } else if reason.starts_with("binary blob conflict") {
        format!("Both changes changed the binary file {path}.")
    } else if reason.starts_with("merged file does not parse") {
        format!("The two changes' edits to {path} merge into code that does not parse.")
    } else {
        format!("{what} does not merge: {}.", without_citations(reason))
    }
}

/// `text` without its `(spec §…)` and `(ADR …)` citations.
fn without_citations(text: &str) -> String {
    let mut out = text.to_owned();
    for opener in [" (spec §", " (ADR "] {
        while let Some(start) = out.find(opener) {
            let Some(len) = out[start..].find(')') else {
                break;
            };
            out.replace_range(start..=start + len, "");
        }
    }
    out.trim_end_matches('.').to_owned()
}

/// One failure a verification reason names.
enum Failure {
    /// A test, by name.
    Test(String),
    /// Anything else, in words.
    Other(String),
}

/// The failures in a verification reason: the lander's (one line per
/// failed check, `<command>: <n> failed: <names>`, `build failed: …`) or
/// the pinned acceptance run's (ADR 0034).
fn failures(verification: &str) -> Vec<Failure> {
    let mut out = Vec::new();
    for line in verification
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        if let Some(rest) = line.strip_prefix("protected acceptance tests fail (ADR 0034): ") {
            out.extend(
                rest.split("; ")
                    .filter_map(|part| part.split_whitespace().next())
                    .map(|name| Failure::Test(name.to_owned())),
            );
        } else if let Some(names) = failed_tests(line) {
            out.extend(names.into_iter().map(Failure::Test));
        } else if let Some((_, error)) = line.split_once("build failed: ") {
            out.push(Failure::Other(format!(
                "The merged code does not build: {error}"
            )));
        } else {
            out.push(Failure::Other(format!(
                "Verification failed: {}",
                short_ids(without_command(line))
            )));
        }
    }
    out
}

/// The test names of `… <n> failed: a, b and 3 more`.
fn failed_tests(line: &str) -> Option<Vec<String>> {
    let (head, names) = line.rsplit_once(" failed: ")?;
    head.rsplit(|c: char| c.is_whitespace() || c == ':')
        .next()?
        .parse::<usize>()
        .ok()?;
    let names = match names.rsplit_once(" and ") {
        Some((names, more)) if more.ends_with(" more") => names,
        _ => names,
    };
    Some(
        names
            .split(", ")
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// A check's result without the check's command line (`<program> <args
/// with evidence ids>: <result>`).
fn without_command(line: &str) -> &str {
    match line.split_once(": ") {
        Some((command, rest)) if command.ends_with("(reused)") || long_hex(command) => rest,
        _ => line,
    }
}

/// Whether `text` holds an object id (a run of at least 40 hex digits).
fn long_hex(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .any(|run| run.len() >= 40)
}

/// `text` with every object id cut to its first 12 characters.
fn short_ids(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_hexdigit() {
            run.push(c);
            continue;
        }
        out.push_str(if run.len() >= 40 { short(&run) } else { &run });
        run.clear();
        out.push(c);
    }
    out.pop();
    out
}

/// Why verification failed, in words: each failing test with where it
/// is, which side brought it, and what it asserts (from `tests`, each
/// side's tests with whether that side landed).
fn verification_reasons(verification: &str, tests: &[(Def, bool)]) -> Vec<String> {
    let failures = failures(verification);
    let mut out = Vec::new();
    if !verification.starts_with("protected acceptance tests fail") {
        out.push("The changes merge cleanly, but the merged code fails verification.".to_owned());
    }
    for failure in failures {
        match failure {
            Failure::Other(text) => out.push(text),
            Failure::Test(name) => {
                let leaf = name.rsplit("::").next().unwrap_or(&name);
                let found = tests.iter().find(|(def, _)| {
                    def.name
                        .as_deref()
                        .is_some_and(|n| n.rsplit("::").next() == Some(leaf))
                });
                out.push(match found {
                    Some((def, landed)) => format!(
                        "Test {name} fails ({}, from the {} change){}.",
                        def.path,
                        if *landed { "landed" } else { "parked" },
                        def.asserts
                            .as_deref()
                            .map(|a| asserts_clause(a, "it "))
                            .unwrap_or_default()
                    ),
                    None => format!("Test {name} fails."),
                });
            }
        }
    }
    out
}

/// The first 12 characters of an id, for prose.
fn short(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

fn render(
    nodes: &[SummaryNode],
    paths: &BTreeSet<RepoPath>,
    sides: &[ConflictSide],
    reasons: &[String],
) -> String {
    let mut text = String::new();
    for side in sides {
        let _ = writeln!(
            text,
            "{}: \"{}\" ({}, {})",
            if side.landed { "Landed" } else { "Parked" },
            side.intent,
            short(&side.change.to_string()),
            side.actor.id()
        );
    }
    if !reasons.is_empty() {
        let _ = writeln!(text, "\nWhy:");
        for reason in reasons {
            let _ = writeln!(text, "  - {reason}");
        }
    }
    let mut contested: Vec<String> = nodes.iter().map(SummaryNode::label).collect();
    for path in paths {
        let path = path.to_string();
        if !nodes.iter().any(|n| n.path.as_ref() == Some(&path)) {
            contested.push(path);
        }
    }
    contested.dedup();
    if !contested.is_empty() {
        let _ = writeln!(text, "\nContested: {}", contested.join(", "));
    }
    let landed = sides.iter().filter(|s| s.landed).count();
    for side in sides {
        let whose = match (side.landed, landed) {
            (false, _) => "the parked change".to_owned(),
            (true, 1) => "the landed change".to_owned(),
            (true, _) => format!("landed change {}", short(&side.change.to_string())),
        };
        let _ = writeln!(text, "\nWhat {whose} changed:");
        if side.changed.is_empty() {
            let _ = writeln!(text, "  - nothing in a file");
        }
        for line in &side.changed {
            let _ = writeln!(text, "  - {line}");
        }
    }
    text
}
