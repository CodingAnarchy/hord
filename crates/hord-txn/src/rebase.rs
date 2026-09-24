//! Structural rebase onto `head` (spec §5.2, §6.4 rung 1).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{Bytes, ChangeRecord, IdentityDelta, NodeId, ObjectId, Op, RepoPath, SnapshotId};
use hord_lang::{IdentifiedTree, IdentityMapping, NodeTree, Site};

use crate::conflict::{AdapterMerge, MergeConflict, MergeSeverity};
use crate::files::{FileChange, file_changes};
use crate::propose::check_reproduces;
use crate::repo::Inner;
use crate::semantic::carry;
use crate::snapshot::IdentityEdits;
use crate::{Error, Result};

/// A rebase that produced a tree (possibly with soft conflicts).
#[derive(Debug)]
pub(crate) struct Rebased {
    /// The result snapshot, stored (its identity tree included).
    pub result: SnapshotId,
    pub ops: Vec<Op>,
    pub soft: Vec<MergeConflict>,
    /// Files whose landed ops were checked to reproduce their result from
    /// head during the rebase; landing validation need not re-apply them.
    pub checked: BTreeSet<RepoPath>,
    /// Files a purpose-built adapter merge resolved with no conflict.
    pub adapter_merged: Vec<AdapterMerge>,
}

/// `path`'s record for [`ConflictReport::adapter_merged`]: the file root and
/// every definition id the file has at the change's base, at `head`, or in
/// the change's result. A set conflict between the change and `L` on this
/// file names only these ids (each side's sets name ids of its own trees).
fn adapter_merge(
    inner: &Inner,
    record: &ChangeRecord,
    head: SnapshotId,
    path: &RepoPath,
) -> Result<AdapterMerge> {
    let mut nodes = BTreeSet::from([NodeId::file_root(path)]);
    for snapshot in [record.base, head, record.result] {
        if let Some(parsed) = inner.parsed_at(snapshot, path)? {
            nodes.extend(parsed.tree.ids.values().copied());
        }
    }
    Ok(AdapterMerge {
        path: path.clone(),
        nodes: nodes.into_iter().collect(),
    })
}

