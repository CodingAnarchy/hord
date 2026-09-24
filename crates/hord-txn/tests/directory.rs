//! Directory workspaces: copy-on-write checkouts, the stat index, gc
//! (ADR 0016).

mod common;

use std::fs;
use std::time::{Duration, SystemTime};

use common::*;
use hord_core::NodeId;
use hord_txn::{BeginOptions, Materialization, MaterializeMode};

fn checkout_of(ws: &hord_txn::Workspace) -> TestResult<std::path::PathBuf> {
    match ws.materialization() {
        Materialization::Directory { path } => Ok(path.clone()),
        Materialization::InMemory => Err("expected a directory workspace".into()),
    }
}

#[tokio::test]
async fn clone_and_copy_checkouts_are_independent_and_writable() -> TestResult {
    let t = repo(&fixture()).await?;
    let a = t
        .repo
        .begin_directory_with(BeginOptions::at_head(actor("a")), MaterializeMode::Clone)
        .await?;
    let b = t
        .repo
        .begin_directory_with(BeginOptions::at_head(actor("b")), MaterializeMode::Copy)
        .await?;
    if cfg!(target_vendor = "apple") {
        // APFS clones the whole tree (the temp dir is on APFS).
        assert_eq!(a.materialized_as(), Some(MaterializeMode::Clone));
    }
    assert_eq!(b.materialized_as(), Some(MaterializeMode::Copy));
    let (da, db) = (checkout_of(&a)?, checkout_of(&b)?);
    for dir in [&da, &db] {
        assert_eq!(fs::read_to_string(dir.join("src/lib.rs"))?, LIB);
        assert!(
            !fs::metadata(dir.join("src/lib.rs"))?
                .permissions()
                .readonly()
        );
    }
    // Writing one checkout touches neither the other nor the pristine copy.
    fs::write(da.join("src/lib.rs"), "changed\n")?;
    fs::write(da.join("new.txt"), "x\n")?;
    assert_eq!(fs::read_to_string(db.join("src/lib.rs"))?, LIB);
    let c = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("c")))
        .await?;
    assert_eq!(
        fs::read_to_string(checkout_of(&c)?.join("src/lib.rs"))?,
        LIB
    );
    assert!(!checkout_of(&c)?.join("new.txt").exists());
    // Reopening by id recovers the mode from the stat index.
    let again = t.repo.open_workspace(b.id(), actor("b"), None).await?;
    assert_eq!(again.materialized_as(), Some(MaterializeMode::Copy));
    Ok(())
}

#[tokio::test]
async fn propose_skips_stat_clean_files_and_paranoid_rehashes() -> TestResult {
    let t = repo(&fixture()).await?;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await?;
    let dir = checkout_of(&ws)?;
    let lib = dir.join("src/lib.rs");
    let before = fs::metadata(&lib)?;

    // Same size, content changed, mtime put back: the stat index says clean.
    let sneaky = LIB.replace("    2\n", "    3\n");
    assert_eq!(sneaky.len(), LIB.len());
    fs::write(&lib, &sneaky)?;
    let file = fs::OpenOptions::new().write(true).open(&lib)?;
    file.set_modified(before.modified()?)?;
    drop(file);
    if fs::metadata(&lib)?.modified()? == before.modified()? {
        assert!(matches!(
            ws.preview(intent("x")).await,
            Err(hord_txn::Error::NothingToPropose)
        ));
    }
    ws.set_paranoid(true);
    let found = ws.preview(intent("x")).await?;
    assert!(!found.record.ops.is_empty());
    ws.set_paranoid(false);

    // An ordinary edit changes mtime and is found without paranoia.
    std::thread::sleep(Duration::from_millis(5));
    fs::write(&lib, LIB.replace("    4\n", "    44\n"))?;
    let file = fs::OpenOptions::new().write(true).open(&lib)?;
    file.set_modified(SystemTime::now())?;
    drop(file);
    fs::remove_file(dir.join("README.md"))?;
    let proposal = ws.propose(intent("edit")).await?;
    let delta = def(&mut ws, "src/lib.rs", "delta").await?;
    assert!(proposal.record.write_set.contains(&delta));
    assert!(
        proposal
            .record
            .write_set
            .contains(&NodeId::file_root(&path("README.md")))
    );
    Ok(())
}

