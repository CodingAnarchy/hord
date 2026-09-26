//! "Take theirs" for an arbiter (spec §6.4 rung 3): the parked change
//! merged onto head definition by definition, with every hard conflict
//! resolved in favor of the parked change's side.
//!
//! For each file the parked change touched:
//!
//! - head did not change it since the change's base: the change's version;
//! - both changed it and it parses on all three sides: the structural 3-way
//!   merge (spec §5.2, the lander's mode). While it reports a hard conflict,
//!   each contested definition gets the parked change's text at the place
//!   head has that [`NodeId`] now (so a move head made within the file is
//!   followed); it is removed when the change deleted it, and re-added at
//!   its old place when head deleted it. A definition head moved to another
//!   file keeps its [`NodeId`] (ADR 0033): its parked text goes to that
//!   file, at head's place of it, and it leaves this file's parked side.
//!   The result is identified against head, and the merge runs again. Every definition not in contention stays as the
//!   merge produces it, so a non-conflicting edit head gained in the same
//!   file (a third agent's landed change) is kept;
//! - otherwise (a blob-tier file, one that does not parse, a conflict on
//!   the file itself rather than a definition in it, or a file deleted on
//!   one side): the line merge for a blob, else the change's whole file.
//!   These files are reported ([`TheirsMerge::whole_file`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use hord_core::{Bytes, ChangeRecord, NodeId, RepoPath, SnapshotId};
use hord_lang::{IdentifiedTree, LangAdapter};

use crate::repo::Inner;
use crate::semantic::{carry, definitions};
use crate::{DefinitionInfo, Result};

/// Merge rounds before a file falls back to the change's whole file.
const MAX_ROUNDS: usize = 16;

/// The files "take theirs" writes on head.
#[derive(Debug, Default)]
pub(crate) struct TheirsMerge {
    /// Each file's new content; `None` deletes it.
    pub files: Vec<(RepoPath, Option<Bytes>)>,
    /// Files taken whole from the parked change: hord could not merge them
    /// by definition.
    pub whole_file: Vec<RepoPath>,
}

impl Inner {
    /// `record`'s files merged onto `head`, the parked change winning every
    /// hard conflict.
    pub(crate) fn merge_theirs(
        &self,
        record: &ChangeRecord,
        head: SnapshotId,
    ) -> Result<TheirsMerge> {
        let mut out = TheirsMerge::default();
        let mut moves = Moves::default();
        for delta in self.changed_paths(record.base, record.result)? {
            let path = delta.path;
            let ours = self.blob_id(head, &path)?;
            if ours == delta.to {
                continue;
            }
            let theirs_bytes = match delta.to {
                Some(blob) => Some(self.blob_bytes(blob)?),
                None => None,
            };
            if ours == delta.from {
                out.files.push((path, theirs_bytes));
                continue;
            }
            let (Some(ours), Some(theirs), Some(from)) = (ours, delta.to, delta.from) else {
                // Created on both sides, or deleted on one: the file itself
                // is contested.
                out.whole_file.push(path.clone());
                out.files.push((path, theirs_bytes));
                continue;
            };
            let merged = match self.merge_definitions(record, head, &path, &mut moves)? {
                Some(bytes) => Some(bytes),
                None => {
                    let (base, ours, theirs) = (
                        self.blob_bytes(from)?,
                        self.blob_bytes(ours)?,
                        self.blob_bytes(theirs)?,
                    );
                    let parsed = self.parsed_at(record.base, &path)?.is_some();
                    match hord_diff::merge_blob(base.as_slice(), ours.as_slice(), theirs.as_slice())
                    {
                        // A blob-tier file's line merge is its merge.
                        Ok(bytes) if !parsed => Some(Bytes::new(bytes)),
                        _ => None,
                    }
                }
            };
            match merged {
                Some(bytes) => out.files.push((path, Some(bytes))),
                None => {
                    out.whole_file.push(path.clone());
                    out.files.push((path, theirs_bytes));
                }
            }
        }
        self.place_moved(head, moves.relocated, &mut out)?;
        Ok(out)
    }

