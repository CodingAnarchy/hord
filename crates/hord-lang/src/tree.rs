//! Interned lossless CSTs (spec §3.3).

use std::collections::BTreeMap;
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

fn stripped_form(raw: &Bytes, stripped: &Bytes, is_leaf: bool) -> Stripped {
    if is_leaf && let Some((lead, text)) = find_in_raw(raw.as_slice(), stripped.as_slice()) {
        return Stripped::InRaw { lead, text };
    }
    Stripped::Owned(stripped.clone())
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
/// Leaf nodes hold tokens plus attached trivia (see [`crate::attach_trivia`]).
///
/// Cloning is O(1): clones share the interned nodes until one of them
/// interns more (perf review #3: an identified view no longer deep-copies
/// the tree it wraps).
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
        if !children.is_empty() {
            check_concat_slices(
                kind,
                raw.as_slice(),
                children.iter().map(|id| {
                    self.entries
                        .get(id)
                        .map(|child| child.node.raw.as_slice())
                        .ok_or(ParseError::MissingNode(*id))
                }),
            )?;
        }

        let form = stripped_form(&raw, &stripped, children.is_empty());
        self.insert_node(kind, lang, raw, form, children, name)
    }

    fn insert_node(
        &mut self,
        kind: NodeKind,
        lang: LangId,
        raw: Bytes,
        stripped: Stripped,
        children: Vec<ObjectId>,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let normalized = if children.is_empty() {
            normalized_hash(stripped.bytes(raw.as_slice()))?
        } else {
            let mut child_norm = Vec::with_capacity(children.len());
            for id in &children {
                let child = self.entries.get(id).ok_or(ParseError::MissingNode(*id))?;
                child_norm.push(child.node.normalized);
            }
            normalized_of_children(&child_norm)?
        };
        let node = Node {
            kind,
            lang,
            raw,
            normalized,
            children,
            name,
        };
        let id = node.content_id()?;
        Arc::make_mut(&mut self.entries)
            .entry(id)
            .or_insert(Entry { node, stripped });
        Ok(id)
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
        let raw = token.raw();
        let lead = u32::try_from(token.leading.len()).unwrap_or(u32::MAX);
        let text = u32::try_from(token.text.len()).unwrap_or(u32::MAX);
        self.insert_node(
            token.kind,
            lang,
            raw,
            Stripped::InRaw { lead, text },
            Vec::new(),
            name,
        )
    }

    /// Intern a leaf from source ranges produced by [`crate::attach_trivia_spans`].
    ///
    /// Copies `source[token.raw]` once. Trivia-stripped text is a subslice of
    /// that buffer (`lead_len` / `text_len`). Prefer this on the parse path
    /// over [`Self::intern_token`].
    pub fn intern_source_token(
        &mut self,
        lang: LangId,
        source: &[u8],
        token: &AttachedSpan,
        name: Option<QualifiedName>,
    ) -> Result<ObjectId, ParseError> {
        let raw = source
            .get(token.raw.start..token.raw.end)
            .ok_or_else(|| ParseError::failed("attached span raw range is outside the source"))?;
        if token.text.start < token.raw.start || token.text.end > token.raw.end {
            return Err(ParseError::failed(
                "attached span text range is not inside raw",
            ));
        }
        let lead = u32::try_from(token.text.start - token.raw.start).unwrap_or(u32::MAX);
        let text = u32::try_from(token.text.end - token.text.start).unwrap_or(u32::MAX);
        self.insert_node(
            token.kind,
            lang,
            Bytes::from(raw),
            Stripped::InRaw { lead, text },
            Vec::new(),
            name,
        )
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
        if children.len() == 1 {
            let child = self
                .entries
                .get(&children[0])
                .ok_or(ParseError::MissingNode(children[0]))?;
            return self.insert_node(
                kind,
                lang,
                child.node.raw.clone(),
                child.stripped.clone(),
                children,
                name,
            );
        }
        let mut raw_len = 0usize;
        let mut stripped_len = 0usize;
        for id in &children {
            let child = self.entries.get(id).ok_or(ParseError::MissingNode(*id))?;
            raw_len += child.node.raw.len();
            stripped_len += child.stripped_slice().len();
        }
        let mut raw = Vec::with_capacity(raw_len);
        let mut stripped = Vec::with_capacity(stripped_len);
        for id in &children {
            let child = self.entries.get(id).ok_or(ParseError::MissingNode(*id))?;
            raw.extend_from_slice(child.node.raw.as_slice());
            stripped.extend_from_slice(child.stripped_slice());
        }
        self.insert_node(
            kind,
            lang,
            Bytes::new(raw),
            Stripped::Owned(Bytes::new(stripped)),
            children,
            name,
        )
    }

    /// A tree whose root is a single leaf: `raw == source`, stripped empty.
    ///
    /// Used for empty or trivia-only files (no token to attach trivia to).
    pub fn from_raw_root(kind: NodeKind, lang: LangId, source: &[u8]) -> Result<Self, ParseError> {
        let mut tree = Self::new();
        let id = tree.insert_node(
            kind,
            lang,
            Bytes::from(source),
            Stripped::InRaw { lead: 0, text: 0 },
            Vec::new(),
            None,
        )?;
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
            let kind = entry.node.kind;
            let raw = entry.node.raw.as_slice();
            check_concat_slices(
                kind,
                raw,
                entry.node.children.iter().map(|id| {
                    self.entries
                        .get(id)
                        .map(|child| child.node.raw.as_slice())
                        .ok_or(ParseError::MissingNode(*id))
                }),
            )?;
        }
        Ok(())
    }
}

/// `concat(children) == raw`. `children_raw_len` stays the sum of every child
/// when an earlier slice already disagrees with `raw`.
fn check_concat_slices<'a>(
    kind: NodeKind,
    raw: &[u8],
    children: impl Iterator<Item = Result<&'a [u8], ParseError>>,
) -> Result<(), ParseError> {
    let mut concat_len = 0usize;
    let mut off = 0usize;
    let mut bytes_match = true;
    for child in children {
        let slice = child?;
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
        assert_eq!(
            a.get(id_ws).unwrap().normalized,
            normalized_hash(b"foo").unwrap()
        );
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
    fn intern_source_token_copies_once_from_source() {
        use crate::trivia::{TokenSpan, attach_trivia_spans};
        let source = b"  foo\n";
        let spans = attach_trivia_spans(source, vec![TokenSpan::new("ident", 2, 5)]);
        let mut tree = NodeTree::new();
        let id = tree
            .intern_source_token(lang(), source, &spans[0], None)
            .unwrap();
        let node = tree.get(id).unwrap();
        assert_eq!(node.raw.as_slice(), source);
        assert_eq!(tree.stripped(id).unwrap(), b"foo");
        let stripped = tree.stripped(id).unwrap();
        assert!(
            std::ptr::eq(stripped.as_ptr(), node.raw[2..].as_ptr()),
            "stripped text must be a subslice of raw, not a second copy"
        );
    }
}
