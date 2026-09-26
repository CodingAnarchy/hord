//! `hord policy check`: a dry run of spec §7.2 policy against a
//! workspace's current proposal.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> TestResult<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        common::clear_stale(&path)?;
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        common::drop_tree(&self.0);
    }
}

fn hord(dir: &Path, args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_hord"))
        .args(args)
        .current_dir(dir)
        .env("HORD_ACTOR", "tester")
        .env("HORD_AGENT_MODEL", "test-model")
        // These tests open the store themselves; see `daemon.rs` for the
        // daemon path.
        .env("HORD_NO_DAEMON", "1")
        .output()?)
}

fn ok(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = hord(dir, args)?;
    assert!(
        out.status.success(),
        "hord {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

fn git(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()?;
    assert!(out.status.success(), "git {args:?}");
    Ok(())
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

fn fixture(head_policy: Option<&str>) -> TestResult<Fixture> {
    let source = TempDir::new("hord-policy-git")?;
    fs::create_dir_all(source.0.join("src"))?;
    fs::write(source.0.join("src/lib.rs"), LIB)?;
    if let Some(policy) = head_policy {
        fs::write(source.0.join(".hord-policy.toml"), policy)?;
    }
    git(&source.0, &["init", "-q", "-b", "main"])?;
    git(&source.0, &["add", "."])?;
    git(&source.0, &["commit", "-q", "-m", "fixture"])?;
    let repo = TempDir::new("hord-policy-repo")?;
    let from = source.0.to_str().ok_or("UTF-8 source path")?;
    ok(&repo.0, &["init", "--from-git", from])?;
    let ws: serde_json::Value = serde_json::from_str(&ok(&repo.0, &["ws", "new", "--json"])?)?;
    let checkout = PathBuf::from(
        ws["materialization"]
            .as_str()
            .ok_or("workspace materialization")?,
    );
    let policy = repo.0.join("policy.toml");
    fs::write(&policy, POLICY)?;
    Ok(Fixture {
        _source: source,
        repo,
        checkout,
        policy,
    })
}

/// Run `policy check --json` with `extra` args; the exit code and JSON.
fn check(f: &Fixture, extra: &[&str]) -> TestResult<(Option<i32>, serde_json::Value)> {
    let mut args = vec!["policy", "check", "--json"];
    args.extend_from_slice(extra);
    let out = hord(&f.repo.0, &args)?;
    let json = serde_json::from_slice(&out.stdout).map_err(|e| {
        format!(
            "hord {args:?}: {e}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    Ok((out.status.code(), json))
}

fn rules(json: &serde_json::Value) -> TestResult<Vec<(String, String)>> {
    let rules = json["reasons"]
        .as_array()
        .ok_or("reasons array")?
        .iter()
        .map(|r| {
            let requirement = r["requirement"].as_str().ok_or("reason requirement")?;
            Ok::<_, &str>((
                r["rule"].as_str().unwrap_or("").to_owned(),
                requirement.to_owned(),
            ))
        })
        .collect::<Result<_, _>>()?;
    Ok(rules)
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

#[test]
fn no_changes_has_no_decision() -> TestResult {
    let f = fixture(Some(POLICY))?;
    let (code, out) = check(&f, &[])?;
    assert_eq!(code, Some(0));
    assert_eq!(out["changes"], false);
    assert_eq!(out["policy_source"], "head");
    assert!(out.get("decision").is_none(), "{out}");
    Ok(())
}

#[test]
fn deny_lists_rules_requirements_and_triggers() -> TestResult {
    let f = fixture(Some(POLICY))?;
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(1), "deny exits 1");
    assert_eq!(json["changes"], true);
    assert_eq!(json["policy_source"], "head");
    assert_eq!(json["policy"], ".hord-policy.toml");
    assert_eq!(json["actor"], "agent");
    assert_eq!(json["decision"], "deny");
    assert_eq!(json["evidence"], serde_json::json!([]));
    assert_eq!(
        rules(&json)?,
        [
            pair("", "check"),
            pair("functions", "review:human"),
            pair("agent sources", "review"),
        ]
    );
    let reasons = json["reasons"].as_array().ok_or("reasons array")?;
    assert_eq!(reasons[0]["source"], "land");
    // `two` is the only definition written, and it is found by NodeId.
    let triggers = reasons[1]["triggers"].as_array().ok_or("triggers array")?;
    assert_eq!(triggers.len(), 1, "{triggers:?}");
    assert_eq!(triggers[0]["on"], "definition");
    assert_eq!(triggers[0]["path"], "src/lib.rs");
    assert_eq!(
        triggers[0]["node"].as_str().ok_or("trigger node id")?.len(),
        26
    );
    assert_eq!(
        reasons[2]["triggers"],
        serde_json::json!([
            { "on": "actor", "actor": "agent" },
            { "on": "path", "path": "src/lib.rs" },
        ])
    );

    let text = hord(&f.repo.0, &["policy", "check"])?;
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8(text.stdout)?;
    assert!(text.contains("deny"), "{text}");
    assert!(
        text.contains("rule \"functions\": review:human missing"),
        "{text}"
    );
    Ok(())
}

#[test]
fn head_policy_judges_not_the_change() -> TestResult {
    // Weakening the policy in the workspace does not change the verdict.
    let f = fixture(Some(POLICY))?;
    fs::write(f.checkout.join(".hord-policy.toml"), "")?;
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(1));
    assert_eq!(json["policy_source"], "head");
    assert_eq!(rules(&json)?[0], pair("", "check"));
    Ok(())
}

#[test]
fn no_policy_file_allows_with_defaults() -> TestResult {
    let f = fixture(None)?;
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(0));
    assert_eq!(json["policy_source"], "default");
    assert_eq!(json["decision"], "allow");
    Ok(())
}

#[test]
fn policy_flag_overrides_head() -> TestResult {
    let f = fixture(Some(POLICY))?;
    fs::write(&f.policy, "[land]\nmax_write_set = 5\n")?;
    fs::write(f.checkout.join("NOTES.md"), "notes\n")?;
    let policy = f.policy.to_str().ok_or("UTF-8 policy path")?;
    let (code, json) = check(&f, &["--policy", policy])?;
    assert_eq!(code, Some(0));
    assert_eq!(json["policy_source"], "file");
    assert_eq!(json["policy"], policy);
    assert_eq!(json["decision"], "allow");
    assert_eq!(json["changes"], true);
    Ok(())
}

#[test]
fn adapter_reports_unsafe_blocks_and_visibility() -> TestResult {
    let f = fixture(Some(ADAPTER_POLICY))?;
    // `one` stays `pub` and changes; `two` gains an unsafe block.
    fs::write(
        f.checkout.join("src/lib.rs"),
        "pub fn one() -> u32 {\n    11\n}\n\nfn two() -> u32 {\n    unsafe { core::hint::unreachable_unchecked() }\n}\n",
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(
        rules(&json)?,
        [
            pair("unsafe requires human", "review:human"),
            pair("public API", "review:human"),
            pair("public API", "test:full"),
        ]
    );
    let reasons = json["reasons"].as_array().ok_or("reasons array")?;
    let node = |i: usize| {
        reasons[i]["triggers"][0]["node"]
            .as_str()
            .map(str::to_owned)
            .ok_or("trigger node id")
    };
    assert_eq!(
        reasons[0]["triggers"]
            .as_array()
            .ok_or("unsafe triggers")?
            .len(),
        1
    );
    assert_eq!(
        reasons[1]["triggers"]
            .as_array()
            .ok_or("public API triggers")?
            .len(),
        1
    );
    assert_ne!(node(0)?, node(1)?, "unsafe `two` vs pub `one`");

    // A private function without unsafe triggers neither rule.
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(0), "{json}");
    Ok(())
}

#[test]
fn deleting_a_pub_definition_is_public_api() -> TestResult {
    let f = fixture(Some(ADAPTER_POLICY))?;
    fs::write(
        f.checkout.join("src/lib.rs"),
        "fn two() -> u32 {\n    2\n}\n",
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(
        rules(&json)?,
        [
            pair("public API", "review:human"),
            pair("public API", "test:full")
        ]
    );
    Ok(())
}

#[test]
fn bad_policy_reports_line_and_column() -> TestResult {
    let bad = "[land]\nrequire = [\"check\"]\nstrict_reads = 3\n";
    let f = fixture(Some(bad))?;
    let out = hord(&f.repo.0, &["policy", "check"])?;
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr)?;
    assert!(stderr.contains(".hord-policy.toml:3:16:"), "{stderr}");

    fs::write(&f.policy, bad)?;
    let policy = f.policy.to_str().ok_or("UTF-8 policy path")?;
    let out = hord(&f.repo.0, &["policy", "check", "--policy", policy])?;
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr)?;
    assert!(stderr.contains("policy.toml:3:16:"), "{stderr}");
    Ok(())
}

#[test]
fn evidence_indexed_for_the_result_snapshot_counts() -> TestResult {
    let f = fixture(Some("[land]\nrequire = [\"check\", \"test:selected\"]\n"))?;
    fs::write(
        f.checkout.join("src/lib.rs"),
        LIB.replace("    2\n", "    22\n"),
    )?;
    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(1));
    let snapshot: hord_core::ObjectId = json["snapshot"].as_str().ok_or("snapshot id")?.parse()?;

    let store = hord_store::Store::open(&f.repo.0)?;
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
        signature: None,
    };
    store.put_evidence(&evidence(hord_core::EvidenceKind::Check, None, snapshot))?;
    // A full run meets `test:selected` (ADR 0026).
    store.put_evidence(&evidence(
        hord_core::EvidenceKind::Test,
        Some("full"),
        snapshot,
    ))?;
    // Evidence for another snapshot does not count.
    store.put_evidence(&evidence(
        hord_core::EvidenceKind::Lint,
        None,
        hord_core::ObjectId::from_canonical(b"other"),
    ))?;
    drop(store);

    let (code, json) = check(&f, &[])?;
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json["decision"], "allow");
    assert_eq!(
        json["evidence"],
        serde_json::json!([
            { "tag": "check", "result": "Pass" },
            { "tag": "test:full", "result": "Pass" },
        ])
    );
    Ok(())
}

/// ADR 0026 amendment: a workspace edit that breaks `.hord-policy.toml` is
/// reported by `policy check` (and refused by `propose`) with the parse
/// error's line and column; the lander would reject it.
#[test]
fn a_policy_edit_that_does_not_parse_is_reported_early() -> TestResult {
    let f = fixture(Some("[land]\nrequire = [\"check\"]\n"))?;
    fs::write(
        f.checkout.join(".hord-policy.toml"),
        "[land]\nrequire = [\"check\"]\nstrict_reads = 3\n",
    )?;
    let out = hord(&f.repo.0, &["policy", "check"])?;
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr)?;
    assert!(stderr.contains(".hord-policy.toml:3:16:"), "{stderr}");
    let intent = f.repo.0.join("intent.md");
    fs::write(&intent, "---\nsummary: break the policy\n---\n")?;
    let out = hord(
        &f.repo.0,
        &[
            "propose",
            "--intent",
            intent.to_str().ok_or("UTF-8 intent path")?,
        ],
    )?;
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr)?;
    assert!(stderr.contains(".hord-policy.toml:3:16:"), "{stderr}");
    Ok(())
}
