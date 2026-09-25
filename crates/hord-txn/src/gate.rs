//! Verification and policy at landing (spec §6.5, §7; ADRs 0022, 0025,
//! 0026).
//!
//! For each candidate the lander:
//!
//! 1. reads **head's policy** (`.hord-policy.toml` in the candidate's
//!    landing base, ADR 0026) and the change's **policy facts** (actor,
//!    write-set size, touched definitions with their kinds and visibility,
//!    touched paths);
//! 2. derives the **requirements that apply** (the policy's verdict on the
//!    facts without evidence) and hands them to the [`Verifier`] as a
//!    [`VerifyPolicy`], with a [`VerifyContext`] that yields the impact set
//!    (over [`crate::graph`]), a checkout of the candidate snapshot, and the
//!    evidence index (the store) on demand;
//! 3. evaluates the policy against the evidence indexed for the candidate
//!    snapshot once verification is done.
//!
//! Verifiers: [`EngineVerifier`] runs a language's `hord_verify::Verifier`
//! (the default is [`RustFactory`], cargo); [`FailClosedVerifier`] is the
//! fallback without a toolchain; [`StubVerifier`] lands everything (opt-in,
//! for throughput simulations, spec §12 M3).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use hord_core::{ChangeId, ChangeRecord, Evidence, NodeId, RepoPath, SnapshotId};
use hord_policy::{CompiledPolicy, Decision, EvidenceFact, Facts, POLICY_PATH, TouchedDefinition};
use hord_verify::{
    Checkout, CoverageRecord, Drift, EvidenceIndex, ImpactBound, ImpactSet, Toolchain, VerifyPlan,
    VerifyPolicy,
};
use hord_verify_rust::{Cancel, DefinitionIndex, InstrumentedRun};
use tokio_util::task::TaskTracker;

use crate::conflict::ConflictReport;
use crate::repo::{Inner, fs_path, lock};
use crate::semantic::definitions;
use crate::{Error, Result};

pub use hord_verify::Verdict;

// ------------------------------------------------------------ verifier API

/// What a [`Verifier`] may ask for, lazily: nothing is computed or checked
/// out unless the verifier needs it.
pub trait VerifyContext: Send + Sync {
    /// The change's impact set under `bound` (ADR 0022, amendments after
    /// the 50-commit measurement): its own facts (base to result), seeded
    /// by the writes coverage cannot attribute
    /// ([`hord_verify::impact_set_attributable`]).
    fn impact(&self, bound: ImpactBound) -> hord_verify::Result<ImpactSet>;

    /// The landed changes up to the change's base, each with its write set,
    /// for per-test coverage freshness ([`hord_verify::Drift`]). `None`
    /// when the chain is unknown (selection then treats every recorded
    /// test as stale).
    fn drift(&self) -> hord_verify::Result<Option<Arc<Drift>>>;

    /// The definitions of the snapshot being verified, by file and line,
    /// to attribute coverage to.
    fn definitions(&self) -> hord_verify::Result<Arc<DefinitionIndex>>;

    /// A directory holding exactly the snapshot being verified.
    fn checkout(&self) -> hord_verify::Result<Checkout>;

    /// Where evidence is stored and indexed (ADR 0025): the store.
    fn index(&self) -> &dyn EvidenceIndex;

    /// Snapshots to search for a coverage record, newest first.
    fn history(&self) -> Vec<SnapshotId>;

    /// The verifier planned `plan` (the `Verifying` event; `hord verify
    /// --plan-only`).
    fn planned(&self, plan: &VerifyPlan);

    /// Set when verification must stop (the repository is shutting down):
    /// running commands are killed and nothing is recorded. Never set by
    /// default.
    fn cancel(&self) -> Cancel {
        Cancel::default()
    }

    /// Where to run blocking verification work so that closing the
    /// repository waits for it; `None` runs it untracked.
    fn tasks(&self) -> Option<TaskTracker> {
        None
    }
}

/// What the lander asks a [`Verifier`] to check.
pub struct VerifyRequest {
    /// Id the change will land under.
    pub change_id: ChangeId,
    /// The record as it will land (its base is the landing base).
    pub change: Arc<ChangeRecord>,
    /// Set overlaps and soft merge conflicts to re-check.
    pub report: ConflictReport,
    /// The requirements head's policy applies to this change, the impact
    /// bound, and `max_impact` (ADR 0022).
    pub policy: VerifyPolicy,
    /// Impact set, checkout, and evidence index on demand.
    pub context: Arc<dyn VerifyContext>,
    /// Plan and report the plan ([`VerifyContext::planned`]) without
    /// running anything (`hord verify --plan-only`).
    pub plan_only: bool,
}

impl std::fmt::Debug for VerifyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyRequest")
            .field("change_id", &self.change_id)
            .field("policy", &self.policy)
            .field("plan_only", &self.plan_only)
            .finish_non_exhaustive()
    }
}

/// Future returned by [`Verifier::verify`].
pub type VerifyFuture<'a> = Pin<Box<dyn Future<Output = Verdict> + Send + 'a>>;

/// Verification at landing (spec §6.5, ADR 0025): checks the request and
/// returns the evidence it produced or reused.
///
/// The lander awaits it outside any lock and, with the speculative window
/// (ADR 0025), for several candidates at once.
pub trait Verifier: Send + Sync {
    /// Verify `request.change`'s result snapshot.
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_>;
}

/// Lands everything (spec §12 M3: verification stubbed). Opt-in.
#[derive(Clone, Copy, Debug, Default)]
pub struct StubVerifier;

impl Verifier for StubVerifier {
    fn verify(&self, _request: VerifyRequest) -> VerifyFuture<'_> {
        Box::pin(async {
            Verdict::Pass {
                evidence: Vec::new(),
            }
        })
    }
}

/// The verdict when nothing was verified (spec §15): a clean report
/// passes. So do overlaps confined to files a purpose-built adapter merge
/// resolved (ADR 0013 amendment, [`ConflictReport::only_adapter_merged`]).
/// Anything else needs evidence and fails.
fn fail_closed(report: &ConflictReport) -> Verdict {
    if report.only_adapter_merged() {
        return Verdict::Pass {
            evidence: Vec::new(),
        };
    }
    let mut kinds: Vec<&str> = report
        .conflicts
        .iter()
        .map(|c| match c.kind {
            crate::ConflictKind::WriteWrite => "write-write",
            crate::ConflictKind::ReadWrite => "read-write",
            crate::ConflictKind::WriteRead => "write-read",
        })
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    Verdict::Fail {
        evidence: Vec::new(),
        reason: format!(
            "unverified overlap: {} set conflict(s) [{}] and {} soft merge conflict(s); \
             nothing verified the rebased change, so it does not land",
            report.conflicts.len(),
            kinds.join(", "),
            report.merge.len(),
        ),
    }
}

