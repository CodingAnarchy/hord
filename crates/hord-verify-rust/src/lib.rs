//! Rust Tier 3 (spec §4.2): cargo `check`, `test`, `clippy`, and `bench`
//! as [`hord_core::Evidence`], per-test coverage with `cargo-llvm-cov`,
//! and test selection from coverage-derived `Tests` edges (ADR 0022).
//!
//! - [`RustVerifier`] implements [`hord_verify::Verifier`]: it plans the
//!   policy's requirements for a change's impact set and runs them with a
//!   [`CargoRunner`], one evidence object per command.
//! - [`select`] is ADR 0022's selection: tests whose coverage or static
//!   edges reach the impact set, tests the change adds or edits, package
//!   fallbacks for what coverage cannot attribute ([`Fallback`]), and the
//!   whole workspace without a usable coverage record. Tests run with
//!   `--exact` per test binary.
//! - [`coverage::collect`] builds the [`hord_verify::CoverageRecord`]
//!   that selection reads, following subprocesses.
//! - [`diff_rust_file`] classifies what a change touched
//!   ([`hord_verify::ChangeFacts`]) with the Rust adapter.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod cancel;
mod cargo;
pub mod coverage;
mod facts;
pub mod libtest;
pub mod narrow;
mod runner;
mod select;
mod verifier;

pub use cancel::Cancel;
pub use cargo::{CargoWorkspace, Package, Target};
pub use coverage::{CoverageOptions, CoverageRun, DefinitionIndex, TestFilter};
pub use facts::{diff_rust_file, rust_traits};
pub use runner::{CargoRunner, DEFAULT_IDLE_TIMEOUT, DEFAULT_TIMEOUT, RunOutput, detect_toolchain};
pub use select::{Fallback, SelectInput, Selection, select, select_ignoring};
pub use verifier::{InstrumentedRun, RustVerifier, requirement};
