//! Interned lossless CSTs (spec §3.3).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use hord_core::{Bytes, LangId, Node, NodeKind, ObjectId, QualifiedName};

use crate::ParseError;
use crate::normalized::{normalized_hash, normalized_of_children};
use crate::trivia::{AttachedSpan, AttachedToken};

/// Trivia-stripped bytes kept so [`NodeTree::stripped`] can return them.
///
/// Leaves store offsets into [`Node::raw`] so the token text is not copied
/// twice. Branches cache the concat of children's stripped bytes. That cache
/// is not an input to an internal node's `normalized` id (ADR 0008).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Stripped {
    /// `node.raw[lead as usize..][..text as usize]`.
    InRaw { lead: u32, text: u32 },
    /// Concat of children, or a leaf whose stripped bytes are not a subslice.
    Owned(Bytes),
}

/// One interned node plus the trivia-stripped bytes used for `normalized`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    node: Node,
    stripped: Stripped,
}

impl Stripped {
    fn bytes<'a>(&'a self, raw: &'a [u8]) -> &'a [u8] {
        match self {
            Self::InRaw { lead, text } => {
                let start = *lead as usize;
                &raw[start..start + *text as usize]
            }
            Self::Owned(bytes) => bytes.as_slice(),
        }
    }
}

impl Entry {
    fn stripped_slice(&self) -> &[u8] {
        self.stripped.bytes(self.node.raw.as_slice())
    }
}

fn find_in_raw(raw: &[u8], stripped: &[u8]) -> Option<(u32, u32)> {
    if stripped.len() > raw.len() {
        return None;
    }
    if stripped.is_empty() {
        return Some((0, 0));
    }
    let lead = raw.windows(stripped.len()).position(|w| w == stripped)?;
    Some((
        u32::try_from(lead).ok()?,
        u32::try_from(stripped.len()).ok()?,
    ))
}

/// An interned syntax tree: [`Node`]s keyed by their content [`ObjectId`].
///
/// Children are [`ObjectId`]s. For every node with children,
/// `concat(children[i].raw) == raw` (spec §3.3). [`intern_branch`](Self::intern_branch)
/// holds this by construction; [`intern`](Self::intern) checks it.
///
/// Leaf nodes hold tokens plus attached trivia (see
/// [`crate::attach_trivia_spans`]). Parsers build trees with
/// [`TreeBuilder`].
///
/// Cloning is O(1): clones share the interned nodes until one of them
/// interns more.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodeTree {
    root: Option<ObjectId>,
    /// Keyed by content id. Iteration order is that key, not insertion order.
    entries: Arc<BTreeMap<ObjectId, Entry>>,
}

impl NodeTree {
    /// An empty tree with no root.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `node` after verifying the concat invariant when `children` is
    /// non-empty.
    ///
    /// `stripped` is the trivia-stripped source of this subtree (leaf token
    /// text, or concat of children's stripped bytes). A leaf's
    /// [`Node::normalized`] is [`crate::normalized_hash`] of that text. An
    /// internal node's is the hash of its children's `normalized` ids.
    pub fn intern(
        &mut self,
        kind: NodeKind,
        lang: LangId,
        raw: Bytes,
        stripped: Bytes,
        children: Vec<ObjectId>,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        if children.is_empty() {
            let form = match find_in_raw(raw.as_slice(), stripped.as_slice()) {
                Some((lead, text)) => Stripped::InRaw { lead, text },
                None => Stripped::Owned(stripped),
            };
            return self.insert_leaf(kind, lang, raw, form, name);
        }
        let entries = self.children(&children)?;
        check_concat_slices(
            kind,
            raw.as_slice(),
            entries.iter().map(|e| e.node.raw.as_slice()),
        )?;
        let normalized = normalized_of(&entries)?;
        let node = Node {
            kind,
            lang,
            raw,
            normalized,
            children,
            name,
        };
        self.insert(node, Stripped::Owned(stripped))
    }

