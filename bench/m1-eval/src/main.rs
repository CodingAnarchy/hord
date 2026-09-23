//! M1 corpus checks (spec §12): lossless parse/project,
//! `apply(base, diff(base, result))` on consecutive first-parent commits,
//! and the labeled 3-way merge corpus in `corpora/merges/`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
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

/// Scored merge cases the M0 corpora can supply (ADR 0006). Spec §12's
/// original 200 counted every mined conflict, including hand edits and
/// hunks that cover two definitions.
const MIN_SCORED_MERGES: u64 = 61;
/// Parsed trees kept across commits. A blob is usually the result of one
/// edit and the base of the next, a handful of commits later.
const PARSE_CACHE_CAP: usize = 1024;
/// Skip the cache for large sources so one file cannot pin the runner's memory.
const PARSE_CACHE_MAX_BYTES: usize = 256 * 1024;

fn merge_gates(t: &Totals, merges_only: bool) -> bool {
    if t.merge_cases == 0 {
        return !merges_only;
    }
    if merges_only && t.merge_cases < MIN_SCORED_MERGES {
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
    let mut skipped_manual = 0u64;
    let mut skipped_coarse = 0u64;
    let mut hard_reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut buckets = [0u64; 5];
    let mut candidates = 0u64;
    for (i, case) in cases.iter().enumerate() {
        match case_class(case)? {
            CaseClass::Manual => {
                skipped_manual += 1;
                continue;
            }
            CaseClass::Coarse => {
                skipped_coarse += 1;
                continue;
            }
            CaseClass::Candidate => candidates += 1,
        }
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
        "[merges] candidates {candidates} skipped-manual {skipped_manual} skipped-coarse {skipped_coarse} auto {auto}/{candidates} match {matched}/{auto} unparseable {unparseable} (byte {} stripped {} norms {} names-only {} other {})",
        buckets[0], buckets[1], buckets[2], buckets[3], buckets[4],
    );
    Ok((candidates, auto, matched, unparseable))
}

/// Why a mined conflict is not in the M1 denominator (ADR 0006).
enum CaseClass {
    /// The merge commit is not `git merge-file --ours`: a hand edit,
    /// reorder, rewrite, or the other side of a hunk.
    Manual,
    /// One git conflict hunk covers two disjoint definitions. The `--ours`
    /// label drops one of them; structural merge keeps both (spec §5.2 rule 1).
    Coarse,
    /// Git's landing-order resolution of hunks that each touch one definition.
    Candidate,
}

/// Score a case only when the label is git's landing-order auto-merge and no
/// conflict hunk covers two disjoint definitions (ADR 0006).
fn case_class(dir: &Path) -> Result<CaseClass> {
    let (ext, base, ours, theirs, result) = match load_case(dir) {
        Ok(files) => files,
        Err(_) => return Ok(CaseClass::Manual),
    };
    let merged = git_merge_file(&base, &ours, &theirs, true)?;
    if merged != result {
        return Ok(CaseClass::Manual);
    }
    let conflicted = git_merge_file(&base, &ours, &theirs, false)?;
    let coarse = match ext {
        "rs" => hunk_covers_disjoint_defs(&RustAdapter, &base, &ours, &theirs, &conflicted),
        "toml" => hunk_covers_disjoint_defs(&TomlAdapter, &base, &ours, &theirs, &conflicted),
        _ => false,
    };
    Ok(if coarse {
        CaseClass::Coarse
    } else {
        CaseClass::Candidate
    })
}

struct DefSpan {
    id: hord_core::NodeId,
    /// Qualified name when the adapter stored one. Empty when it did not.
    name: String,
    start: usize,
    end: usize,
}

/// True when some git conflict hunk overlaps two disjoint definitions.
///
/// Each side is counted on its own. A definition edited on both sides often
/// has a different [`hord_core::NodeId`] once the edit is inside it, because
/// an outer attribute belongs to the next definition (ADR 0011). Those two
/// ids are still one definition when the qualified name matches. Different
/// names, or two definitions on one side, are a coarse hunk (ADR 0006).
fn hunk_covers_disjoint_defs<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
    conflicted: &[u8],
) -> bool {
    let Ok((ours_spans, theirs_spans)) = side_spans(adapter, base_src, ours_src, theirs_src) else {
        return false;
    };
    let mut ours_at = 0usize;
    let mut theirs_at = 0usize;
    for (ours_body, theirs_body) in conflict_bodies(conflicted) {
        // Judge each side on its own. The same definition often has a different
        // `NodeId` on ours and theirs once the edit is inside it (an outer
        // attribute is part of the next definition, ADR 0011). Unioning the
        // ids counted that one definition twice.
        //
        // The conflict body ends with the line ending, which is often the
        // next definition's leading trivia. On a CRLF checkout that ending
        // is `\r\n`; leaving the `\r` still overlaps the following def.
        let ours_body = trim_one_trailing_newline(&ours_body);
        let theirs_body = trim_one_trailing_newline(&theirs_body);
        let ours_hits = locate(ours_src, ours_body, &mut ours_at)
            .filter(|(start, end)| end > start)
            .map(|(start, end)| minimal_defs(&ours_spans, start, end))
            .unwrap_or_default();
        let theirs_hits = locate(theirs_src, theirs_body, &mut theirs_at)
            .filter(|(start, end)| end > start)
            .map(|(start, end)| minimal_defs(&theirs_spans, start, end))
            .unwrap_or_default();
        if ours_hits.len() >= 2 || theirs_hits.len() >= 2 {
            return true;
        }
        if different_definitions(
            ours_src,
            &ours_spans,
            &ours_hits,
            theirs_src,
            &theirs_spans,
            &theirs_hits,
        ) {
            return true;
        }
    }
    false
}

