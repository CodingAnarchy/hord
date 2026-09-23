//! Syntactic reference recall (ADR 0011).
//!
//! The walk is the snapshot's tree-sitter CST, not a call into the resolver.
//! A site counts when source names a sampled definition without types or
//! macro expansion:
//! - a path whose segments are that definition's module path (`crate`, `self`,
//!   `super`, a path-dependency prefix, or a module walked up from here);
//! - `Type::method` / `Self::method` in this crate, or `extern::Type::method`
//!   for a linked package;
//! - a bare call, which names every visible same-named function or method;
//! - a selector (`.name`), which names every visible same-named method.
//!
//! An outer attribute is a child of the next definition, so a name written
//! on it is a site of that definition. Names that exist only after macro
//! expansion are not nodes of these kinds and are not in the denominator.

use std::collections::{HashMap, HashSet};

use hord_core::{NodeId, ObjectId};
use hord_lang::LangAdapter;
use hord_lang::NodeTree;
use hord_lang_rust::{ManifestFile, RustAdapter, RustFile};

use crate::snapshot::{FileSnap, ManifestSnap, Sampled};

pub(crate) struct SyntacticReport {
    pub labeled: usize,
    pub expected: usize,
    pub hit: usize,
}

pub(crate) fn measure(
    files: &[FileSnap],
    manifests: &[ManifestSnap],
    sample: &[Sampled],
) -> SyntacticReport {
    let adapter = RustAdapter;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|file| RustFile {
            path: &file.path,
            tree: &file.tree,
            ids: &file.ids,
        })
        .collect();
    let file_modules = adapter.file_modules(&views);
    let manifest_views: Vec<ManifestFile<'_>> = manifests
        .iter()
        .map(|manifest| ManifestFile {
            path: &manifest.path,
            bytes: &manifest.bytes,
        })
        .collect();
    let links = adapter.manifest_links(&views, &manifest_views);

    let mut visible: HashMap<String, HashSet<String>> = HashMap::new();
    let mut externs: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (from, name, target) in &links {
        let from_crate = crate_of(from);
        visible
            .entry(from_crate.clone())
            .or_default()
            .insert(crate_of(target));
        externs
            .entry(from_crate)
            .or_default()
            .push((name.clone(), target.clone()));
    }

    let mut metas: HashMap<(usize, ObjectId), FunMeta> = HashMap::new();
    let mut modules: HashSet<String> = HashSet::new();
    for (file_i, file) in files.iter().enumerate() {
        let mut module = split_mod(file_modules.get(file_i).map(String::as_str).unwrap_or(""));
        remember_module(&module, &mut modules);
        if let Some(root) = file.tree.root() {
            index_fns(
                &file.tree,
                root,
                &mut module,
                &mut Vec::new(),
                file_i,
                &mut modules,
                &mut metas,
            );
        }
    }

    let mut by_name: HashMap<String, Vec<SampledDef>> = HashMap::new();
    let mut labeled = 0usize;
    for def in sample {
        let Some(file_i) = files.iter().position(|f| f.path.to_string() == def.path) else {
            continue;
        };
        if !metas.contains_key(&(file_i, def.oid)) {
            continue;
        }
        let Some(node_id) = files[file_i].ids.get(&def.oid).copied() else {
            continue;
        };
        labeled += 1;
        let Some(simple) = simple_name(Some(def.qname.as_str())) else {
            continue;
        };
        if simple.len() < 4 {
            continue;
        }
        let meta = &metas[&(file_i, def.oid)];
        by_name.entry(simple).or_default().push(SampledDef {
            node_id,
            crate_key: meta.crate_key.clone(),
            full: meta.full.clone(),
            is_method: meta.is_method,
            type_keys: meta.type_keys.clone(),
        });
    }

    let mut sites = Vec::new();
    for (file_i, file) in files.iter().enumerate() {
        let module = split_mod(file_modules.get(file_i).map(String::as_str).unwrap_or(""));
        let here = module.first().cloned().unwrap_or_default();
        let mut can_see = visible.get(&here).cloned().unwrap_or_default();
        can_see.insert(here.clone());
        let Some(root) = file.tree.root() else {
            continue;
        };
        let externs_here = externs.get(&here).map(Vec::as_slice).unwrap_or(&[]);
        let mut walk = Walk {
            tree: &file.tree,
            file_i,
            file_root: root,
            can_see: &can_see,
            externs: externs_here,
            modules: &modules,
            by_name: &by_name,
            sites: &mut sites,
            defs: Vec::new(),
            parents: Vec::new(),
            module,
            self_type: Vec::new(),
        };
        walk.visit(root);
    }

    let ctx = adapter.resolve_context_with(&views, &manifest_views);
    let mut resolved_cache: HashMap<(usize, ObjectId), HashSet<NodeId>> = HashMap::new();
    let mut hit = 0usize;
    let mut by_class: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut miss_lines = Vec::new();
    for site in &sites {
        let file = &files[site.file_i];
        let resolved = resolved_cache
            .entry((site.file_i, site.enclosing))
            .or_insert_with(|| {
                let Some(node) = file.tree.get(site.enclosing) else {
                    return HashSet::new();
                };
                adapter
                    .references(&ctx, node)
                    .into_iter()
                    .filter_map(|name| name.resolved)
                    .collect()
            });
        let class = by_class.entry(site.class).or_default();
        class.1 += 1;
        if resolved.contains(&site.target) {
            hit += 1;
            class.0 += 1;
        } else if miss_lines.len() < 40 {
            miss_lines.push(format!(
                "[references] syntactic miss {} `{}` in {}",
                site.class, site.text, file.path
            ));
        }
    }
    let ratio_ok = !sites.is_empty() && hit * 100 >= sites.len() * 95;
    if !ratio_ok {
        for line in &miss_lines {
            eprintln!("{line}");
        }
    }
    if !by_class.is_empty() {
        let mut parts: Vec<_> = by_class.into_iter().collect();
        parts.sort_by(|a, b| a.0.cmp(b.0));
        let shown = parts
            .iter()
            .map(|(class, (ok_n, total))| format!("{class} {ok_n}/{total}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("[references] syntactic parts {shown}");
    }

    SyntacticReport {
        labeled,
        expected: sites.len(),
        hit,
    }
}

pub(crate) fn ok(report: &SyntacticReport, sample: usize) -> bool {
    report.labeled >= sample && report.expected > 0 && report.hit * 100 >= report.expected * 95
}

struct FunMeta {
    is_method: bool,
    full: Vec<String>,
    type_keys: Vec<String>,
    crate_key: String,
}

struct SampledDef {
    node_id: NodeId,
    crate_key: String,
    full: Vec<String>,
    is_method: bool,
    type_keys: Vec<String>,
}

struct Site {
    file_i: usize,
    enclosing: ObjectId,
    target: NodeId,
    class: &'static str,
    text: String,
}

struct Walk<'a> {
    tree: &'a NodeTree,
    file_i: usize,
    file_root: ObjectId,
    can_see: &'a HashSet<String>,
    externs: &'a [(String, String)],
    modules: &'a HashSet<String>,
    by_name: &'a HashMap<String, Vec<SampledDef>>,
    sites: &'a mut Vec<Site>,
    defs: Vec<ObjectId>,
    parents: Vec<String>,
    module: Vec<String>,
    self_type: Vec<String>,
}

