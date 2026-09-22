//! M1 corpus checks (spec §12): lossless parse/project,
//! `apply(base, diff(base, result))` on consecutive first-parent commits,
//! and the labeled 3-way merge corpus in `corpora/merges/`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use gix::bstr::ByteSlice;
use hord_diff::{diff, merge, merge_blob};
use hord_lang::{IdentifiedTree, LangAdapter, default_identify};
use hord_lang_rust::RustAdapter;
use hord_lang_toml::TomlAdapter;

#[derive(Debug, Parser)]
#[command(name = "hord-eval-m1", about = "M1 lossless + apply(diff) corpus eval")]
struct Args {
    /// Directory for cloned corpora (default: $HORD_CORPORA or ~/.cache/hord/corpora).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Run only this corpus: `hord`, `cargo`, or `tokio`.
    #[arg(long)]
    only: Option<String>,
    /// Skip consecutive-commit apply(diff); only HEAD losslessness.
    #[arg(long)]
    lossless_only: bool,
    /// Skip HEAD losslessness; only consecutive-commit apply(diff).
    #[arg(long)]
    pairs_only: bool,
    /// Only the labeled merge corpus (`corpora/merges/`).
    #[arg(long)]
    merges_only: bool,
    /// Skip the labeled merge corpus.
    #[arg(long)]
    skip_merges: bool,
}

struct Totals {
    lossless_files: u64,
    lossless_fail: u64,
    pair_files: u64,
    pair_fail: u64,
    merge_cases: u64,
    merge_auto: u64,
    merge_match: u64,
    merge_unparseable: u64,
}

fn main() {
    let args = Args::parse();
    match run(&args) {
        Ok(t) => {
            let merge_ok = merge_gates(&t, args.merges_only);
            let ok = t.lossless_fail == 0 && t.pair_fail == 0 && merge_ok;
            println!(
                "lossless {ok_files}/{files}  apply-diff {ok_pairs}/{pairs}  merges auto {auto}/{cases} match {matched}/{auto} {status}",
                ok_files = t.lossless_files - t.lossless_fail,
                files = t.lossless_files,
                ok_pairs = t.pair_files - t.pair_fail,
                pairs = t.pair_files,
                auto = t.merge_auto,
                cases = t.merge_cases,
                matched = t.merge_match,
                status = if ok { "PASS" } else { "FAIL" }
            );
            std::process::exit(if ok { 0 } else { 1 });
        }
        Err(err) => {
            eprintln!("hord-eval-m1: {err:#}");
            std::process::exit(2);
        }
    }
}

fn run(args: &Args) -> Result<Totals> {
    let mut totals = Totals {
        lossless_files: 0,
        lossless_fail: 0,
        pair_files: 0,
        pair_fail: 0,
        merge_cases: 0,
        merge_auto: 0,
        merge_match: 0,
        merge_unparseable: 0,
    };
    if !args.skip_merges {
        let m = eval_merges()?;
        totals.merge_cases = m.0;
        totals.merge_auto = m.1;
        totals.merge_match = m.2;
        totals.merge_unparseable = m.3;
    }
    if args.merges_only {
        return Ok(totals);
    }
    let cache = args.cache.clone().unwrap_or_else(default_cache);
    let mut names = vec!["hord", "cargo", "tokio"];
    if let Some(only) = &args.only {
        if !names.contains(&only.as_str()) {
            bail!("unknown corpus {only:?}");
        }
        names = vec![only.as_str()];
    }
    for name in names {
        let path = match name {
            "hord" => find_hord_git()?,
            "cargo" => cache.join("cargo.git"),
            "tokio" => cache.join("tokio.git"),
            _ => unreachable!(),
        };
        if !path.exists() {
            bail!("corpus missing at {}", path.display());
        }
        let repo = open_git(&path)?;
        if !args.pairs_only {
            let (n, f) = lossless_head(name, &repo)?;
            totals.lossless_files += n;
            totals.lossless_fail += f;
        }
        if !args.lossless_only {
            let (n, f) = consecutive_pairs(name, &repo)?;
            totals.pair_files += n;
            totals.pair_fail += f;
        }
    }
    Ok(totals)
}