    /// Intern a leaf. Its `normalized` id hashes its stripped text.
    fn insert_leaf(
        &mut self,
        kind: NodeKind,
        lang: LangId,
        raw: Bytes,
        stripped: Stripped,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let normalized = normalized_hash(stripped.bytes(raw.as_slice()));
        let node = Node {
            kind,
            lang,
            raw,
            normalized,
            children: Vec::new(),
            name,
        };
        self.insert(node, stripped)
    }

    fn insert(&mut self, node: Node, stripped: Stripped) -> Result<ObjectId, ParseError> {
        let id = node.content_id()?;
        Arc::make_mut(&mut self.entries)
            .entry(id)
            .or_insert(Entry { node, stripped });
        Ok(id)
    }

    fn children(&self, ids: &[ObjectId]) -> Result<Vec<&Entry>, ParseError> {
        ids.iter()
            .map(|id| self.entries.get(id).ok_or(ParseError::MissingNode(*id)))
            .collect()
    }

    /// Intern a leaf from an attached token.
    ///
    /// `raw` is leading+text+trailing; stripped form is `token.text`.
    pub fn intern_token(
        &mut self,
        lang: LangId,
        token: &AttachedToken,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let lead = u32::try_from(token.leading.len()).unwrap_or(u32::MAX);
        let text = u32::try_from(token.text.len()).unwrap_or(u32::MAX);
        let stripped = Stripped::InRaw { lead, text };
        self.insert_leaf(token.kind, lang, token.raw(), stripped, name)
    }

    /// Intern a parent whose `raw` and stripped bytes are the concat of
    /// `children`. The concat invariant holds by construction.
    ///
    /// A single child reuses that child's `raw` [`std::sync::Arc`].
    pub fn intern_branch(
        &mut self,
        kind: NodeKind,
        lang: LangId,
        children: Vec<ObjectId>,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let entries = self.children(&children)?;
        let (raw, stripped) = if let [only] = entries[..] {
            (only.node.raw.clone(), only.stripped.clone())
        } else {
            let mut raw = Vec::with_capacity(entries.iter().map(|e| e.node.raw.len()).sum());
            let mut stripped =
                Vec::with_capacity(entries.iter().map(|e| e.stripped_slice().len()).sum());
            for entry in &entries {
                raw.extend_from_slice(entry.node.raw.as_slice());
                stripped.extend_from_slice(entry.stripped_slice());
            }
            (Bytes::new(raw), Stripped::Owned(Bytes::new(stripped)))
        };
        let normalized = normalized_of(&entries)?;
        let node = Node {
            kind,
            lang,
            raw,
            normalized,
            children,
            name,
        };
        self.insert(node, stripped)
    }

    /// Copy `id` and its descendants from `src`, skipping nodes already
    /// here. Returns `id`: nodes are keyed by content, so nothing is hashed.
    pub fn graft(&mut self, src: &NodeTree, id: ObjectId) -> Result<ObjectId, ParseError> {
        if self.contains(id) {
            return Ok(id);
        }
        let entry = src.entries.get(&id).ok_or(ParseError::MissingNode(id))?;
        for child in &entry.node.children {
            self.graft(src, *child)?;
        }
        Arc::make_mut(&mut self.entries).insert(id, entry.clone());
        Ok(id)
    }

    /// A tree whose root is a single leaf: `raw == source`, stripped empty.
    ///
    /// Used for empty or trivia-only files (no token to attach trivia to).
    pub fn from_raw_root(kind: NodeKind, lang: LangId, source: &[u8]) -> Result<Self, ParseError> {
        let mut tree = Self::new();
        let stripped = Stripped::InRaw { lead: 0, text: 0 };
        let id = tree.insert_leaf(kind, lang, Bytes::from(source), stripped, None)?;
        tree.set_root(id)?;
        Ok(tree)
    }

