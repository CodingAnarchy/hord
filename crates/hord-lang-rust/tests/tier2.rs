//! Tier 2 Rust adapter: qualified names, module resolution, references, tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use hord_core::{Node, NodeId, ObjectId, RepoPath};
use hord_lang::{LangAdapter, NameRef, NodeTree};
use hord_lang_rust::{ManifestFile, RustAdapter, RustFile};

struct Owned {
    path: RepoPath,
    tree: NodeTree,
    ids: BTreeMap<ObjectId, NodeId>,
}

fn adapter() -> RustAdapter {
    RustAdapter
}

fn parse_one(path: &str, src: &str) -> Owned {
    let tree = adapter().parse(src.as_bytes()).expect("parse");
    let ids = assign_ids(&tree);
    Owned {
        path: RepoPath::from_str(path).unwrap(),
        tree,
        ids,
    }
}

fn assign_ids(tree: &NodeTree) -> BTreeMap<ObjectId, NodeId> {
    let mut ids = BTreeMap::new();
    let mut n = 1u128;
    if let Some(root) = tree.root() {
        assign_walk(tree, root, &mut ids, &mut n);
    }
    ids
}

fn assign_walk(tree: &NodeTree, oid: ObjectId, ids: &mut BTreeMap<ObjectId, NodeId>, n: &mut u128) {
    let Some(node) = tree.get(oid) else {
        return;
    };
    if adapter().is_definition(&node.kind) {
        ids.insert(oid, NodeId::from_u128(*n));
        *n += 1;
    }
    for child in node.children.clone() {
        assign_walk(tree, child, ids, n);
    }
}

fn context(files: &[Owned]) -> hord_lang::ResolveCtx {
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|f| RustFile {
            path: &f.path,
            tree: &f.tree,
            ids: &f.ids,
        })
        .collect();
    adapter().resolve_context(&views)
}

fn find<'a>(file: &'a Owned, qualified: &str) -> &'a Node {
    file.tree
        .iter()
        .map(|(_, node)| node)
        .find(|node| {
            node.name
                .as_ref()
                .is_some_and(|name| name.as_str() == qualified)
        })
        .unwrap_or_else(|| panic!("no definition {qualified} in {}", file.path))
}

fn id_of(file: &Owned, want: &str) -> NodeId {
    id_of_kind(file, want, None)
}

fn id_of_kind(file: &Owned, want: &str, kind: Option<&str>) -> NodeId {
    let node = file
        .tree
        .iter()
        .map(|(_, node)| node)
        .find(|node| {
            node.name.as_ref().is_some_and(|name| name.as_str() == want)
                && kind.is_none_or(|k| node.kind.as_str() == k)
        })
        .unwrap_or_else(|| panic!("no definition {want} kind {kind:?} in {}", file.path));
    let oid = ObjectId::of(node).expect("object id");
    *file
        .ids
        .get(&oid)
        .unwrap_or_else(|| panic!("no id for {want}"))
}

/// Give every definition in `files` a distinct [`NodeId`].
///
/// [`parse_one`] numbers each file from 1, so the same position in two files
/// shares an id. Cross-file ambiguity checks need ids that do not collide.
fn reassign_ids(files: &mut [Owned]) {
    let mut n = 1u128;
    for file in files {
        file.ids.clear();
        if let Some(root) = file.tree.root() {
            assign_walk(&file.tree, root, &mut file.ids, &mut n);
        }
    }
}

fn resolved(ctx: &hord_lang::ResolveCtx, node: &Node) -> BTreeSet<NodeId> {
    adapter()
        .references(ctx, node)
        .into_iter()
        .filter_map(|name| name.resolved)
        .collect()
}

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

#[test]
fn every_fixture_definition_has_a_qualified_name() {
    let adapter = adapter();
    let mut files = 0;
    for entry in std::fs::read_dir(testdata()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let tree = adapter.parse(&bytes).unwrap();
        let Some(root) = tree.root() else {
            continue;
        };
        let mut ancestors = Vec::new();
        check_names(&adapter, &tree, root, &mut ancestors);
        files += 1;
    }
    assert!(files >= 5);
}

