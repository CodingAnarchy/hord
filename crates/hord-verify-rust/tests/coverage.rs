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
const TEST: &str = "#[test]\nfn runs_the_binary() {\n    let out = std::process::Command::new(env!(\"CARGO_BIN_EXE_fixture\")).output().unwrap();\n    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), \"9\");\n}\n";

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
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), LIB).unwrap();
    fs::write(root.join("src/main.rs"), MAIN).unwrap();
    fs::write(root.join("tests/cli.rs"), TEST).unwrap();
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .status()
        .unwrap();
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
        let text = fs::read_to_string(root.join(file)).unwrap();
        let path = RepoPath::from_str(file).unwrap();
        let mut defs = Vec::new();
        let mut at = 0;
        while let Some(off) = text[at..].find("fn ") {
            let start = at + off;
            let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
            let line = &text[line_start..start];
            let indent = &line[..line.len() - line.trim_start().len()];
            let close = format!("\n{indent}}}");
            let end = text[start..].find(&close).unwrap() + start + close.len();
            let name = text[start + 3..].split('(').next().unwrap().to_owned();
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
fn coverage_follows_subprocesses_and_drives_selection() {
    if !llvm_cov_installed() {
        eprintln!("skipping: cargo-llvm-cov is not installed");
        return;
    }
    let dir = fixture();
    let root = dir.0.clone();
    let (defs, named) = definitions(&root);
    let node = |name: &str| named.iter().find(|(n, _)| n == name).unwrap().1;
    let toolchain = detect_toolchain(&root).unwrap();
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
        },
    )
    .unwrap();
    let record = &run.record;
    let by_name = |name: &str| {
        record
            .tests
            .iter()
            .find(|t| t.test.name == name)
            .unwrap_or_else(|| panic!("no test {name}: {:?}", record.tests))
    };
    // The unit test ran `double` in-process.
    let unit = by_name("tests::doubles");
    let unit_covers: BTreeSet<NodeId> = record.covered_by(unit).collect();
    assert!(unit_covers.contains(&node("double")), "{unit_covers:?}");
    assert!(!unit_covers.contains(&node("triple")));
    assert_eq!(unit.node, Some(node("doubles")));
    assert_eq!(unit.test.target.kind, "lib");
    // The integration test ran the binary as a subprocess: `main`, `shout`,
    // and the library's `triple` are attributed to it.
    let cli = by_name("runs_the_binary");
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

    // Store it as evidence, find it again, and select with it.
    let index = MemoryIndex::new();
    record_evidence(&run, &index, hord_core::Actor::Human { id: "t".into() }).unwrap();
    let (_, found) = find_coverage(&index, [snapshot], toolchain.id().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(&found, record);

    let workspace = CargoWorkspace::load(&root).unwrap();
    let verifier = RustVerifier::new(toolchain, workspace)
        .unwrap()
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
    let lib = RepoPath::from_str("src/lib.rs").unwrap();
    let text = fs::read(root.join("src/lib.rs")).unwrap();
    let span_of = |src: &str| {
        let start = src.find("pub fn triple").unwrap();
        start..start + src[start..].find("\n}").unwrap() + 2
    };
    let original = String::from_utf8(text.clone()).unwrap();
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
    let impact = impact_set(&NoGraph, &write, Default::default(), facts).unwrap();
    let policy = VerifyPolicy::default().requiring(["test:selected"]);
    let selection = verifier.selection(&impact, &policy);
    assert!(selection.fallbacks.is_empty(), "{:?}", selection.fallbacks);
    let exact: BTreeSet<&String> = selection.exact.values().flatten().collect();
    assert_eq!(
        exact.into_iter().cloned().collect::<Vec<_>>(),
        vec!["runs_the_binary"]
    );

    // Verify runs the selection once; the same snapshot again reuses it.
    let verdict = verify(&verifier, &index, &checkout, &impact, &policy).unwrap();
    assert!(verdict.passed(), "{verdict:?}");
    // ADR 0026: selected runs are qualified `selected`.
    for id in verdict.evidence() {
        let ev = hord_verify::get_evidence(&index, *id).unwrap();
        assert_eq!(ev.qualifier.as_deref(), Some("selected"), "{}", ev.command);
    }
    let before = index.evidence_at(snapshot).unwrap().len();
    let again = verify(&verifier, &index, &checkout, &impact, &policy).unwrap();
    assert_eq!(again.evidence().len(), verdict.evidence().len());
    assert_eq!(index.evidence_at(snapshot).unwrap().len(), before);
}
