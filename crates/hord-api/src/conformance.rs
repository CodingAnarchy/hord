//! The `RepoBackend` conformance suite (spec §12 M4, ADR 0024), written
//! once against `&dyn RepoBackend` and run against every implementation:
//! `hord_txn::LocalRepo` now, `RemoteRepo` against `hord serve` later.
//!
//! [`run`] takes a backend over a fresh, empty repository whose lander runs
//! on its own (submitted changes land without further calls) and whose
//! verifier lands clean, disjoint changes. It builds every change it needs
//! from raw objects through [`RepoBackend::put_objects`]: blob-tier files
//! (`.txt`, which no adapter parses) with Tier 0 ops, so the suite does not
//! depend on a workspace or a language adapter. It panics with a message
//! naming the failed check, like an assertion in a test.

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

/// Run every check against `backend`, in order. See the module docs for
/// what the backend must be.
pub async fn run(backend: &dyn RepoBackend) {
    let mut fx = Fixture::default();
    empty_repository(backend).await;
    objects(backend).await;
    let a = submit_and_land(backend, &mut fx).await;
    resubmission(backend, a).await;
    let b = second_change_and_log(backend, &mut fx, a).await;
    queries(backend, &fx, a, b).await;
    evidence(backend, &fx, b).await;
    rejection(backend, &fx).await;
    unimplemented_until_m5(backend).await;
    events_resume(backend).await;
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

fn encoded<T: serde::Serialize>(value: &T) -> (ObjectId, proto::Object) {
    let bytes = hord_encoding::encode(value).expect("encode");
    let object = wire::object(bytes);
    (wire::object_id("id", &object.id).expect("hex"), object)
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
    fn base(&self) -> ObjectId {
        self.snapshot
            .unwrap_or_else(|| ObjectId::of(&Snapshot::empty()).expect("empty snapshot"))
    }

    /// A change adding `path` with `content` on the current snapshot. With
    /// `lie`, its `Blob` op names other content than its result holds.
    fn add_file(&self, path: &str, content: &str, lie: bool) -> Built {
        let mut objects = Vec::new();
        let (blob, o) = encoded(&Blob::new(content.as_bytes().to_vec()));
        objects.push(o);
        let mut files = self.files.clone();
        files.insert(path.to_owned(), blob);
        let paths: Vec<(Vec<&str>, ObjectId)> = files
            .iter()
            .map(|(path, id)| (path.split('/').collect(), *id))
            .collect();
        let tree = build_tree(&paths, &mut objects);
        // Blob-tier files take the fresh assignment: an empty identity tree.
        let (identity, o) = encoded(&IdentityTree::default());
        objects.push(o);
        let (result, o) = encoded(&Snapshot::new(tree, identity));
        objects.push(o);
        let to = if lie {
            let (other, o) = encoded(&Blob::new(b"not what the tree holds".to_vec()));
            objects.push(o);
            other
        } else {
            blob
        };
        let repo_path: RepoPath = path.parse().expect("path");
        let record = ChangeRecord {
            base: self.base(),
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
            },
            read_set: Default::default(),
            write_set: [NodeId::file_root(&repo_path)].into_iter().collect(),
            identity_deltas: Vec::new(),
            evidence: Vec::new(),
            signature: None,
            rebased_from: None,
        };
        let (id, o) = encoded(&record);
        objects.push(o);
        Built {
            id,
            record,
            objects,
        }
    }

    fn landed(&mut self, path: &str, built: &Built) {
        let blob = built.record.ops.iter().find_map(|op| match op {
            Op::Blob { to, .. } => *to,
            _ => None,
        });
        self.files.insert(path.to_owned(), blob.expect("blob op"));
        self.snapshot = Some(built.record.result);
        self.head = Some(built.id);
        self.snapshots.insert(built.id, built.record.result);
    }
}

/// Store the [`Tree`] of `files` (path components → blob) and its
/// subtrees; returns the root's id.
fn build_tree(files: &[(Vec<&str>, ObjectId)], objects: &mut Vec<proto::Object>) -> ObjectId {
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
        let id = build_tree(&files, objects);
        entries.insert(dir.to_owned(), TreeEntry::Tree(id));
    }
    let (id, o) = encoded(&Tree { entries });
    objects.push(o);
    id
}

