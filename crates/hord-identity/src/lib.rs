//! Node identity for one snapshot (spec §3.4, §11).
//!
//! [`assign`] gives every definition a fresh [`hord_core::NodeId`].
//! [`carry`] applies the decided carrying rules and returns the result ids,
//! births, deaths, derivations, and moves. [`identity_map`] is the
//! per-snapshot [`hord_core::IdentityMap`]: each id located by file and a
//! child-index path from that file's root.
//!
//! Rename similarity is [`hord_lang::default_identify`] (ADR 0007). This crate
//! does not choose a different metric or threshold. Fresh birth ids are derived
//! from the definition content id so they do not follow ULID randomness or the
//! source order of unrelated nodes.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod assign;
mod carry;
mod error;
mod map;

pub use assign::assign;
pub use carry::{Declaration, carry};
pub use error::Error;
pub use map::{SnapshotFile, identity_map};

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
