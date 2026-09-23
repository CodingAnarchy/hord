//! Lossless CST from a tree-sitter-rust tree (spec §3.3, §4.2).
//!
//! One walk collects token spans; trivia is attached as ranges into `source`;
//! a second walk interns. Extra/missing nodes are skipped so comments stay
//! trivia. Attachment and interning are [`hord_lang::attach_trivia_spans`]
//! and [`hord_lang::NodeTree`].

use std::cell::RefCell;

use hord_core::{LangId, NodeKind, ObjectId, QualifiedName};
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
    leading: Vec<ObjectId>,
) -> Result<Option<ObjectId>, ParseError> {
    if !is_structural(node) {
        return Ok(None);
    }
    if is_true_token(node) {
        let token = tokens.get(*token_i).ok_or_else(|| {
            ParseError::failed(format!(
                "intern ran out of tokens at {} (source byte {}, token index {token_i})",
                node.kind(),
                node.start_byte()
            ))
        })?;
        *token_i += 1;
        return Ok(Some(tree.intern_source_token(*lang, source, token, None)?));
    }

    let mut children = leading;
    let mut pending_attrs = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if is_structural(child) {
                if child.kind() == "attribute_item" {
                    if let Some(id) =
                        intern(child, lang, source, tokens, token_i, tree, Vec::new())?
                    {
                        pending_attrs.push(id);
                    }
                } else if !pending_attrs.is_empty() && takes_outer_attributes(child.kind()) {
                    let leading_attrs = std::mem::take(&mut pending_attrs);
                    if let Some(id) =
                        intern(child, lang, source, tokens, token_i, tree, leading_attrs)?
                    {
                        children.push(id);
                    }
                } else {
                    children.append(&mut pending_attrs);
                    if let Some(id) =
                        intern(child, lang, source, tokens, token_i, tree, Vec::new())?
                    {
                        children.push(id);
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    children.append(&mut pending_attrs);
    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(tree.intern_branch(
        NodeKind::new(node.kind()),
        *lang,
        children,
        def_name(node, source),
    )?))
}

/// An outer attribute immediately before one of these belongs to it (ADR 0011).
fn takes_outer_attributes(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
            | "type_item"
            | "associated_type"
            | "union_item"
            | "enum_variant"
            | "field_declaration"
            | "use_declaration"
            | "extern_crate_declaration"
    )
}

/// Kinds whose local name is a prefix of nested definitions (spec §3.4).
pub(crate) fn is_name_container(kind: &str) -> bool {
    matches!(
        kind,
        "mod_item"
            | "impl_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "union_item"
            | "enum_variant"
            | "foreign_mod_item"
    )
}

pub(crate) fn def_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<QualifiedName> {
    let local = local_def_name(node, source)?;
    let mut parts = Vec::new();
    let mut parent = node.parent();
    while let Some(p) = parent {
        if is_name_container(p.kind())
            && let Some(m) = local_def_name(p, source)
        {
            parts.push(m);
        }
        parent = p.parent();
    }
    parts.reverse();
    parts.push(local);
    Some(QualifiedName::new(parts.join("::")))
}

pub(crate) fn local_def_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "function_item"
        | "function_signature_item"
        | "struct_item"
        | "enum_item"
        | "trait_item"
        | "mod_item"
        | "const_item"
        | "static_item"
        | "macro_definition"
        | "type_item"
        | "associated_type"
        | "union_item"
        | "enum_variant"
        | "field_declaration" => named_field(node, source),
        "impl_item" => {
            let ty = node
                .child_by_field_name("type")?
                .utf8_text(source)
                .ok()?
                .trim();
            Some(match node.child_by_field_name("trait") {
                Some(tr) => format!("{} for {}", tr.utf8_text(source).ok()?.trim(), ty),
                None => format!("impl {ty}"),
            })
        }
        "use_declaration" => use_clause_name(node, source),
        "extern_crate_declaration" => {
            named_field(node, source).or_else(|| first_identifier(node, source))
        }
        "inner_attribute_item" => {
            let text = node.utf8_text(source).ok()?.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_owned())
            }
        }
        "foreign_mod_item" => {
            let abi = first_child_kind(node, "string_literal")
                .and_then(|n| n.utf8_text(source).ok().map(|s| s.trim().to_owned()))
                .unwrap_or_else(|| "\"C\"".to_owned());
            Some(format!("extern {abi}"))
        }
        _ => None,
    }
}