fn check_names(
    adapter: &RustAdapter,
    tree: &NodeTree,
    oid: ObjectId,
    ancestors: &mut Vec<ObjectId>,
) {
    let node = tree.get(oid).unwrap().clone();
    if adapter.is_definition(&node.kind) {
        let owned: Vec<Node> = ancestors
            .iter()
            .map(|id| tree.get(*id).unwrap().clone())
            .collect();
        let path: Vec<&Node> = owned.iter().collect();
        let named = adapter.qualified_name(&path, &node);
        assert_eq!(
            named.as_ref().map(|q| q.as_str().to_owned()),
            node.name.as_ref().map(|q| q.as_str().to_owned()),
            "qualified_name drifted for {}",
            node.kind
        );
        assert!(
            named.is_some(),
            "definition {} has no qualified name\n{}",
            node.kind,
            String::from_utf8_lossy(&node.raw)
        );
        let mut stripped = node.clone();
        stripped.name = None;
        let rebuilt = adapter.qualified_name(&path, &stripped);
        assert_eq!(
            rebuilt.as_ref().map(|q| q.as_str().to_owned()),
            node.name.as_ref().map(|q| q.as_str().to_owned()),
            "rebuilt name for {}\n{}",
            node.kind,
            String::from_utf8_lossy(&node.raw)
        );
    }
    ancestors.push(oid);
    for child in node.children {
        check_names(adapter, tree, child, ancestors);
    }
    ancestors.pop();
}

#[test]
fn path_dependency_resolves_across_crates() {
    let lib = parse_one("crates/support/src/lib.rs", "pub fn project() {}\n");
    let test = parse_one(
        "tests/testsuite/main.rs",
        "fn caller() { support::project(); }\n",
    );
    let mut files = [lib, test];
    reassign_ids(&mut files);
    let manifest_path = RepoPath::from_str("Cargo.toml").unwrap();
    let manifest = r#"
[package]
name = "app"
[workspace]
members = ["crates/support"]
[workspace.dependencies]
support = { path = "crates/support" }
[dev-dependencies]
support.workspace = true
"#;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|f| RustFile {
            path: &f.path,
            tree: &f.tree,
            ids: &f.ids,
        })
        .collect();
    let manifests = [ManifestFile {
        path: &manifest_path,
        bytes: manifest.as_bytes(),
    }];
    let ctx = adapter().resolve_context_with(&views, &manifests);
    let project = id_of(&files[0], "project");
    let refs = adapter().references(&ctx, find(&files[1], "caller"));
    let edge = refs
        .iter()
        .find(|r| r.name.as_str() == "support::project")
        .expect("edge");
    assert_eq!(edge.resolved, Some(project));
}

#[test]
fn linked_crate_method_is_a_candidate() {
    let lib = parse_one(
        "crates/support/src/lib.rs",
        "pub struct Foo;\nimpl Foo { pub fn layout(&self) {} }\n",
    );
    let test = parse_one(
        "tests/testsuite/main.rs",
        "fn caller() { let x = (); x.layout(); }\n",
    );
    let mut files = [lib, test];
    reassign_ids(&mut files);
    let manifest_path = RepoPath::from_str("Cargo.toml").unwrap();
    let manifest = r#"
[package]
name = "app"
[workspace]
members = ["crates/support"]
[workspace.dependencies]
support = { path = "crates/support" }
[dev-dependencies]
support.workspace = true
"#;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|f| RustFile {
            path: &f.path,
            tree: &f.tree,
            ids: &f.ids,
        })
        .collect();
    let ctx = adapter().resolve_context_with(
        &views,
        &[ManifestFile {
            path: &manifest_path,
            bytes: manifest.as_bytes(),
        }],
    );
    let layout = id_of(&files[0], "impl Foo::layout");
    let refs = adapter().references(&ctx, find(&files[1], "caller"));
    assert!(refs.iter().any(|r| r.resolved == Some(layout)), "{refs:?}");
}

