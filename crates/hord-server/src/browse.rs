//! Views 4–6 of the `Changes` service (spec §10.4, M6) over a local
//! repository: a definition's lineage, a change's provenance trace, and
//! the repository browser (tree, file projection, and a definition's
//! edges).
//!
//! Each answer is assembled from what the other read paths already
//! compute: the store's `node_history` (as `hord blame`), `GetChange`'s
//! view, and `hord-txn`'s [`Query`](hord_txn::Query) over snapshots.

use std::collections::BTreeSet;

use hord_api::proto::event::Kind;
use hord_api::{ApiError, ApiResult, ChangesBackend, RepoBackend, proto, wire};
use hord_core::{ChangeRecord, IdentityDelta, NodeId, RepoPath, SnapshotId};
use hord_txn::{DefinitionInfo, EdgeKind, Repo};
use hord_ui::present::attempt_outcome;
use hord_ui::view::{actor_label, evidence_kind_label, evidence_result_label, short_id};
use hord_verify::line_range;

use crate::changes::{LocalChanges, Names, names_any, op_view};

impl LocalChanges {
    /// The snapshot a request names, or head's result when it names none.
    async fn snapshot(&self, text: &str) -> ApiResult<SnapshotId> {
        if text.is_empty() {
            Ok(self.repo().head().await?.snapshot)
        } else {
            wire::object_id("snapshot", text)
        }
    }

    pub(crate) async fn lineage(
        &self,
        request: proto::NodeLineageRequest,
    ) -> ApiResult<proto::NodeLineageResponse> {
        let node = wire::node_id("node", &request.node)?;
        let history = self
            .local
            .node_history(proto::NodeHistoryRequest {
                node: request.node.clone(),
            })
            .await?
            .changes;
        let repo = self.repo();
        let mut entries = Vec::with_capacity(history.len());
        for summary in history {
            let record = self
                .change(wire::object_id("change", &summary.change)?)
                .await?;
            entries.push(lineage_entry(self, repo, node, summary, &record).await?);
        }
        let head = repo.head().await?.snapshot;
        let at_head = repo
            .query()
            .locate(head, BTreeSet::from([node]))
            .await?
            .remove(&node);
        let live = at_head.is_some();
        let placed = at_head.map(|d| definition_ref(&d)).or_else(|| {
            entries
                .iter()
                .rev()
                .find_map(|e| e.after.clone().or_else(|| e.before.clone()))
        });
        if entries.is_empty() && placed.is_none() {
            return Err(ApiError::NotFound(format!(
                "node {node}: no landed change touched it and head does not have it"
            )));
        }
        Ok(proto::NodeLineageResponse {
            node: Some(placed.unwrap_or_else(|| wire::node_ref(node))),
            live,
            entries,
        })
    }

