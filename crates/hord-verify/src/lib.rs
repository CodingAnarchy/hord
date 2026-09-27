//! Verification engine (spec §6.5, §7.1): impact sets, the evidence index
//! and reuse, test-coverage records, and the [`Verifier`] contract.
//!
//! ```text
//! write_set ──impact_set(graph, bound)──▶ ImpactSet (+ ChangeFacts)
//!   Verifier::plan(snapshot, impact, policy) ──▶ VerifyPlan (checks to run)
//!   VerifyPlan::apply_reuse(index)            ──▶ evidence already indexed is not re-run
//!   Verifier::run(checkout, plan)             ──▶ Vec<Evidence>
//! verify(..) = plan → reuse → run → index ──▶ Verdict::Pass { evidence } | Fail { evidence, reason }
//! ```
//!
//! Evidence lives beside snapshots, never inside change records (ADR 0025):
//! [`EvidenceIndex`] keys it by `(snapshot, toolchain, command, scope)`.
//! [`hord_store::Store`] implements the index; [`MemoryIndex`] is an
//! in-memory one for tests and dry runs.
//!
//! Test selection is language-specific and lives in the language's verifier
//! (`hord-verify-rust`, ADR 0022). This crate holds what every language
//! shares: the impact set, the per-test coverage record
//! ([`CoverageRecord`], stored as `Evidence { kind: Custom("coverage") }`),
//! and the facts about a change that selection's fallbacks read
//! ([`ChangeFacts`]).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod change;
mod coverage;
mod drift;
mod error;
mod evidence;
mod impact;
mod index;
mod plan;
mod toolchain;
mod verifier;

pub use change::{
    ChangeFacts, DefDelta, DefTraits, Definition, FileVersion, TouchedDef, line_range,
};
pub use coverage::{
    COVERAGE_KIND, CoverageRecord, RUNS_PER_RECORD, TestCoverage, TestRef, TestRun, TestTarget,
    find_coverage, newest_coverage,
};
pub use drift::Drift;
pub use error::{Error, Result};
pub use evidence::EvidenceFields;
pub use impact::{ImpactBound, ImpactSet, ReferenceGraph, impact_set, impact_set_attributable};
pub use index::{EvidenceIndex, MemoryIndex, get_evidence, get_log, put_log};
pub use plan::{Check, Reused, VerifyPlan, VerifyPolicy};
pub use toolchain::Toolchain;
pub use verifier::{Checkout, Verdict, Verifier, plan_with_reuse, verify};
