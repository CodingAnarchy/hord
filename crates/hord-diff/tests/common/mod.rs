//! Shared test helpers for hord-diff integration tests.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use hord_core::{IdentityDelta, NodeId, ObjectId, Op};
use hord_lang::{IdentifiedTree, IdentityMapping, LangAdapter, NodeTree, default_identify};
use hord_lang_rust::RustAdapter;
use hord_lang_toml::TomlAdapter;

pub fn rust() -> RustAdapter {
    RustAdapter
}

pub fn toml() -> TomlAdapter {
    TomlAdapter
}

pub fn parse_identified<A: LangAdapter>(adapter: &A, src: &[u8]) -> IdentifiedTree {
    let tree = adapter.parse(src).expect("parse");
    let empty = IdentifiedTree::default();
    let mapping = default_identify(adapter, &empty, &tree);
    IdentifiedTree::new(tree, mapping.nodes)
}

/// Identify `result` against `base`, then pair leftover defs by a source
/// label (`fn foo`, `mod bar`, `[table]`).
///
/// Identify `result` against `base`. Named matching uses interned
/// [`hord_core::Node::name`]; leftover defs are rebound by a source label
/// (`fn foo`, `mod bar`, `[table]`) so tests stay robust if a kind has no
/// name.
pub fn identify_result<A: LangAdapter>(
    adapter: &A,
    base: &IdentifiedTree,
    src: &[u8],
) -> (NodeTree, IdentityMapping) {
    let tree = adapter.parse(src).expect("parse result");
    let mapping = identify_labeled(adapter, base, &tree);
    (tree, mapping)
}

pub fn project<A: LangAdapter>(adapter: &A, tree: &NodeTree) -> Vec<u8> {
    adapter.project(tree).into_vec()
}

pub fn def_named(tree: &IdentifiedTree, needle: &str) -> Option<(ObjectId, NodeId)> {
    tree.ids.iter().find_map(|(oid, nid)| {
        let node = tree.tree.get(*oid)?;
        let raw = std::str::from_utf8(node.raw.as_slice()).ok()?;
        raw.contains(needle).then_some((*oid, *nid))
    })
}

pub fn ops_kinds(ops: &[Op]) -> Vec<&'static str> {
    ops.iter()
        .map(|op| match op {
            Op::Insert { .. } => "insert",
            Op::Delete { .. } => "delete",
            Op::Replace { .. } => "replace",
            Op::Move { .. } => "move",
            Op::Rename { .. } => "rename",
            Op::Blob { .. } => "blob",
            Op::Tree { .. } => "tree",
        })
        .collect()
}

pub fn has_kind(ops: &[Op], kind: &str) -> bool {
    ops_kinds(ops).contains(&kind)
}

#[derive(Clone)]
struct Lab {
    oid: ObjectId,
    nid: Option<NodeId>,
    kind: String,
    label: String,
    parent_oid: Option<ObjectId>,
    index: u32,
    normalized: ObjectId,
}

