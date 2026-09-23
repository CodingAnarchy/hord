//! Parse node specs and blame targets; resolution itself is
//! [`hord_txn::Query`] (NodeIds read from the snapshots' identity, ADR 0017).
//!
//! Node ids accept the ULID text [`NodeId`] displays and a 32-digit hex
//! encoding of the same 128 bits.

use anyhow::{Context, Result, bail};
use hord_core::{Actor, NodeId, RepoPath};
use hord_txn::Query;

use crate::txn::block_on;

/// `Actor` id used by `log --actor` and blame output.
pub(crate) fn actor_id(actor: &Actor) -> &str {
    match actor {
        Actor::Human { id } | Actor::Agent { id, .. } => id,
    }
}

/// Parse a ULID or a 32-digit hex NodeId. `None` means the string is not an id.
pub(crate) fn parse_node_id(spec: &str) -> Option<NodeId> {
    if let Ok(id) = spec.parse::<NodeId>() {
        return Some(id);
    }
    parse_node_hex(spec)
}

/// NodeId for `log --node`: an id, or a qualified name.
pub(crate) fn resolve_node_spec(query: &Query, spec: &str) -> Result<NodeId> {
    if let Some(id) = parse_node_id(spec) {
        return Ok(id);
    }
    Ok(block_on(query.resolve_name(spec))?)
}

/// A blame argument after it has been classified.
pub(crate) enum BlameTarget {
    /// ULID or 32-digit hex. History is the store's `node_history`, no scan.
    Node(NodeId),
    /// Qualified name resolved by [`Query::resolve_name`].
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

/// Resolve a blame argument to the definition it names.
pub(crate) fn resolve_blame_target(query: &Query, spec: &str) -> Result<NodeId> {
    Ok(match parse_blame_target(spec)? {
        BlameTarget::Node(id) => id,
        BlameTarget::Name(name) => block_on(query.resolve_name(&name))?,
        BlameTarget::Line { path, line } => block_on(query.resolve_line(path, line))?,
    })
}

fn parse_node_hex(spec: &str) -> Option<NodeId> {
    let spec = spec
        .strip_prefix("0x")
        .or_else(|| spec.strip_prefix("0X"))
        .unwrap_or(spec);
    if spec.len() != 32 {
        return None;
    }
    let bytes = spec.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = hex_byte(bytes[i * 2], bytes[i * 2 + 1])?;
    }
    Some(NodeId::from_u128(u128::from_be_bytes(out)))
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_nibble(hi)? << 4 | hex_nibble(lo)?)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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
    use hord_core::{Actor, Bytes, NodeId};

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

    #[test]
    fn actor_id_is_the_id_field() {
        assert_eq!(actor_id(&Actor::Human { id: "ada".into() }), "ada");
        assert_eq!(
            actor_id(&Actor::Agent {
                id: "agent-7".into(),
                model: "m".into(),
                model_hash: Bytes::default(),
                harness: "h".into(),
            }),
            "agent-7"
        );
    }
}