#[tokio::test]
async fn gc_removes_pristine_checkouts_no_workspace_uses() -> TestResult {
    let t = repo(&fixture()).await?;
    let first_head = t.repo.head().await?.snapshot;
    let mut ws = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("a")))
        .await?;
    let pristine = t.path.join(".hord/pristine").join(first_head.to_hex());
    assert!(pristine.is_dir());
    assert!(t.repo.gc_pristine().await?.is_empty(), "still in use");

    // Land a change, check out the new head, drop the old workspace.
    fs::write(
        checkout_of(&ws)?.join("src/lib.rs"),
        LIB.replace("    2\n", "    20\n"),
    )?;
    let change = ws.propose(intent("edit")).await?.change;
    t.repo.submit(change).await?;
    t.repo.land_local().await?;
    let newer = t
        .repo
        .begin_directory(BeginOptions::at_head(actor("b")))
        .await?;
    assert!(t.repo.remove_workspace(ws.id()).await?);
    assert!(!checkout_of(&ws)?.exists());
    assert_eq!(t.repo.gc_pristine().await?, vec![first_head]);
    assert!(!pristine.exists());
    assert_eq!(
        fs::read_to_string(checkout_of(&newer)?.join("src/lib.rs"))?,
        LIB.replace("    2\n", "    20\n")
    );
    Ok(())
}

