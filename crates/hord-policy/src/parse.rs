//! Parse and check `.hord-policy.toml` (spec §7.2, ADR 0026).

use std::collections::HashSet;
use std::ops::Range;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use hord_core::{LandPolicy, Policy, PolicyRule, PolicyWhen, ReplayBudget, ReplayPolicy};
use serde::Deserialize;
use toml::Spanned;

use crate::{ActorClass, EvidenceTag, ParseError};

/// Where the policy lives: a tracked file at the repository root, read from
/// the landing base (head), never from the change being judged (ADR 0026).
pub const POLICY_PATH: &str = ".hord-policy.toml";

/// `max_replay_attempts` when `[land]` leaves it out (ADR 0026, spec §6.4).
pub const DEFAULT_MAX_REPLAY_ATTEMPTS: u64 = 2;

/// A checked policy, ready to evaluate.
///
/// Built by [`parse`] from TOML, or by [`CompiledPolicy::from_object`] from
/// a stored [`Policy`] object. Requirements are parsed into
/// [`EvidenceTag`]s, `actor` into an [`ActorClass`], and `paths` into
/// compiled globs.
#[derive(Clone, Debug)]
pub struct CompiledPolicy {
    pub(crate) policy: Policy,
    pub(crate) land_require: Vec<EvidenceTag>,
    pub(crate) rules: Vec<CompiledRule>,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledRule {
    pub(crate) require: Vec<EvidenceTag>,
    pub(crate) actor: Option<ActorClass>,
    pub(crate) paths: Option<GlobSet>,
}

impl Default for CompiledPolicy {
    /// The policy of a head with no policy file: `[land]` defaults and no
    /// rules, which allows every change (ADR 0026).
    fn default() -> Self {
        Self {
            policy: Policy {
                land: LandPolicy {
                    require: Vec::new(),
                    strict_reads: false,
                    max_write_set: None,
                    max_replay_attempts: DEFAULT_MAX_REPLAY_ATTEMPTS,
                    max_impact: None,
                },
                rules: Vec::new(),
                replay: ReplayPolicy::default(),
            },
            land_require: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl CompiledPolicy {
    /// Check a stored policy object. Errors carry no location.
    pub fn from_object(policy: Policy) -> Result<Self, ParseError> {
        let unspanned = |s: &String| Item {
            value: s.clone(),
            span: None,
        };
        let draft = Draft {
            land_require: policy.land.require.iter().map(unspanned).collect(),
            rules: policy
                .rules
                .iter()
                .map(|rule| DraftRule {
                    name: unspanned(&rule.name),
                    require: rule.require.iter().map(unspanned).collect(),
                    actor: rule.when.actor.as_ref().map(unspanned),
                    paths: rule
                        .when
                        .paths
                        .as_ref()
                        .map(|p| (p.iter().map(unspanned).collect(), None)),
                })
                .collect(),
        };
        draft.compile(None, policy)
    }

    /// The policy object: what the file says, with `[land]` defaults filled
    /// in.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The `[land]` table, including `strict_reads` and
    /// `max_replay_attempts`, which the lander reads.
    #[must_use]
    pub fn land(&self) -> &LandPolicy {
        &self.policy.land
    }

    /// The `[replay]` table (ADR 0028): the budget of each replay attempt.
    #[must_use]
    pub fn replay(&self) -> &ReplayPolicy {
        &self.policy.replay
    }
}

/// Parse `.hord-policy.toml` as spec §7.2 shows it.
///
/// Every `[land]` key is optional (ADR 0026): `require = []`,
/// `strict_reads = false`, no `max_write_set`, `max_replay_attempts = 2`.
/// `max_impact` (ADR 0022) is optional and unset by default.
/// `[[rule]]` tables are optional and need `name`, `when`, and `require`.
/// `[replay]` is optional (ADR 0028): `budget = { wall_time_secs, tokens,
/// cost_usd }`, each key optional, with a ten-minute wall-clock default and
/// no token or cost limit. Unknown keys are errors. Errors carry the 1-based line and column of the
/// offending value.
pub fn parse(source: &str) -> Result<CompiledPolicy, ParseError> {
    let raw: RawPolicy = toml::from_str(source)
        .map_err(|e| ParseError::at(Some(source), e.span(), e.message().to_owned()))?;
    let spanned = |s: &Spanned<String>| Item {
        value: s.get_ref().clone(),
        span: Some(s.span()),
    };
    let draft = Draft {
        land_require: raw.land.require.iter().map(spanned).collect(),
        rules: raw
            .rule
            .iter()
            .map(|rule| {
                let when = rule.when.get_ref();
                DraftRule {
                    name: spanned(&rule.name),
                    require: rule.require.iter().map(spanned).collect(),
                    actor: when.actor.as_ref().map(spanned),
                    paths: when
                        .paths
                        .as_ref()
                        .map(|p| (p.get_ref().iter().map(spanned).collect(), Some(p.span()))),
                }
            })
            .collect(),
    };
    let unspan = |s: &Spanned<String>| s.get_ref().clone();
    let replay = replay_policy(source, &raw.replay)?;
    let policy = Policy {
        land: LandPolicy {
            require: raw.land.require.iter().map(unspan).collect(),
            strict_reads: raw.land.strict_reads.unwrap_or(false),
            max_write_set: raw.land.max_write_set,
            max_replay_attempts: raw
                .land
                .max_replay_attempts
                .unwrap_or(DEFAULT_MAX_REPLAY_ATTEMPTS),
            max_impact: raw.land.max_impact,
        },
        rules: raw
            .rule
            .into_iter()
            .map(|rule| {
                let when = rule.when.into_inner();
                PolicyRule {
                    name: rule.name.into_inner(),
                    when: PolicyWhen {
                        touches_kind: when.touches_kind,
                        touches_visibility: when.touches_visibility,
                        paths: when
                            .paths
                            .map(|p| p.into_inner().iter().map(unspan).collect()),
                        actor: when.actor.map(Spanned::into_inner),
                        write_set_gt: when.write_set_gt,
                    },
                    require: rule.require.iter().map(unspan).collect(),
                }
            })
            .collect(),
        replay,
    };
    draft.compile(Some(source), policy)
}

/// Millionths of a US dollar in one dollar (`cost_usd` is stored as
/// micro-dollars, so the policy object stays integral and hashable).
const MICROS_PER_USD: f64 = 1_000_000.0;

/// The `[replay]` table with its defaults filled in.
fn replay_policy(source: &str, raw: &RawReplay) -> Result<ReplayPolicy, ParseError> {
    let Some(budget) = &raw.budget else {
        return Ok(ReplayPolicy::default());
    };
    let wall_time_ms = match &budget.wall_time_secs {
        None => ReplayBudget::DEFAULT_WALL_TIME_MS,
        Some(secs) => match secs.get_ref().checked_mul(1_000) {
            Some(ms) if ms > 0 => ms,
            _ => {
                return Err(ParseError::at(
                    Some(source),
                    Some(secs.span()),
                    "`wall_time_secs` must be at least 1 and at most u64::MAX / 1000".into(),
                ));
            }
        },
    };
    let cost_micros = match &budget.cost_usd {
        None => None,
        Some(usd) => {
            let micros = (usd.get_ref() * MICROS_PER_USD).round();
            if !micros.is_finite() || micros < 0.0 || micros >= u64::MAX as f64 {
                return Err(ParseError::at(
                    Some(source),
                    Some(usd.span()),
                    "`cost_usd` must be a non-negative number of dollars".into(),
                ));
            }
            // In range and integral: checked just above.
            Some(micros as u64)
        }
    };
    Ok(ReplayPolicy {
        budget: ReplayBudget {
            wall_time_ms,
            tokens: budget.tokens,
            cost_micros,
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    land: RawLand,
    #[serde(default)]
    rule: Vec<RawRule>,
    #[serde(default)]
    replay: RawReplay,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReplay {
    budget: Option<RawBudget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    wall_time_secs: Option<Spanned<u64>>,
    tokens: Option<u64>,
    cost_usd: Option<Spanned<f64>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLand {
    #[serde(default)]
    require: Vec<Spanned<String>>,
    strict_reads: Option<bool>,
    max_write_set: Option<u64>,
    max_replay_attempts: Option<u64>,
    max_impact: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: Spanned<String>,
    when: Spanned<RawWhen>,
    require: Vec<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWhen {
    touches_kind: Option<String>,
    touches_visibility: Option<String>,
    paths: Option<Spanned<Vec<Spanned<String>>>>,
    actor: Option<Spanned<String>>,
    write_set_gt: Option<u64>,
}

/// A value with its source span, when it came from source text.
struct Item {
    value: String,
    span: Option<Range<usize>>,
}

/// The values that need checking, with spans, from either input.
struct Draft {
    land_require: Vec<Item>,
    rules: Vec<DraftRule>,
}

struct DraftRule {
    name: Item,
    require: Vec<Item>,
    actor: Option<Item>,
    paths: Option<(Vec<Item>, Option<Range<usize>>)>,
}

impl Draft {
    fn compile(self, source: Option<&str>, policy: Policy) -> Result<CompiledPolicy, ParseError> {
        let error = |span: &Option<Range<usize>>, message: String| {
            ParseError::at(source, span.clone(), message)
        };
        let tags = |items: &[Item]| {
            items
                .iter()
                .map(|item| {
                    item.value
                        .parse::<EvidenceTag>()
                        .map_err(|e| error(&item.span, format!("invalid requirement {e}")))
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let land_require = tags(&self.land_require)?;
        let mut names = HashSet::new();
        let mut rules = Vec::with_capacity(self.rules.len());
        for rule in &self.rules {
            if !names.insert(rule.name.value.as_str()) {
                return Err(error(
                    &rule.name.span,
                    format!("duplicate rule name `{}`", rule.name.value),
                ));
            }
            let actor = match &rule.actor {
                Some(item) => Some(ActorClass::parse(&item.value).ok_or_else(|| {
                    error(
                        &item.span,
                        format!(
                            "unknown actor `{}`: expected `human` or `agent`",
                            item.value
                        ),
                    )
                })?),
                None => None,
            };
            let paths = match &rule.paths {
                Some((globs, span)) => {
                    if globs.is_empty() {
                        return Err(error(span, "`paths` is empty".into()));
                    }
                    let mut set = GlobSetBuilder::new();
                    for glob in globs {
                        let compiled = GlobBuilder::new(&glob.value)
                            .literal_separator(true)
                            .build()
                            .map_err(|e| {
                                error(&glob.span, format!("invalid glob `{}`: {e}", glob.value))
                            })?;
                        set.add(compiled);
                    }
                    Some(set.build().map_err(|e| error(span, e.to_string()))?)
                }
                None => None,
            };
            rules.push(CompiledRule {
                require: tags(&rule.require)?,
                actor,
                paths,
            });
        }
        Ok(CompiledPolicy {
            policy,
            land_require,
            rules,
        })
    }
}
