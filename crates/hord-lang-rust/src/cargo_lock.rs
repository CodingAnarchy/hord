//! `Cargo.lock` bridge over the generic TOML adapter and merge (spec §4.4,
//! ADR 0013).
//!
//! Everything Cargo-specific about lockfiles lives here; `hord-lang-toml`
//! knows only TOML. [`CargoLockAdapter`] is [`TomlAdapter`] restricted to
//! `Cargo.lock` paths. [`merge_cargo_lock`] runs
//! [`hord_lang_toml::merge::merge_docs`] with Cargo's configuration and then
//! does the two things the generic merge cannot:
//!
//! - **Dependency spelling.** Cargo writes a dependency as `name`,
//!   `name version`, or `name version (source)`, whichever is the shortest
//!   unambiguous form for the whole file. After the merge every dependency
//!   is resolved to one package and re-spelled for the merged package set.
//! - **Canonical order.** Packages sort by name, semver version, then source
//!   kind and URL (Cargo's `PackageId` order); dependency lists sort as
//!   Cargo's `EncodablePackageId`.
//!
//! The emitted layout mirrors Cargo's `serialize_resolve` / `emit_package`,
//! so a merge of lockfiles Cargo wrote is byte-identical to what Cargo writes
//! in the common cases.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

use hord_core::{LangId, NodeKind, RepoPath};
use hord_lang::{LangAdapter, NodeTree, ParseError, Tier};
use hord_lang_toml::TomlAdapter;
use hord_lang_toml::merge::{
    KeyedArray, Layout, MergeConfig, Side, Table, TomlConflict, TomlDoc, TomlMergeError, Value,
    check_reparses, merge_docs,
};

/// Language id reported by [`CargoLockAdapter`].
pub const CARGO_LOCK_LANG: &str = "cargo-lock";

/// `Cargo.lock` adapter (spec §4.4, Tier 1): [`TomlAdapter`] for paths whose
/// last component is `Cargo.lock`.
///
/// Trees are TOML trees (language `toml`). Each `[[package]]` is named by
/// the generic leading-scalar rule, `package::<name>` or, when several
/// versions are locked, `package::<name> <version>` (plus the source when
/// that is still ambiguous), so its `NodeId` follows the package rather than
/// its array position.
///
/// Merge lockfiles with [`merge_cargo_lock`], not the generic structural
/// merge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CargoLockAdapter;

/// Whether `path` names a `Cargo.lock` (last component exactly `Cargo.lock`).
///
/// Callers route such paths to [`merge_cargo_lock`].
#[must_use]
pub fn is_cargo_lock(path: &RepoPath) -> bool {
    path.components().last().is_some_and(|c| c == "Cargo.lock")
}

impl LangAdapter for CargoLockAdapter {
    /// `cargo-lock` (ADR 0013 amendment), so a lookup by language alone
    /// cannot confuse it with [`TomlAdapter`]. The parsed nodes are TOML
    /// nodes (language `toml`), as the grammar is.
    fn lang(&self) -> LangId {
        LangId::new(CARGO_LOCK_LANG)
    }

    fn tier(&self) -> Tier {
        Tier::Syntax
    }

    fn matches(&self, path: &RepoPath, _head: &[u8]) -> bool {
        is_cargo_lock(path)
    }

    fn parse(&self, bytes: &[u8]) -> Result<NodeTree, ParseError> {
        TomlAdapter.parse(bytes)
    }

    fn is_definition(&self, kind: &NodeKind) -> bool {
        TomlAdapter.is_definition(kind)
    }
}

/// One point of contention in a `Cargo.lock` merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CargoLockConflict {
    /// A conflict from the generic TOML merge. Paths name packages as
    /// `package[<name> <version>]`, for example
    /// `package[serde 1.0.0].version` when both sides upgraded `serde` to
    /// different versions.
    Toml(TomlConflict),
    /// After merging, `package` depends on `dependency`, which no longer
    /// names a locked package (one side removed it).
    DanglingDependency {
        /// The dependent package (`name version`).
        package: String,
        /// The dependency as written.
        dependency: String,
    },
    /// After merging, `dependency` could mean more than one locked package,
    /// and the sides disagree on which (for example both sides added `serde`
    /// at different versions).
    AmbiguousDependency {
        /// The dependent package (`name version`).
        package: String,
        /// The dependency as written.
        dependency: String,
    },
}

