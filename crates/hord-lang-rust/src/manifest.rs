//! Cargo path dependencies whose source is in the snapshot (ADR 0010).
//!
//! A name resolves into another crate only when a manifest links it and that
//! crate's source was parsed. Registry and git dependencies stay unresolved.
//! `workspace = true` takes `path` from `[workspace.dependencies]`. A
//! `[lib] path` on the target package is that crate's root; otherwise the
//! root is `src/lib.rs` or `src/main.rs`.

use std::collections::HashMap;

use hord_core::{ObjectId, RepoPath};
use hord_lang::{LangAdapter, NodeTree, ResolveCtx};
use hord_lang_toml::TomlAdapter;

use crate::resolve::RustFile;

/// One `Cargo.toml` in the snapshot.
pub struct ManifestFile<'a> {
    /// Repository path of the manifest.
    pub path: &'a RepoPath,
    /// Manifest bytes.
    pub bytes: &'a [u8],
}

#[derive(Clone, Debug, Default)]
struct Dep {
    path: Option<String>,
    workspace: bool,
}

#[derive(Clone, Debug, Default)]
struct ParsedManifest {
    dir: String,
    package: bool,
    workspace: bool,
    /// `[lib] path`, relative to this manifest. Absent means `src/lib.rs`.
    lib_path: Option<String>,
    workspace_deps: HashMap<String, Dep>,
    normal: HashMap<String, Dep>,
    dev: HashMap<String, Dep>,
    build: HashMap<String, Dep>,
}

enum TableKind {
    Normal,
    Dev,
    Build,
    WorkspaceDeps,
    Package,
    Workspace,
    Lib,
    Other,
}

pub(crate) fn install_links(
    files: &[RustFile<'_>],
    modules: &[Vec<String>],
    manifests: &[ManifestFile<'_>],
    ctx: &mut ResolveCtx,
) {
    let parsed: Vec<ParsedManifest> = manifests
        .iter()
        .filter_map(|manifest| parse_manifest(manifest.path, manifest.bytes))
        .collect();
    if parsed.is_empty() {
        return;
    }
    let by_dir: HashMap<&str, &ParsedManifest> =
        parsed.iter().map(|m| (m.dir.as_str(), m)).collect();
    for (file, module) in files.iter().zip(modules.iter()) {
        if module.is_empty() {
            continue;
        }
        let module_key = module.join("::");
        if module_key.contains("::") {
            continue;
        }
        let file_path = file.path.to_string();
        let Some(package) = owning_package(&file_path, &by_dir) else {
            continue;
        };
        let workspace = workspace_of(package, &by_dir);
        let deps = deps_for(package, &file_path);
        for (key, dep) in &deps {
            let Some(dir) = resolved_dir(key, dep, package, workspace) else {
                continue;
            };
            let lib_rel = by_dir
                .get(dir.as_str())
                .and_then(|manifest| manifest.lib_path.as_deref());
            let extern_name = key.replace('-', "_");
            for target in target_modules(&dir, lib_rel, files, modules) {
                if target != module_key {
                    ctx.add_link(module_key.as_str(), extern_name.as_str(), target.as_str());
                }
            }
        }
    }
}

fn deps_for(package: &ParsedManifest, file: &str) -> HashMap<String, Dep> {
    let rel = relative(package.dir.as_str(), file);
    if rel == "build.rs" {
        package.build.clone()
    } else if rel.starts_with("tests/")
        || rel.starts_with("examples/")
        || rel.starts_with("benches/")
    {
        let mut deps = package.normal.clone();
        deps.extend(package.dev.clone());
        deps
    } else {
        package.normal.clone()
    }
}

fn resolved_dir(
    key: &str,
    dep: &Dep,
    package: &ParsedManifest,
    workspace: Option<&ParsedManifest>,
) -> Option<String> {
    if let Some(path) = &dep.path {
        return Some(join_dir(&package.dir, path));
    }
    if dep.workspace {
        let workspace = workspace?;
        let inherited = workspace.workspace_deps.get(key)?;
        let path = inherited.path.as_ref()?;
        return Some(join_dir(&workspace.dir, path));
    }
    None
}

fn target_modules(
    dir: &str,
    lib_rel: Option<&str>,
    files: &[RustFile<'_>],
    modules: &[Vec<String>],
) -> Vec<String> {
    let mut wanted = vec![child(dir, "src/lib.rs"), child(dir, "src/main.rs")];
    if let Some(rel) = lib_rel {
        let path = join_dir(dir, rel);
        if !wanted.iter().any(|have| have == &path) {
            wanted.push(path);
        }
    }
    let mut out = Vec::new();
    for (file, module) in files.iter().zip(modules.iter()) {
        if module.is_empty() {
            continue;
        }
        let path = file.path.to_string();
        if !wanted.iter().any(|have| have == &path) {
            continue;
        }
        let key = module.join("::");
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

fn child(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_owned()
    } else {
        format!("{dir}/{rel}")
    }
}

fn relative<'a>(dir: &str, file: &'a str) -> &'a str {
    if dir.is_empty() {
        file
    } else {
        file.strip_prefix(dir)
            .unwrap_or(file)
            .trim_start_matches('/')
    }
}

fn owning_package<'a>(
    file: &str,
    by_dir: &HashMap<&str, &'a ParsedManifest>,
) -> Option<&'a ParsedManifest> {
    let mut dir = parent_dir(file);
    loop {
        if let Some(manifest) = by_dir.get(dir.as_str())
            && manifest.package
        {
            return Some(manifest);
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(&dir);
    }
}

fn workspace_of<'a>(
    package: &ParsedManifest,
    by_dir: &HashMap<&str, &'a ParsedManifest>,
) -> Option<&'a ParsedManifest> {
    let mut dir = package.dir.clone();
    loop {
        if let Some(manifest) = by_dir.get(dir.as_str())
            && manifest.workspace
        {
            return Some(manifest);
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(&dir);
    }
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent.to_owned(),
        None => String::new(),
    }
}