impl Walk<'_> {
    fn visit(&mut self, oid: ObjectId) {
        let Some(node) = self.tree.get(oid) else {
            return;
        };
        let kind = node.kind.as_str().to_owned();
        let name = node.name.as_ref().map(|n| n.as_str().to_owned());
        let children = node.children.clone();

        let is_def = is_definition(&kind);
        if is_def {
            self.defs.push(oid);
        }
        let pushed_mod = if kind == "mod_item" {
            mod_local(name.as_deref())
        } else {
            None
        };
        if let Some(local) = &pushed_mod {
            self.module.push(local.clone());
        }
        let pushed_self = match kind.as_str() {
            "impl_item" => name.as_deref().and_then(impl_self_type),
            "trait_item" => name
                .as_deref()
                .and_then(|n| n.rsplit("::").next())
                .filter(|s| !s.is_empty() && !s.contains(' '))
                .map(str::to_owned),
            _ => None,
        };
        if let Some(ty) = &pushed_self {
            self.self_type.push(ty.clone());
        }

        if kind == "scoped_identifier" || kind == "scoped_type_identifier" {
            self.expect_path(oid);
        } else if !self.parent_is_declarator()
            && (kind == "field_identifier"
                || (kind == "identifier" && self.parent_is("shorthand_field_initializer")))
        {
            self.expect_selector(oid);
        } else if kind == "identifier"
            && (self.parent_is("call_expression") || self.parent_is("generic_function"))
        {
            self.expect_call(oid);
        } else {
            self.parents.push(kind);
            for child in children {
                self.visit(child);
            }
            self.parents.pop();
        }

        if pushed_self.is_some() {
            self.self_type.pop();
        }
        if pushed_mod.is_some() {
            self.module.pop();
        }
        if is_def {
            self.defs.pop();
        }
    }

    fn parent_is(&self, kind: &str) -> bool {
        self.parents.last().is_some_and(|parent| parent == kind)
    }

    /// The name token of a definition is not a use of that name.
    fn parent_is_declarator(&self) -> bool {
        self.parents.last().is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "field_declaration"
                    | "enum_variant"
                    | "function_item"
                    | "function_signature_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "mod_item"
                    | "type_item"
                    | "const_item"
                    | "static_item"
                    | "macro_definition"
            )
        })
    }

    fn enclosing(&self) -> ObjectId {
        self.defs.last().copied().unwrap_or(self.file_root)
    }

    fn node_text(&self, oid: ObjectId) -> Option<String> {
        let bytes = self.tree.stripped(oid)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    fn push(&mut self, target: NodeId, class: &'static str, text: &str) {
        let mut shown = text.trim().to_owned();
        if shown.len() > 80 {
            shown.truncate(80);
        }
        self.sites.push(Site {
            file_i: self.file_i,
            enclosing: self.enclosing(),
            target,
            class,
            text: shown,
        });
    }

    fn expect_path(&mut self, oid: ObjectId) {
        let Some(text) = self.node_text(oid) else {
            return;
        };
        let segments = split_path(&text);
        if segments.len() < 2 {
            return;
        }
        let Some(simple) = segments.last() else {
            return;
        };
        if simple.len() < 4 {
            return;
        }
        let Some(defs) = self.by_name.get(simple) else {
            return;
        };
        let paths = absolute_paths(&segments, &self.module, self.modules, self.externs);
        let current = self.module.first().cloned().unwrap_or_default();
        let self_ty = self.self_type.last().cloned();
        let mut chosen = Vec::new();
        let mut seen = HashSet::new();
        for def in defs {
            if !self.can_see.contains(&def.crate_key) {
                continue;
            }
            let named = if def.is_method {
                names_method(&segments, def, &current, self_ty.as_deref(), self.externs)
            } else {
                paths.iter().any(|path| path == &def.full)
            };
            if named && seen.insert(def.node_id) {
                chosen.push(def.node_id);
            }
        }
        for target in chosen {
            self.push(target, "path", &text);
        }
    }

    fn expect_call(&mut self, oid: ObjectId) {
        let Some(text) = self.node_text(oid) else {
            return;
        };
        let simple = bare_name(&text);
        if simple.len() < 4 {
            return;
        }
        let Some(defs) = self.by_name.get(simple) else {
            return;
        };
        let mut chosen = Vec::new();
        let mut seen = HashSet::new();
        for def in defs {
            if self.can_see.contains(&def.crate_key) && seen.insert(def.node_id) {
                chosen.push(def.node_id);
            }
        }
        for target in chosen {
            self.push(target, "call", simple);
        }
    }

    fn expect_selector(&mut self, oid: ObjectId) {
        let Some(text) = self.node_text(oid) else {
            return;
        };
        let simple = bare_name(&text);
        if simple.len() < 4 {
            return;
        }
        let Some(defs) = self.by_name.get(simple) else {
            return;
        };
        let mut chosen = Vec::new();
        let mut seen = HashSet::new();
        for def in defs {
            if def.is_method && self.can_see.contains(&def.crate_key) && seen.insert(def.node_id) {
                chosen.push(def.node_id);
            }
        }
        for target in chosen {
            self.push(target, "selector", simple);
        }
    }
}