/// Wait until `change` settles; panic unless it landed. Returns the events
/// read, through the `HeadMoved` to it.
async fn landing(stream: &mut EventStream, change: &str) -> Vec<proto::EventEnvelope> {
    let mut seen = until(stream, "the change to settle", |k| match k {
        Kind::Landed(l) => l.change == change || l.submitted.as_deref() == Some(change),
        Kind::Rejected(r) => r.change == change,
        Kind::Parked(p) => p.change == change,
        _ => false,
    })
    .await;
    match kinds(&seen).last() {
        Some(Kind::Landed(_)) => {}
        other => panic!("{change} did not land: {other:?}"),
    }
    seen.extend(until(stream, "HeadMoved", |k| matches!(k, Kind::HeadMoved(_))).await);
    seen
}

async fn put(backend: &dyn RepoBackend, built: &Built) {
    backend
        .put_objects(proto::PutObjectsRequest {
            objects: built.objects.clone(),
        })
        .await
        .expect("put_objects: a change's objects");
}

/// Read events from `stream` until one matches, or panic after
/// [`LANDING_TIMEOUT`]. Returns every event read, the match last.
async fn until(
    stream: &mut EventStream,
    what: &str,
    mut done: impl FnMut(&Kind) -> bool,
) -> Vec<proto::EventEnvelope> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + LANDING_TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("events: timed out waiting for {what}; saw {seen:?}"));
        let envelope = next
            .unwrap_or_else(|| panic!("events: stream ended waiting for {what}"))
            .expect("events: stream error");
        let hit = envelope
            .event
            .as_ref()
            .and_then(|e| e.kind.as_ref())
            .is_some_and(&mut done);
        seen.push(envelope);
        if hit {
            return seen;
        }
    }
}

fn kinds(events: &[proto::EventEnvelope]) -> Vec<&Kind> {
    events
        .iter()
        .filter_map(|e| e.event.as_ref().and_then(|e| e.kind.as_ref()))
        .collect()
}

fn expect_err<T: std::fmt::Debug>(
    what: &str,
    result: crate::ApiResult<T>,
    ok: impl Fn(&ApiError) -> bool,
) {
    match result {
        Err(err) if ok(&err) => {}
        other => panic!("{what}: unexpected {other:?}"),
    }
}

/// A fresh repository has no head, no log, and an empty queue.
pub async fn empty_repository(backend: &dyn RepoBackend) {
    let head = backend.head(proto::HeadRequest {}).await.expect("head");
    assert_eq!(head.change, None, "head of an empty repository");
    let log = backend.log(proto::LogQuery::default()).await.expect("log");
    assert!(log.items.is_empty() && log.next.is_none(), "log: {log:?}");
    let queue = backend
        .queue(proto::QueueQuery::default())
        .await
        .expect("queue");
    assert!(queue.entries.is_empty(), "queue: {queue:?}");
}