fn join_dir(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').filter(|s| !s.is_empty()).collect()
    };
    if rel.is_empty() {
        return parts.join("/");
    }
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn parse_manifest(path: &RepoPath, bytes: &[u8]) -> Option<ParsedManifest> {
    let tree = TomlAdapter.parse(bytes).ok()?;
    let root = tree.root()?;
    let mut manifest = ParsedManifest {
        dir: parent_dir(&path.to_string()),
        ..ParsedManifest::default()
    };
    let node = tree.get(root)?;
    for child in &node.children {
        let kind = tree.get(*child)?.kind.as_str();
        if kind == "table" || kind == "table_array_element" {
            read_table(&tree, *child, &mut manifest);
        }
    }
    if manifest.package || manifest.workspace || !manifest.workspace_deps.is_empty() {
        Some(manifest)
    } else if manifest.normal.is_empty() && manifest.dev.is_empty() && manifest.build.is_empty() {
        None
    } else {
        Some(manifest)
    }
}

fn read_table(tree: &NodeTree, id: ObjectId, manifest: &mut ParsedManifest) {
    let Some(node) = tree.get(id) else {
        return;
    };
    let mut header = Vec::new();
    let mut pairs = Vec::new();
    for child in &node.children {
        let Some(child_node) = tree.get(*child) else {
            continue;
        };
        let kind = child_node.kind.as_str();
        if kind == "pair" {
            pairs.push(*child);
        } else if header_key(kind) && pairs.is_empty() {
            header.extend(key_parts(tree, *child));
        }
    }
    let (kind, dep_name) = classify(&header);
    if matches!(kind, TableKind::Package) {
        manifest.package = true;
    }
    if matches!(kind, TableKind::Workspace | TableKind::WorkspaceDeps) {
        manifest.workspace = true;
    }
    if matches!(kind, TableKind::Lib) {
        for pair in pairs {
            let Some((field, value)) = pair_field(tree, pair) else {
                continue;
            };
            if field == "path"
                && let Some(path) = string_value(tree, value)
            {
                manifest.lib_path = Some(path);
            }
        }
        return;
    }
    let Some(map) = dep_map(manifest, kind) else {
        return;
    };
    if let Some(name) = dep_name {
        let dep = dep_from_fields(tree, &pairs);
        if dep.path.is_some() || dep.workspace {
            store_dep(map, name, dep);
        }
        return;
    }
    for pair in pairs {
        if let Some((name, dep)) = dep_from_pair(tree, pair) {
            store_dep(map, name, dep);
        }
    }
}