#[test]
fn unresolved_call_includes_linked_crate_functions() {
    let lib = parse_one("crates/support/src/lib.rs", "pub fn project_layout() {}\n");
    let test = parse_one(
        "tests/testsuite/main.rs",
        "fn caller() { project_layout(); }\n",
    );
    let mut files = [lib, test];
    reassign_ids(&mut files);
    let manifest_path = RepoPath::from_str("Cargo.toml").unwrap();
    let manifest = r#"
[package]
name = "app"
[workspace]
members = ["crates/support"]
[workspace.dependencies]
support = { path = "crates/support" }
[dev-dependencies]
support.workspace = true
"#;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|f| RustFile {
            path: &f.path,
            tree: &f.tree,
            ids: &f.ids,
        })
        .collect();
    let ctx = adapter().resolve_context_with(
        &views,
        &[ManifestFile {
            path: &manifest_path,
            bytes: manifest.as_bytes(),
        }],
    );
    let project = id_of(&files[0], "project_layout");
    let refs = adapter().references(&ctx, find(&files[1], "caller"));
    assert!(refs.iter().any(|r| r.resolved == Some(project)), "{refs:?}");
}

#[test]
fn nested_function_call_is_a_candidate() {
    let src = r#"
#[track_caller]
pub fn assert_deps() {
    let deps = (0..1)
        .map(|_| {
            let _file_len = read_u64();
            1
        })
        .collect::<Vec<_>>();
    let _ = deps;
    fn read_u64() -> u64 {
        0
    }
}
"#;
    let mut files = [parse_one("src/lib.rs", src)];
    reassign_ids(&mut files);
    let ctx = context(&files);
    let target = id_of(&files[0], "read_u64");
    let refs = adapter().references(&ctx, find(&files[0], "assert_deps"));
    assert!(refs.iter().any(|r| r.resolved == Some(target)), "{refs:?}");
}

#[test]
fn unresolved_call_includes_same_crate_functions_outside_the_module() {
    let src = "mod inner { pub fn project_layout() {} }\nfn caller() { project_layout(); }\n";
    let mut files = [parse_one("src/lib.rs", src)];
    reassign_ids(&mut files);
    let ctx = context(&files);
    let project = id_of(&files[0], "inner::project_layout");
    let refs = adapter().references(&ctx, find(&files[0], "caller"));
    assert!(refs.iter().any(|r| r.resolved == Some(project)), "{refs:?}");
}

#[test]
fn outer_attribute_belongs_to_the_next_definition() {
    let files = [parse_one("src/lib.rs", "#[foo::bar]\nfn qux() {}\n")];
    let node = find(&files[0], "qux");
    let raw = String::from_utf8_lossy(node.raw.as_slice());
    assert!(raw.contains("foo"), "{raw}");
    let ctx = context(&files);
    let refs = adapter().references(&ctx, node);
    assert!(
        refs.iter().any(|r| r.name.as_str() == "foo::bar"),
        "{refs:?}"
    );
}

#[test]
fn registry_dependency_stays_unresolved() {
    let lib = parse_one("src/lib.rs", "fn caller() { serde::json(); }\n");
    let files = [lib];
    let manifest_path = RepoPath::from_str("Cargo.toml").unwrap();
    let manifest = r#"
[package]
name = "app"
[dependencies]
serde = "1.0"
"#;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|f| RustFile {
            path: &f.path,
            tree: &f.tree,
            ids: &f.ids,
        })
        .collect();
    let ctx = adapter().resolve_context_with(
        &views,
        &[ManifestFile {
            path: &manifest_path,
            bytes: manifest.as_bytes(),
        }],
    );
    let refs = adapter().references(&ctx, find(&files[0], "caller"));
    let edge = refs.iter().find(|r| r.name.as_str() == "serde::json");
    assert!(edge.is_none_or(|r| r.resolved.is_none()), "{refs:?}");
}

#[test]
fn use_resolves_across_files() {
    let files = vec![
        parse_one(
            "src/lib.rs",
            "mod foo;\nuse foo::helper;\n\nfn caller() {\n    helper();\n}\n",
        ),
        parse_one("src/foo.rs", "pub fn helper() {}\n"),
    ];
    let ctx = context(&files);
    let caller = find(&files[0], "caller");
    let helper = id_of(&files[1], "helper");
    assert!(resolved(&ctx, caller).contains(&helper));
    let mut path = NameRef::new("crate::foo::helper");
    path.scope = Some("crate".into());
    assert_eq!(adapter().resolve(&ctx, &path), Some(helper));
}

