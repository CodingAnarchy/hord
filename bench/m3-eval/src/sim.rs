//! The M3 concurrency simulation (spec §12) and its overlap oracle (ADR 0012).
//!
//! # Plan
//!
//! Targets are `function_item`s under `src/` or `crates/` that have a body,
//! contain no nested definition, and whose simple name no other definition
//! in the snapshot shares. Targets are drawn in seeded order and accepted
//! only if no already-chosen target's text contains the candidate's name and
//! the candidate's text contains no chosen target's name. So an agent that
//! is meant to be disjoint neither writes, reads, nor names anything another
//! agent writes.
//!
//! The overlapping share of agents is paired off. Each pair overlaps one way,
//! symmetrically so that it holds whichever of the two lands first:
//!
//! - `write-write`: both edit one extra shared function;
//! - `read-write`: each reads (`read_definition`) one of the other's targets;
//! - `name`: each edits one of two free functions in the same module, and
//!   that edit names the other's function (a bare path that
//!   resolves in Rust).
//!
//! # Oracle
//!
//! Per agent, from what the harness itself did, never from the
//! `ChangeRecord` sets: `W` is the definitions it wrote, `R` the definitions
//! it read through the API, and `N` every snapshot definition whose simple
//! name is an identifier in the base or the edited text of a written
//! definition (tree-sitter leaves, [`crate::rust`]). For `A` landed before
//! `B`:
//!
//! - write-write: `W_A ∩ W_B`;
//! - read-write: `W_A ∩ (R_B ∪ N_B)`;
//! - write-read: `W_B ∩ (R_A ∪ N_A)`, a true overlap for the gate only under
//!   `strict_reads` (spec §6.3: off by default).
//!
//! A false negative is a gated overlap where `B` landed with no set conflict
//! naming `A`. A false positive is a set conflict against `A` where the pair
//! has no overlap in any direction, or a merge conflict or park of a change
//! that overlaps nothing landed before it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hord_core::{Actor, ChangeId, Intent, NodeId, RepoPath};
use hord_txn::{Base, BeginOptions, ConflictKind, DefinitionInfo, QueueStatus, Repo};
use serde::Serialize;

use crate::rust;

/// Seeded SplitMix64.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// Every definition at the base, as the workspace API lists it. Listing is
/// not a read.
pub(crate) struct Snapshot {
    pub defs: Vec<DefinitionInfo>,
    /// Simple name → every definition with it.
    pub by_name: HashMap<String, Vec<NodeId>>,
    pub bytes: HashMap<RepoPath, Vec<u8>>,
}

pub(crate) fn simple_name(def: &DefinitionInfo) -> Option<&str> {
    let name = def.name.as_ref()?.as_str();
    name.rsplit("::").next()
}