/// ADR 0016 measurements on cargo HEAD: pristine checkout, clone and copy
/// creation, 1,000 live clones, and propose with the stat index. Needs the
/// M0 corpus (`$HORD_CORPORA/cargo.git` or `~/.cache/hord/corpora`). Run:
///
/// ```text
/// cargo test -p hord-txn --release --test directory -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the cargo corpus; run in release"]
async fn directory_workspaces_on_cargo() -> TestResult {
    use std::time::Instant;

    let corpora = match std::env::var("HORD_CORPORA") {
        Ok(dir) => std::path::PathBuf::from(dir),
        Err(_) => std::path::PathBuf::from(std::env::var("HOME")?).join(".cache/hord/corpora"),
    };
    let git_dir = corpora.join("cargo.git");
    assert!(git_dir.is_dir(), "missing {}", git_dir.display());
    let export = temp_dir("cargo-export")?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "git --git-dir '{}' archive HEAD | tar -x -C '{}'",
            git_dir.display(),
            export.display()
        ))
        .status()?;
    assert!(status.success());
    let mut files = Vec::new();
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> TestResult {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|name| format!("non-UTF-8 file name {name:?}"))?;
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                walk(&entry.path(), &rel, out)?;
            } else if meta.is_file() {
                out.push((rel, fs::read(entry.path())?));
            }
        }
        Ok(())
    }
    walk(&export, "", &mut files)?;
    let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
    let _ = remove_tree(&export);

    let dir = temp_dir("cargo-dir-ws")?;
    let store = hord_store::Store::create(&dir)?;
    let repo = hord_txn::Repo::from_store(store, hord_txn::RepoOptions::default()).await?;
    let seed = files
        .iter()
        .map(|(p, b)| Ok((p.parse()?, b.clone())))
        .collect::<TestResult<_>>()?;
    repo.bootstrap(seed, intent("cargo HEAD"), actor("seed"))
        .await?;
    eprintln!(
        "[dir] cargo HEAD: {} files, {:.1} MB",
        files.len(),
        bytes as f64 / 1e6
    );

    let begin = |mode| {
        let repo = repo.clone();
        async move {
            let started = Instant::now();
            let ws = repo
                .begin_directory_with(BeginOptions::at_head(actor("m")), mode)
                .await?;
            Ok::<_, hord_txn::Error>((started.elapsed(), ws))
        }
    };
    let (first, _ws0) = begin(MaterializeMode::Clone).await?;
    eprintln!("[dir] first workspace (writes the pristine checkout): {first:?}");

    // Copies: a sample; 1,000 would be 1,000 full checkouts.
    let mut copies = Vec::new();
    let mut copy_ws = Vec::new();
    for _ in 0..20 {
        let (t, ws) = begin(MaterializeMode::Copy).await?;
        assert_eq!(ws.materialized_as(), Some(MaterializeMode::Copy));
        copies.push(t);
        copy_ws.push(ws);
    }
    let stats = |xs: &mut Vec<Duration>| {
        xs.sort();
        let n = xs.len();
        let mean = xs.iter().sum::<Duration>()
            / u32::try_from(n).expect("at most 1,000 samples fit in u32");
        (mean, xs[n / 2], xs[(n * 99) / 100], xs[n - 1])
    };
    let (mean, p50, p99, max) = stats(&mut copies);
    eprintln!("[dir] copy x20: mean {mean:?} p50 {p50:?} p99 {p99:?} max {max:?}");
    for ws in copy_ws {
        repo.remove_workspace(ws.id()).await?;
    }

    // 1,000 live clones.
    let mut clones = Vec::with_capacity(1_000);
    let mut live = Vec::with_capacity(1_000);
    let started = Instant::now();
    for _ in 0..1_000 {
        let (t, ws) = begin(MaterializeMode::Clone).await?;
        clones.push(t);
        live.push(ws);
    }
    let total = started.elapsed();
    let used = live[0].materialized_as();
    let first100 = clones[..100].iter().sum::<Duration>() / 100;
    let last100 = clones[900..].iter().sum::<Duration>() / 100;
    let (mean, p50, p99, max) = stats(&mut clones);
    eprintln!(
        "[dir] {used:?} x1000 live: total {total:?}, mean {mean:?} p50 {p50:?} p99 {p99:?} max {max:?}; first-100 mean {first100:?}, last-100 mean {last100:?}"
    );

    // Preview of an unedited clone (the walk and stat compare alone),
    // interleaved with the serial `lstat` walk it replaced (M3 review perf
    // #8), so both see the same machine load.
    fn serial_walk(dir: &std::path::Path, prefix: &mut Vec<String>, n: &mut usize) -> TestResult {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            prefix.push(
                entry
                    .file_name()
                    .into_string()
                    .map_err(|name| format!("non-UTF-8 file name {name:?}"))?,
            );
            if meta.is_dir() {
                serial_walk(&entry.path(), prefix, n)?;
            } else if meta.is_file() {
                std::hint::black_box(hord_core::RepoPath::new(prefix.clone()));
                *n += 1;
            }
            prefix.pop();
        }
        Ok(())
    }
    let idle_dir = checkout_of(&live[499])?;
    let (mut idle, mut serial) = (Vec::new(), Vec::new());
    for _ in 0..20 {
        let started = Instant::now();
        let mut n = 0;
        serial_walk(&idle_dir, &mut Vec::new(), &mut n)?;
        serial.push(started.elapsed());
        assert_eq!(n, files.len());
        let started = Instant::now();
        let result = live[499].preview(intent("idle")).await;
        idle.push(started.elapsed());
        assert!(matches!(result, Err(hord_txn::Error::NothingToPropose)));
    }
    let (_, idle_p50, idle_p99, _) = stats(&mut idle);
    let (_, serial_p50, serial_p99, _) = stats(&mut serial);
    eprintln!(
        "[dir] unedited preview (walk + stat compare) x20: p50 {idle_p50:?} p99 {idle_p99:?}; serial lstat walk alone: p50 {serial_p50:?} p99 {serial_p99:?}"
    );

    // Propose from a clone: one edited file, stat index vs paranoid.
    let ws = &mut live[500];
    let edited = files
        .iter()
        .map(|(p, _)| p)
        .find(|p| p.starts_with("src/") && p.ends_with(".rs"))
        .ok_or("a Rust file under src/")?;
    let lib = checkout_of(ws)?.join(edited);
    let text = fs::read_to_string(&lib)?;
    fs::write(&lib, format!("{text}\n// edited\n"))?;
    // Warm-up: the first preview on a snapshot builds its reference
    // context (a one-time parse of every Rust file), which is not what is
    // being compared.
    ws.preview(intent("warm-up")).await?;
    let mut previews = Vec::new();
    let mut fast = None;
    for _ in 0..20 {
        let started = Instant::now();
        fast = Some(ws.preview(intent("edit")).await?);
        previews.push(started.elapsed());
    }
    let fast = fast.ok_or("a preview ran")?;
    let (_, with_index, preview_p99, _) = stats(&mut previews);
    ws.set_paranoid(true);
    let started = Instant::now();
    let slow = ws.preview(intent("edit")).await?;
    let paranoid = started.elapsed();
    assert_eq!(fast.record.result, slow.record.result);
    eprintln!(
        "[dir] preview with stat index x20: p50 {with_index:?} p99 {preview_p99:?}; paranoid {paranoid:?}"
    );
    drop(live);
    let _ = remove_tree(&dir);
    Ok(())
}
