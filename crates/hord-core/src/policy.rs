//! Repository policy objects (spec §7.2).
//!
//! Field names match `.hord/policy.toml`. Evaluation lives in `hord-policy`;
//! this crate only stores the object.

use serde::{Deserialize, Serialize};

/// Versioned policy evaluated at landing against a change and its evidence.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Policy {
    /// Default requirements applied to every land.
    pub land: LandPolicy,
    /// Conditional rules, evaluated in order.
    pub rules: Vec<PolicyRule>,
}

/// `[land]` table of `.hord/policy.toml`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct LandPolicy {
    /// Evidence kinds that must be [`crate::EvidenceResult::Pass`].
    pub require: Vec<String>,
    /// When true, write–read conflicts are strict (spec §6.3).
    pub strict_reads: bool,
    /// Maximum write-set size in definitions; larger changes need review.
    pub max_write_set: u64,
    /// Maximum replay attempts before escalation.
    pub max_replay_attempts: u64,
}

/// One `[[rule]]` entry of `.hord/policy.toml`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Rule name, for diagnostics.
    pub name: String,
    /// When this rule applies.
    pub when: PolicyWhen,
    /// Extra evidence kinds required if `when` matches.
    pub require: Vec<String>,
}

/// Match conditions on a [`PolicyRule`].
///
/// Absent fields are unconstrained. Names match the TOML `when = { … }` keys.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PolicyWhen {
    /// Node kind the change must touch, e.g. `unsafe_block`.
    pub touches_kind: Option<String>,
    /// Visibility the change must touch, e.g. `pub`.
    pub touches_visibility: Option<String>,
    /// Path globs the change must intersect, e.g. `crates/hord-core/**`.
    pub paths: Option<Vec<String>>,
    /// Actor class, e.g. `agent` or `human`.
    pub actor: Option<String>,
    /// Apply when the write set is strictly larger than this many definitions.
    pub write_set_gt: Option<u64>,
}