    pub(crate) async fn trace(
        &self,
        request: proto::ChangeTraceRequest,
    ) -> ApiResult<proto::ChangeTraceResponse> {
        let submitted = self
            .get_change(proto::GetChangeRequest {
                change: request.change.clone(),
            })
            .await?;
        let entry = submitted.queue.as_ref();
        let landed = match entry.and_then(|q| q.landed.as_deref()) {
            Some(landed) if landed != submitted.change => Some(
                self.get_change(proto::GetChangeRequest {
                    change: landed.to_owned(),
                })
                .await?,
            ),
            _ => None,
        };
        let mut replays = Vec::new();
        for attempt in entry
            .and_then(|q| q.escalation.as_ref())
            .map(|e| e.attempts.as_slice())
            .unwrap_or_default()
        {
            let Some(change) = &attempt.change else {
                continue;
            };
            match self
                .get_change(proto::GetChangeRequest {
                    change: change.clone(),
                })
                .await
            {
                Ok(view) => replays.push(view),
                // A proposal the harness reported but never stored is in
                // the attempt's detail already.
                Err(ApiError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let steps = trace_steps(&submitted, landed.as_ref());
        Ok(proto::ChangeTraceResponse {
            change: request.change,
            submitted: Some(submitted),
            landed,
            replays,
            steps,
        })
    }

    pub(crate) async fn tree(
        &self,
        request: proto::ListTreeRequest,
    ) -> ApiResult<proto::ListTreeResponse> {
        let snapshot = self.snapshot(&request.snapshot).await?;
        let dir = path_arg(&request.path)?;
        let files = self
            .repo()
            .query()
            .files_under(snapshot, dir.clone())
            .await?;
        if files.len() == 1 && files[0].0 == dir && !dir.is_root() {
            return Err(ApiError::InvalidArgument(format!(
                "{dir} is a file, not a directory"
            )));
        }
        if files.is_empty() && !dir.is_root() {
            return Err(ApiError::NotFound(format!("directory {dir}")));
        }
        let depth = dir.components().len();
        let mut dirs = BTreeSet::new();
        let mut entries = Vec::new();
        for (path, blob) in &files {
            let parts = path.components();
            let Some(name) = parts.get(depth) else {
                continue;
            };
            if parts.len() > depth + 1 {
                dirs.insert(name.clone());
            } else {
                entries.push(proto::TreeItem {
                    name: name.clone(),
                    path: path.to_string(),
                    dir: false,
                    blob: Some(wire::id(*blob)),
                });
            }
        }
        let mut out: Vec<proto::TreeItem> = dirs
            .into_iter()
            .map(|name| {
                let mut parts = dir.components().to_vec();
                parts.push(name.clone());
                proto::TreeItem {
                    name,
                    path: RepoPath::new(parts).to_string(),
                    dir: true,
                    blob: None,
                }
            })
            .collect();
        out.extend(entries);
        Ok(proto::ListTreeResponse {
            snapshot: wire::id(snapshot),
            path: dir.to_string(),
            entries: out,
        })
    }

    pub(crate) async fn file(
        &self,
        request: proto::GetFileRequest,
    ) -> ApiResult<proto::GetFileResponse> {
        let snapshot = self.snapshot(&request.snapshot).await?;
        let path = wire::repo_path("path", &request.path)?;
        let repo = self.repo();
        let blob = repo
            .query()
            .files_under(snapshot, path.clone())
            .await?
            .into_iter()
            .find(|(p, _)| *p == path)
            .map(|(_, blob)| blob)
            .ok_or_else(|| ApiError::NotFound(format!("file {path}")))?;
        let bytes = repo
            .file_bytes(snapshot, path.clone())
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("file {path}")))?;
        let bytes = bytes.as_slice();
        let definitions = repo
            .definitions_in(snapshot, path.clone())
            .await?
            .into_iter()
            .map(|def| {
                let lines = line_range(bytes, &def.span);
                proto::FileDefinition {
                    kind: def.kind.as_str().to_owned(),
                    start_line: lines.start,
                    end_line: lines.end.saturating_sub(1),
                    parent: def.parent.map(|p| p.to_string()),
                    node: Some(definition_ref(&def)),
                }
            })
            .collect();
        let (text, binary) = match str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), false),
            Err(_) => (String::new(), true),
        };
        Ok(proto::GetFileResponse {
            snapshot: wire::id(snapshot),
            path: path.to_string(),
            blob: wire::id(blob),
            text,
            binary,
            size: bytes.len() as u64,
            definitions,
        })
    }

    pub(crate) async fn neighbourhood(
        &self,
        request: proto::NodeEdgesRequest,
    ) -> ApiResult<proto::NodeEdgesResponse> {
        let snapshot = self.snapshot(&request.snapshot).await?;
        let node = wire::node_id("node", &request.node)?;
        let query = self.repo().query();
        let Some(def) = query
            .locate(snapshot, BTreeSet::from([node]))
            .await?
            .remove(&node)
        else {
            return Err(ApiError::NotFound(format!(
                "node {node} in snapshot {snapshot}"
            )));
        };
        let references = query.edges(snapshot, node, EdgeKind::References).await?;
        let contains = query.edges(snapshot, node, EdgeKind::Contains).await?;
        let referenced_by = query.referenced_by(snapshot, node).await?;
        let tests = query.test_edges(snapshot, node).await?;
        let mut all: BTreeSet<NodeId> = BTreeSet::new();
        all.extend(&references);
        all.extend(&contains);
        all.extend(&referenced_by);
        all.extend(&tests.tested_by);
        all.extend(&tests.tests);
        let known = query.locate(snapshot, all).await?;
        let refs = |ids: &mut dyn Iterator<Item = NodeId>| -> Vec<proto::NodeRef> {
            ids.map(|id| {
                known
                    .get(&id)
                    .map_or_else(|| wire::node_ref(id), definition_ref)
            })
            .collect()
        };
        Ok(proto::NodeEdgesResponse {
            snapshot: wire::id(snapshot),
            node: Some(definition_ref(&def)),
            kind: Some(def.kind.as_str().to_owned()),
            references: refs(&mut references.into_iter()),
            referenced_by: refs(&mut referenced_by.into_iter()),
            tested_by: refs(&mut tests.tested_by.into_iter()),
            tests: refs(&mut tests.tests.into_iter()),
            contains: refs(&mut contains.into_iter()),
            covered_by_tests: tests.unmapped.into_iter().collect(),
        })
    }
}