/// Ours and theirs each name one definition, and those are not the same one.
///
/// The same qualified name with a different id is still one definition when
/// the only byte change is a leading attribute or doc comment. A body change
/// keeps the old rule: different ids mean the hunk is coarse.
fn different_definitions(
    ours_src: &[u8],
    ours_spans: &[DefSpan],
    ours_hits: &BTreeSet<hord_core::NodeId>,
    theirs_src: &[u8],
    theirs_spans: &[DefSpan],
    theirs_hits: &BTreeSet<hord_core::NodeId>,
) -> bool {
    let (Some(&ours_id), Some(&theirs_id)) = (only_id(ours_hits), only_id(theirs_hits)) else {
        return false;
    };
    if ours_id == theirs_id {
        return false;
    }
    let ours_name = span_name(ours_spans, ours_id);
    let theirs_name = span_name(theirs_spans, theirs_id);
    if ours_name.is_empty() || ours_name != theirs_name {
        return true;
    }
    let Some(ours_raw) = span_bytes(ours_src, ours_spans, ours_id) else {
        return true;
    };
    let Some(theirs_raw) = span_bytes(theirs_src, theirs_spans, theirs_id) else {
        return true;
    };
    !same_after_leading_attrs(ours_raw, theirs_raw)
}

fn span_bytes<'a>(src: &'a [u8], spans: &[DefSpan], id: hord_core::NodeId) -> Option<&'a [u8]> {
    let span = spans.iter().find(|span| span.id == id)?;
    src.get(span.start..span.end)
}

/// Drop outer attributes and doc comments that sit in front of a definition.
fn same_after_leading_attrs(ours: &[u8], theirs: &[u8]) -> bool {
    without_leading_attrs(ours) == without_leading_attrs(theirs)
}

fn without_leading_attrs(raw: &[u8]) -> &[u8] {
    let mut i = 0usize;
    loop {
        while i < raw.len() && raw[i].is_ascii_whitespace() {
            i += 1;
        }
        if raw[i..].starts_with(b"#[")
            && let Some(end) = end_of_attribute(raw, i)
        {
            i = end;
            continue;
        }
        if raw[i..].starts_with(b"///") || raw[i..].starts_with(b"//!") {
            match raw[i..].iter().position(|byte| *byte == b'\n') {
                Some(nl) => {
                    i += nl + 1;
                    continue;
                }
                None => return &raw[i..],
            }
        }
        break;
    }
    &raw[i..]
}

