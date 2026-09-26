//! The `RepoBackend` conformance suite (spec §12 M4, ADR 0024), written
//! once against `&dyn RepoBackend` and run against every implementation:
//! `hord_txn::LocalRepo` now, `RemoteRepo` against `hord serve` later.
//!
//! [`run`] takes a backend over a fresh, empty repository whose lander runs
//! on its own (submitted changes land without further calls) and whose
//! verifier lands clean, disjoint changes. It builds every change it needs
//! from raw objects through [`RepoBackend::put_objects`]: blob-tier files
//! (`.txt`, which no adapter parses) with Tier 0 ops, so the suite does not
//! depend on a workspace or a language adapter. It stops at the first
//! check that fails and returns a [`Failure`] naming it, so a test runs it
//! with `?`.

use std::collections::BTreeMap;
use std::time::Duration;

use hord_core::{
    Actor, Blob, ChangeRecord, Evidence, EvidenceKind, EvidenceResult, IdentityTree, Intent,
    NodeId, ObjectId, Op, Provenance, RepoPath, Snapshot, Timestamp, Tree, TreeEntry, TreeOpKind,
};
use tokio_stream::StreamExt;

use crate::proto::event::Kind;
use crate::{ApiError, EventStream, MAX_BATCH_IDS, RepoBackend, proto, wire};

/// How long to wait for the lander to act on a submission.
pub const LANDING_TIMEOUT: Duration = Duration::from_secs(60);

/// The actor id every change the suite builds is authored by.
pub const ACTOR: &str = "hord-conformance";

/// A conformance check that failed: what was checked and what went wrong.
#[derive(Debug, thiserror::Error)]
#[error("conformance: {0}")]
pub struct Failure(String);

/// The result of a conformance check.
pub type Outcome<T = ()> = Result<T, Failure>;

/// Fail the check with a formatted message unless `cond` holds.
macro_rules! ensure {
    ($cond:expr $(,)?) => {
        ensure!($cond, "{}", stringify!($cond))
    };
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(Failure(format!($($msg)+)));
        }
    };
}

/// Fail the check unless `left == right`, naming both values.
macro_rules! ensure_eq {
    ($left:expr, $right:expr $(,)?) => {
        ensure_eq!($left, $right, "{}", stringify!($left))
    };
    ($left:expr, $right:expr, $($msg:tt)+) => {
        match (&$left, &$right) {
            (left, right) => {
                if !(*left == *right) {
                    return Err(Failure(format!(
                        "{}: {left:?} != {right:?}",
                        format_args!($($msg)+)
                    )));
                }
            }
        }
    };
}

/// Turn a failed call or a missing value into a [`Failure`] naming the
/// check.
trait Check<T> {
    fn check(self, what: &str) -> Outcome<T>;
}

impl<T, E: std::fmt::Display> Check<T> for Result<T, E> {
    fn check(self, what: &str) -> Outcome<T> {
        self.map_err(|e| Failure(format!("{what}: {e}")))
    }
}

impl<T> Check<T> for Option<T> {
    fn check(self, what: &str) -> Outcome<T> {
        self.ok_or_else(|| Failure(format!("{what}: missing")))
    }
}

/// Run every check against `backend`, in order, stopping at the first
/// failure. See the module docs for what the backend must be.
///
/// # Errors
///
/// A [`Failure`] naming the first check the backend did not pass.
pub async fn run(backend: &dyn RepoBackend) -> Outcome {
    let mut fx = Fixture::default();
    empty_repository(backend).await?;
    objects(backend).await?;
    let a = submit_and_land(backend, &mut fx).await?;
    resubmission(backend, a).await?;
    let b = second_change_and_log(backend, &mut fx, a).await?;
    queries(backend, &fx, a, b).await?;
    evidence(backend, &fx, b).await?;
    rejection(backend, &fx).await?;
    arbitrate_unknown(backend).await?;
    events_resume(backend).await
}

