//! Lander throughput with verification stubbed (spec §12 M3: ≥ 20 changes/s).
//!
//! The full measurement is a cargo-sized snapshot (1,400 Rust files, ~28,000
//! definitions, ~10 MB) and 100 changes that each edit 1–5 functions, 80 %
//! of them disjoint. Run it in release:
//!
//! ```text
//! cargo test -p hord-txn --release --test throughput -- --ignored --nocapture
//! ```
//!
//! The non-ignored test runs the same code on a small snapshot as a smoke
//! test and does not assert a rate.

mod common;

use std::time::Instant;

use common::*;
use hord_txn::QueueStatus;

const FNS_PER_FILE: usize = 20;

fn file_text(file: usize, bumps: &[(usize, usize)]) -> String {
    let mut out = String::from("//! Synthetic module.\n\nuse std::fmt;\n\n");
    for i in 0..FNS_PER_FILE {
        let next = (i + 1) % FNS_PER_FILE;
        let k = bumps.iter().find(|(f, _)| *f == i).map_or(i, |(_, k)| *k);
        out.push_str(&format!(
            "/// Function {i} of module {file}.\npub fn f{file}_{i}(x: u32) -> u32 {{\n    let y = x.wrapping_mul(3) + {k};\n    if y % 7 == 0 {{ y }} else {{ f{file}_{next}(y / 7) }}\n}}\n\n"
        ));
    }
    out.push_str(&format!(
        "pub struct S{file};\n\nimpl fmt::Display for S{file} {{\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {{\n        write!(f, \"{file}\")\n    }}\n}}\n"
    ));
    out
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

async fn measure(files: usize, changes: usize) -> TestResult<f64> {
    let mut lib = String::new();
    for f in 0..files {
        lib.push_str(&format!("pub mod m{f};\n"));
    }
    let mut contents: Vec<(String, String)> = vec![("src/lib.rs".into(), lib)];
    for f in 0..files {
        contents.push((format!("src/m{f}.rs"), file_text(f, &[])));
    }
    let refs: Vec<(&str, &str)> = contents
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let started = Instant::now();
    let t = repo(&refs).await?;
    eprintln!(
        "[throughput] bootstrap {files} files in {:?}",
        started.elapsed()
    );

    // 80 % of changes pick from their own slice of files; 20 % share a small
    // hot set, so some pairs overlap.
    let mut rng = Lcg(42);
    let hot: Vec<usize> = (0..4).collect();
    let started = Instant::now();
    let mut submitted = Vec::new();
    for c in 0..changes {
        let mut ws = begin(&t.repo, &format!("agent-{c}")).await?;
        let file = if rng.next(10) < 2 {
            hot[rng.next(hot.len())]
        } else {
            hot.len() + (c * 7 + rng.next(3)) % (files - hot.len())
        };
        let count = 1 + rng.next(5);
        let bumps: Vec<(usize, usize)> = (0..count)
            .map(|_| (rng.next(FNS_PER_FILE), 1000 + c))
            .collect();
        ws.write_file(&path(&format!("src/m{file}.rs")), file_text(file, &bumps))
            .await?;
        submitted.push(submit(&t.repo, &mut ws, &format!("change {c}")).await?);
    }
    eprintln!(
        "[throughput] proposed and submitted {changes} in {:?}",
        started.elapsed()
    );

    let started = Instant::now();
    let done = t.repo.land_local().await?;
    let elapsed = started.elapsed();
    assert_eq!(done.len(), changes);
    let landed = done
        .iter()
        .filter(|e| matches!(e.status, QueueStatus::Landed { .. }))
        .count();
    let conflicted = done
        .iter()
        .filter(|e| e.status == QueueStatus::Conflicted)
        .count();
    let flagged = done
        .iter()
        .filter(|e| e.report.as_ref().is_some_and(|r| !r.is_clean()))
        .count();
    assert!(
        done.iter()
            .all(|e| !matches!(e.status, QueueStatus::Rejected { .. }))
    );
    let rate = changes as f64 / elapsed.as_secs_f64();
    eprintln!(
        "[throughput] landed {landed}, conflicted {conflicted}, flagged {flagged} of {changes} in {elapsed:?}: {rate:.1} changes/s"
    );
    Ok(rate)
}

#[tokio::test(flavor = "multi_thread")]
async fn lander_throughput_smoke() -> TestResult {
    measure(40, 20).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "cargo-sized; run in release (see module docs)"]
async fn lander_throughput_cargo_sized() -> TestResult {
    let rate = measure(1_400, 100).await?;
    assert!(rate >= 20.0, "{rate:.1} changes/s < 20");
    Ok(())
}
