//! Targeted refreshes of one workspace's pull request, after the things that move it (Orca's
//! pr-refresh-coordinator): a push, sync or pull, a next-action step finishing, a workspace
//! becoming the selected one while its copy is stale, and GitHub still computing mergeability.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use icarus_git::LocalState;
use icarus_github::rate::PollTier;
use icarus_github::snapshot::PrTarget;
use icarus_rules::facts::Mergeable;
use serde_json::json;
use tokio::time::Instant;

use crate::github::{PR_POLL, local_branch, lock, repo_ref};
use crate::server::Server;
use crate::state::Workspace;

/// GitHub takes a moment to attach a push to its pull request.
pub(crate) const POST_PUSH_DELAY: Duration = Duration::from_millis(2500);
pub(crate) const MERGEABILITY_PENDING_DELAY: Duration = Duration::from_millis(2500);
/// Just past the poll, so a selected workspace only gets its own query when the poll fell behind.
pub(crate) const SELECTED_STALE: Duration = PR_POLL.saturating_add(Duration::from_secs(5));
pub(crate) const NEXT_ACTION_TOPIC: &str = "workspace.next_action";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    Activation,
    Action,
    PostPush,
    MergeabilityPending,
}

impl RefreshReason {
    fn delay(self) -> Duration {
        match self {
            Self::Activation | Self::Action => Duration::ZERO,
            Self::PostPush => POST_PUSH_DELAY,
            Self::MergeabilityPending => MERGEABILITY_PENDING_DELAY,
        }
    }
}

#[derive(Default)]
pub(crate) struct PrRefreshes {
    due: HashMap<String, (Instant, RefreshReason)>,
    /// Workspaces whose refresh query is out now, so a selection can't start a second one.
    running: HashSet<String>,
    synced_heads: HashMap<String, (Option<String>, bool)>,
    local_states: HashMap<String, LocalState>,
    action_agents: HashSet<String>,
}

impl Server {
    /// Refreshes the workspace's pull request after `reason`'s delay; one already due sooner absorbs it.
    pub(crate) fn schedule_pr_refresh(self: &Arc<Self>, workspace_id: &str, reason: RefreshReason) {
        let due = Instant::now() + reason.delay();
        {
            let mut refreshes = lock(&self.github.refreshes);
            if let Some(pending) = refreshes.due.get_mut(workspace_id) {
                if due < pending.0 {
                    *pending = (due, reason);
                }
                return;
            }
            refreshes.due.insert(workspace_id.to_owned(), (due, reason));
        }
        let server = Arc::clone(self);
        let workspace_id = workspace_id.to_owned();
        tokio::spawn(async move {
            let reason = loop {
                let due = {
                    let mut refreshes = lock(&server.github.refreshes);
                    match refreshes.due.get(&workspace_id).copied() {
                        Some((due, reason)) if due <= Instant::now() => {
                            refreshes.due.remove(&workspace_id);
                            refreshes.running.insert(workspace_id.clone());
                            break reason;
                        }
                        Some((due, _)) => due,
                        None => return,
                    }
                };
                tokio::time::sleep_until(due).await;
            };
            server.refresh_workspace_pr(&workspace_id, reason).await;
            lock(&server.github.refreshes).running.remove(&workspace_id);
        });
    }

