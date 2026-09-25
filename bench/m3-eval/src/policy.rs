//! `--policy`: spec §7.2's example policy enforced in the concurrency
//! simulation (spec §12 M4), with verification stubbed by evidence
//! fixtures (§12 M3 allows it for throughput).
//!
//! After planning, a change lands `.hord-policy.toml`, so the landing base
//! of every agent's change carries it (ADR 0026):
//!
//! - `[land] require = ["check", "test:selected", "lint"]`, as §7.2;
//! - "unsafe requires human": `touches_kind = "unsafe_block"` needs
//!   `review:human`. Some agents' first edit is an `unsafe` block;
//! - "public API": `touches_visibility = "pub"` under one directory needs
//!   `review:human` and `test:full`. §7.2's `crates/hord-core/**` does not
//!   exist in the cargo corpus, so the directory is the one holding the
//!   most agents' `pub` targets (a few, not most);
//! - "agent-authored large changes": `actor = "agent"`, `write_set_gt`
//!   needs `review:agent-reviewer` and `bench:no-regression`. §7.2's 50 is
//!   scaled to the simulation's 1–6 definitions per change: more than 4.
//!
//! [`FixtureVerifier`] indexes `Pass` evidence for every requirement it is
//! asked for except reviews, as a CI would; nobody reviews. So a change
//! that a rule applies to is denied for the missing review, with the
//! violations in its report, and parked. An independent oracle
//! ([`expected`]) decides from what each agent did (its edit, the text of
//! the definitions it wrote, how many) which rules apply. Gates: every rule
//! fires; each change the policy denied names exactly the oracle's rules;
//! no change the oracle clears was denied, and none it polices landed.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use hord_core::{Actor, EvidenceKind, EvidenceResult, RepoPath, Timestamp};
use hord_txn::{QueueEntry, QueueStatus, Verdict, Verifier, VerifyFuture, VerifyRequest};
use serde::Serialize;

use crate::sim::{AgentPlan, Snapshot};

/// The three rule names, as the policy file spells them.
pub(crate) const UNSAFE: &str = "unsafe requires human";
pub(crate) const PUBLIC: &str = "public API";
pub(crate) const LARGE: &str = "agent-authored large changes";
/// `write_set_gt` of the large-change rule.
pub(crate) const LARGE_WRITES: usize = 4;

/// The policy file for a public-API directory `dir`.
pub(crate) fn policy_file(dir: &str) -> String {
    format!(
        "[land]\n\
         require = [\"check\", \"test:selected\", \"lint\"]\n\
         strict_reads = false\n\
         max_write_set = 200\n\
         max_replay_attempts = 2\n\
         \n\
         [[rule]]\n\
         name = \"{UNSAFE}\"\n\
         when = {{ touches_kind = \"unsafe_block\" }}\n\
         require = [\"review:human\"]\n\
         \n\
         [[rule]]\n\
         name = \"{PUBLIC}\"\n\
         when = {{ touches_visibility = \"pub\", paths = [\"{dir}/**\"] }}\n\
         require = [\"review:human\", \"test:full\"]\n\
         \n\
         [[rule]]\n\
         name = \"{LARGE}\"\n\
         when = {{ actor = \"agent\", write_set_gt = {LARGE_WRITES} }}\n\
         require = [\"review:agent-reviewer\", \"bench:no-regression\"]\n"
    )
}

/// Whether a function's text is declared exactly `pub` (not `pub(crate)`):
/// the first line that is not an attribute or comment starts with `pub `.
pub(crate) fn is_pub(text: &[u8]) -> bool {
    let text = String::from_utf8_lossy(text);
    text.lines()
        .map(str::trim_start)
        .find(|l| !(l.is_empty() || l.starts_with("#[") || l.starts_with("//")))
        .is_some_and(|l| l.starts_with("pub "))
}

fn def_text<'a>(snapshot: &'a Snapshot, target: &crate::sim::Target) -> &'a [u8] {
    snapshot
        .defs
        .iter()
        .find(|d| d.node == target.node)
        .and_then(|d| {
            snapshot
                .bytes
                .get(&d.path)
                .and_then(|b| b.get(d.span.clone()))
        })
        .unwrap_or_default()
}