pub(crate) async fn load_snapshot(repo: &Repo, files: &[(String, Vec<u8>)]) -> Result<Snapshot> {
    let mut ws = repo
        .begin(BeginOptions::at_head(actor("m3-planner")))
        .await?;
    let mut defs = Vec::new();
    let mut bytes = HashMap::new();
    for (path, content) in files {
        if !path.ends_with(".rs") {
            continue;
        }
        let Ok(repo_path) = path.parse::<RepoPath>() else {
            continue;
        };
        if let Ok(found) = ws.definitions(&repo_path).await {
            defs.extend(found);
            bytes.insert(repo_path, content.clone());
        }
    }
    let mut by_name: HashMap<String, Vec<NodeId>> = HashMap::new();
    for def in &defs {
        if let Some(name) = simple_name(def) {
            by_name.entry(name.to_string()).or_default().push(def.node);
        }
    }
    Ok(Snapshot {
        defs,
        by_name,
        bytes,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub path: RepoPath,
    pub node: NodeId,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PairKind {
    WriteWrite,
    ReadWrite,
    Name,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentPlan {
    pub index: usize,
    pub writes: Vec<Target>,
    /// Extra API reads (a read-write pair's partner target).
    pub reads: Vec<Target>,
    /// Name the first edit's statement refers to (a name pair).
    pub names: Option<String>,
    pub pair: Option<(usize, PairKind)>,
}

struct Picker<'a> {
    snapshot: &'a Snapshot,
    order: Vec<usize>,
    next: usize,
    chosen: HashSet<NodeId>,
    chosen_names: HashSet<String>,
    used_idents: HashSet<String>,
    clean_files: HashMap<RepoPath, bool>,
}

impl<'a> Picker<'a> {
    fn new(snapshot: &'a Snapshot, rng: &mut Rng) -> Self {
        let parents: HashSet<NodeId> = snapshot.defs.iter().filter_map(|d| d.parent).collect();
        let mut order: Vec<usize> = snapshot
            .defs
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.kind.as_str() == "function_item"
                    && matches!(
                        d.path.components().first().map(String::as_str),
                        Some("src" | "crates")
                    )
                    && !parents.contains(&d.node)
                    && d.span.len() < 8_000
                    && simple_name(d)
                        .is_some_and(|n| snapshot.by_name.get(n).is_some_and(|all| all.len() == 1))
            })
            .map(|(i, _)| i)
            .collect();
        rng.shuffle(&mut order);
        // Identifiers the generated statements use.
        let used_idents = ["std", "hint", "black_box", "u64"]
            .into_iter()
            .map(str::to_string)
            .collect();
        Self {
            snapshot,
            order,
            next: 0,
            chosen: HashSet::new(),
            chosen_names: HashSet::new(),
            used_idents,
            clean_files: HashMap::new(),
        }
    }

    /// Whether definition `i` may be a target given everything chosen so
    /// far; its target and identifiers if so.
    fn check(&mut self, i: usize) -> Option<(Target, BTreeSet<String>)> {
        let def = &self.snapshot.defs[i];
        let name = simple_name(def)?;
        if self.chosen.contains(&def.node) || self.used_idents.contains(name) {
            return None;
        }
        let file = self.snapshot.bytes.get(&def.path)?;
        let clean = *self
            .clean_files
            .entry(def.path.clone())
            .or_insert_with(|| rust::parses_clean(file));
        if !clean {
            return None;
        }
        let shape = file.get(def.span.clone()).and_then(rust::fn_shape)?;
        if shape.name != name || shape.idents.iter().any(|i| self.chosen_names.contains(i)) {
            return None;
        }
        let target = Target {
            path: def.path.clone(),
            node: def.node,
            name: name.to_string(),
        };
        Some((target, shape.idents))
    }

    fn commit(&mut self, target: &Target, idents: BTreeSet<String>) {
        self.chosen.insert(target.node);
        self.chosen_names.insert(target.name.clone());
        self.used_idents.extend(idents);
    }

    fn take(&mut self) -> Option<Target> {
        while self.next < self.order.len() {
            let i = self.order[self.next];
            self.next += 1;
            if let Some((target, idents)) = self.check(i) {
                self.commit(&target, idents);
                return Some(target);
            }
        }
        None
    }

    /// Two module-level free functions in one file, so a bare call from one
    /// resolves to the other as it would in Rust. Candidates are drawn from
    /// the unused rest of the seeded order.
    fn take_same_module(&mut self) -> Option<(Target, Target)> {
        let mut seen: HashMap<RepoPath, (Target, BTreeSet<String>)> = HashMap::new();
        for k in self.next..self.order.len() {
            let i = self.order[k];
            let def = &self.snapshot.defs[i];
            let free = def.parent.is_none()
                && def
                    .name
                    .as_ref()
                    .is_some_and(|n| !n.as_str().contains("::"));
            if !free {
                continue;
            }
            let Some((target, idents)) = self.check(i) else {
                continue;
            };
            if let Some((first, first_idents)) = seen.get(&target.path)
                && !idents.contains(&first.name)
                && !first_idents.contains(&target.name)
            {
                let (first, first_idents) = (first.clone(), first_idents.clone());
                self.commit(&first, first_idents);
                self.commit(&target, idents);
                return Some((first, target));
            }
            seen.entry(target.path.clone()).or_insert((target, idents));
        }
        None
    }
}

pub(crate) struct Plan {
    pub agents: Vec<AgentPlan>,
    pub pairs: Vec<(usize, usize, PairKind)>,
    pub pool: usize,
}

pub(crate) fn plan(
    snapshot: &Snapshot,
    agents: usize,
    overlap_percent: usize,
    seed: u64,
) -> Result<Plan> {
    let mut rng = Rng::new(seed);
    let mut picker = Picker::new(snapshot, &mut rng);
    let pool = picker.order.len();
    let mut plans = Vec::with_capacity(agents);
    for index in 0..agents {
        let count = 1 + rng.below(5);
        let mut writes = Vec::with_capacity(count);
        for _ in 0..count {
            writes.push(picker.take().context("target pool exhausted")?);
        }
        plans.push(AgentPlan {
            index,
            writes,
            reads: Vec::new(),
            names: None,
            pair: None,
        });
    }
    let overlapping = (agents * overlap_percent / 100) & !1;
    let mut indices: Vec<usize> = (0..agents).collect();
    rng.shuffle(&mut indices);
    let kinds = [PairKind::WriteWrite, PairKind::ReadWrite, PairKind::Name];
    let offset = rng.below(kinds.len());
    let mut pairs = Vec::new();
    for (j, pair) in indices[..overlapping].chunks(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let kind = kinds[(j + offset) % kinds.len()];
        match kind {
            PairKind::WriteWrite => {
                let shared = picker.take().context("target pool exhausted")?;
                plans[a].writes.push(shared.clone());
                plans[b].writes.push(shared);
            }
            PairKind::ReadWrite => {
                let (ta, tb) = (plans[a].writes[0].clone(), plans[b].writes[0].clone());
                plans[a].reads.push(tb);
                plans[b].reads.push(ta);
            }
            PairKind::Name => {
                // Each edits one of two functions in the same module, and
                // that edit calls the other's function by its bare name.
                let (ta, tb) = picker
                    .take_same_module()
                    .context("no two free functions left in one module")?;
                plans[a].names = Some(tb.name.clone());
                plans[b].names = Some(ta.name.clone());
                plans[a].writes.insert(0, ta);
                plans[b].writes.insert(0, tb);
            }
        }
        plans[a].pair = Some((b, kind));
        plans[b].pair = Some((a, kind));
        pairs.push((a, b, kind));
    }
    Ok(Plan {
        agents: plans,
        pairs,
        pool,
    })
}

pub(crate) fn actor(id: &str) -> Actor {
    Actor::Agent {
        id: id.into(),
        model: "synthetic".into(),
        model_hash: hord_core::Bytes::default(),
        harness: "hord-eval-m3".into(),
    }
}

pub(crate) fn intent(summary: &str) -> Intent {
    Intent {
        summary: summary.into(),
        body: String::new(),
        refs: Vec::new(),
        acceptance: Vec::new(),
    }
}

/// What one agent did, recorded by the harness.
struct AgentRun {
    index: usize,
    change: ChangeId,
    begin: Duration,
    /// `(node, base text, edited text)` per written definition.
    written: Vec<(NodeId, Vec<u8>, Vec<u8>)>,
    reads: BTreeSet<NodeId>,
}

fn statement(rng: &mut Rng, agent: usize, i: usize) -> String {
    let n = rng.below(1_000_000);
    if rng.below(2) == 0 {
        format!("let _sim_a{agent}_{i} = {n}_u64;")
    } else {
        format!("let _ = std::hint::black_box({n}_u64);")
    }
}

/// Submission turns: an agent at position `k` submits once the turn is `k`.
/// `None` lets every agent submit as soon as it has proposed.
type Turns = Option<(tokio::sync::watch::Sender<usize>, usize)>;

async fn run_agent(
    repo: Repo,
    base: ChangeId,
    plan: AgentPlan,
    seed: u64,
    turns: Turns,
) -> Result<AgentRun> {
    let work = agent_work(&repo, base, plan, seed).await;
    match turns {
        None => {
            let (run, change) = work?;
            repo.submit(change).await?;
            Ok(run)
        }
        Some((turn, position)) => {
            let mut rx = turn.subscribe();
            rx.wait_for(|t| *t == position)
                .await
                .context("submission turns closed")?;
            let submitted = match &work {
                Ok((_, change)) => repo.submit(*change).await.map(|_| ()),
                Err(_) => Ok(()),
            };
            turn.send_modify(|t| *t += 1);
            submitted?;
            Ok(work?.0)
        }
    }
}

/// Begin, read, edit, and propose; the proposal is returned unsubmitted.
async fn agent_work(
    repo: &Repo,
    base: ChangeId,
    plan: AgentPlan,
    seed: u64,
) -> Result<(AgentRun, ChangeId)> {
    let mut rng = Rng::new(seed ^ (plan.index as u64).wrapping_mul(0xA24B_AED4_963E_E407));
    let started = Instant::now();
    let mut ws = repo
        .begin(BeginOptions {
            base: Base::Change(base),
            actor: actor(&format!("agent-{}", plan.index)),
            session: Some(format!("m3-sim-{seed}")),
        })
        .await?;
    let begin = started.elapsed();
    let mut reads = BTreeSet::new();
    for target in &plan.reads {
        ws.read_definition(&target.path, target.node).await?;
        reads.insert(target.node);
    }
    let mut written = Vec::new();
    for (i, target) in plan.writes.iter().enumerate() {
        let text = ws.read_definition(&target.path, target.node).await?;
        reads.insert(target.node);
        let stmt = match (&plan.names, i) {
            (Some(name), 0) => format!("let _ = {name};"),
            _ => statement(&mut rng, plan.index, i),
        };
        let Some(edited) = rust::insert_stmt(text.as_slice(), &stmt) else {
            bail!(
                "agent {}: edit of {} in {} does not parse",
                plan.index,
                target.name,
                target.path
            );
        };
        ws.write_definition(&target.path, target.node, &edited)
            .await?;
        written.push((target.node, text.as_slice().to_vec(), edited));
    }
    let proposal = ws
        .propose(intent(&format!("m3 sim agent {}", plan.index)))
        .await?;
    let run = AgentRun {
        index: plan.index,
        change: proposal.change,
        begin,
        written,
        reads,
    };
    Ok((run, proposal.change))
}

/// Oracle sets for one agent.
struct Truth {
    w: BTreeSet<NodeId>,
    /// `R ∪ N`.
    rn: BTreeSet<NodeId>,
}

fn truth(run: &AgentRun, snapshot: &Snapshot) -> Truth {
    let w: BTreeSet<NodeId> = run.written.iter().map(|(node, _, _)| *node).collect();
    let mut rn = run.reads.clone();
    for (_, before, after) in &run.written {
        for ident in rust::idents(before).into_iter().chain(rust::idents(after)) {
            if let Some(nodes) = snapshot.by_name.get(&ident) {
                rn.extend(nodes.iter().copied());
            }
        }
    }
    Truth { w, rn }
}

fn meets(a: &BTreeSet<NodeId>, b: &BTreeSet<NodeId>) -> bool {
    a.intersection(b).next().is_some()
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PairFinding {
    pub earlier: usize,
    pub later: usize,
    pub oracle: Vec<&'static str>,
    pub reported: Vec<ConflictKind>,
    /// Diagnostics only: the pair's nodes on each side, oracle and record.
    pub detail: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChangeRow {
    pub agent: usize,
    pub seq: u64,
    pub status: String,
    pub pair: Option<(usize, PairKind)>,
    pub writes: usize,
    pub set_conflicts: Vec<(usize, ConflictKind)>,
    pub merge_conflicts: Vec<String>,
    pub overlaps_landed: bool,
    pub false_positive: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct IdentityLoss {
    pub agent: usize,
    /// Written definitions missing from the record's `write_set`.
    pub missing: Vec<String>,
    /// `write_set` entries that are no written definition.
    pub extra: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SimReport {
    pub seed: u64,
    pub agents: usize,
    pub pool: usize,
    pub planned_disjoint: usize,
    pub planned_overlapping: usize,
    pub pairs: Vec<(usize, usize, PairKind)>,
    pub strict_reads: bool,
    pub racy_submit: bool,
    pub agents_secs: f64,
    pub agent_begin_max_ms: f64,
    pub land_secs: f64,
    pub throughput: f64,
    pub landed: usize,
    pub conflicted: usize,
    pub rejected: usize,
    pub flagged: usize,
    pub false_negatives: Vec<PairFinding>,
    pub false_positive_pairs: Vec<PairFinding>,
    pub false_positive_changes: usize,
    pub false_positive_rate: f64,
    pub disjoint: usize,
    pub disjoint_landed: usize,
    pub disjoint_not_landed: Vec<usize>,
    pub unknown_landed_refs: usize,
    /// Diagnostics only: agents whose stored `write_set` lacks a definition
    /// the agent wrote (see [`Self::identity_loss`]).
    pub identity_loss: Vec<IdentityLoss>,
    /// False-positive changes where neither side of any false-positive pair
    /// lost identity. Printed; not a gate.
    pub false_positive_changes_clean_identity: usize,
    pub changes: Vec<ChangeRow>,
    /// Files the targets live in, for the workspace run.
    #[serde(skip)]
    pub target_files: Vec<RepoPath>,
}

/// Simulation parameters.
pub(crate) struct SimConfig {
    pub agents: usize,
    pub overlap_percent: usize,
    pub seed: u64,
    pub strict_reads: bool,
    /// Submit in completion order instead of a seeded order.
    pub racy_submit: bool,
}

pub(crate) async fn run(
    repo: &Repo,
    base: ChangeId,
    snapshot: &Snapshot,
    config: &SimConfig,
) -> Result<SimReport> {
    let &SimConfig {
        agents,
        overlap_percent,
        seed,
        strict_reads,
        racy_submit,
    } = config;
    let plan = plan(snapshot, agents, overlap_percent, seed)?;
    let planned_overlapping = plan.agents.iter().filter(|a| a.pair.is_some()).count();
    let target_files: Vec<RepoPath> = plan
        .agents
        .iter()
        .flat_map(|a| a.writes.iter().map(|t| t.path.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let started = Instant::now();
    // Agents work concurrently; unless racy, they submit in a seeded order
    // so a seed fixes the landing order too.
    let mut positions: Vec<usize> = (0..agents).collect();
    Rng::new(seed.rotate_left(17)).shuffle(&mut positions);
    let (turn, _) = tokio::sync::watch::channel(0usize);
    let mut tasks = Vec::with_capacity(agents);
    for agent in plan.agents.iter().cloned() {
        let turns = (!racy_submit).then(|| (turn.clone(), positions[agent.index]));
        tasks.push(tokio::spawn(run_agent(
            repo.clone(),
            base,
            agent,
            seed,
            turns,
        )));
    }
    let mut runs = Vec::with_capacity(agents);
    for task in tasks {
        runs.push(task.await.context("agent task")??);
    }
    let agents_secs = started.elapsed().as_secs_f64();
    let agent_begin_max_ms = runs
        .iter()
        .map(|r| r.begin.as_secs_f64() * 1000.0)
        .fold(0.0, f64::max);

    let started = Instant::now();
    let done = repo.land_local().await?;
    let land = started.elapsed();
    if done.len() != agents {
        bail!("land_local processed {} of {agents} changes", done.len());
    }

    let by_change: HashMap<ChangeId, usize> = runs.iter().map(|r| (r.change, r.index)).collect();
    let runs_by_index: BTreeMap<usize, &AgentRun> = runs.iter().map(|r| (r.index, r)).collect();
    let truths: BTreeMap<usize, Truth> =
        runs.iter().map(|r| (r.index, truth(r, snapshot))).collect();
    let mut landed_as: HashMap<ChangeId, usize> = HashMap::new();
    let mut landed_before: Vec<usize> = Vec::new();

    let mut report = SimReport {
        seed,
        agents,
        pool: plan.pool,
        planned_disjoint: agents - planned_overlapping,
        planned_overlapping,
        pairs: plan.pairs.clone(),
        strict_reads,
        racy_submit,
        agents_secs,
        agent_begin_max_ms,
        land_secs: land.as_secs_f64(),
        throughput: agents as f64 / land.as_secs_f64(),
        landed: 0,
        conflicted: 0,
        rejected: 0,
        flagged: 0,
        false_negatives: Vec::new(),
        false_positive_pairs: Vec::new(),
        false_positive_changes: 0,
        false_positive_rate: 0.0,
        disjoint: 0,
        disjoint_landed: 0,
        disjoint_not_landed: Vec::new(),
        unknown_landed_refs: 0,
        identity_loss: Vec::new(),
        false_positive_changes_clean_identity: 0,
        changes: Vec::new(),
        target_files,
    };

    for run in &runs {
        let record = repo.change(run.change).await?;
        let w = &truths[&run.index].w;
        let missing: BTreeSet<NodeId> = w.difference(&record.write_set).copied().collect();
        if !missing.is_empty() {
            let extra: BTreeSet<NodeId> = record.write_set.difference(w).copied().collect();
            report.identity_loss.push(IdentityLoss {
                agent: run.index,
                missing: describe_each(snapshot, &missing),
                extra: describe_each(snapshot, &extra),
            });
        }
    }
    let lost: HashSet<usize> = report.identity_loss.iter().map(|l| l.agent).collect();

    let mut entries = done;
    entries.sort_by_key(|e| e.seq);
    for entry in &entries {
        let b = *by_change
            .get(&entry.change)
            .context("queue entry for a change no agent proposed")?;
        let tb = &truths[&b];
        let landed = matches!(entry.status, QueueStatus::Landed { .. });
        match &entry.status {
            QueueStatus::Landed { .. } => report.landed += 1,
            QueueStatus::Conflicted => report.conflicted += 1,
            QueueStatus::Rejected { .. } => report.rejected += 1,
            QueueStatus::Queued => bail!("change of agent {b} still queued after land_local"),
        }
        let mut reported: BTreeMap<usize, Vec<ConflictKind>> = BTreeMap::new();
        let mut merge_conflicts = Vec::new();
        if let Some(r) = &entry.report {
            if !r.is_clean() {
                report.flagged += 1;
            }
            for conflict in &r.conflicts {
                match landed_as.get(&conflict.landed) {
                    Some(a) => reported.entry(*a).or_default().push(conflict.kind),
                    None => report.unknown_landed_refs += 1,
                }
            }
            merge_conflicts = r
                .merge
                .iter()
                .map(|m| format!("{:?} {}: {}", m.severity, m.path, m.reason))
                .collect();
        }

        let mut overlaps_landed = false;
        for &a in &landed_before {
            let ta = &truths[&a];
            let ww = meets(&ta.w, &tb.w);
            let rw = meets(&ta.w, &tb.rn);
            let wr = meets(&tb.w, &ta.rn);
            let mut oracle = Vec::new();
            if ww {
                oracle.push("write-write");
            }
            if rw {
                oracle.push("read-write");
            }
            if wr {
                oracle.push("write-read");
            }
            overlaps_landed |= ww || rw || wr;
            let gated = ww || rw || (strict_reads && wr);
            if gated && landed && !reported.contains_key(&a) {
                let detail = explain(repo, snapshot, &runs_by_index, &truths, a, b).await?;
                report.false_negatives.push(PairFinding {
                    earlier: a,
                    later: b,
                    oracle,
                    reported: Vec::new(),
                    detail,
                });
            }
        }
        let mut false_positive = false;
        let mut explained = true;
        for (&a, kinds) in &reported {
            let ta = &truths[&a];
            if !(meets(&ta.w, &tb.w) || meets(&ta.w, &tb.rn) || meets(&tb.w, &ta.rn)) {
                false_positive = true;
                explained &= lost.contains(&a) || lost.contains(&b);
                let detail = explain(repo, snapshot, &runs_by_index, &truths, a, b).await?;
                report.false_positive_pairs.push(PairFinding {
                    earlier: a,
                    later: b,
                    oracle: Vec::new(),
                    reported: kinds.clone(),
                    detail,
                });
            }
        }
        if !overlaps_landed && (!merge_conflicts.is_empty() || !landed) {
            false_positive = true;
            explained = false;
        }
        if false_positive {
            report.false_positive_changes += 1;
            if !explained {
                report.false_positive_changes_clean_identity += 1;
            }
        }
        if !overlaps_landed {
            report.disjoint += 1;
            if landed {
                report.disjoint_landed += 1;
            } else {
                report.disjoint_not_landed.push(b);
            }
        }
        if let QueueStatus::Landed { landed: id } = entry.status {
            landed_as.insert(id, b);
            landed_as.insert(entry.change, b);
            landed_before.push(b);
        }
        report.changes.push(ChangeRow {
            agent: b,
            seq: entry.seq,
            status: status_name(&entry.status),
            pair: plan.agents[b].pair,
            writes: runs_by_index[&b].written.len(),
            set_conflicts: reported
                .iter()
                .flat_map(|(a, kinds)| kinds.iter().map(|k| (*a, *k)))
                .collect(),
            merge_conflicts,
            overlaps_landed,
            false_positive,
        });
    }
    report.false_positive_rate = report.false_positive_changes as f64 / agents as f64;
    Ok(report)
}

fn describe(snapshot: &Snapshot, nodes: &BTreeSet<NodeId>) -> String {
    format!("[{}]", describe_each(snapshot, nodes).join(", "))
}

fn describe_each(snapshot: &Snapshot, nodes: &BTreeSet<NodeId>) -> Vec<String> {
    nodes
        .iter()
        .map(|node| {
            snapshot.defs.iter().find(|d| d.node == *node).map_or_else(
                || format!("{node} (not a base definition)"),
                |d| {
                    format!(
                        "{} {} in {}",
                        d.kind.as_str(),
                        d.name.as_ref().map_or("?", |n| n.as_str()),
                        d.path
                    )
                },
            )
        })
        .collect()
}

/// Diagnostics for a flagged pair: what the oracle and the stored records
/// each put in the intersections. Not used to decide anything.
async fn explain(
    repo: &Repo,
    snapshot: &Snapshot,
    runs: &BTreeMap<usize, &AgentRun>,
    truths: &BTreeMap<usize, Truth>,
    a: usize,
    b: usize,
) -> Result<Vec<String>> {
    let (ra, rb) = (
        repo.change(runs[&a].change).await?,
        repo.change(runs[&b].change).await?,
    );
    let (ta, tb) = (&truths[&a], &truths[&b]);
    let inter = |x: &BTreeSet<NodeId>, y: &BTreeSet<NodeId>| -> BTreeSet<NodeId> {
        x.intersection(y).copied().collect()
    };
    Ok(vec![
        format!(
            "oracle W_A∩W_B {}",
            describe(snapshot, &inter(&ta.w, &tb.w))
        ),
        format!(
            "oracle W_A∩(R∪N)_B {}",
            describe(snapshot, &inter(&ta.w, &tb.rn))
        ),
        format!(
            "oracle W_B∩(R∪N)_A {}",
            describe(snapshot, &inter(&tb.w, &ta.rn))
        ),
        format!("oracle W_A {}", describe(snapshot, &ta.w)),
        format!(
            "record W_A {} nodes, W_B {} nodes",
            ra.write_set.len(),
            rb.write_set.len()
        ),
        format!(
            "record W_A∩W_B {}",
            describe(snapshot, &inter(&ra.write_set, &rb.write_set))
        ),
        format!(
            "record W_A∩R_B {}",
            describe(snapshot, &inter(&ra.write_set, &rb.read_set))
        ),
        format!(
            "oracle W_A missing from record W_A {}",
            describe(snapshot, &ta.w.difference(&ra.write_set).copied().collect())
        ),
    ])
}

pub(crate) fn status_name(status: &QueueStatus) -> String {
    match status {
        QueueStatus::Queued => "queued".into(),
        QueueStatus::Landed { .. } => "landed".into(),
        QueueStatus::Conflicted => "conflicted".into(),
        QueueStatus::Rejected { reason } => format!("rejected: {reason}"),
    }
}
