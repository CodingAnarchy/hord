//! Rust facts about a definition's text, for [`ChangeFacts::diff_file`]:
//! its outer attributes, whether it is a test, and whether it is a trait
//! impl (reachable by dynamic dispatch).

use hord_core::Node;
use hord_lang::{LangAdapter, NodeTree};
use hord_lang_rust::RustAdapter;
use hord_verify::{ChangeFacts, DefTraits, Definition, FileVersion};

/// Classify `text`, the full source of `def` (its span in the file).
///
/// A test is a function with an outer attribute whose path ends in `test`
/// (`#[test]`, `#[tokio::test]`, `#[cargo_test]`, …). Over-matching is
/// safe: a test selects itself. A trait impl is an `impl … for …` item.
#[must_use]
pub fn rust_traits(def: &Definition, text: &[u8]) -> DefTraits {
    let Ok(tree) = RustAdapter.parse(text) else {
        // Unparseable text: say it is everything, so selection falls back.
        return DefTraits {
            attributes: vec![String::from_utf8_lossy(text).into_owned()],
            test: def.kind.as_str() == "function_item",
            dispatch: def.kind.as_str() == "impl_item",
        };
    };
    let Some(item) = first_item(&tree, def.kind.as_str()) else {
        return DefTraits::default();
    };
    let mut attributes = Vec::new();
    let mut dispatch = false;
    for child in item.children.iter().filter_map(|c| tree.get(*c)) {
        match child.kind.as_str() {
            "attribute_item" => attributes.push(
                String::from_utf8_lossy(child.raw.as_slice())
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            "for" => dispatch = def.kind.as_str() == "impl_item",
            _ => {}
        }
    }
    let test = def.kind.as_str() == "function_item" && attributes.iter().any(|a| is_test_attr(a));
    DefTraits {
        attributes,
        test,
        dispatch,
    }
}

/// `#[test]`, `#[tokio::test]`, `#[cargo_test(..)]`: the attribute path's
/// last segment ends in `test`.
fn is_test_attr(attr: &str) -> bool {
    let body = attr
        .trim_start_matches('#')
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let path = body
        .split(|c: char| c == '(' || c == '=' || c.is_whitespace())
        .next()
        .unwrap_or_default();
    path.rsplit("::")
        .next()
        .is_some_and(|last| last.ends_with("test"))
}

fn first_item<'t>(tree: &'t NodeTree, kind: &str) -> Option<&'t Node> {
    let mut stack = vec![tree.root()?];
    while let Some(id) = stack.pop() {
        let node = tree.get(id)?;
        if node.kind.as_str() == kind {
            return Some(node);
        }
        stack.extend(node.children.iter().rev());
    }
    None
}

/// [`ChangeFacts::diff_file`] with [`rust_traits`].
pub fn diff_rust_file(
    facts: &mut ChangeFacts,
    path: &hord_core::RepoPath,
    older: Option<FileVersion<'_>>,
    newer: Option<FileVersion<'_>>,
) {
    facts.diff_file(path, older, newer, &rust_traits);
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::{NodeId, NodeKind, RepoPath};

    use super::*;

    fn def(kind: &str, text: &str) -> DefTraits {
        let d = Definition {
            node: NodeId::from_u128(1),
            path: RepoPath::from_str("src/lib.rs").unwrap(),
            kind: NodeKind::new(kind),
            name: None,
            span: 0..text.len(),
            parent: None,
        };
        rust_traits(&d, text.as_bytes())
    }

    #[test]
    fn tests_attributes_and_trait_impls() {
        let t = def("function_item", "#[test]\nfn f() {}");
        assert!(t.test && t.attributes == vec!["#[test]"]);
        assert!(
            def(
                "function_item",
                "#[tokio::test(flavor = \"x\")]\nasync fn f() {}"
            )
            .test
        );
        assert!(def("function_item", "#[cargo_test]\nfn f() {}").test);
        assert!(!def("function_item", "#[inline]\nfn f() {}").test);
        assert!(!def("function_item", "#[cfg(test)]\nfn f() {}").test);
        assert!(!def("function_item", "fn test() {}").test);
        assert!(def("impl_item", "impl Drop for X { fn drop(&mut self) {} }").dispatch);
        assert!(!def("impl_item", "impl X { fn new() {} }").dispatch);
        let s = def("struct_item", "#[derive(Debug,\n Clone)]\nstruct S;");
        assert_eq!(s.attributes, vec!["#[derive(Debug, Clone)]"]);
    }
}
