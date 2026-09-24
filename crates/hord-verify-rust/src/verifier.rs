//! [`RustVerifier`]: the cargo [`Verifier`] (spec §4.2 Tier 3, §4.3).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hord_core::{Evidence, EvidenceKind, ObjectId, RepoPath, SnapshotId};
use hord_verify::{
    Check, Checkout, CoverageRecord, Error, EvidenceIndex, ImpactSet, Result, TestRef, Toolchain,
    Verifier, VerifyPlan, VerifyPolicy,
};

use crate::select::{SelectInput, Selection, select};
use crate::{CargoRunner, CargoWorkspace};

/// Requirement names [`RustVerifier`] plans (spec §7.2).
pub mod requirement {
    /// `cargo check --all-targets` on the affected packages.
    pub const CHECK: &str = "check";
    /// Selected tests (ADR 0022).
    pub const TEST_SELECTED: &str = "test:selected";
    /// Every test of the workspace.
    pub const TEST_FULL: &str = "test:full";
    /// `cargo clippy --all-targets -- -D warnings` on the affected packages.
    pub const LINT: &str = "lint";
    /// `cargo bench` on the affected packages.
    pub const BENCH: &str = "bench";
}

/// The cargo verifier: plans `check`, `test:selected`, `test:full`,
/// `lint`, and `bench`, and runs them with a [`CargoRunner`].
#[derive(Clone, Debug)]
pub struct RustVerifier {
    toolchain: Toolchain,
    toolchain_id: ObjectId,
    workspace: CargoWorkspace,
    coverage: Option<Arc<CoverageRecord>>,
    /// Runs the planned commands.
    pub runner: CargoRunner,
    /// Quarantined tests, skipped by every test command.
    pub quarantine: BTreeSet<TestRef>,
}

impl RustVerifier {
    /// A verifier for a workspace built with `toolchain`.
    pub fn new(toolchain: Toolchain, workspace: CargoWorkspace) -> Result<Self> {
        Ok(Self {
            toolchain_id: toolchain.id()?,
            toolchain,
            workspace,
            coverage: None,
            runner: CargoRunner::default(),
            quarantine: BTreeSet::new(),
        })
    }

    /// Select tests with `record` (the newest coverage record for this
    /// toolchain, from [`hord_verify::find_coverage`]).
    #[must_use]
    pub fn with_coverage(mut self, record: Option<Arc<CoverageRecord>>) -> Self {
        self.coverage = record;
        self
    }

    /// The workspace it plans for.
    #[must_use]
    pub fn workspace(&self) -> &CargoWorkspace {
        &self.workspace
    }

    /// The test selection for `impact` under `policy`.
    #[must_use]
    pub fn selection(&self, impact: &ImpactSet, policy: &VerifyPolicy) -> Selection {
        select(SelectInput {
            coverage: self.coverage.as_deref(),
            toolchain: self.toolchain_id,
            workspace: &self.workspace,
            impact,
            max_impact: policy.max_impact,
        })
    }

    /// Packages a change touches, with their reverse dependencies: what
    /// `check`, `lint`, and `bench` run on (spec §7.1 item 3).
    #[must_use]
    pub fn affected_packages(&self, impact: &ImpactSet) -> BTreeSet<String> {
        let touched: BTreeSet<String> = impact
            .facts
            .paths
            .iter()
            .filter_map(|p| self.workspace.package_of(p))
            .map(|p| p.name.clone())
            .collect();
        self.workspace.with_reverse_deps(&touched)
    }

