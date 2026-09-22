//! Positional 3-way merge of CST children (spec §3.4).
//!
//! Definition-granularity [`Op::Replace`] on the same [`NodeId`] is a hard
//! conflict when the two `normalized` results differ (spec §5.2 rule 2).
//! Statements inside a definition have no [`NodeId`]; they are identified
//! **positionally**. This module 3-way-merges those children so two edits to
//! different statements in one function compose instead of collapsing to a
//! definition-level hard conflict.

use hord_core::{Bytes, ObjectId};
use hord_lang::NodeTree;

const MAX_CHILDREN: usize = 2048;

/// 3-way merge of interned CST nodes. Returns the interned result id.
pub(crate) fn merge_cst(
    store: &mut NodeTree,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<ObjectId, ()> {
    if ours == theirs || theirs == base {
        return Ok(ours);
    }
    if ours == base {
        return Ok(theirs);
    }

    let (b, o, t) = match (store.get(base), store.get(ours), store.get(theirs)) {
        (Some(b), Some(o), Some(t)) => (b.clone(), o.clone(), t.clone()),
        _ => return Err(()),
    };

    if o.normalized == t.normalized {
        return Ok(ours);
    }

    if o.kind != t.kind && o.kind != b.kind && t.kind != b.kind {
        return Err(());
    }

    let leaf = o.children.is_empty() && t.children.is_empty() && b.children.is_empty();
    if leaf {
        return merge_leaf(store, ours, theirs, &b, &o, &t);
    }

    if o.children.is_empty() || t.children.is_empty() || b.children.is_empty() {
        // Branch vs leaf: fall back to a line merge of the projected raw.
        return merge_leaf(store, ours, theirs, &b, &o, &t);
    }

    match merge_seq(store, &b.children, &o.children, &t.children) {
        Ok(kids) => {
            let kind = if o.kind == t.kind || o.kind != b.kind {
                o.kind
            } else {
                t.kind
            };
            let name = if o.name == t.name || o.name != b.name {
                o.name.clone()
            } else {
                t.name.clone()
            };
            store
                .intern_branch(kind, o.lang, kids, name)
                .map_err(|_| ())
        }
        Err(()) => merge_leaf(store, ours, theirs, &b, &o, &t),
    }
}

fn merge_leaf(
    store: &mut NodeTree,
    ours_id: ObjectId,
    theirs_id: ObjectId,
    base: &hord_core::Node,
    ours: &hord_core::Node,
    theirs: &hord_core::Node,
) -> Result<ObjectId, ()> {
    if ours.raw == theirs.raw {
        return Ok(ours_id);
    }
    let merged = crate::merge_blob(
        base.raw.as_slice(),
        ours.raw.as_slice(),
        theirs.raw.as_slice(),
    )
    .map_err(|_| ())?;
    if merged.as_slice() == ours.raw.as_slice() {
        return Ok(ours_id);
    }
    if merged.as_slice() == theirs.raw.as_slice() {
        return Ok(theirs_id);
    }
    let kind = if ours.kind == theirs.kind {
        ours.kind
    } else {
        return Err(());
    };
    let raw = Bytes::from(merged);
    store
        .intern(
            kind,
            ours.lang,
            raw.clone(),
            raw,
            Vec::new(),
            ours.name.clone(),
        )
        .map_err(|_| ())
}

fn merge_seq(
    store: &mut NodeTree,
    base: &[ObjectId],
    ours: &[ObjectId],
    theirs: &[ObjectId],
) -> Result<Vec<ObjectId>, ()> {
    if ours == theirs {
        return Ok(ours.to_vec());
    }
    if ours == base {
        return Ok(theirs.to_vec());
    }
    if theirs == base {
        return Ok(ours.to_vec());
    }
    if base.len() > MAX_CHILDREN || ours.len() > MAX_CHILDREN || theirs.len() > MAX_CHILDREN {
        return Err(());
    }

    let ho = hunks(base, ours);
    let ht = hunks(base, theirs);
    apply_diff3(store, Sides { base, ours, theirs }, &ho, &ht)
}

#[derive(Clone, Copy)]
struct Sides<'a> {
    base: &'a [ObjectId],
    ours: &'a [ObjectId],
    theirs: &'a [ObjectId],
}

#[derive(Clone, Copy, Debug)]
struct Hunk {
    a0: usize,
    a1: usize,
    s0: usize,
    s1: usize,
}

fn hunks(base: &[ObjectId], side: &[ObjectId]) -> Vec<Hunk> {
    let pairs = lcs_pairs(base, side);
    let mut out = Vec::new();
    let mut ai = 0usize;
    let mut si = 0usize;
    for &(aj, sj) in &pairs {
        if aj > ai || sj > si {
            out.push(Hunk {
                a0: ai,
                a1: aj,
                s0: si,
                s1: sj,
            });
        }
        ai = aj + 1;
        si = sj + 1;
    }
    if ai < base.len() || si < side.len() {
        out.push(Hunk {
            a0: ai,
            a1: base.len(),
            s0: si,
            s1: side.len(),
        });
    }
    out
}

