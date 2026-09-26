//! The bridge: export, pull requests, and divergence checks (ADR 0036).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hord_api::proto::event::Kind;
use hord_api::proto::{BridgeCheckTrigger, QueueStatus};
use hord_api::{ApiError, EventStream, RepoBackend, proto, wire};
use hord_core::{ChangeId, Intent, IntentRef};
use tokio::fs;
use tokio::runtime::Handle;
use tokio::task::spawn_blocking;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep};
use tokio_stream::StreamExt;

use super::SyncError;
use super::cache::{CacheStore, ObjectCache};
use super::mirror::{Mirror, Pushed};
use super::pulls::{PullRequest, PullRequests, StatusState};
use super::state::{PullState, State};
use crate::export::{export_changes, lookup_exported, open_or_init};
use crate::import::{open_repo, propose_git_commit};
use crate::{Error, GitOid};

/// Where exported changes are named in the bridge's export repository.
const CHANGE_REFS: &str = "refs/hord/changes/";

/// How the bridge runs.
#[derive(Clone)]
pub struct BridgeOptions {
    /// The mirror's git URL (GitHub, or a bare repository path in tests).
    /// Keep credentials out of it: pass [`Self::token`].
    pub remote: String,
    /// Token for the mirror's HTTPS git endpoint, from the bridge's config,
    /// never from the repository.
    pub token: Option<String>,
    /// The bridge's own directory: its bare export repository and state.
    pub work_dir: PathBuf,
    /// Key id of the bridge, recorded as the voucher of every pull
    /// request's proposal (ADR 0037). Unset against a repository without
    /// auth.
    pub voucher: Option<String>,
    /// How often pull requests are polled.
    pub poll: Duration,
    /// How often `main` is checked for divergence (ADR 0036: hourly).
    pub check_every: Duration,
}

impl fmt::Debug for BridgeOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeOptions")
            .field("remote", &self.remote)
            .field("token", &self.token.as_ref().map(|_| "…"))
            .field("work_dir", &self.work_dir)
            .field("voucher", &self.voucher)
            .field("poll", &self.poll)
            .field("check_every", &self.check_every)
            .finish()
    }
}

impl BridgeOptions {
    /// Mirror to `remote` from `work_dir`, polling every minute and checking
    /// every hour, with no token or voucher.
    #[must_use]
    pub fn new(remote: impl Into<String>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            remote: remote.into(),
            token: None,
            work_dir: work_dir.into(),
            voucher: None,
            poll: Duration::from_secs(60),
            check_every: Duration::from_secs(60 * 60),
        }
    }
}

/// One divergence check, as recorded in the `BridgeChecked` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Check {
    /// The mirror, without credentials.
    pub remote: String,
    /// What made the bridge check.
    pub trigger: BridgeCheckTrigger,
    /// `main` names a commit the export of the log does not reach.
    pub diverged: bool,
    /// The export of the log's head, hex; unset on an empty log.
    pub expected: Option<String>,
    /// What `main` names, hex; unset when it has none.
    pub actual: Option<String>,
    /// The log's head.
    pub head: Option<ChangeId>,
    /// The outcome in words.
    pub detail: String,
    /// The recorded event's cursor; unset when the backend keeps no event
    /// log.
    pub recorded: Option<u64>,
}

/// What one [`Bridge::sync_once`] did.
#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    /// Landed changes exported, in log order.
    pub exported: Vec<ChangeId>,
    /// The commit pushed to `main`, hex, if one was.
    pub pushed: Option<String>,
    /// Pull requests submitted as proposals: number and change.
    pub proposed: Vec<(u64, ChangeId)>,
    /// Outcomes reported on pull requests: number and outcome.
    pub reported: Vec<(u64, String)>,
    /// Divergence checks run (after a push).
    pub checks: Vec<Check>,
}

