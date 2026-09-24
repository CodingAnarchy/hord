//! Phase 1: land the corpus commits in a hord repository, so every commit
//! has NodeIds carried by hord itself (ADRs 0017–0020), and record per
//! commit what selection needs: the write set, the change facts since the
//! coverage checkpoint, the impact set, and fault targets.
//!
//! Deterministic and cheap next to the cargo runs; the result is one JSON
//! file, so a resumed run skips it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hord_core::{Actor, ChangeId, Intent, NodeId, RepoPath, SnapshotId};
use hord_txn::{BeginOptions, QueueStatus, Repo, RepoOptions, StubVerifier};
use hord_verify::{
    ChangeFacts, DefDelta, Definition, FileVersion, ImpactBound, ImpactSet, ReferenceGraph,
    impact_set, impact_set_attributable,
};
use hord_verify_rust::{DefinitionIndex, diff_rust_file};
use serde::{Deserialize, Serialize};

use crate::git;

/// A function the commit wrote, where a fault can go.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FaultTarget {
    pub path: String,
    pub node: NodeId,
    pub name: String,
    /// Byte span in the commit's version of the file.
    pub span: std::ops::Range<usize>,
}

/// Everything later phases need about one commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CommitFacts {
    pub index: usize,
    pub commit: String,
    pub base: String,
    pub checkpoint: usize,
    pub base_snapshot: SnapshotId,
    pub result_snapshot: SnapshotId,
    pub change: ChangeId,
    pub write_set: BTreeSet<NodeId>,
    /// Paths the commit changed (base → commit).
    pub changed_paths: Vec<String>,
    /// Impact of the write set, with facts from the checkpoint snapshot to
    /// the commit (drift included).
    pub impact: ImpactSet,
    /// Functions the commit wrote (born or edited, not tests).
    pub targets: Vec<FaultTarget>,
    /// `path: qualified name` of every impacted and touched node, for
    /// `--explain`.
    pub names: BTreeMap<NodeId, String>,
    /// Per edited function (checkpoint to commit), its edited lines in the
    /// checkpoint's file: variant B's region filter.
    pub edited_lines: BTreeMap<NodeId, (RepoPath, BTreeSet<u32>)>,
    /// The commit's own facts (base to commit) and the impact set ADR 0022
    /// selects with after the 50-commit measurement: dependents only of
    /// writes coverage cannot attribute (the lander's view, `--fresh`).
    pub own_impact: ImpactSet,
    /// Definitions of the `.rs` files the commit rewrote, at the commit.
    pub defs_delta: DefinitionIndex,
    /// `.rs` files the commit deleted (or that no longer parse).
    pub defs_removed: Vec<RepoPath>,
}

/// A coverage checkpoint: the base of its first commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub first: usize,
    pub commit: String,
    pub snapshot: SnapshotId,
    pub defs: DefinitionIndex,
    /// Union of its commits' edited function lines (variant B's region
    /// data is collected for these).
    pub interest: BTreeMap<RepoPath, BTreeSet<u32>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Prepared {
    pub commits: Vec<CommitFacts>,
    pub checkpoints: Vec<Checkpoint>,
    pub prepare_ms: u64,
}

/// One file of the current snapshot.
#[derive(Clone, Debug)]
struct Entry {
    bytes: Arc<Vec<u8>>,
    defs: Arc<Vec<Definition>>,
    /// Identifier counts in each definition's own text.
    tokens: Arc<Vec<(NodeId, HashMap<String, u32>)>>,
}

/// The current snapshot's files, with a textual reverse-reference graph.
#[derive(Default)]
struct Files {
    entries: HashMap<RepoPath, Entry>,
    names: HashMap<NodeId, (RepoPath, Option<String>)>,
    qualified: HashMap<NodeId, String>,
    /// Private items: the path components of the module subtree that can
    /// name them (Rust privacy), and the file itself.
    private: HashMap<NodeId, (RepoPath, Vec<String>)>,
    manifests: BTreeSet<Vec<String>>,
}

fn simple_name(def: &Definition) -> Option<String> {
    let name = def.name.as_ref()?;
    let last = name.as_str().rsplit("::").next()?;
    let last = last.trim_start_matches("r#");
    (!last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| last.to_owned())
}

