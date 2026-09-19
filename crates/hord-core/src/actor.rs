//! Actors and timestamps attached to changes and evidence.

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
}
