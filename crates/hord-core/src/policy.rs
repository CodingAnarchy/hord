//! Repository policy objects (spec §7.2).
//!
//! Field names match `.hord-policy.toml` (ADR 0026). Evaluation lives in
//! `hord-policy`; this crate only stores the object.

use serde::{Deserialize, Serialize};

/// Versioned policy evaluated at landing against a change and its evidence.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Policy {
    /// Default requirements applied to every land.
    pub land: LandPolicy,
    /// Conditional rules, evaluated in order.
    pub rules: Vec<PolicyRule>,
    /// `[replay]` table (ADR 0028): limits on each replay attempt. Omitted
    /// from the encoding when it is the default, so policies written before
    /// it keep their ids.
    #[serde(default, skip_serializing_if = "ReplayPolicy::is_default")]
    pub replay: ReplayPolicy,
}

/// `[replay]` table of `.hord-policy.toml` (ADR 0028, spec §6.4 rung 2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ReplayPolicy {
    /// What one replay attempt may spend (spec §6.6 `Budget`).
    pub budget: ReplayBudget,
}

impl ReplayPolicy {
    /// Whether every key has its default value.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The spec §6.6 `Budget` of one replay attempt (ADR 0028, ADR 0029): the
/// lander kills a harness that runs past `wall_time_ms` and rejects a
/// result whose reported tokens or cost exceed their limits.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ReplayBudget {
    /// Wall-clock time for one attempt, in milliseconds.
    pub wall_time_ms: u64,
    /// Model tokens the harness may report using; `None` is no limit.
    pub tokens: Option<u64>,
    /// Cost the harness may report, in millionths of a US dollar; `None` is
    /// no limit.
    pub cost_micros: Option<u64>,
}

impl ReplayBudget {
    /// `wall_time_ms` when the policy leaves it out: ten minutes.
    pub const DEFAULT_WALL_TIME_MS: u64 = 600_000;
}

impl Default for ReplayBudget {
    fn default() -> Self {
        Self {
            wall_time_ms: Self::DEFAULT_WALL_TIME_MS,
            tokens: None,
            cost_micros: None,
        }
    }
}

/// `[land]` table of `.hord-policy.toml`. Every key is optional in the file
/// (ADR 0026); the defaults are filled in here.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct LandPolicy {
    /// Evidence kinds that must be [`crate::EvidenceResult::Pass`].
    pub require: Vec<String>,
    /// When true, write–read conflicts are strict (spec §6.3).
    pub strict_reads: bool,
    /// Maximum write-set size in definitions; larger changes need review.
    /// `None` is no limit.
    pub max_write_set: Option<u64>,
    /// Maximum replay attempts before escalation.
    pub max_replay_attempts: u64,
    /// Impact-set size above which verification falls back to the full
    /// suite (spec §7.1 item 4, ADR 0022). `None`, the default, is no size
    /// fallback. Omitted from the encoding when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_impact: Option<usize>,
}

/// One `[[rule]]` entry of `.hord-policy.toml`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// [`LandPolicy`] as it was before `max_impact` (ADR 0022).
    #[derive(Serialize)]
    struct LandPolicyV0 {
        require: Vec<String>,
        strict_reads: bool,
        max_write_set: Option<u64>,
        max_replay_attempts: u64,
    }

    fn land(max_impact: Option<usize>) -> LandPolicy {
        LandPolicy {
            require: vec!["check".into()],
            strict_reads: false,
            max_write_set: Some(200),
            max_replay_attempts: 2,
            max_impact,
        }
    }

    #[test]
    fn no_max_impact_is_omitted_from_the_encoding() -> Result<(), Box<dyn std::error::Error>> {
        let v0 = LandPolicyV0 {
            require: vec!["check".into()],
            strict_reads: false,
            max_write_set: Some(200),
            max_replay_attempts: 2,
        };
        let bytes = hord_encoding::encode(&land(None))?;
        assert_eq!(bytes, hord_encoding::encode(&v0)?);
        let back: LandPolicy = hord_encoding::decode(&bytes)?;
        assert_eq!(back, land(None));

        let with = hord_encoding::encode(&land(Some(40)))?;
        assert_ne!(with, bytes);
        let back: LandPolicy = hord_encoding::decode(&with)?;
        assert_eq!(back.max_impact, Some(40));
        Ok(())
    }

    /// [`Policy`] as it was before `[replay]` (ADR 0028).
    #[derive(Serialize)]
    struct PolicyV0 {
        land: LandPolicy,
        rules: Vec<PolicyRule>,
    }

    #[test]
    fn a_default_replay_table_is_omitted_from_the_encoding()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = Policy {
            land: land(None),
            rules: Vec::new(),
            replay: ReplayPolicy::default(),
        };
        let bytes = hord_encoding::encode(&policy)?;
        let v0 = PolicyV0 {
            land: land(None),
            rules: Vec::new(),
        };
        assert_eq!(bytes, hord_encoding::encode(&v0)?);
        let back: Policy = hord_encoding::decode(&bytes)?;
        assert_eq!(back, policy);

        let limited = Policy {
            replay: ReplayPolicy {
                budget: ReplayBudget {
                    wall_time_ms: 1_000,
                    tokens: Some(10),
                    cost_micros: Some(5),
                },
            },
            ..policy
        };
        let with = hord_encoding::encode(&limited)?;
        assert_ne!(with, bytes);
        let back: Policy = hord_encoding::decode(&with)?;
        assert_eq!(back, limited);
        Ok(())
    }
}