fn end_of_attribute(raw: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    while i < raw.len() {
        match raw[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn only_id(ids: &BTreeSet<hord_core::NodeId>) -> Option<&hord_core::NodeId> {
    let mut iter = ids.iter();
    let id = iter.next()?;
    iter.next().is_none().then_some(id)
}

fn span_name(spans: &[DefSpan], id: hord_core::NodeId) -> &str {
    spans
        .iter()
        .find(|span| span.id == id)
        .map(|span| span.name.as_str())
        .unwrap_or("")
}

fn side_spans<A: LangAdapter>(
    adapter: &A,
    base_src: &[u8],
    ours_src: &[u8],
    theirs_src: &[u8],
) -> Result<(Vec<DefSpan>, Vec<DefSpan>), ()> {
    let empty = IdentifiedTree::default();
    let base_tree = adapter.parse(base_src).map_err(|_| ())?;
    let base_map = default_identify(adapter, &empty, &base_tree);
    let base = IdentifiedTree::new(base_tree, base_map.nodes);
    let ours_tree = adapter.parse(ours_src).map_err(|_| ())?;
    let ours_map = default_identify(adapter, &base, &ours_tree);
    let ours = IdentifiedTree::new(ours_tree, ours_map.nodes);
    let theirs_tree = adapter.parse(theirs_src).map_err(|_| ())?;
    let theirs_map = default_identify(adapter, &base, &theirs_tree);
    let theirs = IdentifiedTree::new(theirs_tree, theirs_map.nodes);
    Ok((def_spans(adapter, &ours), def_spans(adapter, &theirs)))
}

fn def_spans<A: LangAdapter>(adapter: &A, tree: &IdentifiedTree) -> Vec<DefSpan> {
    let mut out = Vec::new();
    if let Some(root) = tree.tree.root() {
        walk_defs(adapter, &tree.tree, &tree.ids, root, 0, &mut out);
    }
    out
}

fn walk_defs<A: LangAdapter>(
    adapter: &A,
    tree: &hord_lang::NodeTree,
    ids: &BTreeMap<hord_core::ObjectId, hord_core::NodeId>,
    id: hord_core::ObjectId,
    offset: usize,
    out: &mut Vec<DefSpan>,
) -> usize {
    let Some(node) = tree.get(id) else {
        return offset;
    };
    let end = offset + node.raw.len();
    if adapter.is_definition(&node.kind)
        && let Some(nid) = ids.get(&id)
    {
        out.push(DefSpan {
            id: *nid,
            name: node
                .name
                .as_ref()
                .map(|name| name.as_str().to_owned())
                .unwrap_or_default(),
            start: offset,
            end,
        });
    }
    let mut child_at = offset;
    for child in &node.children {
        child_at = walk_defs(adapter, tree, ids, *child, child_at, out);
    }
    end
}

fn minimal_defs(spans: &[DefSpan], start: usize, end: usize) -> BTreeSet<hord_core::NodeId> {
    let hit: Vec<&DefSpan> = spans
        .iter()
        .filter(|span| span.start < end && span.end > start)
        .collect();
    let mut out = BTreeSet::new();
    for span in &hit {
        let contains_other = hit.iter().any(|other| {
            other.id != span.id
                && other.start >= span.start
                && other.end <= span.end
                && (other.start > span.start || other.end < span.end)
        });
        if !contains_other {
            out.insert(span.id);
        }
    }
    out
}

fn trim_one_trailing_newline(body: &[u8]) -> &[u8] {
    let body = body.strip_suffix(b"\n").unwrap_or(body);
    body.strip_suffix(b"\r").unwrap_or(body)
}

fn locate(haystack: &[u8], needle: &[u8], cursor: &mut usize) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((*cursor, *cursor));
    }
    let rest = haystack.get(*cursor..)?;
    let pos = rest
        .windows(needle.len())
        .position(|window| window == needle)?;
    let start = *cursor + pos;
    *cursor = start + needle.len();
    Some((start, *cursor))
}

/// Ours-side and theirs-side bytes of each conflict hunk in `git merge-file` output.
fn conflict_bodies(conflicted: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < conflicted.len() {
        if !conflicted[i..].starts_with(b"<<<<<<<") {
            i += 1;
            continue;
        }
        let Some(after_marker) = line_end(conflicted, i) else {
            break;
        };
        let Some(split) = find_line(conflicted, after_marker, b"=======") else {
            break;
        };
        let Some(after_split) = line_end(conflicted, split) else {
            break;
        };
        let Some(close) = find_line(conflicted, after_split, b">>>>>>>") else {
            break;
        };
        out.push((
            conflicted[after_marker..split].to_vec(),
            conflicted[after_split..close].to_vec(),
        ));
        i = line_end(conflicted, close).unwrap_or(conflicted.len());
    }
    out
}

fn line_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rel = bytes[at..].iter().position(|byte| *byte == b'\n')?;
    Some(at + rel + 1)
}

