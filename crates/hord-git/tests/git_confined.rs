//! Git is confined to the git bridge (ADR 0039): only `hord-git`, and the
//! CLI's `hord git` commands that call it, spawn `git` or link a git
//! library. `bench/` is exempt (the corpora are mined from git history).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Git libraries, including the bridge crate that wraps one.
const GIT_LIBRARIES: [&str; 3] = ["gix", "git2", "hord-git"];

/// The CLI's source files for `hord git …` and `hord init --from-git`.
const CLI_BRIDGE_FILES: [&str; 3] = ["src/cmd/git.rs", "src/cmd/git_sync.rs", "src/git_bridge.rs"];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Lines of Rust source that spawn `git` or use a git library's paths.
fn source_uses(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| {
            line.contains("new(\"git\")")
                || line.contains("new(\"git.exe\")")
                || line.contains("gix::")
                || line.contains("git2::")
                || line.contains("hord_git::")
                || line.contains("use gix")
                || line.contains("use git2")
                || line.contains("use hord_git")
        })
        .map(|line| line.trim().to_owned())
        .collect()
}

/// Git libraries a `Cargo.toml` depends on, in any dependency table.
fn manifest_uses(text: &str) -> TestResult<Vec<String>> {
    let manifest: toml::Table = text.parse()?;
    let mut found = Vec::new();
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        tables.extend(manifest.get(key).and_then(toml::Value::as_table));
    }
    for target in manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|t| t.values())
        .filter_map(toml::Value::as_table)
    {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            tables.extend(target.get(key).and_then(toml::Value::as_table));
        }
    }
    for table in tables {
        for (name, spec) in table {
            let package = spec
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(name);
            if GIT_LIBRARIES.contains(&package) {
                found.push(package.to_owned());
            }
        }
    }
    Ok(found)
}

/// Every use of git in the crate at `dir`, except those `allowed_files`
/// (paths relative to the crate) may make.
fn violations(
    dir: &Path,
    allowed_files: &[&str],
    allow_bridge_dep: bool,
) -> TestResult<Vec<String>> {
    let mut out = Vec::new();
    let manifest = dir.join("Cargo.toml");
    for library in manifest_uses(&fs::read_to_string(&manifest)?)? {
        if !(allow_bridge_dep && library == "hord-git") {
            out.push(format!("{} depends on {library}", manifest.display()));
        }
    }
    let mut files = Vec::new();
    rust_files(&dir.join("src"), &mut files)?;
    for file in files {
        let relative = file.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        if allowed_files.contains(&relative.as_str()) {
            continue;
        }
        for line in source_uses(&fs::read_to_string(&file)?) {
            out.push(format!("{}: {line}", file.display()));
        }
    }
    Ok(out)
}

#[test]
fn only_the_bridge_uses_git() -> TestResult {
    let mut found = Vec::new();
    for entry in fs::read_dir(crates_dir())? {
        let dir = entry?.path();
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        match dir.file_name().and_then(|n| n.to_str()) {
            Some("hord-git") => {}
            Some("hord-cli") => found.extend(violations(&dir, &CLI_BRIDGE_FILES, true)?),
            _ => found.extend(violations(&dir, &[], false)?),
        }
    }
    assert!(
        found.is_empty(),
        "git outside the git bridge (ADR 0039):\n{}",
        found.join("\n")
    );
    Ok(())
}

#[test]
fn the_scan_sees_the_bridge_itself() -> TestResult {
    // Not vacuous: without its exemption, hord-git is flagged for both its
    // manifest and its sources.
    let found = violations(&crates_dir().join("hord-git"), &[], false)?;
    assert!(
        found.iter().any(|v| v.contains("depends on gix")),
        "{found:?}"
    );
    assert!(found.iter().any(|v| v.contains("gix::")), "{found:?}");
    assert_eq!(
        source_uses("let out = Command::new(\"git\").arg(\"status\");\n// Command::new(\"git\")"),
        ["let out = Command::new(\"git\").arg(\"status\");"]
    );
    Ok(())
}