/// The fallback without a toolchain (spec §15: never land an unverified
/// merge). Passes a clean change (or one whose overlaps an adapter merge
/// resolved, ADR 0013) and fails any other, so the lander parks it.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailClosedVerifier;

impl Verifier for FailClosedVerifier {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        let verdict = fail_closed(&request.report);
        Box::pin(async move { verdict })
    }
}

/// Builds a language's verifier for one checkout (planning needs the
/// checkout's build metadata, for example `cargo metadata`).
pub trait VerifierFactory: Send + Sync {
    /// The toolchain evidence is produced with; `None` when none is
    /// available (the engine then fails closed).
    fn toolchain(&self) -> Option<Toolchain>;

    /// The verifier for `checkout`, selecting tests with `coverage` and
    /// per-test freshness under `drift`.
    fn build(
        &self,
        checkout: &Checkout,
        coverage: Option<Arc<CoverageRecord>>,
        drift: Option<Arc<Drift>>,
        cancel: &Cancel,
    ) -> hord_verify::Result<Built>;
}

/// What a [`VerifierFactory`] builds for one checkout.
pub struct Built {
    /// Plans and runs every requirement.
    pub verifier: Box<dyn hord_verify::Verifier>,
    /// Runs the test requirements under coverage instead (ADR 0022: every
    /// verification run is instrumented and refreshes the tests it ran);
    /// `None`: tests run through `verifier`, and coverage is not refreshed.
    pub instrumented: Option<Box<dyn InstrumentedTests>>,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built")
            .field("instrumented", &self.instrumented.is_some())
            .finish_non_exhaustive()
    }
}

/// Test requirements run one test per process under coverage (ADR 0022,
/// amendments after the 50-commit measurement).
pub trait InstrumentedTests: Send + Sync {
    /// Whether the tests for `impact` would run in full (a selection
    /// fallback, or `full`): the evidence then has no scope.
    fn runs_everything(&self, impact: &ImpactSet, policy: &VerifyPolicy, full: bool) -> bool;

    /// Run the selected tests (every test with `full`) under coverage in
    /// `checkout`, with `defs` the checkout's definitions; logs go to
    /// `index`. The evidence is returned, not indexed.
    fn run(
        &self,
        checkout: &Checkout,
        impact: &ImpactSet,
        policy: &VerifyPolicy,
        full: bool,
        defs: &DefinitionIndex,
        index: &dyn EvidenceIndex,
    ) -> hord_verify::Result<InstrumentedRun>;

    /// Who the coverage evidence is produced by.
    fn actor(&self) -> hord_core::Actor;
}

/// Cargo (`hord-verify-rust`, ADR 0022): `check`, `test:selected`,
/// `test:full`, `lint`, and `bench` on the affected packages, tests run
/// under coverage. The toolchain is detected once, on first use.
#[derive(Debug)]
pub struct RustFactory {
    probe: PathBuf,
    toolchain: OnceLock<Option<Toolchain>>,
    /// How commands run (target directory, timeouts).
    pub runner: hord_verify_rust::CargoRunner,
    /// Tests run at once under coverage.
    pub coverage_jobs: usize,
    /// Kill a single test under coverage after this long (it fails).
    pub test_timeout: std::time::Duration,
    /// Quarantined tests, never run.
    pub quarantine: BTreeSet<hord_verify::TestRef>,
}

impl RustFactory {
    /// Detect the toolchain in `probe` (a repository root) when first needed.
    #[must_use]
    pub fn new(probe: PathBuf) -> Self {
        Self {
            probe,
            toolchain: OnceLock::new(),
            runner: hord_verify_rust::CargoRunner::default(),
            coverage_jobs: std::thread::available_parallelism().map_or(4, usize::from),
            test_timeout: std::time::Duration::from_secs(600),
            quarantine: BTreeSet::new(),
        }
    }
}

impl VerifierFactory for RustFactory {
    fn toolchain(&self) -> Option<Toolchain> {
        self.toolchain
            .get_or_init(|| hord_verify_rust::detect_toolchain(&self.probe).ok())
            .clone()
    }

    fn build(
        &self,
        checkout: &Checkout,
        coverage: Option<Arc<CoverageRecord>>,
        drift: Option<Arc<Drift>>,
        cancel: &Cancel,
    ) -> hord_verify::Result<Built> {
        let toolchain = self.toolchain().ok_or_else(|| hord_verify::Error::Tool {
            tool: "rustc".into(),
            message: "no toolchain".into(),
        })?;
        let workspace = hord_verify_rust::CargoWorkspace::load(&checkout.root)?;
        let mut verifier = hord_verify_rust::RustVerifier::new(toolchain, workspace)?
            .with_coverage(coverage)
            .with_drift(drift);
        verifier.runner = self.runner.clone();
        verifier.runner.cancel = cancel.clone();
        verifier.quarantine = self.quarantine.clone();
        // Instrumented builds use other flags: their own target directory,
        // kept in the slot (`target/` survives a slot reset) or beside the
        // runner's shared one.
        let target_dir = match &self.runner.target_dir {
            Some(dir) => dir.join("hord-coverage"),
            None => checkout.root.join("target").join("hord-coverage"),
        };
        let instrumented = RustInstrumented {
            verifier: verifier.clone(),
            options: hord_verify_rust::CoverageOptions {
                packages: None,
                target_dir,
                jobs: self.coverage_jobs.max(1),
                test_timeout: self.test_timeout,
                skip: BTreeSet::new(),
                only: None,
                lines_of_interest: BTreeMap::new(),
                cancel: cancel.clone(),
            },
        };
        Ok(Built {
            verifier: Box::new(verifier),
            instrumented: Some(Box::new(instrumented)),
        })
    }
}

/// [`InstrumentedTests`] with `RustVerifier::run_instrumented`.
struct RustInstrumented {
    verifier: hord_verify_rust::RustVerifier,
    options: hord_verify_rust::CoverageOptions,
}

