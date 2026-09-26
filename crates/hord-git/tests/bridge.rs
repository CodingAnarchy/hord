//! The git bridge (spec §9 Sync, ADR 0036) against a local repository with
//! its lander running, a bare git repository as the mirror, and a scripted
//! pull request host. No network.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gix::bstr::{BString, ByteSlice};
use gix::refs::transaction::PreviousValue;
use hord_api::proto::event::Kind;
use hord_api::proto::{self, BridgeCheckTrigger, QueueStatus};
use hord_api::{RepoBackend, wire};
use hord_core::{Actor, ChangeId, ChangeRecord, Intent, IntentRef, RepoPath};
use hord_git::sync::{
    Bridge, BridgeOptions, PullRequest, PullRequests, Reported, ScriptedPulls, StatusState,
};
use hord_git::{ExportCache, MemoryStore, Store, git_tree_sha, import_git, import_git_window};
use hord_txn::{BeginOptions, LocalRepo, Repo, RepoOptions, StubVerifier};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};
use tokio_stream::StreamExt;

use common::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const NOTES: &str = "alpha\nbeta\ngamma\n";

/// A repository with its lander, a bare mirror, and scripted pulls.
struct Setup {
    dir: TempDir,
    repo: Repo,
    backend: Arc<LocalRepo>,
    mirror: PathBuf,
    pulls: Arc<ScriptedPulls>,
}

impl Setup {
    async fn new(tag: &str) -> TestResult<Self> {
        let dir = TempDir::new(tag)?;
        let options = RepoOptions {
            verifier: Some(Arc::new(StubVerifier)),
            ..RepoOptions::default()
        };
        let repo = Repo::create_with(dir.0.join("repo"), options).await?;
        repo.bootstrap(
            vec![
                (path("README.md")?, b"hello\n".to_vec()),
                (path("notes.txt")?, NOTES.as_bytes().to_vec()),
            ],
            Intent::from_summary("seed"),
            Actor::Human {
                id: "Seed <seed@example.com>".into(),
            },
        )
        .await?;
        let backend = Arc::new(LocalRepo::new(repo.clone())?);
        let mirror = dir.0.join("mirror.git");
        gix::init_bare(&mirror)?;
        Ok(Self {
            dir,
            repo,
            backend,
            mirror,
            pulls: Arc::new(ScriptedPulls::new()),
        })
    }

    fn options(&self) -> BridgeOptions {
        BridgeOptions::new(self.mirror.display().to_string(), self.dir.0.join("bridge"))
    }

    async fn bridge(&self) -> TestResult<Bridge> {
        let backend: Arc<dyn RepoBackend> = self.backend.clone();
        let pulls: Arc<dyn PullRequests> = self.pulls.clone();
        Ok(Bridge::open(backend, Some(pulls), self.options()).await?)
    }

    /// Land a change that writes `bytes` to `file`, through the lander.
    async fn land(&self, summary: &str, file: &str, bytes: &str) -> TestResult<ChangeId> {
        let actor = Actor::Human {
            id: "Hal <hal@example.com>".into(),
        };
        let mut ws = self.repo.begin(BeginOptions::at_head(actor)).await?;
        ws.write_file(&path(file)?, bytes.as_bytes()).await?;
        let proposal = ws.propose(Intent::from_summary(summary)).await?;
        self.backend
            .submit(proto::SubmitRequest {
                change: wire::id(proposal.change),
            })
            .await?;
        let entry = self.settle(&wire::id(proposal.change)).await?;
        assert_eq!(entry.status(), QueueStatus::Landed, "{entry:?}");
        let landed = entry.landed.ok_or("a landed entry names its change")?;
        Ok(wire::object_id("landed", &landed)?)
    }