    /// Give each definition head moved to another file its parked text,
    /// at head's place of it in that file (as merged, if the change also
    /// touched that file).
    fn place_moved(
        &self,
        head: SnapshotId,
        relocated: Vec<Relocated>,
        out: &mut TheirsMerge,
    ) -> Result<()> {
        for moved in relocated {
            let (Some(head_view), slot) = (
                self.parsed_at(head, &moved.to)?,
                out.files.iter().position(|(p, _)| *p == moved.to),
            ) else {
                continue;
            };
            let current = match slot.and_then(|i| out.files[i].1.clone()) {
                Some(bytes) => bytes,
                None => match self.blob_id(head, &moved.to)? {
                    Some(blob) => self.blob_bytes(blob)?,
                    None => continue,
                },
            };
            let Some(adapter) = self.adapter_for(&moved.to, head_view.lang) else {
                continue;
            };
            let Some(tree) = self.identify_on_head(
                adapter,
                &moved.to,
                head,
                &head_view.tree,
                current.as_slice().to_vec(),
            )?
            else {
                continue;
            };
            let Some(def) = definitions(adapter, &moved.to, &tree)
                .into_iter()
                .find(|d| d.node == moved.node)
            else {
                continue;
            };
            let mut bytes = current.as_slice().to_vec();
            bytes.splice(def.span, moved.text);
            let bytes = Some(Bytes::new(bytes));
            match slot {
                Some(i) => out.files[i].1 = bytes,
                None => out.files.push((moved.to, bytes)),
            }
        }
        Ok(())
    }

