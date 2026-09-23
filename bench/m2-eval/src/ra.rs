//! rust-analyzer find-references oracle (spec §12 M2, ADR 0009).
//!
//! Extracts cargo HEAD into a temp checkout and runs `rust-analyzer lsif`
//! over the whole workspace once. LSIF, not SCIP: both are one batch index,
//! but SCIP is protobuf and would need a decoder dependency, while LSIF is
//! JSON lines. Each LSIF `referenceResult` holds what rust-analyzer's
//! find-references returns for one symbol, split into `definitions` and
//! `references` items. The oracle sites are the `references` items.
//!
//! A site is recalled when the innermost hord definition enclosing it has a
//! reference edge resolved to the sampled definition's `NodeId`. A matching
//! name that does not resolve to that `NodeId` is a miss.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use hord_core::ObjectId;
use hord_lang::{LangAdapter, NameRef, NodeTree, ResolveCtx};
use hord_lang_rust::{ManifestFile, RustAdapter, RustFile};
use serde::Deserialize;

use crate::git;
use crate::snapshot::{FileSnap, Sampled};

pub(crate) struct RaReport {
    /// Sampled definitions.
    pub defs: usize,
    /// Sampled definitions whose name range rust-analyzer indexed.
    pub indexed: usize,
    /// Reference sites rust-analyzer reported for the indexed definitions.
    pub sites: usize,
    /// Sites hord also reports.
    pub hit: usize,
    pub unindexed: Vec<String>,
    pub misses: Vec<Miss>,
}

pub(crate) struct Miss {
    pub def: String,
    pub site: String,
    pub enclosing: String,
    pub reason: &'static str,
}

pub(crate) fn ok(report: &RaReport, sample: usize) -> bool {
    report.defs >= sample && report.sites > 0 && report.hit * 100 >= report.sites * 95
}

