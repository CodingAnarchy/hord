//! Generic 3-way merge for machine-written TOML (spec §4.4, §5.2).
//!
//! Lockfiles and similar generated files are sets of records written as
//! arrays of tables. A line merge conflicts whenever two sides add records
//! next to each other. This module merges the decoded document instead:
//!
//! - Tables merge key by key, recursively. A value changed on one side wins;
//!   different changes on both sides are [`ConflictKind::BothChanged`];
//!   removing a value the other side changed is [`ConflictKind::DeleteModify`]
//!   (spec §5.2 rule 3).
//! - Arrays of tables listed in [`MergeConfig::keyed_arrays`] merge as keyed
//!   sets. An element's key is the values of its leading scalar pairs (the
//!   rule [`crate::TomlAdapter`] uses to name `[[x]]` elements), as many as
//!   every input needs for its keys to be unique. Additions from either side
//!   are kept, deletions honoured, and an element both sides kept merges as a
//!   table.
//! - Arrays named in [`KeyedArray::set_fields`] merge as sets: additions from
//!   either side are kept, removals from either side honoured.
//!
//! The result is re-emitted in the canonical layout the caller describes
//! ([`Layout`]); formatting and comments other than the leading comment
//! block are not preserved. Values are decoded and printed with the `toml`
//! crate. Nothing here is specific to a package manager; callers supply the
//! per-format configuration (the `Cargo.lock` bridge lives in
//! `hord-lang-rust`, ADR 0013).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use hord_lang::{LangAdapter, prefer_unchanged};
pub use toml::{Table, Value};

use crate::TomlAdapter;

/// How to merge one kind of document.
#[derive(Clone, Debug, Default)]
pub struct MergeConfig {
    /// Arrays of tables merged as keyed sets.
    pub keyed_arrays: Vec<KeyedArray>,
    /// Output layout.
    pub layout: Layout,
}

/// An array of tables merged as a keyed set.
#[derive(Clone, Debug, Default)]
pub struct KeyedArray {
    /// Dotted path from the document root (`package`, `patch.unused`).
    pub path: String,
    /// Keys of array fields inside each element that merge as sets
    /// (`dependencies`). Merged sets are sorted by their printed values.
    pub set_fields: Vec<String>,
    /// Re-keying: when one side removes exactly one element whose first
    /// `rekey_prefix` leading scalars are `P` and adds exactly one element
    /// with the same `P`, the element changed key (a version bump) rather
    /// than being deleted and re-added, so it merges field by field with the
    /// other side's edits. `0` turns this off.
    pub rekey_prefix: usize,
    /// Longest key, in leading scalars (`0` for no limit). Keys start as long
    /// as every input needs to be unique and grow, up to this limit, while
    /// both sides added different elements under one key; at the limit such
    /// elements merge as one (and conflict field by field).
    pub max_key_len: usize,
    /// Output order of elements. `None` sorts by element key.
    pub order: Option<fn(&Table, &Table) -> Ordering>,
}

/// Canonical output layout for [`TomlDoc::emit`].
///
/// The document prints as: the leading comment block; top-level non-table
/// pairs, then a blank line; then each section. An array-of-tables section
/// prints every element as `[[path]]`, its pairs, and a blank line. Arrays
/// inside elements print one item per line. Any other table prints as the
/// `toml` crate formats it.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Keys printed first inside an array-of-tables element, in this order.
    /// Other keys follow in document order.
    pub key_order: Vec<String>,
    /// Dotted section paths printed first, in this order. Other sections
    /// follow in document order.
    pub section_order: Vec<String>,
    /// Drop empty arrays inside array-of-tables elements.
    pub omit_empty_arrays: bool,
    /// Prefix for each item of a multi-line array.
    pub array_indent: String,
    /// End the file with exactly one newline.
    pub trim_trailing_blank_lines: bool,
}

/// Line terminator of a document. The merge keeps it, so a file checked out
/// with CRLF (Windows `core.autocrlf`) re-emits with CRLF.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LineEnding {
    /// `\n`.
    #[default]
    Lf,
    /// `\r\n`.
    CrLf,
}

impl LineEnding {
    /// The terminator of the first line in `text`, or [`Self::Lf`] when
    /// `text` has no line break.
    #[must_use]
    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(i) if text[..i].ends_with('\r') => Self::CrLf,
            _ => Self::Lf,
        }
    }
}

