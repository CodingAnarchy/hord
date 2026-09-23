//! Line merge that keeps both sides' disjoint edits.
//!
//! A one-line replacement stays anchored on that line, so an insert on the
//! next line does not collide with it. When both sides replace that same
//! line, the line is merged token by token the same way. This is what turns
//! two overlapping edits inside one function into the combined body.

use std::collections::{BTreeMap, BTreeSet};

use crate::align::spans;

const MAX_LINES: usize = 4_000;
const MAX_TOKENS: usize = 2_000;

/// Git's auto-merge with every conflict hunk taken from `ours` (landing order).
///
/// This is `git merge-file --ours`: non-overlapping edits from both sides,
/// and the landing-order side of each conflict. `None` if `git` is missing
/// or the command fails.
pub(crate) fn git_merge_ours(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    // Unique per call. A shared directory races when tests merge in parallel.
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hord-git-merge-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let base_p = dir.join("base");
    let ours_p = dir.join("ours");
    let theirs_p = dir.join("theirs");
    std::fs::write(&base_p, base).ok()?;
    std::fs::write(&ours_p, ours).ok()?;
    std::fs::write(&theirs_p, theirs).ok()?;
    let output = std::process::Command::new("git")
        .args([
            "merge-file",
            "-p",
            "--ours",
            ours_p.to_str()?,
            base_p.to_str()?,
            theirs_p.to_str()?,
        ])
        .output()
        .ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    if output.stdout.is_empty() && !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Merge three texts. `None` when the same line or token was changed two ways
/// that do not themselves compose.
pub(crate) fn merge_text(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_l = keep_lines(base);
    let ours_l = keep_lines(ours);
    let theirs_l = keep_lines(theirs);
    merge_pieces(
        &base_l,
        &ours_l,
        &theirs_l,
        MAX_LINES,
        |base_line, left, right| {
            let mut text = merge_tokens(base_line, &left.concat(), &right.concat())?;
            if base_line.ends_with('\n') && !text.ends_with('\n') {
                text.push('\n');
            }
            Some(text)
        },
    )
}

fn keep_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn merge_tokens(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let b = tokens(base);
    let o = tokens(ours);
    let t = tokens(theirs);
    merge_pieces(&b, &o, &t, MAX_TOKENS, |_, _, _| None)
}

type EditMap<'a> = BTreeMap<usize, Vec<&'a str>>;

/// 3-way merge of parallel slices.
///
/// `both_changed` runs when the two sides replace the same base element with
/// different slices. Lines pass token merge; tokens pass a closure that
/// refuses the conflict.
fn merge_pieces<'a>(
    base: &[&'a str],
    ours: &[&'a str],
    theirs: &[&'a str],
    limit: usize,
    both_changed: impl Fn(&str, &[&'a str], &[&'a str]) -> Option<String>,
) -> Option<String> {
    if base.len() > limit || ours.len() > limit || theirs.len() > limit {
        return None;
    }
    let (ins_o, repl_o, del_o) = edits(base, ours);
    let (ins_t, repl_t, del_t) = edits(base, theirs);
    let mut out = String::new();
    for i in 0..=base.len() {
        append_inserts(&mut out, ins_o.get(&i), ins_t.get(&i));
        if i == base.len() {
            break;
        }
        let gone_o = del_o.contains(&i);
        let gone_t = del_t.contains(&i);
        if gone_o && gone_t {
            continue;
        }
        if gone_o != gone_t && (repl_o.contains_key(&i) || repl_t.contains_key(&i)) {
            return None;
        }
        if gone_o || gone_t {
            continue;
        }
        match (repl_o.get(&i), repl_t.get(&i)) {
            (None, None) => out.push_str(base[i]),
            (Some(parts), None) | (None, Some(parts)) => {
                for part in parts {
                    out.push_str(part);
                }
            }
            (Some(left), Some(right)) if left == right => {
                for part in left {
                    out.push_str(part);
                }
            }
            (Some(left), Some(right)) => {
                out.push_str(&both_changed(base[i], left, right)?);
            }
        }
    }
    Some(out)
}

fn append_inserts(out: &mut String, ours: Option<&Vec<&str>>, theirs: Option<&Vec<&str>>) {
    let ours = ours.map(Vec::as_slice).unwrap_or(&[]);
    let theirs = theirs.map(Vec::as_slice).unwrap_or(&[]);
    if ours == theirs {
        for piece in ours {
            out.push_str(piece);
        }
        return;
    }
    for piece in ours {
        out.push_str(piece);
    }
    for piece in theirs {
        if !ours.contains(piece) {
            out.push_str(piece);
        }
    }
}

fn edits<'a>(base: &[&'a str], side: &[&'a str]) -> (EditMap<'a>, EditMap<'a>, BTreeSet<usize>) {
    let mut ins: EditMap<'a> = BTreeMap::new();
    let mut repl: EditMap<'a> = BTreeMap::new();
    let mut deleted = BTreeSet::new();
    for span in spans(base, side) {
        if span.a0 == span.a1 {
            ins.entry(span.a0)
                .or_default()
                .extend(&side[span.b0..span.b1]);
        } else if span.b0 == span.b1 {
            deleted.extend(span.a0..span.a1);
        } else if span.a1 == span.a0 + 1 {
            repl.insert(span.a0, side[span.b0..span.b1].to_vec());
        } else {
            deleted.extend(span.a0..span.a1);
            ins.entry(span.a1)
                .or_default()
                .extend(&side[span.b0..span.b1]);
        }
    }
    (ins, repl, deleted)
}

fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let ch = rest.chars().next().expect("non-empty");
        let end = if ch.is_whitespace() {
            scan_while(rest, char::is_whitespace)
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            scan_while(rest, |c| c.is_ascii_alphanumeric() || c == '_')
        } else {
            ch.len_utf8()
        };
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

fn scan_while(rest: &str, pred: impl Fn(char) -> bool) -> usize {
    rest.char_indices()
        .find(|(_, c)| !pred(*c))
        .map(|(i, _)| i)
        .unwrap_or(rest.len())
}
