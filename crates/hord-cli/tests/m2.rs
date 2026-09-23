//! `hord log` filters, `hord blame`, and `hord query`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::BTreeMap;

use hord_core::{
    Actor, Blob, Bytes, ChangeRecord, FileIdentity, IdentityEntry, IdentityTree, Intent, NodeId,
    ObjectId, Op, Provenance, RepoPath, Snapshot, Timestamp, Tree, TreeEntry,
};
use hord_lang::LangAdapter;
use hord_lang_rust::RustAdapter;
use hord_store::{EdgeKind, Store};

const ADA: &str = "Ada <ada@example.com>";
const AGENT: &str = "agent-7";
const BODY_SECRET: &str = "INTENT-BODY-SECRET";
const EVIDENCE_SECRET: &str = "EVIDENCE-BYTES-SECRET";

fn hord_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hord"))
}

fn hord_in(dir: &Path, args: &[&str]) -> Output {
    hord_bin()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("run hord {args:?}: {err}"))
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_ok(out: &Output, args: &[&str]) {
    assert!(
        out.status.success(),
        "hord {args:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
}

fn assert_err(out: &Output, args: &[&str]) {
    assert!(
        !out.status.success(),
        "hord {args:?} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
}

fn json_stdout(out: &Output, args: &[&str]) -> serde_json::Value {
    assert_ok(out, args);
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!("json ({err}) from hord {args:?}\n{}", stdout(out));
    })
}

fn json_error(out: &Output, args: &[&str]) -> String {
    assert_err(out, args);
    let value: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|err| {
        panic!("error json ({err}) from hord {args:?}\n{}", stderr(out));
    });
    value["error"].as_str().unwrap_or("").to_owned()
}

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("missing array {key} in {value}"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("non-string in {key}: {item}"))
                .to_owned()
        })
        .collect()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Seed {
    dir: TempDir,
    alpha: NodeId,
    beta: NodeId,
    other: NodeId,
    test_node: NodeId,
    snapshot: ObjectId,
    evidence: ObjectId,
    ghost: ObjectId,
    add_alpha: ObjectId,
    rewrite: ObjectId,
    edit_both: ObjectId,
    edit_beta: ObjectId,
    delete_beta: ObjectId,
    read_alpha: ObjectId,
    blob: ObjectId,
}

impl Seed {
    fn changes(&self) -> [&ObjectId; 6] {
        [
            &self.add_alpha,
            &self.rewrite,
            &self.edit_both,
            &self.edit_beta,
            &self.delete_beta,
            &self.read_alpha,
        ]
    }
}

