//! Test selection for cargo (ADR 0022).
//!
//! Tests are selected by the coverage record's `Tests(t, def)` edges and by
//! static edges (a test that is itself in the impact set references an
//! impacted definition within the bound), plus every test the change adds
//! or edits, plus every test that failed during the coverage run (its
//! edges are partial). Whatever coverage cannot attribute falls back to whole
//! packages and their reverse dependencies; no usable coverage record, or
//! an impact set over the threshold, falls back to everything.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use hord_core::{NodeId, ObjectId, RepoPath};
use hord_verify::{CoverageRecord, DefDelta, ImpactSet, TestTarget};
use serde::{Deserialize, Serialize};

use crate::CargoWorkspace;

/// Why selection widened to whole packages or everything.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Fallback {
    /// No coverage record for this toolchain: run everything.
    NoCoverage,
    /// The coverage record was made with another toolchain: run everything.
    ToolchainMismatch,
    /// The impact set exceeds the policy threshold: run everything.
    ImpactThreshold {
        /// Impacted nodes.
        size: usize,
        /// The threshold.
        max: usize,
    },
    /// A workspace-wide file changed (root manifest, lockfile, `.cargo/`,
    /// toolchain file): run everything.
    WorkspaceFile(RepoPath),
    /// A package manifest changed.
    Manifest(RepoPath),
    /// A build script changed.
    BuildScript(RepoPath),
    /// A file no adapter parses changed inside a package (it may be
    /// `include_str!`-ed or read by a test at run time).
    NonRust(RepoPath),
    /// Text outside every definition of a Rust file changed.
    Glue(RepoPath),
    /// A `use`, `mod`, `extern crate`, or inner attribute changed.
    Declaration(NodeId),
    /// A `macro_rules!` definition changed.
    Macro(NodeId),
    /// A `const` or `static` changed.
    Initializer(NodeId),
    /// A definition's outer attributes changed.
    Attributes(NodeId),
    /// A trait impl was born or died.
    DispatchImpl(NodeId),
    /// A function changed that the coverage record has nothing for.
    Uncovered(NodeId),
    /// A test changed whose package is unknown.
    UnknownPackage(RepoPath),
}

impl Fallback {
    /// Short name of this fallback's kind, e.g. `non-rust`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoCoverage => "no-coverage",
            Self::ToolchainMismatch => "toolchain-mismatch",
            Self::ImpactThreshold { .. } => "impact-threshold",
            Self::WorkspaceFile(_) => "workspace-file",
            Self::Manifest(_) => "manifest",
            Self::BuildScript(_) => "build-script",
            Self::NonRust(_) => "non-rust",
            Self::Glue(_) => "glue",
            Self::Declaration(_) => "declaration",
            Self::Macro(_) => "macro",
            Self::Initializer(_) => "initializer",
            Self::Attributes(_) => "attributes",
            Self::DispatchImpl(_) => "dispatch-impl",
            Self::Uncovered(_) => "uncovered",
            Self::UnknownPackage(_) => "unknown-package",
        }
    }

    /// Whether this fallback runs the whole workspace.
    #[must_use]
    pub fn is_full(&self) -> bool {
        matches!(
            self,
            Self::NoCoverage
                | Self::ToolchainMismatch
                | Self::ImpactThreshold { .. }
                | Self::WorkspaceFile(_)
        )
    }
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCoverage => write!(f, "no coverage record"),
            Self::ToolchainMismatch => write!(f, "coverage record is for another toolchain"),
            Self::ImpactThreshold { size, max } => write!(f, "impact set {size} > {max}"),
            Self::WorkspaceFile(p) => write!(f, "workspace file {p}"),
            Self::Manifest(p) => write!(f, "manifest {p}"),
            Self::BuildScript(p) => write!(f, "build script {p}"),
            Self::NonRust(p) => write!(f, "non-Rust file {p}"),
            Self::Glue(p) => write!(f, "file-level glue in {p}"),
            Self::Declaration(n) => write!(f, "use/mod/attribute declaration {n}"),
            Self::Macro(n) => write!(f, "macro_rules! {n}"),
            Self::Initializer(n) => write!(f, "const/static {n}"),
            Self::Attributes(n) => write!(f, "attributes of {n}"),
            Self::DispatchImpl(n) => write!(f, "trait impl born or died {n}"),
            Self::Uncovered(n) => write!(f, "no coverage record for {n}"),
            Self::UnknownPackage(p) => write!(f, "no package for {p}"),
        }
    }
}