/// Re-apply `record` on `head`. A file `head` has not changed since the
/// base takes the change's file, its ops, and its identity unchanged; a file
/// both sides changed is merged 3-way and its ops are diffed from `head`.
/// `Err` carries the merge outcomes when any is hard.
///
/// Identity in a merged file is carried from `head`. A definition the change
/// gave birth to keeps its birth id unless a landed change wrote that id (a
/// concurrent identical birth, ADR 0019); then it is re-derived with the
/// head snapshot.
pub(crate) fn rebase(
    inner: &Inner,
    record: &ChangeRecord,
    head: SnapshotId,
    landed_writes: &BTreeSet<NodeId>,
) -> Result<std::result::Result<Rebased, Vec<MergeConflict>>> {
    let mut identity = IdentityEdits::new();
    let mut changes = BTreeMap::new();
    let mut ops = Vec::new();
    let mut outcomes = Vec::new();
    let mut hard = false;
    let mut checked = BTreeSet::new();
    let mut adapter_merged = Vec::new();
    let proposed_here = inner.was_proposed(hord_core::ObjectId::of(record)?);
    let own = Own {
        births: record
            .identity_deltas
            .iter()
            .filter_map(|delta| match delta {
                IdentityDelta::Birth { node } => Some(*node),
                _ => None,
            })
            .collect(),
        landed: landed_writes,
    };
    let own = &own;
    let files = file_changes(inner, record)?;
    let ours_of = files
        .iter()
        .map(|file| inner.blob_id(head, &file.path))
        .collect::<Result<Vec<_>>>()?;
    // Files both sides changed are independent of each other: re-apply or
    // merge them in parallel, then fold the results in path order.
    let work: Vec<usize> = (0..files.len())
        .filter(|i| ours_of[*i] != files[*i].to && ours_of[*i] != files[*i].from)
        .collect();
    let mut merged_files: BTreeMap<usize, Merged> = BTreeMap::new();
    let results: Vec<(usize, Result<Merged>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = work
            .iter()
            .map(|&i| {
                let (file, ours) = (&files[i], ours_of[i]);
                let job = move || -> Result<Merged> {
                    // Spec §6.3: ops on definitions no landed change wrote
                    // re-apply on head. Only a file whose ops name something
                    // `L` wrote (or replace its glue) needs the 3-way merge.
                    match reapply(inner, record, head, file, own)? {
                        Some(merged) => Ok(merged),
                        None => merge_file(inner, record, head, file, ours, own),
                    }
                };
                (i, scope.spawn(job))
            })
            .collect();
        handles
            .into_iter()
            .map(|(i, handle)| {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err(Error::Task("file merge panicked".into())));
                (i, result)
            })
            .collect()
    });
    for (i, result) in results {
        merged_files.insert(i, result?);
    }
    for (i, file) in files.into_iter().enumerate() {
        let path = &file.path;
        let ours = ours_of[i];
        if ours == file.to {
            continue;
        }
        if ours == file.from {
            // The file is as the change found it: its ops and its ids apply
            // unchanged.
            let theirs = match file.to {
                Some(_) => inner.file_identity(record.result, path)?,
                None => None,
            };
            identity.insert(path.clone(), theirs);
            changes.insert(path.clone(), file.to);
            // Checked at propose against the same bytes and the same ids.
            if proposed_here
                && inner.file_identity(record.base, path)? == inner.file_identity(head, path)?
            {
                checked.insert(path.clone());
            }
            ops.extend(file.ops);
            continue;
        }
        let Some(merged) = merged_files.remove(&i) else {
            continue;
        };
        match merged {
            Merged::Clean {
                blob,
                ops: file_ops,
                identity: file_identity,
                soft,
                adapter_merged: by_adapter,
            } => {
                if by_adapter {
                    adapter_merged.push(adapter_merge(inner, record, head, path)?);
                }
                outcomes.extend(soft);
                if Some(blob) == ours {
                    continue;
                }
                identity.insert(path.clone(), file_identity);
                changes.insert(path.clone(), Some(blob));
                if file_ops.is_empty() {
                    // Blob tier: the merged bytes are the op.
                    ops.push(Op::Blob {
                        path: path.clone(),
                        from: ours,
                        to: Some(blob),
                    });
                } else {
                    // Checked against head in `finish_parsed`, or built by
                    // `apply` from these ops.
                    checked.insert(path.clone());
                    ops.extend(file_ops);
                }
            }
            Merged::Hard(conflict) => {
                hard = true;
                outcomes.push(conflict);
            }
        }
    }
    if hard {
        return Ok(Err(outcomes));
    }
    let result = inner.commit_snapshot(head, &changes, &identity)?;
    Ok(Ok(Rebased {
        result,
        ops,
        soft: outcomes,
        checked,
        adapter_merged,
    }))
}

/// The change's own births, and what `L` wrote: a birth `L` also wrote was
/// born concurrently from the same inputs (ADR 0019).
struct Own<'a> {
    births: BTreeSet<NodeId>,
    landed: &'a BTreeSet<NodeId>,
}

