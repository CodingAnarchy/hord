//! The CI gate harness flags on a tiny synthetic corpus (ADR 0023
//! amendment): two era chains with `--chain i/2`, several faults per
//! informative commit with `--faults-per-commit`, the literal full-suite
//! sample with `--sample-only`, and `--merge` into one report and verdict.
//!
//! Skipped with a message when `cargo llvm-cov` is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MANIFEST: &str =
    "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

/// The library at one commit: three functions, each with its own test.
fn lib(add: &str, small: &str, double: &str) -> String {
    format!(
        "pub fn add(a: i32, b: i32) -> i32 {{\n    {add}\n}}\n\n\
         pub fn is_small(x: i32) -> bool {{\n    {small}\n}}\n\n\
         pub fn double(x: i32) -> i32 {{\n    {double}\n}}\n\n\
         #[cfg(test)]\nmod tests {{\n    #[test]\n    fn adds() {{\n        assert_eq!(super::add(2, 3), 5);\n    }}\n\n    \
         #[test]\n    fn small() {{\n        assert!(super::is_small(3));\n        assert!(!super::is_small(30));\n    }}\n\n    \
         #[test]\n    fn doubles() {{\n        assert_eq!(super::double(4), 8);\n    }}\n}}\n"
    )
}

fn run(cmd: &mut Command) -> Result<String, Box<dyn std::error::Error>> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    run(Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir))
}

/// A linear history of five commits, each editing one function body without
/// changing behavior, cloned bare to `<root>/corpora/cargo.git`.
fn corpus(root: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let src = root.join("src-repo");
    fs::create_dir_all(src.join("src"))?;
    fs::write(src.join("Cargo.toml"), MANIFEST)?;
    fs::write(src.join("src/lib.rs"), lib("a + b", "x < 10", "x * 2"))?;
    run(Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&src))?;
    git(&src, &["init", "-q"])?;
    let versions = [
        ("a + b", "x < 10", "x * 2"),
        ("b + a", "x < 10", "x * 2"),
        ("b + a", "10 > x", "x * 2"),
        ("b + a", "10 > x", "x + x"),
        ("a + b", "10 > x", "x + x"),
    ];
    for (i, (add, small, double)) in versions.iter().enumerate() {
        fs::write(src.join("src/lib.rs"), lib(add, small, double))?;
        git(&src, &["add", "-A"])?;
        git(&src, &["commit", "-q", "-m", &format!("c{i}")])?;
    }
    let corpora = root.join("corpora");
    fs::create_dir_all(&corpora)?;
    git(
        &corpora,
        &["clone", "-q", "--bare", &src.to_string_lossy(), "cargo.git"],
    )?;
    Ok(corpora)
}

fn llvm_cov_installed() -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn chains_faults_sample_and_merge() -> TestResult {
    if !llvm_cov_installed() {
        eprintln!("skipping: cargo-llvm-cov is not installed");
        return Ok(());
    }
    let root = std::env::temp_dir().join(format!("hord-m4-chains-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let corpora = corpus(&root)?;
    let bin = env!("CARGO_BIN_EXE_hord-eval-m4");
    let common = |work: &Path| {
        let mut cmd = Command::new(bin);
        cmd.arg("--cache")
            .arg(&corpora)
            .arg("--work")
            .arg(work)
            .args([
                "--commits-per-chain",
                "2",
                "--chain-stride",
                "2",
                "--jobs",
                "1",
            ])
            .args([
                "--coverage-jobs",
                "2",
                "--command-timeout-mins",
                "10",
                "--seed",
                "1",
            ])
            .current_dir(&root);
        cmd
    };
    // Two era chains: chain 0 is commits c3, c4; chain 1 is c1, c2.
    let dirs = [root.join("w0"), root.join("w1")];
    for (i, dir) in dirs.iter().enumerate() {
        run(common(dir).args(["--chain", &format!("{i}/2")]).args([
            "--faults-per-commit",
            "2",
            "--full-sample",
            "0",
        ]))?;
    }
    // The literal sample, over every chain window, as its own job.
    let sample = root.join("sample");
    run(common(&sample).args(["--chain", "0/2", "--sample-only", "--full-sample", "1"]))?;
    // Merge everything into one report.
    let json = root.join("report.json");
    let md = root.join("report.md");
    // The run is complete and the efficiency gate fails on a three-test
    // suite, so the merge exits 1: the verdict is the exit status.
    let merged = Command::new(bin)
        .arg("--merge")
        .args(&dirs)
        .arg(&sample)
        .arg("--json")
        .arg(&json)
        .arg("--summary")
        .arg(&md)
        .current_dir(&root)
        .output()?;
    assert_eq!(
        merged.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&fs::read(&json)?)?;
    let chains = report["chains"].as_array().ok_or("report has chains")?;
    assert_eq!(chains.len(), 2);
    assert_eq!(report["commits"], 4);
    assert_eq!(report["chained"], 4);
    assert_eq!(report["complete"], true, "{report:#}");
    // Each commit edits one function whose test alone covers it, so (b) does
    // not contain (a) and two faults are graded on it; none is missed.
    let counts = &report["counts"];
    assert_eq!(counts["informative_commits"], 4, "{counts:#}");
    assert_eq!(counts["faults"], 8, "{counts:#}");
    assert_eq!(counts["fault_misses"], 0, "{counts:#}");
    assert_eq!(counts["faults_detected_b"], 8, "{counts:#}");
    assert_eq!(report["safety_gate"], true);
    assert_eq!(
        report["efficiency_gate"], false,
        "a third of a tiny suite is over 20%"
    );
    let chain_names: Vec<&str> = chains.iter().filter_map(|c| c["chain"].as_str()).collect();
    assert_eq!(chain_names, vec!["0/2", "1/2"]);
    assert_eq!(report["sample"].as_array().map(Vec::len), Some(1));
    let summary = fs::read_to_string(&md)?;
    assert!(summary.contains("## Chains"), "{summary}");
    // Disk telemetry: every chain sampled `/` and its work dir at the start,
    // after the initial run and after each lander step and grade.
    for chain in chains {
        let disk = &chain["disk"];
        assert!(disk["samples"].as_u64().is_some_and(|n| n >= 6), "{disk:#}");
        assert!(
            disk["peaks"][0]["peak_used"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{disk:#}"
        );
    }
    assert!(summary.contains("Peak disk used"), "{summary}");
    // Cargo's per-test scratch trees are pruned after each step.
    for dir in &dirs {
        for slot in ["coverage-target-0", "coverage-target-1", "w2/target"] {
            assert!(!dir.join(slot).join("tmp/cit").exists(), "{slot}");
        }
        assert!(
            !dir.join("coverage-target").exists(),
            "the initial run builds in slot 0"
        );
    }
    // Each chain's window is the era its index names.
    let meta1: serde_json::Value =
        serde_json::from_slice(&fs::read(dirs[1].join("chain/meta.json"))?)?;
    let meta0: serde_json::Value =
        serde_json::from_slice(&fs::read(dirs[0].join("chain/meta.json"))?)?;
    let head = git(
        &corpora.join("cargo.git"),
        &["rev-list", "--first-parent", "HEAD"],
    )?;
    let history: Vec<&str> = head.lines().collect();
    assert_eq!(
        meta0["commits"],
        serde_json::json!([history[1], history[0]])
    );
    assert_eq!(
        meta1["commits"],
        serde_json::json!([history[3], history[2]])
    );
    // Nothing wrote a raw profile into the working directory.
    assert_eq!(report["profraw_in_cwd"], serde_json::json!([]));
    let _ = fs::remove_dir_all(&root);
    Ok(())
}
