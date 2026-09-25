//! "Take theirs" for an arbiter (spec §6.4 rung 3): the parked change
//! merged onto head definition by definition, with every hard conflict
//! resolved in favor of the parked change's side.
//!
//! For each file the parked change touched:
//!
//! - head did not change it since the change's base: the change's version;
//! - both changed it and it parses on all three sides: the structural 3-way
//!   merge (spec §5.2, the lander's mode). While it reports a hard conflict,
//!   the contested definitions in head's version are replaced by the parked
//!   change's text (removed, when the change deleted them; appended, when
//!   head deleted them), the result is identified against head, and the
//!   merge runs again. Every definition not in contention stays as the
//!   merge produces it, so a non-conflicting edit head gained in the same
//!   file (a third agent's landed change) is kept;
//! - otherwise (a blob-tier file, one that does not parse, a conflict on
//!   the file itself rather than a definition in it, or a file deleted on
//!   one side): the line merge for a blob, else the change's whole file.
//!   These files are reported ([`TheirsMerge::whole_file`]).

use std::collections::{BTreeMap, BTreeSet};
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
            let merged = match self.merge_definitions(record, head, &path)? {
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
        Ok(out)
    }

    /// The structural merge of `path` with the parked change winning hard
    /// conflicts. `None` when the file is not parsed on every side, or a
    /// conflict is not on a definition hord can take from one side.
    fn merge_definitions(
        &self,
        record: &ChangeRecord,
        head: SnapshotId,
        path: &RepoPath,
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
        let theirs_defs = by_node(definitions(adapter, path, &theirs.tree));
        let theirs_text = adapter.project(&theirs.tree.tree);
        let mut ours: Arc<IdentifiedTree> = Arc::clone(&head_view.tree);
        for _ in 0..MAX_ROUNDS {
            let conflict = match hord_diff::merge(
                adapter,
                path,
                &base.tree,
                &ours,
                &theirs.tree,
                hord_diff::MergeMode::Lander,
            ) {
                Ok(merged) => return Ok(Some(adapter.project(&merged.tree.tree))),
                Err(conflict) => conflict,
            };
            let contested: BTreeSet<NodeId> = conflict.nodes.iter().copied().collect();
            if contested.is_empty() || contested.contains(&root) {
                return Ok(None);
            }
            let ours_text = adapter.project(&ours.tree);
            let ours_defs = by_node(definitions(adapter, path, &ours));
            let Some(next) = take_theirs(
                ours_text.as_slice(),
                &ours_defs,
                theirs_text.as_slice(),
                &theirs_defs,
                &contested,
            ) else {
                return Ok(None);
            };
            if next == ours_text.as_slice() {
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
        let blob = self.put_blob(&bytes)?;
        let Some(tree) = self.parse(adapter, blob, &bytes) else {
            return Ok(None);
        };
        let mapping = carry(adapter, path, head, head_tree, &tree)?;
        Ok(Some(Arc::new(IdentifiedTree::new(
            (*tree).clone(),
            mapping.nodes,
        ))))
    }
}

fn by_node(defs: Vec<DefinitionInfo>) -> BTreeMap<NodeId, DefinitionInfo> {
    defs.into_iter().map(|d| (d.node, d)).collect()
}

/// `ours` with each contested definition replaced by `theirs`' text of it:
/// removed when `theirs` has no such definition, appended when `ours` has
/// none. A contested definition inside another contested one goes with its
/// parent. `None` when a contested id is in neither.
fn take_theirs(
    ours: &[u8],
    ours_defs: &BTreeMap<NodeId, DefinitionInfo>,
    theirs: &[u8],
    theirs_defs: &BTreeMap<NodeId, DefinitionInfo>,
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
    // (span in ours, replacement), applied back to front.
    let mut splices: Vec<(std::ops::Range<usize>, Vec<u8>)> = Vec::new();
    let mut appended: Vec<u8> = Vec::new();
    for node in contested {
        if inside(node, ours_defs) || inside(node, theirs_defs) {
            continue;
        }
        let text = |d: &DefinitionInfo, bytes: &[u8]| bytes.get(d.span.clone()).map(<[u8]>::to_vec);
        match (ours_defs.get(node), theirs_defs.get(node)) {
            (Some(o), Some(t)) => splices.push((o.span.clone(), text(t, theirs)?)),
            (Some(o), None) => splices.push((o.span.clone(), Vec::new())),
            (None, Some(t)) => {
                appended.extend_from_slice(b"\n");
                appended.extend(text(t, theirs)?);
            }
            (None, None) => return None,
        }
    }
    splices.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
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
    if !appended.is_empty() {
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend(appended);
    }
    Some(out)
}
