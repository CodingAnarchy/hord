//! Lossless CST from a tree-sitter-rust tree (spec §3.3, §4.2).
//!
//! One walk collects token spans; trivia is attached as ranges into `source`;
//! a second walk interns. Extra/missing nodes are skipped so comments stay
//! trivia. Attachment and interning are [`hord_lang::attach_trivia_spans`]
//! and [`hord_lang::NodeTree`].

use std::cell::RefCell;

use hord_core::{LangId, NodeKind, ObjectId};
use hord_lang::{AttachedSpan, NodeTree, ParseError, TokenSpan, attach_trivia_spans};

fn is_structural(node: tree_sitter::Node<'_>) -> bool {
    !node.is_extra() && !node.is_missing()
}

fn is_true_token(node: tree_sitter::Node<'_>) -> bool {
    is_structural(node) && node.child_count() == 0 && node.start_byte() < node.end_byte()
}

fn collect_tokens(node: tree_sitter::Node<'_>, out: &mut Vec<TokenSpan>) {
    if !is_structural(node) {
        return;
    }
    if node.child_count() == 0 {
        if node.start_byte() < node.end_byte() {
            out.push(TokenSpan::new(
                node.kind(),
                node.start_byte(),
                node.end_byte(),
            ));
        }
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_tokens(cursor.node(), out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn intern(
    node: tree_sitter::Node<'_>,
    lang: &LangId,
    source: &[u8],
    tokens: &[AttachedSpan],
    token_i: &mut usize,
    tree: &mut NodeTree,
) -> Result<Option<ObjectId>, ParseError> {
    if !is_structural(node) {
        return Ok(None);
    }
    if is_true_token(node) {
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

    let mut children = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if is_structural(child)
                && let Some(id) = intern(child, lang, source, tokens, token_i, tree)?
            {
                children.push(id);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(tree.intern_branch(
        NodeKind::new(node.kind()),
        *lang,
        children,
        None,
    )?))
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
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .map_err(|e| {
                    ParseError::failed(format!("failed to load tree-sitter-rust grammar: {e}"))
                })?;
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

/// Parse `source` with tree-sitter-rust and intern a lossless CST.
pub(crate) fn parse(source: &[u8], lang: &LangId) -> Result<NodeTree, ParseError> {
    let ts_tree = with_parser(|parser| parser.parse(source, None))?
        .ok_or_else(|| ParseError::failed("tree-sitter-rust produced no tree"))?;
    let root = ts_tree.root_node();

    let mut tokens = Vec::with_capacity(root.descendant_count());
    collect_tokens(root, &mut tokens);
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
            "interned {token_i} of {} tokens",
            attached.len()
        )));
    }
    tree.set_root(root_id)?;
    ensure_lossless(source, &tree)?;
    Ok(tree)
}