/// The git bridge daemon for one repository (spec §9 Sync, ADR 0036).
pub struct Bridge {
    backend: Arc<dyn RepoBackend>,
    pulls: Option<Arc<dyn PullRequests>>,
    mirror: Mirror,
    cache: Arc<ObjectCache>,
    state: State,
    state_path: PathBuf,
    options: BridgeOptions,
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bridge")
            .field("mirror", &self.mirror)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// A bridge from `backend` to `options.remote`, with pull requests from
    /// `pulls` (none: export and checks only). Creates the work directory
    /// and loads the state a previous run left there.
    pub async fn open(
        backend: Arc<dyn RepoBackend>,
        pulls: Option<Arc<dyn PullRequests>>,
        options: BridgeOptions,
    ) -> Result<Self, SyncError> {
        let work = options.work_dir.clone();
        fs::create_dir_all(&work)
            .await
            .map_err(|source| SyncError::Io {
                path: work.clone(),
                source,
            })?;
        let git_dir = work.join("export.git");
        let init = git_dir.clone();
        blocking(move || Ok(open_or_init(&init).map(drop)?)).await?;
        let state_path = work.join("state.json");
        let state = State::load(&state_path).await?;
        Ok(Self {
            cache: Arc::new(ObjectCache::new(Arc::clone(&backend))),
            backend,
            pulls,
            mirror: Mirror::new(options.remote.clone(), options.token.clone(), git_dir),
            state,
            state_path,
            options,
        })
    }

    /// The bridge's bare export repository.
    #[must_use]
    pub fn export_dir(&self) -> &Path {
        self.mirror.git_dir()
    }

