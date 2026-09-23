//! Blob-tier 3-way line merge (spec §5.2 rule 5).

use crate::Conflict;

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
        Err(_) => Err(Conflict::hard(
            Vec::new(),
            "blob 3-way line merge conflict (spec §5.2 rule 5)",
        )),
    }
}

fn utf8<'a>(bytes: &'a [u8], side: &str) -> Result<&'a str, Conflict> {
    std::str::from_utf8(bytes).map_err(|_| {
        Conflict::hard(
            Vec::new(),
            format!("binary blob conflict on {side} (spec §5.2 rule 5)"),
        )
    })
}
