//! Resolve node specs, blame targets, and the snapshot `query` reads.
//!
//! Node ids accept the ULID text [`NodeId`] displays and a 32-digit hex
//! encoding of the same 128 bits. Qualified names are read from the node
//! objects named by stored identity maps.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use anyhow::{Context, Result, anyhow, bail};
use hord_core::{
    Actor, ChangeRecord, IdentityDelta, IdentityMap, Node, NodeFile, NodeId, ObjectId, Op,
    RepoPath, SnapshotId, Tree, TreeEntry,
};
use hord_store::Store;
use serde::de::DeserializeOwned;

use crate::repo;

/// Where a repository path sits in a snapshot tree.
enum AtPath {
    Absent,
    Directory,
    Blob,
    /// Root [`Node`] object of a [`NodeFile`].
    File(ObjectId),
}

/// Cached identity maps for one command.
pub(crate) struct IdentityCache {
    maps: HashMap<SnapshotId, Option<IdentityMap>>,
}

impl IdentityCache {
    pub(crate) fn new() -> Self {
        Self {
            maps: HashMap::new(),
        }
    }

    fn get<'a>(
        &'a mut self,
        store: &Store,
        snapshot: SnapshotId,
    ) -> Result<Option<&'a IdentityMap>> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.maps.entry(snapshot) {
            slot.insert(store.identity_map(snapshot)?);
        }
        Ok(self.maps.get(&snapshot).and_then(Option::as_ref))
    }
}

/// `Actor` id used by `log --actor` and blame output.
pub(crate) fn actor_id(actor: &Actor) -> &str {
    match actor {
        Actor::Human { id } | Actor::Agent { id, .. } => id,
    }
}

/// Parse a ULID or a 32-digit hex NodeId. `None` means the string is not an id.
pub(crate) fn parse_node_id(spec: &str) -> Option<NodeId> {
    if let Ok(id) = spec.parse::<NodeId>() {
        return Some(id);
    }
    parse_node_hex(spec)
}

/// NodeId for `log --node`: an id, or a qualified name in stored identity maps.
pub(crate) fn resolve_node_spec(store: &Store, spec: &str) -> Result<NodeId> {
    if let Some(id) = parse_node_id(spec) {
        return Ok(id);
    }
    resolve_name(store, spec)
}

/// A blame argument after it has been classified.
pub(crate) enum BlameTarget {
    /// ULID or 32-digit hex. History is [`Store::node_history`] with no scan.
    Node(NodeId),
    /// Qualified name resolved through identity maps and node objects.
    Name(String),
    /// 1-based line in a repository path.
    Line { path: RepoPath, line: u32 },
}

/// Classify `name`, `path:line`, or a NodeId.
pub(crate) fn parse_blame_target(spec: &str) -> Result<BlameTarget> {
    if let Some((path, line)) = split_path_line(spec)? {
        return Ok(BlameTarget::Line { path, line });
    }
    if let Some(id) = parse_node_id(spec) {
        return Ok(BlameTarget::Node(id));
    }
    if spec.is_empty() {
        bail!("blame target is empty");
    }
    Ok(BlameTarget::Name(spec.to_owned()))
}

/// Resolve a blame argument to the definition it names.
pub(crate) fn resolve_blame_target(store: &Store, spec: &str) -> Result<NodeId> {
    match parse_blame_target(spec)? {
        BlameTarget::Node(id) => Ok(id),
        BlameTarget::Name(name) => resolve_name(store, &name),
        BlameTarget::Line { path, line } => resolve_line(store, &path, line),
    }
}

/// Result snapshot of the newest log entry that decodes as a change.
///
/// Entries that are not change records are skipped. This is the snapshot
/// `hord query` passes to [`Store::edges`].
pub(crate) fn latest_result_snapshot(store: &Store) -> Result<SnapshotId> {
    let log = store.log()?;
    for id in log.iter().rev() {
        if let Some(change) = repo::try_change(store, *id)? {
            return Ok(change.result);
        }
    }
    bail!("cannot resolve a snapshot")
}

