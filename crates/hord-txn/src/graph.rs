//! The reference graph of a snapshot for impact sets (spec §6.5): which
//! definitions name a given one, resolved by the Rust resolver.
//!
//! `dependents(x)` has two stages:
//!
//! 1. **Candidates.** A per-snapshot index maps each identifier to the
//!    definitions whose own text (without nested definitions) contains it.
//!    The per-file part is cached by file version, so a snapshot's index is
//!    rebuilt from cached files. Only definitions naming `x`'s simple name
//!    are candidates.
//! 2. **Resolution.** A candidate is a dependent when one of its references
//!    ([`hord_lang::LangAdapter::references_at`]) resolves to `x`, or when
//!    one whose last segment is `x`'s name does not resolve at all: an
//!    unresolvable name might be `x`, and over-approximation is safe for an
//!    impact set while missing a dependent is not. Candidates in files
//!    without the Rust resolver (other languages) are kept.
//!
//! Packages are the nearest enclosing directory with a `Cargo.toml`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use hord_core::{NodeId, ObjectId, RepoPath, SnapshotId};
use hord_lang::{Anchor, LangAdapter};
use hord_verify::ReferenceGraph;

use crate::repo::{Inner, lock};
use crate::semantic::{DefinitionInfo, definitions};
use crate::{Error, Result};

/// Snapshots whose reference index is kept.
const MAX_CACHED_INDEXES: usize = 8;

/// One file's definitions and the identifiers in their own text.
#[derive(Debug, Default)]
pub(crate) struct FileRefs {
    defs: Vec<DefRefs>,
}

#[derive(Debug)]
struct DefRefs {
    node: NodeId,
    name: Option<String>,
    idents: HashSet<String>,
}

/// The reference index of one snapshot.
#[derive(Debug, Default)]
pub(crate) struct RefIndex {
    by_ident: HashMap<String, Vec<(NodeId, RepoPath)>>,
    nodes: HashMap<NodeId, (RepoPath, Option<String>)>,
    /// Directories holding a `Cargo.toml`, as path strings (`""` is the
    /// root).
    packages: HashSet<String>,
}

/// Cache of per-file refs and per-snapshot indexes.
#[derive(Default)]
pub(crate) struct RefCache {
    files: HashMap<(RepoPath, ObjectId, Option<ObjectId>), Arc<FileRefs>>,
    snapshots: BTreeMap<u64, (SnapshotId, Arc<RefIndex>)>,
    tick: u64,
}

/// Identifiers (`[A-Za-z_][A-Za-z0-9_]*`) in `text`. A candidate filter
/// only: resolution decides.
fn identifiers(text: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < text.len() && (text[i].is_ascii_alphanumeric() || text[i] == b'_') {
                i += 1;
            }
            out.insert(String::from_utf8_lossy(&text[start..i]).into_owned());
        } else {
            i += 1;
        }
    }
    out
}

fn simple_name(def: &DefinitionInfo) -> Option<String> {
    def.name.as_ref().map(|n| {
        n.as_str()
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .to_owned()
    })
}

/// Each definition's own text: its span without its nested definitions.
fn file_refs(bytes: &[u8], defs: &[DefinitionInfo]) -> FileRefs {
    let mut out = Vec::with_capacity(defs.len());
    for def in defs {
        let mut own = Vec::with_capacity(def.span.len());
        let mut cursor = def.span.start;
        let mut children: Vec<_> = defs
            .iter()
            .filter(|d| d.parent == Some(def.node))
            .map(|d| d.span.clone())
            .collect();
        children.sort_by_key(|s| s.start);
        for child in children {
            if child.start >= cursor && child.end <= def.span.end {
                own.extend_from_slice(bytes.get(cursor..child.start).unwrap_or_default());
                own.push(b' ');
                cursor = child.end;
            }
        }
        own.extend_from_slice(bytes.get(cursor..def.span.end).unwrap_or_default());
        out.push(DefRefs {
            node: def.node,
            name: simple_name(def),
            idents: identifiers(&own),
        });
    }
    FileRefs { defs: out }
}

