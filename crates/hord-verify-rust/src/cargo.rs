//! The cargo workspace of a checkout (`cargo metadata --no-deps`): packages,
//! their directories and targets, and the workspace-internal dependency
//! graph that package-level fallbacks follow.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use hord_core::RepoPath;
use hord_verify::{Error, Result};
use serde::{Deserialize, Serialize};

/// One build target of a package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// Target name.
    pub name: String,
    /// Cargo kinds, e.g. `["lib"]`, `["bin"]`, `["test"]`, `["custom-build"]`.
    pub kind: Vec<String>,
    /// Source root, relative to the workspace root.
    pub src_path: RepoPath,
    /// `cargo test` runs its tests.
    pub test: bool,
    /// `cargo test` runs its doctests.
    pub doctest: bool,
}

/// One workspace member.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Package {
    /// Package name.
    pub name: String,
    /// Directory of its `Cargo.toml`, relative to the workspace root.
    pub dir: RepoPath,
    /// Its targets.
    pub targets: Vec<Target>,
    /// Workspace members it depends on (any dependency kind).
    pub deps: BTreeSet<String>,
}

impl Package {
    /// Whether it has a library with doctests.
    #[must_use]
    pub fn has_doctests(&self) -> bool {
        self.targets.iter().any(|t| t.doctest)
    }
}

/// The members of a cargo workspace.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CargoWorkspace {
    /// Members by name.
    pub packages: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
    workspace_root: String,
}

#[derive(Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
    manifest_path: String,
    targets: Vec<MetaTarget>,
    dependencies: Vec<MetaDep>,
}

#[derive(Deserialize)]
struct MetaTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
    #[serde(default = "yes")]
    test: bool,
    #[serde(default = "yes")]
    doctest: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct MetaDep {
    name: String,
    #[serde(default)]
    path: Option<String>,
}