/// Whether `change` touched `node` the same way [`Store::node_history`] does.
///
/// `read_set` does not count. `write_set`, every [`NodeId`] on an [`Op`], and
/// every [`IdentityDelta`] do.
pub(crate) fn touches_node(change: &ChangeRecord, node: NodeId) -> bool {
    if change.write_set.contains(&node) {
        return true;
    }
    for op in &change.ops {
        let hit = match op {
            Op::Insert { parent, .. } => *parent == node,
            Op::Delete { node: id }
            | Op::Replace { node: id, .. }
            | Op::Rename { node: id, .. } => *id == node,
            Op::Move {
                node: id,
                from_parent,
                to_parent,
                ..
            } => *id == node || *from_parent == node || *to_parent == node,
            Op::Blob { .. } | Op::Tree { .. } => false,
        };
        if hit {
            return true;
        }
    }
    for delta in &change.identity_deltas {
        let hit = match delta {
            IdentityDelta::Birth { node: id } | IdentityDelta::Death { node: id } => *id == node,
            IdentityDelta::DerivedFrom { node: id, from } => *id == node || *from == node,
            IdentityDelta::SplitInto { node: id, into } => *id == node || into.contains(&node),
            IdentityDelta::MergedFrom { node: id, from } => *id == node || from.contains(&node),
        };
        if hit {
            return true;
        }
    }
    false
}

/// `file` equals `filter` or is nested under it. Components, not a string prefix.
pub(crate) fn path_touches(file: &RepoPath, filter: &RepoPath) -> bool {
    let file = file.components();
    let filter = filter.components();
    file.len() >= filter.len() && file[..filter.len()] == filter[..]
}

