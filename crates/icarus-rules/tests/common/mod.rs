#![allow(dead_code)]

use icarus_rules::engine::{Classification, classify};
use icarus_rules::facts::{
    Check, CheckConclusion, CheckRunStatus, Facts, PrLink, PullRequest, RepoRef, Snapshot,
    StatusContextState, Workspace,
};
use icarus_rules::orca_preset::{self, OrcaPreset, OrcaPresetInput};
use icarus_rules::{EngineState, Evaluation, Event, PlannedAction, evaluate};
use serde_json::{Value, json};

pub const VIEWER: &str = "me";
pub const PM_TEAM: &str = "acme/pm";

pub fn repo() -> RepoRef {
    RepoRef::new("acme", "app")
}

pub fn pr(number: u64) -> PullRequest {
    PullRequest {
        repo: repo(),
        number,
        title: "Add a thing".to_owned(),
        url: format!("https://github.com/acme/app/pull/{number}"),
        author: Some(VIEWER.to_owned()),
        head_ref_name: "feat/thing".to_owned(),
        base_ref_name: "main".to_owned(),
        head_ref_oid: "abc".to_owned(),
        ..PullRequest::default()
    }
}

pub fn success_run(name: &str) -> Check {
    Check::run(
        name,
        CheckRunStatus::Completed,
        Some(CheckConclusion::Success),
        None,
    )
}

pub fn status_check(name: &str, state: StatusContextState) -> Check {
    Check::status(name, state, None)
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        viewer_login: Some(VIEWER.to_owned()),
        ..Snapshot::default()
    }
}

pub fn with_pm(mut snapshot: Snapshot, members: &[&str]) -> Snapshot {
    snapshot.team_members.insert(
        PM_TEAM.to_owned(),
        members.iter().map(|login| (*login).to_owned()).collect(),
    );
    snapshot
}

pub fn workspace(id: &str, pr_number: u64) -> Workspace {
    Workspace {
        id: id.to_owned(),
        repo_id: "repo-1".to_owned(),
        display_name: id.to_owned(),
        path: format!("/w/{id}"),
        branch: Some("feat/thing".to_owned()),
        status: Some("todo".to_owned()),
        head_oid: "abc".to_owned(),
        pr: Some(PrLink {
            repo: repo(),
            number: pr_number,
        }),
        ..Workspace::default()
    }
}

pub fn rules(overrides: Value) -> Value {
    let mut base = json!({ "enabled": true, "repoIds": ["repo-1"], "ledgerVersion": 1 });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), overrides.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

pub fn preset(overrides: Value) -> OrcaPreset {
    orca_preset::build(&OrcaPresetInput {
        workspace_status_rules: rules(overrides),
        ..OrcaPresetInput::default()
    })
}

pub fn classify_with(
    overrides: Value,
    pr: &PullRequest,
    snapshot: &Snapshot,
) -> Option<Classification> {
    let preset = preset(overrides);
    let group = preset
        .config
        .groups
        .first()
        .expect("the preset has a status group");
    classify(&preset.config, group, pr, snapshot, &preset.state.ledger)
}

pub fn condition(overrides: Value, pr: &PullRequest, snapshot: &Snapshot) -> Option<String> {
    classify_with(overrides, pr, snapshot).and_then(|outcome| outcome.condition)
}

pub fn run(
    preset: &OrcaPreset,
    workspaces: Vec<Workspace>,
    snapshot: Option<Snapshot>,
    events: &[Event],
) -> Evaluation {
    run_with_state(preset, workspaces, snapshot, events, &preset.state, 10_000)
}

pub fn run_with_state(
    preset: &OrcaPreset,
    workspaces: Vec<Workspace>,
    snapshot: Option<Snapshot>,
    events: &[Event],
    state: &EngineState,
    now: u64,
) -> Evaluation {
    evaluate(
        &preset.config,
        &Facts {
            workspaces,
            snapshot,
        },
        events,
        state,
        now,
    )
}

pub fn set_columns(evaluation: &Evaluation) -> Vec<(String, String, Option<String>)> {
    evaluation
        .actions
        .iter()
        .filter_map(|action| match action {
            PlannedAction::SetColumn {
                workspace_id,
                status,
                condition,
                ..
            } => Some((workspace_id.clone(), status.clone(), condition.clone())),
            _ => None,
        })
        .collect()
}

pub fn deletes(evaluation: &Evaluation) -> Vec<(String, bool, Option<String>)> {
    evaluation
        .actions
        .iter()
        .filter_map(|action| match action {
            PlannedAction::DeleteWorkspace {
                workspace_id,
                force,
                forget_handled_key,
                ..
            } => Some((workspace_id.clone(), *force, forget_handled_key.clone())),
            _ => None,
        })
        .collect()
}

pub fn creations(evaluation: &Evaluation) -> Vec<(String, Option<String>)> {
    evaluation
        .actions
        .iter()
        .filter_map(|action| match action {
            PlannedAction::CreateWorkspaceFromPr {
                handled_key,
                since_review_commit,
                ..
            } => Some((handled_key.clone(), since_review_commit.clone())),
            _ => None,
        })
        .collect()
}

pub fn launches(evaluation: &Evaluation) -> Vec<icarus_rules::engine::AgentLaunch> {
    evaluation
        .actions
        .iter()
        .filter_map(|action| match action {
            PlannedAction::LaunchAgent { launch, .. } => Some(launch.clone()),
            _ => None,
        })
        .collect()
}

pub fn syncs(evaluation: &Evaluation) -> Vec<(String, String)> {
    evaluation
        .actions
        .iter()
        .filter_map(|action| match action {
            PlannedAction::SyncToPrHead {
                workspace_id,
                head_oid,
                ..
            } => Some((workspace_id.clone(), head_oid.clone())),
            _ => None,
        })
        .collect()
}