fn id_list(ids: &[&ObjectId]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn seed() -> Seed {
    let dir = TempDir::new("hord-m2");
    let alpha = NodeId::from_u128(1);
    let beta = NodeId::from_u128(2);
    let other = NodeId::from_u128(9);
    let test_node = NodeId::from_u128(8);
    let decoy = NodeId::from_u128(4);

    let store = Store::create(dir.path()).unwrap();
    let snapshot = snapshot(&store, &[("src/lib.rs", LIB_SRC, &[alpha, beta])]);
    let empty_tree = empty_snapshot(&store);
    store
        .put_edge(empty_tree, EdgeKind::References, alpha, decoy)
        .unwrap();
    store
        .put_edge(snapshot, EdgeKind::References, alpha, beta)
        .unwrap();
    store
        .put_edge(snapshot, EdgeKind::References, alpha, other)
        .unwrap();
    store
        .put_edge(snapshot, EdgeKind::Depends, beta, alpha)
        .unwrap();
    store
        .put_edge(snapshot, EdgeKind::Tests, test_node, alpha)
        .unwrap();

    let evidence = store
        .put_object(&Blob::new(EVIDENCE_SECRET.as_bytes().to_vec()))
        .unwrap();
    let ghost = ObjectId::from_bytes([0xee; 32]);

    let add_alpha = land(
        &store,
        Draft::new("add alpha", ada(), 1_000, snapshot, snapshot)
            .write([alpha])
            .evidence([evidence, ghost]),
    );
    let rewrite = land(
        &store,
        Draft::new("rewrite from base", ada(), 3_000, snapshot, empty_tree).write([alpha]),
    );
    let edit_both = land(
        &store,
        Draft::new("edit both", agent(), 5_000, snapshot, snapshot).write([alpha, beta]),
    );
    let edit_beta = land(
        &store,
        Draft::new("edit beta", ada(), 9_000, snapshot, snapshot).write([beta]),
    );
    let delete_beta = land(
        &store,
        Draft::new("delete beta", ada(), 9_001, snapshot, snapshot)
            .ops(vec![Op::Delete { node: beta }]),
    );
    let read_alpha = land(
        &store,
        Draft::new("read alpha", ada(), 1_500, snapshot, snapshot).read([alpha]),
    );
    store.set_head(read_alpha).unwrap();
    store.append_log(evidence).unwrap();
    store.flush().unwrap();
    drop(store);

    Seed {
        dir,
        alpha,
        beta,
        other,
        test_node,
        snapshot,
        evidence,
        ghost,
        add_alpha,
        rewrite,
        edit_both,
        edit_beta,
        delete_beta,
        read_alpha,
        blob: evidence,
    }
}

/// `alpha` covers lines 1-3 and `beta` lines 4-6.
const LIB_SRC: &str = "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\n";

/// A snapshot of Rust `files` (ADR 0017): the content tree, and an identity
/// tree whose [`FileIdentity`] gives each file's definitions `ids` in
/// preorder (the rest keep the fresh assignment).
fn snapshot(store: &Store, files: &[(&str, &str, &[NodeId])]) -> ObjectId {
    let mut content = Vec::new();
    let mut identity = Vec::new();
    for (path, source, ids) in files {
        let path: RepoPath = path.parse().unwrap();
        let blob = store
            .put_object(&Blob::new(source.as_bytes().to_vec()))
            .unwrap();
        let tree = RustAdapter.parse(source.as_bytes()).unwrap();
        let fresh = hord_identity::assign_in(&RustAdapter, &path, None, &tree);
        let mut nodes: Vec<(Vec<u32>, NodeId)> = fresh.nodes.into_iter().collect();
        assert!(
            nodes.len() >= ids.len(),
            "{path} has {} definitions",
            nodes.len()
        );
        for (slot, id) in nodes.iter_mut().zip(ids.iter()) {
            slot.1 = *id;
        }
        let file = store.put_object(&FileIdentity { blob, nodes }).unwrap();
        content.push((path.components().to_vec(), blob));
        identity.push((path.components().to_vec(), file));
    }
    let tree = put_dir(
        store,
        &content,
        TreeEntry::Blob,
        TreeEntry::Tree,
        Tree::default(),
        |t: &mut Tree| &mut t.entries,
    );
    let ids = put_dir(
        store,
        &identity,
        IdentityEntry::File,
        IdentityEntry::Dir,
        IdentityTree::default(),
        |t: &mut IdentityTree| &mut t.entries,
    );
    store.put_object(&Snapshot::new(tree, ids)).unwrap()
}

/// Store a directory (content or identity) of `files` and return its id.
fn put_dir<T: serde::Serialize + Clone, E>(
    store: &Store,
    files: &[(Vec<String>, ObjectId)],
    leaf: fn(ObjectId) -> E,
    dir: fn(ObjectId) -> E,
    empty: T,
    entries: fn(&mut T) -> &mut BTreeMap<String, E>,
) -> ObjectId {
    let mut out = empty.clone();
    let mut nested: BTreeMap<String, Vec<(Vec<String>, ObjectId)>> = BTreeMap::new();
    for (path, id) in files {
        match path.as_slice() {
            [name] => {
                entries(&mut out).insert(name.clone(), leaf(*id));
            }
            [first, rest @ ..] => nested
                .entry(first.clone())
                .or_default()
                .push((rest.to_vec(), *id)),
            [] => {}
        }
    }
    for (name, sub) in nested {
        let id = put_dir(store, &sub, leaf, dir, empty.clone(), entries);
        entries(&mut out).insert(name, dir(id));
    }
    store.put_object(&out).unwrap()
}

/// [`Snapshot::empty`], stored with the objects it names.
fn empty_snapshot(store: &Store) -> ObjectId {
    store.put_object(&Tree::default()).unwrap();
    store.put_object(&IdentityTree::default()).unwrap();
    store.put_object(&Snapshot::empty()).unwrap()
}

fn mark(n: u8) -> ObjectId {
    let mut bytes = [0x11; 32];
    bytes[0] = n;
    ObjectId::from_bytes(bytes)
}

fn ada() -> Actor {
    Actor::Human { id: ADA.into() }
}

fn agent() -> Actor {
    Actor::Agent {
        id: AGENT.into(),
        model: "model".into(),
        model_hash: Bytes::default(),
        harness: "harness".into(),
    }
}

struct Draft {
    summary: &'static str,
    actor: Actor,
    at: u64,
    base: ObjectId,
    result: ObjectId,
    write: Vec<NodeId>,
    read: Vec<NodeId>,
    ops: Vec<Op>,
    evidence: Vec<ObjectId>,
}

impl Draft {
    fn new(summary: &'static str, actor: Actor, at: u64, base: ObjectId, result: ObjectId) -> Self {
        Self {
            summary,
            actor,
            at,
            base,
            result,
            write: Vec::new(),
            read: Vec::new(),
            ops: Vec::new(),
            evidence: Vec::new(),
        }
    }

    fn write(mut self, nodes: impl IntoIterator<Item = NodeId>) -> Self {
        self.write.extend(nodes);
        self
    }

    fn read(mut self, nodes: impl IntoIterator<Item = NodeId>) -> Self {
        self.read.extend(nodes);
        self
    }

    fn ops(mut self, ops: Vec<Op>) -> Self {
        self.ops = ops;
        self
    }

    fn evidence(mut self, ids: impl IntoIterator<Item = ObjectId>) -> Self {
        self.evidence.extend(ids);
        self
    }

    fn build(self) -> ChangeRecord {
        ChangeRecord {
            base: self.base,
            result: self.result,
            parents: Vec::new(),
            ops: self.ops,
            intent: Intent {
                summary: self.summary.into(),
                body: BODY_SECRET.into(),
                refs: Vec::new(),
                acceptance: Vec::new(),
            },
            provenance: Provenance {
                actor: self.actor,
                toolchain: mark(3),
                created_at: Timestamp::from_millis(self.at),
                session: None,
                parent_intent: None,
            },
            read_set: self.read.into_iter().collect(),
            write_set: self.write.into_iter().collect(),
            identity_deltas: Vec::new(),
            evidence: self.evidence,
            signature: None,
            rebased_from: None,
        }
    }
}

fn land(store: &Store, draft: Draft) -> ObjectId {
    let id = store.put_object(&draft.build()).unwrap();
    store.append_log(id).unwrap();
    store.index_change(id).unwrap();
    id
}

fn append_change(store: &Store, change: &ChangeRecord) -> ObjectId {
    let id = store.put_object(change).unwrap();
    store.append_log(id).unwrap();
    id
}

fn node_hex(id: NodeId) -> String {
    format!("{:032x}", id.as_u128())
}

#[test]
fn log_help_lists_filters() {
    let out = hord_bin().args(["log", "--help"]).output().unwrap();
    assert_ok(&out, &["log", "--help"]);
    let text = stdout(&out);
    for flag in ["--node", "--path", "--actor", "--since"] {
        assert!(text.contains(flag), "help missing {flag}:\n{text}");
    }
}

#[test]
fn query_help_lists_edges() {
    let out = hord_bin().args(["query", "--help"]).output().unwrap();
    assert_ok(&out, &["query", "--help"]);
    let text = stdout(&out);
    for name in ["references", "dependents", "tests-of"] {
        assert!(text.contains(name), "help missing {name}:\n{text}");
    }
}

#[test]
fn unfiltered_log_lists_every_landed_id() {
    let seed = seed();
    let out = hord_in(seed.dir.path(), &["log", "--json"]);
    let value = json_stdout(&out, &["log", "--json"]);
    let mut expected = id_list(&seed.changes());
    expected.push(seed.blob.to_string());
    assert_eq!(strings(&value, "log"), expected);
    let head = seed.read_alpha.to_string();
    assert_eq!(value["head"].as_str(), Some(head.as_str()));
}

#[test]
fn log_filters_by_node_path_actor_and_since() {
    let seed = seed();
    let dir = seed.dir.path();
    let alpha = seed.alpha.to_string();
    let by_ulid = hord_in(dir, &["log", "--json", "--node", &alpha]);
    let by_hex = hord_in(dir, &["log", "--json", "--node", &node_hex(seed.alpha)]);
    let by_upper = hord_in(
        dir,
        &[
            "log",
            "--json",
            "--node",
            &node_hex(seed.alpha).to_ascii_uppercase(),
        ],
    );
    let by_name = hord_in(dir, &["log", "--json", "--node", "alpha"]);
    let expected = id_list(&[&seed.add_alpha, &seed.rewrite, &seed.edit_both]);
    for (out, args) in [
        (&by_ulid, "--node ulid"),
        (&by_hex, "--node hex"),
        (&by_upper, "--node HEX"),
        (&by_name, "--node name"),
    ] {
        assert_eq!(
            strings(&json_stdout(out, &[args]), "log"),
            expected,
            "{args}"
        );
    }

    let beta = hord_in(dir, &["log", "--json", "--node", &seed.beta.to_string()]);
    assert_eq!(
        strings(&json_stdout(&beta, &["log", "--node", "beta"]), "log"),
        id_list(&[&seed.edit_both, &seed.edit_beta, &seed.delete_beta])
    );

    let path = hord_in(dir, &["log", "--json", "--path", "src/lib.rs"]);
    let path_ids = id_list(&[
        &seed.add_alpha,
        &seed.rewrite,
        &seed.edit_both,
        &seed.edit_beta,
    ]);
    assert_eq!(
        strings(&json_stdout(&path, &["log", "--path"]), "log"),
        path_ids
    );
    let dir_path = hord_in(dir, &["log", "--json", "--path", "src"]);
    assert_eq!(
        strings(&json_stdout(&dir_path, &["log", "--path", "src"]), "log"),
        path_ids
    );
    let missed = hord_in(dir, &["log", "--json", "--path", "src/lib"]);
    assert_eq!(
        strings(&json_stdout(&missed, &["log", "--path", "src/lib"]), "log"),
        Vec::<String>::new()
    );

    let actor = hord_in(dir, &["log", "--json", "--actor", AGENT]);
    assert_eq!(
        strings(&json_stdout(&actor, &["log", "--actor"]), "log"),
        id_list(&[&seed.edit_both])
    );
    let model = hord_in(dir, &["log", "--json", "--actor", "model"]);
    assert_eq!(
        strings(&json_stdout(&model, &["log", "--actor", "model"]), "log"),
        Vec::<String>::new()
    );

    let since = hord_in(dir, &["log", "--json", "--since", "5000"]);
    assert_eq!(
        strings(&json_stdout(&since, &["log", "--since"]), "log"),
        id_list(&[&seed.edit_both, &seed.edit_beta, &seed.delete_beta])
    );
    let boundary = hord_in(dir, &["log", "--json", "--since", "9000"]);
    assert_eq!(
        strings(&json_stdout(&boundary, &["log", "--since", "9000"]), "log"),
        id_list(&[&seed.edit_beta, &seed.delete_beta])
    );
    let all_changes = hord_in(dir, &["log", "--json", "--since", "0"]);
    assert_eq!(
        strings(&json_stdout(&all_changes, &["log", "--since", "0"]), "log"),
        id_list(&seed.changes())
    );

    let combined = hord_in(
        dir,
        &[
            "log",
            "--json",
            "--node",
            "alpha",
            "--path",
            "src/lib.rs",
            "--actor",
            AGENT,
            "--since",
            "5000",
        ],
    );
    assert_eq!(
        strings(&json_stdout(&combined, &["log", "combined"]), "log"),
        id_list(&[&seed.edit_both])
    );

    let empty = hord_in(dir, &["log", "--actor", "nobody"]);
    assert_ok(&empty, &["log", "--actor", "nobody"]);
    assert!(stdout(&empty).contains("(empty log)"), "{}", stdout(&empty));
}

#[test]
fn log_rejects_a_bad_path_and_an_unknown_name() {
    let seed = seed();
    let bad = hord_in(seed.dir.path(), &["log", "--json", "--path", "/src"]);
    let err = json_error(&bad, &["log", "--path", "/src"]);
    assert!(err.contains("path"), "{err}");
    let missing = hord_in(
        seed.dir.path(),
        &["log", "--json", "--node", "crate::missing"],
    );
    let err = json_error(&missing, &["log", "--node", "missing"]);
    assert!(err.contains("cannot resolve node"), "{err}");
}

#[test]
fn blame_prints_history_from_the_index() {
    let seed = seed();
    let dir = seed.dir.path();
    let by_name = hord_in(dir, &["blame", "--json", "alpha"]);
    let value = json_stdout(&by_name, &["blame", "alpha"]);
    assert_no_secrets(&value, &stdout(&by_name));
    let alpha = seed.alpha.to_string();
    let add_alpha = seed.add_alpha.to_string();
    assert_eq!(value["node"].as_str(), Some(alpha.as_str()));
    let history = value["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["change"].as_str(), Some(add_alpha.as_str()));
    assert_eq!(history[0]["intent"].as_str(), Some("add alpha"));
    assert_eq!(history[0]["actor"].as_str(), Some(ADA));
    assert_eq!(
        strings(&history[0], "evidence"),
        vec![seed.evidence.to_string(), seed.ghost.to_string()]
    );
    assert_eq!(history[1]["intent"].as_str(), Some("rewrite from base"));
    assert_eq!(strings(&history[1], "evidence"), Vec::<String>::new());
    let edit_both = seed.edit_both.to_string();
    assert_eq!(history[2]["change"].as_str(), Some(edit_both.as_str()));
    assert_eq!(history[2]["intent"].as_str(), Some("edit both"));
    assert_eq!(history[2]["actor"].as_str(), Some(AGENT));
    let text = serde_json::to_string(&value).unwrap();
    assert!(!text.contains("read alpha"), "{text}");
    assert!(!text.contains("edit beta"), "{text}");

    let by_id = hord_in(dir, &["blame", "--json", &node_hex(seed.alpha)]);
    let by_id = json_stdout(&by_id, &["blame", "node"]);
    assert_eq!(by_id["node"], value["node"]);
    assert_eq!(by_id["history"], value["history"]);

    let by_line = hord_in(dir, &["blame", "--json", "src/lib.rs:2"]);
    let by_line = json_stdout(&by_line, &["blame", "src/lib.rs:2"]);
    assert_eq!(by_line["node"].as_str(), Some(alpha.as_str()));
    assert_eq!(by_line["history"], value["history"]);

    let beta_line = hord_in(dir, &["blame", "--json", "src/lib.rs:5"]);
    let beta_line = json_stdout(&beta_line, &["blame", "src/lib.rs:5"]);
    let beta = seed.beta.to_string();
    assert_eq!(beta_line["node"].as_str(), Some(beta.as_str()));
    assert_eq!(
        beta_line["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["intent"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["edit both", "edit beta", "delete beta"]
    );

    let human = hord_in(dir, &["blame", "beta"]);
    assert_ok(&human, &["blame", "beta"]);
    let text = stdout(&human);
    let edit_beta = seed.edit_beta.to_string();
    assert!(text.contains(&beta), "{text}");
    assert!(text.contains("intent: edit beta"), "{text}");
    assert!(text.contains("actor: "), "{text}");
    assert!(text.contains(&edit_beta), "{text}");
    assert!(!text.contains(BODY_SECRET), "{text}");
    assert!(!text.contains(EVIDENCE_SECRET), "{text}");

    let outside = hord_in(dir, &["blame", "--json", "src/lib.rs:7"]);
    let err = json_error(&outside, &["blame", "src/lib.rs:7"]);
    assert!(
        err.contains("outside") || err.contains("no definition"),
        "{err}"
    );
    let unknown = hord_in(dir, &["blame", "--json", "missing"]);
    let err = json_error(&unknown, &["blame", "missing"]);
    assert!(err.contains("cannot resolve node"), "{err}");
}

#[test]
fn query_reads_edges_from_the_latest_change_result() {
    let seed = seed();
    let dir = seed.dir.path();
    let alpha = seed.alpha.to_string();
    let refs = hord_in(dir, &["query", "--json", "references", &alpha]);
    let refs = json_stdout(&refs, &["query", "references"]);
    let snapshot = seed.snapshot.to_hex();
    let beta = seed.beta.to_string();
    let other = seed.other.to_string();
    assert_eq!(refs["snapshot"].as_str(), Some(snapshot.as_str()));
    assert_eq!(refs["edge"].as_str(), Some("references"));
    assert_eq!(refs["node"].as_str(), Some(alpha.as_str()));
    assert_eq!(strings(&refs, "targets"), vec![beta.clone(), other.clone()]);

    let human = hord_in(dir, &["query", "references", &node_hex(seed.alpha)]);
    assert_ok(&human, &["query", "references"]);
    let lines: Vec<String> = stdout(&human)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(lines, vec![beta, other]);

    let deps = hord_in(dir, &["query", "--json", "dependents", &alpha]);
    let deps = json_stdout(&deps, &["query", "dependents"]);
    assert_eq!(strings(&deps, "targets"), Vec::<String>::new());
    let from_beta = hord_in(
        dir,
        &["query", "--json", "dependents", &seed.beta.to_string()],
    );
    let from_beta = json_stdout(&from_beta, &["query", "dependents", "beta"]);
    assert_eq!(strings(&from_beta, "targets"), vec![alpha.clone()]);

    let tests = hord_in(
        dir,
        &["query", "--json", "tests-of", &seed.test_node.to_string()],
    );
    let tests = json_stdout(&tests, &["query", "tests-of"]);
    assert_eq!(strings(&tests, "targets"), vec![seed.alpha.to_string()]);
    let not_target = hord_in(dir, &["query", "--json", "tests-of", &alpha]);
    let not_target = json_stdout(&not_target, &["query", "tests-of", "alpha"]);
    assert_eq!(strings(&not_target, "targets"), Vec::<String>::new());

    let none = hord_in(dir, &["query", "dependents", &alpha]);
    assert_ok(&none, &["query", "dependents"]);
    assert!(stdout(&none).contains("(none)"), "{}", stdout(&none));
}

#[test]
fn query_prefers_the_newest_result_over_an_older_identity_snapshot() {
    let dir = TempDir::new("hord-m2-query-latest");
    let source = NodeId::from_u128(1);
    let older_target = NodeId::from_u128(2);
    let newer_target = NodeId::from_u128(3);
    let new_hex = {
        let store = Store::create(dir.path()).unwrap();
        let old = snapshot(&store, &[("a.rs", "fn source() {}\n", &[source])]);
        let new = snapshot(&store, &[("a.rs", "fn source() { }\n", &[source])]);
        store
            .put_edge(old, EdgeKind::References, source, older_target)
            .unwrap();
        store
            .put_edge(new, EdgeKind::References, source, newer_target)
            .unwrap();
        append_change(&store, &plain_change(old, source, "old"));
        let head = append_change(&store, &plain_change(new, source, "new"));
        store.set_head(head).unwrap();
        new.to_hex()
    };
    let out = hord_in(
        dir.path(),
        &["query", "--json", "references", &source.to_string()],
    );
    let value = json_stdout(&out, &["query", "latest"]);
    assert_eq!(value["snapshot"].as_str(), Some(new_hex.as_str()));
    assert_eq!(strings(&value, "targets"), vec![newer_target.to_string()]);
}

#[test]
fn qualified_name_uses_the_newest_identity_snapshot() {
    let dir = TempDir::new("hord-m2-name");
    let old_node = NodeId::from_u128(1);
    let new_node = NodeId::from_u128(2);
    let (c_old, c_new) = {
        let store = Store::create(dir.path()).unwrap();
        let old = snapshot(
            &store,
            &[("lib.rs", "fn alpha() { /* 1 */ }\n", &[old_node])],
        );
        let new = snapshot(
            &store,
            &[("lib.rs", "fn alpha() { /* 2 */ }\n", &[new_node])],
        );
        let c_old = append_change(&store, &plain_change(old, old_node, "old alpha"));
        let c_new = append_change(&store, &plain_change(new, new_node, "new alpha"));
        store.set_head(c_new).unwrap();
        (c_old, c_new)
    };
    let out = hord_in(dir.path(), &["log", "--json", "--node", "alpha"]);
    let value = json_stdout(&out, &["log", "--node", "alpha"]);
    assert_eq!(strings(&value, "log"), vec![c_new.to_string()]);
    let by_old = hord_in(
        dir.path(),
        &["log", "--json", "--node", &old_node.to_string()],
    );
    let by_old = json_stdout(&by_old, &["log", "--node", "old"]);
    assert_eq!(strings(&by_old, "log"), vec![c_old.to_string()]);
}

#[test]
fn ambiguous_qualified_name_fails() {
    let tmp = TempDir::new("hord-m2-ambiguous");
    {
        let store = Store::create(tmp.path()).unwrap();
        let left = NodeId::from_u128(1);
        let right = NodeId::from_u128(2);
        let snapshot = snapshot(
            &store,
            &[("lib.rs", "fn alpha() {}\nfn alpha() { }\n", &[left, right])],
        );
        let head = append_change(&store, &plain_change(snapshot, left, "two alphas"));
        store.set_head(head).unwrap();
    }
    let out = hord_in(tmp.path(), &["log", "--json", "--node", "alpha"]);
    let err = json_error(&out, &["log", "--node", "alpha"]);
    assert!(err.contains("multiple"), "{err}");
}

#[test]
fn log_path_uses_the_result_location_when_the_node_moved() {
    let dir = TempDir::new("hord-m2-moved-path");
    let alpha = NodeId::from_u128(1);
    let moved = {
        let store = Store::create(dir.path()).unwrap();
        let old = snapshot(&store, &[("src/old.rs", "fn alpha() {}\n", &[alpha])]);
        let new = snapshot(&store, &[("src/new.rs", "fn alpha() {}\n", &[alpha])]);
        let empty = empty_snapshot(&store);
        let moved = append_change(
            &store,
            &Draft::new("move alpha", ada(), 1, old, new)
                .write([alpha])
                .build(),
        );
        let dropped = append_change(
            &store,
            &Draft::new("drop alpha", ada(), 2, new, empty)
                .write([alpha])
                .build(),
        );
        store.set_head(dropped).unwrap();
        moved
    };

    let at_old = hord_in(dir.path(), &["log", "--json", "--path", "src/old.rs"]);
    assert_eq!(
        strings(
            &json_stdout(&at_old, &["log", "--path", "src/old.rs"]),
            "log"
        ),
        Vec::<String>::new()
    );
    let at_new = hord_in(dir.path(), &["log", "--json", "--path", "src/new.rs"]);
    let ids = strings(
        &json_stdout(&at_new, &["log", "--path", "src/new.rs"]),
        "log",
    );
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_eq!(ids[0], moved.to_string());
}

#[test]
fn query_without_a_snapshot_fails() {
    let dir = TempDir::new("hord-m2-empty");
    assert_ok(&hord_in(dir.path(), &["init"]), &["init"]);
    let out = hord_in(
        dir.path(),
        &[
            "query",
            "--json",
            "references",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
    );
    let err = json_error(&out, &["query"]);
    assert!(err.contains("cannot resolve a snapshot"), "{err}");

    let log = hord_in(
        dir.path(),
        &["log", "--json", "--node", "01ARZ3NDEKTSV4RRFFQ69G5FAV"],
    );
    let log = json_stdout(&log, &["log", "--node"]);
    assert_eq!(strings(&log, "log"), Vec::<String>::new());

    let bad = hord_in(dir.path(), &["query", "--json", "references", "not-a-node"]);
    let err = json_error(&bad, &["query", "not-a-node"]);
    assert!(err.contains("invalid NodeId"), "{err}");

    let edge = hord_in(dir.path(), &["query", "nope", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
    assert_err(&edge, &["query", "nope"]);
}

fn assert_no_secrets(value: &serde_json::Value, text: &str) {
    let rendered = serde_json::to_string(value).unwrap();
    assert!(!rendered.contains(BODY_SECRET), "{rendered}");
    assert!(!rendered.contains(EVIDENCE_SECRET), "{rendered}");
    assert!(!text.contains(BODY_SECRET), "{text}");
    assert!(!text.contains(EVIDENCE_SECRET), "{text}");
}

fn plain_change(result: ObjectId, write: NodeId, summary: &'static str) -> ChangeRecord {
    Draft::new(summary, ada(), 1, result, result)
        .write([write])
        .build()
}