impl RustInstrumented {
    /// The verifier for `full`: without a coverage record, selection runs
    /// everything.
    fn for_run(&self, full: bool) -> std::borrow::Cow<'_, hord_verify_rust::RustVerifier> {
        if full {
            std::borrow::Cow::Owned(self.verifier.clone().with_coverage(None))
        } else {
            std::borrow::Cow::Borrowed(&self.verifier)
        }
    }
}

impl InstrumentedTests for RustInstrumented {
    fn runs_everything(&self, impact: &ImpactSet, policy: &VerifyPolicy, full: bool) -> bool {
        full || self.verifier.selection(impact, policy).full
    }

    fn run(
        &self,
        checkout: &Checkout,
        impact: &ImpactSet,
        policy: &VerifyPolicy,
        full: bool,
        defs: &DefinitionIndex,
        index: &dyn EvidenceIndex,
    ) -> hord_verify::Result<InstrumentedRun> {
        self.for_run(full)
            .run_instrumented(checkout, impact, policy, defs, &self.options, index)
    }

    fn actor(&self) -> hord_core::Actor {
        self.verifier.runner.actor.clone()
    }
}

/// Runs a language's `hord_verify::Verifier` for the lander (ADR 0025):
/// finds the newest coverage record, computes the impact set, plans with
/// reuse (evidence already indexed for the same key is not re-run), runs
/// the rest in a checkout of the candidate, and indexes the new evidence.
///
/// Test requirements run under coverage when the factory can
/// ([`Built::instrumented`], ADR 0022 amendments): the run's per-test
/// records are merged into the newest record and stored as coverage
/// evidence on the candidate's snapshot, so the next landing selects from
/// fresh records.
///
/// With no requirements, or no toolchain, it fails closed like
/// [`FailClosedVerifier`]; so does a pass that verified nothing on a
/// change with overlaps.
#[derive(Clone)]
pub struct EngineVerifier {
    factory: Arc<dyn VerifierFactory>,
}

impl std::fmt::Debug for EngineVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineVerifier").finish_non_exhaustive()
    }
}

impl EngineVerifier {
    /// Verify with `factory`'s verifiers.
    #[must_use]
    pub fn new(factory: Arc<dyn VerifierFactory>) -> Self {
        Self { factory }
    }

    /// Cargo, with the toolchain found from `repo_root`.
    #[must_use]
    pub fn rust(repo_root: PathBuf) -> Self {
        Self::new(Arc::new(RustFactory::new(repo_root)))
    }
}

impl Verifier for EngineVerifier {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        let factory = Arc::clone(&self.factory);
        Box::pin(async move {
            // Tracked, so closing the repository waits for it: a cancel
            // kills its commands, and it returns at once.
            let tasks = request.context.tasks();
            let run = move || engine_verify(factory.as_ref(), &request);
            let task = match tasks {
                Some(tasks) => tasks.spawn_blocking(run),
                None => tokio::task::spawn_blocking(run),
            };
            task.await.unwrap_or_else(|err| Verdict::Fail {
                evidence: Vec::new(),
                reason: format!("verification task failed: {err}"),
            })
        })
    }
}

fn engine_verify(factory: &dyn VerifierFactory, request: &VerifyRequest) -> Verdict {
    if request.policy.require.is_empty() && !request.plan_only {
        return fail_closed(&request.report);
    }
    let Some(toolchain) = factory.toolchain() else {
        return fail_closed(&request.report);
    };
    match engine_run(factory, &toolchain, request) {
        Ok(verdict) if verdict.passed() && verdict.evidence().is_empty() && !request.plan_only => {
            fail_closed(&request.report)
        }
        Ok(verdict) => verdict,
        Err(err) => Verdict::Fail {
            evidence: Vec::new(),
            reason: format!("verification could not run: {err}"),
        },
    }
}

/// The test requirements [`InstrumentedTests`] answers.
const TEST_REQUIREMENTS: [&str; 2] = [
    hord_verify_rust::requirement::TEST_SELECTED,
    hord_verify_rust::requirement::TEST_FULL,
];

/// `Check::program` of instrumented test evidence.
const COVERAGE_PROGRAM: &str = "hord-coverage";

fn engine_run(
    factory: &dyn VerifierFactory,
    toolchain: &Toolchain,
    request: &VerifyRequest,
) -> hord_verify::Result<Verdict> {
    let context = &request.context;
    let index = context.index();
    let toolchain_id = toolchain.id()?;
    let coverage = hord_verify::find_coverage(index, context.history(), toolchain_id)?;
    let impact = context.impact(request.policy.bound)?;
    let checkout = context.checkout()?;
    let drift = context.drift()?;
    let built = factory.build(
        &checkout,
        coverage.map(|(_, record)| Arc::new(record)),
        drift,
        &context.cancel(),
    )?;
    let verifier = built.verifier.as_ref();
    let tests_required = TEST_REQUIREMENTS.iter().any(|r| request.policy.requires(r));
    let instrumented = match built.instrumented {
        Some(instrumented) if tests_required => instrumented,
        _ => {
            let plan = hord_verify::plan_with_reuse(
                verifier,
                index,
                checkout.snapshot,
                &impact,
                &request.policy,
            )?;
            context.planned(&plan);
            if request.plan_only {
                return Ok(Verdict::Pass {
                    evidence: plan.reused.iter().map(|r| r.evidence).collect(),
                });
            }
            return hord_verify::verify(verifier, index, &checkout, &impact, &request.policy);
        }
    };

    // Tests under coverage; everything else as planned.
    let full = request
        .policy
        .requires(hord_verify_rust::requirement::TEST_FULL);
    let scope =
        (!instrumented.runs_everything(&impact, &request.policy, full)).then(|| impact.node_set());
    let reused_tests = reusable_tests(index, checkout.snapshot, toolchain_id, &scope, &impact)?;
    let mut plan =
        hord_verify::plan_with_reuse(verifier, index, checkout.snapshot, &impact, &request.policy)?;
    if !reused_tests.is_empty() {
        plan.checks
            .retain(|c| !TEST_REQUIREMENTS.contains(&c.requirement.as_str()));
        plan.notes.push(format!(
            "tests: {} instrumented evidence reused",
            reused_tests.len()
        ));
    } else {
        plan.notes
            .push("tests run one per process under coverage (ADR 0022)".into());
    }
    context.planned(&plan);
    let rest = VerifyPolicy {
        require: request
            .policy
            .require
            .iter()
            .filter(|r| !TEST_REQUIREMENTS.contains(&r.as_str()))
            .cloned()
            .collect(),
        ..request.policy.clone()
    };
    if request.plan_only {
        let mut evidence: Vec<_> = plan
            .reused
            .iter()
            .filter(|r| !TEST_REQUIREMENTS.contains(&r.check.requirement.as_str()))
            .map(|r| r.evidence)
            .collect();
        evidence.extend(reused_tests.iter().map(|(id, _)| *id));
        return Ok(Verdict::Pass { evidence });
    }

    index.put_raw(&hord_encoding::encode(toolchain)?)?;
    let (mut evidence, mut failures) = if rest.require.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        match hord_verify::verify(verifier, index, &checkout, &impact, &rest)? {
            Verdict::Pass { evidence } => (evidence, Vec::new()),
            Verdict::Fail { evidence, reason } => (evidence, vec![reason]),
        }
    };
    if reused_tests.is_empty() {
        let defs = context.definitions()?;
        let run = instrumented.run(&checkout, &impact, &request.policy, full, &defs, index)?;
        evidence.extend(index.put_evidence_batch(&run.evidence)?);
        failures.extend(run.evidence.iter().filter_map(|ev| match &ev.result {
            hord_core::EvidenceResult::Fail { summary } => {
                Some(format!("{}: {summary}", ev.command))
            }
            _ => None,
        }));
        store_coverage(
            index,
            context.history(),
            toolchain_id,
            run.coverage,
            instrumented.actor(),
        )?;
    } else {
        for (id, ev) in &reused_tests {
            evidence.push(*id);
            if let hord_core::EvidenceResult::Fail { summary } = &ev.result {
                failures.push(format!("{} (reused): {summary}", ev.command));
            }
        }
    }
    Ok(if failures.is_empty() {
        Verdict::Pass { evidence }
    } else {
        Verdict::Fail {
            evidence,
            reason: failures.join("\n"),
        }
    })
}