/// A decoded TOML document: its leading comment block and its table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TomlDoc {
    /// Leading lines that start with `#`, verbatim (without newlines).
    pub header: Vec<String>,
    /// The document's root table, in document order.
    pub table: Table,
    /// Line terminator [`Self::emit`] writes.
    pub line_ending: LineEnding,
}

impl TomlDoc {
    /// Decode `bytes`. Empty input is an empty document.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
        let table: Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let header = text
            .lines()
            .take_while(|l| l.starts_with('#'))
            .map(str::to_owned)
            .collect();
        Ok(Self {
            header,
            table,
            line_ending: LineEnding::detect(text),
        })
    }

    /// Print in `layout`, ending lines with [`Self::line_ending`].
    #[must_use]
    pub fn emit(&self, layout: &Layout) -> String {
        let out = self.emit_lf(layout);
        match self.line_ending {
            LineEnding::Lf => out,
            LineEnding::CrLf => out.replace('\n', "\r\n"),
        }
    }

    fn emit_lf(&self, layout: &Layout) -> String {
        let mut out = String::new();
        for line in &self.header {
            out.push_str(line);
            out.push('\n');
        }
        let mut sections = Vec::new();
        let mut any_top = false;
        for (k, v) in &self.table {
            if is_section(v) {
                collect_sections(vec![k.clone()], v, &mut sections);
            } else {
                out.push_str(&format!("{} = {v}\n", key_text(k)));
                any_top = true;
            }
        }
        if any_top {
            out.push('\n');
        }
        sections.sort_by_key(|(path, _)| {
            let dotted = path.join(".");
            layout
                .section_order
                .iter()
                .position(|s| *s == dotted)
                .unwrap_or(usize::MAX)
        });
        for (path, value) in sections {
            match value {
                Value::Array(items) => {
                    let header: Vec<String> = path.iter().map(|p| key_text(p)).collect();
                    for item in items {
                        if let Value::Table(t) = item {
                            out.push_str(&format!("[[{}]]\n", header.join(".")));
                            emit_element(t, layout, &mut out);
                            out.push('\n');
                        }
                    }
                }
                _ => {
                    let mut wrapped = value.clone();
                    for part in path.iter().rev() {
                        let mut t = Table::new();
                        t.insert(part.clone(), wrapped);
                        wrapped = Value::Table(t);
                    }
                    if let Value::Table(t) = wrapped {
                        out.push_str(&t.to_string());
                    }
                }
            }
        }
        if layout.trim_trailing_blank_lines {
            while out.ends_with("\n\n") {
                out.pop();
            }
        }
        out
    }
}

fn is_array_of_tables(v: &Value) -> bool {
    matches!(v, Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_table))
}

fn is_section(v: &Value) -> bool {
    is_array_of_tables(v) || v.is_table()
}

/// Descend through tables that hold only sections (`[patch]` holding
/// `[[patch.unused]]`), so each prints under its full header.
fn collect_sections<'a>(path: Vec<String>, v: &'a Value, out: &mut Vec<(Vec<String>, &'a Value)>) {
    match v {
        Value::Table(t) if !t.is_empty() && t.values().all(is_section) => {
            for (k, child) in t {
                let mut p = path.clone();
                p.push(k.clone());
                collect_sections(p, child, out);
            }
        }
        _ => out.push((path, v)),
    }
}

fn emit_element(t: &Table, layout: &Layout, out: &mut String) {
    let ordered = layout
        .key_order
        .iter()
        .filter_map(|k| t.get_key_value(k.as_str()))
        .chain(t.iter().filter(|(k, _)| !layout.key_order.contains(k)));
    for (k, v) in ordered {
        match v {
            Value::Array(items) if items.is_empty() && layout.omit_empty_arrays => {}
            Value::Array(items) if !items.is_empty() && !items.iter().any(Value::is_table) => {
                out.push_str(&format!("{} = [\n", key_text(k)));
                for item in items {
                    out.push_str(&format!("{}{item},\n", layout.array_indent));
                }
                out.push_str("]\n");
            }
            _ => out.push_str(&format!("{} = {v}\n", key_text(k))),
        }
    }
}

/// A bare key when TOML allows it, else a quoted one.
fn key_text(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        k.to_owned()
    } else {
        Value::String(k.to_owned()).to_string()
    }
}