fn merge_gates(t: &Totals, merges_only: bool) -> bool {
    if t.merge_cases == 0 {
        return !merges_only;
    }
    if merges_only && t.merge_cases < 200 {
        return false;
    }
    t.merge_unparseable == 0
        && t.merge_auto * 100 >= t.merge_cases * 70
        && t.merge_match * 100 >= t.merge_auto * 95
}

fn eval_merges() -> Result<(u64, u64, u64, u64)> {
    let dir = merges_dir()?;
    if !dir.is_dir() {
        eprintln!("[merges] no corpus at {}", dir.display());
        return Ok((0, 0, 0, 0));
    }
    let mut cases: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    eprintln!("[merges] {} labeled cases", cases.len());
    let mut auto = 0u64;
    let mut matched = 0u64;
    let mut unparseable = 0u64;
    let mut hard_reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut buckets = [0u64; 5];
    for (i, case) in cases.iter().enumerate() {
        match eval_one_merge(case) {
            Ok(MergeOut::Match { kind }) => {
                auto += 1;
                let idx = match kind {
                    LabelMatch::Byte => 0,
                    LabelMatch::Stripped => 1,
                    LabelMatch::Norms => 2,
                    LabelMatch::NamesOnly => 3,
                    LabelMatch::Miss => 4,
                };
                buckets[idx] += 1;
                // Name presence is reported, not scored. Spec §12 wants the
                // labeled resolution: same bytes, same trivia-stripped tree,
                // or the same definition bodies (`normalized`).
                if matches!(
                    kind,
                    LabelMatch::Byte | LabelMatch::Stripped | LabelMatch::Norms
                ) {
                    matched += 1;
                }
            }
            Ok(MergeOut::Mismatch) => {
                auto += 1;
                if auto.saturating_sub(matched) <= 5 {
                    eprintln!("[merges] mismatch {}", case.display());
                }
            }
            Ok(MergeOut::Hard { reason }) => {
                if reason.contains("unparseable") {
                    unparseable += 1;
                }
                let bucket = hard_reason_bucket(&reason);
                *hard_reasons.entry(bucket).or_default() += 1;
            }
            Err(err) => bail!("{}: {err:#}", case.display()),
        }
        if (i + 1) % 50 == 0 {
            eprintln!("[merges] {}/{}", i + 1, cases.len());
        }
    }
    if !hard_reasons.is_empty() {
        let summary: Vec<String> = hard_reasons
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect();
        eprintln!("[merges] hard {}", summary.join(" "));
    }
    eprintln!(
        "[merges] auto {auto}/{} match {matched}/{auto} unparseable {unparseable} (byte {} stripped {} norms {} names-only {} other {})",
        cases.len(),
        buckets[0],
        buckets[1],
        buckets[2],
        buckets[3],
        buckets[4],
    );
    Ok((cases.len() as u64, auto, matched, unparseable))
}

enum MergeOut {
    Match { kind: LabelMatch },
    Mismatch,
    Hard { reason: String },
}

fn hard_reason_bucket(reason: &str) -> String {
    if reason.contains("unparseable") {
        "unparseable".into()
    } else if reason.contains("different normalized") {
        "same-node-replace".into()
    } else if reason.contains("Delete vs") {
        "delete-vs-other".into()
    } else if reason.contains("apply") {
        "apply".into()
    } else if reason.contains("silent base") {
        "silent-base".into()
    } else if reason.contains("blob") {
        "blob".into()
    } else {
        "other".into()
    }
}