#[test]
fn inline_mod_super_and_test_attributes() {
    let src = r#"
fn prod() {}

#[cfg(test)]
mod tests {
    use super::prod;

    fn helper() {
        prod();
    }

    #[test]
    fn t() {
        helper();
    }
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let prod = id_of(&files[0], "prod");
    let helper = id_of(&files[0], "tests::helper");
    let test_fn = find(&files[0], "tests::t");
    let targets = adapter().test_targets(&ctx, test_fn);
    assert!(
        targets.contains(&helper),
        "test should see helper: {targets:?}"
    );
    assert!(
        !targets.contains(&prod),
        "the test fn calls helper, not prod directly"
    );

    let module = find(&files[0], "tests");
    let module_targets = adapter().test_targets(&ctx, module);
    assert!(
        module_targets.contains(&prod),
        "cfg(test) module should include helper's callees, got {module_targets:?}"
    );
    assert!(module_targets.contains(&helper));
}

#[test]
fn inner_cfg_test_marks_the_module() {
    let src = r#"
fn prod() {}

mod tests {
    #![cfg(test)]
    use super::prod;

    fn helper() {
        prod();
    }

    #[test]
    fn t() {
        helper();
    }
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let module_targets = adapter().test_targets(&ctx, find(&files[0], "tests"));
    assert!(module_targets.contains(&id_of(&files[0], "prod")));
    assert!(module_targets.contains(&id_of(&files[0], "tests::helper")));
}

#[test]
fn cfg_not_test_is_not_a_test() {
    let src = "fn other() {}\n#[cfg(not(test))]\nfn prod() { other(); }\n";
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let prod = find(&files[0], "prod");
    assert!(adapter().test_targets(&ctx, prod).is_empty());
}

#[test]
fn path_test_attribute_counts() {
    let src = "fn target() {}\n#[tokio::test]\nfn t() { target(); }\n";
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let targets = adapter().test_targets(&ctx, find(&files[0], "t"));
    assert!(targets.contains(&id_of(&files[0], "target")));
}

#[test]
fn macro_invocation_stays_lexical() {
    let hidden = r#"
macro_rules! call_hidden {
    () => { hidden() };
    ($x:ident) => { $x() };
}
fn hidden() {}
fn caller() { call_hidden!(); }
fn caller_with_ident() { call_hidden!(hidden); }
"#;
    let files = vec![parse_one("src/lib.rs", hidden)];
    let ctx = context(&files);
    let hidden_id = id_of(&files[0], "hidden");
    assert!(
        !resolved(&ctx, find(&files[0], "caller")).contains(&hidden_id),
        "opaque invocation must not see names that exist only after expansion"
    );
    assert!(resolved(&ctx, find(&files[0], "caller_with_ident")).contains(&hidden_id));
}

#[test]
fn path_attribute_and_mod_rs_layout() {
    let files = vec![
        parse_one(
            "src/lib.rs",
            "#[path = \"renamed.rs\"]\nmod via_path;\nmod foo;\nfn caller() { via_path::bar(); foo::bar::baz(); }\n",
        ),
        parse_one("src/renamed.rs", "pub fn bar() {}\n"),
        parse_one("src/foo/mod.rs", "pub mod bar;\n"),
        parse_one("src/foo/bar.rs", "pub fn baz() {}\n"),
    ];
    let ctx = context(&files);
    let caller = find(&files[0], "caller");
    let edges = resolved(&ctx, caller);
    assert!(edges.contains(&id_of(&files[1], "bar")));
    assert!(edges.contains(&id_of(&files[3], "baz")));
}

#[test]
fn inherent_method_and_self_path() {
    let src = r#"
struct Point;
impl Point {
    fn origin() -> Point { Point }
    fn other() { Self::origin(); }
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "impl Point::other"));
    assert!(edges.contains(&id_of(&files[0], "impl Point::origin")));
    assert!(edges.contains(&id_of(&files[0], "Point")));
}

#[test]
fn method_calls_over_approximate_without_types() {
    let src = r#"
struct A;
struct B;
impl A { fn draw(&self) {} }
impl B { fn draw(&self) {} }
fn paint(a: &A) { a.draw(); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "paint"));
    let draws: Vec<_> = files[0]
        .tree
        .iter()
        .filter(|(_, n)| {
            n.name
                .as_ref()
                .is_some_and(|q| q.as_str() == "impl A::draw" || q.as_str() == "impl B::draw")
        })
        .collect();
    assert_eq!(draws.len(), 2);
    for (_, node) in draws {
        let id = files[0].ids[&ObjectId::of(node).unwrap()];
        assert!(edges.contains(&id), "missing draw candidate {id}");
    }
}