/// Which input a [`TomlMergeError::Parse`] refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// The merge base.
    Base,
    /// The landed head.
    Ours,
    /// The proposed change.
    Theirs,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Base => "base",
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        })
    }
}

/// What kind of contention a [`TomlConflict`] is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    /// Both sides changed the value differently.
    BothChanged,
    /// One side removed the value and the other changed it.
    DeleteModify,
    /// Two elements of a keyed array end up with the same key.
    DuplicateKey,
}

/// One point of contention in a TOML merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TomlConflict {
    /// Where: a dotted path, with keyed-array elements as `path[key]`
    /// (`package[serde 1.0.0].checksum`). `#header` is the comment block.
    pub path: String,
    /// What.
    pub kind: ConflictKind,
}

impl fmt::Display for TomlConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ConflictKind::BothChanged => "changed on both sides",
            ConflictKind::DeleteModify => "deleted on one side, changed on the other",
            ConflictKind::DuplicateKey => "duplicate key",
        };
        write!(f, "{}: {what}", self.path)
    }
}

/// Why [`merge_toml`] produced no result.
#[derive(Debug, thiserror::Error)]
pub enum TomlMergeError {
    /// An input is not valid TOML.
    #[error("{side} is not valid TOML: {reason}")]
    Parse {
        /// The offending input.
        side: Side,
        /// The decoder's message.
        reason: String,
    },
    /// The sides contend on the listed points. Hard conflict.
    #[error("TOML merge conflict: {}", join(.0))]
    Conflict(Vec<TomlConflict>),
    /// The merged bytes failed to re-parse (spec §5.2: a hard conflict).
    #[error("merged TOML does not re-parse: {0}")]
    Unparseable(String),
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// 3-way merge of TOML bytes under `config`, re-emitted in its layout.
///
/// Commutative: swapping `ours` and `theirs` gives the same bytes or the
/// same conflicts. The result re-parses under [`TomlAdapter`].
pub fn merge_toml(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    config: &MergeConfig,
) -> Result<Vec<u8>, TomlMergeError> {
    let parse = |side, bytes| {
        TomlDoc::parse(bytes).map_err(|reason| TomlMergeError::Parse { side, reason })
    };
    let merged = merge_docs(
        &parse(Side::Base, base)?,
        &parse(Side::Ours, ours)?,
        &parse(Side::Theirs, theirs)?,
        config,
    )
    .map_err(TomlMergeError::Conflict)?;
    let out = merged.emit(&config.layout).into_bytes();
    check_reparses(&out)?;
    Ok(out)
}

/// Require `bytes` to parse under [`TomlAdapter`] and decode as TOML.
pub fn check_reparses(bytes: &[u8]) -> Result<(), TomlMergeError> {
    TomlAdapter
        .parse(bytes)
        .map_err(|e| TomlMergeError::Unparseable(e.to_string()))?;
    TomlDoc::parse(bytes).map_err(TomlMergeError::Unparseable)?;
    Ok(())
}

/// 3-way merge of decoded documents (see the module docs).
///
/// Returns every conflict found, in document order.
pub fn merge_docs(
    base: &TomlDoc,
    ours: &TomlDoc,
    theirs: &TomlDoc,
    config: &MergeConfig,
) -> Result<TomlDoc, Vec<TomlConflict>> {
    let mut m = Merger {
        config,
        conflicts: Vec::new(),
    };
    let header = match prefer_unchanged(
        &Some(&base.header),
        &Some(&ours.header),
        &Some(&theirs.header),
    )
    .copied()
    {
        Some(h) => h.cloned().unwrap_or_default(),
        None => {
            m.conflict("#header", ConflictKind::BothChanged);
            ours.header.clone()
        }
    };
    // Two values, so a side that changed it wins and the sides never disagree.
    let line_ending = if ours.line_ending == base.line_ending {
        theirs.line_ending
    } else {
        ours.line_ending
    };
    let table = m.table(
        &Path::root(),
        Some(&base.table),
        &ours.table,
        &theirs.table,
        &[],
    );
    if m.conflicts.is_empty() {
        Ok(TomlDoc {
            header,
            table,
            line_ending,
        })
    } else {
        Err(m.conflicts)
    }
}

/// A position: `config` is the dotted path used to look up configuration,
/// `shown` is the path reported in conflicts.
#[derive(Clone)]
struct Path {
    config: String,
    shown: String,
}

impl Path {
    fn root() -> Self {
        Self {
            config: String::new(),
            shown: String::new(),
        }
    }