/// An empty path is the root.
fn path_arg(text: &str) -> ApiResult<RepoPath> {
    let text = text.trim_matches('/');
    if text.is_empty() {
        Ok(RepoPath::default())
    } else {
        wire::repo_path("path", text)
    }
}

fn definition_ref(def: &DefinitionInfo) -> proto::NodeRef {
    proto::NodeRef {
        id: def.node.to_string(),
        name: def.name.as_ref().map(|n| n.as_str().to_owned()),
        path: Some(def.path.to_string()),
    }
}

/// `node` in `snapshot`, looked up in `paths` (the files a change
/// touched: a definition a change touches is in one of them).
async fn locate_in(
    repo: &Repo,
    snapshot: SnapshotId,
    paths: &[RepoPath],
    node: NodeId,
) -> ApiResult<Option<DefinitionInfo>> {
    for path in paths {
        if let Some(def) = repo
            .definitions_in(snapshot, path.clone())
            .await?
            .into_iter()
            .find(|d| d.node == node)
        {
            return Ok(Some(def));
        }
    }
    Ok(None)
}

async fn lineage_entry(
    changes: &LocalChanges,
    repo: &Repo,
    node: NodeId,
    summary: proto::ChangeSummary,
    record: &ChangeRecord,
) -> ApiResult<proto::LineageEntry> {
    let paths = repo.changed_paths(record.base, record.result).await?;
    let before = locate_in(repo, record.base, &paths, node).await?;
    let after = locate_in(repo, record.result, &paths, node).await?;
    let names = Names::for_records(repo, &[record]).await?;
    let ops = record
        .ops
        .iter()
        .filter(|op| op.node_ids().any(|n| n == node))
        .map(|op| op_view(op, &names))
        .collect();
    let identity = record
        .identity_deltas
        .iter()
        .filter(|d| d.node_ids().any(|n| n == node))
        .map(|d| identity_view(d, &names))
        .collect();
    let evidence = changes.evidence(record, None).await?;
    Ok(proto::LineageEntry {
        change: Some(summary),
        kind: after
            .as_ref()
            .or(before.as_ref())
            .map(|d| d.kind.as_str().to_owned()),
        before: before.as_ref().map(definition_ref),
        after: after.as_ref().map(definition_ref),
        ops,
        identity,
        evidence,
    })
}

