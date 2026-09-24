//! Evaluate a [`CompiledPolicy`] against [`ChangeFacts`].

use hord_core::{EvidenceResult, NodeId};
use serde::{Deserialize, Serialize};

use crate::parse::CompiledRule;
use crate::{ActorClass, ChangeFacts, CompiledPolicy, EvidenceTag};

/// The outcome of evaluating a policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Every requirement that applies is met.
    Allow,
    /// At least one requirement is not met.
    Deny {
        /// Every unmet requirement, in policy order: `[land] require`, then
        /// `max_write_set`, then each `[[rule]]`.
        reasons: Vec<Violation>,
    },
}

impl Decision {
    /// Whether the change may land.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// One unmet requirement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// Which part of the policy required it.
    pub source: ViolationSource,
    /// The rule's `name` when [`Self::source`] is [`ViolationSource::Rule`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// The evidence that is required and missing.
    pub requirement: EvidenceTag,
    /// Whether matching evidence is absent, failed, or skipped.
    pub evidence: EvidenceState,
    /// The facts that made the requirement apply: the definitions or paths
    /// a rule matched, its actor, the write-set size. Empty for
    /// `[land] require`, which always applies.
    pub triggers: Vec<Trigger>,
}

/// The part of a policy a [`Violation`] comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationSource {
    /// `[land] require`.
    Land,
    /// `[land] max_write_set`: a larger write set needs `review` evidence.
    MaxWriteSet,
    /// A `[[rule]]`.
    Rule,
}

/// What evidence there is for an unmet requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    /// No evidence of that kind.
    Absent,
    /// Evidence of that kind exists and at least one failed.
    Failed,
    /// Evidence of that kind exists and was skipped.
    Skipped,
}

/// A fact that made a requirement apply.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Trigger {
    /// A definition matched `touches_kind` / `touches_visibility` (and
    /// `paths`, if the rule has it).
    Definition {
        /// Its identity.
        node: NodeId,
        /// Its file, `/`-separated.
        path: String,
    },
    /// A touched file matched `paths`.
    Path {
        /// The file, `/`-separated.
        path: String,
    },
    /// The author matched `actor`.
    Actor {
        /// The author's class.
        actor: ActorClass,
    },
    /// The write set is larger than a limit (`write_set_gt` or
    /// `max_write_set`).
    WriteSet {
        /// Size of the write set.
        size: usize,
        /// The limit it exceeds.
        limit: u64,
    },
}

/// The requirement a write set over `max_write_set` needs: review of any
/// kind ("larger changes need review", spec §7.2, ADR 0026).
fn review() -> EvidenceTag {
    EvidenceTag::new("review", None).expect("`review` is a valid tag")
}

impl CompiledPolicy {
    /// Evaluate against `facts`: [`Decision::Allow`] when every requirement
    /// that applies is met by `Pass` evidence, else [`Decision::Deny`] with
    /// every unmet one.
    ///
    /// `strict_reads` and `max_replay_attempts` are for the lander and do
    /// not affect the decision.
    pub fn evaluate(&self, facts: &impl ChangeFacts) -> Decision {
        let mut reasons = Vec::new();
        for requirement in &self.land_require {
            if let Some(evidence) = unmet(facts, requirement) {
                reasons.push(Violation {
                    source: ViolationSource::Land,
                    rule: None,
                    requirement: requirement.clone(),
                    evidence,
                    triggers: Vec::new(),
                });
            }
        }
        let size = facts.write_set_len();
        if let Some(limit) = self.policy.land.max_write_set
            && exceeds(size, limit)
        {
            let requirement = review();
            if let Some(evidence) = unmet(facts, &requirement) {
                reasons.push(Violation {
                    source: ViolationSource::MaxWriteSet,
                    rule: None,
                    requirement,
                    evidence,
                    triggers: vec![Trigger::WriteSet { size, limit }],
                });
            }
        }
        for (rule, compiled) in self.policy.rules.iter().zip(&self.rules) {
            let Some(triggers) = matches(&rule.when, compiled, facts) else {
                continue;
            };
            for requirement in &compiled.require {
                if let Some(evidence) = unmet(facts, requirement) {
                    reasons.push(Violation {
                        source: ViolationSource::Rule,
                        rule: Some(rule.name.clone()),
                        requirement: requirement.clone(),
                        evidence,
                        triggers: triggers.clone(),
                    });
                }
            }
        }
        if reasons.is_empty() {
            Decision::Allow
        } else {
            Decision::Deny { reasons }
        }
    }
}

fn exceeds(size: usize, limit: u64) -> bool {
    u64::try_from(size).map_or(true, |size| size > limit)
}

/// `None` when some `Pass` evidence meets `requirement`, else what there is.
fn unmet(facts: &impl ChangeFacts, requirement: &EvidenceTag) -> Option<EvidenceState> {
    let mut state = EvidenceState::Absent;
    for fact in facts
        .evidence()
        .iter()
        .filter(|f| f.tag.satisfies(requirement))
    {
        match fact.result {
            EvidenceResult::Pass => return None,
            EvidenceResult::Fail { .. } => state = EvidenceState::Failed,
            EvidenceResult::Skipped { .. } => {
                if state == EvidenceState::Absent {
                    state = EvidenceState::Skipped;
                }
            }
        }
    }
    Some(state)
}

/// The triggers if `when` holds, else `None`.
///
/// Every predicate present must hold. `touches_kind`, `touches_visibility`,
/// and `paths` hold together on one definition when the rule has a
/// definition predicate; `paths` alone matches any touched file.
fn matches(
    when: &hord_core::PolicyWhen,
    compiled: &CompiledRule,
    facts: &impl ChangeFacts,
) -> Option<Vec<Trigger>> {
    let mut triggers = Vec::new();
    if let Some(actor) = compiled.actor {
        if facts.actor() != actor {
            return None;
        }
        triggers.push(Trigger::Actor { actor });
    }
    if let Some(limit) = when.write_set_gt {
        let size = facts.write_set_len();
        if !exceeds(size, limit) {
            return None;
        }
        triggers.push(Trigger::WriteSet { size, limit });
    }
    let path_matches = |path: &str| compiled.paths.as_ref().is_none_or(|set| set.is_match(path));
    if when.touches_kind.is_some() || when.touches_visibility.is_some() {
        let before = triggers.len();
        for def in facts.touched_definitions() {
            let kind_ok = when
                .touches_kind
                .as_ref()
                .is_none_or(|kind| def.kinds.contains(kind));
            let visibility_ok = when
                .touches_visibility
                .as_ref()
                .is_none_or(|v| def.visibility.as_ref() == Some(v));
            let path = def.path.to_string();
            if kind_ok && visibility_ok && path_matches(&path) {
                triggers.push(Trigger::Definition {
                    node: def.node,
                    path,
                });
            }
        }
        if triggers.len() == before {
            return None;
        }
    } else if compiled.paths.is_some() {
        let before = triggers.len();
        for path in facts.touched_paths() {
            let path = path.to_string();
            if path_matches(&path) {
                triggers.push(Trigger::Path { path });
            }
        }
        if triggers.len() == before {
            return None;
        }
    }
    Some(triggers)
}