/// Object put, get, and has, and their failures: a wrong hash, a malformed
/// id, a missing object, an oversized batch.
pub async fn objects(backend: &dyn RepoBackend) {
    let (id, object) = encoded(&Blob::new(b"conformance object\n".to_vec()));
    let missing = wire::id(ObjectId::from_canonical(b"never stored"));
    backend
        .put_objects(proto::PutObjectsRequest {
            objects: vec![object.clone(), object.clone()],
        })
        .await
        .expect("put_objects is idempotent");
    let has = backend
        .has(proto::HasRequest {
            ids: vec![wire::id(id), missing.clone()],
        })
        .await
        .expect("has");
    assert_eq!(has.present, vec![true, false], "has");
    let got = backend
        .get_objects(proto::GetObjectsRequest {
            ids: vec![wire::id(id)],
        })
        .await
        .expect("get_objects");
    assert_eq!(got.objects, vec![object.clone()], "get_objects round trip");
    expect_err(
        "get_objects of a missing id",
        backend
            .get_objects(proto::GetObjectsRequest { ids: vec![missing] })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    );
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
    );
    expect_err(
        "has with a malformed id",
        backend
            .has(proto::HasRequest {
                ids: vec!["xyz".into()],
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    );
    expect_err(
        "has over the batch limit",
        backend
            .has(proto::HasRequest {
                ids: vec![wire::id(id); MAX_BATCH_IDS + 1],
            })
            .await,
        |e| matches!(e, ApiError::ResourceExhausted(_)),
    );
}

/// Submit a change built from objects; it lands, with the events in order,
/// and head, queue, and log agree.
async fn submit_and_land(backend: &dyn RepoBackend, fx: &mut Fixture) -> ObjectId {
    let built = fx.add_file("a.txt", "alpha\n", false);
    put(backend, &built).await;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .expect("events");
    let change = wire::id(built.id);
    let submitted = backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .expect("submit");
    let entry = submitted.entry.expect("submit returns the entry");
    assert_eq!(entry.change, change, "submitted entry names the change");
    assert_eq!(entry.seq, submitted.submission, "submission id is the seq");
    assert_eq!(entry.summary, "add a.txt");
    let seen = landing(&mut events, &change).await;
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
    assert_eq!(
        order,
        ["submitted", "conflict_check", "landed", "head_moved"],
        "event order"
    );
    for pair in seen.windows(2) {
        assert!(
            pair[0].cursor < pair[1].cursor,
            "cursors increase: {seen:?}"
        );
    }
    if let Some(Kind::Submitted(s)) = kinds(&seen).first() {
        assert_eq!(s.actor.as_ref().map(wire::actor_id), Some(ACTOR));
    }
    let head = backend.head(proto::HeadRequest {}).await.expect("head");
    assert_eq!(head.change.as_deref(), Some(change.as_str()), "head");
    let queue = backend
        .queue(proto::QueueQuery {
            change: Some(change.clone()),
            ..Default::default()
        })
        .await
        .expect("queue");
    let [entry] = queue.entries.as_slice() else {
        panic!("queue by change: {queue:?}");
    };
    assert_eq!(entry.status(), proto::QueueStatus::Landed);
    assert_eq!(entry.landed.as_deref(), Some(change.as_str()));
    assert!(entry.report.as_ref().is_some_and(|r| r.clean), "{entry:?}");
    fx.landed("a.txt", &built);
    built.id
}

/// Submitting a landed change again returns its entry; an unknown change
/// is not found.
async fn resubmission(backend: &dyn RepoBackend, a: ObjectId) {
    let first = backend
        .queue(proto::QueueQuery::default())
        .await
        .expect("queue")
        .entries;
    let again = backend
        .submit(proto::SubmitRequest {
            change: wire::id(a),
        })
        .await
        .expect("resubmit");
    assert_eq!(again.submission, first[0].seq, "resubmission is a no-op");
    let after = backend
        .queue(proto::QueueQuery::default())
        .await
        .expect("queue")
        .entries;
    assert_eq!(after.len(), first.len(), "no new entry");
    expect_err(
        "submit of an unknown change",
        backend
            .submit(proto::SubmitRequest {
                change: wire::id(ObjectId::from_canonical(b"no such change")),
            })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    );
}

/// A second change lands after the first; the log pages newest first and
/// filters by actor, node, and path.
async fn second_change_and_log(
    backend: &dyn RepoBackend,
    fx: &mut Fixture,
    a: ObjectId,
) -> ObjectId {
    let built = fx.add_file("docs/b.txt", "beta\n", false);
    put(backend, &built).await;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .expect("events");
    let change = wire::id(built.id);
    backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .expect("submit");
    let seen = landing(&mut events, &change).await;
    let landed = kinds(&seen).into_iter().find_map(|k| match k {
        Kind::Landed(l) => Some(l.position),
        _ => None,
    });
    assert_eq!(landed, Some(1), "second landing is at position 1");
    fx.landed("docs/b.txt", &built);

    let all = backend.log(proto::LogQuery::default()).await.expect("log");
    let ids: Vec<&str> = all.items.iter().map(|s| s.change.as_str()).collect();
    assert_eq!(ids, [change.as_str(), &wire::id(a)], "log is newest first");
    assert_eq!(all.next, None);
    assert_eq!(all.items[0].summary, "add docs/b.txt");
    assert_eq!(all.items[1].position, 0);

    let first = backend
        .log(proto::LogQuery {
            limit: 1,
            ..Default::default()
        })
        .await
        .expect("log page 1");
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.next.as_deref(), Some(change.as_str()), "next page");
    let second = backend
        .log(proto::LogQuery {
            limit: 1,
            after: first.next.clone(),
            ..Default::default()
        })
        .await
        .expect("log page 2");
    assert_eq!(second.items[0].change, wire::id(a));

    let by_node = backend
        .log(proto::LogQuery {
            node: Some(
                NodeId::file_root(
                    &"docs/b.txt"
                        .parse()
                        .expect("docs/b.txt is a valid repo path"),
                )
                .to_string(),
            ),
            ..Default::default()
        })
        .await
        .expect("log by node");
    assert_eq!(by_node.items.len(), 1, "log by node: {by_node:?}");
    let by_path = backend
        .log(proto::LogQuery {
            path: Some("docs".into()),
            ..Default::default()
        })
        .await
        .expect("log by path");
    assert_eq!(by_path.items.len(), 1, "log by path: {by_path:?}");
    let by_actor = backend
        .log(proto::LogQuery {
            actor: Some("somebody else".into()),
            ..Default::default()
        })
        .await
        .expect("log by actor");
    assert!(by_actor.items.is_empty(), "log by actor: {by_actor:?}");
    expect_err(
        "log after an unlanded change",
        backend
            .log(proto::LogQuery {
                after: Some(wire::id(ObjectId::from_canonical(b"unlanded"))),
                ..Default::default()
            })
            .await,
        |e| matches!(e, ApiError::NotFound(_)),
    );
    built.id
}

