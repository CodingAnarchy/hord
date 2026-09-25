//! [`Query`]: read-only questions about landed history (spec §10.1).
//!
//! Every answer reads NodeIds from the snapshots' own objects (ADR 0017):
//! the same identity the lander and `propose` use, whether the snapshot came
//! from `land`, `bootstrap`, or a git import. `hord blame`, `hord log
//! --node`/`--path`, and `hord query` are thin wrappers over this type.

use std::collections::{BTreeSet, HashSet};

use hord_core::{ChangeId, ChangeRecord, NodeId, ObjectId, RepoPath, SnapshotId};
use hord_lang::Anchor;
use hord_store::EdgeKind;

use crate::repo::{Inner, Repo, blocking};
use crate::semantic::enclosing;
use crate::{Error, Result};

/// Read-only queries over a [`Repo`]'s landed history. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Query {
    repo: Repo,
}

impl Repo {
    /// Queries over this repository's history (blame, log filters, edges).
    #[must_use]
    pub fn query(&self) -> Query {
        Query { repo: self.clone() }
    }
}

impl Query {
    /// The definition whose qualified name is `name`, else whose name ends
    /// in `::name`, in the newest snapshot that has one: `head`'s result,
    /// then older landed results, then their bases. Two matches in that
    /// snapshot are [`Error::AmbiguousName`]; none anywhere is
    /// [`Error::UnknownName`].
    pub async fn resolve_name(&self, name: &str) -> Result<NodeId> {
        let name = name.to_owned();
        blocking(&self.repo.inner, move |inner| inner.resolve_name(&name)).await
    }

    /// The innermost definition that covers the start of `line` (1-based)
    /// of `path` at `head`.
    pub async fn resolve_line(&self, path: RepoPath, line: u32) -> Result<NodeId> {
        blocking(&self.repo.inner, move |inner| {
            inner.resolve_line(&path, line)
        })
        .await
    }

    /// Landed changes that touched `node`, in landing order (the store's
    /// `node_history`, which follows the landed records' deltas, ADR 0018).
    pub async fn node_history(&self, node: NodeId) -> Result<Vec<ChangeId>> {
        blocking(&self.repo.inner, move |inner| {
            Ok(inner.store.node_history(node)?)
        })
        .await
    }

    /// Whether `change` wrote something located under `filter`: a
    /// `write_set` node (a definition, or a file's path id) placed at or
    /// below `filter` in its result snapshot, or placed there in its base
    /// when the result no longer places it anywhere.
    pub async fn touches_path(&self, change: ChangeRecord, filter: RepoPath) -> Result<bool> {
        blocking(&self.repo.inner, move |inner| {
            inner.touches_path(&change, &filter)
        })
        .await
    }

    /// Targets of `kind` edges leaving `source` in `snapshot`, in id order.
    ///
    /// Edges are derived (spec §3.8): `References` are resolved by the
    /// language adapter over the snapshot (the same one hop `propose` reads,
    /// ADR 0012), `Contains` from the definitions nested in `source`. Edges
    /// recorded in the store's edge index are included for every kind.
    pub async fn edges(
        &self,
        snapshot: SnapshotId,
        source: NodeId,
        kind: EdgeKind,
    ) -> Result<Vec<NodeId>> {
        blocking(&self.repo.inner, move |inner| {
            inner.edges(snapshot, source, kind)
        })
        .await
    }
}

/// Whether `change` touches `node` the way `node_history` counts it
/// ([`hord_store::touched_nodes`]): its `write_set`, any [`NodeId`] an
/// [`Op`](hord_core::Op) names, or any identity delta. The read set does not count.
pub(crate) fn touches_node(change: &ChangeRecord, node: NodeId) -> bool {
    hord_store::touched_nodes(change).contains(&node)
}

/// Byte offset where `line` (1-based) starts in `source`, if that line has
/// any byte.
#[must_use]
pub fn line_start(source: &[u8], line: u32) -> Option<usize> {
    if line == 0 || source.is_empty() {
        return None;
    }
    if line == 1 {
        return Some(0);
    }
    let mut current = 1u32;
    for (index, byte) in source.iter().enumerate() {
        if *byte == b'\n' {
            current += 1;
            if current == line {
                let start = index + 1;
                return (start < source.len()).then_some(start);
            }
        }
    }
    None
}