/// Test evidence already indexed for `snapshot` by an instrumented run
/// with this toolchain and `scope` (its doctests, scoped to the impact
/// set, with it); empty when there is none, and the tests must run.
fn reusable_tests(
    index: &dyn EvidenceIndex,
    snapshot: SnapshotId,
    toolchain: hord_core::ObjectId,
    scope: &Option<BTreeSet<NodeId>>,
    impact: &ImpactSet,
) -> hord_verify::Result<Vec<(hord_core::ObjectId, Evidence)>> {
    let mut tests = Vec::new();
    let mut instrumented = false;
    let docs = Some(impact.node_set());
    for id in index.evidence_at(snapshot)? {
        let ev = hord_verify::get_evidence(index, id)?;
        if ev.kind != hord_core::EvidenceKind::Test || ev.toolchain != toolchain {
            continue;
        }
        let coverage = ev
            .command
            .split_whitespace()
            .next()
            .is_some_and(|program| program == COVERAGE_PROGRAM);
        if coverage && ev.scope == *scope {
            instrumented = true;
            tests.push((id, ev));
        } else if !coverage && ev.scope == docs {
            tests.push((id, ev));
        }
    }
    if !instrumented {
        tests.clear();
    }
    Ok(tests)
}

/// Merge `run`'s per-test records into the newest record (each test keeps
/// its last runs, `CoverageRecord::merge`) and store the result as coverage
/// evidence on the run's snapshot (ADR 0022: every verification run
/// refreshes the tests it ran). The newest record is looked up again: a
/// candidate ahead in the window may have stored one since selection.
fn store_coverage(
    index: &dyn EvidenceIndex,
    history: Vec<SnapshotId>,
    toolchain: hord_core::ObjectId,
    mut run: hord_verify_rust::CoverageRun,
    actor: hord_core::Actor,
) -> hord_verify::Result<hord_core::ObjectId> {
    if let Some((_, ledger)) = hord_verify::find_coverage(index, history, toolchain)? {
        run.record = ledger.merge(&run.record);
    }
    hord_verify_rust::coverage::record_evidence(&run, index, actor)
}

// ------------------------------------------------------------ policy

/// Where the policy judging a change came from (ADR 0026).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// `.hord-policy.toml` in the landing base.
    Head,
    /// The base has no policy file: `[land]` defaults only.
    Default,
}

impl PolicySource {
    /// `head` or `default`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Default => "default",
        }
    }
}

/// Parse the bytes of a `.hord-policy.toml`; the error is the message a
/// rejection or a parked change reports, with the file's line and column.
pub(crate) fn parse_policy_file(bytes: &[u8]) -> std::result::Result<CompiledPolicy, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{POLICY_PATH} is not UTF-8"))?;
    hord_policy::parse(text).map_err(|err| format!("{POLICY_PATH}:{err}"))
}

/// [`POLICY_PATH`] as a repository path.
fn policy_path() -> Result<RepoPath> {
    POLICY_PATH
        .parse()
        .map_err(|_| Error::InvalidPath(POLICY_PATH.into()))
}

/// Head's policy for a change, as read at its landing base: the policy and
/// where it came from, or why the file does not parse.
pub(crate) type HeadPolicy = std::result::Result<(Arc<CompiledPolicy>, PolicySource), String>;

impl Inner {
    /// The policy in `snapshot` (ADR 0026). An unparseable file is an
    /// error message: the change is judged by nothing weaker.
    pub(crate) fn policy_at(&self, snapshot: SnapshotId) -> Result<HeadPolicy> {
        let Some(blob) = self.blob_id(snapshot, &policy_path()?)? else {
            return Ok(Ok((
                Arc::new(CompiledPolicy::default()),
                PolicySource::Default,
            )));
        };
        if let Some(policy) = lock(&self.policies).get(&blob) {
            return Ok(Ok((Arc::clone(policy), PolicySource::Head)));
        }
        let policy = match parse_policy_file(self.blob_bytes(blob)?.as_slice()) {
            Ok(policy) => Arc::new(policy),
            Err(reason) => return Ok(Err(reason)),
        };
        lock(&self.policies).insert(blob, Arc::clone(&policy));
        Ok(Ok((policy, PolicySource::Head)))
    }