    fn child(&self, key: &str) -> Self {
        let dot = |p: &str| {
            if p.is_empty() {
                key.to_owned()
            } else {
                format!("{p}.{key}")
            }
        };
        Self {
            config: dot(&self.config),
            shown: dot(&self.shown),
        }
    }

    fn element(&self, key: &ElementKey) -> Self {
        Self {
            config: self.config.clone(),
            shown: format!("{}[{}]", self.shown, key_label(key)),
        }
    }
}

/// Leading `(key, value)` scalars of an element, as far as the array's key
/// length.
type ElementKey = Vec<(String, String)>;

fn key_label(key: &ElementKey) -> String {
    key.iter()
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The element's leading scalar pairs (up to the first array or table).
fn leading_scalars(t: &Table) -> ElementKey {
    t.iter()
        .take_while(|(_, v)| !matches!(v, Value::Array(_) | Value::Table(_)))
        .map(|(k, v)| (k.clone(), scalar_label(v)))
        .collect()
}

fn scalar_label(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Keys absent from `base`, in the order both sides agree on, else sorted
/// (so the merge stays commutative).
fn fresh_keys<'a>(base: Option<&Table>, ours: &'a Table, theirs: &'a Table) -> Vec<&'a String> {
    let fresh = |first: &'a Table, second: &'a Table| -> Vec<&'a String> {
        let mut out: Vec<&String> = Vec::new();
        for k in first.keys().chain(second.keys()) {
            if !base.is_some_and(|b| b.contains_key(k.as_str())) && !out.contains(&k) {
                out.push(k);
            }
        }
        out
    };
    let (a, b) = (fresh(ours, theirs), fresh(theirs, ours));
    if a == b {
        a
    } else {
        let mut sorted = a;
        sorted.sort();
        sorted
    }
}

/// Shortest prefix length that makes every element's key unique.
fn unique_prefix(keys: &[ElementKey]) -> usize {
    let longest = keys.iter().map(Vec::len).max().unwrap_or(0);
    (1..=longest.max(1))
        .find(|&n| {
            let mut seen = BTreeSet::new();
            keys.iter().all(|k| seen.insert(&k[..n.min(k.len())]))
        })
        .unwrap_or(longest)
}

struct Merger<'c> {
    config: &'c MergeConfig,
    conflicts: Vec<TomlConflict>,
}

impl Merger<'_> {
    fn conflict(&mut self, path: &str, kind: ConflictKind) {
        self.conflicts.push(TomlConflict {
            path: path.to_owned(),
            kind,
        });
    }

    /// Merge two tables key by key. Keys keep the base order; keys new on
    /// either side follow, sorted (so the result is commutative).
    fn table(
        &mut self,
        path: &Path,
        base: Option<&Table>,
        ours: &Table,
        theirs: &Table,
        sets: &[String],
    ) -> Table {
        let mut keys: Vec<&String> = base.map(|b| b.keys().collect()).unwrap_or_default();
        keys.extend(fresh_keys(base, ours, theirs));
        let mut out = Table::new();
        for key in keys {
            let b = base.and_then(|b| b.get(key.as_str()));
            let (o, t) = (ours.get(key.as_str()), theirs.get(key.as_str()));
            let is_set = sets.contains(key);
            if let Some(v) = self.value(&path.child(key), b, o, t, is_set) {
                out.insert(key.clone(), v);
            }
        }
        out
    }

