//! [`LocalWorkspaces`]: the `Workspaces` service (ADR 0024 amendment) run
//! over a [`Repo`] in this process.
//!
//! The per-repo daemon serves it over its local endpoint; `--no-daemon`
//! calls it directly; against a true remote it runs over the clone's store
//! as a cache, with the remote as the object source, and `propose` pushes
//! the new objects to the remote.
//!
//! Each call runs on tokio's blocking pool, where the command code blocks
//! on the repository's async API.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use hord_api::{ApiError, ApiResult, WorkspacesBackend, proto, wire};
use hord_core::{Actor, ChangeRecord, Intent, ObjectId, Op};
use hord_remote::RemoteRepo;
use hord_store::WorkspaceId;
use hord_txn::{Base, BeginOptions, Materialization, MaterializeMode, Repo};

use crate::txn::{self, Names, block_on, hex};
use crate::{intent, repo};

/// The `Workspaces` service over `repo`.
#[derive(Clone)]
pub struct LocalWorkspaces {
    repo: Repo,
    remote: Option<RemoteRepo>,
    shutdown: Option<Arc<tokio::sync::Notify>>,
}

impl LocalWorkspaces {
    /// Over a local repository (`--no-daemon`, or inside the daemon, which
    /// passes the notifier that stops it).
    pub fn local(repo: Repo, shutdown: Option<Arc<tokio::sync::Notify>>) -> Self {
        Self {
            repo,
            remote: None,
            shutdown,
        }
    }

    /// Over a clone's cache store for `remote` (its objects are read from
    /// `remote` and proposals pushed to it).
    pub fn remote(cache: Repo, remote: RemoteRepo) -> Self {
        Self {
            repo: cache,
            remote: Some(remote),
            shutdown: None,
        }
    }

    async fn run<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Repo, Option<&RemoteRepo>) -> Result<T> + Send + 'static,
    {
        let repo = self.repo.clone();
        let remote = self.remote.clone();
        tokio::task::spawn_blocking(move || f(&repo, remote.as_ref()))
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))?
            .map_err(api_error)
    }
}

/// An API error for a failed command: the repository's own errors keep
/// their kind; anything else is a precondition with the full message.
pub fn api_error(err: anyhow::Error) -> ApiError {
    let message = format!("{err:#}");
    if let Some(txn) = err.downcast_ref::<hord_txn::Error>() {
        let kind = match txn {
            hord_txn::Error::MissingChange(_)
            | hord_txn::Error::UnknownWorkspace(_)
            | hord_txn::Error::MissingFile(_) => ApiError::NotFound(message),
            hord_txn::Error::NothingToPropose | hord_txn::Error::UnresolvedDeclaration(_) => {
                ApiError::FailedPrecondition(message)
            }
            _ => ApiError::Internal(message),
        };
        return kind;
    }
    if let Some(ApiError::NotFound(_) | ApiError::InvalidArgument(_)) =
        err.downcast_ref::<ApiError>()
    {
        return err
            .downcast::<ApiError>()
            .unwrap_or(ApiError::Internal(message));
    }
    ApiError::FailedPrecondition(message)
}

fn caller(caller: Option<&proto::Caller>) -> Result<(Actor, Option<String>)> {
    match caller {
        Some(caller) => {
            let actor = match &caller.actor {
                Some(actor) => wire::actor_from("caller.actor", actor)?,
                None => txn::actor(),
            };
            Ok((actor, caller.session.clone()))
        }
        None => Ok((txn::actor(), txn::session())),
    }
}

/// The caller of this process, for requests.
pub fn this_caller() -> proto::Caller {
    proto::Caller {
        actor: Some(wire::actor(&txn::actor())),
        session: txn::session(),
    }
}

/// Where a remote workspace starts: head, or a change, snapshot, or ref of
/// the remote.
fn remote_base(remote: &RemoteRepo, cache: &Repo, spec: Option<&str>) -> Result<Base> {
    use hord_api::RepoBackend;
    let id = match spec {
        None | Some("head" | "HEAD") => {
            let head = block_on(remote.head(proto::HeadRequest {}))?;
            match head.change {
                Some(change) => wire::object_id("head", &change)?,
                None => return Ok(Base::Head),
            }
        }
        Some(spec) => match spec.parse::<ObjectId>() {
            Ok(id) => id,
            Err(_) => {
                let refs = block_on(remote.refs(proto::RefsRequest {
                    prefix: spec.to_owned(),
                }))?;
                let id = refs.refs.get(spec).ok_or_else(|| {
                    anyhow!("unknown ref {spec:?} on remote {}", remote.address())
                })?;
                wire::object_id("ref", id)?
            }
        },
    };
    // A change starts from its result and becomes the parent; anything
    // else is taken as a snapshot.
    Ok(match block_on(cache.change(id)) {
        Ok(_) => Base::Change(id),
        Err(_) => Base::Snapshot(id),
    })
}