    /// Intern each token as a leaf and a `root_kind` parent over them.
    pub fn from_tokens(
        lang: LangId,
        root_kind: NodeKind,
        tokens: &[AttachedToken],
    ) -> Result<Self, ParseError> {
        let mut tree = Self::new();
        let mut children = Vec::with_capacity(tokens.len());
        for token in tokens {
            children.push(tree.intern_token(lang, token, None)?);
        }
        let root = tree.intern_branch(root_kind, lang, children, None)?;
        tree.set_root(root)?;
        Ok(tree)
    }

    /// Set the root to an already-interned node.
    pub fn set_root(&mut self, id: ObjectId) -> Result<(), ParseError> {
        if !self.entries.contains_key(&id) {
            return Err(ParseError::MissingNode(id));
        }
        self.root = Some(id);
        Ok(())
    }

    /// Content id of the root node, if set.
    #[must_use]
    pub fn root(&self) -> Option<ObjectId> {
        self.root
    }

    /// Borrow an interned node.
    #[must_use]
    pub fn get(&self, id: ObjectId) -> Option<&Node> {
        self.entries.get(&id).map(|e| &e.node)
    }

    /// Trivia-stripped bytes of an interned subtree.
    #[must_use]
    pub fn stripped(&self, id: ObjectId) -> Option<&[u8]> {
        self.entries.get(&id).map(Entry::stripped_slice)
    }

    /// Whether `id` is interned.
    #[must_use]
    pub fn contains(&self, id: ObjectId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Number of interned nodes (shared subtrees count once).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no nodes are interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Estimated heap bytes this tree holds, for byte-bounded caches.
    ///
    /// Counts each node's stored bytes (`raw`, an owned stripped copy, child
    /// ids) plus a fixed per-node overhead. A single-child branch shares its
    /// child's `raw`, so it is not counted twice. Clones share one copy, so
    /// a cache holding clones over-estimates, which only evicts earlier.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        // BTreeMap slot, `Entry`, and the `Arc` headers of `raw`/`stripped`.
        const PER_NODE: usize = std::mem::size_of::<(ObjectId, Entry)>() + 48;
        self.entries
            .values()
            .map(|e| {
                let raw = if e.node.children.len() == 1 {
                    0
                } else {
                    e.node.raw.len()
                };
                let stripped = match &e.stripped {
                    Stripped::InRaw { .. } => 0,
                    Stripped::Owned(bytes) => bytes.len(),
                };
                PER_NODE + raw + stripped + e.node.children.len() * ObjectId::LEN
            })
            .sum()
    }

    /// Iterate interned `(id, node)` pairs. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (ObjectId, &Node)> + '_ {
        self.entries.iter().map(|(id, e)| (*id, &e.node))
    }

    /// File projection: the root node's `raw` (spec §3.3 tree walk).
    ///
    /// Empty if no root is set. When the concat invariant holds this equals
    /// the concatenation of every leaf `raw`.
    #[must_use]
    pub fn to_bytes(&self) -> Bytes {
        self.root
            .and_then(|id| self.get(id))
            .map(|n| n.raw.clone())
            .unwrap_or_default()
    }

    /// Borrow the root node's `raw`, if a root is set.
    #[must_use]
    pub fn root_bytes(&self) -> Option<&[u8]> {
        self.root
            .and_then(|id| self.get(id))
            .map(|n| n.raw.as_slice())
    }

    /// Check `concat(children.raw) == raw` for every interned node with children.
    pub fn check_concat(&self) -> Result<(), ParseError> {
        for entry in self.entries.values() {
            if entry.node.children.is_empty() {
                continue;
            }
            let children = self.children(&entry.node.children)?;
            check_concat_slices(
                entry.node.kind,
                entry.node.raw.as_slice(),
                children.iter().map(|e| e.node.raw.as_slice()),
            )?;
        }
        Ok(())
    }
}

