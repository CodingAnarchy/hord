//! A record's changes, file by file (spec §3.5, ADR 0015).
//!
//! Which files a change touched comes from the tree diff of `base` →
//! `result`. Each structural op belongs to the file whose definitions (or
//! path-derived root, [`NodeId::file_root`]) it names; ops are not
//! scoped by their position. [`Op::Blob`] and [`Op::Tree`] ops name their
//! path. A file move (ADR 0020) is `Op::Tree { path: from, kind: Rename {
//! to } }`: the target's structural ops apply to the source's base content,
//! less the `Move`s that re-parent its top-level definitions from the source
//! root to the target root.

use std::collections::BTreeMap;
use std::sync::Arc;

use hord_core::{ChangeId, ChangeRecord, NodeId, ObjectId, Op, RepoPath, TreeOpKind};
use hord_lang::IdentifiedTree;

use crate::propose::check_reproduces;
use crate::repo::Inner;
use crate::{Error, Result};

/// One changed file of a record.
#[derive(Clone, Debug)]
pub(crate) struct FileChange {
    pub path: RepoPath,
    pub from: Option<ObjectId>,
    pub to: Option<ObjectId>,
    /// The record's ops for this file, in record order: structural ops and
    /// any `Blob`/`Tree` ops on the path.
    pub ops: Vec<Op>,
    /// For the target of a file move, the source path (ADR 0020).
    pub moved_from: Option<RepoPath>,
    /// For the source of a file move, the target path.
    pub moved_to: Option<RepoPath>,
}

impl FileChange {
    pub fn structural(&self) -> impl Iterator<Item = &Op> {
        self.ops
            .iter()
            .filter(|op| !matches!(op, Op::Blob { .. } | Op::Tree { .. }))
    }

    /// Structural ops to apply to the base content: for a move target, the
    /// re-parenting `Move`s from the source root are left out (the rename
    /// itself does that).
    pub fn applicable(&self) -> Vec<Op> {
        let from_root = self.moved_from.as_ref().map(NodeId::file_root);
        self.structural()
            .filter(
                |op| !matches!(op, Op::Move { from_parent, .. } if Some(*from_parent) == from_root),
            )
            .cloned()
            .collect()
    }

    pub fn has_structural(&self) -> bool {
        self.structural().next().is_some()
    }