impl Inner {
    /// `head`'s result, then every landed result newest first, then their
    /// bases; each once.
    fn snapshots_newest_first(&self) -> Result<Vec<SnapshotId>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let head = self.head()?.snapshot;
        seen.insert(head);
        out.push(head);
        let mut bases = Vec::new();
        for change in self.store.log()?.iter().rev() {
            let record = match self.change_record(*change) {
                Ok(record) => record,
                Err(Error::MissingChange(_)) => continue,
                Err(err) => return Err(err),
            };
            if seen.insert(record.result) {
                out.push(record.result);
            }
            bases.push(record.base);
        }
        for base in bases {
            if seen.insert(base) {
                out.push(base);
            }
        }
        Ok(out)
    }

    fn resolve_name(&self, name: &str) -> Result<NodeId> {
        // A file version (path, blob, identity) that had no match in a newer
        // snapshot has none in an older one either.
        let mut scanned: HashSet<(RepoPath, ObjectId, Option<ObjectId>)> = HashSet::new();
        for snapshot in self.snapshots_newest_first()? {
            let hits = self.named_in(snapshot, name, |path, blob| {
                let key = (path.clone(), blob, self.file_identity(snapshot, path)?);
                Ok(!scanned.insert(key))
            })?;
            match hits.len() {
                0 => {}
                1 => return Ok(hits.into_iter().next().expect("one hit")),
                _ => {
                    return Err(Error::AmbiguousName {
                        name: name.to_owned(),
                        nodes: hits.into_iter().collect(),
                    });
                }
            }
        }
        Err(Error::UnknownName(name.to_owned()))
    }

    /// Definitions of `snapshot` named exactly `name`, else those whose
    /// name ends in `::name`, in id order (the per-snapshot form of
    /// [`Self::resolve_name`], for `RepoBackend::resolve_name`).
    pub(crate) fn resolve_in(&self, snapshot: SnapshotId, name: &str) -> Result<Vec<NodeId>> {
        let hits = self.named_in(snapshot, name, |_, _| Ok(false))?;
        Ok(hits.into_iter().collect())
    }

    /// [`Self::resolve_in`], without reading the files `skip` says to skip.
    fn named_in(
        &self,
        snapshot: SnapshotId,
        name: &str,
        mut skip: impl FnMut(&RepoPath, ObjectId) -> Result<bool>,
    ) -> Result<BTreeSet<NodeId>> {
        let suffix = format!("::{name}");
        let mut exact = BTreeSet::new();
        let mut suffixed = BTreeSet::new();
        for (path, blob) in self.list_files(snapshot)? {
            if skip(&path, blob)? {
                continue;
            }
            for def in self.definitions_at(snapshot, &path)? {
                let Some(qualified) = &def.name else {
                    continue;
                };
                if qualified.as_str() == name {
                    exact.insert(def.node);
                } else if qualified.as_str().ends_with(&suffix) {
                    suffixed.insert(def.node);
                }
            }
        }
        Ok(if exact.is_empty() { suffixed } else { exact })
    }

    fn resolve_line(&self, path: &RepoPath, line: u32) -> Result<NodeId> {
        let snapshot = self.head()?.snapshot;
        let Some(view) = self.file_view(snapshot, path)? else {
            return Err(Error::MissingFile(path.clone()));
        };
        if view.parsed.is_none() {
            return Err(Error::NotParsed(path.clone()));
        }
        let Some(at) = line_start(view.bytes.as_slice(), line) else {
            return Err(Error::LineOutOfRange {
                path: path.clone(),
                line,
            });
        };
        self.definitions_at(snapshot, path)?
            .into_iter()
            .filter(|def| def.span.contains(&at))
            .min_by_key(|def| (def.span.len(), def.node))
            .map(|def| def.node)
            .ok_or_else(|| Error::NoDefinitionAt {
                path: path.clone(),
                line,
            })
    }

    pub(crate) fn touches_path(&self, change: &ChangeRecord, filter: &RepoPath) -> Result<bool> {
        let writes = &change.write_set;
        if writes.is_empty() {
            return Ok(false);
        }
        let in_result = self.ids_under(change.result, filter)?;
        if writes.iter().any(|n| in_result.contains(n)) {
            return Ok(true);
        }
        let in_base = self.ids_under(change.base, filter)?;
        let gone: Vec<NodeId> = writes
            .iter()
            .filter(|n| in_base.contains(n))
            .copied()
            .collect();
        if gone.is_empty() {
            return Ok(false);
        }
        let anywhere = self.ids_under(change.result, &RepoPath::default())?;
        Ok(gone.iter().any(|n| !anywhere.contains(n)))
    }

    /// Path ids and definition ids of every file at or below `prefix` in
    /// `snapshot`.
    fn ids_under(&self, snapshot: SnapshotId, prefix: &RepoPath) -> Result<HashSet<NodeId>> {
        let mut out = HashSet::new();
        for (path, _) in self.files_under(snapshot, prefix)? {
            out.insert(NodeId::file_root(&path));
            if let Some(parsed) = self.parsed_at(snapshot, &path)? {
                out.extend(parsed.tree.ids.values().copied());
            }
        }
        Ok(out)
    }

    pub(crate) fn edges(
        &self,
        snapshot: SnapshotId,
        source: NodeId,
        kind: EdgeKind,
    ) -> Result<Vec<NodeId>> {
        let mut out: BTreeSet<NodeId> = self
            .store
            .edges(snapshot, source, kind)?
            .into_iter()
            .collect();
        if matches!(kind, EdgeKind::References | EdgeKind::Contains) {
            for (path, _) in self.list_files(snapshot)? {
                let Some(parsed) = self.parsed_at(snapshot, &path)? else {
                    continue;
                };
                let Some((site, _)) = parsed.tree.ids.iter().find(|(_, id)| **id == source) else {
                    continue;
                };
                let site = site.clone();
                match kind {
                    EdgeKind::References => {
                        if parsed.lang.as_str() == hord_lang_rust::LANG
                            && let Some(node) = parsed.tree.node_at(&site)
                        {
                            let ctx = self.rust_ctx(snapshot)?;
                            let anchor = Anchor::Definition(source);
                            self.rust_references(&ctx, snapshot, source, &anchor, node, &mut out)?;
                        }
                    }
                    _ => {
                        let parents = enclosing(&parsed.tree);
                        out.extend(
                            parsed
                                .tree
                                .ids
                                .iter()
                                .filter(|(child, _)| parents.get(*child) == Some(&site))
                                .map(|(_, id)| *id),
                        );
                    }
                }
                break;
            }
        }
        out.remove(&source);
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_start_is_one_based_and_rejects_the_empty_line_past_eof() {
        assert_eq!(line_start(b"", 1), None);
        assert_eq!(line_start(b"a", 1), Some(0));
        assert_eq!(line_start(b"a", 2), None);
        assert_eq!(line_start(b"a\n", 1), Some(0));
        assert_eq!(line_start(b"a\n", 2), None);
        assert_eq!(line_start(b"a\nb", 2), Some(2));
    }
}
