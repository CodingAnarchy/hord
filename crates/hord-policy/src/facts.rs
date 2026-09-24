//! What evaluation needs to know about a change.

use std::collections::BTreeSet;

use hord_core::{Actor, Evidence, EvidenceResult, NodeId, RepoPath};
use serde::{Deserialize, Serialize};

use crate::EvidenceTag;

/// Who authored a change, as `when = { actor = … }` names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorClass {
    /// [`Actor::Human`].
    Human,
    /// [`Actor::Agent`].
    Agent,
}

impl ActorClass {
    /// The policy spelling: `human` or `agent`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

impl From<&Actor> for ActorClass {
    fn from(actor: &Actor) -> Self {
        match actor {
            Actor::Human { .. } => Self::Human,
            Actor::Agent { .. } => Self::Agent,
        }
    }
}

/// A definition the change writes: in its write set, with what policy can
/// match on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TouchedDefinition {
    /// The definition's identity.
    pub node: NodeId,
    /// File that contains it (in the result, or in the base if deleted).
    pub path: RepoPath,
    /// Node kinds the change touches in this definition: its own kind
    /// (`function_item`) and the kinds of nodes inside its changed content
    /// (`unsafe_block`). `touches_kind` matches any of them.
    pub kinds: BTreeSet<String>,
    /// Visibility as written (`pub`, `pub(crate)`), or `None` when private
    /// or when the language has none. `touches_visibility` matches it
    /// exactly.
    pub visibility: Option<String>,
}

/// One piece of evidence for the change's snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceFact {
    /// What it is, e.g. `test:selected`.
    pub tag: EvidenceTag,
    /// Its outcome. Only [`EvidenceResult::Pass`] meets a requirement.
    pub result: EvidenceResult,
}

impl EvidenceFact {
    /// The fact for a stored [`Evidence`], or `None` when it has no tag
    /// ([`EvidenceTag::of`]).
    #[must_use]
    pub fn of(evidence: &Evidence) -> Option<Self> {
        Some(Self {
            tag: EvidenceTag::of(evidence)?,
            result: evidence.result.clone(),
        })
    }
}

/// The facts about a change that policy evaluation reads (spec §7.2).
///
/// Implemented from a change record, its result snapshot, and the evidence
/// indexed for that snapshot (ADR 0025). Every slice is read in order, so
/// an implementation that returns them in a stable order gets a stable
/// [`crate::Decision`].
pub trait ChangeFacts {
    /// Who authored the change.
    fn actor(&self) -> ActorClass;

    /// Size of the change's write set (`ChangeRecord::write_set`), for
    /// `max_write_set` and `write_set_gt`.
    fn write_set_len(&self) -> usize;

    /// Definitions the change writes, with their kinds and visibility, for
    /// `touches_kind` and `touches_visibility`.
    fn touched_definitions(&self) -> &[TouchedDefinition];

    /// Every file the change creates, edits, deletes, or moves, for
    /// `paths` in a rule without a definition predicate.
    fn touched_paths(&self) -> &[RepoPath];

    /// Evidence for the change's snapshot.
    fn evidence(&self) -> &[EvidenceFact];
}

/// Plain-data [`ChangeFacts`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    /// See [`ChangeFacts::actor`].
    pub actor: ActorClass,
    /// See [`ChangeFacts::write_set_len`].
    pub write_set_len: usize,
    /// See [`ChangeFacts::touched_definitions`].
    pub definitions: Vec<TouchedDefinition>,
    /// See [`ChangeFacts::touched_paths`].
    pub paths: Vec<RepoPath>,
    /// See [`ChangeFacts::evidence`].
    pub evidence: Vec<EvidenceFact>,
}

impl Facts {
    /// Facts for a change by `actor` that touches nothing and has no
    /// evidence.
    #[must_use]
    pub fn new(actor: ActorClass) -> Self {
        Self {
            actor,
            write_set_len: 0,
            definitions: Vec::new(),
            paths: Vec::new(),
            evidence: Vec::new(),
        }
    }
}

impl ChangeFacts for Facts {
    fn actor(&self) -> ActorClass {
        self.actor
    }

    fn write_set_len(&self) -> usize {
        self.write_set_len
    }

    fn touched_definitions(&self) -> &[TouchedDefinition] {
        &self.definitions
    }

    fn touched_paths(&self) -> &[RepoPath] {
        &self.paths
    }

    fn evidence(&self) -> &[EvidenceFact] {
        &self.evidence
    }
}
