//! Positional 3-way merge of CST children (spec §3.4).
//!
//! Definition-granularity [`Op::Replace`] on the same [`NodeId`] is a hard
//! conflict when the two `normalized` results differ (spec §5.2 rule 2).
//! Statements inside a definition have no [`NodeId`]; they are identified
//! **positionally**. This module 3-way-merges those children so two edits to
//! different statements in one function compose instead of collapsing to a
//! definition-level hard conflict.

use hord_core::{Bytes, ObjectId};
use hord_lang::{NodeTree, prefer_unchanged};

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
            // Ours, unless only theirs changed.
            let kind = *prefer_unchanged(&b.kind, &o.kind, &t.kind).unwrap_or(&o.kind);
            let name = prefer_unchanged(&b.name, &o.name, &t.name)
                .unwrap_or(&o.name)
                .clone();
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
    let mut o_i = 0usize;
    let mut t_i = 0usize;
    let mut a = 0usize;

    loop {
        let o_h = ho.get(o_i).copied();
        let t_h = ht.get(t_i).copied();
        if o_h.is_none() && t_h.is_none() {
            out.extend_from_slice(&sides.base[a..]);
            break;
        }
        let next_a = match (o_h, t_h) {
            (Some(o), Some(t)) => o.a0.min(t.a0),
            (Some(o), None) => o.a0,
            (None, Some(t)) => t.a0,
            (None, None) => unreachable!(),
        };
        if a < next_a {
            out.extend_from_slice(&sides.base[a..next_a]);
            a = next_a;
            continue;
        }

        let (oh, th) = match (o_h.filter(|h| h.a0 == a), t_h.filter(|h| h.a0 == a)) {
            (Some(oh), Some(th)) => (oh, th),
            (Some(oh), None) => match t_h.filter(|th| th.a0 < oh.a1) {
                Some(th) => (oh, th),
                None => {
                    out.extend_from_slice(&sides.ours[oh.b0..oh.b1]);
                    a = oh.a1;
                    o_i += 1;
                    continue;
                }
            },
            (None, Some(th)) => match o_h.filter(|oh| oh.a0 < th.a1) {
                Some(oh) => (oh, th),
                None => {
                    out.extend_from_slice(&sides.theirs[th.b0..th.b1]);
                    a = th.a1;
                    t_i += 1;
                    continue;
                }
            },
            (None, None) => return Err(()),
        };
        take_overlap(store, sides, &mut out, oh, th, &mut a)?;
        o_i += 1;
        t_i += 1;
        skip_consumed(ho, &mut o_i, a);
        skip_consumed(ht, &mut t_i, a);
    }
    Ok(out)
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
    a: &mut usize,
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
    *a = oh.a1;
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
