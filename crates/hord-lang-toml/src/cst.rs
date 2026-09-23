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
        if let Some(id) = intern(child, lang, source, tokens, token_i, tree)? {
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
        toml_def_name(node, source),
    )?))
}

fn toml_def_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<QualifiedName> {
    let local = toml_local_name(node, source)?;
    if node.kind() != "pair" {
        return Some(QualifiedName::new(local));
    }
    let mut parent = node.parent();
    while let Some(p) = parent {
        if matches!(p.kind(), "table" | "table_array_element")
            && let Some(table) = toml_local_name(p, source)
        {
            return Some(QualifiedName::new(format!("{table}::{local}")));
        }
        parent = p.parent();
    }
    Some(QualifiedName::new(local))
}

fn toml_local_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let name = match node.kind() {
        "table" | "table_array_element" => {
            let text = node.utf8_text(source).ok()?.trim();
            let inner = text.trim_start_matches('[').trim_end_matches(']').trim();
            inner.lines().next()?.trim().to_owned()
        }
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
    let root_id = intern(root, lang, source, &attached, &mut token_i, &mut tree)?
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
