//! Optional lossless walk of `*.toml` in the cargo.git M0 corpus.
//!
//! Requires the bare clone at `~/.cache/hord/corpora/cargo.git`. Skipped if
//! absent. Ignored by default (large); run with `cargo test -p hord-lang-toml
//! -- --ignored`.

use std::path::PathBuf;
use std::process::Command;

use hord_lang::LangAdapter;
use hord_lang_toml::TomlAdapter;

const CARGO_GIT: &str = ".cache/hord/corpora/cargo.git";

fn cargo_git() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("HOME")?).join(CARGO_GIT);
    path.exists().then_some(path)
}

fn git(git_dir: &std::path::Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

#[test]
#[ignore = "walks cargo.git HEAD; run with --ignored when the corpus is present"]
fn cargo_git_toml_lossless() -> Result<(), Box<dyn std::error::Error>> {
    let Some(git_dir) = cargo_git() else {
        return Ok(());
    };
    let listing = git(&git_dir, &["ls-tree", "-r", "--name-only", "HEAD"]).expect("ls-tree");
    let adapter = TomlAdapter;
    let mut checked = 0usize;
    for path in String::from_utf8_lossy(&listing).lines() {
        if !path.ends_with(".toml") {
            continue;
        }
        let bytes = git(&git_dir, &["show", &format!("HEAD:{path}")])
            .ok_or_else(|| format!("git show HEAD:{path}"))?;
        let tree = adapter
            .parse(&bytes)
            .map_err(|e| format!("parse {path}: {e}"))?;
        assert_eq!(
            adapter.project(&tree).as_slice(),
            bytes.as_slice(),
            "lossless {path}"
        );
        tree.check_concat()
            .map_err(|e| format!("concat {path}: {e}"))?;
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one .toml file in cargo.git HEAD"
    );
    Ok(())
}