fn dep_map(manifest: &mut ParsedManifest, kind: TableKind) -> Option<&mut HashMap<String, Dep>> {
    match kind {
        TableKind::Normal => Some(&mut manifest.normal),
        TableKind::Dev => Some(&mut manifest.dev),
        TableKind::Build => Some(&mut manifest.build),
        TableKind::WorkspaceDeps => Some(&mut manifest.workspace_deps),
        TableKind::Package | TableKind::Workspace | TableKind::Lib | TableKind::Other => None,
    }
}

/// Dependency table kind, and the dependency name when the header is
/// `[dependencies.name]` (or the workspace / dev / build form of that).
fn classify(header: &[String]) -> (TableKind, Option<String>) {
    if let Some(idx) = header.iter().position(|key| {
        matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    }) {
        let kind = match header[idx].as_str() {
            "dev-dependencies" => TableKind::Dev,
            "build-dependencies" => TableKind::Build,
            "dependencies" if idx > 0 && header[idx - 1] == "workspace" => TableKind::WorkspaceDeps,
            _ => TableKind::Normal,
        };
        let name = if header.len() > idx + 1 {
            Some(header[idx + 1..].join("."))
        } else {
            None
        };
        return (kind, name);
    }
    if header == ["package"] {
        (TableKind::Package, None)
    } else if header == ["lib"] {
        (TableKind::Lib, None)
    } else if header.first().is_some_and(|key| key == "workspace") {
        (TableKind::Workspace, None)
    } else {
        (TableKind::Other, None)
    }
}

fn store_dep(map: &mut HashMap<String, Dep>, name: String, dep: Dep) {
    match map.entry(name) {
        std::collections::hash_map::Entry::Occupied(mut slot) => {
            let existing = slot.get_mut();
            if dep.path.is_some() {
                existing.path = dep.path;
            }
            if dep.workspace {
                existing.workspace = true;
            }
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(dep);
        }
    }
}

fn dep_from_fields(tree: &NodeTree, pairs: &[ObjectId]) -> Dep {
    let mut dep = Dep::default();
    for pair in pairs {
        let Some((field, value)) = pair_field(tree, *pair) else {
            continue;
        };
        apply_field(&mut dep, &field, tree, value);
    }
    dep
}

fn header_key(kind: &str) -> bool {
    matches!(kind, "bare_key" | "dotted_key" | "quoted_key")
}

fn dep_from_pair(tree: &NodeTree, pair: ObjectId) -> Option<(String, Dep)> {
    let node = tree.get(pair)?;
    let mut key = Vec::new();
    let mut value = None;
    for child in &node.children {
        let kind = tree.get(*child)?.kind.as_str();
        if value.is_none() && header_key(kind) && key.is_empty() {
            key = key_parts(tree, *child);
        } else if !header_key(kind) {
            value = Some(*child);
        }
    }
    if key.is_empty() {
        return None;
    }
    let value = value?;
    let mut dep = Dep::default();
    let kind = tree.get(value)?.kind.as_str();
    let name = if kind == "inline_table" {
        read_inline(tree, value, &mut dep);
        key.join(".")
    } else if key.len() >= 2
        && let Some(field) = key
            .last()
            .filter(|part| *part == "workspace" || *part == "path" || *part == "package")
    {
        apply_field(&mut dep, field, tree, value);
        key[..key.len() - 1].join(".")
    } else {
        return None;
    };
    if dep.path.is_none() && !dep.workspace {
        return None;
    }
    Some((name, dep))
}

fn read_inline(tree: &NodeTree, id: ObjectId, dep: &mut Dep) {
    let Some(node) = tree.get(id) else {
        return;
    };
    for child in &node.children {
        let Some((field, value)) = pair_field(tree, *child) else {
            continue;
        };
        apply_field(dep, &field, tree, value);
    }
}

fn pair_field(tree: &NodeTree, pair: ObjectId) -> Option<(String, ObjectId)> {
    let node = tree.get(pair)?;
    if node.kind.as_str() != "pair" {
        return None;
    }
    let mut key = Vec::new();
    let mut value = None;
    for child in &node.children {
        let kind = tree.get(*child)?.kind.as_str();
        if value.is_none() && header_key(kind) && key.is_empty() {
            key = key_parts(tree, *child);
        } else if !header_key(kind) {
            value = Some(*child);
        }
    }
    Some((key.last()?.clone(), value?))
}

fn apply_field(dep: &mut Dep, field: &str, tree: &NodeTree, value: ObjectId) {
    match field {
        "workspace" => dep.workspace = bool_value(tree, value),
        "path" => dep.path = string_value(tree, value),
        _ => {}
    }
}