    /// Wait until the lander has decided on `change`.
    async fn settle(&self, change: &str) -> TestResult<proto::QueueEntry> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let queue = self
                .backend
                .queue(proto::QueueQuery {
                    change: Some(change.to_owned()),
                    ..Default::default()
                })
                .await?;
            if let Some(entry) = queue.entries.last()
                && !matches!(entry.status(), QueueStatus::Queued | QueueStatus::Replaying)
            {
                return Ok(entry.clone());
            }
            if Instant::now() > deadline {
                return Err(format!("{change} did not settle").into());
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// Landed changes in log order.
    fn log(&self) -> TestResult<Vec<ChangeId>> {
        Ok(Store::log(self.repo.store())?)
    }

    fn mirror_repo(&self) -> TestResult<gix::Repository> {
        Ok(gix::open(&self.mirror)?)
    }

    /// `main` on the mirror, if any.
    fn main(&self) -> TestResult<Option<gix::ObjectId>> {
        let repo = self.mirror_repo()?;
        Ok(repo
            .try_find_reference("refs/heads/main")?
            .map(|r| r.id().detach()))
    }

    /// `main`'s first-parent chain on the mirror, oldest first: each
    /// commit's id, tree, message, and author name.
    fn chain(&self) -> TestResult<Vec<(gix::ObjectId, gix::ObjectId, String, String)>> {
        let repo = self.mirror_repo()?;
        let mut out = Vec::new();
        let mut next = self.main()?;
        while let Some(id) = next {
            let commit = repo.find_commit(id)?;
            let message = commit.message_raw()?.to_str_lossy().into_owned();
            let author = commit.author()?.name.to_str_lossy().into_owned();
            out.push((id, commit.tree_id()?.detach(), message, author));
            next = commit.parent_ids().next().map(|p| p.detach());
        }
        out.reverse();
        Ok(out)
    }

    /// Write a commit on the mirror whose parent is `parent` and whose tree
    /// is the parent's with `file` set to `bytes`, and point `git_ref` at it.
    fn push_commit(
        &self,
        parent: gix::ObjectId,
        file: &str,
        bytes: &str,
        message: &str,
        git_ref: &str,
    ) -> TestResult<gix::ObjectId> {
        let repo = self.mirror_repo()?;
        let parent_tree = repo.find_commit(parent)?.tree_id()?.detach();
        let mut tree: gix::objs::Tree = repo.find_tree(parent_tree)?.decode()?.into();
        let blob = repo.write_blob(bytes.as_bytes())?.detach();
        let entry = tree
            .entries
            .iter_mut()
            .find(|e| e.filename == file)
            .ok_or("the file is in the parent's tree")?;
        entry.oid = blob;
        let tree = repo.write_object(&tree)?.detach();
        let author = gix::actor::Signature {
            name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            time: gix::date::Time::new(1_800_000_000, 0),
        };
        let commit = gix::objs::Commit {
            tree,
            parents: [parent].into_iter().collect(),
            author: author.clone(),
            committer: author,
            encoding: None,
            message: BString::from(message),
            extra_headers: Vec::new(),
        };
        let id = repo.write_object(&commit)?.detach();
        repo.reference(git_ref, id, PreviousValue::Any, "test")?;
        Ok(id)
    }

    fn pull(&self, number: u64, head: gix::ObjectId, title: &str) -> PullRequest {
        PullRequest {
            number,
            title: title.into(),
            body: "Because the notes were wrong.".into(),
            head_sha: head.to_string(),
            git_ref: format!("refs/pull/{number}/head"),
            url: format!("https://example.com/pulls/{number}"),
        }
    }

    /// Every `BridgeChecked` event recorded so far.
    async fn checks(&self) -> TestResult<Vec<proto::BridgeChecked>> {
        let mut stream = self
            .backend
            .events(proto::EventsRequest { from: Some(0) })
            .await?;
        let mut out = Vec::new();
        // Recorded events come first; stop at the first quiet pause.
        while let Ok(Some(item)) = timeout(Duration::from_millis(300), stream.next()).await {
            if let Some(Kind::BridgeChecked(check)) = item?.event.and_then(|e| e.kind) {
                out.push(check);
            }
        }
        Ok(out)
    }
}

fn path(p: &str) -> TestResult<RepoPath> {
    Ok(p.parse::<RepoPath>()?)
}

/// The git tree the projection of `change`'s result is.
fn projected_tree(repo: &Repo, change: ChangeId) -> TestResult<String> {
    let record: ChangeRecord = Store::get_object(repo.store(), change)?;
    let sha = git_tree_sha(
        repo.store(),
        record.result,
        gix::hash::Kind::Sha1,
        &ExportCache::default(),
    )?;
    Ok(sha.to_hex())
}

fn trailer(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    message
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(str::to_owned))
}