fn identifiers(text: &[u8]) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < text.len() && (text[i].is_ascii_alphanumeric() || text[i] == b'_') {
                i += 1;
            }
            let word = String::from_utf8_lossy(&text[start..i]).into_owned();
            *out.entry(word).or_default() += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Where a private function can be named from: its file, and the
/// directory of its module's child files (`a/foo.rs` → `a/foo/`,
/// `a/mod.rs` → `a/`). `None` for anything that may be public: a `pub`
/// item, or a method of a trait or trait impl (public through the trait).
/// Only free functions and inherent methods qualify.
fn private_scope(
    path: &RepoPath,
    def: &Definition,
    defs: &[Definition],
    bytes: &[u8],
) -> Option<Vec<String>> {
    if def.kind.as_str() != "function_item" {
        return None;
    }
    if let Some(parent) = def.parent {
        let parent = defs.iter().find(|d| d.node == parent)?;
        match parent.kind.as_str() {
            "mod_item" => {}
            "impl_item" => {
                let header = bytes.get(parent.span.clone()).unwrap_or_default();
                let brace = header
                    .iter()
                    .position(|b| *b == b'{')
                    .unwrap_or(header.len());
                if identifiers(&header[..brace]).contains_key("for") {
                    return None;
                }
            }
            _ => return None,
        }
    }
    // `pub` anywhere before the body counts (conservative).
    let text = bytes.get(def.span.clone()).unwrap_or_default();
    let head = &text[..text.iter().position(|b| *b == b'{').unwrap_or(text.len())];
    if identifiers(head).contains_key("pub") {
        return None;
    }
    let mut dir = path.components().to_vec();
    let file = dir.pop()?;
    let stem = file.strip_suffix(".rs")?;
    if !matches!(stem, "mod" | "lib" | "main") {
        dir.push(stem.to_owned());
    }
    Some(dir)
}

/// The lines of `node` in `old`'s file that the edit to `new` changes: each
/// deleted or replaced line, and for a pure insertion the lines on both
/// sides of it. `None` when the definition is missing from either side.
fn edited_lines_of(node: NodeId, old: &Entry, new: &Entry) -> Option<BTreeSet<u32>> {
    let before = old.defs.iter().find(|d| d.node == node)?;
    let after = new.defs.iter().find(|d| d.node == node)?;
    let first = hord_verify::line_range(&old.bytes, &before.span).start;
    let old_text = String::from_utf8_lossy(old.bytes.get(before.span.clone())?).into_owned();
    let new_text = String::from_utf8_lossy(new.bytes.get(after.span.clone())?).into_owned();
    Some(
        changed_old_lines(&old_text, &new_text)
            .into_iter()
            .map(|l| first + l)
            .collect(),
    )
}

/// 0-based lines of `old` that differ in `new` (see [`edited_lines_of`]).
fn changed_old_lines(old: &str, new: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let patch = diffy::create_patch(old, new);
    for hunk in patch.hunks() {
        // `old_range().start()` is 1-based.
        let mut at = u32::try_from(hunk.old_range().start())
            .unwrap_or(1)
            .saturating_sub(1);
        // Inserts right after deletes replace those lines; only a pure
        // insertion marks its neighbours.
        let mut replacing = false;
        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    at += 1;
                    replacing = false;
                }
                diffy::Line::Delete(_) => {
                    out.insert(at);
                    at += 1;
                    replacing = true;
                }
                diffy::Line::Insert(_) if replacing => {}
                diffy::Line::Insert(_) => {
                    out.insert(at.saturating_sub(1));
                    out.insert(at);
                }
            }
        }
    }
    let last = u32::try_from(old.lines().count().max(1) - 1).unwrap_or(0);
    out.into_iter().map(|l| l.min(last)).collect()
}

/// A definition's text without its child definitions.
fn own_text(bytes: &[u8], def: &Definition, defs: &[Definition]) -> Vec<u8> {
    let mut kids: Vec<&Definition> = defs.iter().filter(|d| d.parent == Some(def.node)).collect();
    kids.sort_by_key(|d| d.span.start);
    let mut out = Vec::new();
    let mut at = def.span.start;
    for kid in kids {
        let start = kid.span.start.clamp(at, def.span.end);
        out.extend_from_slice(bytes.get(at..start).unwrap_or_default());
        at = kid.span.end.clamp(at, def.span.end);
    }
    out.extend_from_slice(bytes.get(at..def.span.end).unwrap_or_default());
    out
}

