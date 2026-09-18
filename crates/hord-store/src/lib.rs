//! Local content-addressed object store (spec §8.1).
//!
//! Objects are addressed by [`hord_core::ObjectId`] (BLAKE3-256 of canonical CBOR).
//! Recently written data is stored as loose files under `.hord/objects/`.
//! [`Store::pack`] writes zstd-compressed pack files with a sidecar offset
//! index (spec §8.1: packing is a background job, not part of [`Store::put`]).
//!
//! The redb index holds the landing [`Store::log`], named refs, workspace
//! metadata, and packed-object locations. [`Store::set_ref`] is buffered and
//! flushed on [`Store::append_log`], [`Store::set_head`], [`Store::pack`],
//! [`Store::flush`], or drop. Tables `node_history` and
//! `evidence_by_snapshot` are created empty and are rebuildable.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod error;
mod pack;
mod store;
mod workspace;

pub use error::Error;
pub use store::{HORD_DIR, Store};
pub use workspace::{WorkspaceId, WorkspaceMeta};

/// Result of a store operation.
pub type Result<T> = std::result::Result<T, Error>;
