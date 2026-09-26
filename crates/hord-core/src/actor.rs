//! Actors and timestamps attached to changes and evidence.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Bytes;

/// Who produced a change or evidence object (spec §3.5).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Actor {
    /// A human author, identified by an opaque id (email, login, git author).
    Human {
        /// Stable identifier for this person in the repository.
        id: String,
    },
    /// An agent author, including the model and harness that produced the work.
    Agent {
        /// Stable identifier for this agent identity.
        id: String,
        /// Model name as reported by the harness.
        model: String,
        /// Hash of the model artifact or weights, if known.
        model_hash: Bytes,
        /// Harness that invoked the model (tooling, prompt pack, …).
        harness: String,
    },
}

impl Actor {
    /// The stable identifier of either kind of actor.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Human { id } | Self::Agent { id, .. } => id,
        }
    }
}

/// Unix time in milliseconds.
///
/// Hashed object fields must not contain floating-point values (spec §3.9).
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Wrap a millisecond Unix timestamp.
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    /// Milliseconds since the Unix epoch.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// The wall clock now: 0 if it reads before the epoch, and
    /// `u64::MAX` past the year 584 million.
    #[must_use]
    pub fn now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self(ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_the_id_field_of_either_kind() {
        assert_eq!(Actor::Human { id: "ada".into() }.id(), "ada");
        let agent = Actor::Agent {
            id: "agent-7".into(),
            model: "m".into(),
            model_hash: Bytes::default(),
            harness: "h".into(),
        };
        assert_eq!(agent.id(), "agent-7");
    }
}