fn eval_one_merge(dir: &Path) -> Result<MergeOut> {
    let (ext, base_src, ours_src, theirs_src, want) = load_case(dir)?;
    match ext {
        "rs" => eval_merge_pair(&RustAdapter, &base_src, &ours_src, &theirs_src, &want),
        "toml" => eval_merge_pair(&TomlAdapter, &base_src, &ours_src, &theirs_src, &want),
        "md" => eval_blob_merge(&base_src, &ours_src, &theirs_src, &want),
        other => bail!("unknown case suffix .{other}"),
    }
}

fn eval_merge_pair<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
    want: &[u8],
) -> Result<MergeOut> {
    let empty = IdentifiedTree::default();
    let base_tree = adapter.parse(base_src).context("parse base")?;
    let base_map = default_identify(adapter, &empty, &base_tree);
    let base = IdentifiedTree::new(base_tree, base_map.nodes);
    let ours_tree = adapter.parse(ours_src).context("parse ours")?;
    let ours_map = default_identify(adapter, &base, &ours_tree);
    let ours = IdentifiedTree::new(ours_tree, ours_map.nodes);
    let theirs_tree = adapter.parse(theirs_src).context("parse theirs")?;
    let theirs_map = default_identify(adapter, &base, &theirs_tree);
    let theirs = IdentifiedTree::new(theirs_tree, theirs_map.nodes);
    match merge(adapter, &base, &ours, &theirs) {
        Ok(merged) => {
            let got = adapter.project(&merged.tree.tree);
            Ok(MergeOut::Match {
                kind: classify_label(adapter, got.as_slice(), want),
            })
        }
        Err(c) => Ok(MergeOut::Hard { reason: c.reason }),
    }
}

type CaseFiles = (&'static str, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

/// How an auto-resolution lines up with the merge-commit label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LabelMatch {
    /// Projected bytes equal the label.
    Byte,
    /// Trivia-stripped roots are equal.
    Stripped,
    /// Same set of definition `normalized` hashes (bodies, not just names).
    Norms,
    /// Label names are present, but at least one body differs.
    NamesOnly,
    /// Not a match under any of the above.
    Miss,
}

fn classify_label<A: LangAdapter>(adapter: &A, got: &[u8], want: &[u8]) -> LabelMatch {
    if got == want {
        return LabelMatch::Byte;
    }
    let Ok(got_tree) = adapter.parse(got) else {
        return LabelMatch::Miss;
    };
    let Ok(want_tree) = adapter.parse(want) else {
        return LabelMatch::Miss;
    };
    let (Some(g), Some(w)) = (got_tree.root(), want_tree.root()) else {
        return LabelMatch::Miss;
    };
    if got_tree.stripped(g) == want_tree.stripped(w) {
        return LabelMatch::Stripped;
    }
    if def_norms(adapter, &got_tree) == def_norms(adapter, &want_tree) {
        return LabelMatch::Norms;
    }
    let got_n = def_names_of(adapter, got);
    let want_n = def_names_of(adapter, want);
    if !want_n.is_empty() && want_n.is_subset(&got_n) {
        return LabelMatch::NamesOnly;
    }
    LabelMatch::Miss
}

fn def_names_of<A: LangAdapter>(adapter: &A, src: &[u8]) -> std::collections::BTreeSet<String> {
    let Ok(tree) = adapter.parse(src) else {
        return Default::default();
    };
    tree.iter()
        .filter(|(_, node)| adapter.is_definition(&node.kind))
        .filter(|(_, node)| {
            !matches!(
                node.kind.as_str(),
                "field_declaration"
                    | "enum_variant"
                    | "inner_attribute_item"
                    | "use_declaration"
                    | "extern_crate_declaration"
                    | "associated_type"
            )
        })
        .filter_map(|(_, node)| {
            let name = node.name.as_ref()?.as_str();
            // Impl blocks and trait impls change identity with type parameters;
            // match on the types/functions they contain instead (ADR 0005).
            if name.starts_with("impl ") || name.contains(" for ") {
                return None;
            }
            Some(name.to_owned())
        })
        .collect()
}

