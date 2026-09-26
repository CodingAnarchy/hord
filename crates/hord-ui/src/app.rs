//! The UI's routes (ADR 0030): server-rendered pages over [`UiRepo`]'s
//! backends, an SSE relay of the event stream for the landing strip, and
//! form POSTs that each become one RPC. A `{snapshot}` of `head` means
//! head's result.
//!
//! | Route | Reads | Acts |
//! |---|---|---|
//! | `GET /` | `Queue`, `Head` | |
//! | `GET /events` (SSE) | `Events`, `Queue` | |
//! | `GET /changes/{id}` | `GetChange`, `ChangeDiff` | |
//! | `POST /changes/{id}/review` | | the review seam ([`ReviewBackend`]) |
//! | `GET /arbitrate/{id}` | `GetChange`, `Head` | |
//! | `POST /arbitrate/{id}` | | `Arbitrate` |
//! | `GET /nodes/{id}` | `NodeLineage` | |
//! | `GET /trace/{id}` and `…/json` | `ChangeTrace` | |
//! | `GET /tree` and `/tree/{snapshot}?path=` | `ListTree` | |
//! | `GET /file/{snapshot}?path=` | `GetFile` | |
//! | `GET /graph/{snapshot}/{node}` | `NodeEdges` | |
//! | `GET /recordings` | `ListRecordings` | |
//! | `GET /recordings/{id}` and `…/frames` | `GetRecording`, `Queue` | |
//! | `GET /static/{name}` | embedded assets | |
//! | `GET`/`POST /login`, `POST /logout` | | the sign-in cookie |

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use askama::Template;
use async_trait::async_trait;
use axum::Router;
use axum::extract::{Form, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use cookie::{Cookie, SameSite};
use hord_api::proto::event::Kind;
use hord_api::{ApiError, ApiResult, ChangesBackend, RepoBackend, proto};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::browse;
use crate::playback::Playback;
use crate::present;
use crate::strip::Strip;
use crate::view::{
    self, ArbitrationPage, ChangePage, ErrorPage, LoginPage, PlaybackPage, RecordingRow,
    RecordingsPage, RowView, StripPage, StripRow, StripRows,
};

/// The cookie the sign-in page keeps the bearer token in (spec §10.5.4).
/// The server reads it on UI paths only, never for a gRPC call.
pub const UI_TOKEN_COOKIE: &str = "hord_token";

/// Most rows the strip renders: the latest submissions.
pub const MAX_ROWS: usize = 500;

/// Recordings kept parsed for scrubbing. Recordings are content-addressed
/// Blobs, so a cached one never goes stale.
const PLAYBACK_CACHE: usize = 4;

/// Signs and attaches a `Review` evidence for a change (spec §10.4 view 2,
/// §10.5.4).
///
/// **Seam for agent `auth`.** A review is signed `Evidence` attached
/// through the API; the signing key and the RPC that carries it belong to
/// the auth slice (`hord review`). Until the server provides one, the
/// change view says review is unavailable and shows no form.
#[async_trait]
pub trait ReviewBackend: Send + Sync {
    /// Record `approve` (or reject) with `message` on `change`. Returns the
    /// Evidence id.
    async fn review(&self, change: String, approve: bool, message: String) -> ApiResult<String>;
}

/// Signs an arbiter's decision before it goes to `Arbitrate` (spec §6.4
/// rung 3, §10.5.4): sets `arbiter`, `key_id`, and `signature` on the
/// request. The server provides it with the key it signs UI decisions
/// with; without one, decisions go unsigned, which only a server without
/// auth accepts.
pub trait ArbitrationSigner: Send + Sync {
    /// Sign `request` in place.
    fn sign(&self, request: &mut proto::ArbitrateRequest) -> ApiResult<()>;
}

/// One repository as the UI sees it: the API it calls and nothing else.
#[derive(Clone)]
pub struct UiRepo {
    /// `hord.v1.RepoBackend`.
    pub backend: Arc<dyn RepoBackend>,
    /// `hord.v1.Changes`.
    pub changes: Arc<dyn ChangesBackend>,
    /// The review seam, when the server provides one.
    pub review: Option<Arc<dyn ReviewBackend>>,
    /// Signs arbitration decisions, when the server provides a key.
    pub arbiter: Option<Arc<dyn ArbitrationSigner>>,
    /// Repository name, for titles.
    pub name: String,
    /// URL prefix of its pages: `""`, or `/r/<name>`.
    pub base: String,
}

impl std::fmt::Debug for UiRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiRepo")
            .field("name", &self.name)
            .field("base", &self.base)
            .field("review", &self.review.is_some())
            .field("arbiter", &self.arbiter.is_some())
            .finish_non_exhaustive()
    }
}