    /// Export and push what landed, report outcomes on pull requests, and
    /// submit new or updated pull requests: `hord git sync --once`.
    pub async fn sync_once(&mut self) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::default();
        self.sync_exports(&mut report).await?;
        self.report_outcomes(&mut report).await?;
        self.sync_pulls(&mut report).await?;
        self.report_outcomes(&mut report).await?;
        Ok(report)
    }

    /// Run until `shutdown`: follow the event stream from the saved cursor
    /// (exporting on each landing), poll pull requests, and check for
    /// divergence every [`BridgeOptions::check_every`]. A failed step is
    /// reported on stderr and retried on the next tick.
    pub async fn run(&mut self, shutdown: impl Future<Output = ()>) -> Result<(), SyncError> {
        tokio::pin!(shutdown);
        let mut poll = interval(self.options.poll);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let every = self.options.check_every;
        let mut hourly = interval_at(Instant::now() + every, every);
        hourly.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut events = self.subscribe().await;
        loop {
            tokio::select! {
                () = &mut shutdown => return Ok(()),
                _ = poll.tick() => {
                    let step = self.sync_once().await;
                    warn("sync", step.map(drop));
                }
                _ = hourly.tick() => {
                    let mut report = SyncReport::default();
                    let step = async {
                        self.sync_exports(&mut report).await?;
                        self.check(BridgeCheckTrigger::Hourly).await
                    }
                    .await;
                    warn("hourly check", step.map(drop));
                }
                item = next_event(&mut events) => match item {
                    Some(Ok(envelope)) => {
                        let step = self.on_event(envelope).await;
                        warn("event", step);
                    }
                    Some(Err(err)) => {
                        warn("event stream", Err(err.into()));
                        events = None;
                    }
                    None => {
                        // The stream ended or was never opened: resubscribe
                        // from the saved cursor after a pause.
                        sleep(self.options.poll).await;
                        events = self.subscribe().await;
                    }
                },
            }
        }
    }

    async fn subscribe(&self) -> Option<EventStream> {
        let request = proto::EventsRequest {
            from: self.state.cursor,
        };
        match self.backend.events(request).await {
            Ok(stream) => Some(stream),
            Err(err) => {
                warn("subscribe to events", Err(err.into()));
                None
            }
        }
    }

    async fn on_event(&mut self, envelope: proto::EventEnvelope) -> Result<(), SyncError> {
        let kind = envelope.event.as_ref().and_then(|e| e.kind.as_ref());
        let relevant = matches!(
            kind,
            Some(
                Kind::Landed(_)
                    | Kind::Parked(_)
                    | Kind::Rejected(_)
                    | Kind::Arbitrated(_)
                    | Kind::Replaying(_)
                    | Kind::HeadMoved(_)
            )
        );
        if relevant {
            let mut report = SyncReport::default();
            self.sync_exports(&mut report).await?;
            self.report_outcomes(&mut report).await?;
        }
        self.state.cursor = Some(envelope.cursor);
        self.save().await
    }

    async fn save(&self) -> Result<(), SyncError> {
        self.state.save(&self.state_path).await
    }

    // ---------------------------------------------------------------- export

    /// Landed changes after `after` (all, when unset), in log order.
    async fn landed_since(&self, after: Option<ChangeId>) -> Result<Vec<ChangeId>, SyncError> {
        let mut newest_first = Vec::new();
        let mut next = None;
        loop {
            let page = self
                .backend
                .log(proto::LogQuery {
                    after: next,
                    ..Default::default()
                })
                .await?;
            for item in page.items {
                let id = wire::object_id("change", &item.change)?;
                if Some(id) == after {
                    newest_first.reverse();
                    return Ok(newest_first);
                }
                newest_first.push(id);
            }
            match page.next {
                Some(cursor) => next = Some(cursor),
                None => break,
            }
        }
        if let Some(after) = after {
            return Err(SyncError::State {
                path: self.state_path.clone(),
                reason: format!("the change last pushed to main, {after}, is not in the log"),
            });
        }
        newest_first.reverse();
        Ok(newest_first)
    }

    /// Export `changes` (and any unexported ancestors) into the export
    /// repository; their commits, in order.
    async fn export(&self, changes: &[ChangeId]) -> Result<Vec<gix::ObjectId>, SyncError> {
        self.cache.prefetch(changes).await?;
        let store = CacheStore::new(Arc::clone(&self.cache), Handle::current());
        let git_dir = self.export_dir().to_owned();
        let changes = changes.to_vec();
        blocking(move || {
            Ok(export_changes(&store, &changes, &git_dir)?
                .into_iter()
                .map(|oid| oid.as_gix())
                .collect())
        })
        .await
    }

    /// Export what landed since the last push and push it to `main`, fast
    /// forward only; then check for divergence.
    async fn sync_exports(&mut self, report: &mut SyncReport) -> Result<(), SyncError> {
        let pushed = self
            .state
            .pushed
            .as_deref()
            .map(|id| wire::object_id("pushed", id))
            .transpose()?;
        let pending = self.landed_since(pushed).await?;
        let Some(last) = pending.last().copied() else {
            return Ok(());
        };
        let commits = self.export(&pending).await?;
        report.exported.extend(&pending);
        let Some(tip) = commits.last().copied() else {
            return Ok(());
        };
        // The log is a chain of first parents (the lander lands on head),
        // so the newest export fast-forwards `main` over the others.
        match self.mirror.push(tip, false).await? {
            Pushed::Updated => {
                self.state.pushed = Some(wire::id(last));
                self.save().await?;
                report.pushed = Some(tip.to_string());
            }
            Pushed::Rejected(_) => {}
        }
        report
            .checks
            .push(self.check(BridgeCheckTrigger::Push).await?);
        Ok(())
    }

    /// Compare `main` with the export of the log's head and record the
    /// outcome as a `BridgeChecked` event: `hord git sync --check`.
    pub async fn check(&self, trigger: BridgeCheckTrigger) -> Result<Check, SyncError> {
        let head = self
            .backend
            .head(proto::HeadRequest {})
            .await?
            .change
            .map(|id| wire::object_id("head", &id))
            .transpose()?;
        let expected = match head {
            Some(head) => self.export(&[head]).await?.first().copied(),
            None => None,
        };
        let actual = self.mirror.main().await?;
        let (diverged, detail) = match (expected, actual) {
            (None, None) => (false, "the log and main are both empty".to_owned()),
            (None, Some(actual)) => (true, format!("main names {actual}, but the log is empty")),
            (Some(expected), None) => (
                false,
                format!("main does not exist yet; the bridge pushes {expected} next"),
            ),
            (Some(expected), Some(actual)) if expected == actual => {
                (false, format!("main is the export of the log ({expected})"))
            }
            (Some(expected), Some(actual)) => {
                let git_dir = self.export_dir().to_owned();
                let behind = blocking(move || Ok(is_ancestor(&git_dir, actual, expected))).await?;
                if behind {
                    (
                        false,
                        format!(
                            "main lags the log: {actual} is an earlier export; \
                             the bridge pushes {expected} next"
                        ),
                    )
                } else {
                    (
                        true,
                        format!(
                            "main names {actual}, which the export of the log ({expected}) \
                             does not reach: run `hord git sync --repair`"
                        ),
                    )
                }
            }
        };
        let event = proto::BridgeChecked {
            remote: self.mirror.display_url(),
            diverged,
            expected: expected.map(|c| c.to_string()),
            actual: actual.map(|c| c.to_string()),
            trigger: trigger.into(),
            detail: detail.clone(),
            head: head.map(wire::id),
        };
        let recorded = match self.backend.record_bridge_check(event).await {
            Ok(reply) => Some(reply.cursor),
            Err(ApiError::Unimplemented(_)) => None,
            Err(err) => return Err(err.into()),
        };
        Ok(Check {
            remote: self.mirror.display_url(),
            trigger,
            diverged,
            expected: expected.map(|c| c.to_string()),
            actual: actual.map(|c| c.to_string()),
            head,
            detail,
            recorded,
        })
    }

    /// Force-push the export of the log's head to `main`, whatever `main`
    /// names, then check: `hord git sync --repair`. Commits on `main` that
    /// the export does not reach are dropped from it.
    pub async fn repair(&mut self) -> Result<Check, SyncError> {
        let head = self
            .backend
            .head(proto::HeadRequest {})
            .await?
            .change
            .ok_or_else(|| SyncError::Unproposable("the log is empty: nothing to push".into()))?;
        let head = wire::object_id("head", &head)?;
        let commit = self
            .export(&[head])
            .await?
            .first()
            .copied()
            .ok_or_else(|| SyncError::Unproposable(format!("{head} did not export")))?;
        if let Pushed::Rejected(line) = self.mirror.push(commit, true).await? {
            return Err(SyncError::Command {
                command: "push --force".into(),
                detail: line,
            });
        }
        self.state.pushed = Some(wire::id(head));
        self.save().await?;
        self.check(BridgeCheckTrigger::Repair).await
    }

    /// Whether the landed change `change` is on `main`: exported, and an
    /// ancestor of (or) the last commit pushed.
    async fn on_main(&self, change: ChangeId) -> Result<bool, SyncError> {
        let Some(pushed) = self.state.pushed.as_deref() else {
            return Ok(false);
        };
        let pushed = wire::object_id("pushed", pushed)?;
        let git_dir = self.export_dir().to_owned();
        blocking(move || {
            let repo = open_repo(&git_dir)?;
            let (Some(commit), Some(tip)) = (
                lookup_exported(&repo, change),
                lookup_exported(&repo, pushed),
            ) else {
                return Ok(false);
            };
            Ok(is_ancestor(&git_dir, commit, tip))
        })
        .await
    }

    // ---------------------------------------------------------------- pull requests

    /// Submit each open pull request whose head is new. An update to one
    /// whose earlier proposal the lander is still working on waits for it.
    async fn sync_pulls(&mut self, report: &mut SyncReport) -> Result<(), SyncError> {
        let Some(pulls) = self.pulls.clone() else {
            return Ok(());
        };
        let open = pulls.open_pulls().await?;
        let numbers: BTreeSet<u64> = open.iter().map(|p| p.number).collect();
        // Closed elsewhere: forget it.
        self.state.pulls.retain(|n, _| numbers.contains(n));
        for pull in open {
            let tracked = self.state.pulls.get(&pull.number).cloned();
            if let Some(tracked) = &tracked {
                if tracked.head == pull.head_sha {
                    continue;
                }
                if let Some(change) = &tracked.change
                    && self.pending(change).await?
                {
                    continue;
                }
            }
            match self.propose(&pull).await {
                Ok((change, head)) => {
                    report.proposed.push((pull.number, change));
                    if let Some(old) = tracked.as_ref().and_then(|t| t.change.as_deref()) {
                        let text = format!(
                            "The new push supersedes proposal `{}`; submitted `{}` to the hord \
                             lander.",
                            short(old),
                            short(&wire::id(change))
                        );
                        pulls.comment(pull.number, &text).await?;
                    }
                    self.state.pulls.insert(
                        pull.number,
                        PullState {
                            head,
                            change: Some(wire::id(change)),
                            reported: None,
                        },
                    );
                }
                Err(err) if reportable(&err) => {
                    let text = format!("The hord bridge could not submit this pull request: {err}");
                    pulls
                        .set_status(
                            pull.number,
                            &pull.head_sha,
                            StatusState::Error,
                            &clip(&text),
                        )
                        .await?;
                    pulls.comment(pull.number, &text).await?;
                    report.reported.push((pull.number, "error".into()));
                    self.state.pulls.insert(
                        pull.number,
                        PullState {
                            head: pull.head_sha.clone(),
                            change: None,
                            reported: Some("error".into()),
                        },
                    );
                }
                Err(err) => return Err(err),
            }
            self.save().await?;
        }
        Ok(())
    }

    /// Whether the lander still has `change` queued or replaying.
    async fn pending(&self, change: &str) -> Result<bool, SyncError> {
        let entry = self.entry(change).await?;
        Ok(entry
            .is_some_and(|e| matches!(e.status(), QueueStatus::Queued | QueueStatus::Replaying)))
    }

    async fn entry(&self, change: &str) -> Result<Option<proto::QueueEntry>, SyncError> {
        let queue = self
            .backend
            .queue(proto::QueueQuery {
                change: Some(change.to_owned()),
                ..Default::default()
            })
            .await?;
        Ok(queue.entries.into_iter().last())
    }

    /// Import `pull`'s head as one proposal against the landed change it is
    /// based on, store its objects on the backend, and submit it. Returns
    /// the change and the head it was built from.
    async fn propose(&self, pull: &PullRequest) -> Result<(ChangeId, String), SyncError> {
        let local = format!("refs/hord-bridge/pulls/{}", pull.number);
        let head = self.mirror.fetch(&pull.git_ref, &local).await?;
        let git_dir = self.export_dir().to_owned();
        let base = blocking(move || find_base(&git_dir, head))
            .await?
            .ok_or_else(|| {
                SyncError::Unproposable(format!(
                    "{head} is not based on a commit of main; rebase it onto main and push again"
                ))
            })?;
        self.cache.prefetch(&[base]).await?;
        let mut refs = Vec::new();
        if !pull.url.is_empty() {
            refs.push(IntentRef::Url {
                url: pull.url.clone(),
            });
        }
        let intent = Intent {
            summary: pull.title.clone(),
            body: pull.body.clone(),
            refs,
            acceptance: Vec::new(),
        };
        let voucher = self.options.voucher.clone();
        let mut store = CacheStore::new(Arc::clone(&self.cache), Handle::current());
        let git_dir = self.export_dir().to_owned();
        let (change, store) = blocking(move || {
            let change = propose_git_commit(
                &mut store,
                &git_dir,
                GitOid::from_gix(head),
                base,
                intent,
                voucher,
            )?;
            Ok((change, store))
        })
        .await?;
        self.cache.upload(&store.written).await?;
        self.backend
            .submit(proto::SubmitRequest {
                change: wire::id(change),
            })
            .await?;
        Ok((change, head.to_string()))
    }

    /// Report each proposal's lander outcome on its pull request, once per
    /// outcome, and close a pull request whose change is on `main`.
    async fn report_outcomes(&mut self, report: &mut SyncReport) -> Result<(), SyncError> {
        let Some(pulls) = self.pulls.clone() else {
            return Ok(());
        };
        let tracked: Vec<(u64, PullState)> = self
            .state
            .pulls
            .iter()
            .map(|(n, s)| (*n, s.clone()))
            .collect();
        for (number, pull) in tracked {
            let Some(change) = pull.change.as_deref() else {
                continue;
            };
            let Some(entry) = self.entry(change).await? else {
                continue;
            };
            let Some(outcome) = self.outcome(change, &entry).await? else {
                continue;
            };
            if pull.reported.as_deref() == Some(outcome.key) {
                continue;
            }
            pulls
                .set_status(number, &pull.head, outcome.state, &clip(&outcome.status))
                .await?;
            if let Some(text) = &outcome.comment {
                pulls.comment(number, text).await?;
            }
            if outcome.close {
                pulls.close(number).await?;
            }
            report.reported.push((number, outcome.key.to_owned()));
            if let Some(state) = self.state.pulls.get_mut(&number) {
                state.reported = Some(outcome.key.to_owned());
            }
            self.save().await?;
        }
        Ok(())
    }

    /// What to report for `entry`; `None` while there is nothing new (a
    /// landed change not yet on `main`).
    async fn outcome(
        &self,
        change: &str,
        entry: &proto::QueueEntry,
    ) -> Result<Option<Outcome>, SyncError> {
        let parked = |key, status: String| Outcome {
            key,
            state: StatusState::Failure,
            comment: Some(parked_comment(&status, entry)),
            status,
            close: false,
        };
        let outcome = match entry.status() {
            QueueStatus::Unspecified => return Ok(None),
            QueueStatus::Queued => Outcome::pending("queued", "Queued in the hord lander"),
            QueueStatus::Replaying => {
                Outcome::pending("replaying", "The hord lander is replaying it")
            }
            QueueStatus::Landed | QueueStatus::Replayed | QueueStatus::Arbitrated => {
                let landed = entry.landed.as_deref().unwrap_or(change);
                if !self.on_main(wire::object_id("landed", landed)?).await? {
                    return Ok(None);
                }
                let how = match entry.status() {
                    QueueStatus::Replayed => " after a replay",
                    QueueStatus::Arbitrated => " after arbitration",
                    _ => "",
                };
                Outcome {
                    key: "landed",
                    state: StatusState::Success,
                    status: format!("Landed on main as {}", short(landed)),
                    comment: Some(format!(
                        "Landed{how} as hord change `{landed}`, now on `main`. The pull \
                         request is closed: `main` carries the change as one commit."
                    )),
                    close: true,
                }
            }
            QueueStatus::Conflicted => parked(
                "conflicted",
                "Parked: it conflicts with changes landed since its base".into(),
            ),
            QueueStatus::NeedsArbitration => {
                parked("needs_arbitration", "Parked for arbitration".into())
            }
            QueueStatus::Parked => Outcome {
                key: "parked",
                state: StatusState::Pending,
                status: "Parked: head's policy requires evidence it lacks".into(),
                comment: Some(parked_comment(
                    "Parked until it has the evidence head's policy requires",
                    entry,
                )),
                close: false,
            },
            QueueStatus::Rejected => {
                let reason = entry.reason.clone().unwrap_or_default();
                Outcome {
                    key: "rejected",
                    state: StatusState::Failure,
                    status: format!("Rejected: {reason}"),
                    comment: Some(format!("The hord lander rejected this change: {reason}")),
                    close: false,
                }
            }
        };
        Ok(Some(outcome))
    }
}