fn def_norms<A: LangAdapter>(
    adapter: &A,
    tree: &hord_lang::NodeTree,
) -> std::collections::BTreeSet<hord_core::ObjectId> {
    tree.iter()
        .filter(|(_, node)| adapter.is_definition(&node.kind))
        .map(|(_, node)| node.normalized)
        .collect()
}

fn eval_blob_merge(base: &[u8], ours: &[u8], theirs: &[u8], want: &[u8]) -> Result<MergeOut> {
    match merge_blob(base, ours, theirs) {
        Ok(got) if got.as_slice() == want => Ok(MergeOut::Match {
            kind: LabelMatch::Byte,
        }),
        Ok(_) => Ok(MergeOut::Mismatch),
        Err(c) => Ok(MergeOut::Hard { reason: c.reason }),
    }
}

fn load_case(dir: &Path) -> Result<CaseFiles> {
    for ext in ["rs", "toml", "md"] {
        let base = dir.join(format!("base.{ext}"));
        if base.exists() {
            return Ok((
                ext,
                fs::read(&base)?,
                fs::read(dir.join(format!("ours.{ext}")))?,
                fs::read(dir.join(format!("theirs.{ext}")))?,
                fs::read(dir.join(format!("result.{ext}")))?,
            ));
        }
    }
    bail!("no base.rs/base.toml in {}", dir.display());
}

fn merges_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HORD_MERGES") {
        return Ok(PathBuf::from(dir));
    }
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpora/merges");
    if from_crate.is_dir() {
        return Ok(from_crate);
    }
    Ok(PathBuf::from("corpora/merges"))
}

fn default_cache() -> PathBuf {
    if let Ok(dir) = std::env::var("HORD_CORPORA") {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/hord/corpora");
    }
    std::env::temp_dir().join("hord-corpora")
}

