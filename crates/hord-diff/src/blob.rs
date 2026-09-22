//! Blob-tier 3-way line merge (spec §5.2 rule 5).

use hord_core::NodeId;

use crate::{Conflict, ConflictKind};

/// Git-style 3-way line merge of blob bytes via `diffy`.
///
/// Non-UTF-8 (binary) inputs are a hard conflict. Text conflicts returned
/// by `diffy` are hard conflicts; the conflict-marker payload is not used.
pub fn merge_blob(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Vec<u8>, Conflict> {
    let base_s = utf8(base, "base")?;
    let ours_s = utf8(ours, "ours")?;
    let theirs_s = utf8(theirs, "theirs")?;
    match diffy::merge(base_s, ours_s, theirs_s) {
        Ok(merged) => Ok(merged.into_bytes()),
        Err(_) => Err(Conflict {
            nodes: Vec::new(),
            kind: ConflictKind::Hard,
            reason: "blob 3-way line merge conflict (spec §5.2 rule 5)".into(),
            delete_vs: false,
        }),
    }
}

fn utf8<'a>(bytes: &'a [u8], side: &str) -> Result<&'a str, Conflict> {
    std::str::from_utf8(bytes).map_err(|_| Conflict {
        nodes: Vec::<NodeId>::new(),
        kind: ConflictKind::Hard,
        reason: format!("binary blob conflict on {side} (spec §5.2 rule 5)"),
        delete_vs: false,
    })
}
