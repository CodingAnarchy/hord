//! `hord policy check`: a dry run of spec §7.2 policy against a
//! workspace's current proposal.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hord(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_AGENT_MODEL", "test-model")
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = hord(dir, args);
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

const POLICY: &str = r#"[land]
require = ["check"]
strict_reads = false
max_write_set = 200
max_replay_attempts = 2

[[rule]]
name = "functions"
when = { touches_kind = "function_item", paths = ["src/**"] }
require = ["review:human"]

[[rule]]
name = "agent sources"
when = { actor = "agent", paths = ["src/*.rs"] }
require = ["review"]

[[rule]]
name = "docs"
when = { paths = ["docs/**"] }
require = ["review:human"]
"#;

/// The §7.2 example rules that need adapter facts, scoped to `src/`.
const ADAPTER_POLICY: &str = r#"[[rule]]
name = "unsafe requires human"
when = { touches_kind = "unsafe_block" }
require = ["review:human"]

[[rule]]
name = "public API"
when = { touches_visibility = "pub", paths = ["src/**"] }
require = ["review:human", "test:full"]
"#;

const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n\nfn two() -> u32 {\n    2\n}\n";

/// A repository with `src/lib.rs` and, if given, `.hord-policy.toml` at
/// head; a workspace; and a local policy file for `--policy`.
struct Fixture {
    _source: TempDir,
    repo: TempDir,
    checkout: PathBuf,
    policy: PathBuf,
}

fn fixture(head_policy: Option<&str>) -> Fixture {
    let source = TempDir::new("hord-policy-git");
    fs::create_dir_all(source.0.join("src")).unwrap();
    fs::write(source.0.join("src/lib.rs"), LIB).unwrap();
    if let Some(policy) = head_policy {
        fs::write(source.0.join(".hord-policy.toml"), policy).unwrap();
    }
    git(&source.0, &["init", "-q", "-b", "main"]);
    git(&source.0, &["add", "."]);
    git(&source.0, &["commit", "-q", "-m", "fixture"]);
    let repo = TempDir::new("hord-policy-repo");
    ok(&repo.0, &["init", "--from-git", source.0.to_str().unwrap()]);
    let ws: serde_json::Value =
        serde_json::from_str(&ok(&repo.0, &["ws", "new", "--json"])).unwrap();
    let checkout = PathBuf::from(ws["materialization"].as_str().unwrap());
    let policy = repo.0.join("policy.toml");
    fs::write(&policy, POLICY).unwrap();
    Fixture {
        _source: source,
        repo,
        checkout,
        policy,
    }
}

/// Run `policy check --json` with `extra` args; the exit code and JSON.
fn check(f: &Fixture, extra: &[&str]) -> (Option<i32>, serde_json::Value) {
    let mut args = vec!["policy", "check", "--json"];
    args.extend_from_slice(extra);
    let out = hord(&f.repo.0, &args);
    let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "hord {args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code(), json)
}