/// Index cargo HEAD with rust-analyzer and score hord's edges for `sample`.
pub(crate) fn measure(
    git_dir: &Path,
    files: &[FileSnap],
    manifests: &[crate::snapshot::ManifestSnap],
    sample: &[Sampled],
) -> Result<RaReport> {
    let dir = checkout(git_dir)?;
    ensure_deps(&dir)?;
    eprintln!("[references-ra] rust-analyzer lsif {}", dir.display());
    let lsif = index(&dir)?;
    eprintln!(
        "[references-ra] lsif documents {} ranges {}",
        lsif.doc_paths.len(),
        lsif.starts.len()
    );

    let adapter = RustAdapter;
    let views: Vec<RustFile<'_>> = files
        .iter()
        .map(|file| RustFile {
            path: &file.path,
            tree: &file.tree,
            ids: &file.ids,
        })
        .collect();
    let manifest_views: Vec<ManifestFile<'_>> = manifests
        .iter()
        .map(|manifest| ManifestFile {
            path: &manifest.path,
            bytes: &manifest.bytes,
        })
        .collect();
    let ctx = adapter.resolve_context_with(&views, &manifest_views);
    let by_path: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(i, file)| (file.path.to_string(), i))
        .collect();
    let mut layouts: HashMap<usize, Layout> = HashMap::new();

    // Pass 1: locate every oracle site and its innermost hord definition.
    let mut report = RaReport {
        defs: sample.len(),
        indexed: 0,
        sites: 0,
        hit: 0,
        unindexed: Vec::new(),
        misses: Vec::new(),
    };
    let mut located = Vec::new();
    for def in sample {
        let Some(&fi) = by_path.get(&def.path) else {
            report
                .unindexed
                .push(format!("{} ({})", def.qname, def.path));
            continue;
        };
        let layout = layouts
            .entry(fi)
            .or_insert_with(|| Layout::new(&adapter, &files[fi].tree));
        let range = layout
            .names
            .get(&def.oid)
            .map(|offset| layout.position(*offset))
            .and_then(|pos| lsif.range_at(&def.path, pos));
        let (Some(range), Some(target)) = (range, files[fi].ids.get(&def.oid).copied()) else {
            report
                .unindexed
                .push(format!("{} ({})", def.qname, def.path));
            continue;
        };
        report.indexed += 1;
        for (doc, site) in lsif.references(range) {
            report.sites += 1;
            let site_path = lsif.doc_paths.get(&doc).cloned().unwrap_or_default();
            let pos = lsif.starts[&site];
            let miss = |reason| Miss {
                def: def.qname.clone(),
                site: format!("{site_path}:{}:{}", pos.line + 1, pos.character + 1),
                enclosing: "-".into(),
                reason,
            };
            let Some(&si) = by_path.get(&site_path) else {
                report.misses.push(miss("file not in hord snapshot"));
                continue;
            };
            let layout = layouts
                .entry(si)
                .or_insert_with(|| Layout::new(&adapter, &files[si].tree));
            let Some(enclosing) = layout.offset(pos).and_then(|at| layout.innermost(at)) else {
                report.misses.push(miss("no enclosing hord definition"));
                continue;
            };
            located.push((miss(""), target, si, enclosing));
        }
    }

    // Pass 2: hord edges for each distinct enclosing definition. Each
    // `references` call rebuilds the resolver index, so spread them over
    // threads.
    let keys: Vec<(usize, ObjectId)> = located
        .iter()
        .map(|(_, _, si, enclosing)| (*si, *enclosing))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    eprintln!(
        "[references-ra] sites {} enclosing definitions {}",
        report.sites,
        keys.len()
    );
    let edges = edges_parallel(&adapter, &ctx, files, &keys);

    // Pass 3: score.
    for (mut miss, target, si, enclosing) in located {
        let refs = edges.get(&(si, enclosing)).map_or(&[][..], Vec::as_slice);
        if refs.iter().any(|name| name.resolved == Some(target)) {
            report.hit += 1;
            continue;
        }
        let simple = miss.def.rsplit("::").next().unwrap_or(&miss.def);
        let named = refs.iter().any(|name| {
            let name = name.name.as_str();
            let name = name.strip_prefix('.').unwrap_or(name);
            name.split("::").any(|segment| segment == simple)
        });
        miss.reason = if named {
            "name edge not resolved to definition"
        } else {
            "no edge"
        };
        miss.enclosing = files[si]
            .tree
            .get(enclosing)
            .map(|node| match &node.name {
                Some(name) => format!("{} {}", node.kind.as_str(), name.as_str()),
                None => node.kind.as_str().to_string(),
            })
            .unwrap_or_default();
        report.misses.push(miss);
    }
    Ok(report)
}

/// `references` for each `(file, definition)` key, computed across threads.
fn edges_parallel(
    adapter: &RustAdapter,
    ctx: &ResolveCtx,
    files: &[FileSnap],
    keys: &[(usize, ObjectId)],
) -> HashMap<(usize, ObjectId), Vec<NameRef>> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = keys.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = keys
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|&(si, oid)| {
                            let refs = files[si]
                                .tree
                                .get(oid)
                                .map(|node| adapter.references(ctx, node))
                                .unwrap_or_default();
                            ((si, oid), refs)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("references worker panicked"))
            .collect()
    })
}

// --- cargo checkout --------------------------------------------------------

/// `git archive HEAD` into `$TMPDIR/hord-m2-ra/<sha>`, reused across runs.
fn checkout(git_dir: &Path) -> Result<PathBuf> {
    let head = String::from_utf8(git::git(git_dir, &["rev-parse", "HEAD"])?)?
        .trim()
        .to_string();
    let dir = std::env::temp_dir().join("hord-m2-ra").join(&head);
    let stamp = dir.join(".hord-extracted");
    if stamp.is_file() {
        return Ok(dir);
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut archive = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["archive", "--format=tar", "HEAD"])
        .stdout(Stdio::piped())
        .spawn()
        .context("git archive")?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(&dir)
        .stdin(archive.stdout.take().context("git archive stdout")?)
        .status()
        .context("tar -x")?;
    let archived = archive.wait().context("git archive")?;
    if !archived.success() || !tar.success() {
        bail!("extracting cargo HEAD failed (git {archived}, tar {tar})");
    }
    fs::write(&stamp, b"").context("checkout stamp")?;
    Ok(dir)
}

