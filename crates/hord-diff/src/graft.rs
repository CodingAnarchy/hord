//! Copy interned subtrees between [`NodeTree`]s.

use hord_core::ObjectId;
use hord_lang::{NodeTree, ParseError};

use crate::Error;

/// Copy `id` and its descendants from `src` into `dest` ([`NodeTree::graft`]).
pub(crate) fn graft(dest: &mut NodeTree, src: &NodeTree, id: ObjectId) -> Result<ObjectId, Error> {
    dest.graft(src, id).map_err(|e| match e {
        ParseError::MissingNode(id) => Error::MissingNode(id),
        other => other.into(),
    })
}

/// Intern every node from `src` into a clone of `dest`.
pub(crate) fn union_trees(a: &NodeTree, b: &NodeTree) -> Result<NodeTree, Error> {
    let mut out = a.clone();
    for (id, _) in b.iter() {
        graft(&mut out, b, id)?;
    }
    Ok(out)
}