/// Picks the repository a request addresses (the server knows how its
/// `/r/<name>/` prefix was recorded in the request's extensions).
pub trait UiHosts: Send + Sync {
    /// The repository `extensions` address.
    fn resolve(&self, extensions: &Extensions) -> ApiResult<UiRepo>;
}

/// One repository at `/`.
#[derive(Clone, Debug)]
pub struct SingleRepo(pub UiRepo);

impl UiHosts for SingleRepo {
    fn resolve(&self, _extensions: &Extensions) -> ApiResult<UiRepo> {
        Ok(self.0.clone())
    }
}

/// Parsed recordings by `base|id`, most recently loaded last.
type PlaybackCache = Mutex<Vec<(String, Arc<Loaded>)>>;

#[derive(Clone)]
struct App {
    hosts: Arc<dyn UiHosts>,
    playbacks: Arc<PlaybackCache>,
}

/// A parsed recording and the summaries that label its rows.
struct Loaded {
    playback: Playback,
    summaries: HashMap<String, String>,
}

/// The UI's router. Mount it beside the gRPC services; it has no
/// fallback, so it merges with a router that has one.
pub fn router(hosts: Arc<dyn UiHosts>) -> Router {
    let app = App {
        hosts,
        playbacks: Arc::default(),
    };
    Router::new()
        .route("/", get(strip_page))
        .route("/events", get(strip_events))
        .route("/changes/{id}", get(change_page))
        .route("/changes/{id}/review", post(review))
        .route("/arbitrate/{id}", get(workbench).post(arbitrate))
        .route("/nodes/{id}", get(lineage))
        .route("/trace/{id}", get(trace))
        .route("/trace/{id}/json", get(trace_json))
        .route("/tree", get(tree_head))
        .route("/tree/{snapshot}", get(tree))
        .route("/file/{snapshot}", get(file))
        .route("/graph/{snapshot}/{node}", get(graph))
        .route("/recordings", get(recordings))
        .route("/recordings/{id}", get(playback_page))
        .route("/recordings/{id}/frames", get(frames))
        .route("/static/{name}", get(asset))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .with_state(app)
}

/// The addressed repository, or an error page.
struct Ctx(UiRepo);

impl FromRequestParts<App> for Ctx {
    type Rejection = PageError;

    async fn from_request_parts(parts: &mut Parts, state: &App) -> Result<Self, Self::Rejection> {
        state.hosts.resolve(&parts.extensions).map(Ctx).or_page("")
    }
}

/// A failed call, rendered as an error page with the matching status.
#[derive(Debug)]
struct PageError {
    base: String,
    err: ApiError,
}