fn rules(json: &serde_json::Value) -> Vec<(String, String)> {
    json["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["rule"].as_str().unwrap_or("").to_owned(),
                r["requirement"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

#[test]
fn no_changes_has_no_decision() {
    let f = fixture(Some(POLICY));
    let (code, out) = check(&f, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(out["changes"], false);
    assert_eq!(out["policy_source"], "head");
    assert!(out.get("decision").is_none(), "{out}");
}

#[test]
fn deny_lists_rules_requirements_and_triggers() {
    let f = fixture(Some(POLICY));
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(1), "deny exits 1");
    assert_eq!(json["changes"], true);
    assert_eq!(json["policy_source"], "head");
    assert_eq!(json["policy"], ".hord-policy.toml");
    assert_eq!(json["actor"], "agent");
    assert_eq!(json["decision"], "deny");
    assert_eq!(json["evidence"], serde_json::json!([]));
    assert_eq!(
        rules(&json),
        [
            pair("", "check"),
            pair("functions", "review:human"),
            pair("agent sources", "review"),
        ]
    );
    let reasons = json["reasons"].as_array().unwrap();
    assert_eq!(reasons[0]["source"], "land");
    // `two` is the only definition written, and it is found by NodeId.
    let triggers = reasons[1]["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1, "{triggers:?}");
    assert_eq!(triggers[0]["on"], "definition");
    assert_eq!(triggers[0]["path"], "src/lib.rs");
    assert_eq!(triggers[0]["node"].as_str().unwrap().len(), 26);
    assert_eq!(
        reasons[2]["triggers"],
        serde_json::json!([
            { "on": "actor", "actor": "agent" },
            { "on": "path", "path": "src/lib.rs" },
        ])
    );

    let text = hord(&f.repo.0, &["policy", "check"]);
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("deny"), "{text}");
    assert!(
        text.contains("rule \"functions\": review:human missing"),
        "{text}"
    );
}

#[test]
fn head_policy_judges_not_the_change() {
    // Weakening the policy in the workspace does not change the verdict.
    let f = fixture(Some(POLICY));
    fs::write(f.checkout.join(".hord-policy.toml"), "").unwrap();
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(1));
    assert_eq!(json["policy_source"], "head");
    assert_eq!(rules(&json)[0], pair("", "check"));
}

#[test]
fn no_policy_file_allows_with_defaults() {
    let f = fixture(None);
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(json["policy_source"], "default");
    assert_eq!(json["decision"], "allow");
}

#[test]
fn policy_flag_overrides_head() {
    let f = fixture(Some(POLICY));
    fs::write(&f.policy, "[land]\nmax_write_set = 5\n").unwrap();
    fs::write(f.checkout.join("NOTES.md"), "notes\n").unwrap();
    let policy = f.policy.to_str().unwrap();
    let (code, json) = check(&f, &["--policy", policy]);
    assert_eq!(code, Some(0));
    assert_eq!(json["policy_source"], "file");
    assert_eq!(json["policy"], policy);
    assert_eq!(json["decision"], "allow");
    assert_eq!(json["changes"], true);
}

#[test]
fn adapter_reports_unsafe_blocks_and_visibility() {
    let f = fixture(Some(ADAPTER_POLICY));
    // `one` stays `pub` and changes; `two` gains an unsafe block.
    fs::write(
        f.checkout.join("src/lib.rs"),
        "pub fn one() -> u32 {\n    11\n}\n\nfn two() -> u32 {\n    unsafe { core::hint::unreachable_unchecked() }\n}\n",
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(
        rules(&json),
        [
            pair("unsafe requires human", "review:human"),
            pair("public API", "review:human"),
            pair("public API", "test:full"),
        ]
    );
    let reasons = json["reasons"].as_array().unwrap();
    let node = |i: usize| {
        reasons[i]["triggers"][0]["node"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(reasons[0]["triggers"].as_array().unwrap().len(), 1);
    assert_eq!(reasons[1]["triggers"].as_array().unwrap().len(), 1);
    assert_ne!(node(0), node(1), "unsafe `two` vs pub `one`");

    // A private function without unsafe triggers neither rule.
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(0), "{json}");
}

#[test]
fn deleting_a_pub_definition_is_public_api() {
    let f = fixture(Some(ADAPTER_POLICY));
    fs::write(
        f.checkout.join("src/lib.rs"),
        "fn two() -> u32 {\n    2\n}\n",
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(
        rules(&json),
        [
            pair("public API", "review:human"),
            pair("public API", "test:full")
        ]
    );
}

#[test]
fn bad_policy_reports_line_and_column() {
    let bad = "[land]\nrequire = [\"check\"]\nstrict_reads = 3\n";
    let f = fixture(Some(bad));
    let out = hord(&f.repo.0, &["policy", "check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains(".hord-policy.toml:3:16:"), "{stderr}");

    fs::write(&f.policy, bad).unwrap();
    let policy = f.policy.to_str().unwrap();
    let out = hord(&f.repo.0, &["policy", "check", "--policy", policy]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("policy.toml:3:16:"), "{stderr}");
}

#[test]
fn evidence_indexed_for_the_result_snapshot_counts() {
    let f = fixture(Some("[land]\nrequire = [\"check\", \"test:selected\"]\n"));
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )
    .unwrap();
    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(1));
    let snapshot: hord_core::ObjectId = json["snapshot"].as_str().unwrap().parse().unwrap();

    let store = hord_store::Store::open(&f.repo.0).unwrap();
    let evidence = |kind, qualifier: Option<&str>, snapshot| hord_core::Evidence {
        kind,
        qualifier: qualifier.map(str::to_owned),
        snapshot,
        toolchain: hord_core::ObjectId::from_canonical(b"toolchain"),
        command: "cargo test".into(),
        scope: None,
        result: hord_core::EvidenceResult::Pass,
        log: None,
        cost_ms: 1,
        produced_by: hord_core::Actor::Human { id: "ada".into() },
        produced_at: hord_core::Timestamp::from_millis(1),
    };
    store
        .put_evidence(&evidence(hord_core::EvidenceKind::Check, None, snapshot))
        .unwrap();
    // A full run meets `test:selected` (ADR 0026).
    store
        .put_evidence(&evidence(
            hord_core::EvidenceKind::Test,
            Some("full"),
            snapshot,
        ))
        .unwrap();
    // Evidence for another snapshot does not count.
    store
        .put_evidence(&evidence(
            hord_core::EvidenceKind::Lint,
            None,
            hord_core::ObjectId::from_canonical(b"other"),
        ))
        .unwrap();
    drop(store);

    let (code, json) = check(&f, &[]);
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json["decision"], "allow");
    assert_eq!(
        json["evidence"],
        serde_json::json!([
            { "tag": "check", "result": "Pass" },
            { "tag": "test:full", "result": "Pass" },
        ])
    );
}
