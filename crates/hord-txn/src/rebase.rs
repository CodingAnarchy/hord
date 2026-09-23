//! Structural rebase onto `head` (spec §5.2, §6.4 rung 1).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{Bytes, ChangeRecord, NodeId, ObjectId, Op, RepoPath, SnapshotId};
use hord_lang::{IdentifiedTree, Site};

use crate::conflict::{AdapterMerge, MergeConflict, MergeSeverity};
use crate::files::{FileChange, file_changes};
use crate::ids::path_node_id;
use crate::propose::check_reproduces;
use crate::repo::Inner;
use crate::semantic::IdentityIndex;
use crate::{Error, Result};

/// A rebase that produced a tree (possibly with soft conflicts).
#[derive(Debug)]
pub(crate) struct Rebased {
    pub result: SnapshotId,
    pub ops: Vec<Op>,
    pub index: IdentityIndex,
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
    let mut nodes = BTreeSet::from([path_node_id(path)]);
    for snapshot in [record.base, head, record.result] {
        if let Some(parsed) = inner.file_view(snapshot, path)?.and_then(|v| v.parsed) {
            nodes.extend(parsed.tree.ids.values().copied());
        }
    }
    Ok(AdapterMerge {
        path: path.clone(),
        nodes: nodes.into_iter().collect(),
    })
}

