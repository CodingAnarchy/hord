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

use crate::align::{Span, spans};

const MAX_CHILDREN: usize = 2048;

/// 3-way merge of interned CST nodes. Returns the interned result id.
pub(crate) fn merge_cst(
    store: &mut NodeTree,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<ObjectId, ()> {
    if let Some(&kept) = prefer_unchanged(&base, &ours, &theirs) {
        return Ok(kept);
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

    if o.children.is_empty() || t.children.is_empty() || b.children.is_empty() {
        // A leaf, or a branch paired with a leaf, merges as raw text.
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

/// When one side matches another, the edit (or the shared result) is that side.
fn prefer_unchanged<'a, T: PartialEq + ?Sized>(
    base: &'a T,
    ours: &'a T,
    theirs: &'a T,
) -> Option<&'a T> {
    if ours == theirs || theirs == base {
        Some(ours)
    } else if ours == base {
        Some(theirs)
    } else {
        None
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
    if let Some(kept) = prefer_unchanged(base, ours, theirs) {
        return Ok(kept.to_vec());
    }
    if base.len() > MAX_CHILDREN || ours.len() > MAX_CHILDREN || theirs.len() > MAX_CHILDREN {
        return Err(());
    }

    let ho = spans(base, ours);
    let ht = spans(base, theirs);
    apply_diff3(store, Sides { base, ours, theirs }, &ho, &ht)
}

#[derive(Clone, Copy)]
struct Sides<'a> {
    base: &'a [ObjectId],
    ours: &'a [ObjectId],
    theirs: &'a [ObjectId],
}

fn apply_diff3(
    store: &mut NodeTree,
    sides: Sides<'_>,
    ho: &[Span],
    ht: &[Span],
) -> Result<Vec<ObjectId>, ()> {
    let mut out = Vec::new();
    let mut walk = Walk {
        ho,
        ht,
        o_i: 0,
        t_i: 0,
        pos: Pos { a: 0 },
    };

    loop {
        let o_h = walk.ho.get(walk.o_i).copied();
        let t_h = walk.ht.get(walk.t_i).copied();
        if o_h.is_none() && t_h.is_none() {
            out.extend_from_slice(&sides.base[walk.pos.a..]);
            break;
        }
        let next_a = match (o_h, t_h) {
            (Some(o), Some(t)) => o.a0.min(t.a0),
            (Some(o), None) => o.a0,
            (None, Some(t)) => t.a0,
            (None, None) => unreachable!(),
        };
        if walk.pos.a < next_a {
            out.extend_from_slice(&sides.base[walk.pos.a..next_a]);
            walk.pos.a = next_a;
            continue;
        }

        let o_here = o_h.filter(|h| h.a0 == walk.pos.a);
        let t_here = t_h.filter(|h| h.a0 == walk.pos.a);
        match (o_here, t_here) {
            (Some(oh), Some(th)) => consume_overlap(store, sides, &mut out, &mut walk, oh, th)?,
            (Some(oh), None) => {
                if let Some(th) = t_h.filter(|th| th.a0 < oh.a1) {
                    consume_overlap(store, sides, &mut out, &mut walk, oh, th)?;
                } else {
                    out.extend_from_slice(&sides.ours[oh.b0..oh.b1]);
                    walk.pos.a = oh.a1;
                    walk.o_i += 1;
                }
            }
            (None, Some(th)) => {
                if let Some(oh) = o_h.filter(|oh| oh.a0 < th.a1) {
                    consume_overlap(store, sides, &mut out, &mut walk, oh, th)?;
                } else {
                    out.extend_from_slice(&sides.theirs[th.b0..th.b1]);
                    walk.pos.a = th.a1;
                    walk.t_i += 1;
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

struct Walk<'a> {
    ho: &'a [Span],
    ht: &'a [Span],
    o_i: usize,
    t_i: usize,
    pos: Pos,
}

fn consume_overlap(
    store: &mut NodeTree,
    sides: Sides<'_>,
    out: &mut Vec<ObjectId>,
    walk: &mut Walk<'_>,
    oh: Span,
    th: Span,
) -> Result<(), ()> {
    take_overlap(store, sides, out, oh, th, &mut walk.pos)?;
    walk.o_i += 1;
    walk.t_i += 1;
    skip_consumed(walk.ho, &mut walk.o_i, walk.pos.a);
    skip_consumed(walk.ht, &mut walk.t_i, walk.pos.a);
    Ok(())
}

fn skip_consumed(hunks: &[Span], i: &mut usize, a: usize) {
    while hunks.get(*i).is_some_and(|h| h.a0 < a) {
        *i += 1;
    }
}

fn take_overlap(
    store: &mut NodeTree,
    sides: Sides<'_>,
    out: &mut Vec<ObjectId>,
    oh: Span,
    th: Span,
    pos: &mut Pos,
) -> Result<(), ()> {
    if oh.a0 != th.a0 || oh.a1 != th.a1 {
        return Err(());
    }
    out.extend(merge_gap(
        store,
        &sides.base[oh.a0..oh.a1.min(sides.base.len())],
        &sides.ours[oh.b0..oh.b1],
        &sides.theirs[th.b0..th.b1],
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
    if let Some(kept) = prefer_unchanged(base, ours, theirs) {
        return Ok(kept.to_vec());
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
