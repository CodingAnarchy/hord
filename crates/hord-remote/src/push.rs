//! [`push_change`]: send the objects a proposed change needs to a server.

use hord_api::{ApiError, ApiResult, wire};
use hord_core::{
    ChangeId, ChangeRecord, Evidence, FileIdentity, IdentityEntry, IdentityTree, ObjectId,
    Snapshot, Tree, TreeEntry,
};
use hord_txn::Repo;

use crate::RemoteRepo;

/// What an object is, which says what it names.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Change,
    Snapshot,
    Tree,
    IdentityTree,
    FileIdentity,
    Evidence,
    /// Names nothing that must travel with it (a blob, a toolchain).
    Leaf,
}

/// Send `change` and every object it reaches that `remote` lacks: its
/// result snapshot's trees, identity trees, file identities, and blobs, its
/// evidence, and its toolchain object. Its base and parents are not sent
/// (they came from the server).
///
/// The walk is breadth first and asks the server which objects it has one
/// level at a time, descending only into those it lacks (a stored tree has
/// its subtree). Objects are sent children first, so an interrupted push
/// never leaves a parent without its children. Returns how many objects
/// were sent.
pub async fn push_change(remote: &RemoteRepo, repo: &Repo, change: ChangeId) -> ApiResult<usize> {
    let mut frontier = vec![(change, Kind::Change)];
    let mut send: Vec<Vec<u8>> = Vec::new();
    while !frontier.is_empty() {
        let ids: Vec<ObjectId> = frontier.iter().map(|(id, _)| *id).collect();
        let present = remote.has_all(&ids).await?;
        let missing: Vec<(ObjectId, Kind)> = frontier
            .into_iter()
            .zip(present)
            .filter(|(_, p)| !p)
            .map(|(item, _)| item)
            .collect();
        let local = repo.clone();
        let (bytes, next) = tokio::task::spawn_blocking(move || expand(&local, &missing))
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))??;
        send.extend(bytes);
        frontier = next;
    }
    let count = send.len();
    // Children first: later levels were appended last.
    send.reverse();
    remote
        .store(send.into_iter().map(wire::object).collect())
        .await?;
    Ok(count)
}

/// Objects to visit, with what each is.
type Frontier = Vec<(ObjectId, Kind)>;

/// The local bytes of `items` that exist here, and the objects they name.
fn expand(repo: &Repo, items: &[(ObjectId, Kind)]) -> ApiResult<(Vec<Vec<u8>>, Frontier)> {
    let mut bytes = Vec::new();
    let mut next = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (id, kind) in items {
        let data = match repo.store().get(*id) {
            Ok(data) => data,
            // Not here: the server must already have it, or never needed it
            // (for example a toolchain object written elsewhere).
            Err(hord_store::Error::MissingObject(_)) => continue,
            Err(err) => return Err(ApiError::Internal(err.to_string())),
        };
        let decode_err = |err: hord_encoding::Error| {
            ApiError::Internal(format!("push: object {id} is not a {kind:?}: {err}"))
        };
        let mut name = |child: ObjectId, kind: Kind| {
            if seen.insert(child) {
                next.push((child, kind));
            }
        };
        match kind {
            Kind::Change => {
                let record: ChangeRecord = hord_encoding::decode(&data).map_err(decode_err)?;
                name(record.result, Kind::Snapshot);
                name(record.provenance.toolchain, Kind::Leaf);
                for evidence in &record.evidence {
                    name(*evidence, Kind::Evidence);
                }
            }
            Kind::Snapshot => {
                let snapshot: Snapshot = hord_encoding::decode(&data).map_err(decode_err)?;
                name(snapshot.tree, Kind::Tree);
                if let Some(identity) = snapshot.index.identity {
                    name(identity, Kind::IdentityTree);
                }
                if let Some(edges) = snapshot.index.edges {
                    name(edges, Kind::Leaf);
                }
            }
            Kind::Tree => {
                let tree: Tree = hord_encoding::decode(&data).map_err(decode_err)?;
                for entry in tree.entries.values() {
                    match entry {
                        TreeEntry::Tree(child) => name(*child, Kind::Tree),
                        TreeEntry::Blob(child) | TreeEntry::NodeFile(child) => {
                            name(*child, Kind::Leaf);
                        }
                    }
                }
            }
            Kind::IdentityTree => {
                let tree: IdentityTree = hord_encoding::decode(&data).map_err(decode_err)?;
                for entry in tree.entries.values() {
                    match entry {
                        IdentityEntry::Dir(child) => name(*child, Kind::IdentityTree),
                        IdentityEntry::File(child) => name(*child, Kind::FileIdentity),
                    }
                }
            }
            Kind::FileIdentity => {
                let file: FileIdentity = hord_encoding::decode(&data).map_err(decode_err)?;
                name(file.blob, Kind::Leaf);
            }
            Kind::Evidence => {
                let evidence: Evidence = hord_encoding::decode(&data).map_err(decode_err)?;
                if let Some(log) = evidence.log {
                    name(log, Kind::Leaf);
                }
                name(evidence.toolchain, Kind::Leaf);
            }
            Kind::Leaf => {}
        }
        bytes.push(data);
    }
    Ok((bytes, next))
}