/// Re-apply `record` on `head`. A file `head` has not changed since the
/// base takes the change's file and its ops unchanged; a file both sides
/// changed is merged 3-way and its ops are diffed from `head`. `Err` carries
/// the merge outcomes when any is hard.
pub(crate) fn rebase(
    inner: &Inner,
    record: &ChangeRecord,
    head: SnapshotId,
    landed_writes: &BTreeSet<NodeId>,
) -> Result<std::result::Result<Rebased, Vec<MergeConflict>>> {
    let mut index = (*inner.identity_index(head)?).clone();
    let theirs_index = inner.identity_index(record.result)?;
    let mut changes = BTreeMap::new();
    let mut ops = Vec::new();
    let mut outcomes = Vec::new();
    let mut hard = false;
    let mut checked = BTreeSet::new();
    let mut adapter_merged = Vec::new();
    let proposed_here = {
        let proposed = crate::repo::lock(&inner.proposed);
        let id = hord_core::ObjectId::of(record)?;
        proposed.contains(&id)
    };
    let base_index = inner.identity_index(record.base)?;
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
                    match reapply(inner, record, head, file, landed_writes)? {
                        Some(merged) => Ok(merged),
                        None => merge_file(inner, record, head, file, ours),
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
            // The file is as the change found it: its ops apply unchanged.
            let identity = match (file.to, file.has_structural()) {
                (Some(to), true) => Some(theirs_identity(inner, record, &theirs_index, path, to)?),
                _ => None,
            };
            index.set(path, identity);
            changes.insert(path.clone(), file.to);
            // Checked at propose against the same bytes and the same ids.
            if proposed_here && base_index.get(path) == index_before(inner, head, path)? {
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
                identity,
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
                index.set(path, identity);
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
    let result = inner.update_tree(head, &changes)?;
    Ok(Ok(Rebased {
        result,
        ops,
        index,
        soft: outcomes,
        checked,
        adapter_merged,
    }))
}

/// The identity object `head` records for `path`, if any.
fn index_before(inner: &Inner, head: SnapshotId, path: &RepoPath) -> Result<Option<ObjectId>> {
    Ok(inner.identity_index(head)?.get(path))
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
    landed_writes: &BTreeSet<NodeId>,
) -> Result<Option<Merged>> {
    let path = &file.path;
    let root = hord_diff::file_parent(path);
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
        inner.file_view(head, path)?.and_then(|v| v.parsed),
        inner.file_view(record.result, path)?.and_then(|v| v.parsed),
    ) else {
        return Ok(None);
    };
    let Some(adapter) = inner.adapter_for(path, ours.lang) else {
        return Ok(None);
    };
    let Ok(applied) = hord_diff::apply(path, &ours.tree, &ops, &theirs.tree.tree) else {
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
    let unioned = if same_root {
        union_ids(adapter, &applied, &ours.tree, &theirs.tree, &ops)
    } else {
        None
    };
    if let Some(ids) = unioned {
        let identity = inner.put_file_identity(
            path,
            blob,
            &Arc::new(IdentifiedTree::new((*parsed).clone(), ids)),
        )?;
        return Ok(Some(Merged::Clean {
            blob,
            ops,
            identity: Some(identity),
            soft: Vec::new(),
            adapter_merged: false,
        }));
    }
    match finish_parsed(inner, head, path, bytes, Vec::new())? {
        Merged::Hard(_) => Ok(None),
        clean => Ok(Some(clean)),
    }
}

/// Ids for `applied` (head with the change's ops applied; the caller has
/// checked it equals the re-parse). `apply` keeps head's ids at their new
/// sites. Content the change brought in (a replaced definition's body, an
/// inserted definition) takes the ids it has in the change's result,
/// relative to that definition. `None` when a definition site is left
/// without a known id or two sites share one; then identity is carried the
/// slow way.
fn union_ids(
    adapter: &dyn hord_lang::LangAdapter,
    applied: &IdentifiedTree,
    ours: &IdentifiedTree,
    theirs: &IdentifiedTree,
    ops: &[Op],
) -> Option<BTreeMap<Site, NodeId>> {
    let mut ids = applied.ids.clone();
    let mut inserted: BTreeMap<ObjectId, Vec<Site>> = BTreeMap::new();
    for op in ops {
        match op {
            Op::Replace { node, .. } => {
                let at = applied.site_of(*node)?.clone();
                let from = theirs.site_of(*node)?.clone();
                copy_subtree(&mut ids, theirs, &from, &at);
            }
            Op::Insert { node, .. } => {
                inserted.entry(*node).or_default();
            }
            _ => {}
        }
    }
    // Pair each inserted content's sites in the result with its sites in
    // the change, in preorder.
    let known: std::collections::HashSet<NodeId> = ours
        .ids
        .values()
        .chain(theirs.ids.values())
        .copied()
        .collect();
    for (oid, _) in inserted {
        let targets: Vec<Site> = applied
            .ids
            .iter()
            .filter(|(site, id)| !known.contains(id) && applied.oid_at(site) == Some(oid))
            .map(|(site, _)| site.clone())
            .collect();
        let sources: Vec<Site> = theirs
            .ids
            .iter()
            .filter(|(site, id)| {
                !ours.ids.values().any(|o| o == *id) && theirs.oid_at(site) == Some(oid)
            })
            .map(|(site, _)| site.clone())
            .collect();
        if targets.len() != sources.len() {
            return None;
        }
        for (at, from) in targets.iter().zip(&sources) {
            copy_subtree(&mut ids, theirs, from, at);
        }
    }
    // Every definition site has a known id, and no id is at two sites.
    fn check(
        adapter: &dyn hord_lang::LangAdapter,
        tree: &hord_lang::NodeTree,
        oid: ObjectId,
        site: &mut Site,
        ids: &BTreeMap<Site, NodeId>,
        known: &std::collections::HashSet<NodeId>,
        seen: &mut std::collections::HashSet<NodeId>,
    ) -> bool {
        let Some(node) = tree.get(oid) else {
            return false;
        };
        if adapter.is_definition(&node.kind) {
            let Some(id) = ids.get(site.as_slice()) else {
                return false;
            };
            if !known.contains(id) || !seen.insert(*id) {
                return false;
            }
        }
        for (i, child) in node.children.iter().enumerate() {
            site.push(u32::try_from(i).unwrap_or(u32::MAX));
            let ok = check(adapter, tree, *child, site, ids, known, seen);
            site.pop();
            if !ok {
                return false;
            }
        }
        true
    }
    let root = applied.tree.root()?;
    let mut seen = std::collections::HashSet::new();
    if !check(
        adapter,
        &applied.tree,
        root,
        &mut Vec::new(),
        &ids,
        &known,
        &mut seen,
    ) {
        return None;
    }
    ids.retain(|site, _| applied.oid_at(site).is_some());
    Some(ids)
}

/// Replace the ids at and below `at` with `theirs`' ids at and below
/// `from`, at the same relative sites.
fn copy_subtree(
    ids: &mut BTreeMap<Site, NodeId>,
    theirs: &IdentifiedTree,
    from: &[u32],
    at: &[u32],
) {
    ids.retain(|site, _| !site.starts_with(at));
    for (site, id) in &theirs.ids {
        if site.starts_with(from) {
            let mut key = at.to_vec();
            key.extend_from_slice(&site[from.len()..]);
            ids.insert(key, *id);
        }
    }
}

/// The stored identity of `path` in the change's result, or carry it from
/// the base now (a record proposed elsewhere may not have one).
fn theirs_identity(
    inner: &Inner,
    record: &ChangeRecord,
    theirs_index: &IdentityIndex,
    path: &RepoPath,
    to: ObjectId,
) -> Result<ObjectId> {
    if let Some(id) = theirs_index.get(path) {
        return Ok(id);
    }
    let bytes = inner.blob_bytes(to)?;
    let adapter = inner
        .adapter(path, bytes.as_slice())
        .ok_or_else(|| Error::NotParsed(path.clone()))?;
    let tree = inner
        .parse(adapter, to, bytes.as_slice())
        .ok_or_else(|| Error::NotParsed(path.clone()))?;
    let base = parsed_or_empty(inner, record.base, path)?;
    let mapping = hord_identity::carry_in(adapter, path, &base, &tree, &[]).map_err(|source| {
        Error::Identity {
            path: path.clone(),
            source,
        }
    })?;
    inner.put_file_identity(
        path,
        to,
        &Arc::new(IdentifiedTree::new((*tree).clone(), mapping.nodes)),
    )
}

fn parsed_or_empty(
    inner: &Inner,
    snapshot: SnapshotId,
    path: &RepoPath,
) -> Result<Arc<IdentifiedTree>> {
    Ok(inner
        .file_view(snapshot, path)?
        .and_then(|view| view.parsed)
        .map(|parsed| parsed.tree)
        .unwrap_or_default())
}

enum Merged {
    Clean {
        blob: ObjectId,
        ops: Vec<Op>,
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
        return match hord_lang_rust::merge_cargo_lock(&base_bytes, &ours_bytes, &theirs_bytes) {
            Ok(bytes) => Ok(match finish_parsed(inner, head, path, bytes, Vec::new())? {
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
            }),
            Err(hord_lang_rust::CargoLockMergeError::Unsupported { .. }) => {
                finish_blob(inner, path, &base_bytes, &ours_bytes, &theirs_bytes)
            }
            Err(err) => Ok(hard(path, Vec::new(), err.to_string())),
        };
    }

    let adapter = inner.adapter(path, theirs_bytes.as_slice());
    let parsed = match (adapter, file.from) {
        (Some(_), Some(_)) => {
            let base = inner.file_view(record.base, path)?.and_then(|v| v.parsed);
            let ours_view = inner.file_view(head, path)?.and_then(|v| v.parsed);
            let theirs_view = inner.file_view(record.result, path)?.and_then(|v| v.parsed);
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
            finish_parsed(inner, head, path, bytes, soft)
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

/// Store merged parsed bytes: ids carried from `head`, ops diffed from it.
fn finish_parsed(
    inner: &Inner,
    head: SnapshotId,
    path: &RepoPath,
    bytes: Vec<u8>,
    soft: Vec<MergeConflict>,
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
    let mapping = hord_identity::carry_in(adapter, path, &ours, &tree, &[]).map_err(|source| {
        Error::Identity {
            path: path.clone(),
            source,
        }
    })?;
    let ops = hord_diff::diff(path, &ours, &tree, &mapping);
    check_reproduces(adapter, path, &ours, &ops, &tree, &bytes)?;
    let identity = inner.put_file_identity(
        path,
        blob,
        &Arc::new(IdentifiedTree::new((*tree).clone(), mapping.nodes)),
    )?;
    Ok(Merged::Clean {
        blob,
        ops,
        identity: Some(identity),
        soft,
        adapter_merged: false,
    })
}