impl fmt::Display for CargoLockConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(c) => c.fmt(f),
            Self::DanglingDependency {
                package,
                dependency,
            } => write!(f, "{package} depends on removed `{dependency}`"),
            Self::AmbiguousDependency {
                package,
                dependency,
            } => write!(f, "`{dependency}` of {package} is ambiguous after merge"),
        }
    }
}

/// Why [`merge_cargo_lock`] produced no result.
#[derive(Debug, thiserror::Error)]
pub enum CargoLockMergeError {
    /// An input is not a lockfile this merge understands (not TOML, an
    /// unknown top-level key, a package without name or version, or a
    /// dependency that names no single package). Fall back to the blob
    /// merge.
    #[error("{side} is not a Cargo.lock this merge understands: {reason}")]
    Unsupported {
        /// The offending input.
        side: Side,
        /// What was wrong with it.
        reason: String,
    },
    /// The sides contend on the listed points. Hard conflict.
    #[error("Cargo.lock conflict: {}", join(.0))]
    Conflict(Vec<CargoLockConflict>),
    /// The merged bytes failed to re-parse (spec §5.2: a hard conflict).
    #[error("merged Cargo.lock does not re-parse: {0}")]
    Unparseable(String),
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// 3-way merge of `Cargo.lock` contents (spec §4.4, §12 M3).
///
/// `base` is the common ancestor, `ours` the landed head, `theirs` the
/// proposed change. An empty `base` is an empty lockfile (both sides added
/// the file).
///
/// Semantics (the generic ones are from [`hord_lang_toml::merge`]):
///
/// - `[[package]]` entries are a set keyed by name and version (and source
///   when needed to be unique). Additions from either side are kept;
///   deletions are honoured; deleting a package the other side changed is a
///   conflict.
/// - A package one side moved to a new version (one entry of that name
///   removed, one added) is the same package: the new version merges with
///   the other side's edits to it. Both sides moving it to different
///   versions is a conflict on its `version`.
/// - `checksum`, `replace`, the `version = N` header, the comment header,
///   and each `[metadata]` key merge 3-way.
/// - `dependencies` merge as sets (additions kept, removals honoured), are
///   resolved against the merged packages, and re-spelled the way Cargo
///   would. A dependency whose package is gone is
///   [`CargoLockConflict::DanglingDependency`]; one the sides resolve to
///   different packages is [`CargoLockConflict::AmbiguousDependency`].
///
/// The output is Cargo's layout and order. Merging a lockfile Cargo wrote
/// with itself returns it byte for byte. The result re-parses under
/// [`CargoLockAdapter`]. The merge is commutative: swapping `ours` and
/// `theirs` gives the same bytes, or the same conflicts. It does not
/// re-resolve: a clean result can still be a lockfile Cargo would change
/// (for example two semver-compatible versions of one crate); the verifier
/// is the check for that.
pub fn merge_cargo_lock(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Vec<u8>, CargoLockMergeError> {
    let parse = |side, bytes| -> Result<(TomlDoc, SideIndex), CargoLockMergeError> {
        let unsupported = |reason| CargoLockMergeError::Unsupported { side, reason };
        let doc = TomlDoc::parse(bytes).map_err(unsupported)?;
        let index = validate(&doc).map_err(unsupported)?;
        Ok((doc, index))
    };
    let (b, b_keys) = parse(Side::Base, base)?;
    let (o, o_keys) = parse(Side::Ours, ours)?;
    let (t, t_keys) = parse(Side::Theirs, theirs)?;

    let mut merged = merge_docs(&b, &o, &t, &merge_config()).map_err(|cs| {
        CargoLockMergeError::Conflict(cs.into_iter().map(CargoLockConflict::Toml).collect())
    })?;
    let keys: Vec<PackageKey> = package_tables(&merged)
        .map(|ts| ts.iter().filter_map(|t| package_key(t)).collect())
        .unwrap_or_default();
    let format = Format::of(&merged);
    let speller = Speller::new(&keys, format);
    let sides = [&b_keys, &o_keys, &t_keys];

    let mut conflicts = Vec::new();
    if let Some(Value::Array(packages)) = merged.table.get_mut("package") {
        for package in packages.iter_mut().filter_map(Value::as_table_mut) {
            let Some(Value::Array(deps)) = package.get("dependencies") else {
                continue;
            };
            let owner_key = package_key(package);
            let owner_name = owner_key.as_ref().map_or("", |k| k.name.as_str());
            let owner = owner_key
                .as_ref()
                .map_or_else(String::new, PackageKey::short);
            let mut resolved = BTreeSet::new();
            for dep in deps.iter().filter_map(Value::as_str) {
                match resolve_merged(dep, owner_name, &keys, &sides) {
                    Ok(k) => {
                        resolved.insert(k);
                    }
                    Err(Unresolved::Missing) => {
                        conflicts.push(CargoLockConflict::DanglingDependency {
                            package: owner.clone(),
                            dependency: dep.to_owned(),
                        });
                    }
                    Err(Unresolved::Ambiguous) => {
                        conflicts.push(CargoLockConflict::AmbiguousDependency {
                            package: owner.clone(),
                            dependency: dep.to_owned(),
                        });
                    }
                }
            }
            let mut spelled: Vec<Spelled<'_>> = resolved.iter().map(|k| speller.spell(k)).collect();
            spelled.sort();
            let respelled = spelled
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect();
            package.insert("dependencies".to_owned(), Value::Array(respelled));
        }
    }
    if !conflicts.is_empty() {
        return Err(CargoLockMergeError::Conflict(conflicts));
    }

    let out = merged.emit(&layout(format)).into_bytes();
    check_reparses(&out).map_err(|e| match e {
        TomlMergeError::Unparseable(m) => CargoLockMergeError::Unparseable(m),
        other => CargoLockMergeError::Unparseable(other.to_string()),
    })?;
    Ok(out)
}

/// Cargo's merge configuration for the generic TOML merge.
fn merge_config() -> MergeConfig {
    MergeConfig {
        keyed_arrays: vec![
            KeyedArray {
                path: "package".into(),
                set_fields: vec!["dependencies".into()],
                rekey_prefix: 1,
                max_key_len: 3,
                order: Some(cmp_package_tables),
            },
            KeyedArray {
                path: "patch.unused".into(),
                set_fields: Vec::new(),
                rekey_prefix: 0,
                max_key_len: 3,
                order: Some(cmp_package_tables),
            },
        ],
        layout: layout(Format::V3Plus),
    }
}

/// Cargo's `serialize_resolve` layout.
fn layout(format: Format) -> Layout {
    Layout {
        key_order: [
            "name",
            "version",
            "source",
            "checksum",
            "dependencies",
            "replace",
        ]
        .map(String::from)
        .to_vec(),
        section_order: ["package", "patch.unused", "metadata"]
            .map(String::from)
            .to_vec(),
        omit_empty_arrays: true,
        array_indent: " ".into(),
        // Cargo trims the trailing blank line from V2 on.
        trim_trailing_blank_lines: format >= Format::V2,
    }
}

/// One input's packages, and which dependency strings each package name
/// lists (to recover what a dependency meant on the side that wrote it).
struct SideIndex {
    keys: Vec<PackageKey>,
    deps: Vec<(String, String)>,
}

/// Check an input is a lockfile this merge understands; index it.
fn validate(doc: &TomlDoc) -> Result<SideIndex, String> {
    for key in doc.table.keys() {
        if !matches!(key.as_str(), "version" | "package" | "patch" | "metadata") {
            return Err(format!("unknown top-level key `{key}`"));
        }
    }
    let tables = package_tables(doc)?;
    let keys = tables
        .iter()
        .map(|t| package_key(t).ok_or_else(|| "a package has no name or version".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut listed = Vec::new();
    for (t, key) in tables.iter().zip(&keys) {
        if let Some(deps) = t.get("dependencies") {
            let deps = deps
                .as_array()
                .ok_or_else(|| format!("`dependencies` of {} is not an array", key.short()))?;
            for dep in deps {
                let dep = dep
                    .as_str()
                    .ok_or_else(|| format!("a dependency of {} is not a string", key.short()))?;
                if resolve_dep(dep, &keys).is_none() {
                    return Err(format!(
                        "dependency `{dep}` of {} does not name one package",
                        key.short()
                    ));
                }
                listed.push((key.name.clone(), dep.to_owned()));
            }
        }
    }
    Ok(SideIndex { keys, deps: listed })
}

fn package_tables(doc: &TomlDoc) -> Result<Vec<&Table>, String> {
    match doc.table.get("package") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_table()
                    .ok_or_else(|| "`package` entry is not a table".to_owned())
            })
            .collect(),
        Some(_) => Err("`package` is not an array of tables".into()),
    }
}

/// Identity of one locked package: `(name, version, source)`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageKey {
    name: String,
    version: String,
    source: Option<String>,
}

