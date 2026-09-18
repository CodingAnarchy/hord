//! Git import/export at Tier 0 (spec §9).
//!
//! Every file is a [`Blob`](hord_core::Blob); there are no
//! [`NodeFile`](hord_core::NodeFile) objects. Import walks history with
//! [`gix`]. Export projects a snapshot to a git tree. The round-trip invariant
//! is `export(import(repo))` reproduces every git tree SHA.
//!
//! [`hord_store::Store`] implements [`Store`]. [`MemoryStore`] is an in-memory
//! stand-in for tests.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod error;
mod export;
mod import;
mod leaf;
mod oid;
mod store;

pub use error::Error;
pub use export::{export_change, export_log, export_tree, format_commit_message};
pub use import::{import_git, import_git_ref, import_git_window};
pub use oid::GitOid;
pub use store::{MemoryStore, Store};

use hord_core::ChangeId;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Head of the imported log: the [`ChangeId`] returned by [`import_git`].
pub type ImportedHead = ChangeId;