/// Node history, edges, and name resolution over the landed snapshots.
async fn queries(backend: &dyn RepoBackend, fx: &Fixture, a: ObjectId, b: ObjectId) {
    let root = NodeId::file_root(&"a.txt".parse().expect("a.txt is a valid repo path"));
    let history = backend
        .node_history(proto::NodeHistoryRequest {
            node: root.to_string(),
        })
        .await
        .expect("node_history");
    let changes: Vec<&str> = history.changes.iter().map(|c| c.change.as_str()).collect();
    assert_eq!(changes, [wire::id(a)], "node_history of a.txt");
    let snapshot = wire::id(fx.snapshots[&b]);
    let edges = backend
        .edges(proto::EdgesRequest {
            snapshot: snapshot.clone(),
            node: root.to_string(),
            kind: proto::EdgeKind::References.into(),
        })
        .await
        .expect("edges");
    assert!(
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
    );
    let names = backend
        .resolve_name(proto::ResolveNameRequest {
            snapshot,
            name: "no_such_definition".into(),
        })
        .await
        .expect("resolve_name");
    assert!(names.nodes.is_empty(), "resolve_name: {names:?}");
    expect_err(
        "node_history of a malformed node id",
        backend
            .node_history(proto::NodeHistoryRequest {
                node: "not a node".into(),
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    );
}

/// Evidence for a change's result snapshot is stored and announced;
/// evidence for another snapshot is refused (ADR 0025).
async fn evidence(backend: &dyn RepoBackend, fx: &Fixture, b: ObjectId) {
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
    };
    let good = hord_encoding::encode(&make(fx.snapshots[&b])).expect("encode");
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .expect("events");
    let attached = backend
        .attach_evidence(proto::AttachEvidenceRequest {
            change: wire::id(b),
            evidence: good.clone(),
        })
        .await
        .expect("attach_evidence");
    assert_eq!(
        attached.evidence,
        wire::id(ObjectId::from_canonical(&good)),
        "evidence id is its hash"
    );
    let has = backend
        .has(proto::HasRequest {
            ids: vec![attached.evidence.clone()],
        })
        .await
        .expect("has");
    assert_eq!(has.present, [true], "attached evidence is stored");
    let seen = until(
        &mut events,
        "EvidenceAttached",
        |k| matches!(k, Kind::EvidenceAttached(e) if e.evidence == attached.evidence),
    )
    .await;
    if let Some(Kind::EvidenceAttached(e)) = kinds(&seen).last() {
        assert_eq!(e.change, wire::id(b));
        assert!(matches!(
            e.kind.as_ref().and_then(|k| k.kind.as_ref()),
            Some(proto::evidence_kind::Kind::Review(true))
        ));
        assert!(matches!(
            e.result.as_ref().and_then(|r| r.result.as_ref()),
            Some(proto::evidence_result::Result::Pass(true))
        ));
    }
    let empty = ObjectId::of(&Snapshot::empty()).expect("empty snapshot");
    let stale = hord_encoding::encode(&make(empty)).expect("encode");
    expect_err(
        "evidence for another snapshot",
        backend
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(b),
                evidence: stale,
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    );
    expect_err(
        "evidence that is not an Evidence object",
        backend
            .attach_evidence(proto::AttachEvidenceRequest {
                change: wire::id(b),
                evidence: b"\x01".to_vec(),
            })
            .await,
        |e| matches!(e, ApiError::InvalidArgument(_)),
    );
}

/// A change whose ops do not reproduce its result is rejected, not landed
/// (spec §3.5), and head does not move.
async fn rejection(backend: &dyn RepoBackend, fx: &Fixture) {
    let built = fx.add_file("c.txt", "gamma\n", true);
    put(backend, &built).await;
    let mut events = backend
        .events(proto::EventsRequest { from: None })
        .await
        .expect("events");
    let change = wire::id(built.id);
    backend
        .submit(proto::SubmitRequest {
            change: change.clone(),
        })
        .await
        .expect("submit");
    until(
        &mut events,
        "Rejected",
        |k| matches!(k, Kind::Rejected(r) if r.change == change),
    )
    .await;
    let entry = backend
        .queue(proto::QueueQuery {
            change: Some(change.clone()),
            ..Default::default()
        })
        .await
        .expect("queue")
        .entries;
    assert_eq!(entry[0].status(), proto::QueueStatus::Rejected);
    assert!(entry[0].reason.is_some(), "a rejection has a reason");
    let head = backend.head(proto::HeadRequest {}).await.expect("head");
    assert_eq!(head.change, fx.head.map(wire::id), "head did not move");
    let pending = backend
        .queue(proto::QueueQuery {
            pending_only: true,
            ..Default::default()
        })
        .await
        .expect("queue");
    assert!(pending.entries.is_empty(), "nothing is left queued");
}

/// Arbitration is M5; refs list.
async fn unimplemented_until_m5(backend: &dyn RepoBackend) {
    expect_err(
        "arbitrate before M5",
        backend
            .arbitrate(proto::ArbitrateRequest {
                change: wire::id(ObjectId::from_canonical(b"x")),
                action: Some(proto::Arbitration {
                    action: Some(proto::arbitration::Action::Replay(true)),
                }),
            })
            .await,
        |e| matches!(e, ApiError::Unimplemented(_)),
    );
    let refs = backend
        .refs(proto::RefsRequest::default())
        .await
        .expect("refs");
    assert!(
        refs.refs
            .values()
            .all(|v| wire::object_id("refs", v).is_ok()),
        "refs name object ids: {refs:?}"
    );
}

/// The whole log replays from cursor 0 in order, and resuming from a
/// cursor yields exactly the events after it.
async fn events_resume(backend: &dyn RepoBackend) {
    let mut from_start = backend
        .events(proto::EventsRequest { from: Some(0) })
        .await
        .expect("events from 0");
    // Everything so far: through the Rejected event of the last check.
    let all = until(&mut from_start, "the Rejected event", |k| {
        matches!(k, Kind::Rejected(_))
    })
    .await;
    for (i, e) in all.iter().enumerate() {
        assert_eq!(e.cursor, i as u64 + 1, "cursors are 1, 2, 3, …");
    }
    let middle = all[all.len() / 2].cursor;
    let mut resumed = backend
        .events(proto::EventsRequest { from: Some(middle) })
        .await
        .expect("events from a cursor");
    let last = all.last().expect("events").cursor;
    let rest = until(&mut resumed, "the last event", |_| true).await;
    assert_eq!(rest[0].cursor, middle + 1, "resume starts after the cursor");
    let mut cursors = vec![rest[0].cursor];
    while cursors.last() != Some(&last) {
        let more = until(&mut resumed, "the last event", |_| true).await;
        cursors.push(more[0].cursor);
    }
    let expected: Vec<u64> = (middle + 1..=last).collect();
    assert_eq!(cursors, expected, "resume yields exactly the later events");
}