/// Files of the repository as the suite built it, and the objects that
/// make the current snapshot.
#[derive(Default)]
struct Fixture {
    files: BTreeMap<String, ObjectId>,
    snapshot: Option<ObjectId>,
    head: Option<ObjectId>,
    snapshots: BTreeMap<ObjectId, ObjectId>,
}

/// A change built from objects: its id, record, and the objects to put.
struct Built {
    id: ObjectId,
    record: ChangeRecord,
    objects: Vec<proto::Object>,
}

fn encoded<T: serde::Serialize>(value: &T) -> Outcome<(ObjectId, proto::Object)> {
    let bytes = hord_encoding::encode(value).check("encode a fixture object")?;
    let object = wire::object(bytes);
    let id = wire::object_id("id", &object.id).check("a fixture object's id")?;
    Ok((id, object))
}

fn actor() -> Actor {
    Actor::Agent {
        id: ACTOR.into(),
        model: "none".into(),
        model_hash: hord_core::Bytes::default(),
        harness: "hord-api conformance".into(),
    }
}

impl Fixture {
    fn base(&self) -> Outcome<ObjectId> {
        match self.snapshot {
            Some(snapshot) => Ok(snapshot),
            None => ObjectId::of(&Snapshot::empty()).check("the empty snapshot's id"),
        }
    }

    /// A change adding `path` with `content` on the current snapshot. With
    /// `lie`, its `Blob` op names other content than its result holds.
    fn add_file(&self, path: &str, content: &str, lie: bool) -> Outcome<Built> {
        let mut objects = Vec::new();
        let (blob, o) = encoded(&Blob::new(content.as_bytes().to_vec()))?;
        objects.push(o);
        let mut files = self.files.clone();
        files.insert(path.to_owned(), blob);
        let paths: Vec<(Vec<&str>, ObjectId)> = files
            .iter()
            .map(|(path, id)| (path.split('/').collect(), *id))
            .collect();
        let tree = build_tree(&paths, &mut objects)?;
        // Blob-tier files take the fresh assignment: an empty identity tree.
        let (identity, o) = encoded(&IdentityTree::default())?;
        objects.push(o);
        let (result, o) = encoded(&Snapshot::new(tree, identity))?;
        objects.push(o);
        let to = if lie {
            let (other, o) = encoded(&Blob::new(b"not what the tree holds".to_vec()))?;
            objects.push(o);
            other
        } else {
            blob
        };
        let repo_path: RepoPath = path.parse().check("a fixture path")?;
        let record = ChangeRecord {
            base: self.base()?,
            result,
            parents: self.head.into_iter().collect(),
            ops: vec![
                Op::Tree {
                    path: repo_path.clone(),
                    kind: TreeOpKind::CreateFile,
                },
                Op::Blob {
                    path: repo_path.clone(),
                    from: None,
                    to: Some(to),
                },
            ],
            intent: Intent::from_summary(format!("add {path}")),
            provenance: Provenance {
                actor: actor(),
                toolchain: ObjectId::from_canonical(b"hord-api conformance"),
                created_at: Timestamp::from_millis(1),
                session: None,
                parent_intent: None,
                voucher: None,
            },
            read_set: Default::default(),
            write_set: [NodeId::file_root(&repo_path)].into_iter().collect(),
            identity_deltas: Vec::new(),
            evidence: Vec::new(),
            signature: None,
            rebased_from: None,
        };
        let (id, o) = encoded(&record)?;
        objects.push(o);
        Ok(Built {
            id,
            record,
            objects,
        })
    }

    fn landed(&mut self, path: &str, built: &Built) -> Outcome {
        let blob = built.record.ops.iter().find_map(|op| match op {
            Op::Blob { to, .. } => *to,
            _ => None,
        });
        self.files
            .insert(path.to_owned(), blob.check("the landed change's blob op")?);
        self.snapshot = Some(built.record.result);
        self.head = Some(built.id);
        self.snapshots.insert(built.id, built.record.result);
        Ok(())
    }
}