fn index_fns(
    tree: &NodeTree,
    oid: ObjectId,
    module: &mut Vec<String>,
    ancestors: &mut Vec<String>,
    file_i: usize,
    modules: &mut HashSet<String>,
    metas: &mut HashMap<(usize, ObjectId), FunMeta>,
) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    let kind = node.kind.as_str().to_owned();
    let name = node.name.as_ref().map(|n| n.as_str().to_owned());
    let children = node.children.clone();
    if kind == "function_item"
        && let Some(simple) = simple_name(name.as_deref())
    {
        let is_method = method_flag(ancestors);
        let mut full = module.clone();
        full.push(simple);
        let type_keys = if is_method {
            type_keys(name.as_deref().unwrap_or(""))
        } else {
            Vec::new()
        };
        metas.insert(
            (file_i, oid),
            FunMeta {
                is_method,
                full,
                type_keys,
                crate_key: module.first().cloned().unwrap_or_default(),
            },
        );
    }
    ancestors.push(kind.clone());
    let mut pushed_mod = false;
    if kind == "mod_item"
        && let Some(local) = mod_local(name.as_deref())
    {
        module.push(local);
        remember_module(module, modules);
        pushed_mod = true;
    }
    for child in children {
        index_fns(tree, child, module, ancestors, file_i, modules, metas);
    }
    if pushed_mod {
        module.pop();
    }
    ancestors.pop();
}