/// Re-apply the change's ops for `file` on head's version of it (spec §6.3
/// trivial rebase). `None` when the fast path does not apply: an op names a
/// node `L` wrote, the ops replace the file root (glue or whole file), the
/// file is not parsed on both sides, or the ops do not apply or re-parse.
fn reapply(
    inner: &Inner,
    record: &ChangeRecord,
    head: SnapshotId,
    file: &FileChange,
    own: &Own<'_>,
) -> Result<Option<Merged>> {
    let landed_writes = own.landed;
    let path = &file.path;
    let root = NodeId::file_root(path);
    let ops: Vec<Op> = file.structural().cloned().collect();
    if ops.is_empty() || file.from.is_none() || file.to.is_none() {
        return Ok(None);
    }
    let names_landed = ops.iter().any(|op| match op {
        Op::Replace { node, .. } => *node == root || landed_writes.contains(node),
        Op::Delete { node } | Op::Rename { node, .. } => landed_writes.contains(node),
        Op::Move {
            node,
            from_parent,
            to_parent,
            ..
        } => [node, from_parent, to_parent]
            .iter()
            .any(|n| **n != root && landed_writes.contains(n)),
        Op::Insert { parent, .. } => *parent != root && landed_writes.contains(parent),
        Op::Blob { .. } | Op::Tree { .. } => true,
    });
    if names_landed {
        return Ok(None);
    }
    let (Some(ours), Some(theirs)) = (
        inner.parsed_at(head, path)?,
        inner.parsed_at(record.result, path)?,
    ) else {
        return Ok(None);
    };
    let Some(adapter) = inner.adapter_for(path, ours.lang) else {
        return Ok(None);
    };
    let Ok(applied) = hord_diff::apply_identified(path, &ours.tree, &ops, &theirs.tree) else {
        return Ok(None);
    };
    let bytes = adapter.project(&applied.tree).into_vec();
    let blob = inner.put_blob(&bytes)?;
    let Some(parsed) = inner.parse(adapter, blob, &bytes) else {
        return Ok(None);
    };
    // Spec §5.2: the result re-parses. When the parse is the tree `apply`
    // built (equal Merkle roots), the change's ops reproduce it by
    // construction and each definition keeps the id it has on its side.
    let same_root = parsed.root().is_some() && parsed.root() == applied.tree.root();
    if same_root && fully_identified(adapter, &applied) {
        let identity = inner.put_file_identity(
            adapter,
            path,
            blob,
            &Arc::new(IdentifiedTree::new((*parsed).clone(), applied.ids)),
        )?;
        return Ok(Some(Merged::Clean {
            blob,
            ops,
            identity,
            soft: Vec::new(),
            adapter_merged: false,
        }));
    }
    let theirs = Some(&*theirs.tree);
    match finish_parsed(inner, head, path, bytes, Vec::new(), theirs, own)? {
        Merged::Hard(_) => Ok(None),
        clean => Ok(Some(clean)),
    }
}

/// Whether `tree` (head with the change's ops applied, ids placed by
/// [`hord_diff::apply_identified`]) gives every definition site an id and no
/// id to two sites. Otherwise identity is carried the slow way.
fn fully_identified(adapter: &dyn hord_lang::LangAdapter, tree: &IdentifiedTree) -> bool {
    fn walk(
        adapter: &dyn hord_lang::LangAdapter,
        tree: &IdentifiedTree,
        oid: ObjectId,
        site: &mut Site,
        seen: &mut std::collections::HashSet<NodeId>,
    ) -> bool {
        let Some(node) = tree.tree.get(oid) else {
            return false;
        };
        if adapter.is_definition(&node.kind) {
            let Some(id) = tree.ids.get(site.as_slice()) else {
                return false;
            };
            if !seen.insert(*id) {
                return false;
            }
        }
        for (i, child) in node.children.iter().enumerate() {
            site.push(u32::try_from(i).unwrap_or(u32::MAX));
            let ok = walk(adapter, tree, *child, site, seen);
            site.pop();
            if !ok {
                return false;
            }
        }
        true
    }
    let Some(root) = tree.tree.root() else {
        return false;
    };
    walk(
        adapter,
        tree,
        root,
        &mut Vec::new(),
        &mut std::collections::HashSet::new(),
    )
}

fn parsed_or_empty(
    inner: &Inner,
    snapshot: SnapshotId,
    path: &RepoPath,
) -> Result<Arc<IdentifiedTree>> {
    Ok(inner
        .parsed_at(snapshot, path)?
        .map(|parsed| parsed.tree)
        .unwrap_or_default())
}

enum Merged {
    Clean {
        blob: ObjectId,
        ops: Vec<Op>,
        /// The file's identity entry (`None`: the fresh assignment).
        identity: Option<ObjectId>,
        soft: Vec<MergeConflict>,
        /// Resolved by the file's purpose-built adapter merge with no
        /// conflict (ADR 0013 fail-closed exemption).
        adapter_merged: bool,
    },
    Hard(MergeConflict),
}

