//! Regression: a file holding two definitions with identical content (here
//! the same `use` item inside two function bodies, as in cargo's
//! `crates/cargo-util/src/paths.rs`) must still diff an edit to a third
//! function as an edit to that function. Found by `bench/m3-eval`: the
//! record came out as one `Replace` of the file root, and `write_set` held
//! the file-glue id and the duplicate `use` items but not the edited
//! function (an under-declared write set, so a lander false negative).

mod common;

use common::*;
use hord_core::{NodeId, Op};

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

#[tokio::test(flavor = "multi_thread")]
async fn edit_beside_duplicate_content_definitions_writes_the_edited_function() -> TestResult {
    let t = repo(&[("src/lib.rs", SRC)]).await?;
    let mut ws = begin(&t.repo, "duplicate-content").await?;
    let target = rewrite(
        &mut ws,
        "src/lib.rs",
        "target",
        "pub fn target(x: u32) -> u32 {\n    let _sim = 1_u64;\n    x + 1\n}",
    )
    .await?;
    let record = ws.propose(intent("edit target")).await?.record;

    // The edit is exactly one Replace of `target`: no whole-file replace of
    // the root (nil or the path-derived root), no spurious move.
    let root = NodeId::file_root(&path("src/lib.rs"));
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
    Ok(())
}
