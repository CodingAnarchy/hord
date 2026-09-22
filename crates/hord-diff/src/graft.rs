//! Copy interned subtrees between [`NodeTree`]s.

use hord_core::ObjectId;
use hord_lang::NodeTree;

use crate::Error;

/// Intern `id` and its descendants from `src` into `dest`.
///
/// Content-addressed: the returned id equals `id` when `src` interned it.
pub(crate) fn graft(dest: &mut NodeTree, src: &NodeTree, id: ObjectId) -> Result<ObjectId, Error> {
    if dest.contains(id) {
        return Ok(id);
    }
    let node = src.get(id).ok_or(Error::MissingNode(id))?;
    let stripped = hord_core::Bytes::from(src.stripped(id).ok_or(Error::MissingNode(id))?);
    for child in &node.children {
        graft(dest, src, *child)?;
    }
    dest.intern(
        node.kind,
        node.lang,
        node.raw.clone(),
        stripped,
        node.children.clone(),
        node.name.clone(),
    )
    .map_err(Error::from)
}

/// Intern every node from `src` into a clone of `dest`.
pub(crate) fn union_trees(a: &NodeTree, b: &NodeTree) -> Result<NodeTree, Error> {
    let mut out = a.clone();
    let mut ids: Vec<ObjectId> = b.iter().map(|(id, _)| id).collect();
    ids.sort();
    for id in ids {
        graft(&mut out, b, id)?;
    }
    Ok(out)
}