impl Inner {
    /// The reference index of `snapshot`, built from cached files.
    pub(crate) fn ref_index(&self, snapshot: SnapshotId) -> Result<Arc<RefIndex>> {
        {
            let mut cache = lock(&self.refs);
            cache.tick += 1;
            let tick = cache.tick;
            let hit = cache
                .snapshots
                .iter()
                .find(|(_, (s, _))| *s == snapshot)
                .map(|(k, (_, index))| (*k, Arc::clone(index)));
            if let Some((old, index)) = hit {
                cache.snapshots.remove(&old);
                cache.snapshots.insert(tick, (snapshot, Arc::clone(&index)));
                return Ok(index);
            }
        }
        let mut index = RefIndex::default();
        let mut live = HashSet::new();
        for (path, blob) in self.list_files(snapshot)? {
            if path.components().last().is_some_and(|n| n == "Cargo.toml") {
                let dir = path.components();
                index.packages.insert(dir[..dir.len() - 1].join("/"));
                continue;
            }
            let key = (path.clone(), blob, self.file_identity(snapshot, &path)?);
            let cached = lock(&self.refs).files.get(&key).cloned();
            let refs = match cached {
                Some(refs) => refs,
                None => {
                    let view = self.file_view(snapshot, &path)?;
                    let refs = match view.as_ref().and_then(|v| Some((v, v.parsed.as_ref()?))) {
                        Some((view, parsed)) => match self.adapter_for(&path, parsed.lang) {
                            Some(adapter) => Arc::new(file_refs(
                                view.bytes.as_slice(),
                                &definitions(adapter, &path, &parsed.tree),
                            )),
                            None => Arc::default(),
                        },
                        None => Arc::default(),
                    };
                    lock(&self.refs)
                        .files
                        .insert(key.clone(), Arc::clone(&refs));
                    refs
                }
            };
            live.insert(key);
            for def in &refs.defs {
                index
                    .nodes
                    .insert(def.node, (path.clone(), def.name.clone()));
                for ident in &def.idents {
                    index
                        .by_ident
                        .entry(ident.clone())
                        .or_default()
                        .push((def.node, path.clone()));
                }
            }
        }
        let index = Arc::new(index);
        let mut cache = lock(&self.refs);
        // Keep file entries some snapshot still uses: this one's, and those
        // of the cached snapshots (they are rebuilt from files on a miss, so
        // dropping others is only a cost).
        if cache.files.len() > 4 * live.len().max(1024) {
            cache.files.retain(|k, _| live.contains(k));
        }
        cache.tick += 1;
        let tick = cache.tick;
        cache.snapshots.insert(tick, (snapshot, Arc::clone(&index)));
        while cache.snapshots.len() > MAX_CACHED_INDEXES {
            cache.snapshots.pop_first();
        }
        Ok(index)
    }
}

/// [`ReferenceGraph`] of one snapshot over the repository's resolver.
pub(crate) struct SnapshotGraph<'a> {
    inner: &'a Inner,
    snapshot: SnapshotId,
    index: Arc<RefIndex>,
}

impl<'a> SnapshotGraph<'a> {
    pub(crate) fn new(inner: &'a Inner, snapshot: SnapshotId) -> Result<Self> {
        Ok(Self {
            inner,
            snapshot,
            index: inner.ref_index(snapshot)?,
        })
    }

    /// Whether `candidate` (in `path`) references `target` named `name`,
    /// by the resolver: resolves to it, or leaves a same-named reference
    /// unresolved. Kept when it cannot be decided.
    fn references(
        &self,
        candidate: NodeId,
        path: &RepoPath,
        target: NodeId,
        name: &str,
    ) -> Result<bool> {
        let Some(parsed) = self.inner.parsed_at(self.snapshot, path)? else {
            return Ok(true);
        };
        if parsed.lang.as_str() != hord_lang_rust::LANG {
            return Ok(true);
        }
        let Some((site, _)) = parsed.tree.ids.iter().find(|(_, id)| **id == candidate) else {
            return Ok(true);
        };
        let Some(node) = parsed.tree.node_at(site) else {
            return Ok(true);
        };
        let ctx = self.inner.rust_ctx(self.snapshot)?;
        let rust = hord_lang_rust::RustAdapter;
        let anchor = Anchor::Definition(candidate);
        for reference in rust.references_at(&ctx.ctx, &anchor, node) {
            match rust.resolve(&ctx.ctx, &reference) {
                Some(resolved) if resolved == target => return Ok(true),
                Some(_) => {}
                None => {
                    let last = reference
                        .name
                        .as_str()
                        .rsplit("::")
                        .next()
                        .unwrap_or_default();
                    if last == name {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

impl ReferenceGraph for SnapshotGraph<'_> {
    fn dependents(&self, node: NodeId) -> hord_verify::Result<Vec<NodeId>> {
        let Some((_, Some(name))) = self.index.nodes.get(&node) else {
            return Ok(Vec::new());
        };
        let Some(candidates) = self.index.by_ident.get(name) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (candidate, path) in candidates {
            if *candidate == node {
                continue;
            }
            let keep = self
                .references(*candidate, path, node, name)
                .map_err(|err: Error| hord_verify::Error::Graph(err.to_string()))?;
            if keep {
                out.push(*candidate);
            }
        }
        Ok(out)
    }

    fn package(&self, node: NodeId) -> Option<String> {
        let (path, _) = self.index.nodes.get(&node)?;
        let parts = path.components();
        (0..parts.len())
            .rev()
            .map(|n| parts[..n].join("/"))
            .find(|dir| self.index.packages.contains(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_words() {
        let ids = identifiers(b"fn a_b() { x1 + _y::z(\"q\") }");
        for w in ["fn", "a_b", "x1", "_y", "z", "q"] {
            assert!(ids.contains(w), "{w}");
        }
        assert!(!ids.contains("1"));
    }
}