/// The directory (three path components) holding `pub` targets of the most
/// agents, among those holding at most `max` of them; `None` without one.
pub(crate) fn public_dir(agents: &[AgentPlan], snapshot: &Snapshot, max: usize) -> Option<String> {
    let mut by_dir: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for agent in agents {
        for target in &agent.writes {
            if !is_pub(def_text(snapshot, target)) {
                continue;
            }
            let parts = target.path.components();
            if parts.len() > 3 {
                by_dir
                    .entry(parts[..3].join("/"))
                    .or_default()
                    .insert(agent.index);
            }
        }
    }
    by_dir
        .into_iter()
        .filter(|(_, agents)| agents.len() <= max)
        .max_by_key(|(dir, agents)| (agents.len(), std::cmp::Reverse(dir.clone())))
        .map(|(dir, _)| dir)
}

/// Agents whose first edit becomes an `unsafe` block: every tenth
/// unpaired agent.
pub(crate) fn unsafe_agents(agents: &mut [AgentPlan]) {
    for agent in agents
        .iter_mut()
        .filter(|a| a.pair.is_none() && a.names.is_none())
    {
        if agent.index % 10 == 3 {
            agent.unsafe_edit = true;
        }
    }
}

/// The rules the oracle says apply to each agent's change.
pub(crate) fn expected(
    agents: &[AgentPlan],
    snapshot: &Snapshot,
    dir: &str,
    written: &BTreeMap<usize, usize>,
) -> BTreeMap<usize, BTreeSet<&'static str>> {
    let prefix: RepoPath = dir.parse().expect("directory path");
    agents
        .iter()
        .map(|agent| {
            let mut rules = BTreeSet::new();
            if agent.unsafe_edit {
                rules.insert(UNSAFE);
            }
            if agent.writes.iter().any(|t| {
                t.path.components().starts_with(prefix.components())
                    && is_pub(def_text(snapshot, t))
            }) {
                rules.insert(PUBLIC);
            }
            if written.get(&agent.index).copied().unwrap_or(0) > LARGE_WRITES {
                rules.insert(LARGE);
            }
            (agent.index, rules)
        })
        .collect()
}

/// Land the policy file on `base`, for `plan` (whose unsafe agents it
/// picks). Returns the new base and the public-API directory.
pub(crate) async fn install(
    repo: &hord_txn::Repo,
    base: hord_core::ChangeId,
    plan: &mut crate::sim::Plan,
    snapshot: &Snapshot,
) -> Result<(hord_core::ChangeId, String)> {
    unsafe_agents(&mut plan.agents);
    let dir = public_dir(&plan.agents, snapshot, plan.agents.len() / 8)
        .ok_or_else(|| anyhow::anyhow!("no directory holds a pub target"))?;
    let mut ws = repo
        .begin(hord_txn::BeginOptions {
            base: hord_txn::Base::Change(base),
            actor: crate::sim::actor("m3-policy"),
            session: None,
        })
        .await?;
    ws.write_file(&hord_policy::POLICY_PATH.parse()?, policy_file(&dir))
        .await?;
    let change = ws
        .propose(crate::sim::intent("install the §7.2 example policy"))
        .await?
        .change;
    repo.submit(change).await?;
    let done = repo.land_local().await?;
    anyhow::ensure!(
        done.iter()
            .any(|e| e.change == change && matches!(e.status, QueueStatus::Landed { .. })),
        "the policy change did not land: {done:?}"
    );
    Ok((change, dir))
}

/// Indexes `Pass` evidence for every requirement except reviews, under the
/// candidate's snapshot, as a CI would, and runs nothing.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FixtureVerifier;

impl Verifier for FixtureVerifier {
    fn verify(&self, request: VerifyRequest) -> VerifyFuture<'_> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || fixtures(&request))
                .await
                .unwrap_or_else(|err| Verdict::Fail {
                    evidence: Vec::new(),
                    reason: err.to_string(),
                })
        })
    }
}