fn package_key(t: &Table) -> Option<PackageKey> {
    Some(PackageKey {
        name: t.get("name")?.as_str()?.to_owned(),
        version: t.get("version")?.as_str()?.to_owned(),
        source: t.get("source").and_then(Value::as_str).map(str::to_owned),
    })
}

impl PackageKey {
    /// `name version`, for messages.
    fn short(&self) -> String {
        format!("{} {}", self.name, self.version)
    }

    /// Source with the `#precise` fragment removed, as dependency lists
    /// spell it.
    fn source_without_precise(&self) -> Option<&str> {
        self.source
            .as_deref()
            .map(|s| s.split_once('#').map_or(s, |(url, _)| url))
    }
}

/// Cargo's `PackageId` order: name, semver version, then source.
impl Ord for PackageKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| cmp_version(&self.version, &other.version))
            .then_with(|| cmp_source(self.source.as_deref(), other.source.as_deref()))
    }
}

impl PartialOrd for PackageKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn cmp_package_tables(a: &Table, b: &Table) -> Ordering {
    match (package_key(a), package_key(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (a, b) => a.is_some().cmp(&b.is_some()),
    }
}

/// Semver order; unparsable versions sort after parsable ones, as text.
/// Ties break on the raw text so `Ord` agrees with `Eq`.
fn cmp_version(a: &str, b: &str) -> Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => Ordering::Equal,
    }
    .then_with(|| a.cmp(b))
}

