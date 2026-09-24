//! Parse node specs and blame targets; resolution itself goes through the
//! session's backend (NodeIds read from the snapshots' identity, ADR 0017).
//!
//! Node ids accept the ULID text [`NodeId`] displays and a 32-digit hex
//! encoding of the same 128 bits.

use anyhow::{Context, Result, bail};
use hord_core::{NodeId, RepoPath};

/// Parse a ULID or a 32-digit hex NodeId. `None` means the string is not an id.
pub(crate) fn parse_node_id(spec: &str) -> Option<NodeId> {
    if let Ok(id) = spec.parse::<NodeId>() {
        return Some(id);
    }
    parse_node_hex(spec)
}

/// A blame argument after it has been classified.
pub(crate) enum BlameTarget {
    /// ULID or 32-digit hex. History is the store's `node_history`, no scan.
    Node(NodeId),
    /// Qualified name, resolved at head (`RepoBackend::resolve_name`).
    Name(String),
    /// 1-based line in a repository path.
    Line { path: RepoPath, line: u32 },
}

/// Classify `name`, `path:line`, or a NodeId.
pub(crate) fn parse_blame_target(spec: &str) -> Result<BlameTarget> {
    if let Some((path, line)) = split_path_line(spec)? {
        return Ok(BlameTarget::Line { path, line });
    }
    if let Some(id) = parse_node_id(spec) {
        return Ok(BlameTarget::Node(id));
    }
    if spec.is_empty() {
        bail!("blame target is empty");
    }
    Ok(BlameTarget::Name(spec.to_owned()))
}

fn parse_node_hex(spec: &str) -> Option<NodeId> {
    let spec = spec
        .strip_prefix("0x")
        .or_else(|| spec.strip_prefix("0X"))
        .unwrap_or(spec);
    // `from_str_radix` alone would also take a leading `+`.
    if spec.len() != 32 || !spec.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(spec, 16).ok().map(NodeId::from_u128)
}

fn split_path_line(spec: &str) -> Result<Option<(RepoPath, u32)>> {
    let Some((path, line)) = spec.rsplit_once(':') else {
        return Ok(None);
    };
    if path.is_empty() || path.ends_with(':') || line.is_empty() {
        return Ok(None);
    }
    if !line.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let Ok(line) = line.parse::<u64>() else {
        return Ok(None);
    };
    if line == 0 {
        bail!("line number in {spec:?} must be from 1 to {}", u32::MAX);
    }
    let Ok(line) = u32::try_from(line) else {
        bail!("line number in {spec:?} must be from 1 to {}", u32::MAX);
    };
    let path = path
        .parse::<RepoPath>()
        .with_context(|| format!("invalid repository path {path:?}"))?;
    Ok(Some((path, line)))
}

#[cfg(test)]
mod tests {
    use hord_core::NodeId;

    use super::*;

    fn node(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    #[test]
    fn node_id_accepts_ulid_and_hex() {
        let id = node(0xAB);
        assert_eq!(parse_node_id(&id.to_string()), Some(id));
        let hex = format!("{:032x}", id.as_u128());
        assert_eq!(parse_node_id(&hex), Some(id));
        assert_eq!(parse_node_id(&format!("0x{hex}")), Some(id));
        assert_eq!(parse_node_id(&hex.to_ascii_uppercase()), Some(id));
        assert_eq!(parse_node_id("crate::alpha"), None);
        assert_eq!(parse_node_id("deadbeef"), None);
        assert_eq!(parse_node_id(&"ab".repeat(32)), None);
    }

    #[test]
    fn blame_target_splits_path_line_and_names() {
        let line = parse_blame_target("src/lib.rs:12").unwrap();
        match line {
            BlameTarget::Line { path, line } => {
                assert_eq!(path.to_string(), "src/lib.rs");
                assert_eq!(line, 12);
            }
            BlameTarget::Node(_) | BlameTarget::Name(_) => panic!("expected path:line"),
        }
        assert!(matches!(
            parse_blame_target("crate::alpha").unwrap(),
            BlameTarget::Name(name) if name == "crate::alpha"
        ));
        let id = node(1);
        assert!(matches!(
            parse_blame_target(&id.to_string()).unwrap(),
            BlameTarget::Node(parsed) if parsed == id
        ));
        assert!(parse_blame_target("src/lib.rs:0").is_err());
        assert!(parse_blame_target("/src/lib.rs:1").is_err());
    }
}