impl Files {
    fn set(&mut self, path: RepoPath, bytes: Vec<u8>, defs: Vec<Definition>) {
        self.remove(&path);
        let tokens = defs
            .iter()
            .map(|d| (d.node, identifiers(&own_text(&bytes, d, &defs))))
            .collect();
        for d in &defs {
            if let Some(scope) = private_scope(&path, d, &defs, &bytes) {
                self.private.insert(d.node, (path.clone(), scope));
            }
            self.names.insert(d.node, (path.clone(), simple_name(d)));
            let name = d
                .name
                .as_ref()
                .map_or_else(|| d.kind.to_string(), |n| n.as_str().to_owned());
            self.qualified.insert(d.node, format!("{path}: {name}"));
        }
        if path.components().last().is_some_and(|n| n == "Cargo.toml") {
            let mut dir = path.components().to_vec();
            dir.pop();
            self.manifests.insert(dir);
        }
        self.entries.insert(
            path,
            Entry {
                bytes: Arc::new(bytes),
                defs: Arc::new(defs),
                tokens: Arc::new(tokens),
            },
        );
    }

    fn remove(&mut self, path: &RepoPath) {
        if let Some(old) = self.entries.remove(path) {
            for d in old.defs.iter() {
                self.names.remove(&d.node);
                self.qualified.remove(&d.node);
                self.private.remove(&d.node);
            }
        }
        if path.components().last().is_some_and(|n| n == "Cargo.toml") {
            let mut dir = path.components().to_vec();
            dir.pop();
            self.manifests.remove(&dir);
        }
    }

    fn package_of(&self, path: &RepoPath) -> Option<String> {
        self.manifests
            .iter()
            .filter(|dir| path.components().starts_with(dir))
            .max_by_key(|dir| dir.len())
            .map(|dir| dir.join("/"))
    }

    fn definition_index(&self) -> DefinitionIndex {
        let mut index = DefinitionIndex::new();
        for (path, e) in &self.entries {
            if !e.defs.is_empty() {
                index.add_file(path, &e.bytes, &e.defs);
            }
        }
        index
    }
}

/// `References` read backwards, by identifier: `d` depends on `x` when
/// `d`'s own text names `x`'s simple name (twice, when `d` has that name
/// itself, so its own declaration does not count). The Rust resolver's
/// edges are names lexically present in a body, with one edge per
/// candidate for names it cannot resolve (ADR 0011), so this is a superset
/// of them: safe for an impact set.
impl ReferenceGraph for Files {
    fn dependents(&self, node: NodeId) -> hord_verify::Result<Vec<NodeId>> {
        let Some((_, Some(name))) = self.names.get(&node) else {
            return Ok(Vec::new());
        };
        let scope = self.private.get(&node);
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            if let Some((file, dir)) = scope
                && path != file
                && !path.components().starts_with(dir)
            {
                continue;
            }
            for (i, (d, tokens)) in entry.tokens.iter().enumerate() {
                let Some(count) = tokens.get(name) else {
                    continue;
                };
                let own = simple_name(&entry.defs[i]);
                if *d != node && (own.as_deref() != Some(name.as_str()) || *count >= 2) {
                    out.push(*d);
                }
            }
        }
        Ok(out)
    }

    fn package(&self, node: NodeId) -> Option<String> {
        let (path, _) = self.names.get(&node)?;
        self.package_of(path)
    }
}

fn actor() -> Actor {
    Actor::Agent {
        id: "m4-importer".into(),
        model: "corpus".into(),
        model_hash: hord_core::Bytes::default(),
        harness: "hord-eval-m4".into(),
    }
}

fn intent(summary: &str) -> Intent {
    Intent {
        summary: summary.into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    }
}

fn convert(defs: Vec<hord_txn::DefinitionInfo>) -> Vec<Definition> {
    defs.into_iter()
        .map(|d| Definition {
            node: d.node,
            path: d.path,
            kind: d.kind,
            name: d.name,
            span: d.span,
            parent: d.parent,
        })
        .collect()
}