fn find_line(bytes: &[u8], from: usize, marker: &[u8]) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i..].starts_with(marker)
            && (i == 0 || bytes[i - 1] == b'\n')
            && bytes.get(i + marker.len()).is_none_or(|byte| {
                // Git for Windows writes CRLF, so the byte after `=======` is `\r`.
                matches!(*byte, b'\n' | b'\r' | b' ')
            })
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn git_merge_file(base: &[u8], ours: &[u8], theirs: &[u8], favor_ours: bool) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "hord-merge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir)?;
    let base_p = dir.join("base");
    let ours_p = dir.join("ours");
    let theirs_p = dir.join("theirs");
    fs::write(&base_p, base)?;
    fs::write(&ours_p, ours)?;
    fs::write(&theirs_p, theirs)?;
    let mut cmd = Command::new("git");
    cmd.args(["merge-file", "-p"]);
    if favor_ours {
        cmd.arg("--ours");
    }
    let output = cmd
        .arg(ours_p.to_str().context("ours path")?)
        .arg(base_p.to_str().context("base path")?)
        .arg(theirs_p.to_str().context("theirs path")?)
        .output()
        .context("git merge-file")?;
    let _ = fs::remove_dir_all(&dir);
    if output.stdout.is_empty() && !output.status.success() {
        bail!(
            "git merge-file failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

enum MergeOut {
    Match { kind: LabelMatch },
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
        Ok(_) => Ok(MergeOut::Match {
            kind: LabelMatch::Miss,
        }),
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
    let cache = ParseCache::new(PARSE_CACHE_CAP);
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
                base_oid: old_oid,
                result_oid: new_oid,
                base,
                result,
            });
            if batch.len() >= BATCH {
                let (bn, bf) = drain_batch(name, &cache, &mut batch);
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
    let (bn, bf) = drain_batch(name, &cache, &mut batch);
    n += bn;
    fail += bf;
    eprintln!(
        "[{name}] apply-diff {n} file-pairs, {fail} fail in {:.1}s (parse cache {}/{} hits)",
        start.elapsed().as_secs_f64(),
        cache.hits(),
        cache.lookups(),
    );
    Ok((n, fail))
}

struct FilePair {
    path: String,
    commit_hex: String,
    base_oid: gix::ObjectId,
    result_oid: gix::ObjectId,
    base: Vec<u8>,
    result: Vec<u8>,
}

fn drain_batch(name: &str, cache: &ParseCache, batch: &mut Vec<FilePair>) -> (u64, u64) {
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
                    if let Err(err) = check_apply_diff(cache, job) {
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

/// `lang` separates `.rs` and `.toml` parses of the same blob bytes.
struct ParseCache {
    cap: usize,
    inner: Mutex<ParseCacheInner>,
    hits: AtomicU64,
    lookups: AtomicU64,
}

struct ParseCacheInner {
    order: VecDeque<(gix::ObjectId, u8)>,
    trees: HashMap<(gix::ObjectId, u8), hord_lang::NodeTree>,
}

impl ParseCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(ParseCacheInner {
                order: VecDeque::new(),
                trees: HashMap::new(),
            }),
            hits: AtomicU64::new(0),
            lookups: AtomicU64::new(0),
        }
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    fn tree<A: LangAdapter>(
        &self,
        lang: u8,
        oid: gix::ObjectId,
        adapter: &A,
        bytes: &[u8],
    ) -> Result<hord_lang::NodeTree> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        if bytes.len() <= PARSE_CACHE_MAX_BYTES
            && let Some(tree) = self.cached(lang, oid)
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(tree);
        }
        let tree = adapter.parse(bytes).context("parse")?;
        if bytes.len() <= PARSE_CACHE_MAX_BYTES {
            self.store(lang, oid, tree.clone());
        }
        Ok(tree)
    }

    fn cached(&self, lang: u8, oid: gix::ObjectId) -> Option<hord_lang::NodeTree> {
        let mut inner = self.inner.lock().expect("parse cache");
        let key = (oid, lang);
        let tree = inner.trees.get(&key)?.clone();
        if let Some(pos) = inner.order.iter().position(|item| *item == key) {
            inner.order.remove(pos);
        }
        inner.order.push_back(key);
        Some(tree)
    }

    fn store(&self, lang: u8, oid: gix::ObjectId, tree: hord_lang::NodeTree) {
        let mut inner = self.inner.lock().expect("parse cache");
        let key = (oid, lang);
        if inner.trees.contains_key(&key)
            && let Some(pos) = inner.order.iter().position(|item| *item == key)
        {
            inner.order.remove(pos);
        }
        inner.order.push_back(key);
        inner.trees.insert(key, tree);
        while inner.order.len() > self.cap {
            if let Some(old) = inner.order.pop_front() {
                inner.trees.remove(&old);
            }
        }
    }
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