/// Which tests to run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    /// Run the whole workspace's tests.
    pub full: bool,
    /// Run every test of these packages (fallback, reverse deps included).
    pub packages: BTreeSet<String>,
    /// Run these tests by exact name, per package and test binary.
    pub exact: BTreeMap<(String, TestTarget), BTreeSet<String>>,
    /// Run tests whose names contain these filters, per package: tests the
    /// change adds, which no record names yet.
    pub filters: BTreeMap<String, BTreeSet<String>>,
    /// Run the doctests of these packages (coverage does not see doctests).
    pub doc: BTreeSet<String>,
    /// Why selection widened, in rule order.
    pub fallbacks: Vec<Fallback>,
}

impl Selection {
    /// A selection that runs everything.
    #[must_use]
    pub fn everything(reason: Fallback) -> Self {
        Self {
            full: true,
            fallbacks: vec![reason],
            ..Self::default()
        }
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.full
            && self.packages.is_empty()
            && self.exact.is_empty()
            && self.filters.is_empty()
            && self.doc.is_empty()
    }
}

/// Inputs to [`select`].
#[derive(Clone, Copy, Debug)]
pub struct SelectInput<'a> {
    /// The coverage record, if any. Its snapshot must be the older side of
    /// `impact.facts` (drift since coverage ran is part of the change).
    pub coverage: Option<&'a CoverageRecord>,
    /// Toolchain the selected tests will run with.
    pub toolchain: ObjectId,
    /// The workspace of the snapshot being verified.
    pub workspace: &'a CargoWorkspace,
    /// The change's impact set and facts.
    pub impact: &'a ImpactSet,
    /// Run everything above this many impacted nodes.
    pub max_impact: Option<usize>,
}

/// Select the tests that can observe the change (ADR 0022).
#[must_use]
pub fn select(input: SelectInput<'_>) -> Selection {
    select_ignoring(input, &BTreeSet::new())
}

