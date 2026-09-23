//! [`NodeId`]s that stand for files rather than definitions.
//!
//! Read and write sets are `BTreeSet<NodeId>` (spec §3.5). Blob-tier files use
//! the path as identity (spec §6.3, ADR 0012), and a parsed file's root has a
//! path-derived id (ADR 0015, [`hord_diff::file_parent`]). Both are the same
//! id, so one set holds definitions and files and the conflict check compares
//! them the same way.

use hord_core::{NodeId, RepoPath};

/// [`NodeId`] for a whole file: [`hord_diff::file_parent`]`(path)`.
///
/// Used for blob-tier reads and writes and for coarse writes (deletes,
/// unparseable results, Tier 0 imports). For a parsed file it is also the
/// root that file-level ops name and glue edits write.
#[must_use]
pub fn path_node_id(path: &RepoPath) -> NodeId {
    hord_diff::file_parent(path)
}

/// Former id for a parsed file's glue. Since ADR 0015 glue edits write the
/// file root, so this is [`path_node_id`]; kept so existing callers build.
#[deprecated(note = "ADR 0015: glue edits write the file root; use `path_node_id`")]
#[must_use]
pub fn glue_node_id(path: &RepoPath) -> NodeId {
    path_node_id(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_ids_are_the_file_roots() {
        let a: RepoPath = "src/lib.rs".parse().unwrap();
        let b: RepoPath = "src/main.rs".parse().unwrap();
        assert_eq!(path_node_id(&a), hord_diff::file_parent(&a));
        assert_ne!(path_node_id(&a), path_node_id(&b));
        assert_ne!(path_node_id(&a), NodeId::nil());
        #[allow(deprecated)]
        let glue = glue_node_id(&a);
        assert_eq!(glue, path_node_id(&a));
    }
}