impl PageError {
    fn new(base: &str, err: ApiError) -> Self {
        Self {
            base: base.to_owned(),
            err,
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        error_page(&self.base, &self.err)
    }
}

fn status_of(err: &ApiError) -> StatusCode {
    match err {
        ApiError::NotFound(_) => StatusCode::NOT_FOUND,
        ApiError::InvalidArgument(_) => StatusCode::BAD_REQUEST,
        ApiError::FailedPrecondition(_) => StatusCode::CONFLICT,
        ApiError::Unimplemented(_) => StatusCode::NOT_IMPLEMENTED,
        ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        ApiError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
        ApiError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_page(base: &str, err: &ApiError) -> Response {
    let page = ErrorPage {
        title: status_of(err)
            .canonical_reason()
            .unwrap_or("error")
            .to_owned(),
        base: base.to_owned(),
        message: err.to_string(),
        sign_in: matches!(
            err,
            ApiError::Unauthenticated(_) | ApiError::PermissionDenied(_)
        ),
    };
    (status_of(err), html(&page)).into_response()
}

fn html(page: &impl Template) -> Response {
    match page.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("render failed: {e}"),
        )
            .into_response(),
    }
}

type PageResult = Result<Response, PageError>;

trait OrPage<T> {
    fn or_page(self, base: &str) -> Result<T, PageError>;
}

impl<T> OrPage<T> for ApiResult<T> {
    fn or_page(self, base: &str) -> Result<T, PageError> {
        self.map_err(|e| PageError::new(base, e))
    }
}

// ------------------------------------------------------------ landing strip

async fn seeded_strip(repo: &UiRepo) -> ApiResult<Strip> {
    let queue = repo.backend.queue(proto::QueueQuery::default()).await?;
    let mut strip = Strip::new();
    let skip = queue.entries.len().saturating_sub(MAX_ROWS);
    for entry in &queue.entries[skip..] {
        strip.seed(entry);
    }
    Ok(strip)
}

async fn strip_page(Ctx(repo): Ctx) -> PageResult {
    let strip = seeded_strip(&repo).await.or_page(&repo.base)?;
    let head = repo
        .backend
        .head(proto::HeadRequest {})
        .await
        .or_page(&repo.base)?
        .change;
    Ok(html(&StripPage {
        title: repo.name.clone(),
        base: repo.base.clone(),
        rows: view::rows(&strip),
        live: Some(format!("{}/events", repo.base)),
        head,
    }))
}

/// SSE: first a `reset` event with every row (seeded from the queue after
/// subscribing, so nothing falls between the two), then one `row` event
/// per event that touched a row, with the event cursor as its id. A
/// reconnecting browser sends `Last-Event-ID`, and the relay resumes after
/// it.
async fn strip_events(Ctx(repo): Ctx, headers: HeaderMap) -> PageResult {
    let from = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut events = repo
        .backend
        .events(proto::EventsRequest { from })
        .await
        .or_page(&repo.base)?;
    let mut strip = seeded_strip(&repo).await.or_page(&repo.base)?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let reset = StripRows {
            base: repo.base.clone(),
            rows: view::rows(&strip),
        };
        if tx.send(Ok(render_event("reset", &reset))).await.is_err() {
            return;
        }
        while let Some(Ok(envelope)) = events.next().await {
            let Some(i) = strip.apply(&envelope) else {
                continue;
            };
            if let Some(Kind::Submitted(s)) = envelope.event.as_ref().and_then(|e| e.kind.as_ref())
                && strip.rows()[i].summary.is_none()
            {
                label(&repo, &mut strip, &s.change).await;
            }
            let row = StripRow {
                base: repo.base.clone(),
                row: RowView::from(&strip.rows()[i]),
            };
            let event = render_event("row", &row).id(envelope.cursor.to_string());
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }
    });
    Ok(
        Sse::new(ReceiverStream::<Result<Event, Infallible>>::new(rx))
            .keep_alive(KeepAlive::default())
            .into_response(),
    )
}

/// Label a new row with its intent summary from the queue.
async fn label(repo: &UiRepo, strip: &mut Strip, change: &str) {
    let query = proto::QueueQuery {
        change: Some(change.to_owned()),
        ..Default::default()
    };
    if let Ok(reply) = repo.backend.queue(query).await
        && let Some(entry) = reply.entries.last()
        && !entry.summary.is_empty()
    {
        strip.set_summary(change, entry.summary.clone());
    }
}

fn render_event(name: &str, fragment: &impl Template) -> Event {
    match fragment.render() {
        Ok(body) => Event::default().event(name).data(body),
        Err(e) => Event::default().event("error").data(e.to_string()),
    }
}