fn lcs_pairs(a: &[ObjectId], b: &[ObjectId]) -> Vec<(usize, usize)> {
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

fn apply_diff3(
    store: &mut NodeTree,
    sides: Sides<'_>,
    ho: &[Hunk],
    ht: &[Hunk],
) -> Result<Vec<ObjectId>, ()> {
    let mut out = Vec::new();
    let mut pos = Pos { a: 0 };
    let mut o_i = 0usize;
    let mut t_i = 0usize;

    loop {
        let o_h = ho.get(o_i).copied();
        let t_h = ht.get(t_i).copied();
        if o_h.is_none() && t_h.is_none() {
            out.extend_from_slice(&sides.base[pos.a..]);
            break;
        }
        let next_a = match (o_h, t_h) {
            (Some(o), Some(t)) => o.a0.min(t.a0),
            (Some(o), None) => o.a0,
            (None, Some(t)) => t.a0,
            (None, None) => unreachable!(),
        };
        if pos.a < next_a {
            out.extend_from_slice(&sides.base[pos.a..next_a]);
            pos.a = next_a;
            continue;
        }

        let o_here = o_h.filter(|h| h.a0 == pos.a);
        let t_here = t_h.filter(|h| h.a0 == pos.a);
        match (o_here, t_here) {
            (Some(oh), Some(th)) => {
                take_overlap(store, sides, &mut out, oh, th, &mut pos)?;
                o_i += 1;
                t_i += 1;
                skip_consumed(ho, &mut o_i, pos.a);
                skip_consumed(ht, &mut t_i, pos.a);
            }
            (Some(oh), None) => {
                if let Some(th) = t_h.filter(|th| th.a0 < oh.a1) {
                    take_overlap(store, sides, &mut out, oh, th, &mut pos)?;
                    o_i += 1;
                    t_i += 1;
                    skip_consumed(ho, &mut o_i, pos.a);
                    skip_consumed(ht, &mut t_i, pos.a);
                } else {
                    out.extend_from_slice(&sides.ours[oh.s0..oh.s1]);
                    pos.a = oh.a1;
                    o_i += 1;
                }
            }
            (None, Some(th)) => {
                if let Some(oh) = o_h.filter(|oh| oh.a0 < th.a1) {
                    take_overlap(store, sides, &mut out, oh, th, &mut pos)?;
                    o_i += 1;
                    t_i += 1;
                    skip_consumed(ho, &mut o_i, pos.a);
                    skip_consumed(ht, &mut t_i, pos.a);
                } else {
                    out.extend_from_slice(&sides.theirs[th.s0..th.s1]);
                    pos.a = th.a1;
                    t_i += 1;
                }
            }
            (None, None) => return Err(()),
        }
    }
    Ok(out)
}

struct Pos {
    a: usize,
}

fn skip_consumed(hunks: &[Hunk], i: &mut usize, a: usize) {
    while hunks.get(*i).is_some_and(|h| h.a0 < a) {
        *i += 1;
    }
}

fn take_overlap(
    store: &mut NodeTree,
    sides: Sides<'_>,
    out: &mut Vec<ObjectId>,
    oh: Hunk,
    th: Hunk,
    pos: &mut Pos,
) -> Result<(), ()> {
    if oh.a0 != th.a0 || oh.a1 != th.a1 {
        return Err(());
    }
    out.extend(merge_gap(
        store,
        &sides.base[oh.a0..oh.a1.min(sides.base.len())],
        &sides.ours[oh.s0..oh.s1],
        &sides.theirs[th.s0..th.s1],
    )?);
    pos.a = oh.a1;
    Ok(())
}

fn merge_gap(
    store: &mut NodeTree,
    base: &[ObjectId],
    ours: &[ObjectId],
    theirs: &[ObjectId],
) -> Result<Vec<ObjectId>, ()> {
    if ours == theirs {
        return Ok(ours.to_vec());
    }
    if ours == base {
        return Ok(theirs.to_vec());
    }
    if theirs == base {
        return Ok(ours.to_vec());
    }
    if base.is_empty() {
        // Two inserts at the same place: landing order, ours then theirs.
        let mut out = ours.to_vec();
        for id in theirs {
            if !out.contains(id) {
                out.push(*id);
            }
        }
        return Ok(out);
    }
    if ours.len() == theirs.len() && ours.len() == base.len() {
        let mut out = Vec::with_capacity(base.len());
        for i in 0..base.len() {
            out.push(merge_cst(store, base[i], ours[i], theirs[i])?);
        }
        return Ok(out);
    }
    Err(())
}