/// [`select`] with the fallback kinds in `ignore` (by [`Fallback::kind`])
/// turned off. **For measurement only** (what a trigger costs,
/// `bench/m4-eval`): the result is not a safe selection.
#[must_use]
pub fn select_ignoring(input: SelectInput<'_>, ignore: &BTreeSet<&str>) -> Selection {
    let Some(coverage) = input.coverage else {
        return Selection::everything(Fallback::NoCoverage);
    };
    if coverage.toolchain != input.toolchain {
        return Selection::everything(Fallback::ToolchainMismatch);
    }
    if let Some(max) = input.max_impact
        && input.impact.len() > max
    {
        return Selection::everything(Fallback::ImpactThreshold {
            size: input.impact.len(),
            max,
        });
    }
    let ws = input.workspace;
    let facts = &input.impact.facts;
    let mut sel = Selection::default();
    let mut fallback_pkgs: BTreeSet<String> = BTreeSet::new();
    let mut touched_pkgs: BTreeSet<String> = BTreeSet::new();
    let widen =
        |sel: &mut Selection, pkgs: &mut BTreeSet<String>, path: &RepoPath, why: Fallback| {
            if ignore.contains(why.kind()) {
                return;
            }
            if let Some(p) = ws.package_of(path) {
                pkgs.insert(p.name.clone());
                sel.fallbacks.push(why);
            } else if ws.is_workspace_wide(path) {
                sel.fallbacks.push(why);
                sel.full = true;
            }
        };

    for path in &facts.paths {
        if let Some(p) = ws.package_of(path) {
            touched_pkgs.insert(p.name.clone());
        }
        let name = path.components().last().map_or("", String::as_str);
        if ws.is_workspace_wide(path) {
            if ignore.contains("workspace-file") {
                continue;
            }
            sel.fallbacks.push(Fallback::WorkspaceFile(path.clone()));
            sel.full = true;
        } else if name == "Cargo.toml" || name == "Cargo.lock" {
            widen(
                &mut sel,
                &mut fallback_pkgs,
                path,
                Fallback::Manifest(path.clone()),
            );
        } else if ws.is_build_script(path) {
            widen(
                &mut sel,
                &mut fallback_pkgs,
                path,
                Fallback::BuildScript(path.clone()),
            );
        } else if !name.ends_with(".rs") {
            widen(
                &mut sel,
                &mut fallback_pkgs,
                path,
                Fallback::NonRust(path.clone()),
            );
        }
    }
    if sel.full {
        return Selection {
            full: true,
            fallbacks: sel.fallbacks,
            ..Selection::default()
        };
    }

    let mut lookup: BTreeSet<NodeId> = input.impact.node_set();
    for t in &facts.touched {
        let rule = if t.is_glue() {
            Some(Fallback::Glue(t.path.clone()))
        } else {
            match t.kind.as_str() {
                "use_declaration"
                | "mod_item"
                | "extern_crate_declaration"
                | "inner_attribute_item"
                | "foreign_mod_item" => Some(Fallback::Declaration(t.node)),
                "macro_definition" => Some(Fallback::Macro(t.node)),
                "const_item" | "static_item" => Some(Fallback::Initializer(t.node)),
                _ if t.attributes_changed && !t.test => Some(Fallback::Attributes(t.node)),
                "impl_item" if t.dispatch && t.delta != DefDelta::Edited => {
                    Some(Fallback::DispatchImpl(t.node))
                }
                "function_item"
                    if !t.test
                        && t.delta != DefDelta::Died
                        && !coverage.is_instrumented(t.node) =>
                {
                    Some(Fallback::Uncovered(t.node))
                }
                _ => None,
            }
        };
        let rule = rule.filter(|r| !ignore.contains(r.kind()));
        if let Some(rule) = rule {
            widen(&mut sel, &mut fallback_pkgs, &t.path, rule);
            continue;
        }
        if t.test {
            if t.delta == DefDelta::Died {
                continue;
            }
            let known: Vec<_> = coverage.tests_at(t.node).cloned().collect();
            if !known.is_empty() {
                for test in known {
                    sel.exact
                        .entry((test.package, test.target))
                        .or_default()
                        .insert(test.name);
                }
            } else if let Some(p) = ws.package_of(&t.path) {
                let name = t
                    .name
                    .as_ref()
                    .and_then(|n| n.as_str().rsplit("::").next().map(str::to_owned));
                match name {
                    Some(name) => {
                        sel.filters.entry(p.name.clone()).or_default().insert(name);
                    }
                    None => {
                        fallback_pkgs.insert(p.name.clone());
                        sel.fallbacks.push(Fallback::Uncovered(t.node));
                    }
                }
            } else {
                sel.fallbacks.push(Fallback::UnknownPackage(t.path.clone()));
            }
        }
        lookup.insert(t.node);
    }

    for test in coverage.tests_covering(&lookup) {
        sel.exact
            .entry((test.package.clone(), test.target.clone()))
            .or_default()
            .insert(test.name.clone());
    }
    // A test that failed during the coverage run has partial edges: it is
    // always selected (it cannot be ruled out).
    for t in coverage.tests.iter().filter(|t| t.failed) {
        sel.exact
            .entry((t.test.package.clone(), t.test.target.clone()))
            .or_default()
            .insert(t.test.name.clone());
    }
    // Static edges: a test in the impact set references an impacted node.
    for node in input.impact.nodes.keys() {
        for test in coverage.tests_at(*node) {
            sel.exact
                .entry((test.package.clone(), test.target.clone()))
                .or_default()
                .insert(test.name.clone());
        }
    }

    sel.packages = ws.with_reverse_deps(&fallback_pkgs);
    sel.exact.retain(|(pkg, _), _| !sel.packages.contains(pkg));
    sel.filters.retain(|pkg, _| !sel.packages.contains(pkg));
    sel.doc = ws
        .with_reverse_deps(&touched_pkgs)
        .into_iter()
        .filter(|p| !sel.packages.contains(p) && ws.packages[p].has_doctests())
        .collect();
    sel
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::{NodeKind, QualifiedName};
    use hord_verify::{ChangeFacts, TestRef, TouchedDef};

    use super::*;
    use crate::cargo::tests::sample;

    fn n(v: u128) -> NodeId {
        NodeId::from_u128(v)
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::from_str(s).unwrap()
    }

    fn tc() -> ObjectId {
        ObjectId::from_bytes([7; 32])
    }

    fn t(pkg: &str, target: &str, name: &str) -> TestRef {
        TestRef {
            package: pkg.into(),
            target: TestTarget {
                kind: if target == "lib" {
                    "lib".into()
                } else {
                    "test".into()
                },
                name: target.into(),
            },
            name: name.into(),
        }
    }

    /// `f`(1) is run by `a::t1`(100) and `a::t2`(101); `g`(2) by `t2`;
    /// `h`(3) by nothing but instrumented; `b::k`(4) by `b::t3`(102).
    fn record() -> CoverageRecord {
        CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            tc(),
            [n(1), n(2), n(3), n(4), n(100), n(101), n(102)]
                .into_iter()
                .collect(),
            vec![
                (
                    t("a", "testsuite", "m::t1"),
                    Some(n(100)),
                    [n(1), n(100)].into_iter().collect(),
                    false,
                ),
                (
                    t("a", "testsuite", "m::t2"),
                    Some(n(101)),
                    [n(1), n(2), n(101)].into_iter().collect(),
                    false,
                ),
                (
                    t("b", "b", "t3"),
                    Some(n(102)),
                    [n(4), n(102)].into_iter().collect(),
                    false,
                ),
            ],
        )
    }

    fn touched(node: u128, path: &str, kind: &str, delta: DefDelta) -> TouchedDef {
        TouchedDef {
            node: n(node),
            path: p(path),
            kind: NodeKind::new(kind),
            name: None,
            delta,
            attributes_changed: false,
            test: false,
            dispatch: false,
        }
    }

    fn impact(write: &[u128], extra: &[u128], touched: Vec<TouchedDef>) -> ImpactSet {
        let mut facts = ChangeFacts::default();
        for t in &touched {
            facts.paths.insert(t.path.clone());
        }
        facts.touched = touched;
        ImpactSet {
            write_set: write.iter().copied().map(n).collect(),
            nodes: write
                .iter()
                .map(|w| (n(*w), 0))
                .chain(extra.iter().map(|e| (n(*e), 1)))
                .collect(),
            facts,
        }
    }

    fn run(record: Option<&CoverageRecord>, impact: &ImpactSet) -> Selection {
        let ws = sample();
        select(SelectInput {
            coverage: record,
            toolchain: tc(),
            workspace: &ws,
            impact,
            max_impact: Some(50),
        })
    }

    fn exact(sel: &Selection) -> BTreeSet<String> {
        sel.exact.values().flatten().cloned().collect()
    }

    #[test]
    fn tests_that_failed_under_coverage_are_always_selected() {
        let r = CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            tc(),
            [n(1)].into_iter().collect(),
            vec![
                (
                    t("a", "testsuite", "ok"),
                    None,
                    [n(1)].into_iter().collect(),
                    false,
                ),
                (t("a", "testsuite", "broken"), None, BTreeSet::new(), true),
            ],
        );
        let sel = run(Some(&r), &impact(&[3], &[], vec![]));
        assert_eq!(exact(&sel), ["broken".to_owned()].into_iter().collect());
    }

    #[test]
    fn coverage_selects_only_tests_that_ran_the_change() {
        let r = record();
        let sel = run(
            Some(&r),
            &impact(
                &[2],
                &[],
                vec![touched(2, "src/g.rs", "function_item", DefDelta::Edited)],
            ),
        );
        assert!(!sel.full && sel.packages.is_empty() && sel.fallbacks.is_empty());
        assert_eq!(exact(&sel), ["m::t2".to_owned()].into_iter().collect());
        // Doctests of the touched package run (a has a library with doctests).
        assert_eq!(sel.doc, ["a".to_owned()].into_iter().collect());
        // A covered-but-never-run function selects nothing but doctests.
        let sel = run(
            Some(&r),
            &impact(
                &[3],
                &[],
                vec![touched(3, "src/g.rs", "function_item", DefDelta::Edited)],
            ),
        );
        assert!(sel.exact.is_empty() && sel.fallbacks.is_empty());
    }

    #[test]
    fn impact_nodes_select_their_tests_and_static_edges() {
        let r = record();
        // `g` changed; `f` is a dependent within the bound; test t1 (100) is
        // in the impact set by a static reference.
        let sel = run(
            Some(&r),
            &impact(
                &[3],
                &[1, 100],
                vec![touched(3, "src/g.rs", "function_item", DefDelta::Edited)],
            ),
        );
        assert_eq!(
            exact(&sel),
            ["m::t1".to_owned(), "m::t2".to_owned()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn no_record_mismatched_toolchain_and_threshold_run_everything() {
        let i = impact(&[1], &[], vec![]);
        assert_eq!(run(None, &i).fallbacks, vec![Fallback::NoCoverage]);
        let mut other = record();
        other.toolchain = ObjectId::from_bytes([8; 32]);
        assert!(run(Some(&other), &i).full);
        let big: Vec<u128> = (1000..1100).collect();
        let sel = run(Some(&record()), &impact(&big, &[], vec![]));
        assert!(
            sel.full
                && matches!(
                    sel.fallbacks[0],
                    Fallback::ImpactThreshold { size: 100, max: 50 }
                )
        );
        assert!(sel.fallbacks[0].is_full());
    }

    fn fallback_for(t: TouchedDef) -> Selection {
        let r = record();
        let node = t.node.as_u128();
        run(Some(&r), &impact(&[node], &[], vec![t]))
    }

    #[test]
    fn every_fallback_trigger_widens_to_the_package_and_reverse_deps() {
        let c_path = "crates/c/src/lib.rs";
        let all: BTreeSet<String> = ["a", "b", "c"].into_iter().map(str::to_owned).collect();
        type Case = (TouchedDef, fn(&Fallback) -> bool);
        let cases: Vec<Case> = vec![
            (
                touched(9, c_path, "use_declaration", DefDelta::Edited),
                |f| matches!(f, Fallback::Declaration(_)),
            ),
            (touched(9, c_path, "mod_item", DefDelta::Born), |f| {
                matches!(f, Fallback::Declaration(_))
            }),
            (
                touched(9, c_path, "inner_attribute_item", DefDelta::Edited),
                |f| matches!(f, Fallback::Declaration(_)),
            ),
            (
                touched(9, c_path, "macro_definition", DefDelta::Edited),
                |f| matches!(f, Fallback::Macro(_)),
            ),
            (touched(9, c_path, "const_item", DefDelta::Edited), |f| {
                matches!(f, Fallback::Initializer(_))
            }),
            (touched(9, c_path, "static_item", DefDelta::Born), |f| {
                matches!(f, Fallback::Initializer(_))
            }),
            (
                TouchedDef {
                    attributes_changed: true,
                    ..touched(4, c_path, "struct_item", DefDelta::Edited)
                },
                |f| matches!(f, Fallback::Attributes(_)),
            ),
            (
                TouchedDef {
                    dispatch: true,
                    ..touched(9, c_path, "impl_item", DefDelta::Born)
                },
                |f| matches!(f, Fallback::DispatchImpl(_)),
            ),
            (
                TouchedDef {
                    dispatch: true,
                    ..touched(9, c_path, "impl_item", DefDelta::Died)
                },
                |f| matches!(f, Fallback::DispatchImpl(_)),
            ),
            (touched(9, c_path, "function_item", DefDelta::Born), |f| {
                matches!(f, Fallback::Uncovered(_))
            }),
            (touched(9, c_path, "function_item", DefDelta::Edited), |f| {
                matches!(f, Fallback::Uncovered(_))
            }),
            (
                TouchedDef {
                    node: NodeId::file_root(&p(c_path)),
                    ..touched(0, c_path, "source_file", DefDelta::Edited)
                },
                |f| matches!(f, Fallback::Glue(_)),
            ),
        ];
        for (t, want) in cases {
            let label = format!("{:?} {:?}", t.kind, t.delta);
            let sel = fallback_for(t);
            assert!(!sel.full, "{label}");
            assert_eq!(sel.packages, all, "{label}");
            assert!(
                sel.fallbacks.iter().any(want),
                "{label}: {:?}",
                sel.fallbacks
            );
            assert!(
                sel.exact.is_empty(),
                "{label}: exact tests are subsumed by packages"
            );
        }
        // A trait impl edited in place is ordinary: its methods are covered.
        let sel = fallback_for(TouchedDef {
            dispatch: true,
            ..touched(9, c_path, "impl_item", DefDelta::Edited)
        });
        assert!(sel.packages.is_empty());
        // A died function selects the tests that ran it, no fallback.
        let sel = fallback_for(touched(2, "src/g.rs", "function_item", DefDelta::Died));
        assert!(sel.packages.is_empty() && exact(&sel).contains("m::t2"));
    }

    #[test]
    fn path_triggers() {
        let r = record();
        let with_paths = |paths: &[&str]| {
            let mut i = impact(&[1], &[], vec![]);
            i.facts.paths = paths.iter().map(|s| p(s)).collect();
            run(Some(&r), &i)
        };
        for wide in [
            "Cargo.lock",
            "Cargo.toml",
            ".cargo/config.toml",
            "rust-toolchain.toml",
        ] {
            let sel = with_paths(&[wide]);
            assert!(sel.full, "{wide}");
            assert!(
                matches!(sel.fallbacks[..], [Fallback::WorkspaceFile(_)]),
                "{wide}"
            );
        }
        let b: BTreeSet<String> = ["a", "b"].into_iter().map(str::to_owned).collect();
        let sel = with_paths(&["crates/b/Cargo.toml"]);
        assert!(
            !sel.full && sel.packages == b && matches!(sel.fallbacks[0], Fallback::Manifest(_))
        );
        let sel = with_paths(&["crates/b/build.rs"]);
        assert!(sel.packages == b && matches!(sel.fallbacks[0], Fallback::BuildScript(_)));
        let sel = with_paths(&["crates/b/data/fixture.json"]);
        assert!(sel.packages == b && matches!(sel.fallbacks[0], Fallback::NonRust(_)));
        // A Rust file with no touched definitions changes nothing by path.
        let sel = with_paths(&["crates/b/src/lib.rs"]);
        assert!(sel.packages.is_empty() && sel.fallbacks.is_empty());
    }

    #[test]
    fn ignoring_a_kind_measures_its_cost() {
        let r = record();
        let mut i = impact(
            &[2],
            &[],
            vec![touched(2, "src/g.rs", "function_item", DefDelta::Edited)],
        );
        i.facts.paths.insert(p("crates/b/README.md"));
        let ws = sample();
        let input = SelectInput {
            coverage: Some(&r),
            toolchain: tc(),
            workspace: &ws,
            impact: &i,
            max_impact: None,
        };
        let with = select(input);
        assert_eq!(with.fallbacks[0].kind(), "non-rust");
        assert!(with.packages.contains("b"));
        let without = select_ignoring(input, &["non-rust"].into_iter().collect());
        assert!(without.packages.is_empty() && without.fallbacks.is_empty());
        assert_eq!(exact(&without), ["m::t2".to_owned()].into_iter().collect());
    }

    #[test]
    fn added_and_edited_tests_select_themselves() {
        let r = record();
        let edited = TouchedDef {
            test: true,
            ..touched(
                100,
                "tests/testsuite/m.rs",
                "function_item",
                DefDelta::Edited,
            )
        };
        let sel = run(Some(&r), &impact(&[100], &[], vec![edited]));
        assert!(exact(&sel).contains("m::t1") && sel.fallbacks.is_empty());
        let added = TouchedDef {
            test: true,
            attributes_changed: true,
            name: Some(QualifiedName::new("testsuite::m::brand_new")),
            ..touched(200, "tests/testsuite/m.rs", "function_item", DefDelta::Born)
        };
        let sel = run(Some(&r), &impact(&[200], &[], vec![added]));
        assert!(sel.fallbacks.is_empty(), "{:?}", sel.fallbacks);
        assert_eq!(
            sel.filters["a"],
            ["brand_new".to_owned()].into_iter().collect()
        );
        let died = TouchedDef {
            test: true,
            ..touched(101, "tests/testsuite/m.rs", "function_item", DefDelta::Died)
        };
        let sel = run(Some(&r), &impact(&[101], &[], vec![died]));
        // The died test is not run; the tests covering it (itself) would be,
        // but it is gone from the suite, so only its own record matches.
        assert!(sel.filters.is_empty() && sel.packages.is_empty());
    }
}
