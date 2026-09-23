//! Lossless CST from a tree-sitter-toml tree (spec §3.3, §4.4).
//!
//! Trivia attachment uses [`hord_lang::attach_trivia_spans`] (DECIDED §3.3):
//! leading trivia attaches to the following token; trailing trivia on the
//! same line attaches to the preceding token.
//!
//! tree-sitter-toml does not emit string/quoted-key contents as nodes (they
//! live in byte gaps between quote tokens). Those gaps are interned as
//! [`CONTENT_KIND`] leaves so they participate in `normalized`. Whitespace and
//! comments in other parents stay trivia.

use std::cell::RefCell;
use std::collections::BTreeMap;

use hord_core::{LangId, NodeKind, ObjectId, QualifiedName};
use hord_lang::{AttachedSpan, NodeTree, ParseError, TokenSpan, attach_trivia_spans};

/// Kind for bytes that tree-sitter-toml omits from the tree (string contents).
///
/// Not a definition.
pub(crate) const CONTENT_KIND: &str = "_content";

fn is_structural(node: tree_sitter::Node<'_>) -> bool {
    !node.is_extra() && !node.is_missing()
}

fn should_emit(node: tree_sitter::Node<'_>) -> bool {
    is_structural(node) && (node.start_byte() < node.end_byte() || node.child_count() > 0)
}

/// Visit structural children that [`should_emit`].
///
/// `visit` returns `Ok(true)` to stop, `Ok(false)` to keep going.
fn walk_emitted(
    node: tree_sitter::Node<'_>,
    mut visit: impl FnMut(tree_sitter::Node<'_>) -> Result<bool, ParseError>,
) -> Result<(), ParseError> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return Ok(());
    }
    loop {
        let child = cursor.node();
        if should_emit(child) && visit(child)? {
            return Ok(());
        }
        if !cursor.goto_next_sibling() {
            return Ok(());
        }
    }
}

fn has_emitted_child(node: tree_sitter::Node<'_>) -> bool {
    let mut found = false;
    let _ = walk_emitted(node, |_| {
        found = true;
        Ok(true)
    });
    found
}

fn is_content_parent(kind: &str) -> bool {
    matches!(kind, "string" | "quoted_key")
}

/// Comments (`#` to end of line) and ASCII whitespace only.
fn is_trivia_bytes(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'#' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            _ => return false,
        }
    }
    true
}

fn collect_tokens(node: tree_sitter::Node<'_>, source: &[u8], out: &mut Vec<TokenSpan>) {
    if !should_emit(node) {
        return;
    }
    if !has_emitted_child(node) {
        if node.child_count() == 0 {
            out.push(TokenSpan::new(
                node.kind(),
                node.start_byte(),
                node.end_byte(),
            ));
        }
        return;
    }

    let mut cursor_byte = node.start_byte();
    let _ = walk_emitted(node, |child| {
        if child.start_byte() > cursor_byte {
            maybe_content_gap(node.kind(), source, cursor_byte, child.start_byte(), out);
        }
        collect_tokens(child, source, out);
        cursor_byte = child.end_byte();
        Ok(false)
    });
    if node.end_byte() > cursor_byte {
        maybe_content_gap(node.kind(), source, cursor_byte, node.end_byte(), out);
    }
}

fn maybe_content_gap(
    parent_kind: &str,
    source: &[u8],
    start: usize,
    end: usize,
    out: &mut Vec<TokenSpan>,
) {
    if start >= end {
        return;
    }
    let gap = &source[start..end];
    if is_content_parent(parent_kind) || !is_trivia_bytes(gap) {
        out.push(TokenSpan::new(CONTENT_KIND, start, end));
    }
}