/// Landed changes reach `main` in log order, one commit each, with the
/// `Hord-*` trailers, and each tree is the change's projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landed_changes_reach_main_in_order_with_trailers() -> TestResult {
    let t = Setup::new("export").await?;
    t.land("tidy the readme", "README.md", "hello, hord\n")
        .await?;
    t.land("more notes", "notes.txt", "alpha\nbeta\ngamma\ndelta\n")
        .await?;
    let mut bridge = t.bridge().await?;
    let report = bridge.sync_once().await?;
    let log = t.log()?;
    assert_eq!(report.exported, log);

    let chain = t.chain()?;
    assert_eq!(chain.len(), log.len(), "one commit per landed change");
    for ((_, tree, message, _), change) in chain.iter().zip(&log) {
        let record: ChangeRecord = Store::get_object(t.repo.store(), *change)?;
        assert_eq!(
            trailer(message, "Hord-Change"),
            Some(change.to_string()),
            "{message}"
        );
        assert_eq!(
            trailer(message, "Hord-Intent"),
            Some(record.intent.summary.clone())
        );
        assert_eq!(
            trailer(message, "Hord-Actor").as_deref(),
            Some(record.provenance.actor.id())
        );
        assert_eq!(tree.to_string(), projected_tree(&t.repo, *change)?);
    }

    // The push was checked, and the check is on the event stream.
    let check = report.checks.last().ok_or("a check after the push")?;
    assert!(!check.diverged, "{check:?}");
    assert!(check.recorded.is_some());
    let recorded = t.checks().await?;
    let last = recorded.last().ok_or("a BridgeChecked event")?;
    assert!(!last.diverged);
    assert_eq!(last.trigger(), BridgeCheckTrigger::Push);
    assert_eq!(last.actual, t.main()?.map(|m| m.to_string()));
    assert_eq!(last.head, log.last().map(|c| wire::id(*c)));
    Ok(())
}

/// A restarted bridge exports only what landed since, and the daemon
/// follows the event stream to push each landing as it happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_resumes_without_duplicates() -> TestResult {
    let t = Setup::new("resume").await?;
    t.land("first", "README.md", "one\n").await?;
    let mut bridge = t.bridge().await?;
    bridge.sync_once().await?;
    drop(bridge);

    let second = t.land("second", "README.md", "two\n").await?;
    let mut bridge = t.bridge().await?;
    let report = bridge.sync_once().await?;
    assert_eq!(report.exported, vec![second], "only what landed since");
    let again = bridge.sync_once().await?;
    assert!(again.exported.is_empty() && again.pushed.is_none());
    drop(bridge);

    // The daemon: restarted again, it resumes from its saved cursor and
    // pushes a new landing on its `Landed` event (polling is off).
    let mut options = t.options();
    options.poll = Duration::from_secs(3600);
    let backend: Arc<dyn RepoBackend> = t.backend.clone();
    let mut daemon = Bridge::open(backend, None, options).await?;
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        daemon
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    let third = t.land("third", "README.md", "three\n").await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let chain = t.chain()?;
        if chain
            .last()
            .is_some_and(|(_, _, m, _)| trailer(m, "Hord-Change") == Some(third.to_string()))
        {
            break;
        }
        if Instant::now() > deadline {
            return Err("the daemon did not push the landing".into());
        }
        sleep(Duration::from_millis(50)).await;
    }
    let _ = stop.send(());
    task.await??;

    let log = t.log()?;
    let chain = t.chain()?;
    let ids: Vec<String> = chain
        .iter()
        .filter_map(|(_, _, m, _)| trailer(m, "Hord-Change"))
        .collect();
    let unique: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "no change twice: {ids:?}");
    assert_eq!(
        ids,
        log.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "every landed change once, in log order"
    );
    Ok(())
}