// ------------------------------------------------------------ semantic change

async fn change_page_with(repo: &UiRepo, id: String, flash: Option<String>) -> PageResult {
    let view = repo
        .changes
        .get_change(proto::GetChangeRequest { change: id.clone() })
        .await
        .or_page(&repo.base)?;
    let diff = repo
        .changes
        .change_diff(proto::ChangeDiffRequest { change: id })
        .await
        .ok()
        .map(|d| {
            d.files
                .iter()
                .map(|f| {
                    if f.binary {
                        format!("binary file {} changed\n", f.path)
                    } else {
                        f.unified.clone()
                    }
                })
                .collect::<String>()
        });
    let side = present::side(&view);
    Ok(html(&ChangePage {
        title: side.summary.clone(),
        base: repo.base.clone(),
        status: view.queue.as_ref().map(present::status),
        result: (!view.result.is_empty()).then(|| view.result.clone()),
        evidence: present::evidence(&view),
        provenance: present::provenance(&view),
        reads: view.read_set.iter().map(view::NodeView::from).collect(),
        writes: view.write_set.iter().map(view::NodeView::from).collect(),
        diff,
        history: present::rungs(
            &view.history,
            view.queue.as_ref().and_then(|q| q.escalation.as_ref()),
        ),
        reviewable: repo.review.is_some(),
        review_note: repo.review.is_none().then(|| {
            "Review signing is not available on this server yet (it arrives with `hord review`, spec §10.5.4).".to_owned()
        }),
        flash,
        side,
    }))
}

async fn change_page(Ctx(repo): Ctx, Path(id): Path<String>) -> PageResult {
    change_page_with(&repo, id, None).await
}

#[derive(Debug, Deserialize)]
struct ReviewForm {
    verdict: String,
    #[serde(default)]
    message: String,
}

async fn review(
    Ctx(repo): Ctx,
    Path(id): Path<String>,
    Form(form): Form<ReviewForm>,
) -> PageResult {
    let Some(reviewer) = repo.review.clone() else {
        return Err(PageError::new(
            &repo.base,
            ApiError::Unimplemented("review signing is not available on this server".into()),
        ));
    };
    let approve = match form.verdict.as_str() {
        "approve" => true,
        "reject" => false,
        other => {
            return Err(PageError::new(
                &repo.base,
                ApiError::InvalidArgument(format!("verdict {other:?}")),
            ));
        }
    };
    let flash = match reviewer.review(id.clone(), approve, form.message).await {
        Ok(evidence) => format!(
            "{} recorded as evidence {}",
            if approve { "Approval" } else { "Rejection" },
            view::short_id(&evidence)
        ),
        Err(e) => format!("Review failed: {e}"),
    };
    change_page_with(&repo, id, Some(flash)).await
}

// ------------------------------------------------------------ arbitration

async fn workbench_with(
    repo: &UiRepo,
    id: String,
    flash: Option<String>,
    show_workspace: bool,
) -> PageResult {
    let theirs = repo
        .changes
        .get_change(proto::GetChangeRequest { change: id })
        .await
        .or_page(&repo.base)?;
    let mut ours = Vec::new();
    for landed in present::landed_against(&theirs) {
        ours.push(
            repo.changes
                .get_change(proto::GetChangeRequest { change: landed })
                .await
                .or_page(&repo.base)?,
        );
    }
    let head = repo
        .backend
        .head(proto::HeadRequest {})
        .await
        .or_page(&repo.base)?
        .change;
    let (contested, paths) = present::contested(&theirs, &ours);
    let side = present::side(&theirs);
    let escalation = theirs.queue.as_ref().and_then(|q| q.escalation.as_ref());
    let (reasons, sides) = present::summary(escalation);
    Ok(html(&ArbitrationPage {
        title: format!("Arbitration: {}", side.summary),
        base: repo.base.clone(),
        reason: present::park_reason(&theirs),
        ours: ours.iter().map(present::side).collect(),
        theirs: side,
        contested,
        paths,
        ladder: present::rungs(&theirs.history, escalation),
        status: theirs
            .queue
            .as_ref()
            .map_or_else(|| "not submitted".to_owned(), present::status),
        open: present::arbitrable(theirs.queue.as_ref()),
        attempts: present::attempts(escalation),
        candidates: present::candidates(escalation),
        reasons,
        sides,
        pending: present::pending(escalation),
        head,
        show_workspace,
        flash,
    }))
}

