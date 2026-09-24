//! [`DefinitionFacts`] for Rust definitions (ADR 0026): the node kinds
//! inside a definition and its visibility modifier.

use std::ops::Range;

use hord_lang::DefinitionFacts;

use crate::cst;

/// One [`DefinitionFacts`] per span. A span that holds no definition, or a
/// source that does not parse, reports empty facts.
///
/// The definition in a span is the outermost definition node inside it (a
/// span includes attached trivia and attributes, which are siblings of the
/// item in tree-sitter-rust). Inner kinds are the named nodes inside the
/// span other than that node itself, so attributes count and nested
/// definitions contribute their contents. Error nodes are left out.
pub(crate) fn definition_facts(
    source: &[u8],
    spans: &[Range<usize>],
    is_definition: impl Fn(&str) -> bool,
) -> Vec<DefinitionFacts> {
    let tree = cst::with_parser(|parser| parser.parse(source, None))
        .ok()
        .flatten();
    let Some(tree) = tree else {
        return vec![DefinitionFacts::default(); spans.len()];
    };
    spans
        .iter()
        .map(|span| facts_in(tree.root_node(), source, span, &is_definition))
        .collect()
}

fn facts_in(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    span: &Range<usize>,
    is_definition: &impl Fn(&str) -> bool,
) -> DefinitionFacts {
    let inside =
        |n: tree_sitter::Node<'_>| n.start_byte() >= span.start && n.end_byte() <= span.end;
    let Some(def) = outermost_definition(root, span, &inside, is_definition) else {
        return DefinitionFacts::default();
    };
    let mut facts = DefinitionFacts {
        visibility: visibility(def, source),
        ..DefinitionFacts::default()
    };
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= span.start || node.start_byte() >= span.end {
            continue;
        }
        if inside(node) && node.is_named() && node.id() != def.id() && !is_error(node) {
            facts.inner_kinds.insert(node.kind().to_owned());
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    facts
}

/// The first definition node in preorder that lies inside `span`.
fn outermost_definition<'t>(
    root: tree_sitter::Node<'t>,
    span: &Range<usize>,
    inside: &impl Fn(tree_sitter::Node<'t>) -> bool,
    is_definition: &impl Fn(&str) -> bool,
) -> Option<tree_sitter::Node<'t>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.end_byte() <= span.start || node.start_byte() >= span.end {
            continue;
        }
        if inside(node) && is_definition(node.kind()) {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    None
}

fn visibility(def: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = def.walk();
    def.named_children(&mut cursor)
        .find(|c| c.kind() == "visibility_modifier")
        .and_then(|c| c.utf8_text(source).ok())
        .map(str::to_owned)
}

fn is_error(node: tree_sitter::Node<'_>) -> bool {
    node.is_error() || node.is_missing()
}

#[cfg(test)]
mod tests {
    use hord_core::NodeKind;
    use hord_lang::LangAdapter;

    use crate::RustAdapter;

    use super::*;

    fn span_of(source: &str, needle: &str) -> Range<usize> {
        let start = source.find(needle).expect("needle in source");
        start..start + needle.len()
    }

    fn facts(source: &str, needles: &[&str]) -> Vec<DefinitionFacts> {
        let spans: Vec<_> = needles.iter().map(|n| span_of(source, n)).collect();
        RustAdapter.definition_facts(source.as_bytes(), &spans)
    }

    #[test]
    fn unsafe_block_is_an_inner_kind() {
        let src =
            "fn safe() -> u32 {\n    1\n}\n\nfn raw(p: *const u8) -> u8 {\n    unsafe { *p }\n}\n";
        let got = facts(
            src,
            &[
                "fn safe() -> u32 {\n    1\n}",
                "fn raw(p: *const u8) -> u8 {\n    unsafe { *p }\n}",
            ],
        );
        assert!(!got[0].inner_kinds.contains("unsafe_block"));
        assert!(got[0].inner_kinds.contains("block"));
        assert!(!got[0].inner_kinds.contains("function_item"), "own kind");
        assert!(got[1].inner_kinds.contains("unsafe_block"));
        assert_eq!(got[0].visibility, None);
    }

    #[test]
    fn visibility_as_written() {
        let src = "pub fn a() {}\npub(crate) struct B {\n    pub x: u32,\n    y: u32,\n}\nenum C { V }\npub(in crate::m) const D: u8 = 0;\n";
        let got = facts(
            src,
            &[
                "pub fn a() {}",
                "pub(crate) struct B {\n    pub x: u32,\n    y: u32,\n}",
                "pub x: u32",
                "y: u32",
                "enum C { V }",
                "V",
                "pub(in crate::m) const D: u8 = 0;",
            ],
        );
        let vis: Vec<_> = got.iter().map(|f| f.visibility.as_deref()).collect();
        assert_eq!(
            vis,
            [
                Some("pub"),
                Some("pub(crate)"),
                Some("pub"),
                None,
                None,
                None,
                Some("pub(in crate::m)"),
            ]
        );
        assert!(got[1].inner_kinds.contains("field_declaration"));
    }

    #[test]
    fn method_in_impl_and_attached_trivia() {
        let src = "impl S {\n    /// Docs.\n    #[inline]\n    pub unsafe fn m(&self) {\n        unsafe { g() }\n    }\n\n    fn n(&self) {}\n}\n";
        let method =
            "/// Docs.\n    #[inline]\n    pub unsafe fn m(&self) {\n        unsafe { g() }\n    }";
        let got = facts(src, &[method, "fn n(&self) {}", src.trim_end()]);
        assert_eq!(got[0].visibility.as_deref(), Some("pub"));
        assert!(got[0].inner_kinds.contains("unsafe_block"));
        assert!(got[0].inner_kinds.contains("attribute_item"));
        assert!(got[0].inner_kinds.contains("line_comment"));
        assert!(!got[1].inner_kinds.contains("unsafe_block"));
        // The impl contains both methods.
        assert!(got[2].inner_kinds.contains("unsafe_block"));
        assert!(got[2].inner_kinds.contains("function_item"));
        assert_eq!(got[2].visibility, None);
    }

    #[test]
    fn no_definition_or_no_parse_is_empty() {
        let src = "// just a comment\nfn f() {}\n";
        let got = facts(src, &["// just a comment"]);
        assert_eq!(got, [DefinitionFacts::default()]);
        let got = RustAdapter.definition_facts(b"fn f() {}", &[]);
        assert!(got.is_empty());
        let got = RustAdapter.definition_facts(
            b"fn f() {}",
            &[Range {
                start: 100,
                end: 200,
            }],
        );
        assert_eq!(got, [DefinitionFacts::default()]);
    }

    #[test]
    fn is_definition_matches_the_adapter() {
        let got = definition_facts(b"fn f() {}", &[Range { start: 0, end: 9 }], |k| {
            RustAdapter.is_definition(&NodeKind::new(k))
        });
        assert!(got[0].inner_kinds.contains("block"));
    }
}
