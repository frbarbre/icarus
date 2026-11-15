//! The rules loop: gathers facts for Icarus-owned workspaces, evaluates the rules and executes the plan.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use icarus_github::snapshot::PrDetails;
use icarus_protocol::RpcError;
use icarus_rules::ci_triage::ChecksObserved;
use icarus_rules::engine::{AgentLaunch, PlannedAction};
use icarus_rules::facts::{
    CheckState, Facts, PrLifecycle, PrLink, PullRequest, RepoRef, Snapshot,
    Workspace as FactWorkspace,
};
use icarus_rules::head_sync::{HeadSyncDecision, HeadSyncObservation, decide_head_sync};
use icarus_rules::model::StatusDef;
use icarus_rules::next_action::stack_merge_target;
use icarus_rules::orca_preset::{OrcaPresetInput, build};
use icarus_rules::{ActionOutcome, EngineState, Event, evaluate};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{repo_ref, same_repo};
use crate::next_action::{choose_agent, setting};
use crate::server::{Server, params, rpc};
use crate::state::{Project, Workspace};
use crate::terminal::{SpawnOptions, default_shell};

const ENGINE_STATE_KEY: &str = "rules.engine_state";
const DEFAULT_INTERVAL_MS: u64 = 60_000;
const MIN_INTERVAL_MS: u64 = 1_000;

#[derive(Default)]
pub struct Automation {
    tick_lock: tokio::sync::Mutex<()>,
    last: Mutex<Option<Value>>,
    ticks: Mutex<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Runs a tick every `rules.interval_ms`; ticks stay dry runs until `rules.armed` is true.
pub(crate) fn spawn(server: Arc<Server>) {
    tokio::spawn(async move {
        loop {
            let interval = server.rules_settings().interval_ms;
            tokio::time::sleep(Duration::from_millis(interval)).await;
            if let Err(error) = server.rules_tick(None, "timer").await {
                tracing::warn!("rules tick failed: {error}");
            }
        }
    });
}

struct RulesSettings {
    enabled: bool,
    armed: bool,
    interval_ms: u64,
    projects: Vec<String>,
    orca: Value,
    settings: Value,
}

struct Target {
    project: Project,
    repo: RepoRef,
}

/// What executing one planned action did, and what the engine should remember of it.
struct Executed {
    ok: bool,
    detail: Value,
    outcome: Option<ActionOutcome>,
}

impl Executed {
    fn ok(detail: Value) -> Self {
        Self {
            ok: true,
            detail,
            outcome: None,
        }
    }

    fn failed(error: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            detail: json!({ "error": error.to_string() }),
            outcome: None,
        }
    }

    fn with(mut self, outcome: Option<ActionOutcome>) -> Self {
        self.outcome = outcome;
        self
    }
}