    fn value(
        &mut self,
        path: &Path,
        base: Option<&Value>,
        ours: Option<&Value>,
        theirs: Option<&Value>,
        is_set: bool,
    ) -> Option<Value> {
        if let Some(v) = prefer_unchanged(&base, &ours, &theirs).copied() {
            return v.cloned();
        }
        let config = self.config;
        let keyed = config.keyed_arrays.iter().find(|k| k.path == path.config);
        match (base, ours, theirs) {
            (_, Some(Value::Array(o)), Some(Value::Array(t))) if keyed.is_some() => {
                let b = match base {
                    Some(Value::Array(b)) => b.as_slice(),
                    _ => &[],
                };
                self.keyed(path, keyed.expect("checked"), b, o, t)
                    .map(Value::Array)
            }
            (None | Some(Value::Array(_)), Some(Value::Array(o)), Some(Value::Array(t)))
                if is_set =>
            {
                let b = match base {
                    Some(Value::Array(b)) => b.as_slice(),
                    _ => &[],
                };
                Some(Value::Array(merge_set(b, o, t)))
            }
            (None | Some(Value::Table(_)), Some(Value::Table(o)), Some(Value::Table(t))) => {
                let b = base.and_then(Value::as_table);
                Some(Value::Table(self.table(path, b, o, t, &[])))
            }
            (_, None, _) | (_, _, None) => {
                self.conflict(&path.shown, ConflictKind::DeleteModify);
                ours.cloned()
            }
            _ => {
                self.conflict(&path.shown, ConflictKind::BothChanged);
                ours.cloned()
            }
        }
    }

    /// Merge an array of tables as a keyed set.
    fn keyed(
        &mut self,
        path: &Path,
        spec: &KeyedArray,
        base: &[Value],
        ours: &[Value],
        theirs: &[Value],
    ) -> Option<Vec<Value>> {
        let tables = |vs: &[Value]| -> Option<Vec<Table>> {
            vs.iter().map(|v| v.as_table().cloned()).collect()
        };
        let (Some(b), Some(o), Some(t)) = (tables(base), tables(ours), tables(theirs)) else {
            self.conflict(&path.shown, ConflictKind::BothChanged);
            return None;
        };
        let len = key_len(spec, &b, &o, &t);
        let key_of = |t: &Table| -> ElementKey {
            let mut k = leading_scalars(t);
            k.truncate(len);
            k
        };
        let mut index = |side: &[Table]| -> BTreeMap<ElementKey, Table> {
            let mut out = BTreeMap::new();
            for t in side {
                let key = key_of(t);
                if out.insert(key.clone(), t.clone()).is_some() {
                    self.conflict(&path.element(&key).shown, ConflictKind::DuplicateKey);
                }
            }
            out
        };
        let b = index(&b);
        let o = rekey(&b, index(&o), spec.rekey_prefix);
        let t = rekey(&b, index(&t), spec.rekey_prefix);

        let ids: BTreeSet<&ElementKey> = b.keys().chain(o.keys()).chain(t.keys()).collect();
        let mut merged: Vec<(ElementKey, Table)> = Vec::new();
        for id in ids {
            let at = path.element(id);
            let (be, oe, te) = (b.get(id), o.get(id), t.get(id));
            let result = match (oe, te) {
                (Some(oe), Some(te)) => Some(self.table(&at, be, oe, te, &spec.set_fields)),
                (Some(kept), None) | (None, Some(kept)) => match be {
                    None => Some(kept.clone()),
                    Some(be) if be == kept => None,
                    Some(_) => {
                        self.conflict(&at.shown, ConflictKind::DeleteModify);
                        None
                    }
                },
                (None, None) => None,
            };
            if let Some(r) = result {
                // The merged key, read by the identity's key names so it does
                // not depend on the merged table's key order.
                let key: ElementKey = id
                    .iter()
                    .map(|(k, _)| {
                        (
                            k.clone(),
                            r.get(k.as_str()).map(scalar_label).unwrap_or_default(),
                        )
                    })
                    .collect();
                match merged.iter().find(|(k, _)| *k == key) {
                    None => merged.push((key, r)),
                    Some((_, same)) if *same == r => {}
                    Some(_) => self.conflict(&path.element(&key).shown, ConflictKind::DuplicateKey),
                }
            }
        }
        match spec.order {
            Some(order) => merged.sort_by(|(_, a), (_, b)| order(a, b)),
            None => merged.sort_by(|(a, _), (b, _)| a.cmp(b)),
        }
        Some(merged.into_iter().map(|(_, t)| Value::Table(t)).collect())
    }
}