/// rust-analyzer needs `cargo metadata` to load the workspace. Fetch the
/// locked dependencies once if the registry cache lacks them.
fn ensure_deps(dir: &Path) -> Result<()> {
    let metadata = |dir: &Path| -> Result<std::process::Output> {
        Command::new("cargo")
            .args(["metadata", "--offline", "--format-version", "1"])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("cargo metadata")
    };
    if metadata(dir)?.status.success() {
        return Ok(());
    }
    eprintln!("[references-ra] cargo fetch (cargo's locked dependencies)");
    let fetch = Command::new("cargo")
        .arg("fetch")
        .current_dir(dir)
        .status()
        .context("cargo fetch")?;
    let retry = metadata(dir)?;
    if !fetch.success() || !retry.status.success() {
        bail!(
            "cargo metadata for {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&retry.stderr)
        );
    }
    Ok(())
}

// --- LSIF ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
struct Pos {
    line: u32,
    character: u32,
}

/// The fields of an LSIF vertex or edge this oracle reads.
#[derive(Deserialize)]
struct Line {
    id: u64,
    label: String,
    #[serde(default, rename = "projectRoot")]
    project_root: Option<String>,
    #[serde(default, rename = "positionEncoding")]
    position_encoding: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    start: Option<Pos>,
    #[serde(default, rename = "outV")]
    out_v: Option<u64>,
    #[serde(default, rename = "inV")]
    in_v: Option<u64>,
    #[serde(default, rename = "inVs")]
    in_vs: Vec<u64>,
    #[serde(default)]
    document: Option<u64>,
    #[serde(default)]
    property: Option<String>,
}

#[derive(Default)]
struct Lsif {
    /// Document id → repository path.
    doc_paths: HashMap<u64, String>,
    /// Range id → start position (UTF-16).
    starts: HashMap<u64, Pos>,
    /// `(repository path, start)` → range id, for ranges a document contains.
    by_pos: HashMap<(String, Pos), u64>,
    /// Range → result set.
    next: HashMap<u64, u64>,
    /// Result set → reference result.
    refs: HashMap<u64, u64>,
    /// Reference result → `references` items as `(document, range)`.
    items: HashMap<u64, Vec<(u64, u64)>>,
}

impl Lsif {
    fn read(input: impl Read) -> Result<Self> {
        let mut lsif = Self::default();
        let mut root = None;
        let mut uris = HashMap::new();
        let mut contains = Vec::new();
        for line in BufReader::new(input).lines() {
            let line = line.context("read lsif")?;
            if line.is_empty() {
                continue;
            }
            let entry: Line = serde_json::from_str(&line).context("parse lsif line")?;
            match entry.label.as_str() {
                "metaData" => {
                    if entry.position_encoding.as_deref() != Some("utf-16") {
                        bail!("lsif position encoding {:?}", entry.position_encoding);
                    }
                    let project = entry.project_root.context("lsif projectRoot")?;
                    root = Some(format!("{}/", project.trim_end_matches('/')));
                }
                "document" => {
                    uris.insert(entry.id, entry.uri.context("lsif document uri")?);
                }
                "range" => {
                    lsif.starts
                        .insert(entry.id, entry.start.context("lsif range start")?);
                }
                "contains" => {
                    if let Some(doc) = entry.out_v {
                        contains.push((doc, entry.in_vs));
                    }
                }
                "next" => {
                    if let (Some(out), Some(inv)) = (entry.out_v, entry.in_v) {
                        lsif.next.insert(out, inv);
                    }
                }
                "textDocument/references" => {
                    if let (Some(out), Some(inv)) = (entry.out_v, entry.in_v) {
                        lsif.refs.insert(out, inv);
                    }
                }
                "item" if entry.property.as_deref() == Some("references") => {
                    if let (Some(out), Some(doc)) = (entry.out_v, entry.document) {
                        let sites = lsif.items.entry(out).or_default();
                        sites.extend(entry.in_vs.iter().map(|range| (doc, *range)));
                    }
                }
                _ => {}
            }
        }
        let root = root.context("lsif metaData")?;
        for (id, uri) in uris {
            if let Some(path) = uri.strip_prefix(&root) {
                lsif.doc_paths.insert(id, path.to_string());
            }
        }
        for (doc, ranges) in contains {
            let Some(path) = lsif.doc_paths.get(&doc) else {
                continue;
            };
            for range in ranges {
                if let Some(start) = lsif.starts.get(&range) {
                    lsif.by_pos.insert((path.clone(), *start), range);
                }
            }
        }
        Ok(lsif)
    }