impl CargoWorkspace {
    /// Read the workspace at `root` with `cargo metadata --no-deps`.
    pub fn load(root: &Path) -> Result<Self> {
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--offline",
            ])
            .current_dir(root)
            .output()?;
        if !output.status.success() {
            return Err(Error::tool(
                "cargo metadata",
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Self::from_metadata_json(&output.stdout)
    }

    /// Parse `cargo metadata --format-version 1` output.
    pub fn from_metadata_json(json: &[u8]) -> Result<Self> {
        let meta: Metadata = serde_json::from_slice(json)
            .map_err(|e| Error::tool("cargo metadata", e.to_string()))?;
        let root = Path::new(&meta.workspace_root);
        let rel = |p: &str| -> RepoPath {
            let path = Path::new(p);
            let rel = path.strip_prefix(root).unwrap_or(path);
            let text = rel.to_string_lossy().replace('\\', "/");
            RepoPath::from_str(&text).unwrap_or_default()
        };
        let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
        let names: BTreeSet<String> = meta
            .packages
            .iter()
            .filter(|p| members.contains(p.id.as_str()))
            .map(|p| p.name.clone())
            .collect();
        let mut packages = BTreeMap::new();
        for p in &meta.packages {
            if !members.contains(p.id.as_str()) {
                continue;
            }
            let manifest = rel(&p.manifest_path);
            let mut dir = manifest.components().to_vec();
            dir.pop();
            packages.insert(
                p.name.clone(),
                Package {
                    name: p.name.clone(),
                    dir: RepoPath::new(dir),
                    targets: p
                        .targets
                        .iter()
                        .map(|t| Target {
                            name: t.name.clone(),
                            kind: t.kind.clone(),
                            src_path: rel(&t.src_path),
                            test: t.test,
                            doctest: t.doctest && t.kind.iter().any(|k| k.ends_with("lib")),
                        })
                        .collect(),
                    deps: p
                        .dependencies
                        .iter()
                        .filter(|d| d.path.is_some() && names.contains(&d.name))
                        .map(|d| d.name.clone())
                        .collect(),
                },
            );
        }
        Ok(Self { packages })
    }

    /// The package whose directory is the deepest one containing `path`.
    #[must_use]
    pub fn package_of(&self, path: &RepoPath) -> Option<&Package> {
        self.packages
            .values()
            .filter(|p| path.components().starts_with(p.dir.components()))
            .max_by_key(|p| p.dir.components().len())
    }

    /// Whether `path` is the workspace root's own `Cargo.toml`,
    /// `Cargo.lock`, `.cargo/` config, or toolchain file: files that
    /// change how every member builds.
    #[must_use]
    pub fn is_workspace_wide(&self, path: &RepoPath) -> bool {
        let parts = path.components();
        match parts {
            [one] => matches!(
                one.as_str(),
                "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
            ),
            [dir, ..] => dir == ".cargo",
            [] => false,
        }
    }

    /// `names` and every member that depends on one of them, transitively.
    #[must_use]
    pub fn with_reverse_deps(&self, names: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = names
            .iter()
            .filter(|n| self.packages.contains_key(*n))
            .cloned()
            .collect();
        loop {
            let more: Vec<String> = self
                .packages
                .values()
                .filter(|p| !out.contains(&p.name) && p.deps.iter().any(|d| out.contains(d)))
                .map(|p| p.name.clone())
                .collect();
            if more.is_empty() {
                return out;
            }
            out.extend(more);
        }
    }

    /// Whether `path` is the build script of its package.
    #[must_use]
    pub fn is_build_script(&self, path: &RepoPath) -> bool {
        if path.components().last().is_some_and(|n| n == "build.rs") {
            return true;
        }
        self.package_of(path).is_some_and(|p| {
            p.targets
                .iter()
                .any(|t| t.kind.iter().any(|k| k == "custom-build") && &t.src_path == path)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> CargoWorkspace {
        let json = r#"{
          "workspace_root": "/w",
          "workspace_members": ["a 0.1", "b 0.1", "c 0.1"],
          "packages": [
            {"id": "a 0.1", "name": "a", "manifest_path": "/w/Cargo.toml",
             "targets": [{"name": "a", "kind": ["lib"], "src_path": "/w/src/lib.rs"},
                         {"name": "a", "kind": ["bin"], "src_path": "/w/src/main.rs", "doctest": false},
                         {"name": "testsuite", "kind": ["test"], "src_path": "/w/tests/testsuite/main.rs", "doctest": false},
                         {"name": "build-script-build", "kind": ["custom-build"], "src_path": "/w/gen.rs"}],
             "dependencies": [{"name": "b", "path": "/w/crates/b"}, {"name": "serde"}]},
            {"id": "b 0.1", "name": "b", "manifest_path": "/w/crates/b/Cargo.toml",
             "targets": [{"name": "b", "kind": ["lib"], "src_path": "/w/crates/b/src/lib.rs"}],
             "dependencies": [{"name": "c", "path": "/w/crates/c"}]},
            {"id": "c 0.1", "name": "c", "manifest_path": "/w/crates/c/Cargo.toml",
             "targets": [{"name": "c", "kind": ["lib"], "src_path": "/w/crates/c/src/lib.rs", "doctest": false}],
             "dependencies": []},
            {"id": "serde 1", "name": "serde", "manifest_path": "/r/serde/Cargo.toml",
             "targets": [], "dependencies": []}
          ]
        }"#;
        CargoWorkspace::from_metadata_json(json.as_bytes()).unwrap()
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::from_str(s).unwrap()
    }

    #[test]
    fn packages_paths_and_reverse_deps() {
        let ws = sample();
        assert_eq!(ws.packages.len(), 3);
        assert_eq!(
            ws.packages["a"].deps,
            ["b".to_owned()].into_iter().collect()
        );
        assert_eq!(ws.package_of(&p("crates/b/src/x.rs")).unwrap().name, "b");
        assert_eq!(ws.package_of(&p("src/lib.rs")).unwrap().name, "a");
        assert_eq!(ws.package_of(&p("crates/bb/x.rs")).unwrap().name, "a");
        let c: BTreeSet<String> = ["c".to_owned()].into_iter().collect();
        assert_eq!(ws.with_reverse_deps(&c).len(), 3);
        assert!(ws.packages["a"].has_doctests() && !ws.packages["c"].has_doctests());
        assert!(ws.is_build_script(&p("gen.rs")) && ws.is_build_script(&p("crates/b/build.rs")));
        assert!(!ws.is_build_script(&p("src/lib.rs")));
        assert!(
            ws.is_workspace_wide(&p("Cargo.lock"))
                && ws.is_workspace_wide(&p(".cargo/config.toml"))
        );
        assert!(!ws.is_workspace_wide(&p("crates/b/Cargo.toml")));
    }
}