/// Cargo's `SourceKind` order: path, registry, sparse, local registry,
/// directory, git; then the URL text.
fn cmp_source(a: Option<&str>, b: Option<&str>) -> Ordering {
    source_rank(a).cmp(&source_rank(b)).then_with(|| a.cmp(&b))
}

fn source_rank(s: Option<&str>) -> u8 {
    match s {
        None => 0,
        Some(s) if s.starts_with("registry+") => 1,
        Some(s) if s.starts_with("sparse+") => 2,
        Some(s) if s.starts_with("local-registry+") => 3,
        Some(s) if s.starts_with("directory+") => 4,
        Some(s) if s.starts_with("git+") => 5,
        Some(_) => 6,
    }
}

/// Lockfile format generation; spelling and trailing-newline rules differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Format {
    /// No `version`, checksums in `[metadata]`; dependencies spelled in full.
    V1,
    /// No `version`, checksums inline; shortest unambiguous spelling.
    V2,
    /// `version = 3` or later.
    V3Plus,
}

impl Format {
    fn of(doc: &TomlDoc) -> Self {
        let v1_metadata = doc
            .table
            .get("metadata")
            .and_then(Value::as_table)
            .is_some_and(|m| m.keys().any(|k| k.starts_with("checksum ")));
        match doc.table.get("version") {
            Some(_) => Self::V3Plus,
            None if v1_metadata => Self::V1,
            None => Self::V2,
        }
    }
}

enum Unresolved {
    Missing,
    Ambiguous,
}