/// Builds the [`NodeTree`] of one source file (the parse path).
///
/// A token or branch built from input already seen in this file (the same
/// kind, bytes, and trivia split, or the same kind, name, and children)
/// reuses that node's [`ObjectId`] instead of copying and hashing it again.
/// The tree is the same as interning every node on its own.
pub struct TreeBuilder<'s> {
    tree: NodeTree,
    lang: LangId,
    source: &'s [u8],
    /// `(kind, raw, leading trivia length, text length)` of each token.
    tokens: HashMap<(NodeKind, &'s [u8], usize, usize), ObjectId>,
    branches: HashMap<BranchKey, ObjectId>,
}

/// A branch's inputs (its language is the builder's).
#[derive(Eq, PartialEq)]
struct BranchKey {
    kind: NodeKind,
    name: Option<QualifiedName>,
    children: Vec<ObjectId>,
}

impl std::hash::Hash for BranchKey {
    /// Child ids are BLAKE3 digests, so their first word is enough to spread
    /// keys; equality still compares every field.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        for child in &self.children {
            let (word, _) = child.as_bytes().split_first_chunk::<8>().expect("32 bytes");
            state.write_u64(u64::from_le_bytes(*word));
        }
    }
}

impl<'s> TreeBuilder<'s> {
    /// An empty tree of `lang` over `source`.
    #[must_use]
    pub fn new(lang: LangId, source: &'s [u8]) -> Self {
        Self {
            tree: NodeTree::new(),
            lang,
            source,
            tokens: HashMap::new(),
            branches: HashMap::new(),
        }
    }

    /// Intern a leaf from a span produced by [`crate::attach_trivia_spans`]
    /// over this builder's source.
    pub fn token(&mut self, token: &AttachedSpan) -> Result<ObjectId, ParseError> {
        let raw = self
            .source
            .get(token.raw.clone())
            .ok_or_else(|| ParseError::failed("attached span raw range is outside the source"))?;
        if token.text.start < token.raw.start || token.text.end > token.raw.end {
            return Err(ParseError::failed(
                "attached span text range is not inside raw",
            ));
        }
        let lead = token.text.start - token.raw.start;
        let text = token.text.end - token.text.start;
        let key = (token.kind, raw, lead, text);
        if let Some(id) = self.tokens.get(&key) {
            return Ok(*id);
        }
        let stripped = Stripped::InRaw {
            lead: u32::try_from(lead).unwrap_or(u32::MAX),
            text: u32::try_from(text).unwrap_or(u32::MAX),
        };
        let id = self
            .tree
            .insert_leaf(token.kind, self.lang, Bytes::from(raw), stripped, None)?;
        self.tokens.insert(key, id);
        Ok(id)
    }

    /// Intern a parent over `children` (see [`NodeTree::intern_branch`]).
    pub fn branch(
        &mut self,
        kind: NodeKind,
        children: Vec<ObjectId>,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let key = BranchKey {
            kind,
            name,
            children,
        };
        if let Some(id) = self.branches.get(&key) {
            return Ok(*id);
        }
        let id =
            self.tree
                .intern_branch(kind, self.lang, key.children.clone(), key.name.clone())?;
        self.branches.insert(key, id);
        Ok(id)
    }

    /// The finished tree, rooted at `root`.
    pub fn finish(mut self, root: ObjectId) -> Result<NodeTree, ParseError> {
        self.tree.set_root(root)?;
        Ok(self.tree)
    }
}

/// An internal node's `normalized`: the hash of its children's (ADR 0008).
fn normalized_of(children: &[&Entry]) -> Result<ObjectId, ParseError> {
    let ids: Vec<ObjectId> = children.iter().map(|e| e.node.normalized).collect();
    normalized_of_children(&ids)
}