async fn workbench(Ctx(repo): Ctx, Path(id): Path<String>) -> PageResult {
    workbench_with(&repo, id, None, false).await
}

#[derive(Debug, Deserialize)]
struct ArbitrateForm {
    action: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    resolved: String,
}

async fn arbitrate(
    Ctx(repo): Ctx,
    Path(id): Path<String>,
    Form(form): Form<ArbitrateForm>,
) -> PageResult {
    use proto::arbitration::Action;
    let action = match form.action.as_str() {
        "pick_ours" => Action::PickOurs(true),
        "pick_theirs" => Action::PickTheirs(true),
        "replay" => Action::Replay(true),
        "resolved" => Action::Resolved(form.resolved.trim().to_owned()),
        // Editing happens in a workspace; the page shows how, and the
        // resolving change comes back through `resolved`.
        "workspace" => return workbench_with(&repo, id, None, true).await,
        other => {
            return Err(PageError::new(
                &repo.base,
                ApiError::InvalidArgument(format!("action {other:?}")),
            ));
        }
    };
    let note = form.note.trim();
    let mut request = proto::ArbitrateRequest {
        change: id.clone(),
        note: (matches!(action, Action::Replay(_)) && !note.is_empty()).then(|| note.to_owned()),
        action: Some(proto::Arbitration {
            action: Some(action),
        }),
        ..Default::default()
    };
    let signed = match &repo.arbiter {
        Some(arbiter) => arbiter.sign(&mut request),
        None => Ok(()),
    };
    let flash = match signed {
        Err(e) => format!("Could not sign the decision: {e}"),
        Ok(()) => match repo.backend.arbitrate(request).await {
            Ok(reply) => {
                let now = reply
                    .entry
                    .as_ref()
                    .map(present::status)
                    .unwrap_or_default();
                if reply.change == id {
                    format!("Replay requested ({now})")
                } else {
                    format!(
                        "Resolution {} submitted; it lands with both as parents ({now})",
                        view::short_id(&reply.change)
                    )
                }
            }
            Err(e) => format!("Arbitration failed: {e}"),
        },
    };
    workbench_with(&repo, id, Some(flash), false).await
}

// ------------------------------------------------------------ views 4–6

async fn lineage(Ctx(repo): Ctx, Path(id): Path<String>) -> PageResult {
    let reply = repo
        .changes
        .node_lineage(proto::NodeLineageRequest { node: id })
        .await
        .or_page(&repo.base)?;
    Ok(html(&browse::lineage_page(&repo.base, &reply)))
}

async fn change_trace(repo: &UiRepo, id: String) -> ApiResult<proto::ChangeTraceResponse> {
    repo.changes
        .change_trace(proto::ChangeTraceRequest { change: id })
        .await
}

async fn trace(Ctx(repo): Ctx, Path(id): Path<String>) -> PageResult {
    let reply = change_trace(&repo, id).await.or_page(&repo.base)?;
    Ok(html(&browse::trace_page(&repo.base, &reply)))
}

