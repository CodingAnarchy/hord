//! Conversions between `hord.proto`'s wire strings and [`hord_core`] values.
//!
//! An `ObjectId` (and so a `ChangeId` or `SnapshotId`) travels as 64
//! lowercase hex digits, a [`NodeId`] as ULID text, a [`RepoPath`] as its
//! `/`-joined display form. Parsing a malformed one is
//! [`ApiError::InvalidArgument`] naming the field.

use hord_core::{Actor, Bytes, EvidenceKind, EvidenceResult, NodeId, ObjectId, RepoPath};

use crate::proto;
use crate::{ApiError, ApiResult};

/// Wire form of an [`ObjectId`].
#[must_use]
pub fn id(id: ObjectId) -> String {
    id.to_hex()
}

/// Parse the `field` of a request as an [`ObjectId`].
pub fn object_id(field: &str, text: &str) -> ApiResult<ObjectId> {
    text.parse()
        .map_err(|_| ApiError::InvalidArgument(format!("{field}: not an object id: {text:?}")))
}

/// Parse every entry of a repeated id field.
pub fn object_ids(field: &str, texts: &[String]) -> ApiResult<Vec<ObjectId>> {
    texts.iter().map(|t| object_id(field, t)).collect()
}

/// Parse an optional id field; an empty string counts as unset.
pub fn optional_object_id(field: &str, text: Option<&str>) -> ApiResult<Option<ObjectId>> {
    match text {
        None | Some("") => Ok(None),
        Some(text) => object_id(field, text).map(Some),
    }
}

/// Parse the `field` of a request as a [`NodeId`].
pub fn node_id(field: &str, text: &str) -> ApiResult<NodeId> {
    text.parse()
        .map_err(|_| ApiError::InvalidArgument(format!("{field}: not a node id: {text:?}")))
}

/// Parse the `field` of a request as a [`RepoPath`].
pub fn repo_path(field: &str, text: &str) -> ApiResult<RepoPath> {
    text.parse()
        .map_err(|_| ApiError::InvalidArgument(format!("{field}: not a path: {text:?}")))
}

/// A [`proto::NodeRef`] with only the id.
#[must_use]
pub fn node_ref(node: NodeId) -> proto::NodeRef {
    proto::NodeRef {
        id: node.to_string(),
        name: None,
        path: None,
    }
}

/// Wire form of an [`Actor`].
#[must_use]
pub fn actor(actor: &Actor) -> proto::Actor {
    let kind = match actor {
        Actor::Human { id } => proto::actor::Kind::Human(proto::Human { id: id.clone() }),
        Actor::Agent {
            id,
            model,
            model_hash,
            harness,
        } => proto::actor::Kind::Agent(proto::Agent {
            id: id.clone(),
            model: model.clone(),
            model_hash: model_hash.as_slice().to_vec(),
            harness: harness.clone(),
        }),
    };
    proto::Actor { kind: Some(kind) }
}

/// Parse a wire [`proto::Actor`]; one with no kind is invalid.
pub fn actor_from(field: &str, actor: &proto::Actor) -> ApiResult<Actor> {
    match &actor.kind {
        Some(proto::actor::Kind::Human(h)) => Ok(Actor::Human { id: h.id.clone() }),
        Some(proto::actor::Kind::Agent(a)) => Ok(Actor::Agent {
            id: a.id.clone(),
            model: a.model.clone(),
            model_hash: Bytes::new(a.model_hash.clone()),
            harness: a.harness.clone(),
        }),
        None => Err(ApiError::InvalidArgument(format!(
            "{field}: actor has no kind"
        ))),
    }
}

/// The id of a wire actor, human or agent; empty when it has no kind.
#[must_use]
pub fn actor_id(actor: &proto::Actor) -> &str {
    match &actor.kind {
        Some(proto::actor::Kind::Human(h)) => &h.id,
        Some(proto::actor::Kind::Agent(a)) => &a.id,
        None => "",
    }
}

/// Wire form of an [`EvidenceKind`].
#[must_use]
pub fn evidence_kind(kind: &EvidenceKind) -> proto::EvidenceKind {
    use proto::evidence_kind::Kind;
    let kind = match kind {
        EvidenceKind::Check => Kind::Check(true),
        EvidenceKind::Test => Kind::Test(true),
        EvidenceKind::Bench => Kind::Bench(true),
        EvidenceKind::Lint => Kind::Lint(true),
        EvidenceKind::Review => Kind::Review(true),
        EvidenceKind::Custom(name) => Kind::Custom(name.clone()),
        EvidenceKind::Rebase { submitted } => Kind::Rebase(id(*submitted)),
    };
    proto::EvidenceKind { kind: Some(kind) }
}

/// Wire form of an [`EvidenceResult`].
#[must_use]
pub fn evidence_result(result: &EvidenceResult) -> proto::EvidenceResult {
    use proto::evidence_result::Result as R;
    let result = match result {
        EvidenceResult::Pass => R::Pass(true),
        EvidenceResult::Fail { summary } => R::Fail(summary.clone()),
        EvidenceResult::Skipped { reason } => R::Skipped(reason.clone()),
    };
    proto::EvidenceResult {
        result: Some(result),
    }
}

/// A [`proto::Object`] for canonical CBOR `bytes`, with its id computed.
#[must_use]
pub fn object(bytes: Vec<u8>) -> proto::Object {
    proto::Object {
        id: id(ObjectId::from_canonical(&bytes)),
        cbor: bytes,
    }
}

/// Check that `object`'s id is the hash of its bytes and return that id.
pub fn verified_object(object: &proto::Object) -> ApiResult<ObjectId> {
    let claimed = object_id("objects.id", &object.id)?;
    let actual = ObjectId::from_canonical(&object.cbor);
    if claimed != actual {
        return Err(ApiError::InvalidArgument(format!(
            "objects.id {claimed} is not the hash of its bytes ({actual})"
        )));
    }
    Ok(actual)
}

/// Wrap one [`proto::event::Kind`] as an [`proto::Event`].
#[must_use]
pub fn event(kind: proto::event::Kind) -> proto::Event {
    proto::Event { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_bad_ones_name_the_field() {
        let oid = ObjectId::from_canonical(b"x");
        assert_eq!(object_id("change", &id(oid)).unwrap(), oid);
        let err = object_id("change", "zz").unwrap_err();
        assert!(matches!(&err, ApiError::InvalidArgument(m) if m.starts_with("change:")));
        let node = NodeId::file_root(&"a/b.rs".parse().unwrap());
        assert_eq!(node_id("node", &node.to_string()).unwrap(), node);
        assert!(node_id("node", "not a ulid").is_err());
        assert_eq!(optional_object_id("after", Some("")).unwrap(), None);
    }

    #[test]
    fn actors_round_trip() {
        let agent = Actor::Agent {
            id: "a".into(),
            model: "m".into(),
            model_hash: Bytes::new(vec![1, 2]),
            harness: "h".into(),
        };
        let human = Actor::Human { id: "h".into() };
        for a in [agent, human] {
            let wire = actor(&a);
            assert_eq!(actor_from("actor", &wire).unwrap(), a);
        }
        assert!(actor_from("actor", &proto::Actor::default()).is_err());
    }

    #[test]
    fn an_object_must_hash_to_its_id() {
        let good = object(vec![0x01]);
        assert!(verified_object(&good).is_ok());
        let bad = proto::Object {
            id: good.id.clone(),
            cbor: vec![0x02],
        };
        assert!(matches!(
            verified_object(&bad),
            Err(ApiError::InvalidArgument(_))
        ));
    }
}
