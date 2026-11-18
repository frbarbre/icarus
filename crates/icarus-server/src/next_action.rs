//! The next-action button (plan §7d): one next step per workspace, run as a new agent session or a GitHub call.

use std::path::Path;
use std::sync::Arc;

use icarus_git::LocalState;
use icarus_protocol::RpcError;
use icarus_rules::model::PrOperation;
use icarus_rules::next_action::{LocalGitState, MergeStep, NextAction, next_action};
use icarus_rules::prompts::{
    ActionPromptId, resolve_action_prompt_template, resolve_pull_request_conflicts_prompt,
};
use icarus_rules::template::{Variables, render_action_prompt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentOptions;
use crate::github::PrView;
use crate::pr_refresh::RefreshReason;
use crate::server::{Server, params, rpc};
use crate::state::Workspace;

const PR_TEMPLATES: [&str; 5] = [
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
];

pub(crate) fn label(action: NextAction) -> &'static str {
    match action {
        NextAction::ResolveConflicts => "Resolve conflicts",
        NextAction::CreatePullRequest => "Create PR",
        NextAction::CommitAndPush => "Commit & push",
        NextAction::ReadyForReview => "Ready for review",
        NextAction::Merge(MergeStep::Merge) => "Merge",
        NextAction::Merge(MergeStep::EnableAutoMerge) => "Turn on auto-merge",
        NextAction::Merge(MergeStep::DisableAutoMerge) => "Turn off auto-merge",
        NextAction::Merge(MergeStep::AddToMergeQueue) => "Add to merge queue",
        NextAction::Merge(MergeStep::MergeStack { .. }) => "Merge stack",
        NextAction::Merge(MergeStep::EnableStackAutoMerge) => "Turn on stack auto-merge",
        NextAction::Merge(MergeStep::DisableStackAutoMerge) => "Turn off stack auto-merge",
    }
}

fn is_agent_step(action: NextAction) -> bool {
    !matches!(action, NextAction::Merge(_))
}

pub(crate) fn action_json(action: Option<NextAction>) -> Value {
    action.map_or(Value::Null, |action| {
        json!({ "kind": action.name(), "label": label(action), "agent_step": is_agent_step(action) })
    })
}

pub(crate) fn setting<'a>(settings: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(settings, |value, key| value.get(*key))
}

fn setting_str<'a>(settings: &'a Value, path: &[&str]) -> Option<&'a str> {
    setting(settings, path)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// How an agent session is launched: which provider, model and command line.
pub(crate) struct AgentChoice {
    pub provider: String,
    pub model: Option<String>,
    pub command: Option<Vec<String>>,
    pub remote_control: bool,
}

/// `agents.commands.<provider>`: the provider's command line, e.g. the path to a `codex` binary.
pub(crate) fn configured_command(settings: &Value, provider: &str) -> Option<Vec<String>> {
    setting(settings, &["agents", "commands", provider])
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .filter(|c| !c.is_empty())
}

/// The provider for a session that names none: `agents.default`, else Claude Code.
pub(crate) fn default_provider(settings: &Value) -> String {
    choose_agent(settings, None, None).provider
}

/// The first configured provider wins; `agents.commands.<provider>` overrides its command line.
pub(crate) fn choose_agent(
    settings: &Value,
    kind: Option<&str>,
    preferred: Option<&str>,
) -> AgentChoice {
    let step = kind.and_then(|kind| setting_str(settings, &["actions", kind, "agent"]));
    let provider = step
        .or_else(|| setting_str(settings, &["agents", "default"]))
        .or(preferred.filter(|p| !p.trim().is_empty()))
        .unwrap_or("claude")
        .to_owned();
    let command = configured_command(settings, &provider);
    let model = kind
        .and_then(|kind| setting_str(settings, &["actions", kind, "model"]))
        .map(str::to_owned);
    AgentChoice {
        provider,
        model,
        command,
        remote_control: remote_control_default(settings),
    }
}

/// `agents.remote_control`: start new Claude Code sessions with Remote Control (default off).
pub(crate) fn remote_control_default(settings: &Value) -> bool {
    setting(settings, &["agents", "remote_control"]).and_then(Value::as_bool) == Some(true)
}

fn base_name(base: &str) -> &str {
    base.strip_prefix("origin/").unwrap_or(base)
}

fn template_rule(worktree: &Path) -> String {
    match PR_TEMPLATES.iter().find(|p| worktree.join(p).is_file()) {
        Some(path) => format!("Fill in the repository's pull request template ({path})."),
        None => "There is no pull request template; describe what changed and why, and how it was tested.".to_owned(),
    }
}

fn local_conflict_prompt(path: &str) -> String {
    format!(
        "A merge or rebase in this worktree ({path}) stopped on conflicts.\n\n\
         - Start with git status to see the operation in progress and the unmerged paths.\n\
         - Resolve each conflict by reading both sides and the surrounding code; do not take ours or theirs wholesale unless that is clearly right.\n\
         - Stage each resolved path and continue the operation (git merge --continue or git rebase --continue) until it completes.\n\
         - Do not abort the operation, reset, stash or push.\n\n\
         Reply with the decision for each file and the final git status."
    )
}

