//! The workspace button: what the project's automation script returned for it, run on click; and
//! `workspace.merge`, which takes the merge step GitHub allows for the workspace's PR.

use std::sync::Arc;

use icarus_git::LocalState;
use icarus_protocol::RpcError;
use icarus_rules::model::PrOperation;
use icarus_rules::next_action::{MergeStep, merge_step};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentOptions;
use crate::github::PrView;
use crate::pr_refresh::RefreshReason;
use crate::server::{Server, params, rpc};
use crate::state::Workspace;

pub(crate) const PR_TEMPLATES: [&str; 5] = [
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
];

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
    choose_agent(settings, None).provider
}

/// `agents.default`, else `preferred`, else Claude Code; `agents.commands.<provider>` overrides its command line.
pub(crate) fn choose_agent(settings: &Value, preferred: Option<&str>) -> AgentChoice {
    let provider = setting_str(settings, &["agents", "default"])
        .or(preferred.filter(|p| !p.trim().is_empty()))
        .unwrap_or("claude")
        .to_owned();
    let command = configured_command(settings, &provider);
    AgentChoice {
        provider,
        model: None,
        command,
        remote_control: remote_control_default(settings),
    }
}

/// `agents.remote_control`: start new Claude Code sessions with Remote Control (default off).
pub(crate) fn remote_control_default(settings: &Value) -> bool {
    setting(settings, &["agents", "remote_control"]).and_then(Value::as_bool) == Some(true)
}

fn merge_label(step: &MergeStep) -> &'static str {
    match step {
        MergeStep::Merge => "Merge",
        MergeStep::EnableAutoMerge => "Turn on auto-merge",
        MergeStep::DisableAutoMerge => "Turn off auto-merge",
        MergeStep::AddToMergeQueue => "Add to merge queue",
        MergeStep::MergeStack { .. } => "Merge stack",
        MergeStep::EnableStackAutoMerge => "Turn on stack auto-merge",
        MergeStep::DisableStackAutoMerge => "Turn off stack auto-merge",
    }
}

impl Server {
    async fn local_state(&self, workspace: &Workspace) -> Result<LocalState, RpcError> {
        let path = workspace.path.clone();
        tokio::task::spawn_blocking(move || icarus_git::local_state(&path))
            .await
            .map_err(|e| rpc("internal", e))?
            .map_err(|e| rpc("git_failed", format!("{e:#}")))
    }

    /// The worktree's git state and its pull request, as far as GitHub has answered.
    pub(crate) async fn local_and_pr(
        self: &Arc<Self>,
        workspace: &Workspace,
    ) -> Result<(LocalState, PrView), RpcError> {
        let local = self.local_state(workspace).await?;
        self.note_local_state(workspace, &local);
        let view = self
            .pr_view(workspace, local.branch.as_deref(), false)
            .await;
        Ok((local, view))
    }

    /// What `workspace.next_action` answers, and what the `workspace.next_action` event carries.
    pub(crate) async fn next_action_reply(
        self: &Arc<Self>,
        workspace: &Workspace,
    ) -> Result<Value, RpcError> {
        let (local, view) = self.local_and_pr(workspace).await?;
        let mut reply = json!({
            "workspace": workspace.id,
            "action": null,
            "buttons": [],
            "local": local,
            "known": view.known,
            "error": view.error,
            "decided_by": null,
        });
        if let Some(buttons) = self.automations.buttons(&workspace.id) {
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
        }
        let p: P = params(p)?;
        let workspace = self
            .store
            .workspace(&p.id)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", format!("workspace {}", p.id)))?;
        match method {
            "workspace.next_action" => {
                if p.selected {
                    self.note_selected(&workspace).await;
                }
                self.next_action_reply(&workspace).await
            }
            "workspace.merge" => self.take_merge_step(&workspace).await,
            _ => {
                let buttons = self.automations.buttons(&workspace.id).ok_or_else(|| {
                    rpc(
                        "no_action",
                        "no automation script gave this workspace a button",
                    )
                })?;
                self.run_script_button(&workspace, &buttons, p.button).await
            }
        }
    }

    /// Runs the merge step GitHub allows for the workspace's PR (the scope's `merge_step`).
    async fn take_merge_step(self: &Arc<Self>, workspace: &Workspace) -> Result<Value, RpcError> {
        let (_, view) = self.local_and_pr(workspace).await?;
        let (Some(repo), Some(details), Some(pr)) = (&view.repo, &view.details, &view.pr) else {
            return Err(rpc("no_pull_request", "the pull request is not known"));
        };
        let step = merge_step(pr)
            .ok_or_else(|| rpc("no_action", "the pull request can't be merged now"))?;
        let result = match step {
            MergeStep::EnableStackAutoMerge => {
                let stack = pr
                    .stack
                    .as_ref()
                    .ok_or_else(|| rpc("github_failed", "not in a stack"))?;
                self.enable_stack_auto_merge(repo, pr.number, stack.number, None)
            }
            step => {
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
        }
        .map_err(|e| rpc("github_failed", e))?;
        self.github.forget_stacks();
        self.schedule_pr_refresh(&workspace.id, RefreshReason::Action);
        Ok(json!({ "step": step, "label": merge_label(&step), "github": result }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prefers_the_default_then_the_caller() {
        let settings = json!({
            "agents": { "default": "codex", "commands": { "codex": ["fake-codex"] } },
        });
        let choice = choose_agent(&settings, Some("other"));
        assert_eq!(choice.provider, "codex");
        assert_eq!(choice.command, Some(vec!["fake-codex".to_owned()]));
        assert_eq!(choose_agent(&json!({}), Some("codex")).provider, "codex");
        assert_eq!(choose_agent(&json!({}), None).provider, "claude");
    }
}
