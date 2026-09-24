//! The repositories one server hosts, by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hord_api::{ApiError, RepoBackend};
use hord_txn::{LocalRepo, Repo, RepoOptions};

use crate::route::RepoName;
use crate::{Error, Result};

/// How deep `--root` looks for repositories.
const ROOT_DEPTH: usize = 4;

/// Hosted repositories: one (`--repo`), reached with or without a
/// `/r/<name>/` prefix, or many (`--root`), each reached only by its name.
pub struct Hosts {
    repos: BTreeMap<String, Arc<dyn RepoBackend>>,
    single: Option<String>,
    locals: Vec<Arc<LocalRepo>>,
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts")
            .field("repos", &self.repos.keys().collect::<Vec<_>>())
            .field("single", &self.single)
            .finish()
    }
}

impl Hosts {
    /// Host one repository at `root` with a lander task (`--repo`). Its
    /// name is the directory name.
    pub async fn open_repo(root: &Path, options: RepoOptions) -> Result<Self> {
        let name = dir_name(&root.canonicalize()?);
        let local = open_local(root, options).await?;
        let mut hosts = Self::empty();
        hosts.insert_local(name.clone(), local);
        hosts.single = Some(name);
        Ok(hosts)
    }

    /// Host an open repository, named by its directory, with a lander
    /// task.
    pub fn from_repo(repo: Repo) -> Result<Self> {
        let path = repo.store().repo_root().to_path_buf();
        let name = dir_name(&path);
        let local = LocalRepo::new(repo).map_err(|source| Error::Repo { path, source })?;
        let mut hosts = Self::empty();
        hosts.insert_local(name.clone(), Arc::new(local));
        hosts.single = Some(name);
        Ok(hosts)
    }

    /// Host every repository (a directory holding `.hord/`) under `dir`,
    /// named by its path relative to `dir` (`--root`).
    pub async fn open_root(dir: &Path, options: impl Fn() -> RepoOptions) -> Result<Self> {
        let mut found = Vec::new();
        find_repos(dir, dir, 0, &mut found)?;
        if found.is_empty() {
            return Err(Error::NoRepos(dir.to_path_buf()));
        }
        let mut hosts = Self::empty();
        for (name, path) in found {
            let local = open_local(&path, options()).await?;
            hosts.insert_local(name, local);
        }
        Ok(hosts)
    }

    /// Host already-open backends by name. With one entry, it is also
    /// reached without a prefix.
    #[must_use]
    pub fn from_backends(repos: BTreeMap<String, Arc<dyn RepoBackend>>) -> Self {
        let single = (repos.len() == 1)
            .then(|| repos.keys().next().cloned())
            .flatten();
        Self {
            repos,
            single,
            locals: Vec::new(),
        }
    }

    fn empty() -> Self {
        Self {
            repos: BTreeMap::new(),
            single: None,
            locals: Vec::new(),
        }
    }

    fn insert_local(&mut self, name: String, local: Arc<LocalRepo>) {
        self.repos
            .insert(name, Arc::clone(&local) as Arc<dyn RepoBackend>);
        self.locals.push(local);
    }

    /// Names of the hosted repositories.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.repos.keys().map(String::as_str)
    }

    /// Every hosted backend by name.
    #[must_use]
    pub(crate) fn backends(&self) -> &BTreeMap<String, Arc<dyn RepoBackend>> {
        &self.repos
    }

    /// The backend a request names, or the only one when it names none.
    pub(crate) fn resolve(
        &self,
        name: Option<&RepoName>,
    ) -> Result<Arc<dyn RepoBackend>, ApiError> {
        let name = match (name, &self.single) {
            (Some(RepoName(name)), _) => name,
            (None, Some(single)) => single,
            (None, None) => {
                return Err(ApiError::InvalidArgument(
                    "this server hosts several repositories; address one as /r/<name>/".into(),
                ));
            }
        };
        self.repos
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("no repository {name:?} on this server")))
    }

    /// Stop every lander this server started.
    pub async fn shutdown(&self) {
        for local in &self.locals {
            local.shutdown().await;
        }
    }
}

/// A repository's name from its directory: the last component, else
/// `repo`.
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned())
}

async fn open_local(root: &Path, options: RepoOptions) -> Result<Arc<LocalRepo>> {
    let repo_err = |source| Error::Repo {
        path: root.to_path_buf(),
        source,
    };
    let repo = Repo::open_with(root, options).await.map_err(repo_err)?;
    Ok(Arc::new(LocalRepo::new(repo).map_err(repo_err)?))
}

fn find_repos(
    base: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    if dir.join(hord_store::HORD_DIR).is_dir() {
        let name = dir
            .strip_prefix(base)
            .unwrap_or(dir)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let name = if name.is_empty() { dir_name(dir) } else { name };
        out.push((name, dir.to_path_buf()));
        return Ok(());
    }
    if depth >= ROOT_DEPTH {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && entry.file_type()?.is_dir() {
            find_repos(base, &entry.path(), depth + 1, out)?;
        }
    }
    Ok(())
}
