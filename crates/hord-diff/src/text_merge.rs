//! Line merge that keeps both sides' disjoint edits.
//!
//! A one-line replacement stays anchored on that line, so an insert on the
//! next line does not collide with it. When both sides replace that same
//! line, the line is merged token by token the same way. This is what turns
//! two overlapping edits inside one function into the combined body.

use std::collections::{BTreeMap, BTreeSet};

use crate::align::{Span, spans};

const MAX_LINES: usize = 4_000;
const MAX_TOKENS: usize = 2_000;

/// A diff3 line merge that takes `ours` (landing order) for every conflict.
///
/// Each side's changes against `base` are grouped into hunks, and changes
/// from the two sides whose base ranges overlap or touch form one conflict
/// hunk. A hunk changed on one side takes that side; a hunk both sides
/// changed takes ours. `None` past the size limit.
pub(crate) fn merge_ours(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base = keep_lines(base);
    let ours = keep_lines(ours);
    let theirs = keep_lines(theirs);
    if base.len() > MAX_LINES || ours.len() > MAX_LINES || theirs.len() > MAX_LINES {
        return None;
    }
    let ours_spans = spans(&base, &ours);
    let theirs_spans = spans(&base, &theirs);
    let mut changes: Vec<(bool, &Span)> = ours_spans
        .iter()
        .map(|s| (true, s))
        .chain(theirs_spans.iter().map(|s| (false, s)))
        .collect();
    changes.sort_by_key(|(is_ours, s)| (s.a0, s.a1, !is_ours));
    let mut out = String::new();
    let mut next = 0usize;
    let mut i = 0usize;
    while i < changes.len() {
        let (g0, mut g1) = (changes[i].1.a0, changes[i].1.a1);
        let mut j = i + 1;
        while j < changes.len() && changes[j].1.a0 <= g1 {
            g1 = g1.max(changes[j].1.a1);
            j += 1;
        }
        let group = &changes[i..j];
        let side = |want_ours: bool| {
            let mut mine = group
                .iter()
                .filter(|(o, _)| *o == want_ours)
                .map(|(_, s)| s);
            let first = mine.next()?;
            let last = mine.next_back().unwrap_or(first);
            Some((first.b0 - (first.a0 - g0), last.b1 + (g1 - last.a1)))
        };
        for line in &base[next..g0] {
            out.push_str(line);
        }
        let taken = match (side(true), side(false)) {
            (Some((b0, b1)), _) => &ours[b0..b1],
            (None, Some((b0, b1))) => &theirs[b0..b1],
            (None, None) => &base[g0..g1],
        };
        for line in taken {
            out.push_str(line);
        }
        next = g1;
        i = j;
    }
    for line in &base[next..] {
        out.push_str(line);
    }
    Some(out)
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

#[cfg(test)]
mod tests {
    use super::merge_ours;

    #[test]
    fn disjoint_hunks_keep_both_sides() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        assert_eq!(
            merge_ours(base, ours, theirs).as_deref(),
            Some("A\nb\nc\nd\nE\n")
        );
    }

    #[test]
    fn a_conflict_takes_ours_whole_hunk() {
        let base = "a\nb\nc\n";
        let ours = "a\nours\nc\n";
        let theirs = "a\ntheirs\nmore\nc\n";
        assert_eq!(
            merge_ours(base, ours, theirs).as_deref(),
            Some("a\nours\nc\n")
        );
    }

    #[test]
    fn touching_changes_are_one_conflict() {
        // Git treats adjacent changes as one hunk: theirs' edit of `c` is
        // dropped with the rest of the conflict.
        let base = "a\nb\nc\nd\n";
        let ours = "a\nB\nc\nd\n";
        let theirs = "a\nb\nC\nd\n";
        assert_eq!(
            merge_ours(base, ours, theirs).as_deref(),
            Some("a\nB\nc\nd\n")
        );
    }

    #[test]
    fn delete_against_edit_and_same_edit() {
        let base = "a\nb\nc\n";
        assert_eq!(
            merge_ours(base, "a\nc\n", "a\nB\nc\n").as_deref(),
            Some("a\nc\n")
        );
        assert_eq!(
            merge_ours(base, "a\nB\nc\n", "a\nB\nc\n").as_deref(),
            Some("a\nB\nc\n")
        );
        assert_eq!(
            merge_ours(base, "a\nb\nc\n", "x\na\nb\nc\ny\n").as_deref(),
            Some("x\na\nb\nc\ny\n")
        );
    }
}
