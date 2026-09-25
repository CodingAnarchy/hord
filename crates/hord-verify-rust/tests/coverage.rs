//! Per-test coverage on a real (tiny) cargo workspace, and test selection
//! and evidence reuse on top of it (ADR 0022, ADR 0025).
//!
//! Skipped with a message when `cargo llvm-cov` is not installed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hord_core::{NodeId, NodeKind, ObjectId, RepoPath};
use hord_verify::{
    ChangeFacts, Checkout, Definition, EvidenceIndex, MemoryIndex, VerifyPolicy, find_coverage,
    impact_set, verify,
};
use hord_verify_rust::coverage::{collect, record_evidence};
use hord_verify_rust::{
    CargoWorkspace, CoverageOptions, DefinitionIndex, RustVerifier, detect_toolchain,
};

const LIB: &str = "pub fn double(x: u32) -> u32 {\n    x * 2\n}\n\npub fn triple(x: u32) -> u32 {\n    x * 3\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn doubles() {\n        assert_eq!(super::double(2), 4);\n    }\n}\n";
const MAIN: &str = "fn shout() -> String {\n    format!(\"{}\", fixture::triple(3))\n}\n\nfn main() {\n    println!(\"{}\", shout());\n}\n";
const TEST: &str = "#[test]\nfn runs_the_binary() {\n    let out = std::process::Command::new(env!(\"CARGO_BIN_EXE_fixture\")).output().expect(\"run the fixture binary\");\n    assert_eq!(String::from_utf8(out.stdout).expect(\"output is UTF-8\").trim(), \"9\");\n}\n";

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> Dir {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "hord-verify-rust-cov-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(root.join("src")).expect("create dir all");
    fs::create_dir_all(root.join("tests")).expect("create dir all");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("write a fixture file");
    fs::write(root.join("src/lib.rs"), LIB).expect("write a fixture file");
    fs::write(root.join("src/main.rs"), MAIN).expect("write a fixture file");
    fs::write(root.join("tests/cli.rs"), TEST).expect("write a fixture file");
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .status()
        .expect("run cargo generate-lockfile");
    assert!(lock.success());
    Dir(root)
}

/// Definitions of the fixture, found by text (each `fn` item to its
/// closing brace at the same indentation).
fn definitions(root: &Path) -> (DefinitionIndex, Vec<(String, NodeId)>) {
    let mut index = DefinitionIndex::new();
    let mut named = Vec::new();
    let mut next = 1u128;
    for file in ["src/lib.rs", "src/main.rs", "tests/cli.rs"] {
        let text = fs::read_to_string(root.join(file)).expect("read to string");
        let path = RepoPath::from_str(file).expect("parse test path");
        let mut defs = Vec::new();
        let mut at = 0;
        while let Some(off) = text[at..].find("fn ") {
            let start = at + off;
            let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
            let line = &text[line_start..start];
            let indent = &line[..line.len() - line.trim_start().len()];
            let close = format!("\n{indent}}}");
            let end = text[start..]
                .find(&close)
                .expect("each fn item in the fixture has a closing brace")
                + start
                + close.len();
            let name = text[start + 3..]
                .split('(')
                .next()
                .expect("split yields at least one piece")
                .to_owned();
            let node = NodeId::from_u128(next);
            next += 1;
            named.push((name.clone(), node));
            defs.push(Definition {
                node,
                path: path.clone(),
                kind: NodeKind::new("function_item"),
                name: None,
                span: line_start..end,
                parent: None,
            });
            at = start + 3;
        }
        index.add_file(&path, text.as_bytes(), &defs);
    }
    (index, named)
}