fn hard(path: &RepoPath, nodes: Vec<hord_core::NodeId>, reason: impl Into<String>) -> Merged {
    Merged::Hard(MergeConflict {
        path: path.clone(),
        severity: MergeSeverity::Hard,
        nodes,
        reason: reason.into(),
    })
}

/// 3-way merge of one file that both `head` and the change edited.
fn merge_file(
    inner: &Inner,
    record: &ChangeRecord,
    head: SnapshotId,
    file: &FileChange,
    ours: Option<ObjectId>,
    own: &Own<'_>,
) -> Result<Merged> {
    let path = &file.path;
    let (Some(ours), Some(theirs)) = (ours, file.to) else {
        return Ok(hard(
            path,
            Vec::new(),
            "file deleted on one side and changed on the other",
        ));
    };
    let base_bytes = match file.from {
        Some(id) => inner.blob_bytes(id)?,
        None => Bytes::default(),
    };
    let ours_bytes = inner.blob_bytes(ours)?;
    let theirs_bytes = inner.blob_bytes(theirs)?;

    if hord_lang_rust::is_cargo_lock(path) {
        // ADR 0013: the lockfile merge, not the generic structural one.
        let theirs_tree = parsed_or_empty(inner, record.result, path)?;
        return match hord_lang_rust::merge_cargo_lock(&base_bytes, &ours_bytes, &theirs_bytes) {
            Ok(bytes) => Ok(
                match finish_parsed(
                    inner,
                    head,
                    path,
                    bytes,
                    Vec::new(),
                    Some(&theirs_tree),
                    own,
                )? {
                    Merged::Clean {
                        blob,
                        ops,
                        identity,
                        soft,
                        ..
                    } => Merged::Clean {
                        adapter_merged: soft.is_empty(),
                        blob,
                        ops,
                        identity,
                        soft,
                    },
                    hard => hard,
                },
            ),
            Err(hord_lang_rust::CargoLockMergeError::Unsupported { .. }) => {
                finish_blob(inner, path, &base_bytes, &ours_bytes, &theirs_bytes)
            }
            Err(err) => Ok(hard(path, Vec::new(), err.to_string())),
        };
    }

    let adapter = inner.adapter(path, theirs_bytes.as_slice());
    let parsed = match (adapter, file.from) {
        (Some(_), Some(_)) => {
            let base = inner.parsed_at(record.base, path)?;
            let ours_view = inner.parsed_at(head, path)?;
            let theirs_view = inner.parsed_at(record.result, path)?;
            match (base, ours_view, theirs_view) {
                (Some(b), Some(o), Some(t)) => Some((b.tree, o.tree, t.tree)),
                _ => None,
            }
        }
        _ => None,
    };
    let (Some(adapter), Some((base, ours_tree, theirs_tree))) = (adapter, parsed) else {
        return finish_blob(inner, path, &base_bytes, &ours_bytes, &theirs_bytes);
    };
    let merged_result = hord_diff::merge(
        adapter,
        path,
        &base,
        &ours_tree,
        &theirs_tree,
        hord_diff::MergeMode::Lander,
    );
    match merged_result {
        Ok(merged) => {
            let soft = merged
                .soft
                .iter()
                .map(|c| MergeConflict {
                    path: path.clone(),
                    severity: MergeSeverity::Soft,
                    nodes: c.nodes.clone(),
                    reason: c.reason.clone(),
                })
                .collect();
            let bytes = adapter.project(&merged.tree.tree).into_vec();
            finish_parsed(inner, head, path, bytes, soft, Some(&theirs_tree), own)
        }
        Err(conflict) => Ok(hard(path, conflict.nodes.clone(), conflict.reason.clone())),
    }
}