/// Per-parse naming state (spec §3.4 durable names for TOML definitions).
///
/// - `[a.b]` tables are named by their header.
/// - `[[a.b]]` elements are named `a.b::<v1> <v2> …` from the values of their
///   leading scalar pairs, taking one more value until the name is unique
///   among the file's `[[a.b]]` elements. An element with no leading scalars
///   is named by its header alone; one still ambiguous after all of them
///   keeps its longest name.
/// - Pairs are `<enclosing table name>::<key>`.
struct Namer {
    /// `table_array_element` start byte → its name.
    elements: BTreeMap<usize, String>,
}

impl Namer {
    fn new(root: tree_sitter::Node<'_>, source: &[u8]) -> Self {
        let mut by_header: BTreeMap<String, Vec<(usize, Vec<String>)>> = BTreeMap::new();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() != "table_array_element" {
                continue;
            }
            if let Some(header) = toml_local_name(child, source) {
                let scalars = leading_scalars(child, source);
                by_header
                    .entry(header)
                    .or_default()
                    .push((child.start_byte(), scalars));
            }
        }
        let mut elements = BTreeMap::new();
        for (header, items) in by_header {
            for (start, scalars) in &items {
                let mut n = scalars.len().min(1);
                while n < scalars.len()
                    && items
                        .iter()
                        .filter(|(_, other)| other.len() >= n && other[..n] == scalars[..n])
                        .count()
                        > 1
                {
                    n += 1;
                }
                let name = if n == 0 {
                    header.clone()
                } else {
                    format!("{header}::{}", scalars[..n].join(" "))
                };
                elements.insert(*start, name);
            }
        }
        Self { elements }
    }

    fn def_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<QualifiedName> {
        let local = self.local_name(node, source)?;
        if node.kind() != "pair" {
            return Some(QualifiedName::new(local));
        }
        let mut parent = node.parent();
        while let Some(p) = parent {
            if matches!(p.kind(), "table" | "table_array_element")
                && let Some(table) = self.local_name(p, source)
            {
                return Some(QualifiedName::new(format!("{table}::{local}")));
            }
            parent = p.parent();
        }
        Some(QualifiedName::new(local))
    }

    fn local_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        if node.kind() == "table_array_element"
            && let Some(name) = self.elements.get(&node.start_byte())
        {
            return Some(name.clone());
        }
        toml_local_name(node, source)
    }
}

/// Values of an element's leading `key = scalar` pairs, in order, up to the
/// first pair whose value is an array or inline table.
fn leading_scalars(element: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = element.walk();
    for pair in element.children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let Some(value) = pair.child(pair.child_count().saturating_sub(1)) else {
            break;
        };
        if matches!(value.kind(), "array" | "inline_table") {
            break;
        }
        let Ok(text) = value.utf8_text(source) else {
            break;
        };
        out.push(scalar_label(value.kind(), text));
    }
    out
}

/// A scalar's text for a name: single-line strings without their quotes,
/// everything else as written.
fn scalar_label(kind: &str, text: &str) -> String {
    let unquoted = (kind == "string")
        .then(|| {
            text.strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .or_else(|| text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        })
        .flatten()
        .filter(|t| !t.starts_with(['"', '\'']));
    unquoted.unwrap_or(text).to_owned()
}

fn intern_spans_before(
    before: usize,
    lang: &LangId,
    source: &[u8],
    tokens: &[AttachedSpan],
    token_i: &mut usize,
    tree: &mut NodeTree,
    children: &mut Vec<ObjectId>,
) -> Result<(), ParseError> {
    while *token_i < tokens.len() && tokens[*token_i].text.start < before {
        children.push(tree.intern_source_token(*lang, source, &tokens[*token_i], None)?);
        *token_i += 1;
    }
    Ok(())
}

fn intern(
    node: tree_sitter::Node<'_>,
    namer: &Namer,
    lang: &LangId,
    source: &[u8],
    tokens: &[AttachedSpan],
    token_i: &mut usize,
    tree: &mut NodeTree,
) -> Result<Option<ObjectId>, ParseError> {
    if !should_emit(node) {
        return Ok(None);
    }
    if !has_emitted_child(node) {
        if node.child_count() == 0 {
            let token = tokens.get(*token_i).ok_or_else(|| {
                ParseError::failed(format!(
                    "token underflow at {} byte {}",
                    node.kind(),
                    node.start_byte()
                ))
            })?;
            *token_i += 1;
            return Ok(Some(tree.intern_source_token(*lang, source, token, None)?));
        }
        return Ok(None);
    }

    let mut children = Vec::new();
    walk_emitted(node, |child| {
        intern_spans_before(
            child.start_byte(),
            lang,
            source,
            tokens,
            token_i,
            tree,
            &mut children,
        )?;
        if let Some(id) = intern(child, namer, lang, source, tokens, token_i, tree)? {
            children.push(id);
        }
        Ok(false)
    })?;
    intern_spans_before(
        node.end_byte(),
        lang,
        source,
        tokens,
        token_i,
        tree,
        &mut children,
    )?;
    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(tree.intern_branch(
        NodeKind::new(node.kind()),
        *lang,
        children,
        namer.def_name(node, source),
    )?))
}