    /// The `cargo test` commands for `selection`, one per package and test
    /// binary, each with the quarantine skipped.
    #[must_use]
    pub fn test_checks(
        &self,
        selection: &Selection,
        requirement: &str,
        scope: Option<BTreeSet<hord_core::NodeId>>,
    ) -> Vec<Check> {
        let mut checks = Vec::new();
        // ADR 0026: a full run is `test:full`, which also satisfies
        // `test:selected`; a selection is `test:selected`.
        let qualifier = if selection.full || requirement == requirement::TEST_FULL {
            "full"
        } else {
            "selected"
        };
        let check = |args: Vec<String>| Check {
            requirement: requirement.to_owned(),
            kind: EvidenceKind::Test,
            qualifier: Some(qualifier.to_owned()),
            program: "cargo".into(),
            args,
            env: BTreeMap::new(),
            dir: RepoPath::default(),
            scope: scope.clone(),
        };
        let skips = |package: Option<&str>| -> Vec<String> {
            self.quarantine
                .iter()
                .filter(|t| package.is_none_or(|p| t.package == p))
                .flat_map(|t| ["--skip".to_owned(), t.name.clone()])
                .collect()
        };
        let with_skips = |mut args: Vec<String>, package: Option<&str>, exact: bool| {
            let skip = skips(package);
            if !skip.is_empty() || exact {
                args.push("--".into());
                if !skip.is_empty() {
                    args.push("--exact".into());
                }
                args.extend(skip);
            }
            args
        };
        if selection.full {
            checks.push(check(with_skips(
                vec!["test".into(), "--workspace".into(), "--no-fail-fast".into()],
                None,
                false,
            )));
            return checks;
        }
        for package in &selection.packages {
            checks.push(check(with_skips(
                vec![
                    "test".into(),
                    "-p".into(),
                    package.clone(),
                    "--no-fail-fast".into(),
                ],
                Some(package),
                false,
            )));
        }
        for ((package, target), names) in &selection.exact {
            let mut args = vec!["test".into(), "-p".into(), package.clone()];
            match target.kind.as_str() {
                "lib" => args.push("--lib".into()),
                kind @ ("bin" | "test" | "bench" | "example") => {
                    args.push(format!("--{kind}"));
                    args.push(target.name.clone());
                }
                _ => {}
            }
            args.extend(["--no-fail-fast".into(), "--".into(), "--exact".into()]);
            args.extend(names.iter().cloned());
            for t in &self.quarantine {
                if &t.package == package && &t.target == target {
                    args.push("--skip".into());
                    args.push(t.name.clone());
                }
            }
            checks.push(check(args));
        }
        for (package, filters) in &selection.filters {
            let mut args = vec![
                "test".into(),
                "-p".into(),
                package.clone(),
                "--no-fail-fast".into(),
                "--".into(),
            ];
            args.extend(filters.iter().cloned());
            checks.push(check(args));
        }
        for package in &selection.doc {
            checks.push(check(vec![
                "test".into(),
                "-p".into(),
                package.clone(),
                "--doc".into(),
                "--no-fail-fast".into(),
            ]));
        }
        checks
    }

    fn package_check(
        &self,
        requirement: &str,
        kind: EvidenceKind,
        sub: &[&str],
        packages: &BTreeSet<String>,
        tail: &[&str],
        scope: Option<BTreeSet<hord_core::NodeId>>,
    ) -> Check {
        let mut args: Vec<String> = sub.iter().map(|s| (*s).to_owned()).collect();
        if packages.len() == self.workspace.packages.len() {
            args.push("--workspace".into());
        } else {
            for p in packages {
                args.push("-p".into());
                args.push(p.clone());
            }
        }
        args.extend(tail.iter().map(|s| (*s).to_owned()));
        Check {
            requirement: requirement.to_owned(),
            kind,
            qualifier: None,
            program: "cargo".into(),
            args,
            env: BTreeMap::new(),
            dir: RepoPath::default(),
            scope,
        }
    }
}

impl Verifier for RustVerifier {
    fn lang(&self) -> &str {
        hord_lang_rust::LANG
    }

    fn toolchain(&self) -> &Toolchain {
        &self.toolchain
    }

    fn plan(
        &self,
        snapshot: SnapshotId,
        impact: &ImpactSet,
        policy: &VerifyPolicy,
    ) -> Result<VerifyPlan> {
        use requirement::*;
        let mut plan = VerifyPlan::new(snapshot, self.toolchain_id);
        let scope = Some(impact.node_set());
        let affected = self.affected_packages(impact);
        if policy.requires(CHECK) && !affected.is_empty() {
            plan.checks.push(self.package_check(
                CHECK,
                EvidenceKind::Check,
                &["check"],
                &affected,
                &["--all-targets"],
                scope.clone(),
            ));
        }
        if policy.requires(LINT) && !affected.is_empty() {
            plan.checks.push(self.package_check(
                LINT,
                EvidenceKind::Lint,
                &["clippy"],
                &affected,
                &["--all-targets", "--", "-D", "warnings"],
                scope.clone(),
            ));
        }
        if policy.requires(TEST_FULL) {
            let full = Selection::everything(crate::select::Fallback::NoCoverage);
            plan.checks.extend(self.test_checks(&full, TEST_FULL, None));
        }
        if policy.requires(TEST_SELECTED) {
            let selection = self.selection(impact, policy);
            let scope = if selection.full { None } else { scope.clone() };
            for f in &selection.fallbacks {
                plan.notes.push(format!("fallback: {f}"));
            }
            let exact: usize = selection.exact.values().map(BTreeSet::len).sum();
            plan.notes.push(format!(
                "selected: full={} packages={} exact={exact} filters={} doc={}",
                selection.full,
                selection.packages.len(),
                selection.filters.values().map(BTreeSet::len).sum::<usize>(),
                selection.doc.len()
            ));
            plan.checks
                .extend(self.test_checks(&selection, TEST_SELECTED, scope));
        }
        if policy.requires(BENCH) && !affected.is_empty() {
            plan.checks.push(self.package_check(
                BENCH,
                EvidenceKind::Bench,
                &["bench"],
                &affected,
                &[],
                scope,
            ));
        }
        Ok(plan)
    }