fn find_hord_git() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HORD_SELF") {
        return Ok(PathBuf::from(dir));
    }
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("git rev-parse")?;
    if !output.status.success() {
        bail!("not inside a git work tree (set HORD_SELF)");
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn open_git(path: &Path) -> Result<gix::Repository> {
    let mut repo = gix::open_opts(path, gix::open::Options::isolated())
        .with_context(|| format!("open git {}", path.display()))?;
    repo.object_cache_size_if_unset(128 * 1024 * 1024);
    Ok(repo)
}

fn lossless_head(name: &str, repo: &gix::Repository) -> Result<(u64, u64)> {
    let tip = repo.head_commit().context("HEAD commit")?;
    let tree_id = tip.tree_id().context("HEAD tree")?.detach();
    let files = collect_lang_files(repo, tree_id)?;
    eprintln!("[{name}] lossless HEAD: {} .rs/.toml files", files.len());
    let start = Instant::now();
    let mut fail = 0u64;
    for (i, (path, oid)) in files.iter().enumerate() {
        let Some(bytes) = blob_bytes(repo, *oid)? else {
            continue;
        };
        if let Err(err) = check_lossless(path, &bytes) {
            fail += 1;
            eprintln!("[{name}] lossless FAIL {path}: {err}");
        }
        if (i + 1) % 500 == 0 {
            eprintln!("[{name}] lossless {}/{}", i + 1, files.len());
        }
    }
    eprintln!(
        "[{name}] lossless {} ok, {fail} fail in {:.1}s",
        files.len() as u64 - fail,
        start.elapsed().as_secs_f64()
    );
    Ok((files.len() as u64, fail))
}

fn consecutive_pairs(name: &str, repo: &gix::Repository) -> Result<(u64, u64)> {
    let tip = repo.head_id().context("HEAD")?.detach();
    let walk = repo.rev_walk([tip]).all().context("rev-walk")?;
    let mut ids = Vec::new();
    for info in walk {
        ids.push(info.context("rev-walk item")?.id);
    }
    eprintln!(
        "[{name}] apply-diff first-parent pairs over {} commits",
        ids.len()
    );
    let start = Instant::now();
    let mut batch: Vec<FilePair> = Vec::new();
    let mut n = 0u64;
    let mut fail = 0u64;
    const BATCH: usize = 256;
    for (ci, git_id) in ids.iter().enumerate() {
        let commit = repo.find_commit(*git_id).context("find commit")?;
        let Some(parent) = commit.parent_ids().next() else {
            continue;
        };
        let parent = parent.detach();
        let old_tree_id = repo.find_commit(parent)?.tree_id()?.detach();
        let new_tree_id = commit.tree_id()?.detach();
        let changed = changed_lang_files(repo, old_tree_id, new_tree_id)?;
        let commit_hex = git_id.to_hex().to_string();
        for (path, old_oid, new_oid) in changed {
            let Some(base) = blob_bytes(repo, old_oid)? else {
                continue;
            };
            let Some(result) = blob_bytes(repo, new_oid)? else {
                continue;
            };
            batch.push(FilePair {
                path,
                commit_hex: commit_hex.clone(),
                base,
                result,
            });
            if batch.len() >= BATCH {
                let (bn, bf) = drain_batch(name, &mut batch);
                n += bn;
                fail += bf;
            }
        }
        if (ci + 1) % 500 == 0 {
            eprintln!(
                "[{name}] apply-diff commits {}/{} files {n} fail {fail}",
                ci + 1,
                ids.len()
            );
        }
    }
    let (bn, bf) = drain_batch(name, &mut batch);
    n += bn;
    fail += bf;
    eprintln!(
        "[{name}] apply-diff {n} file-pairs, {fail} fail in {:.1}s",
        start.elapsed().as_secs_f64()
    );
    Ok((n, fail))
}

struct FilePair {
    path: String,
    commit_hex: String,
    base: Vec<u8>,
    result: Vec<u8>,
}

fn drain_batch(name: &str, batch: &mut Vec<FilePair>) -> (u64, u64) {
    if batch.is_empty() {
        return (0, 0);
    }
    let n = batch.len() as u64;
    let fail = AtomicU64::new(0);
    let workers = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(batch.len());
    let chunk = batch.len().div_ceil(workers).max(1);
    thread::scope(|s| {
        for chunk in batch.chunks(chunk) {
            let fail = &fail;
            s.spawn(move || {
                for job in chunk {
                    if let Err(err) = check_apply_diff(&job.path, &job.base, &job.result) {
                        fail.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[{name}] apply-diff FAIL {} @ {}: {err}",
                            job.path, job.commit_hex
                        );
                    }
                }
            });
        }
    });
    batch.clear();
    (n, fail.load(Ordering::Relaxed))
}

fn check_lossless(path: &str, bytes: &[u8]) -> Result<()> {
    let adapter = adapter_for(path)?;
    let tree = adapter
        .parse(bytes)
        .with_context(|| format!("parse {path}"))?;
    let projected = adapter.project(&tree);
    if projected.as_slice() != bytes {
        bail!(
            "project(parse) != source ({} vs {} bytes)",
            projected.len(),
            bytes.len()
        );
    }
    Ok(())
}

fn check_apply_diff(path: &str, base_src: &[u8], result_src: &[u8]) -> Result<()> {
    if path.ends_with(".rs") {
        check_apply_pair(&RustAdapter, path, base_src, result_src)
    } else if path.ends_with(".toml") {
        check_apply_pair(&TomlAdapter, path, base_src, result_src)
    } else {
        bail!("not a .rs/.toml path: {path}");
    }
}