#[test]
fn external_paths_do_not_resolve_inside_the_crate() {
    let src = "fn f() { let _ = std::vec::Vec::<u8>::new(); }\n";
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let refs = adapter().references(&ctx, find(&files[0], "f"));
    let std_ref = refs.iter().find(|r| r.name.as_str().contains("std::"));
    assert!(
        std_ref.is_some(),
        "lexical path should be reported: {refs:?}"
    );
    assert!(std_ref.unwrap().resolved.is_none());
}

#[test]
fn glob_and_alias_imports() {
    let src = r#"
mod foo {
    pub struct Bar;
    pub fn helper() {}
}
use foo::Bar as Baz;
use foo::*;
fn f(x: Baz) { helper(); let _ = x; }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "f"));
    assert!(edges.contains(&id_of(&files[0], "foo::Bar")));
    assert!(edges.contains(&id_of(&files[0], "foo::helper")));
}

#[test]
fn ambiguous_method_resolve_is_not_a_single_id() {
    let src = r#"
struct A;
struct B;
impl A { fn draw(&self) {} }
impl B { fn draw(&self) {} }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let mut name = NameRef::new(".draw");
    name.scope = Some("crate".into());
    assert!(adapter().resolve(&ctx, &name).is_none());
}

#[test]
fn method_call_emits_one_edge_per_candidate() {
    let src = r#"
struct A;
struct B;
impl A { fn draw(&self) {} }
impl B { fn draw(&self) {} }
fn paint(a: &A) { a.draw(); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let refs = adapter().references(&ctx, find(&files[0], "paint"));
    let draws: Vec<_> = refs.iter().filter(|r| r.name.as_str() == ".draw").collect();
    assert_eq!(draws.len(), 2, "one NameRef per candidate: {refs:?}");
    assert!(draws.iter().all(|r| r.resolved.is_some()));
    assert_ne!(draws[0].resolved, draws[1].resolved);
    assert!(
        draws
            .iter()
            .all(|r| r.scope.as_ref().is_some_and(|s| s.as_str() == "crate"))
    );
}

#[test]
fn cfg_any_and_all_include_test_not_does_not() {
    let src = r#"
fn target() {}
#[cfg(any(unix, test))]
fn via_any() { target(); }
#[cfg(all(test, debug_assertions))]
fn via_all() { target(); }
#[cfg(all(any(test), unix))]
fn via_nested() { target(); }
#[cfg(any(not(test), unix))]
fn via_not_any() { target(); }
#[cfg(all(not(test), debug_assertions))]
fn via_not_all() { target(); }
#[cfg(not(any(test)))]
fn via_not_group() { target(); }
#[cfg(feature = "test")]
fn via_feature() { target(); }
#[rstest]
fn via_other_attr() { target(); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let target = id_of(&files[0], "target");
    for name in ["via_any", "via_all", "via_nested"] {
        let got = adapter().test_targets(&ctx, find(&files[0], name));
        assert!(
            got.contains(&target),
            "{name} should count as a test: {got:?}"
        );
    }
    for name in [
        "via_not_any",
        "via_not_all",
        "via_not_group",
        "via_feature",
        "via_other_attr",
    ] {
        let got = adapter().test_targets(&ctx, find(&files[0], name));
        assert!(got.is_empty(), "{name} is not a test: {got:?}");
    }
}

#[test]
fn cfg_test_function_is_a_test_target() {
    let src = "fn prod() {}\n#[cfg(test)]\nfn helper() { prod(); }\n";
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let got = adapter().test_targets(&ctx, find(&files[0], "helper"));
    assert!(got.contains(&id_of(&files[0], "prod")), "{got:?}");
}

#[test]
fn single_file_uses_the_crate_path() {
    let files = vec![parse_one(
        "src/not_a_root.rs",
        "fn helper() {}\nfn caller() { helper(); }\n",
    )];
    let ctx = context(&files);
    let refs = adapter().references(&ctx, find(&files[0], "caller"));
    let helper = refs
        .iter()
        .find(|r| r.name.as_str() == "helper")
        .expect("helper");
    assert_eq!(helper.scope.as_ref().map(|s| s.as_str()), Some("crate"));
    assert_eq!(helper.resolved, Some(id_of(&files[0], "helper")));
}

#[test]
fn several_crate_roots_are_disambiguated_by_repo_path() {
    let files = vec![
        parse_one(
            "src/lib.rs",
            "pub fn helper() {}\nfn only_lib() {}\nfn caller() { helper(); only_lib(); }\n",
        ),
        parse_one(
            "src/main.rs",
            "fn helper() {}\nfn only_bin() {}\nfn caller() { helper(); only_bin(); }\n",
        ),
        parse_one("build.rs", "fn helper() {}\n"),
        parse_one(
            "tests/api.rs",
            "fn helper() {}\nfn caller() { helper(); }\n",
        ),
    ];
    let mut files = files;
    reassign_ids(&mut files);
    let ctx = context(&files);
    let lib_helper = id_of(&files[0], "helper");
    let bin_helper = id_of(&files[1], "helper");
    let build_helper = id_of(&files[2], "helper");
    let test_helper = id_of(&files[3], "helper");

    let scope_of = |file: &Owned| {
        adapter()
            .references(&ctx, find(file, "caller"))
            .into_iter()
            .find(|r| r.name.as_str() == "helper")
            .and_then(|r| r.scope)
    };
    let lib_scope = scope_of(&files[0]).expect("lib scope");
    let bin_scope = scope_of(&files[1]).expect("bin scope");
    let test_scope = scope_of(&files[3]).expect("integration scope");
    assert_ne!(lib_scope, bin_scope);
    assert_ne!(lib_scope, test_scope);
    assert_ne!(lib_scope.as_str(), "crate");

    let resolve_in = |scope: &hord_core::QualifiedName, name: &str| {
        let mut r = NameRef::new(name);
        r.scope = Some(scope.clone());
        adapter().resolve(&ctx, &r)
    };
    assert_eq!(resolve_in(&lib_scope, "helper"), Some(lib_helper));
    assert_eq!(resolve_in(&bin_scope, "helper"), Some(bin_helper));
    assert_eq!(resolve_in(&test_scope, "helper"), Some(test_helper));
    assert_eq!(
        resolve_in(&lib_scope, "crate::only_lib"),
        Some(id_of(&files[0], "only_lib"))
    );
    assert_eq!(resolve_in(&bin_scope, "crate::only_lib"), None);
    assert_eq!(resolve_in(&lib_scope, "only_bin"), None);

    let mut bare = NameRef::new("helper");
    assert!(
        adapter().resolve(&ctx, &bare).is_none(),
        "two helpers, no scope"
    );
    bare.name = "only_lib".into();
    assert_eq!(
        adapter().resolve(&ctx, &bare),
        Some(id_of(&files[0], "only_lib"))
    );
    bare.name = "only_bin".into();
    assert_eq!(
        adapter().resolve(&ctx, &bare),
        Some(id_of(&files[1], "only_bin"))
    );

    let lib_edges = resolved(&ctx, find(&files[0], "caller"));
    assert!(lib_edges.contains(&lib_helper));
    assert!(!lib_edges.contains(&bin_helper));
    assert!(!lib_edges.contains(&build_helper));
    assert!(!lib_edges.contains(&test_helper));
}

#[test]
fn qualified_name_stays_file_local() {
    let files = vec![
        parse_one("src/lib.rs", "mod foo;\n"),
        parse_one("src/foo.rs", "pub fn helper() {}\n"),
    ];
    let helper = find(&files[1], "helper");
    assert_eq!(helper.name.as_ref().map(|n| n.as_str()), Some("helper"));
    let ctx = context(&files);
    let mut path = NameRef::new("crate::foo::helper");
    path.scope = Some("crate".into());
    assert_eq!(
        adapter().resolve(&ctx, &path),
        Some(id_of(&files[1], "helper"))
    );
}

#[test]
fn resolve_prefers_a_set_id_else_the_unique_final() {
    let files = vec![parse_one(
        "src/lib.rs",
        "mod foo { pub fn helper() {} }\nfn caller() { foo::helper(); }\n",
    )];
    let ctx = context(&files);
    let helper = id_of(&files[0], "foo::helper");
    let module = id_of(&files[0], "foo");
    let mut path = NameRef::new("foo::helper");
    path.scope = Some("crate".into());
    assert_eq!(adapter().resolve(&ctx, &path), Some(helper));

    path.resolved = Some(module);
    assert_eq!(
        adapter().resolve(&ctx, &path),
        Some(module),
        "an already-set candidate wins over lookup"
    );
    path.resolved = Some(NodeId::nil());
    assert_eq!(adapter().resolve(&ctx, &path), None);

    let refs = adapter().references(&ctx, find(&files[0], "caller"));
    let finals: Vec<_> = refs
        .iter()
        .filter(|r| r.name.as_str() == "foo::helper" && r.resolved == Some(helper))
        .collect();
    assert_eq!(finals.len(), 1, "{refs:?}");
}

#[test]
fn duplicate_imports_stay_ambiguous() {
    let src = r#"
mod a { pub fn f() {} }
mod b { pub fn f() {} }
use a::f;
use b::f;
fn caller() { f(); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let refs = adapter().references(&ctx, find(&files[0], "caller"));
    let ids: BTreeSet<_> = refs
        .iter()
        .filter(|r| r.name.as_str() == "f")
        .filter_map(|r| r.resolved)
        .collect();
    // `use a::f` stores the same file-local name as `mod a`'s function.
    assert!(ids.contains(&id_of_kind(&files[0], "a::f", Some("function_item"))));
    assert!(ids.contains(&id_of_kind(&files[0], "b::f", Some("function_item"))));
    let mut name = NameRef::new("f");
    name.scope = Some("crate".into());
    assert!(adapter().resolve(&ctx, &name).is_none());
}

#[test]
fn nil_definition_ids_are_skipped() {
    let mut file = parse_one("src/lib.rs", "fn helper() {}\nfn caller() { helper(); }\n");
    let helper = find(&file, "helper");
    let oid = ObjectId::of(helper).expect("object id");
    file.ids.insert(oid, NodeId::nil());
    let ctx = context(std::slice::from_ref(&file));
    let refs = adapter().references(&ctx, find(&file, "caller"));
    let mention = refs
        .iter()
        .find(|r| r.name.as_str() == "helper")
        .expect("mention");
    assert!(mention.resolved.is_none(), "{refs:?}");
    let mut path = NameRef::new("helper");
    path.scope = Some("crate".into());
    assert_eq!(adapter().resolve(&ctx, &path), None);
}

#[test]
fn macro_token_tree_paths_are_lexical() {
    let src = r#"
fn helper() {}
mod foo { pub fn bar() {} }
fn caller() { mac!(helper); mac!(foo::bar); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "caller"));
    assert!(edges.contains(&id_of(&files[0], "helper")));
    assert!(edges.contains(&id_of(&files[0], "foo::bar")));
}

#[test]
fn enum_variant_paths_resolve() {
    let src =
        "enum Kind { A, B { n: i32 } }\nfn f() { let _ = Kind::A; let _ = Kind::B { n: 1 }; }\n";
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "f"));
    assert!(edges.contains(&id_of(&files[0], "Kind::A")), "{edges:?}");
    assert!(edges.contains(&id_of(&files[0], "Kind::B")), "{edges:?}");
}