impl Server {
    async fn local_state(&self, workspace: &Workspace) -> Result<LocalState, RpcError> {
        let path = workspace.path.clone();
        tokio::task::spawn_blocking(move || icarus_git::local_state(&path))
            .await
            .map_err(|e| rpc("internal", e))?
            .map_err(|e| rpc("git_failed", format!("{e:#}")))
    }

    pub(crate) async fn resolve_next(
        self: &Arc<Self>,
        workspace: &Workspace,
    ) -> Result<(Option<NextAction>, LocalState, PrView), RpcError> {
        let local = self.local_state(workspace).await?;
        self.note_local_state(workspace, &local);
        let view = self
            .pr_view(workspace, local.branch.as_deref(), false)
            .await;
        let git = LocalGitState {
            has_uncommitted_changes: local.has_uncommitted_changes,
            has_upstream: local.upstream.is_some(),
            unpushed_commits: local.unpushed_commits,
            has_local_conflicts: local.has_local_conflicts,
            is_default_branch: local.is_default_branch,
            head_oid: local.head_oid.clone(),
        };
        let action = next_action(&git, view.pr.as_ref(), view.known);
        Ok((action, local, view))
    }

    /// What `workspace.next_action` answers, and what the `workspace.next_action` event carries.
    pub(crate) async fn next_action_reply(
        self: &Arc<Self>,
        workspace: &Workspace,
    ) -> Result<Value, RpcError> {
        let (action, local, view) = self.resolve_next(workspace).await?;
        let mut reply = json!({
            "workspace": workspace.id,
            "action": action_json(action),
            "local": local,
            "known": view.known,
            "error": view.error,
            "decided_by": "built-in",
        });
        if let Some(buttons) = self.automations.buttons(&workspace.id) {
            reply["suggested_action"] = reply["action"].take();
            reply["action"] = buttons
                .list
                .first()
                .map(crate::automations::button_action)
                .unwrap_or(Value::Null);
            reply["buttons"] = json!(buttons.list);
            reply["decided_by"] = json!(buttons.run);
        }
        Ok(reply)
    }

    /// Starts a new agent session in the workspace and sends it the prompt.
    pub(crate) async fn start_agent_session(
        &self,
        workspace: &Workspace,
        choice: AgentChoice,
        title: &str,
        prompt: &str,
    ) -> anyhow::Result<String> {
        // As Orca: the workspace's default agent tab, still unused, takes the prompt instead of a new tab.
        let unused = self.agents.list().into_iter().find(|agent| {
            agent.workspace.as_deref() == Some(workspace.id.as_str())
                && agent.turns == 0
                && agent.provider == choice.provider
        });
        if let Some(agent) = unused.and_then(|info| self.agents.get(&info.id)) {
            self.offer_agent_tab(&agent.info());
            agent.send(prompt, &[]).await?;
            return Ok(agent.id().to_owned());
        }
        let mut options = AgentOptions {
            provider: choice.provider,
            cwd: workspace.path.clone(),
            workspace: Some(workspace.id.clone()),
            title: Some(title.to_owned()),
            model: choice.model,
            bypass_permissions: true,
            resume: None,
            command: choice.command,
            env: Default::default(),
            account: None,
            remote_control: choice.remote_control,
            permission_mode: None,
        };
        self.apply_account(&mut options, None)
            .map_err(|e| anyhow::anyhow!(e.message))?;
        let agent = self.agents.create(options).await?;
        self.offer_agent_tab(&agent.info());
        agent.send(prompt, &[]).await?;
        Ok(agent.id().to_owned())
    }

    fn agent_prompt(
        &self,
        action: NextAction,
        workspace: &Workspace,
        local: &LocalState,
        view: &PrView,
        settings: &Value,
    ) -> String {
        let path = workspace.path.to_string_lossy().into_owned();
        let pr = view.pr.as_ref();
        if action == NextAction::ResolveConflicts {
            return match pr.filter(|_| !local.has_local_conflicts) {
                Some(pr) => {
                    resolve_pull_request_conflicts_prompt(Some(&pr.base_ref_name), &[], Some(&path))
                }
                None => local_conflict_prompt(&path),
            };
        }
        let id = match action {
            NextAction::CreatePullRequest => ActionPromptId::CreatePullRequest,
            NextAction::ReadyForReview => ActionPromptId::ReadyForReview,
            _ => ActionPromptId::CommitAndPush,
        };
        let template =
            resolve_action_prompt_template(setting_str(settings, &["prompts", id.key()]), id);
        let base = pr
            .map(|pr| pr.base_ref_name.clone())
            .or_else(|| {
                workspace
                    .base_ref
                    .as_deref()
                    .map(|b| base_name(b).to_owned())
            })
            .unwrap_or_default();
        let draft = setting(settings, &["actions", action.name(), "draft"])
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut vars = Variables::new();
        let mut set = |k: &str, v: String| {
            vars.insert(k.to_owned(), v);
        };
        set(
            "branch",
            local
                .branch
                .clone()
                .or_else(|| workspace.branch.clone())
                .unwrap_or_default(),
        );
        set("baseRef", base);
        set("worktreePath", path);
        set(
            "draftMode",
            if draft {
                "as a draft"
            } else {
                "ready for review"
            }
            .to_owned(),
        );
        set("templateRule", template_rule(&workspace.path));
        set(
            "prInstructions",
            setting_str(settings, &["actions", action.name(), "instructions"])
                .unwrap_or_default()
                .to_owned(),
        );
        if let Some(pr) = pr {
            set("prNumber", pr.number.to_string());
            set("prUrl", pr.url.clone());
            set("prTitle", pr.title.clone());
        }
        render_action_prompt(template, &vars)
    }