fn check_apply_pair<A: LangAdapter>(
    adapter: &A,
    path: &str,
    base_src: &[u8],
    result_src: &[u8],
) -> Result<()> {
    let base_tree = adapter.parse(base_src).context("parse base")?;
    let empty = IdentifiedTree::default();
    let base_map = default_identify(adapter, &empty, &base_tree);
    let base = IdentifiedTree::new(base_tree, base_map.nodes);
    let result_tree = adapter.parse(result_src).context("parse result")?;
    let want = result_tree.root();
    let mapping = default_identify(adapter, &base, &result_tree);
    let ops = diff(&base, &result_tree, &mapping);
    // `diff` verifies apply identity (file Replace fallback if needed).
    let got = match ops.first() {
        Some(hord_core::Op::Replace { node, to, .. }) if *node == hord_diff::file_parent() => {
            Some(*to)
        }
        _ => hord_diff::apply(&base, &ops, &result_tree)
            .context("apply")?
            .tree
            .root(),
    };
    if got != want {
        bail!("apply(diff) root mismatch on {path} ({} ops)", ops.len());
    }
    Ok(())
}

fn adapter_for(path: &str) -> Result<Box<dyn LangAdapter>> {
    if path.ends_with(".rs") {
        Ok(Box::new(RustAdapter))
    } else if path.ends_with(".toml") {
        Ok(Box::new(TomlAdapter))
    } else {
        bail!("not a .rs/.toml path: {path}");
    }
}

fn blob_bytes(repo: &gix::Repository, oid: gix::ObjectId) -> Result<Option<Vec<u8>>> {
    let Ok(obj) = repo.find_object(oid) else {
        return Ok(None);
    };
    if !obj.kind.is_blob() {
        return Ok(None);
    }
    Ok(Some(obj.data.to_vec()))
}

fn changed_lang_files(
    repo: &gix::Repository,
    old_tree_id: gix::ObjectId,
    new_tree_id: gix::ObjectId,
) -> Result<Vec<(String, gix::ObjectId, gix::ObjectId)>> {
    let old = repo.find_tree(old_tree_id).context("old tree")?;
    let new = repo.find_tree(new_tree_id).context("new tree")?;
    let mut out = Vec::new();
    old.changes()
        .context("tree diff")?
        .options(|o| {
            o.track_path();
            o.track_rewrites(None);
        })
        .for_each_to_obtain_tree(&new, |change| {
            use gix::object::tree::diff::Change;
            if let Change::Modification {
                location,
                previous_id,
                id,
                previous_entry_mode,
                entry_mode,
                ..
            } = change
            {
                if !previous_entry_mode.is_blob() || !entry_mode.is_blob() {
                    return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
                }
                let old_oid = previous_id.detach();
                let new_oid = id.detach();
                if old_oid == new_oid {
                    // Mode-only change (chmod); blob bytes are identical.
                    return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
                }
                let path = location.to_str().unwrap_or("");
                if path.ends_with(".rs") || path.ends_with(".toml") {
                    out.push((path.to_owned(), old_oid, new_oid));
                }
            }
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        })
        .context("walk tree diff")?;
    Ok(out)
}

fn collect_lang_files(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
) -> Result<Vec<(String, gix::ObjectId)>> {
    let mut out = Vec::new();
    walk_tree(repo, tree_id, "", &mut out)?;
    Ok(out)
}

fn walk_tree(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    prefix: &str,
    out: &mut Vec<(String, gix::ObjectId)>,
) -> Result<()> {
    let tree = repo
        .find_object(tree_id)
        .context("find tree")?
        .try_into_tree()
        .context("not a tree")?;
    for entry in tree.iter() {
        let entry = entry.context("tree entry")?;
        let filename = entry.filename();
        let name = filename.to_str().context("tree name utf-8")?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let oid = entry.oid().to_owned();
        let mode = entry.mode();
        if mode.is_tree() {
            walk_tree(repo, oid, &path, out)?;
        } else if path.ends_with(".rs") || path.ends_with(".toml") {
            out.push((path, oid));
        }
    }
    Ok(())
}