/// Resolve a dependency of the package named `owner` in the merged file.
/// When the merged package set alone does not decide it, use what it meant
/// on each input side where `owner` lists it, keeping only packages still
/// present; those sides must agree.
fn resolve_merged(
    dep: &str,
    owner: &str,
    merged: &[PackageKey],
    sides: &[&SideIndex; 3],
) -> Result<PackageKey, Unresolved> {
    if let Some(k) = resolve_dep(dep, merged) {
        return Ok(k);
    }
    let meant: BTreeSet<PackageKey> = sides
        .iter()
        .filter(|side| side.deps.iter().any(|(o, d)| o == owner && d == dep))
        .filter_map(|side| resolve_dep(dep, &side.keys))
        .filter(|k| merged.contains(k))
        .collect();
    let mut it = meant.into_iter();
    match (it.next(), it.next()) {
        (Some(k), None) => Ok(k),
        (None, _) => Err(Unresolved::Missing),
        (Some(_), Some(_)) => Err(Unresolved::Ambiguous),
    }
}

/// Resolve `name`, `name version`, or `name version (source)` to the one
/// package it denotes in `keys`.
fn resolve_dep(spelled: &str, keys: &[PackageKey]) -> Option<PackageKey> {
    let (name, rest) = spelled.split_once(' ').unwrap_or((spelled, ""));
    let (version, source) = match rest.split_once(' ') {
        Some((v, s)) => (Some(v), Some(s.strip_prefix('(')?.strip_suffix(')')?)),
        None if rest.is_empty() => (None, None),
        None => (Some(rest), None),
    };
    let matches: Vec<&PackageKey> = keys
        .iter()
        .filter(|k| {
            k.name == name
                && version.is_none_or(|v| k.version == v)
                && source.is_none_or(|s| k.source_without_precise() == Some(s))
        })
        .collect();
    match matches.as_slice() {
        [one] => Some((*one).clone()),
        // A path package has no source to spell, so `name version` also
        // denotes it when a registry or git package shares that version.
        many if version.is_some() && source.is_none() => {
            let mut pathless = many.iter().filter(|k| k.source.is_none());
            let found = pathless.next()?;
            pathless.next().is_none().then(|| (*found).clone())
        }
        _ => None,
    }
}

/// Cargo's `EncodablePackageId`: a dependency as the lockfile spells it.
/// Sorts by name, version text, then source.
#[derive(Debug, Eq, PartialEq)]
struct Spelled<'a> {
    name: &'a str,
    version: Option<&'a str>,
    source: Option<&'a str>,
}

impl Ord for Spelled<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name
            .cmp(other.name)
            .then_with(|| self.version.cmp(&other.version))
            .then_with(|| match (self.source, other.source) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (a, b) => cmp_source(a, b),
            })
    }
}

impl PartialOrd for Spelled<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Spelled<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)?;
        if let Some(v) = self.version {
            write!(f, " {v}")?;
        }
        if let Some(s) = self.source {
            write!(f, " ({s})")?;
        }
        Ok(())
    }
}

/// Spells dependencies against one package set (Cargo's
/// `encodable_package_id`): the source only when two packages share a name
/// and version, the version only when a name has several.
struct Speller {
    format: Format,
    /// name → version → number of packages.
    counts: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>,
}

impl Speller {
    fn new(keys: &[PackageKey], format: Format) -> Self {
        let mut counts: std::collections::BTreeMap<_, std::collections::BTreeMap<_, usize>> =
            std::collections::BTreeMap::new();
        for k in keys {
            *counts
                .entry(k.name.clone())
                .or_default()
                .entry(k.version.clone())
                .or_default() += 1;
        }
        Self { format, counts }
    }

    fn spell<'a>(&self, key: &'a PackageKey) -> Spelled<'a> {
        let mut version = Some(key.version.as_str());
        let mut source = key.source_without_precise();
        if self.format >= Format::V2
            && let Some(versions) = self.counts.get(&key.name)
            && versions.get(&key.version) == Some(&1)
        {
            source = None;
            if versions.len() == 1 {
                version = None;
            }
        }
        Spelled {
            name: &key.name,
            version,
            source,
        }
    }
}