fn fixtures(request: &VerifyRequest) -> Verdict {
    let index = request.context.index();
    let mut fixtures = Vec::new();
    for requirement in request
        .policy
        .require
        .iter()
        .filter(|r| !r.starts_with("review"))
    {
        let (kind, qualifier) = match requirement.split_once(':') {
            Some((k, q)) => (k, Some(q.to_owned())),
            None => (requirement.as_str(), None),
        };
        let kind = match kind {
            "check" => EvidenceKind::Check,
            "test" => EvidenceKind::Test,
            "lint" => EvidenceKind::Lint,
            "bench" => EvidenceKind::Bench,
            other => EvidenceKind::Custom(other.to_owned()),
        };
        fixtures.push(
            hord_verify::EvidenceFields {
                kind,
                qualifier,
                snapshot: request.change.result,
                toolchain: hord_core::ObjectId::from_canonical(b"hord-eval-m3 fixtures"),
                command: format!("fixture {requirement}"),
                scope: None,
                result: EvidenceResult::Pass,
                log: None,
                cost_ms: 0,
                produced_by: Actor::Agent {
                    id: "m3-fixture-ci".into(),
                    model: String::new(),
                    model_hash: hord_core::Bytes::default(),
                    harness: "hord-eval-m3".into(),
                },
                produced_at: Timestamp::from_millis(0),
            }
            .build(),
        );
    }
    // One index commit for the candidate's evidence, as `hord_verify::verify`.
    match index.put_evidence_batch(&fixtures) {
        Ok(evidence) => Verdict::Pass { evidence },
        Err(err) => Verdict::Fail {
            evidence: Vec::new(),
            reason: err.to_string(),
        },
    }
}

/// What `--policy` found.
#[derive(Debug, Default, Serialize)]
pub(crate) struct PolicyReport {
    /// The public-API rule's directory.
    pub public_dir: String,
    /// Changes the policy denied (parked), by rule.
    pub fired: BTreeMap<String, usize>,
    /// Changes the oracle polices.
    pub expected_denied: usize,
    pub denied: usize,
    /// Denied changes whose violations name other rules than the oracle's,
    /// changes denied that the oracle clears, and policed changes that
    /// landed.
    pub mismatches: Vec<String>,
    /// Every rule fired, and no mismatches.
    pub pass: bool,
}

/// Check the queue against the oracle. `agent_of` maps a submitted change
/// to its agent.
pub(crate) fn check(
    entries: &[QueueEntry],
    agent_of: &dyn Fn(&QueueEntry) -> Option<usize>,
    expected: &BTreeMap<usize, BTreeSet<&'static str>>,
    public_dir: &str,
) -> Result<PolicyReport> {
    let mut report = PolicyReport {
        public_dir: public_dir.to_owned(),
        expected_denied: expected.values().filter(|r| !r.is_empty()).count(),
        ..PolicyReport::default()
    };
    for entry in entries {
        let Some(agent) = agent_of(entry) else {
            continue;
        };
        let want = &expected[&agent];
        let named: BTreeSet<String> = entry
            .report
            .iter()
            .flat_map(|r| r.policy.iter().filter_map(|v| v.rule.clone()))
            .collect();
        match &entry.status {
            QueueStatus::Parked { .. } => {
                report.denied += 1;
                for rule in &named {
                    *report.fired.entry(rule.clone()).or_default() += 1;
                }
                let want: BTreeSet<String> = want.iter().map(|r| (*r).to_owned()).collect();
                if want.is_empty() {
                    report.mismatches.push(format!(
                        "agent {agent}: denied {named:?}, but no rule applies"
                    ));
                } else if named != want {
                    report.mismatches.push(format!(
                        "agent {agent}: denied for {named:?}, oracle {want:?}"
                    ));
                }
            }
            QueueStatus::Landed { .. } if !want.is_empty() => report.mismatches.push(format!(
                "agent {agent}: landed, but {want:?} apply and nothing was reviewed"
            )),
            QueueStatus::Rejected { reason } if reason.starts_with("policy") => report
                .mismatches
                .push(format!("agent {agent}: rejected by policy: {reason}")),
            _ => {}
        }
    }
    let all_fired = [UNSAFE, PUBLIC, LARGE]
        .iter()
        .all(|rule| report.fired.contains_key(*rule));
    report.pass = all_fired && report.mismatches.is_empty();
    Ok(report)
}
