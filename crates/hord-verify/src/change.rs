//! What a change touched, as test selection's fallback rules read it
//! (ADR 0022).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use hord_core::{NodeId, NodeKind, QualifiedName, RepoPath};
use serde::{Deserialize, Serialize};

/// One definition of a parsed file in a snapshot, with its identity.
///
/// The same facts `hord-txn` reports for a definition; callers convert.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    /// Stable identity (spec §3.4).
    pub node: NodeId,
    /// File that contains it.
    pub path: RepoPath,
    /// Adapter kind, e.g. `function_item`.
    pub kind: NodeKind,
    /// Qualified name, when the adapter names this kind.
    pub name: Option<QualifiedName>,
    /// Byte range in the file, attached trivia included.
    pub span: Range<usize>,
    /// Nearest enclosing definition.
    pub parent: Option<NodeId>,
}

/// 1-based inclusive line range of the byte range `span` of `source`.
#[must_use]
pub fn line_range(source: &[u8], span: &Range<usize>) -> Range<u32> {
    let line_of = |at: usize| -> u32 {
        let at = at.min(source.len());
        let newlines = source[..at].iter().filter(|b| **b == b'\n').count();
        u32::try_from(newlines + 1).unwrap_or(u32::MAX)
    };
    let end = span.end.max(span.start + 1) - 1;
    line_of(span.start)..line_of(end) + 1
}

/// How a touched definition changed between the two snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum DefDelta {
    /// Only in the newer snapshot.
    Born,
    /// Only in the older snapshot.
    Died,
    /// In both, with different own text.
    Edited,
}

/// One definition (or a file's glue) that a change touched.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TouchedDef {
    /// The definition, or [`NodeId::file_root`] for text outside every
    /// definition (file-level glue).
    pub node: NodeId,
    /// File it is in (the newer snapshot's, unless it died).
    pub path: RepoPath,
    /// Adapter kind; `source_file` for glue.
    pub kind: NodeKind,
    /// Qualified name, if any.
    pub name: Option<QualifiedName>,
    /// Born, died, or edited.
    pub delta: DefDelta,
    /// Its outer attributes differ (always true for births and deaths of a
    /// definition that has any).
    pub attributes_changed: bool,
    /// The adapter recognizes it as a test.
    pub test: bool,
    /// Reachable through dynamic dispatch without a call site that names
    /// it (for Rust, a trait impl).
    pub dispatch: bool,
}

impl TouchedDef {
    /// Whether this is file-level glue rather than a definition.
    #[must_use]
    pub fn is_glue(&self) -> bool {
        self.node == NodeId::file_root(&self.path)
    }
}

/// Adapter facts about one definition, from its text: what
/// [`ChangeFacts::diff_file`] cannot know generically.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DefTraits {
    /// Its outer attributes, as text, in order.
    pub attributes: Vec<String>,
    /// It is a test.
    pub test: bool,
    /// It is reachable through dynamic dispatch (a trait impl).
    pub dispatch: bool,
}

/// One version of a file: its bytes and definitions.
#[derive(Clone, Copy, Debug)]
pub struct FileVersion<'a> {
    /// File contents.
    pub bytes: &'a [u8],
    /// Its definitions (empty for a file no adapter parses).
    pub defs: &'a [Definition],
}

/// Everything a change touched between two snapshots, for selection.
///
/// For test selection the older snapshot is the one the coverage record
/// was made on, which may be older than the change's base: a change's
/// facts then include the drift since coverage ran (ADR 0022 refresh).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeFacts {
    /// Every path added, deleted, or modified, parsed or not.
    pub paths: BTreeSet<RepoPath>,
    /// Touched definitions and glue, in path then node order.
    pub touched: Vec<TouchedDef>,
}

