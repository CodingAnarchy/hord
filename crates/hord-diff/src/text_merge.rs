//! Line merge that keeps both sides' disjoint edits.
//!
//! A one-line replacement stays anchored on that line, so an insert on the
//! next line does not collide with it. When both sides replace that same
//! line, the line is merged token by token the same way. This is what turns
//! two overlapping edits inside one function into the combined body.

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
    let merged = merge_lines(&base_l, &ours_l, &theirs_l)?;
    Some(merged.concat())
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

fn merge_lines(base: &[&str], ours: &[&str], theirs: &[&str]) -> Option<Vec<String>> {
    if base.len() > MAX_LINES || ours.len() > MAX_LINES || theirs.len() > MAX_LINES {
        return None;
    }
    let (ins_o, repl_o, del_o) = edits(base, ours)?;
    let (ins_t, repl_t, del_t) = edits(base, theirs)?;
    let mut out = Vec::new();
    for i in 0..=base.len() {
        push_inserts(&mut out, ins_o.get(&i), ins_t.get(&i));
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
            (None, None) => out.push(base[i].to_owned()),
            (Some(lines), None) | (None, Some(lines)) => {
                out.extend(lines.iter().map(|s| (*s).to_owned()));
            }
            (Some(left), Some(right)) if left == right => {
                out.extend(left.iter().map(|s| (*s).to_owned()));
            }
            (Some(left), Some(right)) => {
                let joined_l = left.concat();
                let joined_r = right.concat();
                let merged = merge_tokens(base[i], &joined_l, &joined_r)?;
                let mut text = merged;
                if base[i].ends_with('\n') && !text.ends_with('\n') {
                    text.push('\n');
                }
                out.push(text);
            }
        }
    }
    Some(out)
}

fn push_inserts(out: &mut Vec<String>, ours: Option<&Vec<&str>>, theirs: Option<&Vec<&str>>) {
    let o = ours.map(Vec::as_slice).unwrap_or(&[]);
    let t = theirs.map(Vec::as_slice).unwrap_or(&[]);
    if o == t {
        out.extend(o.iter().map(|s| (*s).to_owned()));
        return;
    }
    out.extend(o.iter().map(|s| (*s).to_owned()));
    for line in t {
        if !o.contains(line) {
            out.push((*line).to_owned());
        }
    }
}

type EditMap<'a> = std::collections::BTreeMap<usize, Vec<&'a str>>;

fn edits<'a>(
    base: &[&'a str],
    side: &[&'a str],
) -> Option<(EditMap<'a>, EditMap<'a>, std::collections::BTreeSet<usize>)> {
    let codes = opcodes(base, side)?;
    let mut ins = std::collections::BTreeMap::new();
    let mut repl = std::collections::BTreeMap::new();
    let mut deleted = std::collections::BTreeSet::new();
    for code in codes {
        match code {
            Code::Insert { at, j1, j2 } => {
                ins.entry(at).or_insert_with(Vec::new).extend(&side[j1..j2]);
            }
            Code::Delete { i1, i2 } => {
                deleted.extend(i1..i2);
            }
            Code::Replace { i1, i2, j1, j2 } => {
                if i2 == i1 + 1 {
                    repl.insert(i1, side[j1..j2].to_vec());
                } else {
                    deleted.extend(i1..i2);
                    ins.entry(i2).or_insert_with(Vec::new).extend(&side[j1..j2]);
                }
            }
        }
    }
    Some((ins, repl, deleted))
}

fn merge_tokens(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let b = tokens(base);
    let o = tokens(ours);
    let t = tokens(theirs);
    if b.len() > MAX_TOKENS || o.len() > MAX_TOKENS || t.len() > MAX_TOKENS {
        return None;
    }
    let (ins_o, repl_o, del_o) = edits(&b, &o)?;
    let (ins_t, repl_t, del_t) = edits(&b, &t)?;
    let mut out = String::new();
    for i in 0..=b.len() {
        append_inserts(&mut out, ins_o.get(&i), ins_t.get(&i));
        if i == b.len() {
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
            (None, None) => out.push_str(b[i]),
            (Some(toks), None) | (None, Some(toks)) => {
                for tok in toks {
                    out.push_str(tok);
                }
            }
            (Some(left), Some(right)) if left == right => {
                for tok in left {
                    out.push_str(tok);
                }
            }
            (Some(_), Some(_)) => return None,
        }
    }
    Some(out)
}

fn append_inserts(out: &mut String, ours: Option<&Vec<&str>>, theirs: Option<&Vec<&str>>) {
    let o = ours.map(Vec::as_slice).unwrap_or(&[]);
    let t = theirs.map(Vec::as_slice).unwrap_or(&[]);
    if o == t {
        for tok in o {
            out.push_str(tok);
        }
        return;
    }
    for tok in o {
        out.push_str(tok);
    }
    for tok in t {
        if !o.contains(tok) {
            out.push_str(tok);
        }
    }
}

fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let ch = rest.chars().next().expect("non-empty");
        if ch.is_whitespace() {
            let end = rest
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            out.push(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            let end = rest
                .char_indices()
                .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_')
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            out.push(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let len = ch.len_utf8();
        out.push(&rest[..len]);
        rest = &rest[len..];
    }
    out
}

enum Code {
    Insert {
        at: usize,
        j1: usize,
        j2: usize,
    },
    Delete {
        i1: usize,
        i2: usize,
    },
    Replace {
        i1: usize,
        i2: usize,
        j1: usize,
        j2: usize,
    },
}

fn opcodes(a: &[&str], b: &[&str]) -> Option<Vec<Code>> {
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        return None;
    }
    let pairs = lcs(a, b);
    let mut out = Vec::new();
    let mut ai = 0usize;
    let mut bi = 0usize;
    for &(aj, bj) in &pairs {
        if ai < aj || bi < bj {
            push_change(&mut out, ai, aj, bi, bj);
        }
        ai = aj + 1;
        bi = bj + 1;
    }
    if ai < a.len() || bi < b.len() {
        push_change(&mut out, ai, a.len(), bi, b.len());
    }
    Some(out)
}

fn push_change(out: &mut Vec<Code>, i1: usize, i2: usize, j1: usize, j2: usize) {
    if i1 == i2 {
        out.push(Code::Insert { at: i1, j1, j2 });
    } else if j1 == j2 {
        out.push(Code::Delete { i1, i2 });
    } else {
        out.push(Code::Replace { i1, i2, j1, j2 });
    }
}

fn lcs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in 0..n {
        for j in 0..m {
            dp[idx(i + 1, j + 1)] = if a[i] == b[j] {
                dp[idx(i, j)] + 1
            } else {
                dp[idx(i + 1, j)].max(dp[idx(i, j + 1)])
            };
        }
    }
    let mut pairs = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[idx(i - 1, j)] >= dp[idx(i, j - 1)] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}