    fn range_at(&self, path: &str, pos: Pos) -> Option<u64> {
        self.by_pos.get(&(path.to_string(), pos)).copied()
    }

    /// Find-references sites for the symbol at `range`, deduplicated.
    fn references(&self, range: u64) -> BTreeSet<(u64, u64)> {
        self.next
            .get(&range)
            .and_then(|set| self.refs.get(set))
            .and_then(|result| self.items.get(result))
            .map(|sites| {
                sites
                    .iter()
                    .copied()
                    .filter(|(_, site)| *site != range)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Run `rust-analyzer lsif` over `dir` and read its stdout as it streams.
fn index(dir: &Path) -> Result<Lsif> {
    let log = dir.with_extension("lsif.log");
    let mut child = Command::new("rust-analyzer")
        .arg("lsif")
        .arg(dir)
        .stdout(Stdio::piped())
        .stderr(File::create(&log).context("lsif log")?)
        .spawn()
        .context("spawn rust-analyzer lsif")?;
    let stdout = child.stdout.take().context("rust-analyzer stdout")?;
    let lsif = match Lsif::read(stdout) {
        Ok(lsif) => lsif,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    let status = child.wait().context("rust-analyzer lsif")?;
    if !status.success() {
        bail!("rust-analyzer lsif exited {status}; see {}", log.display());
    }
    Ok(lsif)
}

// --- hord positions ----------------------------------------------------------

/// Byte layout of one file: line starts, definition spans, and the byte
/// offset of each definition's name token.
struct Layout {
    text: Vec<u8>,
    line_starts: Vec<usize>,
    defs: Vec<(usize, usize, ObjectId)>,
    names: HashMap<ObjectId, usize>,
}

impl Layout {
    fn new(adapter: &RustAdapter, tree: &NodeTree) -> Self {
        let text = tree.to_bytes().as_slice().to_vec();
        let mut line_starts = vec![0];
        line_starts.extend(
            text.iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .map(|(i, _)| i + 1),
        );
        let mut layout = Self {
            text,
            line_starts,
            defs: Vec::new(),
            names: HashMap::new(),
        };
        if let Some(root) = tree.root() {
            layout.walk(adapter, tree, root, 0);
        }
        layout
    }

    /// Record spans under `oid` starting at byte `start`; return its length.
    fn walk(
        &mut self,
        adapter: &RustAdapter,
        tree: &NodeTree,
        oid: ObjectId,
        start: usize,
    ) -> usize {
        let Some(node) = tree.get(oid) else {
            return 0;
        };
        if node.children.is_empty() {
            return node.raw.len();
        }
        let mut at = start;
        let mut name = None;
        for child in &node.children {
            let len = self.walk(adapter, tree, *child, at);
            if name.is_none()
                && let Some(leaf) = tree.get(*child)
                && leaf.children.is_empty()
                && matches!(leaf.kind.as_str(), "identifier" | "type_identifier")
            {
                name = Some(at + token_lead(tree, *child));
            }
            at += len;
        }
        if adapter.is_definition(&node.kind) {
            self.defs.push((start, at, oid));
            if let Some(name) = name {
                self.names.entry(oid).or_insert(name);
            }
        }
        at - start
    }

    fn innermost(&self, at: usize) -> Option<ObjectId> {
        self.defs
            .iter()
            .filter(|(start, end, _)| *start <= at && at < *end)
            .min_by_key(|(start, end, _)| end - start)
            .map(|(_, _, oid)| *oid)
    }

    fn position(&self, offset: usize) -> Pos {
        let line = self.line_starts.partition_point(|start| *start <= offset) - 1;
        let prefix = &self.text[self.line_starts[line]..offset];
        let character = String::from_utf8_lossy(prefix).encode_utf16().count();
        Pos {
            line: line as u32,
            character: character as u32,
        }
    }

    fn offset(&self, pos: Pos) -> Option<usize> {
        let start = *self.line_starts.get(pos.line as usize)?;
        let end = self
            .line_starts
            .get(pos.line as usize + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line = std::str::from_utf8(&self.text[start..end]).ok()?;
        let mut units = 0u32;
        for (i, ch) in line.char_indices() {
            if units >= pos.character {
                return Some(start + i);
            }
            units += ch.len_utf16() as u32;
        }
        (units >= pos.character).then_some(end)
    }
}

/// Offset of a leaf's token inside its `raw`, past attached leading trivia.
fn token_lead(tree: &NodeTree, leaf: ObjectId) -> usize {
    let (Some(node), Some(text)) = (tree.get(leaf), tree.stripped(leaf)) else {
        return 0;
    };
    let raw = node.raw.as_slice();
    if text.is_empty() || text.len() > raw.len() {
        return 0;
    }
    raw.windows(text.len())
        .position(|window| window == text)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsif_reference_sites_skip_the_definition() {
        let input = r#"{"id":0,"type":"vertex","label":"metaData","projectRoot":"file:///w","positionEncoding":"utf-16"}
{"id":1,"type":"vertex","label":"document","uri":"file:///w/src/lib.rs"}
{"id":2,"type":"vertex","label":"range","start":{"line":0,"character":3},"end":{"line":0,"character":6}}
{"id":3,"type":"vertex","label":"range","start":{"line":1,"character":12},"end":{"line":1,"character":15}}
{"id":4,"type":"edge","label":"contains","inVs":[2,3],"outV":1}
{"id":5,"type":"vertex","label":"resultSet"}
{"id":6,"type":"edge","label":"next","inV":5,"outV":2}
{"id":7,"type":"edge","label":"next","inV":5,"outV":3}
{"id":8,"type":"vertex","label":"referenceResult"}
{"id":9,"type":"edge","label":"textDocument/references","inV":8,"outV":5}
{"id":10,"type":"edge","label":"item","document":1,"property":"definitions","inVs":[2],"outV":8}
{"id":11,"type":"edge","label":"item","document":1,"property":"references","inVs":[2,3],"outV":8}
"#;
        let lsif = Lsif::read(input.as_bytes()).expect("lsif");
        let def = lsif
            .range_at(
                "src/lib.rs",
                Pos {
                    line: 0,
                    character: 3,
                },
            )
            .expect("definition range");
        assert_eq!(def, 2);
        let sites: Vec<_> = lsif.references(def).into_iter().collect();
        assert_eq!(sites, vec![(1, 3)]);
    }

    #[test]
    fn layout_maps_utf16_positions_and_innermost_definitions() {
        let src = "fn outer() {\n    let s = \"é\"; inner();\n}\nfn inner() {}\n";
        let adapter = RustAdapter;
        let tree = adapter.parse(src.as_bytes()).expect("parse");
        let layout = Layout::new(&adapter, &tree);
        let call = src.find("inner()").expect("call");
        // `é` is two UTF-8 bytes and one UTF-16 unit.
        let pos = layout.position(call);
        assert_eq!(
            pos,
            Pos {
                line: 1,
                character: 17
            }
        );
        assert_eq!(layout.offset(pos), Some(call));
        let outer = layout.innermost(call).expect("enclosing");
        assert_eq!(
            tree.get(outer).map(|node| node.kind.as_str()),
            Some("function_item")
        );
        let inner_def = src.rfind("inner").expect("def");
        assert!(layout.names.values().any(|offset| *offset == inner_def));
    }
}
