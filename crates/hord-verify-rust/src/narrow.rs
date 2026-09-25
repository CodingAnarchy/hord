//! The narrowed non-Rust fallback (ADR 0022, amendments after the 50-commit
//! measurement): a changed file with no adapter can affect a package's
//! tests only if the package can read it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use hord_core::RepoPath;

use crate::CargoWorkspace;

/// Whether `path`, a changed file with no adapter in package `package` of
/// `ws`, triggers that package's fallback: it lies inside a build target's
/// source directory (a directory holding a target's root file, other than
/// the package root and the build script's), or its path is named by suffix
/// in a string literal of the package's Rust sources (which covers
/// `include_str!`, `include_bytes!`, and `file!`).
///
/// Without a checkout to read ([`CargoWorkspace::root`] is `None`) or when
/// a source cannot be read, it answers `true`: the conservative fallback.
/// `literals` caches each package's string literals across calls.
#[must_use]
pub fn package_reads(
    ws: &CargoWorkspace,
    package: &str,
    path: &RepoPath,
    literals: &mut BTreeMap<String, Option<Vec<String>>>,
) -> bool {
    let Some(pkg) = ws.packages.get(package) else {
        return true;
    };
    let in_sources = pkg.targets.iter().any(|t| {
        if t.kind.iter().any(|k| k == "custom-build") {
            return false;
        }
        let mut dir = t.src_path.components().to_vec();
        dir.pop();
        dir.len() > pkg.dir.components().len() && path.components().starts_with(&dir)
    });
    if in_sources {
        return true;
    }
    let Some(root) = &ws.root else {
        return true;
    };
    let lits = literals
        .entry(package.to_owned())
        .or_insert_with(|| package_literals(ws, root, package));
    match lits {
        None => true,
        Some(lits) => lits.iter().any(|l| names_path(l, path)),
    }
}

/// Every string literal in the Rust files of `package` (not descending into
/// nested packages, `target/`, or hidden directories). `None` if a file
/// could not be read.
fn package_literals(
    ws: &CargoWorkspace,
    root: &std::path::Path,
    package: &str,
) -> Option<Vec<String>> {
    let pkg = ws.packages.get(package)?;
    let base = root.join(pkg.dir.to_string());
    let nested: Vec<PathBuf> = ws
        .packages
        .values()
        .filter(|p| p.dir != pkg.dir && p.dir.components().starts_with(pkg.dir.components()))
        .map(|p| root.join(p.dir.to_string()))
        .collect();
    let mut all = Vec::new();
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if name != "target" && !name.starts_with('.') && !nested.contains(&p) {
                    stack.push(p);
                }
            } else if name.ends_with(".rs") {
                let text = std::fs::read_to_string(&p).ok()?;
                all.extend(string_literals(&text).into_iter().map(str::to_owned));
            }
        }
    }
    Some(all)
}

/// String literal contents of Rust source, escapes left as written.
/// Comments are skipped; char literals and lifetimes are not mistaken for
/// strings.
#[must_use]
pub fn string_literals(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\'' => {
                if bytes.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'\\') {
                    i += 4;
                } else {
                    i += 1;
                }
            }
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if let Some(lit) = text.get(start..i.min(bytes.len())) {
                    out.push(lit);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether `literal` names `path` by suffix: its `/`-separated components,
/// without leading `.` and `..`, end `path` (at least the file name).
#[must_use]
pub fn names_path(literal: &str, path: &RepoPath) -> bool {
    let parts: Vec<&str> = literal
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    if parts.is_empty() || parts.len() > path.components().len() {
        return false;
    }
    let tail = &path.components()[path.components().len() - parts.len()..];
    tail.iter().zip(&parts).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_path_suffixes() {
        assert_eq!(
            string_literals(r#"a("x/y.md", 'c', "z\"q") // "no""#),
            vec!["x/y.md", r#"z\"q"#]
        );
        let p: RepoPath = "tests/testsuite/foo/stderr.term.svg"
            .parse()
            .expect("parse `tests/testsuite/foo/stderr.term.svg`");
        assert!(names_path("stderr.term.svg", &p));
        assert!(names_path("../foo/stderr.term.svg", &p));
        assert!(!names_path("bar/stderr.term.svg", &p));
        assert!(!names_path("./", &p));
    }

    #[test]
    fn reads_sources_and_named_files_only() {
        let root =
            std::env::temp_dir().join(format!("hord-verify-rust-narrow-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("create dir all");
        std::fs::write(
            root.join("src/lib.rs"),
            "const R: &str = include_str!(\"../docs/used.md\"); // \"docs/comment.md\"\n",
        )
        .expect("write a fixture file");
        let mut ws = crate::cargo::tests::sample();
        ws.root = Some(root.clone());
        let mut cache = BTreeMap::new();
        let mut reads =
            |p: &str| package_reads(&ws, "a", &p.parse().expect("parse test path"), &mut cache);
        assert!(reads("src/data.json"), "inside the lib's source dir");
        assert!(
            reads("tests/testsuite/fixture.svg"),
            "inside a test target's dir"
        );
        assert!(reads("docs/used.md"), "named by include_str!");
        assert!(!reads("docs/comment.md"), "named only in a comment");
        assert!(!reads("triagebot.toml"));
        let _ = std::fs::remove_dir_all(&root);
        // No checkout: conservative.
        let ws = crate::cargo::tests::sample();
        assert!(package_reads(
            &ws,
            "a",
            &"triagebot.toml".parse().expect("parse `triagebot.toml`"),
            &mut BTreeMap::new()
        ));
    }
}