fn observe_head(path: &Path, branch: &str, target: &str) -> HeadSyncObservation {
    let run = |args: &[&str]| {
        icarus_git::git(path, args)
            .ok()
            .map(|s| s.trim().to_owned())
    };
    let _ = run(&["fetch", "--quiet", "origin", branch]);
    HeadSyncObservation {
        head: run(&["rev-parse", "HEAD"]).unwrap_or_default(),
        symbolic_branch: run(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        dirty_tracked: run(&["status", "--porcelain", "--untracked-files=no"])
            .is_none_or(|s| !s.is_empty()),
        target_reachable: run(&["cat-file", "-e", &format!("{target}^{{commit}}")]).is_some(),
        viewer_email: run(&["config", "user.email"]),
        local_only_authors: run(&["log", "--format=%ae", &format!("{target}..HEAD")])
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default(),
    }
}

fn sync_to_head(path: &Path, branch: &str, target: &str) -> Result<Value, String> {
    let observed = observe_head(path, branch, target);
    let decision = decide_head_sync(branch, target, &observed);
    if let HeadSyncDecision::ResetHard { .. } = decision {
        icarus_git::git(path, &["reset", "--hard", "--quiet", target])
            .map_err(|e| format!("{e:#}"))?;
    }
    serde_json::to_value(decision).map_err(|e| e.to_string())
}

impl Server {
    fn rules_settings(&self) -> RulesSettings {
        let settings = self.resolved_settings(None);
        let rules = setting(&settings, &["rules"])
            .cloned()
            .unwrap_or(Value::Null);
        let orca = rules
            .get("orca_compat")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let enabled = rules
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(orca.get("enabled") == Some(&Value::Bool(true)));
        RulesSettings {
            enabled,
            armed: rules.get("armed") == Some(&Value::Bool(true)),
            interval_ms: rules
                .get("interval_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_INTERVAL_MS)
                .max(MIN_INTERVAL_MS),
            projects: rules
                .get("projects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect(),
            orca,
            settings,
        }
    }

    pub(crate) async fn rules_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "rules.status" => {
                let config = self.rules_settings();
                let last = self
                    .automation
                    .last
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let ticks = *self
                    .automation
                    .ticks
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                Ok(json!({
                    "enabled": config.enabled,
                    "armed": config.armed,
                    "interval_ms": config.interval_ms,
                    "projects": config.projects,
                    "ticks": ticks,
                    "last_tick": last,
                    "stack_auto_merge": self.stack_intents(),
                }))
            }
            "rules.tick" => {
                #[derive(Deserialize)]
                struct P {
                    dry_run: Option<bool>,
                }
                let p: P = params(p)?;
                self.rules_tick(p.dry_run, "rpc")
                    .await
                    .map_err(|e| rpc("rules_failed", e))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    fn engine_state(&self, seed: EngineState) -> EngineState {
        match self.store.kv_get(ENGINE_STATE_KEY) {
            Ok(Some(value)) => serde_json::from_value(value).unwrap_or(seed),
            _ => seed,
        }
    }

    pub(crate) async fn rules_tick(
        self: &Arc<Self>,
        dry_run: Option<bool>,
        trigger: &str,
    ) -> Result<Value, String> {
        let _guard = self.automation.tick_lock.lock().await;
        let started = now_ms();
        let config = self.rules_settings();
        let dry_run = dry_run.unwrap_or(!config.armed);
        let mut errors: Vec<String> = Vec::new();
        let mut targets: Vec<Target> = Vec::new();
        for key in &config.projects {
            match self.store.project(key) {
                Ok(Some(project)) if targets.iter().any(|t| t.project.id == project.id) => {}
                Ok(Some(project)) => match repo_ref(&project) {
                    Some(repo) => targets.push(Target { project, repo }),
                    None => errors.push(format!("project {key} has no GitHub remote")),
                },
                Ok(None) => errors.push(format!("rules.projects: no project {key}")),
                Err(error) => errors.push(error.to_string()),
            }
        }
        let mut report = json!({
            "at_ms": started,
            "trigger": trigger,
            "enabled": config.enabled,
            "armed": config.armed,
            "dry_run": dry_run,
            "projects": targets.iter().map(|t| t.project.name.clone()).collect::<Vec<_>>(),
            "planned": [],
            "executed": [],
            "trace": [],
            "ci_triage": [],
        });
        if !config.enabled || targets.is_empty() {
            report["skipped"] = json!(if config.enabled {
                "no projects in scope (set rules.projects)"
            } else {
                "rules are disabled"
            });
            report["errors"] = json!(errors);
            return Ok(self.finish_tick(report, started));
        }

        let (facts, details, ci_triage) = self.gather_facts(&targets, &config, &mut errors).await;
        // Notifications are news, not actions, so dry runs send them too.
        report["notifications"] = json!(self.notify_facts(&facts));
        let statuses: Vec<StatusDef> = setting(&config.settings, &["workspace", "statuses"])
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let mut orca = config.orca.clone();
        if let Some(object) = orca.as_object_mut() {
            object.insert("enabled".into(), json!(true));
            object.insert(
                "repoIds".into(),
                json!(
                    targets
                        .iter()
                        .map(|t| t.project.id.clone())
                        .collect::<Vec<_>>()
                ),
            );
        }
        let preset = build(&OrcaPresetInput {
            workspace_status_rules: orca,
            statuses,
            ci_triage: ci_triage
                .iter()
                .map(|(id, v)| (id.clone(), v.clone()))
                .collect(),
            fix_checks_agent: choose_agent(&config.settings, Some("fix-checks"), None)
                .provider
                .into(),
        });
        let events = ci_events(&facts, &ci_triage);
        let state = self.engine_state(preset.state.clone());
        let evaluation = evaluate(&preset.config, &facts, &events, &state, started);
        report["planned"] = json!(evaluation.actions);
        report["trace"] = json!(
            evaluation
                .trace
                .iter()
                .map(|t| json!({ "workspace_id": t.workspace_id, "group": t.group, "condition": t.condition, "status": t.status }))
                .collect::<Vec<_>>()
        );
        report["ci_triage"] = json!(
            evaluation
                .ci_triage
                .iter()
                .map(|r| json!({ "rule": r.rule, "pr_number": r.pr_number, "branch": r.branch, "decision": r.outcome.decision }))
                .collect::<Vec<_>>()
        );
        if dry_run {
            report["errors"] = json!(errors);
            return Ok(self.finish_tick(report, started));
        }

        let mut state = evaluation.state;
        let mut executed = Vec::new();
        for action in evaluation.actions {
            let result = self.execute(&action, &targets, &details, &config).await;
            if let Some(outcome) = &result.outcome {
                state.record(outcome);
            }
            if !result.ok {
                errors.push(format!(
                    "{}: {}",
                    action_name(&action),
                    result.detail["error"].as_str().unwrap_or("failed")
                ));
            }
            executed.push(json!({ "action": action, "ok": result.ok, "result": result.detail }));
        }
        executed.extend(self.advance_stack_intents(&mut errors).await);
        if let Ok(value) = serde_json::to_value(&state)
            && let Err(error) = self.store.kv_set(ENGINE_STATE_KEY, &value)
        {
            errors.push(format!("saving the rules state: {error}"));
        }
        report["executed"] = json!(executed);
        report["errors"] = json!(errors);
        Ok(self.finish_tick(report, started))
    }

    fn finish_tick(&self, mut report: Value, started: u64) -> Value {
        report["duration_ms"] = json!(now_ms().saturating_sub(started));
        *self
            .automation
            .ticks
            .lock()
            .unwrap_or_else(|p| p.into_inner()) += 1;
        *self
            .automation
            .last
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(report.clone());
        report
    }

    /// Facts for Icarus-owned workspaces only (D14): those whose worktree is under the server's workspace root.
    async fn gather_facts(
        &self,
        targets: &[Target],
        config: &RulesSettings,
        errors: &mut Vec<String>,
    ) -> (Facts, BTreeMap<String, PrDetails>, Vec<(String, Value)>) {
        let mut workspaces = Vec::new();
        let mut details: BTreeMap<String, PrDetails> = BTreeMap::new();
        let mut viewer: Option<String> = None;
        let mut ci_triage = Vec::new();
        for target in targets {
            let project = &target.project;
            let project_settings = self.resolved_settings(Some(&project.root));
            if let Some(triage) =
                setting(&project_settings, &["project", "ciTriage"]).filter(|v| !v.is_null())
            {
                ci_triage.push((project.id.clone(), triage.clone()));
            }
            let owned: Vec<Workspace> = match self.store.workspaces(Some(&project.id)) {
                Ok(list) => list
                    .into_iter()
                    .filter(|w| w.path.starts_with(&self.workspace_root))
                    .collect(),
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            let prs = match self.project_prs(project, None, true).await {
                Ok(prs) => Some(prs),
                Err(error) => {
                    errors.push(format!("{}: {error}", project.name));
                    None
                }
            };
            if let Some(prs) = &prs {
                viewer = viewer.or_else(|| prs.viewer.clone());
            }
            for workspace in owned {
                let path = workspace.path.clone();
                let local =
                    match tokio::task::spawn_blocking(move || icarus_git::local_state(&path)).await
                    {
                        Ok(Ok(local)) => local,
                        Ok(Err(error)) => {
                            errors.push(format!("{}: {error:#}", workspace.name));
                            continue;
                        }
                        Err(error) => {
                            errors.push(error.to_string());
                            continue;
                        }
                    };
                let branch = local.branch.clone().or_else(|| workspace.branch.clone());
                let pull = branch
                    .as_ref()
                    .and_then(|b| prs.as_ref()?.by_branch.get(b).cloned().flatten());
                if let Some(pull) = &pull {
                    details.insert(pull.pr.match_key(), pull.clone());
                }
                workspaces.push(FactWorkspace {
                    id: workspace.id.clone(),
                    repo_id: project.id.clone(),
                    display_name: workspace.title.clone().unwrap_or(workspace.name.clone()),
                    path: workspace.path.to_string_lossy().into_owned(),
                    branch,
                    status: workspace.status.clone(),
                    head_oid: local.head_oid.clone().unwrap_or_default(),
                    dirty: local.has_uncommitted_changes,
                    has_upstream: local.upstream.is_some(),
                    unpushed_commits: local.unpushed_commits,
                    created_at: Some(workspace.created_at_ms),
                    pr: pull.map(|p| PrLink {
                        repo: p.pr.repo.clone(),
                        number: p.pr.number,
                    }),
                    ..FactWorkspace::default()
                });
            }
        }
        let orca = icarus_rules::orca_preset::normalize(&config.orca);
        let mut review_requested = Vec::new();
        if orca.review_inbox.enabled {
            let repos: Vec<RepoRef> = targets.iter().map(|t| t.repo.clone()).collect();
            match self.github.client.review_inbox(&repos).await {
                Ok(inbox) => {
                    viewer = viewer.or(inbox.viewer_login);
                    review_requested = inbox.pulls.iter().map(|d| d.pr.clone()).collect();
                    for pull in inbox.pulls {
                        details.entry(pull.pr.match_key()).or_insert(pull);
                    }
                }
                Err(error) => errors.push(format!("review inbox: {error}")),
            }
        }
        let mut team_members = BTreeMap::new();
        if let Some(team) = &orca.pm_approval_team {
            match self
                .github
                .client
                .team_roster(std::slice::from_ref(team))
                .await
            {
                Ok(roster) => team_members = roster,
                Err(error) => errors.push(format!("team {team}: {error}")),
            }
        }
        if viewer.is_none() {
            match self.github.client.viewer_login().await {
                Ok(login) => viewer = Some(login),
                Err(error) => errors.push(format!("viewer: {error}")),
            }
        }
        let linked: Vec<PullRequest> = workspaces
            .iter()
            .filter_map(|w| w.pr.as_ref())
            .filter_map(|link| details.get(&link.match_key()).map(|d| d.pr.clone()))
            .collect();
        let facts = Facts {
            workspaces,
            snapshot: Some(Snapshot {
                viewer_login: viewer,
                linked,
                review_requested,
                team_members,
            }),
        };
        (facts, details, ci_triage)
    }

    async fn execute(
        self: &Arc<Self>,
        action: &PlannedAction,
        targets: &[Target],
        details: &BTreeMap<String, PrDetails>,
        config: &RulesSettings,
    ) -> Executed {
        match action {
            PlannedAction::SetColumn {
                workspace_id,
                status,
                ..
            } => match self.store.workspace(workspace_id) {
                Ok(Some(mut workspace)) => {
                    workspace.status = Some(status.clone());
                    match self.store.update_workspace(&workspace) {
                        Ok(()) => Executed::ok(json!({ "status": status })),
                        Err(error) => Executed::failed(error),
                    }
                }
                Ok(None) => Executed::failed(format!("workspace {workspace_id} is gone")),
                Err(error) => Executed::failed(error),
            },
            PlannedAction::DeleteWorkspace {
                workspace_id,
                force,
                forget_handled_key,
                ..
            } => {
                match self
                    .workspace_dispatch(
                        "workspace.remove",
                        json!({ "id": workspace_id, "force": force }),
                    )
                    .await
                {
                    Ok(value) => Executed::ok(value).with(Some(ActionOutcome::Deleted {
                        forget_handled_key: forget_handled_key.clone(),
                    })),
                    // Why no outcome: an unforced delete refused for local work is retried next tick.
                    Err(error) => Executed::failed(error.message),
                }
            }
            PlannedAction::CreateWorkspaceFromPr {
                pr,
                handled_key,
                since_review_commit,
                name,
                display_title,
                initial_column,
                agent,
                layout,
            } => {
                let created = self
                    .create_review_workspace(
                        targets,
                        pr,
                        since_review_commit.as_deref(),
                        name,
                        display_title,
                        initial_column.as_deref(),
                        agent.as_ref(),
                        &config.settings,
                    )
                    .await;
                let result = match (created, layout) {
                    (Ok(mut value), Some(layout)) => {
                        let applied = match value["workspace"].as_str() {
                            Some(id) => match self.workspace_by_key(id) {
                                Ok(workspace) => {
                                    self.apply_layout(&workspace, &layout.0, Some(pr.url.clone()))
                                        .await
                                }
                                Err(error) => Err(error),
                            },
                            None => Err(rpc("internal", "created workspace has no id")),
                        };
                        // The workspace exists either way; a layout problem must not retry the create.
                        value["layout"] = match applied {
                            Ok(layout) => layout,
                            Err(error) => json!({ "error": error.message }),
                        };
                        Ok(value)
                    }
                    (created, _) => created,
                };
                let key = handled_key.clone();
                match result {
                    Ok(value) => {
                        Executed::ok(value).with(Some(ActionOutcome::Created { handled_key: key }))
                    }
                    Err(error) => Executed::failed(error)
                        .with(Some(ActionOutcome::CreateFailed { handled_key: key })),
                }
            }
            PlannedAction::LaunchAgent {
                workspace_id,
                launch,
            } => {
                let Some(workspace_id) = workspace_id else {
                    return Executed::failed("no workspace to launch the agent in");
                };
                match self.store.workspace(workspace_id) {
                    Ok(Some(workspace)) => self.launch(&workspace, launch, &config.settings).await,
                    Ok(None) => Executed::failed(format!("workspace {workspace_id} is gone")),
                    Err(error) => Executed::failed(error),
                }
            }
            PlannedAction::RunCommand {
                workspace_id,
                path,
                command,
                timeout_ms,
            } => self.run_command(workspace_id, path, command, *timeout_ms),
            PlannedAction::SyncToPrHead {
                path,
                branch,
                head_oid,
                ..
            } => {
                let (path, branch, head) = (path.clone(), branch.clone(), head_oid.clone());
                match tokio::task::spawn_blocking(move || {
                    sync_to_head(Path::new(&path), &branch, &head)
                })
                .await
                {
                    Ok(Ok(decision)) => Executed::ok(decision),
                    Ok(Err(error)) => Executed::failed(error),
                    Err(error) => Executed::failed(error),
                }
            }
            PlannedAction::Notify {
                workspace_id,
                message,
            } => {
                let delivered = self.broadcast(
                    "notify",
                    json!({ "workspace_id": workspace_id, "message": message, "source": "rules" }),
                );
                let label = workspace_id
                    .as_deref()
                    .and_then(|id| self.store.workspace(id).ok().flatten())
                    .map(|w| w.title.unwrap_or(w.name));
                let stored = self.notify(crate::notifications::NewNotification {
                    kind: crate::notifications::RULES,
                    subject: format!("{}|{message}", workspace_id.as_deref().unwrap_or("-")),
                    title: label.unwrap_or_else(|| "Automation".to_owned()),
                    body: message.clone(),
                    workspace_id: workspace_id.clone(),
                    data: json!({ "source": "rules" }),
                });
                Executed::ok(json!({
                    "delivered": delivered,
                    "notification": stored.map(|n| n.id),
                }))
            }
            PlannedAction::OpenLayout {
                workspace_id,
                layout,
                pr_url,
            } => {
                let applied = match self.workspace_by_key(workspace_id) {
                    Ok(workspace) => {
                        self.apply_layout(&workspace, &layout.0, pr_url.clone())
                            .await
                    }
                    Err(error) => Err(error),
                };
                match applied {
                    Ok(value) => Executed::ok(value),
                    Err(error) => Executed::failed(error.message),
                }
            }
            PlannedAction::EnableStackAutoMerge {
                pr,
                stack_number,
                method,
            } => match self.enable_stack_auto_merge(&pr.repo, pr.number, *stack_number, *method) {
                Ok(value) => Executed::ok(value),
                Err(error) => Executed::failed(error),
            },
            PlannedAction::PullRequest { pr, operation, .. } => {
                let Some(details) = details.get(&pr.match_key()) else {
                    return Executed::failed("the pull request is not in this tick's snapshot");
                };
                let mut pull = details.pr.clone();
                let metadata = self
                    .merge_metadata(&pr.repo, &pull.base_ref_name)
                    .await
                    .ok();
                if let Some(meta) = &metadata {
                    meta.apply(&mut pull);
                }
                if let Some(view) = self.stack_view(&pr.repo, pr.number).await {
                    pull.stack = Some(view.to_fact(pull.merge_queue_required, None));
                }
                let result = self
                    .run_pr_operation(&pr.repo, details, &pull, metadata.as_ref(), *operation)
                    .await;
                for target in targets.iter().filter(|t| same_repo(&t.repo, &pr.repo)) {
                    self.github.invalidate(&target.project.id);
                }
                match result {
                    Ok(value) => Executed::ok(value),
                    Err(error) => Executed::failed(error),
                }
            }
        }
    }

    async fn launch(
        &self,
        workspace: &Workspace,
        launch: &AgentLaunch,
        settings: &Value,
    ) -> Executed {
        let choice = choose_agent(settings, None, launch.agent.as_deref());
        match self
            .start_agent_session(workspace, choice, &launch.title, &launch.prompt)
            .await
        {
            Ok(id) => Executed::ok(json!({ "agent": id })),
            Err(error) => Executed::failed(format!("{error:#}")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_review_workspace(
        self: &Arc<Self>,
        targets: &[Target],
        pr: &PullRequest,
        since_review_commit: Option<&str>,
        name: &str,
        title: &str,
        initial_column: Option<&str>,
        agent: Option<&AgentLaunch>,
        settings: &Value,
    ) -> Result<Value, String> {
        let target = targets
            .iter()
            .find(|t| same_repo(&t.repo, &pr.repo))
            .or_else(|| targets.first())
            .ok_or("no project in scope")?;
        let created = self
            .workspace_dispatch(
                "workspace.create",
                json!({
                    "project": target.project.id,
                    "name": name,
                    "branch": pr.head_ref_name,
                    "base": format!("origin/{}", pr.head_ref_name),
                    "fetch": true,
                    "status": initial_column,
                    "title": title,
                    "agent": false,
                }),
            )
            .await
            .map_err(|e| e.message)?;
        let id = created["id"].as_str().unwrap_or_default().to_owned();
        let mut workspace = self
            .store
            .workspace(&id)
            .map_err(|e| e.to_string())?
            .ok_or("created workspace is missing")?;
        // Why: the diff of a review compares against the PR base, or the last reviewed commit on a re-request.
        workspace.base_ref = Some(
            since_review_commit
                .map(str::to_owned)
                .unwrap_or_else(|| format!("origin/{}", pr.base_ref_name)),
        );
        workspace.pull_request = Some(json!({ "number": pr.number, "url": pr.url }));
        self.store
            .update_workspace(&workspace)
            .map_err(|e| e.to_string())?;
        let agent = match agent {
            Some(launch) => {
                let launched = self.launch(&workspace, launch, settings).await;
                if !launched.ok {
                    return Err(launched.detail["error"]
                        .as_str()
                        .unwrap_or("agent failed")
                        .to_owned());
                }
                launched.detail["agent"].clone()
            }
            None => Value::Null,
        };
        Ok(json!({ "workspace": id, "agent": agent }))
    }

    fn run_command(
        &self,
        workspace_id: &str,
        path: &str,
        command: &str,
        timeout_ms: u64,
    ) -> Executed {
        let session = match self.terminals.create(SpawnOptions {
            cwd: Path::new(path).to_path_buf(),
            command: Some(vec![default_shell(), "-lc".into(), command.to_owned()]),
            env: Default::default(),
            columns: 120,
            rows: 32,
            label: Some(format!("Rule: {command}")),
            workspace: Some(workspace_id.to_owned()),
        }) {
            Ok(session) => session,
            Err(error) => return Executed::failed(format!("{error:#}")),
        };
        let watched = Arc::clone(&session);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            if watched.is_running() {
                let _ = watched.kill();
            }
        });
        Executed::ok(json!({ "terminal": session.id() }))
    }

    /// Merges stacks whose auto-merge intent is recorded, once GitHub reports them ready.
    async fn advance_stack_intents(&self, errors: &mut Vec<String>) -> Vec<Value> {
        let mut done = Vec::new();
        for (key, intent) in self.stack_intents() {
            let (Some(owner), Some(name), Some(number)) = (
                intent["owner"].as_str(),
                intent["name"].as_str(),
                intent["pr"].as_u64(),
            ) else {
                let _ = self.set_stack_intent(key, None);
                continue;
            };
            let repo = RepoRef::new(owner, name);
            let Ok(details) = self.github.client.pull_request(&repo, number).await else {
                continue;
            };
            let Some(view) = self
                .github
                .client
                .stack_view(&repo, number)
                .await
                .ok()
                .flatten()
            else {
                let _ = self.set_stack_intent(key, None);
                continue;
            };
            let mut pull = details.pr.clone();
            let metadata = self.merge_metadata(&repo, &pull.base_ref_name).await.ok();
            if let Some(meta) = &metadata {
                meta.apply(&mut pull);
            }
            let stack = view.to_fact(pull.merge_queue_required, None);
            if stack_merge_target(&stack).is_none() {
                continue;
            }
            pull.stack = Some(stack);
            let result = self
                .run_pr_operation(
                    &repo,
                    &details,
                    &pull,
                    metadata.as_ref(),
                    icarus_rules::model::PrOperation::MergeStack,
                )
                .await;
            match result {
                Ok(value) => {
                    let _ = self.set_stack_intent(key.clone(), None);
                    done.push(json!({ "action": { "action": "stack_auto_merge", "stack": key }, "ok": true, "result": value }));
                }
                Err(error) => errors.push(format!("stack auto-merge {key}: {error}")),
            }
        }
        done
    }
}

fn action_name(action: &PlannedAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v["action"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "action".to_owned())
}

/// A failed check run on one of the viewer's open PRs, for projects with CI triage set up.
fn ci_events(facts: &Facts, ci_triage: &[(String, Value)]) -> Vec<Event> {
    let Some(snapshot) = &facts.snapshot else {
        return Vec::new();
    };
    let viewer = snapshot.viewer_login.as_deref();
    let mut events = Vec::new();
    for workspace in &facts.workspaces {
        if !ci_triage.iter().any(|(id, _)| *id == workspace.repo_id) {
            continue;
        }
        let Some(link) = &workspace.pr else { continue };
        let Some(pr) = snapshot
            .linked
            .iter()
            .find(|pr| pr.match_key() == link.match_key())
        else {
            continue;
        };
        let open = matches!(pr.lifecycle(), PrLifecycle::Open | PrLifecycle::Draft);
        if open && pr.checks_rollup == Some(CheckState::Failure) {
            events.push(Event::ChecksObserved(ChecksObserved {
                repo_id: workspace.repo_id.clone(),
                pr_number: pr.number,
                branch: pr.head_ref_name.clone(),
                head_sha: Some(pr.head_ref_oid.clone()).filter(|s| !s.is_empty()),
                checks: pr.checks.clone(),
                viewer_is_author: viewer.map(|login| pr.is_authored_by(login)),
                trigger: "poll".to_owned(),
            }));
        }
    }
    events
}