fn toml_local_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let name = match node.kind() {
        // The header key (`[a.b]` → `a.b`), not the node text: a table's
        // text runs through its last pair.
        "table" | "table_array_element" => node
            .named_child(0)
            .filter(|k| matches!(k.kind(), "bare_key" | "dotted_key" | "quoted_key"))?
            .utf8_text(source)
            .ok()?
            .trim()
            .to_owned(),
        "pair" => node.child(0)?.utf8_text(source).ok()?.trim().to_owned(),
        _ => return None,
    };
    if name.is_empty() { None } else { Some(name) }
}

fn with_parser<T>(f: impl FnOnce(&mut tree_sitter::Parser) -> T) -> Result<T, ParseError> {
    thread_local! {
        static PARSER: RefCell<Option<tree_sitter::Parser>> = const { RefCell::new(None) };
    }
    PARSER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_toml::LANGUAGE.into())
                .map_err(|e| ParseError::failed(format!("failed to load tree-sitter-toml: {e}")))?;
            *slot = Some(parser);
        }
        Ok(f(slot.as_mut().expect("parser initialized")))
    })
}

fn ensure_lossless(source: &[u8], tree: &NodeTree) -> Result<(), ParseError> {
    match tree.root_bytes() {
        Some(raw) if raw == source => Ok(()),
        Some(raw) => Err(ParseError::failed(format!(
            "parse is not lossless: projected {} bytes from {}-byte source",
            raw.len(),
            source.len()
        ))),
        None => Err(ParseError::failed("parse produced an empty tree")),
    }
}

/// Parse `source` with tree-sitter-toml and intern a lossless [`NodeTree`].
pub(crate) fn parse(source: &[u8], lang: &LangId) -> Result<NodeTree, ParseError> {
    let ts_tree = with_parser(|parser| parser.parse(source, None))?
        .ok_or_else(|| ParseError::failed("tree-sitter-toml produced no tree"))?;
    let root = ts_tree.root_node();

    let mut tokens = Vec::with_capacity(root.descendant_count());
    collect_tokens(root, source, &mut tokens);

    if tokens.is_empty() {
        let tree = NodeTree::from_raw_root(NodeKind::new(root.kind()), *lang, source)?;
        ensure_lossless(source, &tree)?;
        return Ok(tree);
    }

    let attached = attach_trivia_spans(source, tokens);
    let mut tree = NodeTree::new();
    let mut token_i = 0;
    let namer = Namer::new(root, source);
    let root_id = intern(
        root,
        &namer,
        lang,
        source,
        &attached,
        &mut token_i,
        &mut tree,
    )?
    .ok_or_else(|| ParseError::failed("intern produced no root"))?;
    if token_i != attached.len() {
        return Err(ParseError::failed(format!(
            "interned {token_i} of {} leaves",
            attached.len()
        )));
    }
    tree.set_root(root_id)?;
    ensure_lossless(source, &tree)?;
    Ok(tree)
}