/// Key length for one keyed array (see [`KeyedArray::max_key_len`]).
fn key_len(spec: &KeyedArray, base: &[Table], ours: &[Table], theirs: &[Table]) -> usize {
    let scalars = |side: &[Table]| side.iter().map(leading_scalars).collect::<Vec<_>>();
    let (b, o, t) = (scalars(base), scalars(ours), scalars(theirs));
    let longest = b
        .iter()
        .chain(&o)
        .chain(&t)
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    let cap = if spec.max_key_len == 0 {
        longest
    } else {
        spec.max_key_len.min(longest)
    };
    let mut len = [&b, &o, &t]
        .iter()
        .map(|side| unique_prefix(side))
        .max()
        .unwrap_or(1)
        .min(cap.max(1));
    let prefix = |k: &ElementKey, n: usize| k[..n.min(k.len())].to_vec();
    while len < cap {
        let base_keys: BTreeSet<ElementKey> = b.iter().map(|k| prefix(k, len)).collect();
        let added = |side: &[ElementKey]| -> BTreeMap<ElementKey, ElementKey> {
            side.iter()
                .filter(|k| !base_keys.contains(&prefix(k, len)))
                .map(|k| (prefix(k, len), k.clone()))
                .collect()
        };
        let (ao, at) = (added(&o), added(&t));
        if !ao
            .iter()
            .any(|(k, full)| at.get(k).is_some_and(|other| other != full))
        {
            break;
        }
        len += 1;
    }
    len
}

/// Move a side's re-keyed elements (see [`KeyedArray::rekey_prefix`]) back
/// under their base key, so they merge with the other side's edits.
fn rekey(
    base: &BTreeMap<ElementKey, Table>,
    mut side: BTreeMap<ElementKey, Table>,
    prefix: usize,
) -> BTreeMap<ElementKey, Table> {
    if prefix == 0 {
        return side;
    }
    let group = |from: &BTreeMap<ElementKey, Table>, not_in: &BTreeMap<ElementKey, Table>| {
        let mut out: BTreeMap<ElementKey, Vec<ElementKey>> = BTreeMap::new();
        for k in from.keys().filter(|k| !not_in.contains_key(*k)) {
            out.entry(k[..prefix.min(k.len())].to_vec())
                .or_default()
                .push(k.clone());
        }
        out
    };
    let removed = group(base, &side);
    let added = group(&side, base);
    for (p, old) in removed {
        if let (Some(new), [old]) = (added.get(&p), old.as_slice())
            && let [new] = new.as_slice()
            && let Some(t) = side.remove(new)
        {
            side.insert(old.clone(), t);
        }
    }
    side
}