fn identify_labeled<A: LangAdapter>(
    adapter: &A,
    base: &IdentifiedTree,
    result: &NodeTree,
) -> IdentityMapping {
    let mut mapping = adapter.identify(base, result);

    let base_labs = collect_labs(adapter, &base.tree, Some(&base.ids));
    let result_labs = collect_labs(adapter, result, None);
    let base_nids: BTreeSet<NodeId> = base.ids.values().copied().collect();

    let mut used_base: BTreeSet<ObjectId> = mapping
        .nodes
        .values()
        .filter_map(|nid| {
            if !base_nids.contains(nid) {
                return None;
            }
            base.ids
                .iter()
                .find_map(|(oid, id)| (*id == *nid).then_some(*oid))
        })
        .collect();

    // default_identify already inserts every result def (carried or birth).
    // Rebind leftover births onto unmatched base defs by label, then by
    // normalized (moves whose parent def was not identified).
    for r in &result_labs {
        let Some(&nid) = mapping.nodes.get(&r.oid) else {
            continue;
        };
        if base_nids.contains(&nid) {
            continue;
        }
        let found = base_labs.iter().find(|b| {
            !used_base.contains(&b.oid)
                && b.kind == r.kind
                && !r.label.is_empty()
                && b.label == r.label
        });
        if let Some(b) = found
            && let Some(bnid) = b.nid
        {
            used_base.insert(b.oid);
            mapping.nodes.insert(r.oid, bnid);
        }
    }

    for r in &result_labs {
        let Some(&nid) = mapping.nodes.get(&r.oid) else {
            continue;
        };
        if base_nids.contains(&nid) {
            continue;
        }
        let found = base_labs.iter().find(|b| {
            !used_base.contains(&b.oid) && b.kind == r.kind && b.normalized == r.normalized
        });
        if let Some(b) = found
            && let Some(bnid) = b.nid
        {
            used_base.insert(b.oid);
            mapping.nodes.insert(r.oid, bnid);
        }
    }

    let mut deltas = Vec::new();
    let mut seen_births = BTreeSet::new();
    for nid in mapping.nodes.values() {
        if !base_nids.contains(nid) && seen_births.insert(*nid) {
            deltas.push(IdentityDelta::Birth { node: *nid });
        }
    }
    for nid in base.ids.values() {
        if !mapping.nodes.values().any(|id| id == nid) {
            deltas.push(IdentityDelta::Death { node: *nid });
        }
    }
    mapping.deltas = deltas;

    let mut moves = Vec::new();
    for r in &result_labs {
        let Some(&nid) = mapping.nodes.get(&r.oid) else {
            continue;
        };
        let Some(b) = base_labs.iter().find(|b| b.nid == Some(nid)) else {
            continue;
        };
        let from_parent = b.parent_oid.and_then(|p| base.ids.get(&p).copied());
        let to_parent = r.parent_oid.and_then(|p| mapping.nodes.get(&p).copied());
        if from_parent != to_parent
            && let (Some(from_parent), Some(to_parent)) = (from_parent, to_parent)
        {
            moves.push(Op::Move {
                node: nid,
                from_parent,
                to_parent,
                index: r.index,
            });
        }
    }
    mapping.moves = moves;
    mapping
}

fn collect_labs<A: LangAdapter>(
    adapter: &A,
    tree: &NodeTree,
    ids: Option<&BTreeMap<ObjectId, NodeId>>,
) -> Vec<Lab> {
    let Some(root) = tree.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_labs(adapter, tree, ids, root, None, 0, &mut out);
    out
}

fn walk_labs<A: LangAdapter>(
    adapter: &A,
    tree: &NodeTree,
    ids: Option<&BTreeMap<ObjectId, NodeId>>,
    oid: ObjectId,
    parent_def: Option<ObjectId>,
    index: u32,
    out: &mut Vec<Lab>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let mut child_parent = parent_def;
    if adapter.is_definition(&node.kind) {
        out.push(Lab {
            oid,
            nid: ids.and_then(|m| m.get(&oid).copied()),
            kind: node.kind.as_str().to_owned(),
            label: def_label(node.kind.as_str(), node.raw.as_slice()),
            parent_oid: parent_def,
            index,
            normalized: node.normalized,
        });
        child_parent = Some(oid);
    }
    for (i, child) in node.children.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        walk_labs(adapter, tree, ids, *child, child_parent, idx, out);
    }
}

fn def_label(kind: &str, raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match kind {
        "function_item" | "function_signature_item" => ident_after(&s, "fn"),
        "mod_item" => ident_after(&s, "mod"),
        "struct_item" => ident_after(&s, "struct"),
        "enum_item" => ident_after(&s, "enum"),
        "trait_item" => ident_after(&s, "trait"),
        "const_item" => ident_after(&s, "const"),
        "static_item" => ident_after(&s, "static"),
        "type_item" => ident_after(&s, "type"),
        "union_item" => ident_after(&s, "union"),
        "table" => table_name(&s, false),
        "table_array_element" => table_name(&s, true),
        _ => None,
    }
    .unwrap_or_default()
}

fn ident_after(s: &str, kw: &str) -> Option<String> {
    let mut rest = s;
    while let Some(i) = rest.find(kw) {
        let before_ok = i == 0
            || rest
                .as_bytes()
                .get(i - 1)
                .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_');
        let after = i + kw.len();
        let next = rest.as_bytes().get(after);
        if before_ok && next.is_some_and(|b| b.is_ascii_whitespace()) {
            let tail = rest[after..].trim_start();
            let ident: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() {
                return Some(ident);
            }
        }
        rest = &rest[i + kw.len()..];
    }
    None
}

fn table_name(s: &str, array: bool) -> Option<String> {
    let trimmed = s.trim_start();
    let rest = if array {
        trimmed.strip_prefix("[[")?
    } else {
        trimmed.strip_prefix('[')?
    };
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}