impl ChangeFacts {
    /// Record that `path` changed, and which of its definitions did.
    ///
    /// A definition is touched when it is born, dies, or its *own* text
    /// changes: its span with every child definition's span replaced by
    /// that child's id, so a method edit touches the method and not the
    /// `impl` around it. Text outside every definition is the file's glue.
    /// `traits` classifies a definition from its full text.
    pub fn diff_file(
        &mut self,
        path: &RepoPath,
        older: Option<FileVersion<'_>>,
        newer: Option<FileVersion<'_>>,
        traits: &dyn Fn(&Definition, &[u8]) -> DefTraits,
    ) {
        self.paths.insert(path.clone());
        let old = older.map(own_texts).unwrap_or_default();
        let new = newer.map(own_texts).unwrap_or_default();
        let mut touched = Vec::new();
        let glue = NodeId::file_root(path);
        let (old_glue, new_glue) = (old.get(&glue), new.get(&glue));
        if older.is_some() && newer.is_some() && old_glue.map(|g| &g.1) != new_glue.map(|g| &g.1) {
            touched.push(TouchedDef {
                node: glue,
                path: path.clone(),
                kind: NodeKind::new("source_file"),
                name: None,
                delta: DefDelta::Edited,
                attributes_changed: false,
                test: false,
                dispatch: false,
            });
        }
        let ids: BTreeSet<NodeId> = old.keys().chain(new.keys()).copied().collect();
        let old_bytes = older.map_or(&[][..], |v| v.bytes);
        let new_bytes = newer.map_or(&[][..], |v| v.bytes);
        for id in ids {
            if id == glue {
                continue;
            }
            let (delta, def, bytes, before) = match (old.get(&id), new.get(&id)) {
                (None, Some((def, _))) => (DefDelta::Born, *def, new_bytes, None),
                (Some((def, _)), None) => (DefDelta::Died, *def, old_bytes, None),
                (Some((old_def, old_own)), Some((def, own))) => {
                    if old_own == own {
                        continue;
                    }
                    let before = traits(old_def, def_text(old_def, old_bytes));
                    (DefDelta::Edited, *def, new_bytes, Some(before))
                }
                (None, None) => continue,
            };
            let now = traits(def, def_text(def, bytes));
            let attributes_changed = match &before {
                Some(before) => before.attributes != now.attributes,
                None => !now.attributes.is_empty(),
            };
            let (was_test, was_dispatch) = before.map_or((false, false), |b| (b.test, b.dispatch));
            touched.push(TouchedDef {
                node: id,
                path: path.clone(),
                kind: def.kind,
                name: def.name.clone(),
                delta,
                attributes_changed,
                test: now.test || was_test,
                dispatch: now.dispatch || was_dispatch,
            });
        }
        self.touched.extend(touched);
    }

    /// Nodes of every touched definition (glue included).
    #[must_use]
    pub fn nodes(&self) -> BTreeSet<NodeId> {
        self.touched.iter().map(|t| t.node).collect()
    }

    /// Merge `other` into `self` (for example drift since the coverage
    /// snapshot plus the change itself). A node touched by both keeps the
    /// first classification, widened by the second's flags.
    pub fn extend(&mut self, other: ChangeFacts) {
        self.paths.extend(other.paths);
        let mut by_node: BTreeMap<NodeId, usize> = self
            .touched
            .iter()
            .enumerate()
            .map(|(i, t)| (t.node, i))
            .collect();
        for t in other.touched {
            match by_node.get(&t.node) {
                Some(&i) => {
                    let mine = &mut self.touched[i];
                    mine.attributes_changed |= t.attributes_changed;
                    mine.test |= t.test;
                    mine.dispatch |= t.dispatch;
                    if mine.delta != t.delta {
                        // Born then died, or edited then died: whichever,
                        // the definition is not the same as before.
                        mine.delta = mine.delta.min(t.delta);
                    }
                }
                None => {
                    by_node.insert(t.node, self.touched.len());
                    self.touched.push(t);
                }
            }
        }
    }
}

fn def_text<'a>(def: &Definition, bytes: &'a [u8]) -> &'a [u8] {
    bytes.get(def.span.clone()).unwrap_or_default()
}

