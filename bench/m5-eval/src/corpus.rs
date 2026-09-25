//! The M5 conflict corpus (spec §12 M5): cases as TOML files, one per case,
//! written by [`crate::generate`] and read here.
//!
//! A case is a small Rust crate (`base`) and two agent tasks proposed on
//! it. Each task has an intent and an acceptance test that passes only when
//! that task's intent is met. The first task lands; the second collides
//! with it, as a hard merge conflict or as a verification failure after a
//! clean rebase (a semantic conflict, spec §6.5). A case may carry a known
//! `resolution` that meets both intents (the scripted harness uses it) and
//! a `script`: what the scripted harness does on each replay attempt.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// One conflict case.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// `m5-NNN`.
    pub id: String,
    /// The conflict template, such as `signature-vs-caller`.
    pub kind: String,
    /// `hard` (a structural merge conflict) or `semantic` (verification
    /// fails after a clean rebase), as designed.
    pub conflict: String,
    /// The intents contradict: no change meets both acceptance tests, so
    /// the case should end in arbitration.
    pub ambiguous: bool,
    /// How the case was made (generator version and template variant).
    pub made_by: String,
    /// The crate both tasks start from, by path.
    pub base: BTreeMap<String, String>,
    /// The two tasks: the first lands, the second collides.
    pub tasks: Vec<Task>,
    /// Files that meet both intents on top of the first task's result, for
    /// the scripted harness. Absent for ambiguous cases.
    pub resolution: Option<BTreeMap<String, String>>,
    /// What the scripted harness does on each attempt, in order: `resolve`,
    /// `wrong`, `give_up`, `sleep`, or `over_budget` ([`Step`]).
    pub script: Vec<Step>,
}

/// One agent task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// `a` or `b`.
    pub name: String,
    /// One-line intent.
    pub summary: String,
    /// The task description.
    pub body: String,
    /// The acceptance test's function name.
    pub test: String,
    /// The acceptance test file's path in the crate.
    pub test_path: String,
    /// The acceptance test file's content.
    pub test_source: String,
    /// Files the task writes, by path (full contents), besides its test.
    pub files: BTreeMap<String, String>,
}

impl Task {
    /// Everything the task writes: its files and its acceptance test.
    pub fn writes(&self) -> BTreeMap<String, String> {
        let mut out = self.files.clone();
        out.insert(self.test_path.clone(), self.test_source.clone());
        out
    }

    /// The intent file `hord propose` reads: YAML front matter (JSON
    /// strings are YAML scalars) and the body.
    pub fn intent_text(&self) -> Result<String> {
        Ok(format!(
            "---\nsummary: {}\nacceptance:\n  - test: {}\n---\n{}\n",
            serde_json::to_string(&self.summary)?,
            serde_json::to_string(&self.test)?,
            self.body
        ))
    }
}

/// What the scripted harness does on one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Write the case's resolution: meets both intents.
    Resolve,
    /// Write the second task's files again: collides again.
    Wrong,
    /// Change nothing and fail.
    GiveUp,
    /// Run past any budget (the lander must kill it).
    Sleep,
    /// Write the resolution but report more tokens than the budget allows
    /// (the lander must reject it).
    OverBudget,
}

impl Case {
    /// The task that lands first.
    pub fn first(&self) -> &Task {
        &self.tasks[0]
    }

    /// The task that collides.
    pub fn second(&self) -> &Task {
        &self.tasks[1]
    }

    /// Check the shape [`Case`]'s docs describe.
    pub fn validate(&self) -> Result<()> {
        if self.tasks.len() != 2 {
            bail!("{}: {} tasks, not 2", self.id, self.tasks.len());
        }
        if !matches!(self.conflict.as_str(), "hard" | "semantic") {
            bail!("{}: conflict {:?}", self.id, self.conflict);
        }
        if self.ambiguous == self.resolution.is_some() {
            bail!("{}: a resolution exactly when not ambiguous", self.id);
        }
        if self.ambiguous && self.script.contains(&Step::Resolve) {
            bail!("{}: an ambiguous case cannot be resolved", self.id);
        }
        Ok(())
    }

    /// Whether the scripted harness should resolve it with
    /// `max_attempts` replays: a `resolve` step comes within the limit.
    pub fn scripted_resolves(&self, max_attempts: usize) -> bool {
        self.script
            .iter()
            .take(max_attempts)
            .any(|s| *s == Step::Resolve)
    }

    /// The scripted step of attempt `attempt` (from 1); `give_up` past the
    /// script.
    pub fn step(&self, attempt: usize) -> Step {
        attempt
            .checked_sub(1)
            .and_then(|i| self.script.get(i))
            .copied()
            .unwrap_or(Step::GiveUp)
    }
}

/// A case file's text with LF line endings. A checkout may have converted
/// the files to CRLF (git on Windows); the fixture files a case writes must
/// not depend on that.
pub fn read_text(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(text.replace("\r\n", "\n"))
}

/// Read one case file.
pub fn read(path: &Path) -> Result<Case> {
    let text = read_text(path)?;
    let case: Case = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    case.validate()?;
    Ok(case)
}

/// Every case file under `dir`, in file-name order, with its path.
pub fn load(dir: &Path) -> Result<Vec<(PathBuf, Case)>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| read(&path).map(|case| (path, case)))
        .collect()
}

/// A case as TOML.
pub fn write(case: &Case) -> Result<String> {
    Ok(toml::to_string_pretty(case)?)
}
