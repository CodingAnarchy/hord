//! Repository policy (spec §7.2, ADR 0026): parse `.hord-policy.toml` and
//! evaluate it against a change and the evidence for its snapshot.
//!
//! The policy that judges a change is the one in its landing base (head),
//! never the one in the change's own result ([`POLICY_PATH`]). A head
//! without the file judges by [`CompiledPolicy::default`].
//!
//! ```text
//! parse(toml)                  -> CompiledPolicy   (line/column on error)
//! CompiledPolicy::evaluate(&impl ChangeFacts) -> Decision
//!   Allow | Deny { reasons: Vec<Violation> }       (serde-serializable)
//! ```
//!
//! Evaluation reads only [`ChangeFacts`], so this crate does not depend on
//! how a change is stored. The lander and `hord policy check` implement it
//! from a change record, its snapshot, and the evidence indexed for that
//! snapshot (ADR 0025). [`Facts`] is a plain-data implementation.
//!
//! Requirements are evidence tags, `kind` or `kind:qualifier`
//! ([`EvidenceTag`]), and evidence carries the same pair
//! ([`hord_core::Evidence::qualifier`]). A requirement is met by `Pass`
//! evidence of the same kind and, when the requirement names one, the same
//! qualifier; `test:full` also meets `test:selected` (ADR 0026).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod eval;
mod facts;
mod parse;
mod tag;

pub use error::{Location, ParseError};
pub use eval::{Decision, EvidenceState, Trigger, Violation, ViolationSource};
pub use facts::{ActorClass, ChangeFacts, EvidenceFact, Facts, TouchedDefinition};
pub use parse::{CompiledPolicy, DEFAULT_MAX_REPLAY_ATTEMPTS, POLICY_PATH, parse};
pub use tag::EvidenceTag;