    async fn refresh_workspace_pr(self: &Arc<Self>, workspace_id: &str, reason: RefreshReason) {
        let Ok(Some(workspace)) = self.store.workspace(workspace_id) else {
            return;
        };
        let Ok(Some(project)) = self.store.project(&workspace.project_id) else {
            return;
        };
        let Some(repo) = repo_ref(&project) else {
            return;
        };
        let Some(branch) = local_branch(&workspace)
            .await
            .or_else(|| workspace.branch.clone())
        else {
            return;
        };
        if self.github.client.budget().tier() == PollTier::Stopped {
            return;
        }
        let before = self.github.cached_pr_summary(&project.id, &branch);
        let target = PrTarget::Branch(branch.clone());
        let pull = match self
            .github
            .client
            .pr_snapshot(&repo, std::slice::from_ref(&target))
            .await
        {
            Ok(snapshot) => snapshot
                .entries
                .into_iter()
                .find(|entry| entry.target == target)
                .and_then(|entry| entry.pull),
            Err(error) => {
                tracing::warn!("{}: pull request refresh failed: {error}", workspace.name);
                return;
            }
        };
        let pending = pull
            .as_ref()
            .is_some_and(|details| details.pr.mergeable == Some(Mergeable::Unknown));
        self.github.store_branch_pr(&project.id, &branch, pull);
        self.save_pr_cache();
        if pending && reason != RefreshReason::MergeabilityPending {
            self.schedule_pr_refresh(workspace_id, RefreshReason::MergeabilityPending);
        }
        if self.github.cached_pr_summary(&project.id, &branch) != before {
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": [workspace.id], "source": "github" }),
            );
        }
        self.publish_next_action(workspace);
    }

    /// Tells clients the workspace's next step, so the button changes without waiting for a poll.
    pub(crate) fn publish_next_action(self: &Arc<Self>, workspace: Workspace) {
        let server = Arc::clone(self);
        tokio::spawn(async move {
            match server.next_action_reply(&workspace).await {
                Ok(reply) => {
                    server.broadcast(NEXT_ACTION_TOPIC, reply);
                }
                Err(error) => tracing::debug!("next action of {}: {}", workspace.id, error.message),
            }
        });
    }

    /// A push from anywhere (an agent, a terminal) shows up locally as the head now matching its
    /// upstream; that is when GitHub has something new to say about the pull request.
    pub(crate) fn note_local_state(self: &Arc<Self>, workspace: &Workspace, local: &LocalState) {
        let synced = local.upstream.is_some() && local.unpushed_commits == 0;
        let seen = (local.head_oid.clone(), synced);
        let (previous, previous_local) = {
            let mut refreshes = lock(&self.github.refreshes);
            (
                refreshes
                    .synced_heads
                    .insert(workspace.id.clone(), seen.clone()),
                refreshes
                    .local_states
                    .insert(workspace.id.clone(), local.clone()),
            )
        };
        if synced && previous.is_some_and(|previous| previous != seen) {
            self.schedule_pr_refresh(&workspace.id, RefreshReason::PostPush);
        }
        // Edits in a terminal or by an agent change no Icarus state, so the scripts would not hear of them.
        if previous_local.is_some_and(|previous| &previous != local) {
            self.automations.observe(crate::gitops::GIT_CHANGED_TOPIC);
        }
    }

    /// The workspace is the one on screen: refresh its pull request if the poll hasn't lately.
    pub(crate) async fn note_selected(self: &Arc<Self>, workspace: &Workspace) {
        let Some(branch) = local_branch(workspace)
            .await
            .or_else(|| workspace.branch.clone())
        else {
            return;
        };
        let fresh = self
            .github
            .fetched_at(&workspace.project_id, &branch)
            .is_some_and(|at| at.elapsed() < SELECTED_STALE);
        let underway = {
            let refreshes = lock(&self.github.refreshes);
            refreshes.running.contains(&workspace.id) || refreshes.due.contains_key(&workspace.id)
        };
        if !fresh && !underway {
            self.schedule_pr_refresh(&workspace.id, RefreshReason::Activation);
        }
    }

    pub(crate) fn note_action_agent(&self, agent_id: &str) {
        lock(&self.github.refreshes)
            .action_agents
            .insert(agent_id.to_owned());
    }

    /// A next-action agent's turn may have changed the PR without touching git (marking it ready),
    /// so it always refreshes; any other turn only does when it pushed.
    pub(crate) fn agent_turn_ended(self: &Arc<Self>, agent_id: &str, workspace_id: &str) {
        let Ok(Some(workspace)) = self.store.workspace(workspace_id) else {
            return;
        };
        if lock(&self.github.refreshes)
            .action_agents
            .contains(agent_id)
        {
            self.schedule_pr_refresh(workspace_id, RefreshReason::PostPush);
        }
        self.publish_next_action(workspace);
    }
}