    fn run(
        &self,
        checkout: &Checkout,
        plan: &VerifyPlan,
        logs: &dyn EvidenceIndex,
    ) -> Result<Vec<Evidence>> {
        if checkout.snapshot != plan.snapshot {
            return Err(Error::WrongCheckout {
                planned: plan.snapshot,
                checkout: checkout.snapshot,
            });
        }
        let mut out = Vec::with_capacity(plan.checks.len());
        for check in &plan.checks {
            let output = self.runner.run(&checkout.root, check)?;
            out.push(
                self.runner
                    .evidence(plan.snapshot, plan.toolchain, check, &output, logs)?,
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hord_core::{NodeId, NodeKind};
    use hord_verify::{ChangeFacts, DefDelta, TestTarget, TouchedDef};

    use super::*;
    use crate::cargo::tests::sample;

    fn verifier(record: Option<CoverageRecord>) -> RustVerifier {
        let tc = Toolchain::new("rust").with("rustc", "x");
        RustVerifier::new(tc, sample())
            .unwrap()
            .with_coverage(record.map(Arc::new))
    }

    fn impact_in(path: &str, node: u128) -> ImpactSet {
        let mut facts = ChangeFacts::default();
        let path = RepoPath::from_str(path).unwrap();
        facts.paths.insert(path.clone());
        facts.touched.push(TouchedDef {
            node: NodeId::from_u128(node),
            path,
            kind: NodeKind::new("function_item"),
            name: None,
            delta: DefDelta::Edited,
            attributes_changed: false,
            test: false,
            dispatch: false,
        });
        ImpactSet {
            write_set: [NodeId::from_u128(node)].into_iter().collect(),
            nodes: [(NodeId::from_u128(node), 0)].into_iter().collect(),
            facts,
        }
    }

    #[test]
    fn plans_each_requirement() {
        let v = verifier(None);
        let policy =
            VerifyPolicy::default().requiring(["check", "lint", "test:selected", "review:human"]);
        let plan = v
            .plan(
                ObjectId::from_bytes([1; 32]),
                &impact_in("crates/b/src/lib.rs", 5),
                &policy,
            )
            .unwrap();
        let commands: Vec<String> = plan.checks.iter().map(Check::command).collect();
        assert_eq!(
            commands,
            vec![
                "cargo check -p a -p b --all-targets",
                "cargo clippy -p a -p b --all-targets -- -D warnings",
                "cargo test --workspace --no-fail-fast",
            ]
        );
        assert!(plan.notes.iter().any(|n| n.contains("no coverage record")));
        // A full run claims no scope.
        assert_eq!(plan.checks[2].scope, None);
        assert!(plan.checks[0].scope.is_some());
    }

    #[test]
    fn selected_tests_run_exactly_with_the_quarantine_skipped() {
        let tc = Toolchain::new("rust").with("rustc", "x");
        let target = TestTarget {
            kind: "test".into(),
            name: "testsuite".into(),
        };
        let t = |name: &str| TestRef {
            package: "a".into(),
            target: target.clone(),
            name: name.into(),
        };
        let record = CoverageRecord::new(
            ObjectId::from_bytes([1; 32]),
            tc.id().unwrap(),
            BTreeSet::new(),
            vec![
                (
                    t("x::one"),
                    None,
                    [NodeId::from_u128(5)].into_iter().collect(),
                    false,
                ),
                (
                    t("x::two"),
                    None,
                    [NodeId::from_u128(5)].into_iter().collect(),
                    false,
                ),
            ],
        );
        let mut v = verifier(Some(record));
        v.quarantine.insert(t("x::two"));
        let policy = VerifyPolicy::default().requiring(["test:selected"]);
        let plan = v
            .plan(
                ObjectId::from_bytes([1; 32]),
                &impact_in("src/x.rs", 5),
                &policy,
            )
            .unwrap();
        let commands: Vec<String> = plan.checks.iter().map(Check::command).collect();
        assert_eq!(
            commands,
            vec![
                "cargo test -p a --test testsuite --no-fail-fast -- --exact x::one x::two --skip x::two",
                "cargo test -p a --doc --no-fail-fast",
            ]
        );
        let full = v.test_checks(
            &Selection::everything(crate::select::Fallback::NoCoverage),
            "test:full",
            None,
        );
        assert_eq!(
            full[0].command(),
            "cargo test --workspace --no-fail-fast -- --exact --skip x::two"
        );
    }

    #[test]
    fn run_refuses_another_snapshot() {
        let v = verifier(None);
        let plan = VerifyPlan::new(ObjectId::from_bytes([1; 32]), ObjectId::from_bytes([2; 32]));
        let checkout = Checkout {
            root: std::env::temp_dir(),
            snapshot: ObjectId::from_bytes([3; 32]),
        };
        let index = hord_verify::MemoryIndex::new();
        assert!(matches!(
            v.run(&checkout, &plan, &index),
            Err(Error::WrongCheckout { .. })
        ));
    }
}
