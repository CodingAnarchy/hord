//! Regression: a file holding two definitions with identical content (here
//! the same `use` item inside two function bodies, as in cargo's
//! `crates/cargo-util/src/paths.rs`) must still diff an edit to a third
//! function as an edit to that function. Found by `bench/m3-eval`: the
//! record came out as one `Replace` of the file root, and `write_set` held
//! the file-glue id and the duplicate `use` items but not the edited
//! function (an under-declared write set, so a lander false negative).

use hord_core::{Actor, Intent, NodeId, Op, RepoPath};
use hord_txn::{BeginOptions, Repo};

const SRC: &str = "\
pub fn target(x: u32) -> u32 {
    x + 1
}

pub fn first(p: &str) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::prelude::*;
        p.len()
    }
}

pub fn second(p: &str) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::prelude::*;
        p.len() + 1
    }
}
";

fn actor() -> Actor {
    Actor::Human {
        id: "duplicate-content".into(),
    }
}

fn intent(summary: &str) -> Intent {
    Intent {
        summary: summary.into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_beside_duplicate_content_definitions_writes_the_edited_function() {
    let dir = std::env::temp_dir().join(format!("hord-txn-dup-content-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let repo = Repo::create(&dir).await.unwrap();
    let path: RepoPath = "src/lib.rs".parse().unwrap();
    repo.bootstrap(
        vec![(path.clone(), SRC.as_bytes().to_vec())],
        intent("seed"),
        actor(),
    )
    .await
    .unwrap();

    let mut ws = repo.begin(BeginOptions::at_head(actor())).await.unwrap();
    let target = ws
        .definitions(&path)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.name.as_ref().is_some_and(|n| n.as_str() == "target"))
        .expect("definition `target`")
        .node;
    ws.write_definition(
        &path,
        target,
        "pub fn target(x: u32) -> u32 {\n    let _sim = 1_u64;\n    x + 1\n}",
    )
    .await
    .unwrap();
    let record = ws.propose(intent("edit target")).await.unwrap().record;
    let _ = std::fs::remove_dir_all(&dir);

    // The edit is exactly one Replace of `target`: no whole-file replace of
    // the root (nil or the path-derived root), no spurious move.
    let root = hord_txn::path_node_id(&path);
    assert!(
        matches!(record.ops[..], [Op::Replace { node, .. }] if node == target),
        "edit did not diff as Replace(target): {:?}",
        record.ops
    );
    assert!(
        !record.ops.iter().any(|op| matches!(
            op,
            Op::Replace { node, .. } if *node == NodeId::nil() || *node == root
        )),
        "whole-file root replace: {:?}",
        record.ops
    );
    assert_eq!(
        record.write_set.iter().copied().collect::<Vec<_>>(),
        vec![target],
        "write set must be exactly the edited function (no root id)"
    );
}