/// The trace as a download: `ChangeTraceResponse` in the canonical JSON
/// mapping, the messages the page is built from.
async fn trace_json(Ctx(repo): Ctx, Path(id): Path<String>) -> PageResult {
    let reply = change_trace(&repo, id.clone()).await.or_page(&repo.base)?;
    let body = serde_json::to_string_pretty(&reply)
        .map_err(|e| PageError::new(&repo.base, ApiError::Internal(e.to_string())))?;
    let disposition = format!(
        "attachment; filename=\"trace-{}.json\"",
        view::short_id(&id)
    );
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// A snapshot in a URL: `head` is head's result, which the RPCs spell as
/// the empty string.
fn snapshot_arg(snapshot: String) -> String {
    if snapshot == "head" {
        String::new()
    } else {
        snapshot
    }
}

#[derive(Debug, Default, Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

async fn tree_page(repo: &UiRepo, snapshot: String, path: String) -> PageResult {
    let reply = repo
        .changes
        .list_tree(proto::ListTreeRequest {
            snapshot: snapshot_arg(snapshot),
            path,
        })
        .await
        .or_page(&repo.base)?;
    Ok(html(&browse::TreePage {
        title: if reply.path.is_empty() {
            "Repository".into()
        } else {
            reply.path.clone()
        },
        base: repo.base.clone(),
        crumbs: browse::crumbs(&reply.path),
        snapshot: reply.snapshot,
        entries: reply.entries,
    }))
}

async fn tree_head(Ctx(repo): Ctx, Query(query): Query<PathQuery>) -> PageResult {
    tree_page(&repo, String::new(), query.path).await
}

async fn tree(
    Ctx(repo): Ctx,
    Path(snapshot): Path<String>,
    Query(query): Query<PathQuery>,
) -> PageResult {
    tree_page(&repo, snapshot, query.path).await
}

async fn file(
    Ctx(repo): Ctx,
    Path(snapshot): Path<String>,
    Query(query): Query<PathQuery>,
) -> PageResult {
    let reply = repo
        .changes
        .get_file(proto::GetFileRequest {
            snapshot: snapshot_arg(snapshot),
            path: query.path,
        })
        .await
        .or_page(&repo.base)?;
    Ok(html(&browse::file_page(&repo.base, &reply)))
}

async fn graph(Ctx(repo): Ctx, Path((snapshot, node)): Path<(String, String)>) -> PageResult {
    let reply = repo
        .changes
        .node_edges(proto::NodeEdgesRequest {
            snapshot: snapshot_arg(snapshot),
            node,
        })
        .await
        .or_page(&repo.base)?;
    Ok(html(&browse::graph_page(&repo.base, &reply)))
}

// ------------------------------------------------------------ playback

async fn recordings(Ctx(repo): Ctx) -> PageResult {
    let list = repo
        .changes
        .list_recordings(proto::ListRecordingsRequest {})
        .await
        .or_page(&repo.base)?;
    Ok(html(&RecordingsPage {
        title: "Flight recordings".into(),
        base: repo.base.clone(),
        recordings: list
            .recordings
            .into_iter()
            .map(|r| {
                let header = r.header.unwrap_or_default();
                RecordingRow {
                    description: if header.description.is_empty() {
                        view::short_id(&r.id).to_owned()
                    } else {
                        header.description
                    },
                    id: r.id,
                    repo: header.repo,
                    events: r.events,
                }
            })
            .collect(),
    }))
}

impl App {
    /// A recording, parsed and labeled, from the cache or the API.
    async fn load(&self, repo: &UiRepo, id: &str) -> ApiResult<Arc<Loaded>> {
        let key = format!("{}|{id}", repo.base);
        {
            let cache = self
                .playbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((_, loaded)) = cache.iter().find(|(k, _)| *k == key) {
                return Ok(Arc::clone(loaded));
            }
        }
        let reply = repo
            .changes
            .get_recording(proto::GetRecordingRequest { id: id.to_owned() })
            .await?;
        // Recordings ship with the repository, so its queue names their
        // changes' intents.
        let summaries = repo
            .backend
            .queue(proto::QueueQuery::default())
            .await?
            .entries
            .into_iter()
            .filter(|e| !e.summary.is_empty())
            .map(|e| (e.change, e.summary))
            .collect();
        let loaded = Arc::new(Loaded {
            playback: Playback::new(reply.header.unwrap_or_default(), reply.events),
            summaries,
        });
        let mut cache = self
            .playbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.retain(|(k, _)| *k != key);
        cache.push((key, Arc::clone(&loaded)));
        if cache.len() > PLAYBACK_CACHE {
            cache.remove(0);
        }
        Ok(loaded)
    }
}

impl Loaded {
    fn rows_at(&self, at: usize) -> Vec<RowView> {
        let mut strip = self.playback.strip_at(at);
        let unlabeled: Vec<String> = strip.unlabeled().into_iter().map(str::to_owned).collect();
        for change in unlabeled {
            if let Some(summary) = self.summaries.get(&change) {
                strip.set_summary(&change, summary.clone());
            }
        }
        view::rows(&strip)
    }
}

#[derive(Debug, Default, Deserialize)]
struct FrameQuery {
    #[serde(default)]
    at: Option<usize>,
    #[serde(default)]
    speed: Option<f64>,
}

async fn playback_page(
    State(app): State<App>,
    Ctx(repo): Ctx,
    Path(id): Path<String>,
    Query(query): Query<FrameQuery>,
) -> PageResult {
    let loaded = app.load(&repo, &id).await.or_page(&repo.base)?;
    let position = query.at.unwrap_or(0).min(loaded.playback.len());
    let header = loaded.playback.header();
    Ok(html(&PlaybackPage {
        title: "Flight recorder".into(),
        base: repo.base.clone(),
        description: header.description.clone(),
        total: loaded.playback.len(),
        rows: loaded.rows_at(position),
        position,
        recording: id,
    }))
}

/// The strip after `at` events, with `Hord-Next-Delay`: milliseconds to
/// wait before the next event at `speed`.
async fn frames(
    State(app): State<App>,
    Ctx(repo): Ctx,
    Path(id): Path<String>,
    Query(query): Query<FrameQuery>,
) -> PageResult {
    let loaded = app.load(&repo, &id).await.or_page(&repo.base)?;
    let at = query.at.unwrap_or(0).min(loaded.playback.len());
    let speed = query.speed.unwrap_or(1.0);
    let delay = loaded.playback.delay_before(at, speed).map_err(|e| {
        let err = match e {
            crate::Error::Speed(_) => ApiError::InvalidArgument(e.to_string()),
            _ => ApiError::Internal(e.to_string()),
        };
        PageError::new(&repo.base, err)
    })?;
    let fragment = StripRows {
        base: repo.base.clone(),
        rows: loaded.rows_at(at),
    };
    let mut response = html(&fragment);
    if let Ok(value) = HeaderValue::from_str(&delay.as_millis().to_string()) {
        response.headers_mut().insert("hord-next-delay", value);
    }
    Ok(response)
}

// ------------------------------------------------------------ sign-in

async fn login_page(Ctx(repo): Ctx) -> Response {
    html(&LoginPage {
        title: "Sign in".into(),
        base: repo.base,
    })
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    token: String,
}

/// Keep the token in an HttpOnly, SameSite=Strict cookie: other sites'
/// pages cannot send it, and scripts cannot read it. The server reads it on
/// UI paths only.
async fn login(Ctx(repo): Ctx, Form(form): Form<LoginForm>) -> Response {
    let cookie = Cookie::build((UI_TOKEN_COOKIE, form.token.trim().to_owned()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .build();
    signed(&repo.base, &cookie)
}

async fn logout(Ctx(repo): Ctx) -> Response {
    let mut cookie = Cookie::build((UI_TOKEN_COOKIE, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .build();
    cookie.make_removal();
    signed(&repo.base, &cookie)
}

/// Set `cookie` and go back to the landing strip.
fn signed(base: &str, cookie: &Cookie<'_>) -> Response {
    let home = if base.is_empty() { "/" } else { base };
    let mut response = Redirect::to(home).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

// ------------------------------------------------------------ assets

async fn asset(Path(name): Path<String>) -> Response {
    match crate::assets::asset(&name) {
        Some(asset) => (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            asset.body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
