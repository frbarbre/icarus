use crate::facts::{Check, CheckState, PrLifecycle, Workspace};
use crate::prompts::TriagedCheck;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_TRIAGES_PER_PR: u32 = 3;
pub const REMEMBER_MS: u64 = 7 * 24 * 60 * 60_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Starts {
    pub count: u32,
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CiTriageMemory {
    pub runs: BTreeMap<String, u64>,
    pub pr_starts: BTreeMap<String, Starts>,
}

impl CiTriageMemory {
    fn fresh(&self, now: u64, window_ms: u64) -> Self {
        let alive = |at: u64| now.saturating_sub(at) < window_ms;
        Self {
            runs: self
                .runs
                .iter()
                .filter(|(_, at)| alive(**at))
                .map(|(key, at)| (key.clone(), *at))
                .collect(),
            pr_starts: self
                .pr_starts
                .iter()
                .filter(|(_, starts)| alive(starts.at))
                .map(|(key, starts)| (key.clone(), *starts))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksObserved {
    pub repo_id: String,
    pub pr_number: u64,
    pub branch: String,
    #[serde(default)]
    pub head_sha: Option<String>,
    pub checks: Vec<Check>,
    // Why optional: Orca treats a failed author lookup as "not yours".
    #[serde(default)]
    pub viewer_is_author: Option<bool>,
    pub trigger: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CiTriageDecision {
    NoLinkedFailure,
    Rerunning,
    NoWorkspace,
    AlreadyTriaged,
    CapReached,
    NotAuthor,
    Started,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTriageOutcome {
    pub decision: CiTriageDecision,
    pub failed: Vec<TriagedCheck>,
    pub runs: Vec<TriagedCheck>,
    pub workspace_id: Option<String>,
    pub pr_starts: u32,
}

// Why: approximates `localeCompare`, which orders by letter before case and puts lowercase first.
fn locale_compare(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

pub fn failed_checks_to_triage(checks: &[Check]) -> Vec<TriagedCheck> {
    let mut failed: Vec<TriagedCheck> = checks
        .iter()
        .filter(|check| check.is_triage_failure())
        .map(|check| TriagedCheck {
            name: check.name.clone(),
            url: check.url.clone(),
        })
        .collect();
    failed.sort_by(|a, b| locale_compare(&a.name, &b.name));
    failed
}

fn json_key(value: serde_json::Value) -> String {
    value.to_string()
}

pub fn decide_ci_triage(
    observed: &ChecksObserved,
    workspaces: &[Workspace],
    memory: &CiTriageMemory,
    now: u64,
    max_per_pr: u32,
    window_ms: u64,
) -> Option<(CiTriageOutcome, Option<CiTriageMemory>)> {
    let failed = failed_checks_to_triage(&observed.checks);
    if failed.is_empty() {
        return None;
    }
    let outcome =
        |decision, runs: Vec<TriagedCheck>, workspace_id: Option<String>, starts| CiTriageOutcome {
            decision,
            failed: failed.clone(),
            runs,
            workspace_id,
            pr_starts: starts,
        };
    // Why: a status without a link only sums up runs that failed themselves, and has no log to read.
    let linked: Vec<TriagedCheck> = failed
        .iter()
        .filter(|check| check.url.is_some())
        .cloned()
        .collect();
    if linked.is_empty() {
        return Some((
            outcome(CiTriageDecision::NoLinkedFailure, Vec::new(), None, 0),
            None,
        ));
    }
    let rerunning: BTreeSet<&str> = observed
        .checks
        .iter()
        .filter(|check| check.is_running())
        .map(|check| check.name.as_str())
        .collect();
    if failed
        .iter()
        .any(|check| rerunning.contains(check.name.as_str()))
    {
        return Some((
            outcome(CiTriageDecision::Rerunning, Vec::new(), None, 0),
            None,
        ));
    }
    let branch = observed
        .branch
        .strip_prefix("refs/heads/")
        .unwrap_or(&observed.branch);
    let Some(workspace) = workspaces.iter().find(|workspace| {
        workspace.repo_id == observed.repo_id && workspace.branch_name() == Some(branch)
    }) else {
        return Some((
            outcome(CiTriageDecision::NoWorkspace, Vec::new(), None, 0),
            None,
        ));
    };
    let run_key = |check: &TriagedCheck| {
        json_key(serde_json::json!([
            observed.repo_id,
            observed.pr_number,
            check.url
        ]))
    };
    let mut fresh = memory.fresh(now, window_ms);
    let untriaged: Vec<TriagedCheck> = linked
        .iter()
        .filter(|check| !fresh.runs.contains_key(&run_key(check)))
        .cloned()
        .collect();
    let pr_key = json_key(serde_json::json!([observed.repo_id, observed.pr_number]));
    let pr_starts = fresh
        .pr_starts
        .get(&pr_key)
        .map_or(0, |starts| starts.count);
    if untriaged.is_empty() {
        return Some((
            outcome(CiTriageDecision::AlreadyTriaged, Vec::new(), None, 0),
            None,
        ));
    }
    // Why: each pushed fix is a new head, so a fix that keeps failing would otherwise triage forever.
    if pr_starts >= max_per_pr {
        return Some((
            outcome(CiTriageDecision::CapReached, Vec::new(), None, pr_starts),
            None,
        ));
    }
    for check in &untriaged {
        fresh.runs.insert(run_key(check), now);
    }
    fresh.pr_starts.insert(
        pr_key,
        Starts {
            count: pr_starts + 1,
            at: now,
        },
    );
    // Why the memory is spent before the author check: Orca records it first, so a review PR uses it up too.
    if observed.viewer_is_author != Some(true) {
        return Some((
            outcome(CiTriageDecision::NotAuthor, Vec::new(), None, 0),
            Some(fresh),
        ));
    }
    let mut runs = untriaged;
    runs.extend(failed.iter().filter(|check| check.url.is_none()).cloned());
    Some((
        outcome(
            CiTriageDecision::Started,
            runs,
            Some(workspace.id.clone()),
            pr_starts + 1,
        ),
        Some(fresh),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedPr {
    pub number: u64,
    pub lifecycle: PrLifecycle,
    pub checks_status: Option<CheckState>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedWorkspace {
    pub repo_id: String,
    pub ci_triage_enabled: bool,
    pub branch: String,
    pub pr: Option<WatchedPr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTriageFetchTarget {
    pub repo_id: String,
    pub pr_number: u64,
    pub branch: String,
    pub head_sha: Option<String>,
}

pub fn ci_triage_fetch_targets(workspaces: &[WatchedWorkspace]) -> Vec<CiTriageFetchTarget> {
    workspaces
        .iter()
        .filter_map(|workspace| {
            let pr = workspace.pr.as_ref()?;
            let open = matches!(pr.lifecycle, PrLifecycle::Open | PrLifecycle::Draft);
            (workspace.ci_triage_enabled && pr.checks_status == Some(CheckState::Failure) && open)
                .then(|| CiTriageFetchTarget {
                    repo_id: workspace.repo_id.clone(),
                    pr_number: pr.number,
                    branch: workspace.branch.clone(),
                    head_sha: pr.head_sha.clone(),
                })
        })
        .collect()
}
