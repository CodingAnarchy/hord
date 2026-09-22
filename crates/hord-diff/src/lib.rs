//! Structural diff and 3-way merge (spec §5, ADR 0002).
//!
//! # Apply target
//!
//! [`apply`] returns an [`hord_lang::IdentifiedTree`]: the interned CST plus
//! the definition [`hord_core::NodeId`] map after the edit. Inserted nodes
//! that the mapping did not name receive a fresh id.
//!
//! [`hord_core::Op`] names new content by [`hord_core::ObjectId`] only, so
//! [`apply`] takes a `store` [`hord_lang::NodeTree`] that interns those
//! objects — typically the `result` tree passed to [`diff`], or a union of
//! `ours` and `theirs` for [`merge`].
//!
//! # File root
//!
//! Definition [`hord_core::NodeId`]s stop at adapter definitions. The CST
//! file root is not a definition. Ops that need a parent for a file-level
//! definition use [`file_parent`] ([`hord_core::NodeId::nil`]). A
//! [`hord_core::Op::Replace`] of that id swaps the whole file
//! (file-level non-definition glue).
//!
//! # Granularity
//!
//! ADR 0002: GumTree-style matching on `normalized` hashes, definition
//! granularity, no new crate. Sub-definition edits collapse to Replace on
//! the enclosing definition. Optimality is not a gate. Rename similarity
//! stays M2.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod apply;
mod blob;
mod cst_merge;
mod defs;
mod diff;
mod error;
mod graft;
mod merge;
mod text_merge;

pub use apply::apply;
pub use blob::merge_blob;
pub use defs::file_parent;
pub use diff::diff;
pub use error::Error;
pub use merge::{Conflict, ConflictKind, MergeResult, merge, merge_ops};

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
