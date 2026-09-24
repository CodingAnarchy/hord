//! The `RepoBackend` conformance suite (ADR 0024) against [`LocalRepo`].

mod common;

use common::{TestResult, remove_tree, temp_dir};
use hord_txn::{LocalRepo, Repo};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_repo_passes_the_repo_backend_conformance_suite() -> TestResult {
    let dir = temp_dir("conformance")?;
    let repo = Repo::create(&dir).await?;
    let local = LocalRepo::new(repo)?;
    hord_api::conformance::run(&local).await?;
    local.shutdown().await;
    drop(local);
    let _ = remove_tree(&dir);
    Ok(())
}