/// `concat(children) == raw`. `children_raw_len` stays the sum of every child
/// when an earlier slice already disagrees with `raw`.
fn check_concat_slices<'a>(
    kind: NodeKind,
    raw: &[u8],
    children: impl Iterator<Item = &'a [u8]>,
) -> Result<(), ParseError> {
    let mut concat_len = 0usize;
    let mut off = 0usize;
    let mut bytes_match = true;
    for slice in children {
        if bytes_match {
            match raw.get(off..off + slice.len()) {
                Some(got) if got == slice => off += slice.len(),
                _ => bytes_match = false,
            }
        }
        concat_len += slice.len();
    }
    if bytes_match && concat_len == raw.len() {
        Ok(())
    } else {
        Err(ParseError::ConcatInvariant {
            kind,
            raw_len: raw.len(),
            children_raw_len: concat_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalized::{normalized_hash, normalized_of_children};
    use crate::trivia::{Lexeme, attach_trivia};

    fn lang() -> LangId {
        LangId::new("test")
    }

    fn token(kind: &str, text: &str) -> AttachedToken {
        AttachedToken {
            kind: NodeKind::new(kind),
            leading: Bytes::default(),
            text: Bytes::from(text.as_bytes()),
            trailing: Bytes::default(),
        }
    }

    #[test]
    fn intern_branch_concat_equals_raw() {
        let mut tree = NodeTree::new();
        let a = tree
            .intern_token(lang(), &token("ident", "hello"), None)
            .unwrap();
        let b = tree.intern_token(lang(), &token("ws", " "), None).unwrap();
        let c = tree
            .intern_token(lang(), &token("ident", "world"), None)
            .unwrap();
        let root = tree
            .intern_branch(NodeKind::new("file"), lang(), vec![a, b, c], None)
            .unwrap();
        tree.set_root(root).unwrap();

        tree.check_concat().unwrap();
        assert_eq!(tree.to_bytes().as_slice(), b"hello world");

        let parent = tree.get(root).unwrap();
        assert_eq!(parent.children, vec![a, b, c]);
        let concat: Vec<u8> = parent
            .children
            .iter()
            .flat_map(|id| tree.get(*id).unwrap().raw.as_slice().iter().copied())
            .collect();
        assert_eq!(concat, parent.raw.as_slice());
    }

    #[test]
    fn intern_rejects_concat_mismatch() {
        let mut tree = NodeTree::new();
        let a = tree
            .intern_token(lang(), &token("ident", "ab"), None)
            .unwrap();
        let err = tree
            .intern(
                NodeKind::new("file"),
                lang(),
                Bytes::from(b"xyz".as_slice()),
                Bytes::from(b"ab".as_slice()),
                vec![a],
                None,
            )
            .unwrap_err();
        assert!(matches!(err, ParseError::ConcatInvariant { .. }));
    }

    #[test]
    fn identical_leaves_share_object_id() {
        let mut tree = NodeTree::new();
        let a = tree
            .intern_token(lang(), &token("ident", "x"), None)
            .unwrap();
        let b = tree
            .intern_token(lang(), &token("ident", "x"), None)
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn object_id_is_hash_of_canonical_node() {
        let mut tree = NodeTree::new();
        let id = tree
            .intern_token(lang(), &token("ident", "x"), None)
            .unwrap();
        let node = tree.get(id).unwrap();
        assert_eq!(id, ObjectId::of(node).unwrap());
        let parent = tree
            .intern_branch(NodeKind::new("file"), lang(), vec![id], None)
            .unwrap();
        let branch = tree.get(parent).unwrap();
        assert!(!branch.raw.is_empty(), "projection cache is kept in memory");
        assert_eq!(parent, ObjectId::of(branch).unwrap());
    }

    #[test]
    fn trivia_does_not_change_normalized() {
        let lex_ws = [
            Lexeme::trivia(b"  ".as_slice()),
            Lexeme::token("ident", b"foo".as_slice()),
            Lexeme::trivia(b"\n".as_slice()),
        ];
        let lex_bare = [Lexeme::token("ident", b"foo".as_slice())];
        let with_ws = attach_trivia(&lex_ws);
        let bare = attach_trivia(&lex_bare);

        let mut a = NodeTree::new();
        let mut b = NodeTree::new();
        let id_ws = a.intern_token(lang(), &with_ws[0], None).unwrap();
        let id_bare = b.intern_token(lang(), &bare[0], None).unwrap();
        assert_ne!(id_ws, id_bare, "raw differs so content ObjectId differs");
        assert_eq!(
            a.get(id_ws).unwrap().normalized,
            b.get(id_bare).unwrap().normalized
        );
        assert_eq!(a.get(id_ws).unwrap().normalized, normalized_hash(b"foo"));
        assert_eq!(a.get(id_ws).unwrap().raw.as_slice(), b"  foo\n");
        assert_eq!(b.get(id_bare).unwrap().raw.as_slice(), b"foo");

        let parent_a = a
            .intern_branch(NodeKind::new("file"), lang(), vec![id_ws], None)
            .unwrap();
        let parent_b = b
            .intern_branch(NodeKind::new("file"), lang(), vec![id_bare], None)
            .unwrap();
        assert_eq!(
            a.get(parent_a).unwrap().normalized,
            b.get(parent_b).unwrap().normalized,
            "whitespace does not change an internal normalized id"
        );
        assert_ne!(parent_a, parent_b, "ancestor content ids still change");
        assert_eq!(
            a.get(parent_a).unwrap().normalized,
            normalized_of_children(&[a.get(id_ws).unwrap().normalized]).unwrap()
        );
    }

    #[test]
    fn from_tokens_projects_concatenated_raw() {
        let tokens = attach_trivia(&[
            Lexeme::trivia(b"// c\n".as_slice()),
            Lexeme::token("kw", b"fn".as_slice()),
            Lexeme::trivia(b" ".as_slice()),
            Lexeme::token("ident", b"f".as_slice()),
        ]);
        let tree = NodeTree::from_tokens(lang(), NodeKind::new("file"), &tokens).unwrap();
        tree.check_concat().unwrap();
        assert_eq!(tree.to_bytes().as_slice(), b"// c\nfn f");
        let root = tree.root().unwrap();
        assert_eq!(tree.stripped(root).unwrap(), b"fnf");
    }

    #[test]
    fn clones_share_nodes_and_resident_bytes_tracks_source() {
        let source = "fn f() { 1 }\n".repeat(200);
        let lexemes: Vec<Lexeme> = source
            .split_inclusive(' ')
            .map(|w| Lexeme::token("word", w.as_bytes()))
            .collect();
        let tokens = attach_trivia(&lexemes);
        let tree = NodeTree::from_tokens(lang(), NodeKind::new("file"), &tokens).unwrap();
        let copy = tree.clone();
        let root = tree.root().unwrap();
        assert!(
            std::ptr::eq(tree.get(root).unwrap(), copy.get(root).unwrap()),
            "a clone must not deep-copy the interned nodes"
        );
        let bytes = tree.resident_bytes();
        assert!(bytes >= source.len(), "counts at least the root's raw");
        assert!(bytes < source.len() * 64, "estimate stays bounded: {bytes}");
    }

    #[test]
    fn graft_copies_a_subtree_under_the_same_ids() {
        let mut src = NodeTree::new();
        let a = src
            .intern_token(lang(), &token("ident", "a"), None)
            .unwrap();
        let b = src.intern_token(lang(), &token("ws", " "), None).unwrap();
        let pair = src
            .intern_branch(NodeKind::new("pair"), lang(), vec![a, b], None)
            .unwrap();
        let mut dest = NodeTree::new();
        dest.intern_token(lang(), &token("ident", "a"), None)
            .unwrap();
        assert_eq!(dest.graft(&src, pair).unwrap(), pair);
        assert_eq!(dest.len(), 3);
        for id in [a, b, pair] {
            assert_eq!(dest.get(id), src.get(id));
            assert_eq!(dest.stripped(id), src.stripped(id));
        }
        let ghost = ObjectId::from_bytes([0; 32]);
        assert!(matches!(
            dest.graft(&src, ghost),
            Err(ParseError::MissingNode(id)) if id == ghost
        ));
    }

    #[test]
    fn missing_child_is_an_error() {
        let mut tree = NodeTree::new();
        let ghost = ObjectId::from_bytes([0; 32]);
        let err = tree
            .intern_branch(NodeKind::new("file"), lang(), vec![ghost], None)
            .unwrap_err();
        assert!(matches!(err, ParseError::MissingNode(_)));
    }

    #[test]
    fn from_raw_root_projects_source() {
        let tree = NodeTree::from_raw_root(NodeKind::new("file"), lang(), b"// only\n").unwrap();
        assert_eq!(tree.root_bytes(), Some(b"// only\n".as_slice()));
        assert_eq!(tree.stripped(tree.root().unwrap()).unwrap(), b"");
    }

    #[test]
    fn builder_token_copies_once_from_source() {
        use crate::trivia::{TokenSpan, attach_trivia_spans};
        let source = b"  foo\n";
        let spans = attach_trivia_spans(source, vec![TokenSpan::new("ident", 2, 5)]);
        let mut builder = TreeBuilder::new(lang(), source);
        let id = builder.token(&spans[0]).unwrap();
        let tree = builder.finish(id).unwrap();
        let node = tree.get(id).unwrap();
        assert_eq!(node.raw.as_slice(), source);
        assert_eq!(tree.stripped(id).unwrap(), b"foo");
        let stripped = tree.stripped(id).unwrap();
        assert!(
            std::ptr::eq(stripped.as_ptr(), node.raw[2..].as_ptr()),
            "stripped text must be a subslice of raw, not a second copy"
        );
    }

    #[test]
    fn builder_reuses_repeated_tokens_and_branches() {
        use crate::trivia::{TokenSpan, attach_trivia_spans};
        let source = b"a a b a a b ";
        let spans = attach_trivia_spans(
            source,
            [0, 2, 4, 6, 8, 10]
                .into_iter()
                .map(|i| TokenSpan::new("word", i, i + 1))
                .collect(),
        );
        let mut built = TreeBuilder::new(lang(), source);
        let mut plain = NodeTree::new();
        let mut pairs = Vec::new();
        for pair in spans.chunks(3) {
            let ids: Vec<ObjectId> = pair.iter().map(|t| built.token(t).unwrap()).collect();
            let kids: Vec<ObjectId> = pair
                .iter()
                .map(|t| {
                    let raw = &source[t.raw.clone()];
                    let token = AttachedToken {
                        kind: t.kind,
                        leading: Bytes::default(),
                        text: Bytes::from(&source[t.text.clone()]),
                        trailing: Bytes::from(&raw[t.text.end - t.raw.start..]),
                    };
                    plain.intern_token(lang(), &token, None).unwrap()
                })
                .collect();
            assert_eq!(ids, kids);
            let group = built.branch(NodeKind::new("group"), ids, None).unwrap();
            let plain_group = plain
                .intern_branch(NodeKind::new("group"), lang(), kids, None)
                .unwrap();
            assert_eq!(group, plain_group);
            pairs.push(group);
        }
        assert_eq!(pairs[0], pairs[1], "identical input reuses one id");
        let root = built
            .branch(NodeKind::new("file"), pairs.clone(), None)
            .unwrap();
        let plain_root = plain
            .intern_branch(NodeKind::new("file"), lang(), pairs, None)
            .unwrap();
        plain.set_root(plain_root).unwrap();
        assert_eq!(built.finish(root).unwrap(), plain);
    }
}