/// Store the [`Tree`] of `files` (path components → blob) and its
/// subtrees; returns the root's id.
fn build_tree(
    files: &[(Vec<&str>, ObjectId)],
    objects: &mut Vec<proto::Object>,
) -> Outcome<ObjectId> {
    let mut entries = BTreeMap::new();
    let mut dirs: BTreeMap<&str, Vec<(Vec<&str>, ObjectId)>> = BTreeMap::new();
    for (components, blob) in files {
        match components.as_slice() {
            [name] => {
                entries.insert((*name).to_owned(), TreeEntry::Blob(*blob));
            }
            [dir, rest @ ..] => dirs.entry(dir).or_default().push((rest.to_vec(), *blob)),
            [] => {}
        }
    }
    for (dir, files) in dirs {
        let id = build_tree(&files, objects)?;
        entries.insert(dir.to_owned(), TreeEntry::Tree(id));
    }
    let (id, o) = encoded(&Tree { entries })?;
    objects.push(o);
    Ok(id)
}

/// Wait until `change` settles; fail unless it landed. Returns the events
/// read, through the `HeadMoved` to it.
async fn landing(stream: &mut EventStream, change: &str) -> Outcome<Vec<proto::EventEnvelope>> {
    let mut seen = until(stream, "the change to settle", |k| match k {
        Kind::Landed(l) => l.change == change || l.submitted.as_deref() == Some(change),
        Kind::Rejected(r) => r.change == change,
        Kind::Parked(p) => p.change == change,
        _ => false,
    })
    .await?;
    let last = kinds(&seen).last().copied();
    ensure!(
        matches!(last, Some(Kind::Landed(_))),
        "{change} did not land: {last:?}"
    );
    seen.extend(until(stream, "HeadMoved", |k| matches!(k, Kind::HeadMoved(_))).await?);
    Ok(seen)
}

async fn put(backend: &dyn RepoBackend, built: &Built) -> Outcome {
    backend
        .put_objects(proto::PutObjectsRequest {
            objects: built.objects.clone(),
        })
        .await
        .check("put_objects: a change's objects")?;
    Ok(())
}

/// Read events from `stream` until one matches, or fail after
/// [`LANDING_TIMEOUT`]. Returns every event read, the match last.
async fn until(
    stream: &mut EventStream,
    what: &str,
    mut done: impl FnMut(&Kind) -> bool,
) -> Outcome<Vec<proto::EventEnvelope>> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + LANDING_TIMEOUT;
    loop {
        let Ok(next) = tokio::time::timeout_at(deadline, stream.next()).await else {
            return Err(Failure(format!(
                "events: timed out waiting for {what}; saw {seen:?}"
            )));
        };
        let envelope = next
            .check(&format!("events: the stream ended waiting for {what}"))?
            .check("events: stream error")?;
        let hit = envelope
            .event
            .as_ref()
            .and_then(|e| e.kind.as_ref())
            .is_some_and(&mut done);
        seen.push(envelope);
        if hit {
            return Ok(seen);
        }
    }
}

fn kinds(events: &[proto::EventEnvelope]) -> Vec<&Kind> {
    events.iter().filter_map(|e| e.kind()).collect()
}

fn expect_err<T: std::fmt::Debug>(
    what: &str,
    result: crate::ApiResult<T>,
    ok: impl Fn(&ApiError) -> bool,
) -> Outcome {
    match result {
        Err(err) if ok(&err) => Ok(()),
        other => Err(Failure(format!("{what}: unexpected {other:?}"))),
    }
}

/// A fresh repository has no head, no log, and an empty queue.
pub async fn empty_repository(backend: &dyn RepoBackend) -> Outcome {
    let head = backend.head(proto::HeadRequest {}).await.check("head")?;
    ensure_eq!(head.change, None, "head of an empty repository");
    let log = backend.log(proto::LogQuery::default()).await.check("log")?;
    ensure!(log.items.is_empty() && log.next.is_none(), "log: {log:?}");
    let queue = backend
        .queue(proto::QueueQuery::default())
        .await
        .check("queue")?;
    ensure!(queue.entries.is_empty(), "queue: {queue:?}");
    Ok(())
}