fn ws_new(
    repo: &Repo,
    remote: Option<&RemoteRepo>,
    request: &proto::WsNewRequest,
) -> Result<proto::WsNewResponse> {
    let (actor, session) = caller(request.caller.as_ref())?;
    let base = match remote {
        Some(remote) => remote_base(remote, repo, request.base.as_deref())?,
        None => Base::Snapshot(repo::resolve_base(repo.store(), request.base.as_deref())?),
    };
    let mode = match proto::Materialize::try_from(request.materialize) {
        Ok(proto::Materialize::Copy) => MaterializeMode::Copy,
        _ => MaterializeMode::Clone,
    };
    let ws = block_on(repo.begin_directory_with(
        BeginOptions {
            base,
            actor,
            session,
        },
        mode,
    ))?;
    repo::set_current_workspace(repo.store(), ws.id())?;
    let path = match ws.materialization() {
        Materialization::Directory { path } => path.display().to_string(),
        Materialization::InMemory => String::new(),
    };
    Ok(proto::WsNewResponse {
        id: ws.id().to_string(),
        base: ws.base().to_hex(),
        materialization: path,
        materialize: ws.materialized_as().unwrap_or(mode).as_str().to_owned(),
    })
}

fn ws_list(repo: &Repo) -> Result<proto::WsListResponse> {
    let current = repo::current_workspace_id(repo.store())?;
    let mut workspaces: Vec<_> = repo
        .store()
        .list_workspaces()?
        .into_iter()
        .map(|meta| proto::WorkspaceInfo {
            id: meta.id.to_string(),
            base: hex(meta.base),
            materialization: meta.path.display().to_string(),
        })
        .collect();
    workspaces.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(proto::WsListResponse {
        workspaces,
        current: current.map(|id| id.to_string()),
    })
}

fn ws_rm(repo: &Repo, request: &proto::WsRmRequest) -> Result<proto::WsRmResponse> {
    let id: WorkspaceId = request
        .id
        .parse()
        .map_err(|_| anyhow!("invalid workspace id {:?}", request.id))?;
    if !block_on(repo.remove_workspace(id))? {
        return Err(ApiError::NotFound(format!("unknown workspace {}", request.id)).into());
    }
    Ok(proto::WsRmResponse {
        id: request.id.clone(),
        removed: true,
    })
}

fn ws_gc(repo: &Repo) -> Result<proto::WsGcResponse> {
    let removed = block_on(repo.gc_pristine())?;
    Ok(proto::WsGcResponse {
        removed_pristine: removed.iter().map(|s| s.to_hex()).collect(),
    })
}

/// A workspace's flattened op, with names from `names`.
pub fn op_message(op: &Op, names: &Names) -> proto::OpView {
    let text = txn::op_text(op, names);
    let mut view = proto::OpView {
        text,
        ..Default::default()
    };
    match op {
        Op::Insert {
            parent,
            index,
            node,
        } => {
            view.op = "insert".into();
            view.parent = Some(parent.to_string());
            view.index = Some(*index);
            view.node = Some(hex(*node));
        }
        Op::Delete { node } => {
            view.op = "delete".into();
            view.node = Some(node.to_string());
        }
        Op::Replace { node, from, to } => {
            view.op = "replace".into();
            view.node = Some(node.to_string());
            view.from = Some(hex(*from));
            view.to = Some(hex(*to));
        }
        Op::Move {
            node,
            from_parent,
            to_parent,
            index,
        } => {
            view.op = "move".into();
            view.node = Some(node.to_string());
            view.from = Some(from_parent.to_string());
            view.to = Some(to_parent.to_string());
            view.index = Some(*index);
        }
        Op::Rename { node, from, to } => {
            view.op = "rename".into();
            view.node = Some(node.to_string());
            view.from = Some(from.as_str().to_owned());
            view.to = Some(to.as_str().to_owned());
        }
        Op::Blob { path, from, to } => {
            view.op = "blob".into();
            view.path = Some(path.to_string());
            view.from = from.map(hex);
            view.to = to.map(hex);
        }
        Op::Tree { path, kind } => {
            view.op = "tree".into();
            view.path = Some(path.to_string());
            view.kind = Some(format!("{kind:?}"));
        }
    }
    view
}

fn node_ref(names: &Names, node: hord_core::NodeId) -> proto::NodeRef {
    let view = names.view(node);
    proto::NodeRef {
        id: view.id,
        name: view.name,
        path: view.path,
    }
}

/// ADR 0012: a `Directory` workspace cannot see reads made by other tools.
pub fn reads_label(observed: bool) -> &'static str {
    if observed { "observed" } else { "unobserved" }
}