    /// The policy facts of `record` without evidence, and the requirements
    /// `policy` applies to them (its verdict on those facts): what
    /// verification must produce. No facts when the policy can require
    /// nothing.
    pub(crate) fn requirements(
        &self,
        policy: &CompiledPolicy,
        record: &ChangeRecord,
    ) -> Result<(Option<Facts>, BTreeSet<String>)> {
        let p = policy.policy();
        if p.land.require.is_empty() && p.land.max_write_set.is_none() && p.rules.is_empty() {
            return Ok((None, BTreeSet::new()));
        }
        let facts = self.policy_facts(record, false)?;
        let require = match policy.evaluate(&facts) {
            Decision::Allow => BTreeSet::new(),
            Decision::Deny { reasons } => {
                reasons.iter().map(|r| r.requirement.to_string()).collect()
            }
        };
        Ok((Some(facts), require))
    }

    /// ADR 0026 amendment: a change that writes a `.hord-policy.toml` that
    /// does not parse never lands, because once it is head's policy it
    /// would park every change, including the one that fixes it. `Ok` when
    /// the change leaves the file alone or it parses; otherwise the parse
    /// error, with its line and column.
    pub(crate) fn check_policy_file(
        &self,
        base: SnapshotId,
        result: SnapshotId,
    ) -> Result<std::result::Result<(), String>> {
        let path = policy_path()?;
        let after = self.blob_id(result, &path)?;
        if after.is_none() || after == self.blob_id(base, &path)? {
            return Ok(Ok(()));
        }
        Ok(self.policy_at(result)?.map(|_| ()))
    }

    /// Evidence indexed for `snapshot` as policy facts (ADR 0025).
    pub(crate) fn evidence_facts(&self, snapshot: SnapshotId) -> Result<Vec<EvidenceFact>> {
        let mut out = Vec::new();
        for id in self.store.evidence_at(snapshot)? {
            let evidence: Evidence = self.get_object(id)?;
            out.extend(EvidenceFact::of(&evidence));
        }
        Ok(out)
    }

    /// Evidence facts for judging the candidate `landed` with its rebase
    /// `report`: everything indexed for its result, plus, for a rebased
    /// record whose rebase merged nothing (no `merge`, no `adapter_merged`),
    /// the `review:*` evidence indexed for the submitted record's result
    /// (ADR 0031). Machine evidence counts only for the exact snapshot.
    pub(crate) fn candidate_evidence_facts(
        &self,
        landed: &ChangeRecord,
        report: &ConflictReport,
    ) -> Result<Vec<EvidenceFact>> {
        let mut out = self.evidence_facts(landed.result)?;
        if let Some(submitted) = landed.rebased_from
            && report.merge.is_empty()
            && report.adapter_merged.is_empty()
        {
            let submitted = self.change_record(submitted)?;
            if submitted.result != landed.result {
                out.extend(
                    self.evidence_facts(submitted.result)?
                        .into_iter()
                        .filter(|fact| fact.tag.kind() == "review"),
                );
            }
        }
        Ok(out)
    }

    /// Policy facts for `record` (ADR 0026): its actor and write-set size,
    /// the files it changes, the write-set definitions in them (read from
    /// the result, or the base when deleted) with the adapter's kinds and
    /// visibility, and, with `evidence`, the evidence indexed for its
    /// result.
    pub(crate) fn policy_facts(&self, record: &ChangeRecord, evidence: bool) -> Result<Facts> {
        let paths: Vec<RepoPath> = self
            .changed_paths(record.base, record.result)?
            .into_iter()
            .map(|d| d.path)
            .collect();
        let mut definitions_by_node: BTreeMap<NodeId, TouchedDefinition> = BTreeMap::new();
        for path in &paths {
            for snapshot in [record.result, record.base] {
                let Some(view) = self.file_view(snapshot, path)? else {
                    continue;
                };
                let Some(parsed) = view.parsed else {
                    continue;
                };
                let Some(adapter) = self.adapter_for(path, parsed.lang) else {
                    continue;
                };
                let defs: Vec<_> = definitions(adapter, path, &parsed.tree)
                    .into_iter()
                    .filter(|d| {
                        record.write_set.contains(&d.node)
                            && !definitions_by_node.contains_key(&d.node)
                    })
                    .collect();
                if defs.is_empty() {
                    continue;
                }
                let facts = definition_facts(adapter, view.bytes.as_slice(), &defs);
                for (def, facts) in defs.into_iter().zip(facts) {
                    let mut kinds = facts.inner_kinds;
                    kinds.insert(def.kind.as_str().to_owned());
                    definitions_by_node.insert(
                        def.node,
                        TouchedDefinition {
                            node: def.node,
                            path: def.path,
                            kinds,
                            visibility: facts.visibility,
                        },
                    );
                }
            }
        }
        Ok(Facts {
            actor: hord_policy::ActorClass::from(&record.provenance.actor),
            write_set_len: record.write_set.len(),
            definitions: definitions_by_node.into_values().collect(),
            paths,
            evidence: if evidence {
                self.evidence_facts(record.result)?
            } else {
                Vec::new()
            },
        })
    }