fn crate_of(module: &str) -> String {
    module.split("::").next().unwrap_or(module).to_owned()
}

fn split_mod(module: &str) -> Vec<String> {
    module
        .split("::")
        .filter(|seg| !seg.is_empty())
        .map(str::to_owned)
        .collect()
}

fn remember_module(segs: &[String], modules: &mut HashSet<String>) {
    let mut acc = String::new();
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            acc.push_str("::");
        }
        acc.push_str(seg);
        modules.insert(acc.clone());
    }
}

fn mod_local(name: Option<&str>) -> Option<String> {
    let local = name?.rsplit("::").next().unwrap_or("");
    if local.is_empty() || local.contains(' ') {
        None
    } else {
        Some(local.to_owned())
    }
}

fn simple_name(name: Option<&str>) -> Option<String> {
    let simple = name?.rsplit("::").next().unwrap_or("").trim();
    if simple.is_empty() || simple.contains(' ') {
        None
    } else {
        Some(simple.to_owned())
    }
}

fn bare_name(text: &str) -> &str {
    text.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
}

fn is_definition(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "impl_item"
            | "trait_item"
            | "mod_item"
    )
}

/// True when a function sits inside an impl or trait, including a function
/// nested in a method. The resolver keeps that impl or trait as the parent
/// for the whole body, so a selector can name the nested function too.
fn method_flag(ancestors: &[String]) -> bool {
    ancestors
        .iter()
        .any(|kind| kind == "impl_item" || kind == "trait_item")
}

fn type_keys(qname: &str) -> Vec<String> {
    let Some((owner, _)) = qname.rsplit_once("::") else {
        return Vec::new();
    };
    let tail = owner.rsplit("::").next().unwrap_or(owner);
    let mut keys = Vec::new();
    // `Display for Point::fmt` is named by `Point::fmt`, not `Display::fmt`.
    // The trait path needs trait resolution, which is outside this oracle.
    if let Some((_, ty)) = tail.split_once(" for ") {
        push_key(&mut keys, ty);
        return keys;
    }
    if let Some(ty) = tail.strip_prefix("impl ") {
        push_key(&mut keys, ty);
        return keys;
    }
    push_key(&mut keys, tail);
    keys
}

fn impl_self_type(qname: &str) -> Option<String> {
    let tail = qname.rsplit("::").next().unwrap_or(qname);
    if let Some((_, ty)) = tail.split_once(" for ") {
        let key = last_type_seg(ty);
        return (!key.is_empty()).then_some(key);
    }
    let ty = tail.strip_prefix("impl ")?;
    let key = last_type_seg(ty);
    (!key.is_empty()).then_some(key)
}