    pub fn blob_op(&self) -> Option<(Option<ObjectId>, Option<ObjectId>)> {
        self.ops.iter().find_map(|op| match op {
            Op::Blob { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
    }
}

/// Parsed sides of one changed file, for assigning ops.
struct Sides {
    base: Option<Arc<IdentifiedTree>>,
    result: Option<Arc<IdentifiedTree>>,
}

/// Split `record` into its changed files. `Err` names an op that belongs to
/// no changed file.
pub(crate) fn file_changes(inner: &Inner, record: &ChangeRecord) -> Result<Vec<FileChange>> {
    let deltas = inner.changed_paths(record.base, record.result)?;
    let mut changes: Vec<FileChange> = deltas
        .into_iter()
        .map(|d| FileChange {
            path: d.path,
            from: d.from,
            to: d.to,
            ops: Vec::new(),
            moved_from: None,
            moved_to: None,
        })
        .collect();
    let by_path: BTreeMap<RepoPath, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.path.clone(), i))
        .collect();
    for op in &record.ops {
        if let Op::Tree {
            path,
            kind: TreeOpKind::Rename { to },
        } = op
            && let (Some(&from), Some(&target)) = (by_path.get(path), by_path.get(to))
        {
            changes[from].moved_to = Some(to.clone());
            changes[target].moved_from = Some(path.clone());
        }
    }
    let mut sides = Vec::with_capacity(changes.len());
    let mut owners: BTreeMap<NodeId, Vec<usize>> = BTreeMap::new();
    for (i, change) in changes.iter().enumerate() {
        let tree = |snapshot, path| -> Result<Option<Arc<IdentifiedTree>>> {
            Ok(inner.parsed_at(snapshot, path)?.map(|p| p.tree))
        };
        let side = Sides {
            base: tree(
                record.base,
                change.moved_from.as_ref().unwrap_or(&change.path),
            )?,
            result: tree(record.result, &change.path)?,
        };
        owners
            .entry(NodeId::file_root(&change.path))
            .or_default()
            .push(i);
        for tree in [&side.base, &side.result].into_iter().flatten() {
            for node in tree.ids.values() {
                let slot = owners.entry(*node).or_default();
                if slot.last() != Some(&i) {
                    slot.push(i);
                }
            }
        }
        sides.push(side);
    }
    for op in &record.ops {
        let target = match op {
            Op::Blob { path, .. } | Op::Tree { path, .. } => by_path.get(path).copied(),
            _ => owner_of(op, &owners, &sides),
        };
        match target {
            Some(i) => changes[i].ops.push(op.clone()),
            // A Tree op on a directory has no file of its own.
            None if matches!(op, Op::Tree { .. }) => {}
            None => {
                return Err(Error::InvalidChange {
                    change: ChangeId::from_bytes([0; 32]),
                    result: record.result,
                    reason: format!("op names no changed file: {op:?}"),
                });
            }
        }
    }
    Ok(changes)
}

/// The changed file an op names. Identical definitions in two files share a
/// content-derived id; then the file whose trees hold the op's content wins.
fn owner_of(op: &Op, owners: &BTreeMap<NodeId, Vec<usize>>, sides: &[Sides]) -> Option<usize> {
    let key = match op {
        Op::Insert { parent, .. } => *parent,
        Op::Move { to_parent, .. } => *to_parent,
        Op::Delete { node } | Op::Replace { node, .. } | Op::Rename { node, .. } => *node,
        Op::Blob { .. } | Op::Tree { .. } => return None,
    };
    let candidates = owners.get(&key)?;
    if candidates.len() == 1 {
        return candidates.first().copied();
    }
    let holds = |i: &usize, oid: ObjectId, result: bool| {
        let side = if result {
            &sides[*i].result
        } else {
            &sides[*i].base
        };
        side.as_ref().is_some_and(|t| t.tree.contains(oid))
    };
    let pick = match op {
        Op::Replace { from, .. } => candidates.iter().find(|i| holds(i, *from, false)),
        Op::Insert { node, .. } => candidates.iter().find(|i| holds(i, *node, true)),
        _ => None,
    };
    pick.or(candidates.first()).copied()
}

/// Check that `record`'s ops reproduce its result from its base (spec
/// §3.5). Parsed files are checked by applying their structural ops; a
/// `Blob` op stands only for files with no structural ops (blob tier,
/// unparseable, deleted, or a Tier 0 import).
pub(crate) fn validate(inner: &Inner, change: ChangeId, record: &ChangeRecord) -> Result<()> {
    validate_except(inner, change, record, &std::collections::BTreeSet::new())
}

/// [`validate`], without re-applying the structural ops of the files in
/// `checked` (already verified by the caller against the same base).
pub(crate) fn validate_except(
    inner: &Inner,
    change: ChangeId,
    record: &ChangeRecord,
    checked: &std::collections::BTreeSet<RepoPath>,
) -> Result<()> {
    let invalid = |reason: String| Error::InvalidChange {
        change,
        result: record.result,
        reason,
    };
    // ADR 0018: a rebased record names the submitted record it came from.
    if let Some(submitted) = record.rebased_from {
        match inner.change_record(submitted) {
            Ok(_) => {}
            Err(Error::MissingChange(_)) => {
                return Err(invalid(format!(
                    "rebased_from names {submitted}, which is not a stored change record"
                )));
            }
            Err(err) => return Err(err),
        }
    }
    let changes = file_changes(inner, record).map_err(|err| match err {
        Error::InvalidChange { reason, .. } => invalid(reason),
        other => other,
    })?;
    for file in &changes {
        let path = &file.path;
        if let Some((from, to)) = file.blob_op()
            && (from != file.from || to != file.to)
        {
            return Err(invalid(format!(
                "the Blob op on {path} does not match the trees"
            )));
        }
        if let Some(to) = &file.moved_to {
            // The move's source: gone, its content continues at `to`.
            if file.to.is_some() || file.has_structural() {
                return Err(invalid(format!("{path} moved to {to} but still changed")));
            }
            continue;
        }
        if !file.has_structural() && file.moved_from.is_none() {
            if file.blob_op().is_none() {
                return Err(invalid(format!("{path} changed with no op")));
            }
            continue;
        }
        let Some(to) = file.to else {
            return Err(invalid(format!("structural ops on deleted {path}")));
        };
        if checked.contains(path) {
            continue;
        }
        let bytes = inner.blob_bytes(to)?;
        let adapter = inner
            .adapter(path, bytes.as_slice())
            .ok_or_else(|| invalid(format!("structural ops on unparsed {path}")))?;
        let tree = inner
            .parse(adapter, to, bytes.as_slice())
            .ok_or_else(|| invalid(format!("{path} does not parse")))?;
        let base_path = file.moved_from.as_ref().unwrap_or(path);
        let base = match (file.from, &file.moved_from) {
            (Some(_), _) | (None, Some(_)) => inner
                .parsed_at(record.base, base_path)?
                .map(|p| p.tree)
                .ok_or_else(|| invalid(format!("base {base_path} does not parse")))?,
            (None, None) => Arc::new(IdentifiedTree::default()),
        };
        let ops = file.applicable();
        check_reproduces(adapter, path, &base, &ops, &tree, &bytes)
            .map_err(|err| invalid(err.to_string()))?;
    }
    Ok(())
}

/// Path of every coarse (whole-file) write: a `Blob` op on a path whose
/// root no structural op names, and the source of a file move (ADR 0020: a
/// concurrent edit of the old path conflicts with the move). Pure over the
/// record, for footprints.
pub(crate) fn coarse_paths(record: &ChangeRecord) -> Vec<RepoPath> {
    let named: std::collections::BTreeSet<NodeId> =
        record.ops.iter().flat_map(Op::node_ids).collect();
    record
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Blob { path, to, .. }
                if to.is_none() || !named.contains(&NodeId::file_root(path)) =>
            {
                Some(path.clone())
            }
            Op::Tree {
                path,
                kind: TreeOpKind::Rename { .. },
            } => Some(path.clone()),
            _ => None,
        })
        .collect()
}