    /// Verification facts (`hord_verify::ChangeFacts`, ADR 0022): every
    /// file changed between `from` and `to`, with Rust files diffed by
    /// definition.
    pub(crate) fn verify_facts(
        &self,
        from: SnapshotId,
        to: SnapshotId,
    ) -> Result<hord_verify::ChangeFacts> {
        let mut facts = hord_verify::ChangeFacts::default();
        for delta in self.changed_paths(from, to)? {
            let path = delta.path;
            let side = |snapshot| self.rust_definitions(snapshot, &path);
            let rust = path.components().last().is_some_and(|n| n.ends_with(".rs"));
            let (older, newer) = if rust {
                (side(from)?, side(to)?)
            } else {
                (None, None)
            };
            if older.is_none() && newer.is_none() {
                facts.paths.insert(path);
                continue;
            }
            fn version(
                v: &Option<(Vec<u8>, Vec<hord_verify::Definition>)>,
            ) -> Option<hord_verify::FileVersion<'_>> {
                v.as_ref()
                    .map(|(bytes, defs)| hord_verify::FileVersion { bytes, defs })
            }
            hord_verify_rust::diff_rust_file(&mut facts, &path, version(&older), version(&newer));
        }
        Ok(facts)
    }

    /// The impact set of `record` in its result snapshot under `bound`,
    /// over its own facts (base to result). `attributable`: only the writes
    /// coverage cannot attribute are seeded (verification, ADR 0022
    /// amendments, [`hord_verify::impact_set_attributable`]); otherwise
    /// every write ([`crate::Repo::impact`]).
    fn impact_set(
        &self,
        record: &ChangeRecord,
        bound: ImpactBound,
        attributable: bool,
    ) -> Result<hord_verify::Result<ImpactSet>> {
        let facts = self.verify_facts(record.base, record.result)?;
        let graph = crate::graph::SnapshotGraph::new(self, record.result)?;
        let impact = if attributable {
            hord_verify::impact_set_attributable
        } else {
            hord_verify::impact_set
        };
        Ok(impact(&graph, &record.write_set, bound, facts))
    }

    /// A Rust file of `snapshot` with its definitions; `None` when absent,
    /// not parsed as Rust, or no adapter claims it.
    fn rust_definitions(
        &self,
        snapshot: SnapshotId,
        path: &RepoPath,
    ) -> Result<Option<(Vec<u8>, Vec<hord_verify::Definition>)>> {
        let Some(view) = self.file_view(snapshot, path)? else {
            return Ok(None);
        };
        let Some(parsed) = &view.parsed else {
            return Ok(None);
        };
        if parsed.lang.as_str() != hord_lang_rust::LANG {
            return Ok(None);
        }
        let Some(adapter) = self.adapter_for(path, parsed.lang) else {
            return Ok(None);
        };
        let defs = definitions(adapter, path, &parsed.tree)
            .into_iter()
            .map(|d| hord_verify::Definition {
                node: d.node,
                path: d.path,
                kind: d.kind,
                name: d.name,
                span: d.span,
                parent: d.parent,
            })
            .collect();
        Ok(Some((view.bytes.as_slice().to_vec(), defs)))
    }

    /// The definitions of every Rust file of `snapshot`, to attribute
    /// coverage to. The last index built is kept and brought to the next
    /// snapshot by the files that differ.
    pub(crate) fn definition_index(&self, snapshot: SnapshotId) -> Result<Arc<DefinitionIndex>> {
        let cached = lock(&self.definition_index).clone();
        let index = match cached {
            Some((at, index)) if at == snapshot => return Ok(index),
            Some((at, index)) => {
                let mut index = index.as_ref().clone();
                for delta in self.changed_paths(at, snapshot)? {
                    match self.rust_definitions(snapshot, &delta.path)? {
                        Some((bytes, defs)) => index.set_file(&delta.path, &bytes, &defs),
                        None => index.remove_file(&delta.path),
                    }
                }
                index
            }
            None => {
                let mut index = DefinitionIndex::new();
                for (path, _) in self.list_files(snapshot)? {
                    if !path.components().last().is_some_and(|n| n.ends_with(".rs")) {
                        continue;
                    }
                    if let Some((bytes, defs)) = self.rust_definitions(snapshot, &path)? {
                        index.add_file(&path, &bytes, &defs);
                    }
                }
                index
            }
        };
        let index = Arc::new(index);
        *lock(&self.definition_index) = Some((snapshot, Arc::clone(&index)));
        Ok(index)
    }

    /// The landed changes up to `base`, then `pending` (changes ahead in
    /// the speculative window, not landed yet), each with its write set
    /// (ADR 0022: per-test coverage freshness). The chain holds at most
    /// [`MAX_DRIFT_STEPS`] landings; a test whose coverage is older is
    /// treated as stale.
    pub(crate) fn drift(
        &self,
        base: SnapshotId,
        pending: &[(SnapshotId, BTreeSet<NodeId>)],
    ) -> Result<Drift> {
        let chain = self.landed_chain()?;
        let mut steps: &[LandedStep] = &chain;
        // A base behind head (a workspace's): the chain up to it.
        if let Some(at) = steps.iter().rposition(|(_, result, _)| *result == base) {
            steps = &steps[..=at];
        } else if let Some(at) = steps.iter().position(|(from, _, _)| *from == base) {
            steps = &steps[..at];
        }
        let mut drift = Drift::new(steps.first().map_or(base, |(from, _, _)| *from));
        for (_, result, written) in steps {
            drift.push(*result, written.as_ref().clone());
        }
        for (result, written) in pending {
            drift.push(*result, written.clone());
        }
        Ok(drift)
    }

    /// `(base, result, write set)` of the latest landings, oldest first,
    /// brought up to date with the landing log.
    fn landed_chain(&self) -> Result<Vec<LandedStep>> {
        let mut chain = lock(&self.landed_chain);
        let len = self.store.log_len()?;
        if len < chain.seen {
            *chain = LandedChain::default();
        }
        let start = chain.seen.max(len.saturating_sub(MAX_DRIFT_STEPS));
        for change in self.store.log_since(start)? {
            let record = self.change_record(change)?;
            chain.steps.push_back((
                record.base,
                record.result,
                Arc::new(record.write_set.iter().copied().collect()),
            ));
            if chain.steps.len() > MAX_DRIFT_STEPS {
                chain.steps.pop_front();
            }
        }
        chain.seen = len;
        Ok(chain.steps.iter().cloned().collect())
    }

    /// Landed results, newest first, then `first` ahead of them: where to
    /// look for a coverage record (at most `limit`).
    pub(crate) fn snapshot_history(
        &self,
        first: SnapshotId,
        limit: usize,
    ) -> Result<Vec<SnapshotId>> {
        let mut out = vec![first];
        for change in self.store.log()?.iter().rev() {
            if out.len() >= limit {
                break;
            }
            if let Ok(record) = self.change_record(*change)
                && !out.contains(&record.result)
            {
                out.push(record.result);
            }
        }
        Ok(out)
    }
}

/// The adapter's facts for `defs` of a file with `source` (ADR 0026).
///
/// An item (`*_item`) is parsed on its own text, which carries everything
/// its facts describe (its visibility, the kinds inside it), instead of the
/// whole file for each change: a large file costs one small parse per
/// touched item. A definition that yields no facts that way (not an item,
/// or its text does not parse alone) falls back to the whole file, so a
/// fact is never lost.
fn definition_facts(
    adapter: &dyn hord_lang::LangAdapter,
    source: &[u8],
    defs: &[crate::DefinitionInfo],
) -> Vec<hord_lang::DefinitionFacts> {
    let mut out: Vec<Option<hord_lang::DefinitionFacts>> = defs
        .iter()
        .map(|def| {
            if !def.kind.as_str().ends_with("_item") {
                return None;
            }
            let text = source.get(def.span.clone())?;
            let whole = 0..text.len();
            let facts = adapter
                .definition_facts(text, std::slice::from_ref(&whole))
                .pop()?;
            (!facts.inner_kinds.is_empty()).then_some(facts)
        })
        .collect();
    let rest: Vec<usize> = (0..defs.len()).filter(|i| out[*i].is_none()).collect();
    if !rest.is_empty() {
        let spans: Vec<_> = rest.iter().map(|i| defs[*i].span.clone()).collect();
        for (i, facts) in rest
            .into_iter()
            .zip(adapter.definition_facts(source, &spans))
        {
            out[i] = Some(facts);
        }
    }
    out.into_iter().map(Option::unwrap_or_default).collect()
}