/// Own text of every definition of `file`, and of its glue under
/// [`NodeId::file_root`].
fn own_texts(file: FileVersion<'_>) -> BTreeMap<NodeId, (&Definition, Vec<u8>)> {
    let mut children: BTreeMap<Option<NodeId>, Vec<&Definition>> = BTreeMap::new();
    for def in file.defs {
        children.entry(def.parent).or_default().push(def);
    }
    let mut out = BTreeMap::new();
    let cut = |span: Range<usize>, kids: Option<&Vec<&Definition>>| -> Vec<u8> {
        let mut kids: Vec<&Definition> = kids.cloned().unwrap_or_default();
        kids.sort_by_key(|d| d.span.start);
        let mut text = Vec::new();
        let mut at = span.start;
        for kid in kids {
            let start = kid.span.start.clamp(at, span.end);
            text.extend_from_slice(file.bytes.get(at..start).unwrap_or_default());
            text.extend_from_slice(format!("\u{0}{}\u{0}", kid.node).as_bytes());
            at = kid.span.end.clamp(at, span.end);
        }
        text.extend_from_slice(file.bytes.get(at..span.end).unwrap_or_default());
        text
    };
    for def in file.defs {
        out.insert(
            def.node,
            (def, cut(def.span.clone(), children.get(&Some(def.node)))),
        );
    }
    if let Some(first) = file.defs.first() {
        let root = NodeId::file_root(&first.path);
        let glue = cut(0..file.bytes.len(), children.get(&None));
        // The glue entry needs a definition to point at; it is never read
        // as one (`diff_file` skips the root id).
        out.insert(root, (first, glue));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn path() -> RepoPath {
        RepoPath::from_str("src/lib.rs").expect("parse test path")
    }

    fn def(node: u128, kind: &str, span: Range<usize>, parent: Option<u128>) -> Definition {
        Definition {
            node: NodeId::from_u128(node),
            path: path(),
            kind: NodeKind::new(kind),
            name: None,
            span,
            parent: parent.map(NodeId::from_u128),
        }
    }

    fn traits(_: &Definition, text: &[u8]) -> DefTraits {
        let text = String::from_utf8_lossy(text);
        DefTraits {
            attributes: text
                .lines()
                .filter(|l| l.trim_start().starts_with("#["))
                .map(str::to_owned)
                .collect(),
            test: text.contains("#[test]"),
            dispatch: text.contains(" for "),
        }
    }

    fn diff(old: (&str, Vec<Definition>), new: (&str, Vec<Definition>)) -> ChangeFacts {
        let mut facts = ChangeFacts::default();
        facts.diff_file(
            &path(),
            Some(FileVersion {
                bytes: old.0.as_bytes(),
                defs: &old.1,
            }),
            Some(FileVersion {
                bytes: new.0.as_bytes(),
                defs: &new.1,
            }),
            &traits,
        );
        facts
    }

    #[test]
    fn a_method_edit_touches_the_method_not_the_impl() {
        let old = "impl A for B {\n fn f() { 1 }\n}\n";
        let new = "impl A for B {\n fn f() { 2 }\n}\n";
        let defs = |s: &str| {
            vec![
                def(1, "impl_item", 0..s.len() - 1, None),
                def(2, "function_item", 15..28, Some(1)),
            ]
        };
        let facts = diff((old, defs(old)), (new, defs(new)));
        assert_eq!(facts.nodes(), [NodeId::from_u128(2)].into_iter().collect());
        let t = &facts.touched[0];
        assert_eq!(t.delta, DefDelta::Edited);
        assert!(!t.attributes_changed && !t.test);
        assert!(facts.paths.contains(&path()));
    }

    #[test]
    fn births_deaths_glue_and_attributes() {
        let old = "use a;\n#[inline]\nfn f() {}\nfn g() {}\n";
        let new = "use b;\n#[cold]\nfn f() {}\n#[test]\nfn h() {}\n";
        let old_defs = vec![
            def(1, "function_item", 7..26, None),
            def(2, "function_item", 27..36, None),
        ];
        let new_defs = vec![
            def(1, "function_item", 7..24, None),
            def(3, "function_item", 25..42, None),
        ];
        let facts = diff((old, old_defs), (new, new_defs));
        let by: BTreeMap<NodeId, &TouchedDef> = facts.touched.iter().map(|t| (t.node, t)).collect();
        let glue = by[&NodeId::file_root(&path())];
        assert!(glue.is_glue() && glue.delta == DefDelta::Edited);
        let f = by[&NodeId::from_u128(1)];
        assert!(f.delta == DefDelta::Edited && f.attributes_changed);
        assert_eq!(by[&NodeId::from_u128(2)].delta, DefDelta::Died);
        let h = by[&NodeId::from_u128(3)];
        assert!(h.delta == DefDelta::Born && h.test && h.attributes_changed);
    }

    #[test]
    fn unchanged_files_touch_nothing_and_extend_merges() {
        let src = "fn f() {}\n";
        let defs = vec![def(1, "function_item", 0..9, None)];
        let mut facts = diff((src, defs.clone()), (src, defs));
        assert!(facts.touched.is_empty());
        let mut other = ChangeFacts::default();
        other.touched.push(TouchedDef {
            node: NodeId::from_u128(1),
            path: path(),
            kind: NodeKind::new("function_item"),
            name: None,
            delta: DefDelta::Edited,
            attributes_changed: false,
            test: false,
            dispatch: false,
        });
        let mut twice = other.clone();
        twice.touched[0].dispatch = true;
        facts.extend(other);
        facts.extend(twice);
        assert_eq!(facts.touched.len(), 1);
        assert!(facts.touched[0].dispatch);
    }

    #[test]
    fn line_ranges_are_one_based_and_inclusive() {
        let src = b"a\nbc\nd\n";
        assert_eq!(line_range(src, &(0..1)), 1..2);
        assert_eq!(line_range(src, &(2..6)), 2..4);
        assert_eq!(line_range(src, &(2..5)), 2..3);
    }
}