#[test]
fn super_from_a_child_file() {
    let files = vec![
        parse_one("src/lib.rs", "mod foo;\nfn prod() {}\n"),
        parse_one("src/foo.rs", "pub fn child() { super::prod(); }\n"),
    ];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[1], "child"));
    assert!(edges.contains(&id_of(&files[0], "prod")), "{edges:?}");
}

#[test]
fn pub_use_reexport_resolves() {
    let src = r#"
mod foo { pub fn helper() {} }
mod bar { pub use super::foo::helper; }
fn caller() { bar::helper(); }
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "caller"));
    assert!(
        edges.contains(&id_of(&files[0], "foo::helper")),
        "{edges:?}"
    );
}

#[test]
fn self_inside_a_trait_names_that_trait() {
    let src = r#"
trait Trait {
    fn method(&self);
    fn caller(&self) { Self::method(); }
    type Item;
    type Other = Self::Item;
}
struct S;
impl Trait for S {
    fn method(&self) {}
    type Item = ();
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let method = id_of(&files[0], "Trait::method");
    let item = id_of(&files[0], "Trait::Item");
    let caller_edges = resolved(&ctx, find(&files[0], "Trait::caller"));
    assert!(
        caller_edges.contains(&method),
        "Self::method in the trait: {caller_edges:?}"
    );
    let other_edges = resolved(&ctx, find(&files[0], "Trait::Other"));
    assert!(
        other_edges.contains(&item),
        "Self::Item in the trait: {other_edges:?}"
    );
    let trait_edges = resolved(&ctx, find(&files[0], "Trait"));
    assert!(
        trait_edges.contains(&method),
        "walking the trait should see Self::method: {trait_edges:?}"
    );
}

#[test]
fn trait_test_sees_self_paths() {
    let src = r#"
trait Trait {
    fn method(&self) {}
    #[test]
    fn t() { Self::method(); }
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let method = id_of(&files[0], "Trait::method");
    let on_fn = adapter().test_targets(&ctx, find(&files[0], "Trait::t"));
    assert!(on_fn.contains(&method), "{on_fn:?}");
    let on_trait = adapter().test_targets(&ctx, find(&files[0], "Trait"));
    assert!(on_trait.contains(&method), "{on_trait:?}");
}

#[test]
fn impl_header_bounds_are_references() {
    let src = r#"
trait Bound {}
trait Other {}
struct Point<T>(T);
impl<T: Bound> Point<T>
where
    T: Other,
{
    fn m() {}
}
"#;
    let files = vec![parse_one("src/lib.rs", src)];
    let ctx = context(&files);
    let impl_node = find(&files[0], "impl Point<T>");
    let edges = resolved(&ctx, impl_node);
    assert!(edges.contains(&id_of(&files[0], "Bound")), "{edges:?}");
    assert!(edges.contains(&id_of(&files[0], "Other")), "{edges:?}");
}

#[test]
fn path_attribute_accepts_raw_and_byte_strings() {
    let files = vec![
        parse_one(
            "src/lib.rs",
            "#[path = r\"raw_file.rs\"]\nmod raw_mod;\n#[path = r##\"hash_file.rs\"##]\nmod hash_mod;\n#[path = b\"byte_file.rs\"]\nmod byte_mod;\nfn caller() { raw_mod::raw_f(); hash_mod::hash_f(); byte_mod::byte_f(); }\n",
        ),
        parse_one("src/raw_file.rs", "pub fn raw_f() {}\n"),
        parse_one("src/hash_file.rs", "pub fn hash_f() {}\n"),
        parse_one("src/byte_file.rs", "pub fn byte_f() {}\n"),
    ];
    let mut files = files;
    reassign_ids(&mut files);
    let ctx = context(&files);
    let edges = resolved(&ctx, find(&files[0], "caller"));
    assert!(edges.contains(&id_of(&files[1], "raw_f")), "{edges:?}");
    assert!(edges.contains(&id_of(&files[2], "hash_f")), "{edges:?}");
    assert!(edges.contains(&id_of(&files[3], "byte_f")), "{edges:?}");
}