fn llvm_cov_installed() -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn coverage_follows_subprocesses_and_drives_selection() -> Result<(), String> {
    if !llvm_cov_installed() {
        eprintln!("skipping: cargo-llvm-cov is not installed");
        return Ok(());
    }
    let dir = fixture();
    let root = dir.0.clone();
    let (defs, named) = definitions(&root);
    let node = |name: &str| {
        named
            .iter()
            .find(|(n, _)| n == name)
            .expect("the fixture defines this function")
            .1
    };
    let toolchain = detect_toolchain(&root).expect("detect toolchain");
    let snapshot = ObjectId::from_bytes([3; 32]);
    let checkout = Checkout {
        root: root.clone(),
        snapshot,
    };
    let run = collect(
        &checkout,
        &toolchain,
        &defs,
        &CoverageOptions {
            packages: None,
            target_dir: root.join("target-cov"),
            jobs: 2,
            test_timeout: Duration::from_secs(120),
            skip: BTreeSet::new(),
            only: None,
            // `x * 2` (double) and `x * 3` (triple).
            lines_of_interest: [(
                RepoPath::from_str("src/lib.rs").expect("parse test path"),
                [2, 6].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
        },
    )
    .expect("collect coverage on the fixture");
    let record = &run.record;
    let by_name = |name: &str| {
        record
            .tests
            .iter()
            .find(|t| t.test.name == name)
            .ok_or_else(|| format!("no test {name} in the record: {:?}", record.tests))
    };
    // The unit test ran `double` in-process.
    let unit = by_name("tests::doubles")?;
    let unit_covers: BTreeSet<NodeId> = record.covered_by(unit).collect();
    assert!(unit_covers.contains(&node("double")), "{unit_covers:?}");
    assert!(!unit_covers.contains(&node("triple")));
    assert_eq!(unit.node, Some(node("doubles")));
    assert_eq!(unit.test.target.kind, "lib");
    // The integration test ran the binary as a subprocess: `main`, `shout`,
    // and the library's `triple` are attributed to it.
    let cli = by_name("runs_the_binary")?;
    let cli_covers: BTreeSet<NodeId> = record.covered_by(cli).collect();
    for f in ["runs_the_binary", "main", "shout", "triple"] {
        assert!(
            cli_covers.contains(&node(f)),
            "{f} missing from {cli_covers:?}"
        );
    }
    assert!(!cli_covers.contains(&node("double")));
    assert_eq!(cli.test.target.kind, "test");
    assert!(record.is_instrumented(node("triple")));
    assert!(!cli.failed && !unit.failed);
    // Region data: each test executed only its function's body line.
    let lib = RepoPath::from_str("src/lib.rs").expect("parse test path");
    let lines_of = |t: &hord_verify::TestRef| run.lines.get(t).and_then(|l| l.get(&lib)).cloned();
    assert_eq!(lines_of(&unit.test), Some([2].into_iter().collect()));
    assert_eq!(lines_of(&cli.test), Some([6].into_iter().collect()));

    // An instrumented verification run of one selected test refreshes only
    // that test (ADR 0022: coverage is fresh per test).
    let later = ObjectId::from_bytes([4; 32]);
    let refresh = collect(
        &Checkout {
            root: root.clone(),
            snapshot: later,
        },
        &toolchain,
        &defs,
        &CoverageOptions {
            packages: None,
            target_dir: root.join("target-cov"),
            jobs: 2,
            test_timeout: Duration::from_secs(120),
            skip: BTreeSet::new(),
            only: Some(hord_verify_rust::TestFilter {
                tests: [cli.test.clone()].into_iter().collect(),
                ..Default::default()
            }),
            lines_of_interest: Default::default(),
        },
    )
    .expect("collect coverage on the fixture");
    assert_eq!(refresh.record.tests.len(), 1);
    let ledger = record.merge(&refresh.record);
    let snap_of = |name: &str| {
        let t = ledger
            .tests
            .iter()
            .find(|t| t.test.name == name)
            .expect("the ledger has this test");
        ledger.test_snapshot(t)
    };
    // The refreshed test's record unites both runs and dates from the
    // older one (ADR 0022: the union of a test's last three runs).
    assert_eq!(snap_of("runs_the_binary"), snapshot);
    let cli_entry = ledger
        .tests
        .iter()
        .find(|t| t.test.name == "runs_the_binary")
        .expect("the ledger has this test");
    assert_eq!(cli_entry.runs.len(), 2);
    assert_eq!(cli_entry.runs[1].snapshot, later);
    assert_eq!(snap_of("tests::doubles"), snapshot);
    let single = record.merge_keeping(&refresh.record, 1);
    let t = single
        .tests
        .iter()
        .find(|t| t.test.name == "runs_the_binary")
        .expect("the ledger has this test");
    assert_eq!(single.test_snapshot(t), later);
    // An empty selection builds and runs nothing.
    let none = collect(
        &Checkout {
            root: root.clone(),
            snapshot: later,
        },
        &toolchain,
        &defs,
        &CoverageOptions {
            packages: None,
            target_dir: root.join("target-cov"),
            jobs: 2,
            test_timeout: Duration::from_secs(120),
            skip: BTreeSet::new(),
            only: Some(hord_verify_rust::TestFilter::default()),
            lines_of_interest: Default::default(),
        },
    )
    .expect("collect coverage on the fixture");
    assert!(none.record.tests.is_empty());

    // Store it as evidence, find it again, and select with it.
    let index = MemoryIndex::new();
    record_evidence(&run, &index, hord_core::Actor::Human { id: "t".into() })
        .expect("record evidence");
    let (_, found) = find_coverage(
        &index,
        [snapshot],
        toolchain.id().expect("compute toolchain id"),
    )
    .expect("find coverage")
    .expect("the refreshed test is in the record");
    assert_eq!(&found, record);

    let workspace = CargoWorkspace::load(&root).expect("cargo metadata of the fixture");
    let verifier = RustVerifier::new(toolchain, workspace)
        .expect("build a verifier")
        .with_coverage(Some(Arc::new(found)));
    struct NoGraph;
    impl hord_verify::ReferenceGraph for NoGraph {
        fn dependents(&self, _: NodeId) -> hord_verify::Result<Vec<NodeId>> {
            Ok(Vec::new())
        }
        fn package(&self, _: NodeId) -> Option<String> {
            None
        }
    }
    let mut facts = ChangeFacts::default();
    let lib = RepoPath::from_str("src/lib.rs").expect("parse test path");
    let text = fs::read(root.join("src/lib.rs")).expect("read the fixture lib");
    let span_of = |src: &str| {
        let start = src
            .find("pub fn triple")
            .expect("the fixture defines triple");
        start
            ..start
                + src[start..]
                    .find("\n}")
                    .expect("triple has a closing brace")
                + 2
    };
    let original = String::from_utf8(text.clone()).expect("output is UTF-8");
    let lib_defs: Vec<Definition> = vec![Definition {
        node: node("triple"),
        path: lib.clone(),
        kind: NodeKind::new("function_item"),
        name: None,
        span: span_of(&original),
        parent: None,
    }];
    let changed = original.replace("x * 3", "x * 3 + 0");
    hord_verify_rust::diff_rust_file(
        &mut facts,
        &lib,
        Some(hord_verify::FileVersion {
            bytes: &text,
            defs: &lib_defs,
        }),
        Some(hord_verify::FileVersion {
            bytes: changed.as_bytes(),
            defs: &[Definition {
                span: span_of(&changed),
                ..lib_defs[0].clone()
            }],
        }),
    );
    let write: BTreeSet<NodeId> = [node("triple")].into_iter().collect();
    let impact = impact_set(&NoGraph, &write, Default::default(), facts).expect("impact set");
    let policy = VerifyPolicy::default().requiring(["test:selected"]);
    let selection = verifier.selection(&impact, &policy);
    assert!(selection.fallbacks.is_empty(), "{:?}", selection.fallbacks);
    let exact: BTreeSet<&String> = selection.exact.values().flatten().collect();
    assert_eq!(
        exact.into_iter().cloned().collect::<Vec<_>>(),
        vec!["runs_the_binary"]
    );

    // The instrumented verification run: the selection runs under coverage,
    // passes, and returns fresh coverage for exactly the selected test.
    let inst = verifier
        .run_instrumented(
            &checkout,
            &impact,
            &policy,
            &defs,
            &CoverageOptions {
                packages: None,
                target_dir: root.join("target-cov"),
                jobs: 2,
                test_timeout: Duration::from_secs(120),
                skip: BTreeSet::new(),
                only: None,
                lines_of_interest: Default::default(),
            },
            &index,
        )
        .expect("run instrumented");
    assert_eq!(inst.evidence[0].result, hord_core::EvidenceResult::Pass);
    assert_eq!(inst.evidence[0].qualifier.as_deref(), Some("selected"));
    let names: Vec<&str> = inst
        .coverage
        .record
        .tests
        .iter()
        .map(|t| t.test.name.as_str())
        .collect();
    assert_eq!(names, vec!["runs_the_binary"]);

    // Verify runs the selection once; the same snapshot again reuses it.
    let verdict = verify(&verifier, &index, &checkout, &impact, &policy).expect("verify");
    assert!(verdict.passed(), "{verdict:?}");
    // ADR 0026: selected runs are qualified `selected`.
    for id in verdict.evidence() {
        let ev = hord_verify::get_evidence(&index, *id).expect("get evidence");
        assert_eq!(ev.qualifier.as_deref(), Some("selected"), "{}", ev.command);
    }
    let before = index.evidence_at(snapshot).expect("evidence at").len();
    let again = verify(&verifier, &index, &checkout, &impact, &policy).expect("verify");
    assert_eq!(again.evidence().len(), verdict.evidence().len());
    assert_eq!(
        index.evidence_at(snapshot).expect("evidence at").len(),
        before
    );
    Ok(())
}