/// What to tell a pull request.
struct Outcome {
    key: &'static str,
    state: StatusState,
    status: String,
    comment: Option<String>,
    close: bool,
}

impl Outcome {
    fn pending(key: &'static str, status: &str) -> Self {
        Self {
            key,
            state: StatusState::Pending,
            status: status.to_owned(),
            comment: None,
            close: false,
        }
    }
}

/// A comment for a parked change: the lander's conflict summary.
fn parked_comment(headline: &str, entry: &proto::QueueEntry) -> String {
    let mut text = format!("**{headline}.**\n\n");
    let summary = entry
        .escalation
        .as_ref()
        .and_then(|e| e.summary.as_ref())
        .map(|s| s.text.trim())
        .filter(|s| !s.is_empty());
    match summary {
        Some(summary) => {
            let _ = writeln!(text, "```\n{summary}\n```");
        }
        None => text.push_str(&report_text(entry.report.as_ref())),
    }
    text.push_str(
        "\nPush a new commit to this pull request to supersede the proposal, or resolve it \
         with `hord arbitrate`.\n",
    );
    text
}

/// A conflict report in Markdown, one line per finding.
fn report_text(report: Option<&proto::ConflictReport>) -> String {
    let Some(report) = report else {
        return "The lander gave no report.\n".into();
    };
    let mut text = String::new();
    for conflict in &report.conflicts {
        let kind = match conflict.kind() {
            proto::ConflictKind::WriteWrite => "both changed",
            proto::ConflictKind::ReadWrite => "it read what a landed change wrote",
            proto::ConflictKind::WriteRead => "it wrote what a landed change read",
            proto::ConflictKind::Unspecified => "overlap",
        };
        let mut what: Vec<String> = conflict.paths.iter().map(|p| format!("`{p}`")).collect();
        what.extend(conflict.nodes.iter().map(|n| {
            let name = n.name.as_deref().unwrap_or(&n.id);
            format!("`{name}`")
        }));
        let _ = writeln!(
            text,
            "- {kind} {} with landed change `{}` ({})",
            what.join(", "),
            short(&conflict.landed),
            conflict.landed_summary
        );
    }
    for merge in &report.merge {
        let _ = writeln!(text, "- `{}`: {}", merge.path, merge.reason);
    }
    if let Some(failure) = &report.verification {
        let _ = writeln!(text, "- verification failed: {failure}");
    }
    for violation in &report.policy {
        let _ = writeln!(
            text,
            "- policy requires {} ({})",
            violation.requirement, violation.evidence
        );
    }
    if text.is_empty() {
        text.push_str("The lander found no specific conflict.\n");
    }
    text
}

