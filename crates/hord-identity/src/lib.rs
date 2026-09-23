//! Node identity for one snapshot (spec §3.4, §11).
//!
//! [`assign`] gives every definition a fresh [`hord_core::NodeId`].
//! [`carry`] applies the decided carrying rules and returns the result ids,
//! births, deaths, derivations, and moves. [`identity_map`] is the
//! per-snapshot [`hord_core::IdentityMap`]: each id located by file and a
//! child-index path from that file's root. Each file root has the
//! path-derived id [`file_root_id`] (ADR 0015), located at an empty path.
//!
//! Rename similarity is [`hord_lang::default_identify`] (ADR 0007). This crate
//! does not choose a different metric or threshold. Birth ids are derived
//! (ADR 0019, [`birth_id`]) from the definition's content id, its file's root
//! id, its site, and the base snapshot of the change that creates it, so they
//! are deterministic, and a definition that dies and is re-added later gets a
//! new id.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod assign;
mod carry;
mod error;
mod map;
mod root;

pub use assign::{assign, assign_in, birth_id};
pub use carry::{Declaration, carry, carry_in};
pub use error::Error;
pub use map::{SnapshotFile, identity_map};
pub use root::file_root_id;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
