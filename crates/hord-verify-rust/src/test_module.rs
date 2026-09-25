//! The integration-test module a file compiles into (ADR 0022, R2).
//!
//! A non-function edit (glue, a declaration, an initializer, attributes) in
//! a file under a package's `tests/` directory can only change the tests
//! compiled from that file's module subtree, unless another module uses its
//! items. [`test_module`] maps the file to that module; selection then runs
//! the module's tests instead of the whole package, and keeps the package
//! fallback for a target's root file, for shared helper modules, and for a
//! module whose items are referenced from outside it.

use hord_core::RepoPath;

use crate::CargoWorkspace;

/// Module names that hold helpers shared by other test modules. A file in
/// such a module keeps the package fallback even without a known reference.
const SHARED_MODULES: [&str; 8] = [
    "utils", "util", "support", "common", "helpers", "helper", "fixtures", "prelude",
];

/// One module of an integration-test target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TestModule {
    pub package: String,
    /// The test target (binary) name.
    pub target: String,
    /// The module path inside the target, e.g. `cargo_add::add_basic`.
    pub module: String,
    /// The module's own file with `.rs` stripped, relative to the
    /// repository: its subtree is this path's directory and the file.
    stem: Vec<String>,
}

impl TestModule {
    /// Whether test `name` (a path inside the target) is in the module's
    /// subtree.
    pub(crate) fn contains_test(&self, name: &str) -> bool {
        name.strip_prefix(&self.module)
            .is_some_and(|rest| rest.starts_with("::"))
    }

    /// Whether `path` is one of the module subtree's files: the module's own
    /// file (`a/b.rs` or `a/b/mod.rs`) or anything under `a/b/`.
    pub(crate) fn contains_file(&self, path: &RepoPath) -> bool {
        let parts = path.components();
        if parts.starts_with(&self.stem) && parts.len() > self.stem.len() {
            return true;
        }
        let Some((last, dir)) = parts.split_last() else {
            return false;
        };
        let Some((stem_last, stem_dir)) = self.stem.split_last() else {
            return false;
        };
        dir == stem_dir && last.strip_suffix(".rs") == Some(stem_last.as_str())
    }

    /// The name filter for the module's tests (`cargo_add::add_basic::`).
    pub(crate) fn filter(&self) -> String {
        format!("{}::", self.module)
    }
}

/// The module of an integration-test target that `path` compiles into, or
/// `None` when the file is not a module of one, is a target's root file
/// (`tests/x.rs`, `tests/x/main.rs`: every test of the binary), or is a
/// shared helper module ([`SHARED_MODULES`]).
pub(crate) fn test_module(ws: &CargoWorkspace, path: &RepoPath) -> Option<TestModule> {
    let pkg = ws.package_of(path)?;
    let parts = path.components();
    let rel = parts.get(pkg.dir.components().len()..)?;
    if rel.first().map(String::as_str) != Some("tests") {
        return None;
    }
    let file = parts.last()?;
    let stem_name = file.strip_suffix(".rs")?;
    for target in &pkg.targets {
        if !target.kind.iter().any(|k| k == "test") {
            continue;
        }
        let src = target.src_path.components();
        if src == parts {
            return None;
        }
        let (src_file, src_dir) = src.split_last()?;
        // The directory the target's modules live in: next to `main.rs`,
        // or `tests/x/` for `tests/x.rs`.
        let mut root: Vec<String> = src_dir.to_vec();
        if src_file != "main.rs" {
            root.push(src_file.strip_suffix(".rs")?.to_owned());
        }
        let Some(inside) = parts.strip_prefix(root.as_slice()) else {
            continue;
        };
        let (_, dirs) = inside.split_last()?;
        let mut module: Vec<String> = dirs.to_vec();
        if stem_name != "mod" {
            module.push(stem_name.to_owned());
        }
        if module.is_empty() || module.iter().any(|m| SHARED_MODULES.contains(&m.as_str())) {
            return None;
        }
        let stem: Vec<String> = root.iter().chain(&module).cloned().collect();
        return Some(TestModule {
            package: pkg.name.clone(),
            target: target.name.clone(),
            module: module.join("::"),
            stem,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::cargo::tests::sample;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_str(s).expect("parse test path")
    }

    #[test]
    fn files_map_to_their_module_of_the_test_target() {
        let ws = sample();
        let m = test_module(&ws, &p("tests/testsuite/cargo_add/add_basic/mod.rs"))
            .expect("a module of testsuite");
        assert_eq!(
            (m.package.as_str(), m.target.as_str(), m.module.as_str()),
            ("a", "testsuite", "cargo_add::add_basic")
        );
        assert!(m.contains_test("cargo_add::add_basic::case"));
        assert!(!m.contains_test("cargo_add::add_basic_other::case"));
        assert!(m.contains_file(&p("tests/testsuite/cargo_add/add_basic/mod.rs")));
        assert!(m.contains_file(&p("tests/testsuite/cargo_add/add_basic/inner.rs")));
        assert!(!m.contains_file(&p("tests/testsuite/cargo_add/mod.rs")));
        assert_eq!(m.filter(), "cargo_add::add_basic::");
        let flat = test_module(&ws, &p("tests/testsuite/build.rs")).expect("a module");
        assert_eq!(flat.module, "build");
        assert!(flat.contains_file(&p("tests/testsuite/build.rs")));
        assert!(flat.contains_file(&p("tests/testsuite/build/nested.rs")));
        assert!(!flat.contains_file(&p("tests/testsuite/build_script.rs")));
    }

    #[test]
    fn roots_helpers_and_other_files_keep_the_package_fallback() {
        let ws = sample();
        for path in [
            "tests/testsuite/main.rs",
            "tests/testsuite/utils/mod.rs",
            "tests/testsuite/utils/ext.rs",
            "tests/testsuite/support.rs",
            "tests/testsuite/lints/common.rs",
            "src/lib.rs",
            "crates/b/tests/x.rs",
            "tests/testsuite/data.toml",
        ] {
            assert_eq!(test_module(&ws, &p(path)), None, "{path}");
        }
    }
}