async fn load_file(
    repo: &Repo,
    files: &mut Files,
    snapshot: SnapshotId,
    path: &RepoPath,
    bytes: Vec<u8>,
) -> Result<()> {
    let defs = if path.to_string().ends_with(".rs") {
        convert(repo.definitions_in(snapshot, path.clone()).await?)
    } else {
        Vec::new()
    };
    files.set(path.clone(), bytes, defs);
    Ok(())
}

fn version(entry: Option<&Entry>) -> Option<FileVersion<'_>> {
    entry.map(|e| FileVersion {
        bytes: &e.bytes,
        defs: &e.defs,
    })
}

/// Land `commits` (oldest first) in a fresh repository at `hord_dir` and
/// record their facts. A checkpoint starts every `every` commits.
pub(crate) async fn prepare(
    corpus: &Path,
    hord_dir: &Path,
    commits: &[String],
    every: usize,
) -> Result<Prepared> {
    let started = Instant::now();
    if hord_dir.exists() {
        std::fs::remove_dir_all(hord_dir)?;
    }
    std::fs::create_dir_all(hord_dir)?;
    let repo = Repo::create_with(
        hord_dir,
        RepoOptions {
            verifier: Some(Arc::new(StubVerifier)),
            ..RepoOptions::default()
        },
    )
    .await?;
    let first = commits.first().context("no commits")?;
    let base0 = git::parent(corpus, first)?;
    let tree = git::tree_files(corpus, &base0)?;
    eprintln!(
        "[prepare] bootstrap {} files of {}",
        tree.len(),
        &base0[..10]
    );
    let mut initial = Vec::new();
    for (path, bytes) in &tree {
        if let Ok(p) = path.parse::<RepoPath>() {
            initial.push((p, bytes.clone()));
        }
    }
    repo.bootstrap(initial.clone(), intent(&format!("import {base0}")), actor())
        .await?;
    let mut snapshot = repo.head().await?.snapshot;
    let mut files = Files::default();
    for (path, bytes) in initial {
        load_file(&repo, &mut files, snapshot, &path, bytes).await?;
    }
    eprintln!(
        "[prepare] {} definitions in {:.1}s",
        files.names.len(),
        started.elapsed().as_secs_f64()
    );

    let mut checkpoints = Vec::new();
    // Versions at the current checkpoint of every path changed since.
    let mut stash: BTreeMap<RepoPath, Option<Entry>> = BTreeMap::new();
    let mut out = Vec::new();
    let mut base = base0;
    for (index, commit) in commits.iter().enumerate() {
        if index % every.max(1) == 0 {
            checkpoints.push(Checkpoint {
                first: index,
                commit: base.clone(),
                snapshot,
                defs: files.definition_index(),
                interest: BTreeMap::new(),
            });
            stash.clear();
        }
        let changed = git::diff(corpus, &base, commit)?;
        let mut ws = repo.begin(BeginOptions::at_head(actor())).await?;
        let mut edits: Vec<(RepoPath, &Option<Vec<u8>>)> = Vec::new();
        for (path, content) in &changed {
            let Ok(p) = path.parse::<RepoPath>() else {
                continue;
            };
            edits.push((p.clone(), content));
            match content {
                Some(bytes) => ws.write_file(&p, bytes.clone()).await?,
                None => {
                    if files.entries.contains_key(&p) {
                        ws.delete_file(&p).await?;
                    }
                }
            }
        }
        let paths: Vec<RepoPath> = edits.iter().map(|(p, _)| p.clone()).collect();
        let proposal = ws.propose(intent(&format!("cargo {commit}"))).await?;
        repo.submit(proposal.change).await?;
        let landed = repo.land_local().await?;
        let Some(entry) = landed.iter().find(|e| e.change == proposal.change) else {
            bail!("commit {commit} was not processed by the lander");
        };
        let change = match &entry.status {
            QueueStatus::Landed { landed } => *landed,
            other => bail!("commit {commit} did not land: {other:?}"),
        };
        let record = repo.change(change).await?;
        let base_snapshot = snapshot;
        snapshot = record.result;

        // Fault targets: functions the commit itself wrote.
        let mut own = ChangeFacts::default();
        let old: HashMap<RepoPath, Option<Entry>> = paths
            .iter()
            .map(|p| (p.clone(), files.entries.get(p).cloned()))
            .collect();
        for (p, content) in &edits {
            stash
                .entry(p.clone())
                .or_insert_with(|| files.entries.get(p).cloned());
            match content {
                Some(bytes) => load_file(&repo, &mut files, snapshot, p, bytes.clone()).await?,
                None => files.remove(p),
            }
        }
        let mut defs_delta = DefinitionIndex::new();
        let mut defs_removed = Vec::new();
        for p in &paths {
            if p.to_string().ends_with(".rs") {
                diff_rust_file(
                    &mut own,
                    p,
                    version(old[p].as_ref()),
                    version(files.entries.get(p)),
                );
                match files.entries.get(p) {
                    Some(e) if !e.defs.is_empty() => defs_delta.add_file(p, &e.bytes, &e.defs),
                    _ => defs_removed.push(p.clone()),
                }
            } else {
                own.paths.insert(p.clone());
            }
        }
        let mut targets = Vec::new();
        for t in &own.touched {
            if t.kind.as_str() != "function_item" || t.test || t.delta == DefDelta::Died {
                continue;
            }
            let Some(entry) = files.entries.get(&t.path) else {
                continue;
            };
            let Some(def) = entry.defs.iter().find(|d| d.node == t.node) else {
                continue;
            };
            targets.push(FaultTarget {
                path: t.path.to_string(),
                node: t.node,
                name: def
                    .name
                    .as_ref()
                    .map(|n| n.as_str().to_owned())
                    .unwrap_or_default(),
                span: def.span.clone(),
            });
        }

        // Facts since the checkpoint, and the impact set.
        let mut facts = ChangeFacts::default();
        for (p, older) in &stash {
            if p.to_string().ends_with(".rs") {
                diff_rust_file(
                    &mut facts,
                    p,
                    version(older.as_ref()),
                    version(files.entries.get(p)),
                );
            } else {
                facts.paths.insert(p.clone());
            }
        }
        let mut edited_lines = BTreeMap::new();
        for t in &facts.touched {
            if t.kind.as_str() != "function_item" || t.delta != DefDelta::Edited {
                continue;
            }
            let (Some(Some(old)), Some(new)) = (stash.get(&t.path), files.entries.get(&t.path))
            else {
                continue;
            };
            if let Some(lines) = edited_lines_of(t.node, old, new) {
                let cp = checkpoints.last_mut().expect("a checkpoint");
                cp.interest
                    .entry(t.path.clone())
                    .or_default()
                    .extend(lines.iter().copied());
                edited_lines.insert(t.node, (t.path.clone(), lines));
            }
        }
        let impact = impact_set(&files, &record.write_set, ImpactBound::default(), facts)?;
        let own_impact = impact_set_attributable(
            &files,
            &record.write_set,
            ImpactBound::default(),
            own.clone(),
        )?;
        let names = impact
            .nodes
            .keys()
            .chain(impact.facts.touched.iter().map(|t| &t.node))
            .filter_map(|n| files.qualified.get(n).map(|q| (*n, q.clone())))
            .collect();
        out.push(CommitFacts {
            index,
            commit: commit.clone(),
            base: base.clone(),
            checkpoint: checkpoints.len() - 1,
            base_snapshot,
            result_snapshot: snapshot,
            change,
            write_set: record.write_set.clone(),
            changed_paths: changed.iter().map(|(p, _)| p.clone()).collect(),
            impact,
            targets,
            names,
            edited_lines,
            own_impact,
            defs_delta,
            defs_removed,
        });
        if (index + 1) % 25 == 0 || index + 1 == commits.len() {
            eprintln!(
                "[prepare] {}/{} commits landed ({:.1}s)",
                index + 1,
                commits.len(),
                started.elapsed().as_secs_f64()
            );
        }
        base = commit.clone();
    }
    Ok(Prepared {
        commits: out,
        checkpoints,
        prepare_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::{NodeKind, QualifiedName};

    use super::*;

    fn def(
        node: u128,
        name: &str,
        span: std::ops::Range<usize>,
        parent: Option<u128>,
    ) -> Definition {
        Definition {
            node: NodeId::from_u128(node),
            path: RepoPath::from_str("crates/a/src/lib.rs").expect("parse test path"),
            kind: NodeKind::new("function_item"),
            name: Some(QualifiedName::new(name)),
            span,
            parent: parent.map(NodeId::from_u128),
        }
    }

    #[test]
    fn edited_lines_are_replaced_lines_and_insertion_neighbours() {
        let old = "fn f() {\n    a();\n    b();\n    c();\n}\n";
        let replaced = old.replace("b();", "bb();");
        assert_eq!(changed_old_lines(old, &replaced), [2].into_iter().collect());
        let inserted = old.replace("    b();\n", "    b();\n    x();\n");
        assert_eq!(
            changed_old_lines(old, &inserted),
            [2, 3].into_iter().collect()
        );
        let deleted = old.replace("    a();\n", "");
        assert_eq!(changed_old_lines(old, &deleted), [1].into_iter().collect());
    }

    #[test]
    fn textual_graph_finds_callers_not_namesakes() {
        let src = "fn new() {}\nfn build() { new(); }\nfn other() { let x = 1; }\nimpl T { fn new() { Self::new(); } }\n";
        let spans = |needle: &str| {
            let s = src.find(needle).expect("the needle is in the test source");
            s..s + needle.len()
        };
        let defs = vec![
            def(1, "a::new", spans("fn new() {}"), None),
            def(2, "a::build", spans("fn build() { new(); }"), None),
            def(3, "a::other", spans("fn other() { let x = 1; }"), None),
            def(
                4,
                "a::T",
                spans("impl T { fn new() { Self::new(); } }"),
                None,
            ),
            def(5, "a::T::new", spans("fn new() { Self::new(); }"), Some(4)),
        ];
        let mut files = Files::default();
        files.set(
            RepoPath::from_str("crates/a/Cargo.toml").expect("parse test path"),
            Vec::new(),
            Vec::new(),
        );
        files.set(
            RepoPath::from_str("crates/a/src/lib.rs").expect("parse test path"),
            src.as_bytes().to_vec(),
            defs,
        );
        let mut deps = files.dependents(NodeId::from_u128(1)).expect("dependents");
        deps.sort();
        // `build` calls it; `T::new` calls `new` besides declaring it; the
        // impl's own text (without its method) does not.
        assert_eq!(deps, vec![NodeId::from_u128(2), NodeId::from_u128(5)]);
        assert_eq!(
            files.package(NodeId::from_u128(2)).as_deref(),
            Some("crates/a")
        );
        assert_eq!(files.definition_index().len(), 1);
    }

    #[test]
    fn private_functions_are_named_only_from_their_module_subtree() {
        let mut files = Files::default();
        let one = |path: &str, node: u128, name: &str, text: &str| {
            let d = Definition {
                node: NodeId::from_u128(node),
                path: RepoPath::from_str(path).expect("parse test path"),
                kind: NodeKind::new("function_item"),
                name: Some(QualifiedName::new(name)),
                span: 0..text.len(),
                parent: None,
            };
            (
                RepoPath::from_str(path).expect("parse test path"),
                text.as_bytes().to_vec(),
                vec![d],
            )
        };
        for (p, b, d) in [
            one(
                "tests/suite/cache.rs",
                1,
                "cache::simple",
                "fn simple() { helper(); }",
            ),
            one(
                "tests/suite/cache/inner.rs",
                2,
                "cache::inner::x",
                "fn x() { super::simple(); }",
            ),
            one(
                "tests/suite/other.rs",
                3,
                "other::y",
                "fn y() { simple(); }",
            ),
            one("src/lib.rs", 4, "open", "pub fn open() {}"),
            one("src/a.rs", 5, "a::z", "fn z() { open(); }"),
        ] {
            files.set(p, b, d);
        }
        // `simple` is private to `cache`: `other::y` naming a `simple` is
        // another item.
        assert_eq!(
            files.dependents(NodeId::from_u128(1)).expect("dependents"),
            vec![NodeId::from_u128(2)]
        );
        assert_eq!(
            files.dependents(NodeId::from_u128(4)).expect("dependents"),
            vec![NodeId::from_u128(5)]
        );
    }
}