    pub(crate) async fn next_action_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            id: String,
            #[serde(default)]
            selected: bool,
            /// A script button's id; the main one when absent.
            button: Option<String>,
            /// Run the built-in step even when a script decides the buttons (a script's own action).
            #[serde(default)]
            builtin: bool,
        }
        let p: P = params(p)?;
        let workspace = self
            .store
            .workspace(&p.id)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", format!("workspace {}", p.id)))?;
        if method == "workspace.next_action" {
            if p.selected {
                self.note_selected(&workspace).await;
            }
            return self.next_action_reply(&workspace).await;
        }
        if !p.builtin
            && let Some(buttons) = self.automations.buttons(&workspace.id)
        {
            return self.run_script_button(&workspace, &buttons, p.button).await;
        }
        let (action, local, view) = self.resolve_next(&workspace).await?;
        let action = action.ok_or_else(|| rpc("no_action", "nothing to do for this workspace"))?;
        let project = self
            .store
            .project(&workspace.project_id)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", "project"))?;
        let settings = self.resolved_settings(Some(&project.root));
        if is_agent_step(action) {
            let prompt = self.agent_prompt(action, &workspace, &local, &view, &settings);
            let choice = choose_agent(&settings, Some(action.name()), None);
            let id = self
                .start_agent_session(&workspace, choice, label(action), &prompt)
                .await
                .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
            self.note_action_agent(&id);
            return Ok(
                json!({ "action": action_json(Some(action)), "started_agent": id, "github": null }),
            );
        }
        let (Some(repo), Some(details), Some(pr)) = (&view.repo, &view.details, &view.pr) else {
            return Err(rpc("no_pull_request", "the pull request is not known"));
        };
        let result = match action {
            NextAction::Merge(MergeStep::EnableStackAutoMerge) => {
                let stack = pr
                    .stack
                    .as_ref()
                    .ok_or_else(|| rpc("github_failed", "not in a stack"))?;
                self.enable_stack_auto_merge(repo, pr.number, stack.number, None)
            }
            NextAction::Merge(step) => {
                let operation = match step {
                    MergeStep::Merge => PrOperation::Merge,
                    MergeStep::EnableAutoMerge => PrOperation::EnableAutoMerge,
                    MergeStep::DisableAutoMerge => PrOperation::DisableAutoMerge,
                    MergeStep::AddToMergeQueue => PrOperation::AddToMergeQueue,
                    MergeStep::MergeStack { .. } => PrOperation::MergeStack,
                    _ => PrOperation::DisableStackAutoMerge,
                };
                self.run_pr_operation(repo, details, pr, view.metadata.as_ref(), operation)
                    .await
            }
            _ => Err("not a GitHub step".to_owned()),
        }
        .map_err(|e| rpc("github_failed", e))?;
        self.github.forget_stacks();
        self.schedule_pr_refresh(&workspace.id, RefreshReason::Action);
        Ok(json!({ "action": action_json(Some(action)), "started_agent": null, "github": result }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prefers_the_step_then_the_default_then_the_rule() {
        let settings = json!({
            "actions": { "merge": { "agent": "codex" } },
            "agents": { "default": "claude", "commands": { "codex": ["fake-codex"] } },
        });
        let step = choose_agent(&settings, Some("merge"), Some("other"));
        assert_eq!(step.provider, "codex");
        assert_eq!(step.command, Some(vec!["fake-codex".to_owned()]));
        assert_eq!(
            choose_agent(&settings, Some("commit-and-push"), Some("other")).provider,
            "claude"
        );
        assert_eq!(
            choose_agent(&json!({}), None, Some("codex")).provider,
            "codex"
        );
        assert_eq!(choose_agent(&json!({}), None, None).provider, "claude");
    }

    #[test]
    fn every_step_has_the_contract_label() {
        assert_eq!(label(NextAction::CreatePullRequest), "Create PR");
        assert_eq!(
            label(NextAction::Merge(MergeStep::EnableStackAutoMerge)),
            "Turn on stack auto-merge"
        );
        assert!(is_agent_step(NextAction::CommitAndPush));
        assert!(!is_agent_step(NextAction::Merge(MergeStep::Merge)));
    }
}
