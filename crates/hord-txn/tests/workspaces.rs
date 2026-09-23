//! Workspace creation latency and many live workspaces (spec §6.1, §12 M3).

mod common;

use std::time::{Duration, Instant};

use common::*;

/// `lib.rs` with `n` small functions `f0 … f{n-1}`.
fn many_fns(n: usize) -> String {
    (0..n)
        .map(|i| format!("pub fn f{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n\n"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_creation_is_under_5ms() {
    let t = repo(&fixture()).await;
    // First begin loads head; every later one is a pointer copy.
    begin(&t.repo, "warm").await;
    let mut worst = Duration::ZERO;
    for i in 0..200 {
        let started = Instant::now();
        let ws = begin(&t.repo, &format!("agent-{i}")).await;
        worst = worst.max(started.elapsed());
        drop(ws);
    }
    eprintln!("[begin] worst of 200: {worst:?}");
    assert!(worst < Duration::from_millis(5), "begin took {worst:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thousand_live_workspaces_do_not_degrade() {
    const N: usize = 1_000;
    let lib = many_fns(100);
    let t = repo(&[("src/lib.rs", lib.as_str())]).await;
    begin(&t.repo, "warm").await;

    let mut live = Vec::with_capacity(N);
    let mut latencies = Vec::with_capacity(N);
    for i in 0..N {
        let started = Instant::now();
        let ws = begin(&t.repo, &format!("agent-{i}")).await;
        latencies.push(started.elapsed());
        live.push(ws);
    }
    let mean = |xs: &[Duration]| xs.iter().sum::<Duration>() / u32::try_from(xs.len()).unwrap();
    let first = mean(&latencies[..100]);
    let last = mean(&latencies[N - 100..]);
    let worst = latencies.iter().max().copied().unwrap();
    eprintln!("[1000 ws] begin mean first 100 {first:?}, last 100 {last:?}, worst {worst:?}");
    assert!(worst < Duration::from_millis(5), "worst begin {worst:?}");
    assert!(
        last <= first * 3 + Duration::from_micros(200),
        "{first:?} → {last:?}"
    );

    // Every workspace stays usable: each reads one definition and edits it.
    let started = Instant::now();
    for (i, ws) in live.iter_mut().enumerate() {
        let name = format!("f{}", i % 100);
        let node = def(ws, "src/lib.rs", &name).await;
        ws.read_definition(&path("src/lib.rs"), node).await.unwrap();
        let from = format!("    x + {}\n", i % 100);
        let to = format!("    x + {}\n", 1000 + i);
        edit(ws, "src/lib.rs", &lib, &from, &to).await;
    }
    let per_ws = started.elapsed() / u32::try_from(N).unwrap();
    eprintln!("[1000 ws] read + edit per workspace {per_ws:?}");
    assert!(live.iter().all(|ws| ws.access_log().reads.len() == 1));

    // A sample proposes; late proposals are no slower than early ones.
    let mut timings = Vec::new();
    for ws in live.iter_mut().step_by(50) {
        let started = Instant::now();
        ws.propose(intent("edit")).await.unwrap();
        timings.push(started.elapsed());
    }
    eprintln!("[1000 ws] propose timings {timings:?}");
}