fn status(repo: &Repo, request: &proto::StatusRequest) -> Result<proto::StatusResponse> {
    let (actor, session) = caller(request.caller.as_ref())?;
    let meta = repo::resolve_workspace(repo.store(), request.workspace.as_deref())?;
    let head = block_on(repo.head())?.change.map(hex);
    let mut ws = block_on(repo.open_workspace(meta.id, actor, session))?;
    ws.set_paranoid(request.paranoid);
    let preview = Intent {
        summary: "(status preview)".into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    };
    let record: Option<ChangeRecord> = match block_on(ws.preview(preview)) {
        Ok(proposal) => Some(proposal.record),
        Err(hord_txn::Error::NothingToPropose) => None,
        Err(err) => return Err(err.into()),
    };
    let skipped = ws
        .skipped()
        .iter()
        .map(|path| {
            let mut shown = path.to_string();
            if meta.path.join(&shown).is_dir() {
                shown.push('/');
            }
            shown
        })
        .collect();
    let mut response = proto::StatusResponse {
        workspace: meta.id.to_string(),
        base: hex(meta.base),
        head,
        materialization: meta.path.display().to_string(),
        reads: reads_label(ws.access_log().reads_observed).to_owned(),
        changes: record.is_some(),
        skipped,
        ..Default::default()
    };
    if let Some(record) = &record {
        let names = Names::for_records(repo, &[record])?;
        response.ops = record.ops.iter().map(|op| op_message(op, &names)).collect();
        response.read_set = record
            .read_set
            .iter()
            .map(|n| node_ref(&names, *n))
            .collect();
        response.write_set = record
            .write_set
            .iter()
            .map(|n| node_ref(&names, *n))
            .collect();
    }
    Ok(response)
}

fn propose(
    repo: &Repo,
    remote: Option<&RemoteRepo>,
    request: &proto::ProposeRequest,
) -> Result<proto::ProposeResponse> {
    let (actor, session) = caller(request.caller.as_ref())?;
    let file = intent::parse(&request.intent)
        .with_context(|| format!("parse intent file {}", request.intent_path))?;
    let meta = repo::resolve_workspace(repo.store(), request.workspace.as_deref())?;
    let mut ws = block_on(repo.open_workspace(meta.id, actor, session))?;
    for read in file.reads {
        ws.declare_read(read);
    }
    let proposal = block_on(ws.propose(file.intent))?;
    let pushed = match remote {
        Some(remote) => Some(
            block_on(hord_remote::push_change(remote, repo, proposal.change))
                .context("push the proposal's objects to the remote")?,
        ),
        None => None,
    };
    let record = &proposal.record;
    Ok(proto::ProposeResponse {
        change: hex(proposal.change),
        workspace: meta.id.to_string(),
        base: hex(record.base),
        result: hex(record.result),
        parents: record.parents.iter().map(|p| hex(*p)).collect(),
        summary: record.intent.summary.clone(),
        ops: u32::try_from(record.ops.len()).unwrap_or(u32::MAX),
        write_set: record.write_set.iter().map(ToString::to_string).collect(),
        read_set: record.read_set.iter().map(ToString::to_string).collect(),
        reads: reads_label(ws.access_log().reads_observed).to_owned(),
        pushed: pushed.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
    })
}

#[async_trait]
impl WorkspacesBackend for LocalWorkspaces {
    async fn ws_new(&self, request: proto::WsNewRequest) -> ApiResult<proto::WsNewResponse> {
        self.run(move |repo, remote| ws_new(repo, remote, &request))
            .await
    }

    async fn ws_list(&self, _request: proto::WsListRequest) -> ApiResult<proto::WsListResponse> {
        self.run(|repo, _| ws_list(repo)).await
    }

    async fn ws_rm(&self, request: proto::WsRmRequest) -> ApiResult<proto::WsRmResponse> {
        self.run(move |repo, _| ws_rm(repo, &request)).await
    }

    async fn ws_gc(&self, _request: proto::WsGcRequest) -> ApiResult<proto::WsGcResponse> {
        self.run(|repo, _| ws_gc(repo)).await
    }

    async fn status(&self, request: proto::StatusRequest) -> ApiResult<proto::StatusResponse> {
        self.run(move |repo, _| status(repo, &request)).await
    }

    async fn propose(&self, request: proto::ProposeRequest) -> ApiResult<proto::ProposeResponse> {
        self.run(move |repo, remote| propose(repo, remote, &request))
            .await
    }

    /// SEAM (policy agent): `hord policy check` is implemented in
    /// `cmd/policy.rs`, which does not expose a library entry point yet.
    /// Until it does, the daemon does not serve it, and the CLI runs the
    /// command in-process after stopping the daemon.
    async fn policy_check(
        &self,
        _request: proto::PolicyCheckRequest,
    ) -> ApiResult<proto::PolicyCheckResponse> {
        Err(ApiError::Unimplemented(
            "policy check runs in the CLI process until hord-cli's policy command exposes a \
             library entry point"
                .into(),
        ))
    }

    async fn shutdown(
        &self,
        _request: proto::ShutdownRequest,
    ) -> ApiResult<proto::ShutdownResponse> {
        match &self.shutdown {
            Some(stop) => {
                stop.notify_one();
                Ok(proto::ShutdownResponse {})
            }
            None => Err(ApiError::Unimplemented("not a daemon".into())),
        }
    }
}