/// A pull request becomes a proposal by its git author, lands, is reported
/// as landed, and is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_request_lands_reports_and_closes() -> TestResult {
    let t = Setup::new("pull").await?;
    let mut bridge = t.bridge().await?;
    bridge.sync_once().await?;
    let main = t.main()?.ok_or("main after the first sync")?;
    let fixed = "alpha\nbeta (fixed)\ngamma\n";
    let head = t.push_commit(main, "notes.txt", fixed, "fix beta\n", "refs/pull/7/head")?;
    t.pulls.open(t.pull(7, head, "Fix the notes"));

    let report = bridge.sync_once().await?;
    let (number, change) = *report.proposed.first().ok_or("the pull was proposed")?;
    assert_eq!(number, 7);
    let record: ChangeRecord = Store::get_object(t.repo.store(), change)?;
    assert_eq!(record.intent.summary, "Fix the notes");
    assert_eq!(record.intent.body, "Because the notes were wrong.");
    assert_eq!(
        record.provenance.actor,
        Actor::Human {
            id: "Ada Lovelace <ada@example.com>".into()
        }
    );
    let entry = t.settle(&wire::id(change)).await?;
    assert_eq!(entry.status(), QueueStatus::Landed, "{entry:?}");

    bridge.sync_once().await?;
    assert!(!t.pulls.is_open(7), "closed once landed");
    let reported = t.pulls.reported_on(7);
    let states: Vec<StatusState> = reported
        .iter()
        .filter_map(|r| match r {
            Reported::Status { state, sha, .. } => {
                assert_eq!(*sha, head.to_string(), "statuses go on the head");
                Some(*state)
            }
            _ => None,
        })
        .collect();
    assert_eq!(states, vec![StatusState::Pending, StatusState::Success]);
    assert!(
        reported
            .iter()
            .any(|r| matches!(r, Reported::Comment { body, .. } if body.contains("Landed"))),
        "{reported:?}"
    );
    assert_eq!(reported.last(), Some(&Reported::Closed { pull: 7 }));

    // `main` ends with the pull request's change, as its author wrote it.
    let chain = t.chain()?;
    let (_, tree, message, author) = chain.last().ok_or("main has commits")?;
    assert_eq!(author, "Ada Lovelace");
    assert_eq!(
        trailer(message, "Hord-Actor").as_deref(),
        Some("Ada Lovelace <ada@example.com>")
    );
    let pr_tree = t.mirror_repo()?.find_commit(head)?.tree_id()?.detach();
    assert_eq!(*tree, pr_tree, "the landed tree is the pull request's");

    // Nothing more to say about it.
    let quiet = bridge.sync_once().await?;
    assert!(quiet.reported.is_empty() && quiet.proposed.is_empty());
    Ok(())
}

/// A pull request that conflicts with a change landed since its base parks,
/// and its comment carries the conflict summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_pull_request_parks_with_the_summary() -> TestResult {
    let t = Setup::new("park").await?;
    let mut bridge = t.bridge().await?;
    bridge.sync_once().await?;
    let main = t.main()?.ok_or("main after the first sync")?;
    let theirs = "alpha\nbeta from the pull request\ngamma\n";
    let head = t.push_commit(
        main,
        "notes.txt",
        theirs,
        "reword beta\n",
        "refs/pull/9/head",
    )?;
    t.land(
        "reword beta in hord",
        "notes.txt",
        "alpha\nbeta from hord\ngamma\n",
    )
    .await?;
    t.pulls.open(t.pull(9, head, "Reword beta"));

    let report = bridge.sync_once().await?;
    let (_, change) = *report.proposed.first().ok_or("the pull was proposed")?;
    let entry = t.settle(&wire::id(change)).await?;
    assert!(
        matches!(
            entry.status(),
            QueueStatus::Conflicted | QueueStatus::NeedsArbitration
        ),
        "{entry:?}"
    );
    bridge.sync_once().await?;

    let reported = t.pulls.reported_on(9);
    assert!(
        reported.iter().any(|r| matches!(
            r,
            Reported::Status {
                state: StatusState::Failure,
                ..
            }
        )),
        "{reported:?}"
    );
    let comment = reported
        .iter()
        .find_map(|r| match r {
            Reported::Comment { body, .. } => Some(body.clone()),
            _ => None,
        })
        .ok_or("a comment on the parked pull request")?;
    assert!(comment.contains("notes.txt"), "{comment}");
    assert!(comment.contains("Parked"), "{comment}");
    assert!(t.pulls.is_open(9), "a parked pull request stays open");
    Ok(())
}