fn named_field(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let text = name.utf8_text(source).ok()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn first_identifier(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    first_child_kind(node, "identifier").and_then(|n| {
        let text = n.utf8_text(source).ok()?.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_owned())
        }
    })
}

fn first_child_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if cursor.node().kind() == kind {
            return Some(cursor.node());
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
}

fn use_clause_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut bits = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        match child.kind() {
            "visibility_modifier" | "use" | ";" => {}
            _ => {
                if let Ok(text) = child.utf8_text(source) {
                    let text = text.trim();
                    if !text.is_empty() {
                        bits.push(text.to_owned());
                    }
                }
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    let name = bits.join(" ");
    if name.is_empty() { None } else { Some(name) }
}

pub(crate) fn with_parser<T>(
    f: impl FnOnce(&mut tree_sitter::Parser) -> T,
) -> Result<T, ParseError> {
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
                    ParseError::failed(format!("tree-sitter-rust grammar failed to load: {e}"))
                })?;
            *slot = Some(parser);
        }
        Ok(f(slot.as_mut().expect("parser initialized")))
    })
}

fn ensure_lossless(source: &[u8], tree: &NodeTree) -> Result<(), ParseError> {
    match tree.root_bytes() {
        Some(raw) if raw == source => Ok(()),
        Some(raw) => {
            let mismatch = raw
                .iter()
                .zip(source.iter())
                .position(|(a, b)| a != b)
                .or_else(|| (raw.len() != source.len()).then_some(raw.len().min(source.len())));
            let at = mismatch.map_or(String::new(), |i| format!(", first mismatch at byte {i}"));
            Err(ParseError::failed(format!(
                "parse is not lossless: projected {} bytes from a {}-byte source{at}",
                raw.len(),
                source.len()
            )))
        }
        None => Err(ParseError::failed(
            "parse is not lossless: tree has no root to project",
        )),
    }
}

/// Local name of one definition, parsed from its own source bytes.
///
/// `raw` is the node's exact subtree (trivia included). Field and enum-variant
/// fragments are wrapped so tree-sitter can see them as items.
pub(crate) fn local_name_from_raw(kind: &str, raw: &[u8]) -> Option<String> {
    let wrapped;
    let source: &[u8] = match kind {
        "field_declaration" => {
            let text = std::str::from_utf8(raw).ok()?;
            wrapped = format!("struct __HordWrap {{\n{text}\n}}");
            wrapped.as_bytes()
        }
        "enum_variant" => {
            let text = std::str::from_utf8(raw).ok()?;
            wrapped = format!("enum __HordWrap {{\n{text},\n}}");
            wrapped.as_bytes()
        }
        _ => raw,
    };
    with_parser(|parser| {
        let tree = parser.parse(source, None)?;
        let found = find_kind(tree.root_node(), kind)?;
        local_def_name(found, source)
    })
    .ok()
    .flatten()
}

fn find_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if let Some(found) = find_kind(cursor.node(), kind) {
            return Some(found);
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
}

/// Parse `source` with tree-sitter-rust and intern a lossless CST.
pub(crate) fn parse(source: &[u8], lang: &LangId) -> Result<NodeTree, ParseError> {
    let ts_tree = with_parser(|parser| parser.parse(source, None))?
        .ok_or_else(|| ParseError::failed("tree-sitter-rust returned no syntax tree"))?;
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
    let root_id = intern(
        root,
        lang,
        source,
        &attached,
        &mut token_i,
        &mut tree,
        Vec::new(),
    )?
    .ok_or_else(|| ParseError::failed("intern did not produce a root node"))?;
    if token_i != attached.len() {
        return Err(ParseError::failed(format!(
            "intern consumed {token_i} tokens but trivia attachment produced {}",
            attached.len()
        )));
    }
    tree.set_root(root_id)?;
    ensure_lossless(source, &tree)?;
    Ok(tree)
}