/// Set merge: kept if every side that had it in base still has it, or if
/// either side added it. Sorted by printed value.
fn merge_set(base: &[Value], ours: &[Value], theirs: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for v in base.iter().chain(ours).chain(theirs) {
        if out.contains(v) {
            continue;
        }
        let (o, t) = (ours.contains(v), theirs.contains(v));
        if if base.contains(v) { o && t } else { o || t } {
            out.push(v.clone());
        }
    }
    out.sort_by_cached_key(ToString::to_string);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed_config() -> MergeConfig {
        MergeConfig {
            keyed_arrays: vec![KeyedArray {
                path: "item".into(),
                set_fields: vec!["tags".into()],
                rekey_prefix: 1,
                max_key_len: 0,
                order: None,
            }],
            layout: Layout {
                key_order: vec!["id".into(), "rev".into()],
                section_order: vec!["item".into()],
                omit_empty_arrays: true,
                array_indent: "  ".into(),
                trim_trailing_blank_lines: true,
            },
        }
    }

    fn merge(b: &str, o: &str, t: &str) -> Result<String, TomlMergeError> {
        let config = keyed_config();
        let one = merge_toml(b.as_bytes(), o.as_bytes(), t.as_bytes(), &config);
        let two = merge_toml(b.as_bytes(), t.as_bytes(), o.as_bytes(), &config);
        match (&one, &two) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "not commutative"),
            (Err(TomlMergeError::Conflict(x)), Err(TomlMergeError::Conflict(y))) => {
                assert_eq!(x, y, "conflicts not commutative");
            }
            _ => panic!("asymmetric: {one:?} vs {two:?}"),
        }
        one.map(|b| String::from_utf8(b).expect("merge output is built from UTF-8 inputs"))
    }

    const BASE: &str =
        "# generated\nformat = 1\n\n[[item]]\nid = \"a\"\nrev = 1\ntags = [\n  \"x\",\n]\n";

    #[test]
    fn unchanged_round_trips_canonical_input() {
        assert_eq!(
            merge(BASE, BASE, BASE).expect("merge unchanged input"),
            BASE
        );
    }

    #[test]
    fn line_endings_are_kept() {
        let crlf = |s: &str| s.replace('\n', "\r\n");
        let ours = format!("{BASE}\n[[item]]\nid = \"b\"\nrev = 1\n");
        let theirs = format!("{BASE}\n[[item]]\nid = \"c\"\nrev = 1\n");
        let want =
            format!("{BASE}\n[[item]]\nid = \"b\"\nrev = 1\n\n[[item]]\nid = \"c\"\nrev = 1\n");
        // Every input CRLF: the output is CRLF.
        assert_eq!(
            merge(&crlf(BASE), &crlf(&ours), &crlf(&theirs)).expect("merge all-CRLF inputs"),
            crlf(&want)
        );
        // One side switched to CRLF: that side's ending wins.
        assert_eq!(
            merge(BASE, &crlf(&ours), &theirs).expect("merge with ours switched to CRLF"),
            crlf(&want)
        );
        assert_eq!(LineEnding::detect("a = 1"), LineEnding::Lf);
    }

    #[test]
    fn concurrent_element_additions_compose() {
        let ours = format!("{BASE}\n[[item]]\nid = \"b\"\nrev = 1\n");
        let theirs = format!("{BASE}\n[[item]]\nid = \"c\"\nrev = 1\n");
        let want =
            format!("{BASE}\n[[item]]\nid = \"b\"\nrev = 1\n\n[[item]]\nid = \"c\"\nrev = 1\n");
        assert_eq!(
            merge(BASE, &ours, &theirs).expect("merge ours and theirs"),
            want
        );
    }

    #[test]
    fn set_fields_union_and_honour_removals() {
        let ours = BASE.replace("  \"x\",\n", "  \"w\",\n");
        let theirs = BASE.replace("  \"x\",\n", "  \"x\",\n  \"y\",\n");
        let want = BASE.replace("  \"x\",\n", "  \"w\",\n  \"y\",\n");
        assert_eq!(
            merge(BASE, &ours, &theirs).expect("merge ours and theirs"),
            want
        );
    }

    #[test]
    fn rekeyed_element_merges_with_other_side_edit() {
        let ours = BASE.replace("rev = 1", "rev = 2");
        let theirs = BASE.replace("  \"x\",\n", "  \"x\",\n  \"y\",\n");
        let want = ours.replace("  \"x\",\n", "  \"x\",\n  \"y\",\n");
        assert_eq!(
            merge(BASE, &ours, &theirs).expect("merge ours and theirs"),
            want
        );
    }

    #[test]
    fn conflicting_rekeys_conflict() {
        let ours = BASE.replace("rev = 1", "rev = 2");
        let theirs = BASE.replace("rev = 1", "rev = 3");
        let Err(TomlMergeError::Conflict(c)) = merge(BASE, &ours, &theirs) else {
            panic!("expected conflict");
        };
        assert_eq!(
            c,
            vec![TomlConflict {
                path: "item[a].rev".into(),
                kind: ConflictKind::BothChanged,
            }]
        );
    }

    #[test]
    fn delete_vs_modify_conflicts() {
        let ours = "# generated\nformat = 1\n";
        let theirs = BASE.replace("  \"x\",\n", "  \"z\",\n");
        let Err(TomlMergeError::Conflict(c)) = merge(BASE, ours, &theirs) else {
            panic!("expected conflict");
        };
        assert_eq!(c[0].kind, ConflictKind::DeleteModify);
    }

    #[test]
    fn scalar_and_table_values_merge_three_way() {
        let base = "a = 1\nb = 1\n\n[meta]\nx = 1\n";
        let ours = "a = 2\nb = 1\n\n[meta]\nx = 1\ny = 2\n";
        let theirs = "a = 1\nb = 3\n\n[meta]\nx = 1\nz = 3\n";
        assert_eq!(
            merge(base, ours, theirs).expect("merge ours and theirs"),
            "a = 2\nb = 3\n\n[meta]\nx = 1\ny = 2\nz = 3\n"
        );
    }

    #[test]
    fn element_keys_extend_until_unique() {
        let keys = vec![
            vec![("n".into(), "a".into()), ("v".into(), "1".into())],
            vec![("n".into(), "a".into()), ("v".into(), "2".into())],
            vec![("n".into(), "b".into())],
        ];
        assert_eq!(unique_prefix(&keys), 2);
        assert_eq!(unique_prefix(&keys[2..]), 1);
    }
}