/// A manual push to `main` is divergence: `check` reports it (as an event,
/// too) and does not repair it; `repair` restores the export.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manual_push_to_main_is_divergence_until_repaired() -> TestResult {
    let t = Setup::new("diverge").await?;
    t.land("first", "README.md", "one\n").await?;
    let mut bridge = t.bridge().await?;
    bridge.sync_once().await?;
    let exported = t.main()?.ok_or("main after the first sync")?;

    let manual = t.push_commit(
        exported,
        "README.md",
        "by hand\n",
        "manual\n",
        "refs/heads/main",
    )?;
    let check = bridge.check(BridgeCheckTrigger::Check).await?;
    assert!(check.diverged, "{check:?}");
    assert_eq!(check.actual, Some(manual.to_string()));
    assert_eq!(check.expected, Some(exported.to_string()));
    assert_eq!(t.main()?, Some(manual), "a check never repairs");
    let recorded = t.checks().await?;
    let last = recorded.last().ok_or("a BridgeChecked event")?;
    assert!(last.diverged);
    assert_eq!(last.trigger(), BridgeCheckTrigger::Check);

    // The next landing cannot fast-forward over the manual commit.
    t.land("second", "README.md", "two\n").await?;
    let report = bridge.sync_once().await?;
    assert!(report.pushed.is_none());
    assert!(report.checks.last().is_some_and(|c| c.diverged));

    let repaired = bridge.repair().await?;
    assert!(!repaired.diverged, "{repaired:?}");
    assert_eq!(t.main()?.map(|m| m.to_string()), repaired.expected);
    let chain = t.chain()?;
    assert!(chain.iter().all(|(id, ..)| *id != manual));
    assert_eq!(chain.len(), t.log()?.len());
    let recorded = t.checks().await?;
    assert_eq!(
        recorded.last().map(|c| c.trigger()),
        Some(BridgeCheckTrigger::Repair)
    );
    Ok(())
}

/// Where this repository's git directory is, for the round trip below.
fn own_repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Spec §9: `export(import(repo))` reproduces every tree SHA, on hord's own
/// history.
#[test]
fn the_round_trip_reproduces_every_tree_sha_of_hord_itself() -> TestResult {
    let root = own_repository();
    let repo = gix::discover(&root)?;
    let mut store = MemoryStore::new();
    let commits = repo
        .rev_walk([repo.head_id()?.detach()])
        .all()?
        .filter_map(Result::ok)
        .count();
    let head = if repo.is_shallow() {
        // A shallow CI checkout: the commits it has.
        import_git_window(&mut store, &root, "HEAD", commits)?
    } else {
        import_git(&mut store, &root)?
    };
    assert!(Store::head(&store)? == Some(head));
    let cache = ExportCache::default();
    let mut checked = 0;
    for change in Store::log(&store)? {
        let record: ChangeRecord = store.get_object(change)?;
        let Some(sha) = record.intent.refs.iter().find_map(|r| match r {
            IntentRef::GitCommit { sha } => Some(sha.clone()),
            _ => None,
        }) else {
            return Err(format!("{change} names no git commit").into());
        };
        let commit = repo.find_commit(gix::ObjectId::from_hex(sha.as_bytes())?)?;
        let expected = commit.tree_id()?.detach().to_string();
        let actual = git_tree_sha(&store, record.result, repo.object_hash(), &cache)?.to_hex();
        assert_eq!(actual, expected, "tree of {sha}");
        checked += 1;
    }
    assert!(checked > 1, "hord's history has commits");
    assert_eq!(checked, commits, "every commit of HEAD's history");
    Ok(())
}