fn push_key(keys: &mut Vec<String>, ty: &str) {
    let key = last_type_seg(ty);
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

fn last_type_seg(ty: &str) -> String {
    let mut s = ty.trim();
    s = s.trim_start_matches('&').trim();
    s = s.trim_start_matches("mut ").trim();
    s = s.trim_start_matches("dyn ").trim();
    s = s.trim_start_matches("impl ").trim();
    if let Some(i) = s.find(['<', ' ', '&']) {
        s = s[..i].trim();
    }
    s.rsplit("::").next().unwrap_or(s).trim().to_owned()
}

fn split_path(text: &str) -> Vec<String> {
    text.split("::")
        .map(|seg| {
            seg.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .to_owned()
        })
        .filter(|seg| !seg.is_empty())
        .collect()
}

fn names_method(
    segments: &[String],
    def: &SampledDef,
    current_crate: &str,
    self_type: Option<&str>,
    externs: &[(String, String)],
) -> bool {
    if !def.is_method {
        return false;
    }
    if segments.len() == 2 && segments[0] == "Self" {
        return def.crate_key == current_crate
            && self_type.is_some_and(|ty| def.type_keys.iter().any(|key| key == ty));
    }
    if segments.len() == 2 {
        return def.crate_key == current_crate
            && def.type_keys.iter().any(|key| key == &segments[0]);
    }
    if segments.len() == 3 {
        let linked = externs
            .iter()
            .any(|(name, target)| name == &segments[0] && crate_of(target) == def.crate_key);
        return linked && def.type_keys.iter().any(|key| key == &segments[1]);
    }
    false
}

/// Module paths a `::` path can name from `current`.
fn absolute_paths(
    segments: &[String],
    current: &[String],
    modules: &HashSet<String>,
    externs: &[(String, String)],
) -> Vec<Vec<String>> {
    if segments.is_empty() {
        return Vec::new();
    }
    match segments[0].as_str() {
        "crate" => {
            let Some(key) = current.first() else {
                return Vec::new();
            };
            let mut path = vec![key.clone()];
            path.extend(segments.iter().skip(1).cloned());
            vec![path]
        }
        "self" => {
            let mut path = current.to_vec();
            path.extend(segments.iter().skip(1).cloned());
            vec![path]
        }
        "super" => super_path(segments, current).into_iter().collect(),
        _ => unqualified(segments, current, modules, externs),
    }
}

fn super_path(segments: &[String], current: &[String]) -> Option<Vec<String>> {
    let mut segs = current.to_vec();
    let mut i = 0usize;
    while segments.get(i).map(String::as_str) == Some("super") {
        if segs.len() <= 1 {
            return None;
        }
        segs.pop();
        i += 1;
    }
    segs.extend(segments.iter().skip(i).cloned());
    Some(segs)
}

fn unqualified(
    segments: &[String],
    current: &[String],
    modules: &HashSet<String>,
    externs: &[(String, String)],
) -> Vec<Vec<String>> {
    if segments.len() < 2 {
        return Vec::new();
    }
    let mut base = current.to_vec();
    loop {
        let mut parent = base.clone();
        parent.extend(segments[..segments.len() - 1].iter().cloned());
        if modules.contains(&parent.join("::")) {
            if let Some(name) = segments.last() {
                parent.push(name.clone());
            }
            return vec![parent];
        }
        if base.len() <= 1 {
            break;
        }
        base.pop();
    }
    let mut out = Vec::new();
    for (name, target) in externs {
        if name == &segments[0] {
            let mut path = split_mod(target);
            path.extend(segments.iter().skip(1).cloned());
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn type_keys_of_methods() {
        assert_eq!(type_keys("impl Point::layout"), vec!["Point"]);
        assert_eq!(type_keys("foo::impl Point::layout"), vec!["Point"]);
        assert_eq!(type_keys("Display for Point::fmt"), vec!["Point"]);
        assert_eq!(type_keys("MyTrait::method"), vec!["MyTrait"]);
        assert_eq!(type_keys("impl Point<T>::layout"), vec!["Point"]);
        assert_eq!(impl_self_type("Display for Point"), Some("Point".into()));
        assert_eq!(impl_self_type("impl &mut Point"), Some("Point".into()));
    }

    #[test]
    fn crate_self_and_super_paths() {
        let current = segs(&["crate", "cmd"]);
        let modules = HashSet::new();
        assert_eq!(
            absolute_paths(&segs(&["crate", "util", "build"]), &current, &modules, &[]),
            vec![segs(&["crate", "util", "build"])]
        );
        assert_eq!(
            absolute_paths(&segs(&["super", "build"]), &current, &modules, &[]),
            vec![segs(&["crate", "build"])]
        );
        assert_eq!(
            absolute_paths(&segs(&["self", "build"]), &current, &modules, &[]),
            vec![segs(&["crate", "cmd", "build"])]
        );
        assert!(
            absolute_paths(&segs(&["super", "build"]), &segs(&["crate"]), &modules, &[]).is_empty()
        );
    }

    #[test]
    fn walk_up_finds_an_ancestor_module() {
        let mut modules = HashSet::new();
        remember_module(&segs(&["crate", "util"]), &mut modules);
        let got = absolute_paths(
            &segs(&["util", "build"]),
            &segs(&["crate", "cmd"]),
            &modules,
            &[],
        );
        assert_eq!(got, vec![segs(&["crate", "util", "build"])]);
    }

    #[test]
    fn extern_path_when_no_module_matches() {
        let modules = HashSet::from(["crate".to_owned()]);
        let externs = [("support".to_owned(), "crates/support/src/lib".to_owned())];
        let got = absolute_paths(
            &segs(&["support", "project"]),
            &segs(&["tests/testsuite/main"]),
            &modules,
            &externs,
        );
        assert_eq!(got, vec![segs(&["crates/support/src/lib", "project"])]);
    }

    #[test]
    fn local_module_wins_over_an_extern_of_the_same_name() {
        let mut modules = HashSet::new();
        remember_module(&segs(&["crate", "support"]), &mut modules);
        let externs = [("support".to_owned(), "crates/support/src/lib".to_owned())];
        let got = absolute_paths(
            &segs(&["support", "project"]),
            &segs(&["crate"]),
            &modules,
            &externs,
        );
        assert_eq!(got, vec![segs(&["crate", "support", "project"])]);
    }
}