fn key_parts(tree: &NodeTree, id: ObjectId) -> Vec<String> {
    let Some(node) = tree.get(id) else {
        return Vec::new();
    };
    match node.kind.as_str() {
        "bare_key" | "quoted_key" => vec![plain_text(tree, id)],
        "dotted_key" => {
            let mut parts = Vec::new();
            for child in &node.children {
                parts.extend(key_parts(tree, *child));
            }
            parts
        }
        _ => Vec::new(),
    }
}

fn string_value(tree: &NodeTree, id: ObjectId) -> Option<String> {
    let node = tree.get(id)?;
    if node.kind.as_str() == "inline_table" {
        return None;
    }
    if node.kind.as_str() != "string" {
        return None;
    }
    let mut inner = String::new();
    let mut saw = false;
    for child in &node.children {
        if tree
            .get(*child)
            .is_some_and(|n| n.kind.as_str() == "_content")
        {
            inner.push_str(&plain_text(tree, *child));
            saw = true;
        }
    }
    if saw {
        return Some(inner);
    }
    Some(unquote(&plain_text(tree, id)))
}

fn bool_value(tree: &NodeTree, id: ObjectId) -> bool {
    plain_text(tree, id).trim() == "true"
}

fn plain_text(tree: &NodeTree, id: ObjectId) -> String {
    tree.stripped(id)
        .or_else(|| tree.get(id).map(|n| n.raw.as_slice()))
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        inner.to_owned()
    } else if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        inner.to_owned()
    } else {
        raw.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_manifest;
    use hord_core::RepoPath;
    use std::str::FromStr;

    #[test]
    fn reads_path_and_workspace_inheritance() {
        let src = r#"
[package]
name = "app"

[workspace]
members = ["crates/support"]

[workspace.dependencies]
support = { path = "crates/support" }

[dev-dependencies]
support.workspace = true
registry = "1.0"
"#;
        let path = RepoPath::from_str("Cargo.toml").expect("parse repo path Cargo.toml");
        let manifest = parse_manifest(&path, src.as_bytes()).expect("manifest");
        assert!(manifest.package && manifest.workspace);
        assert_eq!(
            manifest
                .workspace_deps
                .get("support")
                .and_then(|d| d.path.as_deref()),
            Some("crates/support")
        );
        let dev = manifest.dev.get("support").expect("dev dep");
        assert!(dev.workspace);
        assert!(dev.path.is_none());
    }

    #[test]
    fn table_form_path_and_workspace_inheritance() {
        let src = r#"
[workspace.dependencies.support]
path = "crates/support"

[dependencies.support]
workspace = true

[dev-dependencies.helper]
path = "../helper"

[lib]
name = "app"
path = "lib.rs"
"#;
        let path = RepoPath::from_str("crates/app/Cargo.toml")
            .expect("parse repo path crates/app/Cargo.toml");
        let manifest = parse_manifest(&path, src.as_bytes()).expect("manifest");
        assert!(manifest.workspace);
        assert_eq!(
            manifest
                .workspace_deps
                .get("support")
                .and_then(|dep| dep.path.as_deref()),
            Some("crates/support")
        );
        let normal = manifest.normal.get("support").expect("normal dep");
        assert!(normal.workspace);
        assert_eq!(
            manifest
                .dev
                .get("helper")
                .and_then(|dep| dep.path.as_deref()),
            Some("../helper")
        );
        assert_eq!(manifest.lib_path.as_deref(), Some("lib.rs"));
    }

    #[test]
    fn registry_git_and_version_deps_are_not_links() {
        let src = r#"
[dependencies]
serde = "1.0"
gix = { git = "https://example.com/gix.git" }
local = { path = "crates/local" }

[target.'cfg(unix)'.dependencies]
libc = "0.2"
"#;
        let path = RepoPath::from_str("Cargo.toml").expect("parse repo path Cargo.toml");
        let manifest = parse_manifest(&path, src.as_bytes()).expect("manifest");
        assert!(!manifest.normal.contains_key("serde"));
        assert!(!manifest.normal.contains_key("gix"));
        assert!(!manifest.normal.contains_key("libc"));
        assert_eq!(
            manifest
                .normal
                .get("local")
                .and_then(|dep| dep.path.as_deref()),
            Some("crates/local")
        );
    }
}