fn finish_blob(
    inner: &Inner,
    path: &RepoPath,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Merged> {
    match hord_diff::merge_blob(base, ours, theirs) {
        Ok(bytes) => {
            let blob = inner.put_blob(&bytes)?;
            Ok(Merged::Clean {
                blob,
                ops: Vec::new(),
                identity: None,
                soft: Vec::new(),
                adapter_merged: false,
            })
        }
        Err(conflict) => Ok(hard(path, Vec::new(), conflict.reason)),
    }
}

/// Store merged parsed bytes: ids carried from `head` (births derived with
/// the head snapshot), except that a birth the change made keeps its id when
/// no landed change wrote it ([`keep_own_births`]); ops diffed from `head`.
fn finish_parsed(
    inner: &Inner,
    head: SnapshotId,
    path: &RepoPath,
    bytes: Vec<u8>,
    soft: Vec<MergeConflict>,
    theirs: Option<&IdentifiedTree>,
    own: &Own<'_>,
) -> Result<Merged> {
    let blob = inner.put_blob(&bytes)?;
    let bytes = Bytes::new(bytes);
    let Some(adapter) = inner.adapter(path, bytes.as_slice()) else {
        return Ok(Merged::Clean {
            blob,
            ops: Vec::new(),
            identity: None,
            soft,
            adapter_merged: false,
        });
    };
    let Some(tree) = inner.parse(adapter, blob, bytes.as_slice()) else {
        return Ok(hard(
            path,
            Vec::new(),
            "merged file does not parse (spec §5.2)",
        ));
    };
    let ours = parsed_or_empty(inner, head, path)?;
    let mut mapping = carry(adapter, path, head, &ours, &tree)?;
    if let Some(theirs) = theirs {
        keep_own_births(&mut mapping, &tree, theirs, own);
    }
    let ops = hord_diff::diff(path, &ours, &tree, &mapping);
    check_reproduces(adapter, path, &ours, &ops, &tree, &bytes)?;
    let identity = inner.put_file_identity(
        adapter,
        path,
        blob,
        &Arc::new(IdentifiedTree::new((*tree).clone(), mapping.nodes)),
    )?;
    Ok(Merged::Clean {
        blob,
        ops,
        identity,
        soft,
        adapter_merged: false,
    })
}

/// Give each definition born in the merge (relative to `head`) the id the
/// change gave it at birth, when the change's result has a definition with
/// the same content and that id is one of the change's births that no
/// landed change wrote. Paired in preorder. A birth `L` also wrote was a
/// concurrent identical birth (ADR 0019) and keeps its head-derived id.
fn keep_own_births(
    mapping: &mut IdentityMapping,
    tree: &NodeTree,
    theirs: &IdentifiedTree,
    own: &Own<'_>,
) {
    let born: BTreeSet<NodeId> = mapping
        .deltas
        .iter()
        .filter_map(|delta| match delta {
            IdentityDelta::Birth { node } => Some(*node),
            _ => None,
        })
        .collect();
    if born.is_empty() || own.births.is_empty() {
        return;
    }
    let mut free: BTreeMap<ObjectId, Vec<NodeId>> = BTreeMap::new();
    for (site, id) in &theirs.ids {
        if own.births.contains(id)
            && !own.landed.contains(id)
            && let Some(oid) = theirs.oid_at(site)
        {
            free.entry(oid).or_default().push(*id);
        }
    }
    let used: BTreeSet<NodeId> = mapping.nodes.values().copied().collect();
    let mut replace: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    for (site, id) in &mapping.nodes {
        if !born.contains(id) {
            continue;
        }
        let Some(oid) = hord_lang::oid_at(tree, site) else {
            continue;
        };
        let Some(ids) = free.get_mut(&oid) else {
            continue;
        };
        if let Some(pos) = ids.iter().position(|own| !used.contains(own)) {
            replace.insert(*id, ids.remove(pos));
        }
    }
    for id in mapping.nodes.values_mut() {
        if let Some(own) = replace.get(id) {
            *id = *own;
        }
    }
    for delta in &mut mapping.deltas {
        if let IdentityDelta::Birth { node } = delta
            && let Some(own) = replace.get(node)
        {
            *node = *own;
        }
    }
}