/// Object put, get, and has, and their failures: a wrong hash, a malformed
/// id, a missing object, an oversized batch.
pub async fn objects(backend: &dyn RepoBackend) -> Outcome {
    let (id, object) = encoded(&Blob::new(b"conformance object\n".to_vec()))?;
    let missing = wire::id(ObjectId::from_canonical(b"never stored"));
    backend
        .put_objects(proto::PutObjectsRequest {
            objects: vec![object.clone(), object.clone()],
        })
        .await
        .check("put_objects is idempotent")?;
    let has = backend
        .has(proto::HasRequest {
            ids: vec![wire::id(id), missing.clone()],
        })
        .await
        .check("has")?;
    ensure_eq!(has.present, vec![true, false], "has");
    let got = backend
        .get_objects(proto::GetObjectsRequest {
            ids: vec![wire::id(id)],
        })
        .await
        .check("get_objects")?;
    ensure_eq!(got.objects, vec![object.clone()], "get_objects round trip");
    expect_err(
        "get_objects of a missing id",
        backend
            .get_objects(proto::GetObjectsRequest { ids: vec![missing] })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    )?;
    expect_err(
        "put_objects with a wrong id",
        backend
            .put_objects(proto::PutObjectsRequest {
                objects: vec![proto::Object {
                    id: object.id.clone(),
                    cbor: b"\x01".to_vec(),
                }],
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    expect_err(
        "has with a malformed id",
        backend
            .has(proto::HasRequest {
                ids: vec!["xyz".into()],
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    expect_err(
        "has over the batch limit",
        backend
            .has(proto::HasRequest {
                ids: vec![wire::id(id); MAX_BATCH_IDS + 1],
            })
            .await,
        |e| matches!(e, ApiError::ResourceExhausted(_)),
    )?;
    Ok(())
}

/// Submit a change built from objects; it lands, with the events in order,
/// and head, queue, and log agree.
async fn submit_and_land(backend: &dyn RepoBackend, fx: &mut Fixture) -> Outcome<ObjectId> {
    let built = fx.add_file("a.txt", "alpha\n", false)?;
    put(backend, &built).await?;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .check("events")?;
    let change = wire::id(built.id);
    let submitted = backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .check("submit")?;
    let entry = submitted.entry.check("submit returns the entry")?;
    ensure_eq!(entry.change, change, "submitted entry names the change");
    ensure_eq!(entry.seq, submitted.submission, "submission id is the seq");
    ensure_eq!(entry.summary, "add a.txt");
    let seen = landing(&mut events, &change).await?;
    let order: Vec<&str> = kinds(&seen)
        .into_iter()
        .filter_map(|k| match k {
            Kind::Submitted(s) if s.change == change => Some("submitted"),
            Kind::ConflictCheck(c) if c.change == change => Some("conflict_check"),
            Kind::Landed(l) if l.change == change => Some("landed"),
            Kind::HeadMoved(_) => Some("head_moved"),
            _ => None,
        })
        .collect();
    ensure_eq!(
        order,
        ["submitted", "conflict_check", "landed", "head_moved"],
        "event order"
    );
    for pair in seen.windows(2) {
        ensure!(
            pair[0].cursor < pair[1].cursor,
            "cursors increase: {seen:?}"
        );
    }
    if let Some(Kind::Submitted(s)) = kinds(&seen).first() {
        ensure_eq!(s.actor.as_ref().map(wire::actor_id), Some(ACTOR));
    }
    let head = backend.head(proto::HeadRequest {}).await.check("head")?;
    ensure_eq!(head.change.as_deref(), Some(change.as_str()), "head");
    let queue = backend
        .queue(proto::QueueQuery {
            change: Some(change.clone()),
            ..Default::default()
        })
        .await
        .check("queue")?;
    let [entry] = queue.entries.as_slice() else {
        return Err(Failure(format!("queue by change: {queue:?}")));
    };
    ensure_eq!(entry.status(), proto::QueueStatus::Landed);
    ensure_eq!(entry.landed.as_deref(), Some(change.as_str()));
    ensure!(entry.report.as_ref().is_some_and(|r| r.clean), "{entry:?}");
    fx.landed("a.txt", &built)?;
    Ok(built.id)
}

/// Submitting a landed change again returns its entry; an unknown change
/// is not found.
async fn resubmission(backend: &dyn RepoBackend, a: ObjectId) -> Outcome {
    let first = backend
        .queue(proto::QueueQuery::default())
        .await
        .check("queue")?
        .entries;
    let again = backend
        .submit(proto::SubmitRequest {
            change: wire::id(a),
        })
        .await
        .check("resubmit")?;
    let first_seq = first.first().check("the queue's first entry")?.seq;
    ensure_eq!(again.submission, first_seq, "resubmission is a no-op");
    let after = backend
        .queue(proto::QueueQuery::default())
        .await
        .check("queue")?
        .entries;
    ensure_eq!(after.len(), first.len(), "no new entry");
    expect_err(
        "submit of an unknown change",
        backend
            .submit(proto::SubmitRequest {
                change: wire::id(ObjectId::from_canonical(b"no such change")),
            })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    )?;
    Ok(())
}

/// A second change lands after the first; the log pages newest first and
/// filters by actor, node, and path.
async fn second_change_and_log(
    backend: &dyn RepoBackend,
    fx: &mut Fixture,
    a: ObjectId,
) -> Outcome<ObjectId> {
    let built = fx.add_file("docs/b.txt", "beta\n", false)?;
    put(backend, &built).await?;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .check("events")?;
    let change = wire::id(built.id);
    backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .check("submit")?;
    let seen = landing(&mut events, &change).await?;
    let landed = kinds(&seen).into_iter().find_map(|k| match k {
        Kind::Landed(l) => Some(l.position),
        _ => None,
    });
    ensure_eq!(landed, Some(1), "second landing is at position 1");
    fx.landed("docs/b.txt", &built)?;

    let all = backend.log(proto::LogQuery::default()).await.check("log")?;
    let ids: Vec<&str> = all.items.iter().map(|s| s.change.as_str()).collect();
    ensure_eq!(ids, [change.as_str(), &wire::id(a)], "log is newest first");
    ensure_eq!(all.next, None);
    ensure_eq!(all.items[0].summary, "add docs/b.txt");
    ensure_eq!(all.items[1].position, 0);

    let first = backend
        .log(proto::LogQuery {
            limit: 1,
            ..Default::default()
        })
        .await
        .check("log page 1")?;
    ensure_eq!(first.items.len(), 1);
    ensure_eq!(first.next.as_deref(), Some(change.as_str()), "next page");
    let second = backend
        .log(proto::LogQuery {
            limit: 1,
            after: first.next.clone(),
            ..Default::default()
        })
        .await
        .check("log page 2")?;
    ensure_eq!(
        second.items.first().check("log page 2's item")?.change,
        wire::id(a)
    );

    let by_node = backend
        .log(proto::LogQuery {
            node: Some(
                NodeId::file_root(
                    &"docs/b.txt"
                        .parse()
                        .check("docs/b.txt is a valid repo path")?,
                )
                .to_string(),
            ),
            ..Default::default()
        })
        .await
        .check("log by node")?;
    ensure_eq!(by_node.items.len(), 1, "log by node: {by_node:?}");
    let by_path = backend
        .log(proto::LogQuery {
            path: Some("docs".into()),
            ..Default::default()
        })
        .await
        .check("log by path")?;
    ensure_eq!(by_path.items.len(), 1, "log by path: {by_path:?}");
    let by_actor = backend
        .log(proto::LogQuery {
            actor: Some("somebody else".into()),
            ..Default::default()
        })
        .await
        .check("log by actor")?;
    ensure!(by_actor.items.is_empty(), "log by actor: {by_actor:?}");
    expect_err(
        "log after an unlanded change",
        backend
            .log(proto::LogQuery {
                after: Some(wire::id(ObjectId::from_canonical(b"unlanded"))),
                ..Default::default()
            })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    )?;
    Ok(built.id)
}

/// Node history, edges, and name resolution over the landed snapshots.
async fn queries(backend: &dyn RepoBackend, fx: &Fixture, a: ObjectId, b: ObjectId) -> Outcome {
    let root = NodeId::file_root(&"a.txt".parse().check("a.txt is a valid repo path")?);
    let history = backend
        .node_history(proto::NodeHistoryRequest {
            node: root.to_string(),
        })
        .await
        .check("node_history")?;
    let changes: Vec<&str> = history.changes.iter().map(|c| c.change.as_str()).collect();
    ensure_eq!(changes, [wire::id(a)], "node_history of a.txt");
    let snapshot = wire::id(*fx.snapshots.get(&b).check("b's snapshot")?);
    let edges = backend
        .edges(proto::EdgesRequest {
            snapshot: snapshot.clone(),
            node: root.to_string(),
            kind: proto::EdgeKind::References.into(),
        })
        .await
        .check("edges")?;
    ensure!(
        edges.nodes.is_empty(),
        "a blob-tier file references nothing"
    );
    expect_err(
        "edges of an unspecified kind",
        backend
            .edges(proto::EdgesRequest {
                snapshot: snapshot.clone(),
                node: root.to_string(),
                kind: proto::EdgeKind::Unspecified.into(),
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    let names = backend
        .resolve_name(proto::ResolveNameRequest {
            snapshot,
            name: "no_such_definition".into(),
        })
        .await
        .check("resolve_name")?;
    ensure!(names.nodes.is_empty(), "resolve_name: {names:?}");
    expect_err(
        "node_history of a malformed node id",
        backend
            .node_history(proto::NodeHistoryRequest {
                node: "not a node".into(),
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    Ok(())
}

/// Evidence for a change's result snapshot is stored and announced;
/// evidence for another snapshot is refused (ADR 0025).
async fn evidence(backend: &dyn RepoBackend, fx: &Fixture, b: ObjectId) -> Outcome {
    let make = |snapshot| Evidence {
        kind: EvidenceKind::Review,
        qualifier: Some("conformance".into()),
        snapshot,
        toolchain: ObjectId::from_canonical(b"hord-api conformance"),
        command: "conformance review".into(),
        scope: None,
        result: EvidenceResult::Pass,
        log: None,
        cost_ms: 0,
        produced_by: actor(),
        produced_at: Timestamp::from_millis(2),
        signature: None,
    };
    let good = hord_encoding::encode(&make(*fx.snapshots.get(&b).check("b's snapshot")?))
        .check("encode")?;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .check("events")?;
    let attached = backend
        .attach_evidence(proto::AttachEvidenceRequest {
            change: wire::id(b),
            evidence: good.clone(),
        })
        .await
        .check("attach_evidence")?;
    ensure_eq!(
        attached.evidence,
        wire::id(ObjectId::from_canonical(&good)),
        "evidence id is its hash"
    );
    let has = backend
        .has(proto::HasRequest {
            ids: vec![attached.evidence.clone()],
        })
        .await
        .check("has")?;
    ensure_eq!(has.present, [true], "attached evidence is stored");
    let seen = until(
        &mut events,
        "EvidenceAttached",
        |k| matches!(k, Kind::EvidenceAttached(e) if e.evidence == attached.evidence),
    )
    .await?;
    if let Some(Kind::EvidenceAttached(e)) = kinds(&seen).last() {
        ensure_eq!(e.change, wire::id(b));
        ensure!(matches!(
            e.kind.as_ref().and_then(|k| k.kind.as_ref()),
            Some(proto::evidence_kind::Kind::Review(true))
        ));
        ensure!(matches!(
            e.result.as_ref().and_then(|r| r.result.as_ref()),
            Some(proto::evidence_result::Result::Pass(true))
        ));
    }
    let empty = ObjectId::of(&Snapshot::empty()).check("empty snapshot")?;
    let stale = hord_encoding::encode(&make(empty)).check("encode")?;
    expect_err(
        "evidence for another snapshot",
        backend
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(b),
                evidence: stale,
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    expect_err(
        "evidence that is not an Evidence object",
        backend
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(b),
                evidence: b"\x01".to_vec(),
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    )?;
    Ok(())
}

/// A change whose ops do not reproduce its result is rejected, not landed
/// (spec §3.5), and head does not move.
async fn rejection(backend: &dyn RepoBackend, fx: &Fixture) -> Outcome {
    let built = fx.add_file("c.txt", "gamma\n", true)?;
    put(backend, &built).await?;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .check("events")?;
    let change = wire::id(built.id);
    backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .check("submit")?;
    until(
        &mut events,
        "Rejected",
        |k| matches!(k, Kind::Rejected(r) if r.change == change),
    )
    .await?;
    let entry = backend
        .queue(proto::QueueQuery {
            change: Some(change.clone()),
            ..Default::default()
        })
        .await
        .check("queue")?
        .entries;
    let entry = entry.first().check("queue by change")?;
    ensure_eq!(entry.status(), proto::QueueStatus::Rejected);
    ensure!(entry.reason.is_some(), "a rejection has a reason");
    let head = backend.head(proto::HeadRequest {}).await.check("head")?;
    ensure_eq!(head.change, fx.head.map(wire::id), "head did not move");
    let pending = backend
        .queue(proto::QueueQuery {
            pending_only: true,
            ..Default::default()
        })
        .await
        .check("queue")?;
    ensure!(pending.entries.is_empty(), "nothing is left queued");
    Ok(())
}

/// Arbitrating a change that is not in the queue is NOT_FOUND; refs list.
async fn arbitrate_unknown(backend: &dyn RepoBackend) -> Outcome {
    expect_err(
        "arbitrate an unknown change",
        backend
            .arbitrate(proto::ArbitrateRequest {
                change: wire::id(ObjectId::from_canonical(b"x")),
                action: Some(proto::Arbitration {
                    action: Some(proto::arbitration::Action::PickOurs(true)),
                }),
                ..Default::default()
            })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    )?;
    let refs = backend
        .refs(proto::RefsRequest::default())
        .await
        .check("refs")?;
    ensure!(
        refs.refs
            .values()
            .all(|v| wire::object_id("refs", v).is_ok()),
        "refs name object ids: {refs:?}"
    );
    Ok(())
}

/// The whole log replays from cursor 0 in order, and resuming from a
/// cursor yields exactly the events after it.
async fn events_resume(backend: &dyn RepoBackend) -> Outcome {
    let mut from_start = backend
        .events(proto::EventsRequest { from: Some(0) })
        .await
        .check("events from 0")?;
    // Everything so far: through the Rejected event of the last check.
    let all = until(&mut from_start, "the Rejected event", |k| {
        matches!(k, Kind::Rejected(_))
    })
    .await?;
    for (i, e) in all.iter().enumerate() {
        ensure_eq!(e.cursor, i as u64 + 1, "cursors are 1, 2, 3, …");
    }
    let middle = all[all.len() / 2].cursor;
    let mut resumed = backend
        .events(proto::EventsRequest { from: Some(middle) })
        .await
        .check("events from a cursor")?;
    let last = all.last().check("events")?.cursor;
    let rest = until(&mut resumed, "the last event", |_| true).await?;
    ensure_eq!(rest[0].cursor, middle + 1, "resume starts after the cursor");
    let mut cursors = vec![rest[0].cursor];
    while cursors.last() != Some(&last) {
        let more = until(&mut resumed, "the last event", |_| true).await?;
        cursors.push(more[0].cursor);
    }
    let expected: Vec<u64> = (middle + 1..=last).collect();
    ensure_eq!(cursors, expected, "resume yields exactly the later events");
    Ok(())
}