/// Landings a [`Drift`] chain reaches back (ADR 0022). Coverage taken
/// before them selects its test again.
pub(crate) const MAX_DRIFT_STEPS: usize = 1024;

/// One landing: `(base, result, write set)`.
pub(crate) type LandedStep = (SnapshotId, SnapshotId, Arc<BTreeSet<NodeId>>);

/// The latest landings' write sets, cached from the landing log.
#[derive(Debug, Default)]
pub(crate) struct LandedChain {
    /// Log entries read.
    seen: usize,
    /// `(base, result, write set)`, oldest first.
    steps: std::collections::VecDeque<LandedStep>,
}

// ------------------------------------------------------------ checkouts

/// Verification checkout directories under `.hord/verify/`, reused across
/// candidates: a slot is brought to a new snapshot by writing only the
/// files that differ, so build caches (cargo's `target/`, which lives in
/// the slot) stay warm.
#[derive(Debug, Default)]
pub(crate) struct Slots {
    free: Mutex<Vec<Slot>>,
    created: Mutex<usize>,
}

#[derive(Debug)]
pub(crate) struct Slot {
    dir: PathBuf,
    snapshot: Option<SnapshotId>,
}

/// A slot in use; returned to the pool on drop.
pub(crate) struct SlotGuard {
    inner: Arc<Inner>,
    slot: Option<Slot>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            lock(&self.inner.slots.free).push(slot);
        }
    }
}

impl Inner {
    /// A slot holding `snapshot`.
    pub(crate) fn checkout_slot(self: &Arc<Self>, snapshot: SnapshotId) -> Result<SlotGuard> {
        let slot = lock(&self.slots.free).pop();
        let mut slot = match slot {
            Some(slot) => slot,
            None => {
                let n = {
                    let mut created = lock(&self.slots.created);
                    *created += 1;
                    *created - 1
                };
                Slot {
                    dir: self
                        .store
                        .hord_dir()
                        .join("verify")
                        .join(format!("slot-{n}")),
                    snapshot: None,
                }
            }
        };
        if slot.snapshot != Some(snapshot) {
            match slot.snapshot {
                Some(old) => {
                    for delta in self.changed_paths(old, snapshot)? {
                        let target = fs_path(&slot.dir, &delta.path);
                        match self.file_bytes(snapshot, &delta.path)? {
                            Some(bytes) => {
                                if let Some(parent) = target.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                std::fs::write(&target, bytes.as_slice())?;
                            }
                            None => match std::fs::remove_file(&target) {
                                Ok(()) => {}
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                                Err(err) => return Err(err.into()),
                            },
                        }
                    }
                }
                None => {
                    // Unknown contents: start over, keeping `target/`.
                    if slot.dir.is_dir() {
                        for entry in std::fs::read_dir(&slot.dir)? {
                            let entry = entry?;
                            if entry.file_name() == "target" {
                                continue;
                            }
                            if entry.file_type()?.is_dir() {
                                std::fs::remove_dir_all(entry.path())?;
                            } else {
                                std::fs::remove_file(entry.path())?;
                            }
                        }
                    }
                    std::fs::create_dir_all(&slot.dir)?;
                    self.checkout(snapshot, &slot.dir)?;
                }
            }
            slot.snapshot = Some(snapshot);
        }
        Ok(SlotGuard {
            inner: Arc::clone(self),
            slot: Some(slot),
        })
    }
}

impl SlotGuard {
    fn checkout(&self) -> Option<Checkout> {
        let slot = self.slot.as_ref()?;
        Some(Checkout {
            root: slot.dir.clone(),
            snapshot: slot.snapshot?,
        })
    }
}

// ------------------------------------------------------------ contexts

/// [`VerifyContext`] for a candidate: a record whose result is checked out
/// in a slot.
pub(crate) struct CandidateContext {
    inner: Arc<Inner>,
    change: ChangeId,
    record: Arc<ChangeRecord>,
    /// Candidates ahead in the speculative window, oldest first: the
    /// snapshot each will land as and its write set (the drift between
    /// head and this candidate's base).
    pending: Vec<(SnapshotId, BTreeSet<NodeId>)>,
    /// Emit `Verifying` events (the lander) or record the plan.
    emit: bool,
    plan: Mutex<Option<VerifyPlan>>,
    slot: Mutex<Option<SlotGuard>>,
}

fn verify_err(err: Error) -> hord_verify::Error {
    match err {
        Error::Io(io) => hord_verify::Error::Io(io),
        other => hord_verify::Error::Graph(other.to_string()),
    }
}

impl VerifyContext for CandidateContext {
    fn cancel(&self) -> Cancel {
        self.inner.verify_cancel.clone()
    }

    fn tasks(&self) -> Option<TaskTracker> {
        Some(self.inner.tasks.clone())
    }

    fn impact(&self, bound: ImpactBound) -> hord_verify::Result<ImpactSet> {
        self.inner
            .impact_set(&self.record, bound, true)
            .map_err(verify_err)?
    }

    fn drift(&self) -> hord_verify::Result<Option<Arc<Drift>>> {
        let drift = self
            .inner
            .drift(self.record.base, &self.pending)
            .map_err(verify_err)?;
        Ok(Some(Arc::new(drift)))
    }

    fn definitions(&self) -> hord_verify::Result<Arc<DefinitionIndex>> {
        self.inner
            .definition_index(self.record.result)
            .map_err(verify_err)
    }

    fn checkout(&self) -> hord_verify::Result<Checkout> {
        let mut slot = lock(&self.slot);
        if slot.is_none() {
            *slot = Some(
                self.inner
                    .checkout_slot(self.record.result)
                    .map_err(verify_err)?,
            );
        }
        slot.as_ref()
            .and_then(SlotGuard::checkout)
            .ok_or_else(|| hord_verify::Error::Graph("checkout slot is empty".into()))
    }

