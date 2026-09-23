//! Independent tree-sitter reads of definition text: where a body opens, and
//! which names the text contains. The oracle uses these instead of anything
//! `hord-txn` computes.

use std::collections::BTreeSet;

use tree_sitter::{Node, Parser, Tree};

fn parse(text: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    parser.parse(text, None)
}

/// Whether `text` parses as Rust with no error nodes.
pub(crate) fn parses_clean(text: &[u8]) -> bool {
    parse(text).is_some_and(|tree| !tree.root_node().has_error())
}

/// A function definition's text, read on its own.
pub(crate) struct FnShape {
    /// The function's own name.
    pub name: String,
    /// Byte offset just past the body's `{`.
    pub body_open: usize,
    /// Every identifier-like leaf in the text, macro token trees included.
    pub idents: BTreeSet<String>,
}

/// Shape of the first `function_item` in `text`, if the text parses clean
/// and that function has a body.
pub(crate) fn fn_shape(text: &[u8]) -> Option<FnShape> {
    let tree = parse(text)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let func = find_kind(root, "function_item")?;
    let body = func.child_by_field_name("body")?;
    let name = func
        .child_by_field_name("name")?
        .utf8_text(text)
        .ok()?
        .to_string();
    let mut idents = BTreeSet::new();
    collect_idents(root, text, &mut idents);
    Some(FnShape {
        name,
        body_open: body.start_byte() + 1,
        idents,
    })
}

/// Identifier-like leaves of `text`.
pub(crate) fn idents(text: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(tree) = parse(text) {
        collect_idents(tree.root_node(), text, &mut out);
    }
    out
}

/// `text` with `stmt` as the first statement of its function body. `None`
/// if the text is not a function with a body or the result does not parse.
pub(crate) fn insert_stmt(text: &[u8], stmt: &str) -> Option<Vec<u8>> {
    let shape = fn_shape(text)?;
    let mut out = Vec::with_capacity(text.len() + stmt.len() + 8);
    out.extend_from_slice(&text[..shape.body_open]);
    out.extend_from_slice(b"\n    ");
    out.extend_from_slice(stmt.as_bytes());
    out.extend_from_slice(&text[shape.body_open..]);
    fn_shape(&out).map(|_| out)
}

fn find_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'t>> = node.children(&mut cursor).collect();
    children
        .into_iter()
        .find_map(|child| find_kind(child, kind))
}

fn collect_idents(node: Node<'_>, text: &[u8], out: &mut BTreeSet<String>) {
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier" | "shorthand_field_identifier"
    ) {
        if let Ok(s) = node.utf8_text(text) {
            out.insert(s.to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_idents(child, text, out);
    }
}