/// Whether any `write_set` node lives at `filter` in the result identity map,
/// or in the base map when the result map does not place that node.
pub(crate) fn write_set_touches_path(
    store: &Store,
    change: &ChangeRecord,
    filter: &RepoPath,
    cache: &mut IdentityCache,
) -> Result<bool> {
    for node in &change.write_set {
        // Copy the path out before borrowing the cache again for the base map.
        let result_file = cache
            .get(store, change.result)?
            .and_then(|map| map.nodes.get(node).map(|path| path.file.clone()));
        match result_file {
            Some(file) => {
                if path_touches(&file, filter) {
                    return Ok(true);
                }
            }
            None => {
                if node_at_snapshot(store, cache, change.base, *node, filter)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn node_at_snapshot(
    store: &Store,
    cache: &mut IdentityCache,
    snapshot: SnapshotId,
    node: NodeId,
    filter: &RepoPath,
) -> Result<bool> {
    let Some(map) = cache.get(store, snapshot)? else {
        return Ok(false);
    };
    Ok(map
        .nodes
        .get(&node)
        .is_some_and(|path| path_touches(&path.file, filter)))
}

/// Byte offset where `line` (1-based) starts, if that line has any byte.
pub(crate) fn line_start(source: &[u8], line: u32) -> Option<usize> {
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
                if start >= source.len() {
                    return None;
                }
                return Some(start);
            }
        }
    }
    None
}

fn resolve_name(store: &Store, name: &str) -> Result<NodeId> {
    let snapshots = snapshots_newest_first(store)?;
    let mut saw_identity = false;
    let mut roots = HashMap::<(SnapshotId, RepoPath), Option<ObjectId>>::new();
    for snapshot in snapshots {
        let Some(map) = store.identity_map(snapshot)? else {
            continue;
        };
        saw_identity = true;
        let mut hits = Vec::new();
        for (node_id, node_path) in &map.nodes {
            let Some(root) = cached_file_root(store, &mut roots, snapshot, &node_path.file)? else {
                continue;
            };
            let Some(node) = node_at(store, root, &node_path.pointer)? else {
                continue;
            };
            if node.name.as_ref().is_some_and(|q| q.as_str() == name) {
                hits.push(*node_id);
            }
        }
        if hits.len() > 1 {
            hits.sort_unstable();
            let list = hits
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            bail!("qualified name {name:?} resolves to multiple nodes: {list}");
        }
        if let Some(id) = hits.first() {
            return Ok(*id);
        }
    }
    if saw_identity {
        bail!("cannot resolve node {name:?}");
    }
    bail!("cannot resolve node {name:?}: no identity map in the log")
}

fn resolve_line(store: &Store, path: &RepoPath, line: u32) -> Result<NodeId> {
    let (snapshot, map) = latest_identity(store)?;
    let wanted: HashMap<Vec<u32>, NodeId> = map
        .nodes
        .iter()
        .filter(|(_, node_path)| &node_path.file == path)
        .map(|(id, node_path)| (node_path.pointer.clone(), *id))
        .collect();
    if wanted.is_empty() {
        bail!(
            "no identified node in {path} at snapshot {}",
            snapshot.to_hex()
        );
    }
    let root = match locate_file(store, snapshot, path)? {
        AtPath::File(root) => root,
        AtPath::Absent => bail!("{path} not found in snapshot {}", snapshot.to_hex()),
        AtPath::Directory => bail!("{path} is a directory"),
        AtPath::Blob => bail!("{path} is not a parsed file"),
    };
    let projected = project_spans(store, root, &wanted)?;
    let Some(at) = line_start(&projected.source, line) else {
        bail!("line {line} is outside {path}");
    };
    let mut best: Option<(usize, NodeId)> = None;
    for (id, span) in &projected.spans {
        if span.contains(&at) {
            let len = span.end - span.start;
            let replace = best
                .map(|(best_len, best_id)| (len, *id) < (best_len, best_id))
                .unwrap_or(true);
            if replace {
                best = Some((len, *id));
            }
        }
    }
    best.map(|(_, id)| id)
        .ok_or_else(|| anyhow!("no definition covers {path}:{line}"))
}

/// Newest result snapshot that has an identity map.
fn latest_identity(store: &Store) -> Result<(SnapshotId, IdentityMap)> {
    let log = store.log()?;
    let mut saw_change = false;
    for id in log.iter().rev() {
        let Some(change) = repo::try_change(store, *id)? else {
            continue;
        };
        saw_change = true;
        if let Some(map) = store.identity_map(change.result)? {
            return Ok((change.result, map));
        }
    }
    if saw_change {
        bail!("cannot resolve a snapshot: no identity map in the log");
    }
    bail!("cannot resolve a snapshot")
}

struct ChangeEnds {
    base: SnapshotId,
    result: SnapshotId,
}

fn snapshots_newest_first(store: &Store) -> Result<Vec<SnapshotId>> {
    let log = store.log()?;
    let mut ends = Vec::new();
    for id in &log {
        if let Some(change) = repo::try_change(store, *id)? {
            ends.push(ChangeEnds {
                base: change.base,
                result: change.result,
            });
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for change in ends.iter().rev() {
        if seen.insert(change.result) {
            out.push(change.result);
        }
    }
    for change in ends.iter().rev() {
        if seen.insert(change.base) {
            out.push(change.base);
        }
    }
    Ok(out)
}

fn cached_file_root(
    store: &Store,
    cache: &mut HashMap<(SnapshotId, RepoPath), Option<ObjectId>>,
    snapshot: SnapshotId,
    path: &RepoPath,
) -> Result<Option<ObjectId>> {
    let key = (snapshot, path.clone());
    if let Some(hit) = cache.get(&key) {
        return Ok(*hit);
    }
    let root = match locate_file(store, snapshot, path)? {
        AtPath::File(id) => Some(id),
        AtPath::Absent | AtPath::Directory | AtPath::Blob => None,
    };
    cache.insert(key, root);
    Ok(root)
}

fn locate_file(store: &Store, snapshot: SnapshotId, path: &RepoPath) -> Result<AtPath> {
    let mut current = snapshot;
    let components = path.components();
    if components.is_empty() {
        return Ok(AtPath::Absent);
    }
    for (i, name) in components.iter().enumerate() {
        let Some(tree) = try_object::<Tree>(store, current)? else {
            return Ok(AtPath::Absent);
        };
        let Some(entry) = tree.entries.get(name) else {
            return Ok(AtPath::Absent);
        };
        let last = i + 1 == components.len();
        match (entry, last) {
            (TreeEntry::Tree(id), false) => current = *id,
            (TreeEntry::Tree(_), true) => return Ok(AtPath::Directory),
            (TreeEntry::Blob(_), _) => return Ok(AtPath::Blob),
            (TreeEntry::NodeFile(id), true) => {
                let Some(file) = try_object::<NodeFile>(store, *id)? else {
                    return Ok(AtPath::Absent);
                };
                return Ok(AtPath::File(file.root));
            }
            (TreeEntry::NodeFile(_), false) => return Ok(AtPath::Absent),
        }
    }
    Ok(AtPath::Absent)
}

fn node_at(store: &Store, root: ObjectId, pointer: &[u32]) -> Result<Option<Node>> {
    let mut id = root;
    for index in pointer {
        let Some(node) = try_object::<Node>(store, id)? else {
            return Ok(None);
        };
        let Ok(index) = usize::try_from(*index) else {
            return Ok(None);
        };
        let Some(child) = node.children.get(index).copied() else {
            return Ok(None);
        };
        id = child;
    }
    try_object(store, id)
}

struct Projected {
    source: Vec<u8>,
    spans: HashMap<NodeId, Range<usize>>,
}

fn project_spans(
    store: &Store,
    root: ObjectId,
    wanted: &HashMap<Vec<u32>, NodeId>,
) -> Result<Projected> {
    let mut projector = Projector {
        store,
        wanted,
        source: Vec::new(),
        spans: HashMap::new(),
        pointer: Vec::new(),
        offset: 0,
    };
    projector.walk(root)?;
    Ok(Projected {
        source: projector.source,
        spans: projector.spans,
    })
}

struct Projector<'a> {
    store: &'a Store,
    wanted: &'a HashMap<Vec<u32>, NodeId>,
    source: Vec<u8>,
    spans: HashMap<NodeId, Range<usize>>,
    pointer: Vec<u32>,
    offset: usize,
}

impl Projector<'_> {
    fn walk(&mut self, id: ObjectId) -> Result<()> {
        let node = self
            .store
            .get_object::<Node>(id)
            .with_context(|| format!("node {id}"))?;
        let start = self.offset;
        if node.children.is_empty() {
            self.source.extend_from_slice(node.raw.as_slice());
            self.offset += node.raw.len();
        } else {
            for (index, child) in node.children.iter().enumerate() {
                let index = u32::try_from(index).context("node has too many children")?;
                self.pointer.push(index);
                self.walk(*child)?;
                self.pointer.pop();
            }
        }
        if let Some(node_id) = self.wanted.get(self.pointer.as_slice()).copied() {
            self.spans.insert(node_id, start..self.offset);
        }
        Ok(())
    }
}

fn try_object<T: DeserializeOwned>(store: &Store, id: ObjectId) -> Result<Option<T>> {
    match store.get_object(id) {
        Ok(value) => Ok(Some(value)),
        Err(hord_store::Error::MissingObject(_)) | Err(hord_store::Error::Encoding(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn parse_node_hex(spec: &str) -> Option<NodeId> {
    let spec = spec
        .strip_prefix("0x")
        .or_else(|| spec.strip_prefix("0X"))
        .unwrap_or(spec);
    if spec.len() != 32 {
        return None;
    }
    let bytes = spec.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = hex_byte(bytes[i * 2], bytes[i * 2 + 1])?;
    }
    Some(NodeId::from_u128(u128::from_be_bytes(out)))
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_nibble(hi)? << 4 | hex_nibble(lo)?)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn split_path_line(spec: &str) -> Result<Option<(RepoPath, u32)>> {
    let Some((path, line)) = spec.rsplit_once(':') else {
        return Ok(None);
    };
    if path.is_empty() || path.ends_with(':') || line.is_empty() {
        return Ok(None);
    }
    if !line.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let Ok(line) = line.parse::<u64>() else {
        return Ok(None);
    };
    if line == 0 {
        bail!("line number in {spec:?} must be from 1 to {}", u32::MAX);
    }
    let Ok(line) = u32::try_from(line) else {
        bail!("line number in {spec:?} must be from 1 to {}", u32::MAX);
    };
    let path = path
        .parse::<RepoPath>()
        .with_context(|| format!("invalid repository path {path:?}"))?;
    Ok(Some((path, line)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use hord_core::{
        Actor, Bytes, ChangeRecord, IdentityDelta, Intent, NodeId, ObjectId, Op, Provenance,
        RepoPath, Timestamp,
    };

    use super::*;

    fn node(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    fn empty_change() -> ChangeRecord {
        ChangeRecord {
            base: ObjectId::from_bytes([1; 32]),
            result: ObjectId::from_bytes([2; 32]),
            parents: Vec::new(),
            ops: Vec::new(),
            intent: Intent {
                summary: String::new(),
                body: String::new(),
                refs: Vec::new(),
                acceptance: Vec::new(),
            },
            provenance: Provenance {
                actor: Actor::Human { id: "ada".into() },
                toolchain: ObjectId::from_bytes([3; 32]),
                created_at: Timestamp::from_millis(0),
                session: None,
                parent_intent: None,
            },
            read_set: BTreeSet::new(),
            write_set: BTreeSet::new(),
            identity_deltas: Vec::new(),
            evidence: Vec::new(),
            signature: None,
        }
    }

    #[test]
    fn node_id_accepts_ulid_and_hex() {
        let id = node(0xAB);
        assert_eq!(parse_node_id(&id.to_string()), Some(id));
        let hex = format!("{:032x}", id.as_u128());
        assert_eq!(parse_node_id(&hex), Some(id));
        assert_eq!(parse_node_id(&format!("0x{hex}")), Some(id));
        assert_eq!(parse_node_id(&hex.to_ascii_uppercase()), Some(id));
        assert_eq!(parse_node_id("crate::alpha"), None);
        assert_eq!(parse_node_id("deadbeef"), None);
        assert_eq!(parse_node_id(&"ab".repeat(32)), None);
    }

    #[test]
    fn blame_target_splits_path_line_and_names() {
        let line = parse_blame_target("src/lib.rs:12").unwrap();
        match line {
            BlameTarget::Line { path, line } => {
                assert_eq!(path.to_string(), "src/lib.rs");
                assert_eq!(line, 12);
            }
            BlameTarget::Node(_) | BlameTarget::Name(_) => panic!("expected path:line"),
        }
        assert!(matches!(
            parse_blame_target("crate::alpha").unwrap(),
            BlameTarget::Name(name) if name == "crate::alpha"
        ));
        let id = node(1);
        assert!(matches!(
            parse_blame_target(&id.to_string()).unwrap(),
            BlameTarget::Node(parsed) if parsed == id
        ));
        assert!(parse_blame_target("src/lib.rs:0").is_err());
        assert!(parse_blame_target("/src/lib.rs:1").is_err());
    }

    #[test]
    fn line_start_is_one_based_and_rejects_the_empty_line_past_eof() {
        assert_eq!(line_start(b"", 1), None);
        assert_eq!(line_start(b"a", 1), Some(0));
        assert_eq!(line_start(b"a", 2), None);
        assert_eq!(line_start(b"a\n", 1), Some(0));
        assert_eq!(line_start(b"a\n", 2), None);
        assert_eq!(line_start(b"a\nb", 2), Some(2));
        let src = b"fn a() {\n    1\n}\n";
        let at = line_start(src, 2).unwrap();
        assert_eq!(&src[at..at + 4], b"    ");
    }

    #[test]
    fn path_prefix_is_components() {
        let file: RepoPath = "src/lib.rs".parse().unwrap();
        assert!(path_touches(&file, &"src".parse().unwrap()));
        assert!(path_touches(&file, &"src/lib.rs".parse().unwrap()));
        assert!(!path_touches(&file, &"src/lib".parse().unwrap()));
        assert!(!path_touches(&file, &"src2".parse().unwrap()));
        assert!(!path_touches(
            &"src2/lib.rs".parse().unwrap(),
            &"src".parse().unwrap()
        ));
    }

    #[test]
    fn touch_matches_history_rules() {
        let alpha = node(1);
        let beta = node(2);
        let mut change = empty_change();
        change.read_set.insert(alpha);
        assert!(!touches_node(&change, alpha));

        change.write_set.insert(alpha);
        assert!(touches_node(&change, alpha));

        change.write_set.clear();
        change.ops.push(Op::Delete { node: beta });
        assert!(touches_node(&change, beta));
        assert!(!touches_node(&change, alpha));

        change.ops.clear();
        change
            .identity_deltas
            .push(IdentityDelta::Birth { node: alpha });
        assert!(touches_node(&change, alpha));

        change.identity_deltas.clear();
        change.ops.push(Op::Blob {
            path: RepoPath::default(),
            from: None,
            to: None,
        });
        assert!(!touches_node(&change, alpha));
    }

    #[test]
    fn actor_id_is_the_id_field() {
        assert_eq!(actor_id(&Actor::Human { id: "ada".into() }), "ada");
        assert_eq!(
            actor_id(&Actor::Agent {
                id: "agent-7".into(),
                model: "m".into(),
                model_hash: Bytes::default(),
                harness: "h".into(),
            }),
            "agent-7"
        );
    }
}