/// Errors that belong on the pull request rather than in the daemon's log:
/// the pull request, not the connection, is at fault.
fn reportable(err: &SyncError) -> bool {
    match err {
        SyncError::Unproposable(_) | SyncError::Git(_) => true,
        SyncError::Api(api) => matches!(
            api,
            ApiError::PermissionDenied(_)
                | ApiError::InvalidArgument(_)
                | ApiError::FailedPrecondition(_)
                | ApiError::NotFound(_)
        ),
        _ => false,
    }
}

/// GitHub limits a status description to 140 characters.
fn clip(text: &str) -> String {
    const MAX: usize = 140;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(MAX - 1).collect();
    out.push('…');
    out
}

fn short(id: &str) -> &str {
    &id[..12.min(id.len())]
}

fn warn(what: &str, step: Result<(), SyncError>) {
    if let Err(err) = step {
        eprintln!("hord git sync: {what}: {err}");
    }
}

async fn next_event(
    events: &mut Option<EventStream>,
) -> Option<Result<proto::EventEnvelope, ApiError>> {
    match events {
        Some(stream) => stream.next().await,
        None => None,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SyncError> + Send + 'static,
) -> Result<T, SyncError> {
    spawn_blocking(f)
        .await
        .map_err(|err| SyncError::Git(Error::Git(format!("background task failed: {err}"))))?
}

/// Whether `ancestor` is `descendant` or reachable from it in `git_dir`.
fn is_ancestor(git_dir: &Path, ancestor: gix::ObjectId, descendant: gix::ObjectId) -> bool {
    if ancestor == descendant {
        return true;
    }
    let Ok(repo) = open_repo(git_dir) else {
        return false;
    };
    let Ok(walk) = repo.rev_walk([descendant]).all() else {
        return false;
    };
    walk.filter_map(Result::ok).any(|info| info.id == ancestor)
}

/// The landed change a pull request's head is based on: its nearest
/// ancestors that are exports of changes, and of those the one the others
/// lead to (a pull request that merged `main` in is based on the newer
/// `main`).
fn find_base(git_dir: &Path, head: gix::ObjectId) -> Result<Option<ChangeId>, SyncError> {
    let repo = open_repo(git_dir)?;
    let mut exported: HashMap<gix::ObjectId, ChangeId> = HashMap::new();
    let refs = repo.references().map_err(Error::git)?;
    for reference in refs.prefixed(CHANGE_REFS).map_err(Error::git)? {
        let reference = reference.map_err(Error::git)?;
        let name = reference.name().as_bstr().to_string();
        let Some(id) = name.strip_prefix(CHANGE_REFS) else {
            continue;
        };
        let Ok(change) = id.parse::<ChangeId>() else {
            continue;
        };
        if let Some(commit) = reference.try_id() {
            exported.insert(commit.detach(), change);
        }
    }
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([head]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        if exported.contains_key(&id) {
            hits.push(id);
            continue;
        }
        let commit = repo.find_commit(id).map_err(Error::git)?;
        queue.extend(commit.parent_ids().map(|p| p.detach()));
    }
    let base = hits
        .iter()
        .copied()
        .find(|h| hits.iter().all(|other| is_ancestor(git_dir, *other, *h)));
    Ok(base.and_then(|commit| exported.get(&commit).copied()))
}