    /// Where each definition is in head, by [`NodeId`] (built on first use).
    fn head_index<'m>(
        &self,
        head: SnapshotId,
        moves: &'m mut Moves,
    ) -> Result<&'m BTreeMap<NodeId, RepoPath>> {
        if moves.index.is_none() {
            let mut index = BTreeMap::new();
            for (path, _) in self.list_files(head)? {
                for def in self.definitions_at(head, &path)? {
                    index.insert(def.node, path.clone());
                }
            }
            moves.index = Some(index);
        }
        Ok(moves.index.get_or_insert_default())
    }

    /// The structural merge of `path` with the parked change winning hard
    /// conflicts. `None` when the file is not parsed on every side, or a
    /// conflict is not on a definition hord can take from one side.
    fn merge_definitions(
        &self,
        record: &ChangeRecord,
        head: SnapshotId,
        path: &RepoPath,
        moves: &mut Moves,
    ) -> Result<Option<Bytes>> {
        let (Some(base), Some(head_view), Some(theirs)) = (
            self.parsed_at(record.base, path)?,
            self.parsed_at(head, path)?,
            self.parsed_at(record.result, path)?,
        ) else {
            return Ok(None);
        };
        let Some(adapter) = self.adapter_for(path, theirs.lang) else {
            return Ok(None);
        };
        let root = NodeId::file_root(path);
        let base_defs = by_node(definitions(adapter, path, &base.tree));
        let mut theirs_tree: Arc<IdentifiedTree> = Arc::clone(&theirs.tree);
        let mut ours: Arc<IdentifiedTree> = Arc::clone(&head_view.tree);
        for _ in 0..MAX_ROUNDS {
            let conflict = match hord_diff::merge(
                adapter,
                path,
                &base.tree,
                &ours,
                &theirs_tree,
                hord_diff::MergeMode::Lander,
            ) {
                Ok(merged) => return Ok(Some(adapter.project(&merged.tree.tree))),
                Err(conflict) => conflict,
            };
            let mut contested: BTreeSet<NodeId> = conflict.nodes.iter().copied().collect();
            if contested.is_empty() || contested.contains(&root) {
                return Ok(None);
            }
            let theirs_defs = by_node(definitions(adapter, path, &theirs_tree));
            let theirs_text = adapter.project(&theirs_tree.tree);
            let ours_text = adapter.project(&ours.tree);
            let ours_defs = by_node(definitions(adapter, path, &ours));
            // ADR 0033: gone from head's file but kept elsewhere in head
            // under the same id: the parked text follows it there, and it
            // leaves this file's parked side.
            let index = self.head_index(head, moves)?;
            let away: Vec<(NodeId, RepoPath)> = contested
                .iter()
                .filter(|n| !ours_defs.contains_key(n) && theirs_defs.contains_key(n))
                .filter_map(|n| index.get(n).filter(|p| *p != path).map(|p| (*n, p.clone())))
                .collect();
            if !away.is_empty() {
                let mut text = theirs_text.as_slice().to_vec();
                let mut spans: Vec<Range<usize>> = Vec::new();
                for (node, to) in &away {
                    let Some(def) = theirs_defs.get(node) else {
                        continue;
                    };
                    let Some(body) = text.get(def.span.clone()) else {
                        return Ok(None);
                    };
                    moves.relocated.push(Relocated {
                        node: *node,
                        to: to.clone(),
                        text: body.to_vec(),
                    });
                    spans.push(def.span.clone());
                    contested.remove(node);
                }
                spans.sort_by_key(|s| std::cmp::Reverse(s.start));
                for span in spans {
                    text.splice(span, Vec::new());
                }
                match self.identify_on(adapter, path, record.result, &theirs.tree, text)? {
                    Some(tree) => theirs_tree = tree,
                    None => return Ok(None),
                }
                if contested.is_empty() {
                    continue;
                }
            }
            let theirs_defs = by_node(definitions(adapter, path, &theirs_tree));
            let theirs_text = adapter.project(&theirs_tree.tree);
            let Some(next) = take_theirs(
                ours_text.as_slice(),
                &ours_defs,
                theirs_text.as_slice(),
                &theirs_defs,
                &base_defs,
                &contested,
            ) else {
                return Ok(None);
            };
            if next == ours_text.as_slice() {
                // Every contested definition already has the parked text at
                // head's place of it (a move one side made, against the
                // other's edit, spec §5.2). Head's text is the answer when
                // the parked side changed nothing outside them.
                let base_text = adapter.project(&base.tree.tree);
                let changed = changed_definitions(
                    base_text.as_slice(),
                    &base_defs,
                    theirs_text.as_slice(),
                    &theirs_defs,
                );
                if changed.is_subset(&contested) {
                    return Ok(Some(ours_text));
                }
                return Ok(None);
            }
            match self.identify_on_head(adapter, path, head, &head_view.tree, next)? {
                Some(tree) => ours = tree,
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// `bytes` parsed, with ids carried from `head`'s version of `path`.
    fn identify_on_head(
        &self,
        adapter: &dyn LangAdapter,
        path: &RepoPath,
        head: SnapshotId,
        head_tree: &IdentifiedTree,
        bytes: Vec<u8>,
    ) -> Result<Option<Arc<IdentifiedTree>>> {
        self.identify_on(adapter, path, head, head_tree, bytes)
    }

    /// `bytes` parsed, with ids carried from `from_tree`, `path` in
    /// `snapshot`.
    fn identify_on(
        &self,
        adapter: &dyn LangAdapter,
        path: &RepoPath,
        snapshot: SnapshotId,
        from_tree: &IdentifiedTree,
        bytes: Vec<u8>,
    ) -> Result<Option<Arc<IdentifiedTree>>> {
        let blob = self.put_blob(&bytes)?;
        let Some(tree) = self.parse(adapter, blob, &bytes) else {
            return Ok(None);
        };
        let mapping = carry(adapter, path, snapshot, from_tree, &tree)?;
        Ok(Some(Arc::new(IdentifiedTree::new(
            (*tree).clone(),
            mapping.nodes,
        ))))
    }
}

/// Definitions head moved to other files (ADR 0033), and where head's
/// definitions are.
#[derive(Default)]
struct Moves {
    index: Option<BTreeMap<NodeId, RepoPath>>,
    relocated: Vec<Relocated>,
}

/// A definition's parked text, for the file head moved it to.
struct Relocated {
    node: NodeId,
    to: RepoPath,
    text: Vec<u8>,
}

/// `span` of `bytes` without leading and trailing ASCII whitespace.
fn trimmed(bytes: &[u8], span: Range<usize>) -> Option<Range<usize>> {
    let text = bytes.get(span.clone())?;
    let start = text.iter().position(|b| !b.is_ascii_whitespace())?;
    let end = text.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
    Some(span.start + start..span.start + end)
}

/// Definitions whose text differs between `base` and `theirs`, or that
/// only one of them has.
fn changed_definitions(
    base: &[u8],
    base_defs: &BTreeMap<NodeId, DefinitionInfo>,
    theirs: &[u8],
    theirs_defs: &BTreeMap<NodeId, DefinitionInfo>,
) -> BTreeSet<NodeId> {
    let text = |bytes: &[u8], d: &DefinitionInfo| bytes.get(d.span.clone()).map(<[u8]>::to_vec);
    base_defs
        .keys()
        .chain(theirs_defs.keys())
        .filter(|node| {
            base_defs.get(node).and_then(|d| text(base, d))
                != theirs_defs.get(node).and_then(|d| text(theirs, d))
        })
        .copied()
        .collect()
}

fn by_node(defs: Vec<DefinitionInfo>) -> BTreeMap<NodeId, DefinitionInfo> {
    defs.into_iter().map(|d| (d.node, d)).collect()
}

/// Where to re-add `node`, a definition `ours` no longer has, and how to
/// pad it: after its nearest earlier sibling in `base` that `ours` still
/// has, else before its nearest later one, else at the end of the file.
fn old_place(
    node: NodeId,
    ours: &[u8],
    ours_defs: &BTreeMap<NodeId, DefinitionInfo>,
    base_defs: &BTreeMap<NodeId, DefinitionInfo>,
) -> (usize, bool) {
    let Some(was) = base_defs.get(&node) else {
        return (ours.len(), true);
    };
    let siblings = base_defs
        .values()
        .filter(|d| d.parent == was.parent && d.node != node && ours_defs.contains_key(&d.node));
    let before = siblings
        .clone()
        .filter(|d| d.span.end <= was.span.start)
        .max_by_key(|d| d.span.end);
    if let Some(prev) = before.and_then(|d| ours_defs.get(&d.node)) {
        return (prev.span.end, true);
    }
    let after = siblings
        .filter(|d| d.span.start >= was.span.end)
        .min_by_key(|d| d.span.start);
    match after.and_then(|d| ours_defs.get(&d.node)) {
        Some(next) => (next.span.start, false),
        None => (ours.len(), true),
    }
}

/// `ours` with each contested definition given `theirs`' text of it, at
/// the place `ours` has that [`NodeId`] now (which follows a move head made
/// within the file): removed when `theirs` has no such definition, and
/// re-added at its old place ([`old_place`]) when `ours` has none (head
/// deleted it). A contested definition inside another contested one goes
/// with its parent. `None` when a contested id is in neither.
fn take_theirs(
    ours: &[u8],
    ours_defs: &BTreeMap<NodeId, DefinitionInfo>,
    theirs: &[u8],
    theirs_defs: &BTreeMap<NodeId, DefinitionInfo>,
    base_defs: &BTreeMap<NodeId, DefinitionInfo>,
    contested: &BTreeSet<NodeId>,
) -> Option<Vec<u8>> {
    let inside = |node: &NodeId, defs: &BTreeMap<NodeId, DefinitionInfo>| {
        let mut parent = defs.get(node).and_then(|d| d.parent);
        while let Some(p) = parent {
            if contested.contains(&p) {
                return true;
            }
            parent = defs.get(&p).and_then(|d| d.parent);
        }
        false
    };
    // (span in ours, replacement), applied back to front. An insertion is
    // an empty span.
    let mut splices: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
    for node in contested {
        if inside(node, ours_defs) || inside(node, theirs_defs) {
            continue;
        }
        let text = |d: &DefinitionInfo| theirs.get(d.span.clone()).map(<[u8]>::to_vec);
        match (ours_defs.get(node), theirs_defs.get(node)) {
            (Some(o), Some(t)) => {
                // Only the definition's own text: head keeps its trivia
                // around it (indentation at a moved place), and a
                // definition that already has the parked text is left.
                let ours_span = trimmed(ours, o.span.clone())?;
                let theirs_span = trimmed(theirs, t.span.clone())?;
                let body = theirs.get(theirs_span)?.to_vec();
                if ours.get(ours_span.clone())? != body.as_slice() {
                    splices.push((ours_span, body));
                }
            }
            (Some(o), None) => splices.push((o.span.clone(), Vec::new())),
            (None, Some(t)) => {
                let (at, after) = old_place(*node, ours, ours_defs, base_defs);
                let body = text(t)?;
                let mut insert = Vec::new();
                if after {
                    insert.extend_from_slice(b"\n\n");
                    insert.extend(body.strip_suffix(b"\n").unwrap_or(&body));
                } else {
                    insert.extend(&body);
                    insert.extend_from_slice(b"\n\n");
                }
                splices.push((at..at, insert));
            }
            (None, None) => return None,
        }
    }
    splices.sort_by_key(|(span, _)| std::cmp::Reverse((span.start, span.end)));
    let mut out = ours.to_vec();
    let mut floor = usize::MAX;
    for (span, replacement) in splices {
        if span.end > floor || span.end > out.len() {
            // Overlapping spans: not a definition-level conflict.
            return None;
        }
        floor = span.start;
        out.splice(span, replacement);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use hord_core::NodeKind;

    use super::*;

    const A: NodeId = NodeId::from_u128(1);
    const D: NodeId = NodeId::from_u128(2);
    const M: NodeId = NodeId::from_u128(3);
    const Z: NodeId = NodeId::from_u128(4);

    /// `node`'s definition: `snippet`'s place in `text`.
    fn def(node: NodeId, text: &str, snippet: &str, parent: Option<NodeId>) -> DefinitionInfo {
        let start = text.find(snippet).expect("the snippet is in the test text");
        DefinitionInfo {
            node,
            path: "src/lib.rs".parse().expect("parse a literal path"),
            kind: NodeKind::new("function_item"),
            name: None,
            span: start..start + snippet.len(),
            parent,
        }
    }

    fn defs(items: Vec<DefinitionInfo>) -> BTreeMap<NodeId, DefinitionInfo> {
        by_node(items)
    }

    /// Head moved `d` into `mod m` (same NodeId): the parked edit of `d`
    /// goes where head has it now, and nothing is left at the old place.
    #[test]
    fn a_move_within_the_file_is_followed() {
        let base = "fn a() {}\n\nfn d() { 4 }\n";
        let ours = "fn a() {}\n\nmod m {\n    fn d() { 4 }\n}\n";
        let theirs = "fn a() {}\n\nfn d() { 44 }\n";
        let ours_defs = defs(vec![
            def(A, ours, "fn a() {}", None),
            def(M, ours, "mod m {\n    fn d() { 4 }\n}", None),
            def(D, ours, "fn d() { 4 }", Some(M)),
        ]);
        let theirs_defs = defs(vec![
            def(A, theirs, "fn a() {}", None),
            def(D, theirs, "fn d() { 44 }", None),
        ]);
        let base_defs = defs(vec![
            def(A, base, "fn a() {}", None),
            def(D, base, "fn d() { 4 }", None),
        ]);
        let out = take_theirs(
            ours.as_bytes(),
            &ours_defs,
            theirs.as_bytes(),
            &theirs_defs,
            &base_defs,
            &BTreeSet::from([D]),
        );
        assert_eq!(
            out.as_deref(),
            Some("fn a() {}\n\nmod m {\n    fn d() { 44 }\n}\n".as_bytes())
        );
    }

    /// Head deleted `d`: the parked side's `d` comes back at its old place,
    /// between its old neighbors, not at the end of the file.
    #[test]
    fn a_deleted_definition_returns_at_its_old_place() {
        let base = "fn a() {}\n\nfn d() { 4 }\n\nfn z() {}\n";
        let ours = "fn a() {}\n\nfn z() {}\n";
        let theirs = "fn a() {}\n\nfn d() { 44 }\n\nfn z() {}\n";
        let ours_defs = defs(vec![
            def(A, ours, "fn a() {}", None),
            def(Z, ours, "fn z() {}", None),
        ]);
        let theirs_defs = defs(vec![
            def(A, theirs, "fn a() {}", None),
            def(D, theirs, "fn d() { 44 }", None),
            def(Z, theirs, "fn z() {}", None),
        ]);
        let base_defs = defs(vec![
            def(A, base, "fn a() {}", None),
            def(D, base, "fn d() { 4 }", None),
            def(Z, base, "fn z() {}", None),
        ]);
        let out = take_theirs(
            ours.as_bytes(),
            &ours_defs,
            theirs.as_bytes(),
            &theirs_defs,
            &base_defs,
            &BTreeSet::from([D]),
        );
        assert_eq!(out.as_deref(), Some(theirs.as_bytes()));
    }
}