    fn index(&self) -> &dyn EvidenceIndex {
        &self.inner.store
    }

    /// The landing base, then landed results newest first.
    fn history(&self) -> Vec<SnapshotId> {
        self.inner
            .snapshot_history(self.record.base, 64)
            .unwrap_or_else(|_| vec![self.record.base])
    }

    fn planned(&self, plan: &VerifyPlan) {
        *lock(&self.plan) = Some(plan.clone());
        if self.emit {
            let summary = hord_api::proto::VerifyPlanSummary {
                commands: plan
                    .checks
                    .iter()
                    .map(hord_verify::Check::command)
                    .collect(),
                selected_tests: u32::try_from(
                    plan.checks
                        .iter()
                        .filter(|c| c.requirement == hord_verify_rust::requirement::TEST_SELECTED)
                        .count(),
                )
                .unwrap_or(u32::MAX),
                reused: u32::try_from(plan.reused.len()).unwrap_or(u32::MAX),
            };
            let _ = self
                .inner
                .emit(vec![hord_api::proto::event::Kind::Verifying(
                    hord_api::proto::Verifying {
                        change: hord_api::wire::id(self.change),
                        plan: Some(summary),
                    },
                )]);
        }
    }
}

impl CandidateContext {
    /// A context for `record`, landing as `change` on top of `pending`
    /// (the candidates ahead in the speculative window), with no checkout
    /// yet.
    pub(crate) fn new(
        inner: Arc<Inner>,
        change: ChangeId,
        record: Arc<ChangeRecord>,
        pending: Vec<(SnapshotId, BTreeSet<NodeId>)>,
        emit: bool,
    ) -> Self {
        Self {
            inner,
            change,
            record,
            pending,
            emit,
            plan: Mutex::new(None),
            slot: Mutex::new(None),
        }
    }

    /// The plan the verifier reported, if any.
    pub(crate) fn take_plan(&self) -> Option<VerifyPlan> {
        lock(&self.plan).take()
    }

    /// Release the checkout slot.
    pub(crate) fn release(&self) {
        lock(&self.slot).take();
    }
}

// ------------------------------------------------------------ public API

/// What `hord verify` found for a workspace's current proposal.
#[derive(Clone, Debug)]
pub struct WorkspaceVerification {
    /// The proposal's result snapshot, which the evidence is for.
    pub snapshot: SnapshotId,
    /// Where head's policy came from.
    pub policy_source: PolicySource,
    /// The requirements head's policy applies to the proposal.
    pub requirements: BTreeSet<String>,
    /// The plan, with reuse applied, if the verifier planned.
    pub plan: Option<VerifyPlan>,
    /// The outcome; with `plan_only`, a pass listing the reused evidence.
    pub verdict: Verdict,
}

impl crate::Repo {
    /// Head's policy (ADR 0026) and where it came from. An unparseable
    /// file is [`Error::Policy`].
    pub async fn head_policy(&self) -> Result<(CompiledPolicy, PolicySource)> {
        crate::repo::blocking(&self.inner, |inner| {
            let head = inner.head()?;
            match inner.policy_at(head.snapshot)? {
                Ok((policy, source)) => Ok((policy.as_ref().clone(), source)),
                Err(reason) => Err(Error::Policy(reason)),
            }
        })
        .await
    }

    /// ADR 0026 amendment: [`Error::Policy`] when `record` writes a
    /// `.hord-policy.toml` that does not parse (the lander would reject
    /// it), for `hord policy check` to report early.
    pub async fn check_policy_file(&self, record: &ChangeRecord) -> Result<()> {
        let (base, result) = (record.base, record.result);
        crate::repo::blocking(&self.inner, move |inner| {
            inner
                .check_policy_file(base, result)?
                .map_err(Error::Policy)
        })
        .await
    }

    /// Policy facts for `record` (ADR 0026), with the evidence indexed for
    /// its result snapshot.
    pub async fn policy_facts(&self, record: ChangeRecord) -> Result<Facts> {
        crate::repo::blocking(&self.inner, move |inner| inner.policy_facts(&record, true)).await
    }

    /// The impact set of `record` in its result snapshot under the default
    /// bound (spec §6.5): its write set and the definitions that reference
    /// it, 2 hops and within its package.
    pub async fn impact(&self, record: ChangeRecord) -> Result<ImpactSet> {
        crate::repo::blocking(&self.inner, move |inner| {
            inner
                .impact_set(&record, ImpactBound::default(), false)?
                .map_err(|err| Error::Verify(err.to_string()))
        })
        .await
    }

    /// Verify a workspace's current proposal (`hord verify`, spec §10.2):
    /// the requirements head's policy applies to it, planned with reuse by
    /// this repository's verifier, run in a clean checkout of the
    /// proposal's result snapshot (a slot under `.hord/verify/`), and
    /// indexed under that snapshot, where the lander reuses them.
    pub async fn verify_workspace(
        &self,
        ws: &mut crate::Workspace,
        plan_only: bool,
    ) -> Result<WorkspaceVerification> {
        let preview = hord_core::Intent::from_summary("(verify preview)");
        let proposal = ws.preview(preview).await?;
        let record = Arc::new(proposal.record);
        let inner = Arc::clone(&self.inner);
        let prepared = {
            let record = Arc::clone(&record);
            crate::repo::blocking(&self.inner, move |inner| {
                let head = inner.head()?;
                let (policy, source) = inner.policy_at(head.snapshot)?.map_err(Error::Policy)?;
                let (_, requirements) = inner.requirements(&policy, &record)?;
                Ok((source, requirements, policy.land().max_impact))
            })
            .await?
        };
        let (policy_source, requirements, max_impact) = prepared;
        let context = Arc::new(CandidateContext::new(
            inner,
            proposal.change,
            Arc::clone(&record),
            Vec::new(),
            false,
        ));
        let request = VerifyRequest {
            change_id: proposal.change,
            change: Arc::clone(&record),
            report: ConflictReport::empty(proposal.change, record.base),
            policy: VerifyPolicy {
                require: requirements.clone(),
                max_impact,
                ..VerifyPolicy::default()
            },
            context: Arc::clone(&context) as Arc<dyn VerifyContext>,
            plan_only,
        };
        let verdict = self.inner.verifier.verify(request).await;
        let plan = context.take_plan();
        context.release();
        Ok(WorkspaceVerification {
            snapshot: record.result,
            policy_source,
            requirements,
            plan,
            verdict,
        })
    }
}