fn identity_view(delta: &IdentityDelta, names: &Names) -> proto::IdentityDeltaView {
    let list = |ids: &[NodeId]| -> String {
        ids.iter()
            .map(|n| names.text(*n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (kind, node, others, text) = match delta {
        IdentityDelta::Birth { node } => ("birth", *node, Vec::new(), "born".to_owned()),
        IdentityDelta::Death { node } => ("death", *node, Vec::new(), "retired".to_owned()),
        IdentityDelta::DerivedFrom { node, from } => (
            "derived_from",
            *node,
            vec![*from],
            format!("{} derived from {}", names.text(*node), names.text(*from)),
        ),
        IdentityDelta::SplitInto { node, into } => (
            "split_into",
            *node,
            into.clone(),
            format!("{} split into {}", names.text(*node), list(into)),
        ),
        IdentityDelta::MergedFrom { node, from } => (
            "merged_from",
            *node,
            from.clone(),
            format!("{} merged from {}", names.text(*node), list(from)),
        ),
    };
    proto::IdentityDeltaView {
        kind: kind.into(),
        node: Some(names.view(node)),
        others: others.into_iter().map(|n| names.view(n)).collect(),
        text,
    }
}

fn kind_of(envelope: &proto::EventEnvelope) -> Option<&Kind> {
    envelope.event.as_ref()?.kind.as_ref()
}

/// The timeline of a change: its intent and proposal, the events that name
/// it (with each replay attempt's outcome and spend from the escalation),
/// and the evidence on its snapshots, oldest first.
fn trace_steps(
    submitted: &proto::ChangeView,
    landed: Option<&proto::ChangeView>,
) -> Vec<proto::TraceStep> {
    let mut steps = Vec::new();
    let created = submitted.provenance.as_ref().map_or(0, |p| p.created_at_ms);
    let intent = submitted.intent.clone().unwrap_or_default();
    steps.push(proto::TraceStep {
        at_ms: created,
        stage: proto::TraceStage::Intent.into(),
        text: if intent.acceptance.is_empty() {
            intent.summary.clone()
        } else {
            format!(
                "{} (acceptance: {})",
                intent.summary,
                intent.acceptance.join("; ")
            )
        },
        actor: submitted.provenance.as_ref().and_then(|p| p.actor.clone()),
        ..Default::default()
    });
    steps.push(proto::TraceStep {
        at_ms: created,
        stage: proto::TraceStage::Proposed.into(),
        text: format!(
            "proposed on base {}: {} ops, writes {}, reads {}",
            short_id(&submitted.base),
            submitted.ops.len(),
            submitted.write_set.len(),
            submitted.read_set.len()
        ),
        change: Some(submitted.change.clone()),
        ..Default::default()
    });
    let escalation = submitted.queue.as_ref().and_then(|q| q.escalation.as_ref());
    for envelope in &submitted.history {
        let mut step = proto::TraceStep {
            at_ms: envelope.at_ms,
            cursor: Some(envelope.cursor),
            ..Default::default()
        };
        match kind_of(envelope) {
            Some(Kind::Submitted(s)) => {
                step.stage = proto::TraceStage::Submitted.into();
                step.text = format!(
                    "submitted as #{} by {}",
                    s.submission,
                    actor_label(s.actor.as_ref())
                );
                step.change = Some(s.change.clone());
            }
            Some(Kind::ConflictCheck(c)) => {
                step.stage = proto::TraceStage::Conflict.into();
                let (outcome, words) = match c.result() {
                    proto::ConflictOutcome::Clean => ("pass", "clean"),
                    proto::ConflictOutcome::Overlap => ("pass", "overlap, rebased"),
                    proto::ConflictOutcome::Hard => ("fail", "hard conflict"),
                    proto::ConflictOutcome::Unspecified => ("skip", "checked"),
                };
                step.outcome = Some(outcome.into());
                step.text = format!(
                    "{words}: {} set conflicts, {} merge conflicts",
                    c.set_conflicts, c.merge_conflicts
                );
            }
            Some(Kind::Verifying(v)) => {
                step.stage = proto::TraceStage::Verify.into();
                step.change = Some(v.change.clone());
                step.text = v.plan.as_ref().map_or_else(
                    || "verification started".to_owned(),
                    |p| {
                        format!(
                            "verifying: {} ({} selected tests, {} reused)",
                            p.commands.join("; "),
                            p.selected_tests,
                            p.reused
                        )
                    },
                );
            }
            // Evidence steps come from the evidence itself, below.
            Some(Kind::EvidenceAttached(_) | Kind::HeadMoved(_)) | None => continue,
            Some(Kind::Replaying(r)) => {
                step.stage = proto::TraceStage::Replay.into();
                let attempt =
                    escalation.and_then(|e| e.attempts.iter().find(|a| a.attempt == r.attempt));
                step.text = match attempt {
                    Some(a) => {
                        let mut spent = vec![format!("{:.1}s", a.elapsed_ms as f64 / 1000.0)];
                        if let Some(tokens) = a.tokens {
                            spent.push(format!("{tokens} tokens"));
                        }
                        if let Some(micros) = a.cost_micros {
                            spent.push(format!("${:.4}", micros as f64 / 1_000_000.0));
                        }
                        if let Some(model) = &a.model {
                            spent.push(format!("model {model}"));
                        }
                        step.change = a.change.clone();
                        step.outcome = match a.outcome() {
                            proto::ReplayOutcome::Proposed => Some("pass".into()),
                            proto::ReplayOutcome::Running | proto::ReplayOutcome::Unspecified => {
                                None
                            }
                            _ => Some("fail".into()),
                        };
                        let note = a
                            .note
                            .as_ref()
                            .map(|n| format!(", with the note \"{n}\""))
                            .unwrap_or_default();
                        format!(
                            "replay #{} ({}{note}): {} [{}]",
                            r.attempt,
                            r.harness,
                            attempt_outcome(a),
                            spent.join(" · ")
                        )
                    }
                    None => format!("replay #{} ({})", r.attempt, r.harness),
                };
            }
            Some(Kind::Parked(p)) => {
                step.stage = proto::TraceStage::Parked.into();
                step.park = Some(p.reason);
                step.outcome = Some("fail".into());
                step.text = p.detail.clone();
            }
            Some(Kind::Arbitrated(a)) => {
                step.stage = proto::TraceStage::Arbitrated.into();
                step.actor = a.by.clone();
                step.change = Some(a.result.clone());
                step.text = format!(
                    "{} resolved it as {} ({})",
                    actor_label(a.by.as_ref()),
                    short_id(&a.result),
                    match (&a.key_id, &a.signature) {
                        (Some(key), Some(_)) => format!("signed with {key}"),
                        _ => "unsigned".to_owned(),
                    }
                );
            }
            Some(Kind::Landed(l)) => {
                step.stage = proto::TraceStage::Landed.into();
                step.outcome = Some("pass".into());
                step.change = Some(l.change.clone());
                step.text = format!("landed at #{} as {}", l.position, short_id(&l.change));
            }
            Some(Kind::Rejected(r)) => {
                step.stage = proto::TraceStage::Rejected.into();
                step.outcome = Some("fail".into());
                step.text = r.reason.clone();
            }
        }
        steps.push(step);
    }
    // Each step, with whether it is evidence about the record it landed as.
    let mut steps: Vec<(proto::TraceStep, bool)> = steps.into_iter().map(|s| (s, false)).collect();
    let mut seen = BTreeSet::new();
    let evidence = submitted
        .evidence
        .iter()
        .map(|ev| (ev, ev.source == "landed"))
        .chain(
            landed
                .into_iter()
                .flat_map(|l| l.evidence.iter().map(|ev| (ev, true))),
        );
    for (ev, of_landed) in evidence {
        if !seen.insert(ev.id.clone()) {
            continue;
        }
        let review = matches!(
            ev.kind.as_ref().and_then(|k| k.kind.as_ref()),
            Some(proto::evidence_kind::Kind::Review(_))
        );
        let (class, result) = evidence_result_label(ev.result.as_ref());
        steps.push((
            proto::TraceStep {
                at_ms: ev.produced_at_ms,
                stage: if review {
                    proto::TraceStage::Review
                } else {
                    proto::TraceStage::Evidence
                }
                .into(),
                text: format!(
                    "{}: {result} ({}, {})",
                    evidence_kind_label(ev.kind.as_ref(), ev.qualifier.as_deref()),
                    ev.command,
                    ev.source
                ),
                outcome: Some(class.to_owned()),
                actor: ev.produced_by.clone(),
                ..Default::default()
            },
            of_landed,
        ));
    }
    // Stable: equal times keep the order above (intent before proposal,
    // events in cursor order).
    steps.sort_by_key(|(s, _)| s.at_ms);
    decisions_first(&mut steps, &submitted.history);
    steps.into_iter().map(|(s, _)| s).collect()
}

/// Put each arbitration decision before its resolution, in causal order
/// (decision → resolution submitted → landed). The lander records
/// `Arbitrated` only once the resolution lands, so by event time the
/// landing would come first. The decision moves to the first step about
/// its resolution (an event naming it, or evidence about the record it
/// landed as) and takes that step's time: it was made no later.
fn decisions_first(steps: &mut Vec<(proto::TraceStep, bool)>, history: &[proto::EventEnvelope]) {
    let event = |cursor: Option<u64>| cursor.and_then(|c| history.iter().find(|e| e.cursor == c));
    for decision in history {
        let Some(Kind::Arbitrated(a)) = kind_of(decision) else {
            continue;
        };
        // The resolution's ids: as the arbiter gave it, and as it landed.
        let mut ids = BTreeSet::from([a.result.clone()]);
        for e in history {
            if let Some(Kind::Landed(l)) = kind_of(e)
                && (ids.contains(&l.change)
                    || l.submitted.as_ref().is_some_and(|s| ids.contains(s)))
            {
                ids.insert(l.change.clone());
                ids.extend(l.submitted.clone());
            }
        }
        let Some(at) = steps
            .iter()
            .position(|(s, _)| s.cursor == Some(decision.cursor))
        else {
            continue;
        };
        let first = steps[..at].iter().position(|(s, of_landed)| {
            *of_landed
                || event(s.cursor).is_some_and(|e| {
                    !matches!(kind_of(e), Some(Kind::Arbitrated(_))) && names_any(e, &ids)
                })
        });
        if let Some(first) = first {
            let (mut step, flag) = steps.remove(at);
            step.at_ms = steps[first].0.at_ms;
            steps.insert(first, (step, flag));
        }
    }
}
