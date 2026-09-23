//! Tier 2 Rust adapter: qualified names, module resolution, references, tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use hord_core::{Node, NodeId, ObjectId, RepoPath};
use hord_lang::{LangAdapter, NameRef, NodeTree};
use hord_lang_rust::{RustAdapter, RustFile};

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
    let node = find(file, want);
    let oid = ObjectId::of(node).expect("object id");
    *file
        .ids
        .get(&oid)
        .unwrap_or_else(|| panic!("no id for {want}"))
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