fn check_apply_diff(cache: &ParseCache, job: &FilePair) -> Result<()> {
    if job.path.ends_with(".rs") {
        check_apply_pair(cache, 1, &RustAdapter, &job.path, job)
    } else if job.path.ends_with(".toml") {
        check_apply_pair(cache, 2, &TomlAdapter, &job.path, job)
    } else {
        bail!("not a .rs/.toml path: {}", job.path);
    }
}

fn check_apply_pair<A: LangAdapter>(
    cache: &ParseCache,
    lang: u8,
    adapter: &A,
    path: &str,
    job: &FilePair,
) -> Result<()> {
    let base_tree = cache.tree(lang, job.base_oid, adapter, &job.base)?;
    let result_tree = cache.tree(lang, job.result_oid, adapter, &job.result)?;
    let empty = IdentifiedTree::default();
    let base_map = default_identify(adapter, &empty, &base_tree);
    let base = IdentifiedTree::new(base_tree, base_map.nodes);
    let mapping = default_identify(adapter, &base, &result_tree);
    let want = result_tree.root();
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

#[cfg(test)]
mod conflict_marker_tests {
    use super::{conflict_bodies, hunk_covers_disjoint_defs};
    use hord_lang_rust::RustAdapter;

    #[test]
    fn lf_conflict_hunks_keep_each_side() {
        let text = b"keep\n<<<<<<< ours\nours line\n=======\ntheirs line\n>>>>>>> theirs\n";
        let hunks = conflict_bodies(text);
        assert_eq!(hunks.len(), 1, "lf hunks");
        assert_eq!(hunks[0].0, b"ours line\n");
        assert_eq!(hunks[0].1, b"theirs line\n");
    }

    #[test]
    fn crlf_conflict_hunks_keep_each_side() {
        let text =
            b"keep\r\n<<<<<<< ours\r\nours line\r\n=======\r\ntheirs line\r\n>>>>>>> theirs\r\n";
        let hunks = conflict_bodies(text);
        assert_eq!(hunks.len(), 1, "crlf hunks");
        assert_eq!(hunks[0].0, b"ours line\r\n");
        assert_eq!(hunks[0].1, b"theirs line\r\n");
    }

    #[test]
    fn crlf_newline_after_one_function_is_not_a_second_definition() {
        let lf_ours = b"fn a() {}\nfn b() {}\n";
        let lf_theirs = b"fn a() { let _x = 1; }\nfn b() {}\n";
        let lf_base = b"fn a() {}\nfn b() {}\n";
        let lf_conflict =
            b"<<<<<<< ours\nfn a() {}\n=======\nfn a() { let _x = 1; }\n>>>>>>> theirs\nfn b() {}\n";
        assert!(
            !hunk_covers_disjoint_defs(&RustAdapter, lf_base, lf_ours, lf_theirs, lf_conflict),
            "lf"
        );

        assert!(
            !hunk_covers_disjoint_defs(
                &RustAdapter,
                &to_crlf(lf_base),
                &to_crlf(lf_ours),
                &to_crlf(lf_theirs),
                &to_crlf(lf_conflict),
            ),
            "crlf"
        );
    }

    /// An outer attribute belongs to the next definition, so the two sides of
    /// an attribute-only hunk do not share a `NodeId`. That is still one
    /// definition, not a coarse hunk.
    #[test]
    fn attribute_only_hunk_is_one_definition() {
        let base = b"struct S {\n    #[cfg(a)]\n    f: u8,\n    g: u8,\n}\n";
        let ours = base;
        let theirs = b"struct S {\n    #[cfg(b)]\n    f: u8,\n    g: u8,\n}\n";
        let conflicted = b"struct S {\n<<<<<<< ours\n    #[cfg(a)]\n=======\n    #[cfg(b)]\n>>>>>>> theirs\n    f: u8,\n    g: u8,\n}\n";
        assert!(
            !hunk_covers_disjoint_defs(&RustAdapter, base, ours, theirs, conflicted),
            "attribute edit is one field"
        );
    }

    #[test]
    fn hunk_mapping_to_two_function_names_is_coarse() {
        let base = b"fn a() {}\nfn b() {}\n";
        let ours = b"fn a() { let _x = 1; }\nfn b() {}\n";
        let theirs = b"fn a() {}\nfn b() { let _y = 2; }\n";
        let conflicted = b"<<<<<<< ours\nfn a() { let _x = 1; }\n=======\nfn b() { let _y = 2; }\n>>>>>>> theirs\n";
        assert!(
            hunk_covers_disjoint_defs(&RustAdapter, base, ours, theirs, conflicted),
            "the two sides name different functions"
        );
    }

    #[test]
    fn one_hunk_over_two_functions_is_coarse() {
        let base = b"fn a() {}\nfn b() {}\n";
        let ours = b"fn a() { let _x = 1; }\nfn b() { let _y = 2; }\n";
        let theirs = base;
        let conflicted = b"<<<<<<< ours\nfn a() { let _x = 1; }\nfn b() { let _y = 2; }\n=======\nfn a() {}\nfn b() {}\n>>>>>>> theirs\n";
        assert!(
            hunk_covers_disjoint_defs(&RustAdapter, base, ours, theirs, conflicted),
            "one side covers both functions"
        );
    }

    /// Windows CI checks `corpora/merges` out with `core.autocrlf=true`.
    /// Classification has to match the LF corpus or the 61-case floor fails.
    #[test]
    #[ignore = "rewrites every merge case; mirrors the Windows checkout"]
    fn crlf_checkout_classifies_like_lf() {
        let dir = super::merges_dir().expect("merges dir");
        let mut cases: Vec<_> = std::fs::read_dir(&dir)
            .expect("read merges")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.is_dir())
            .collect();
        cases.sort();
        let tmp = std::env::temp_dir().join(format!("hord-crlf-merges-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp");
        let mut mismatches = Vec::new();
        for case in &cases {
            let lf = super::case_class(case).expect("lf class");
            let crlf_dir = tmp.join(case.file_name().expect("case name"));
            std::fs::create_dir_all(&crlf_dir).expect("case dir");
            for entry in std::fs::read_dir(case).expect("case files") {
                let entry = entry.expect("file");
                let bytes = std::fs::read(entry.path()).expect("read");
                std::fs::write(crlf_dir.join(entry.file_name()), to_crlf(&bytes)).expect("write");
            }
            let crlf = super::case_class(&crlf_dir).expect("crlf class");
            if std::mem::discriminant(&lf) != std::mem::discriminant(&crlf) {
                mismatches.push(
                    case.file_name()
                        .expect("name")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(mismatches.is_empty(), "CRLF class differs: {mismatches:?}");
    }

    fn to_crlf(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + 8);
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
                out.push(b'\r');
            }
            out.push(*byte);
        }
        out
    }
}

#[cfg(test)]
mod parse_cache_tests {
    use super::ParseCache;
    use hord_lang_rust::RustAdapter;

    fn oid(byte: u8) -> gix::ObjectId {
        let mut bytes = [0u8; 20];
        bytes[0] = byte;
        gix::ObjectId::from_bytes_or_panic(&bytes)
    }

    #[test]
    fn reused_blob_is_a_hit_until_evicted() {
        let cache = ParseCache::new(1);
        let first = b"fn a() {}\n";
        let second = b"fn b() {}\n";
        let kept = cache.tree(1, oid(1), &RustAdapter, first).expect("parse");
        let again = cache.tree(1, oid(1), &RustAdapter, first).expect("hit");
        assert_eq!(cache.hits(), 1);
        assert_eq!(kept.root(), again.root());
        cache.tree(1, oid(2), &RustAdapter, second).expect("evict");
        cache.tree(1, oid(1), &RustAdapter, first).expect("reparse");
        assert_eq!(cache.hits(), 1, "the evicted blob is parsed again");
        assert_eq!(cache.lookups(), 4);
    }
}
