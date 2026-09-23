//! Local content-addressed object store (spec §8.1).
//!
//! Objects are addressed by [`hord_core::ObjectId`] (BLAKE3-256 of canonical CBOR).
//! Recently written data is stored as loose files under `.hord/objects/`.
//! [`Store::pack`] writes zstd-compressed pack files with a sidecar offset
//! index (spec §8.1: packing is a background job, not part of [`Store::put`]).
//!
//! The redb index holds the landing [`Store::log`], named refs, workspace
//! metadata, and packed-object locations. [`Store::set_ref`] and
//! [`Store::append_log`] are buffered and flushed on [`Store::set_head`],
//! [`Store::pack`], [`Store::flush`], or drop. Ingest batches skip fsync;
//! those flush points use a durable commit.
//!
//! `node_history`, `edges`, and `identity` cache facts that also live in
//! stored objects (spec §8.1). [`Store::rebuild_index`] reconstructs them.
//! `evidence_by_snapshot` is created empty.
//!
//! `hord-txn` keeps two more tables here: the persistent lander queue
//! ([`Store::queue_push`]) and a per-snapshot identity index pointer
//! ([`Store::set_identity_index`]). The store does not interpret either value.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod error;
mod pack;
mod store;
mod workspace;

pub use error::Error;
pub use store::{EdgeKind, HORD_DIR, Store};
pub use workspace::{WorkspaceId, WorkspaceMeta};

/// Result of a store operation.
pub type Result<T> = std::result::Result<T, Error>;
