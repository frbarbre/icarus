//! Review workspaces as server methods for automation scripts, and the stack auto-merge loop.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use icarus_protocol::RpcError;
use icarus_rules::facts::{PullRequest, RepoRef};
use icarus_rules::head_sync::{HeadSyncDecision, HeadSyncObservation, decide_head_sync};
use icarus_rules::next_action::stack_merge_target;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::repo_ref;
use crate::next_action::choose_agent;
use crate::server::{Server, params, rpc};
use crate::state::{Project, Workspace};

const STACK_INTERVAL: Duration = Duration::from_secs(60);

/// Merges stacks with a recorded auto-merge intent once GitHub reports them ready.
pub(crate) fn spawn(server: Arc<Server>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(STACK_INTERVAL).await;
            server.advance_stack_intents().await;
        }
    });
}

#[derive(Clone, Deserialize)]
struct ReviewAgent {
    provider: Option<String>,
    prompt: String,
    title: Option<String>,
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
        target_reachable: run(&["cat-file", "-e", &format!("{target}^{{commit}}")]).is_some(),
        viewer_email: run(&["config", "user.email"]),
        local_only_authors: run(&["log", "--format=%ae", &format!("{target}..HEAD")])
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default(),
    }
}

fn fetch_target(path: &Path, branch: &str) {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    // Why ignored: an unreachable remote still leaves a usable base if the ref was fetched before.
    if let Err(error) =
        icarus_git::git(path, &["fetch", "--quiet", "--no-tags", "origin", &refspec])
    {
        tracing::warn!("fetching the review target {branch}: {error:#}");
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
    pub(crate) async fn review_steps_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "review.workspace" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    number: u64,
                    name: Option<String>,
                    title: Option<String>,
                    status: Option<String>,
                    since: Option<String>,
                    agent: Option<ReviewAgent>,
                    layout: Option<String>,
                }
                let p: P = params(p)?;
                let project = self
                    .store
                    .project(&p.project)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("project {}", p.project)))?;
                let repo = repo_ref(&project)
                    .ok_or_else(|| rpc("invalid_params", "the project has no GitHub remote"))?;
                let inbox = self
                    .github
                    .review_inbox(std::slice::from_ref(&repo))
                    .await
                    .map_err(|e| rpc("github_error", e.to_string()))?;
                let pr = inbox
                    .pulls
                    .into_iter()
                    .map(|d| d.pr)
                    .find(|pr| pr.number == p.number)
                    .ok_or_else(|| {
                        rpc(
                            "not_found",
                            format!("#{} is not in your review inbox", p.number),
                        )
                    })?;
                let name = p.name.unwrap_or_else(|| format!("review-{}", p.number));
                let title = p.title.unwrap_or_else(|| pr.title.clone());
                let mut agent = p.agent;
                if let Some(agent) = &mut agent {
                    agent
                        .title
                        .get_or_insert_with(|| format!("Review #{}", p.number));
                }
                let mut created = self
                    .create_review_workspace(
                        &project,
                        &pr,
                        p.since.as_deref(),
                        &name,
                        &title,
                        p.status.as_deref(),
                        agent,
                    )
                    .await
                    .map_err(|e| rpc("create_failed", e))?;
                if let (Some(layout), Some(id)) = (p.layout, created["workspace"].as_str()) {
                    let workspace = self.workspace_by_key(id)?;
                    created["layout"] = match self
                        .apply_layout(&workspace, &layout, Some(pr.url.clone()))
                        .await
                    {
                        Ok(applied) => applied,
                        Err(error) => json!({ "error": error.message }),
                    };
                }
                Ok(created)
            }
            "workspace.sync_to_pr_head" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let (_, _, view) = self.resolve_next(&workspace).await?;
                let pr = view
                    .pr
                    .ok_or_else(|| rpc("not_found", "the workspace has no pull request"))?;
                let branch = workspace
                    .branch
                    .clone()
                    .ok_or_else(|| rpc("invalid_params", "the workspace has no branch"))?;
                let (path, head) = (workspace.path.clone(), pr.head_ref_oid.clone());
                let mut decision = tokio::task::spawn_blocking(move || {
                    sync_to_head(Path::new(&path), &branch, &head)
                })
                .await
                .map_err(|e| rpc("internal", e))?
                .map_err(|e| rpc("sync_failed", e))?;
                if let Some(base_ref) = self
                    .retarget_base(&workspace.id, &pr.base_ref_name)
                    .await
                    .map_err(|e| rpc("sync_failed", e))?
                {
                    decision["base_ref"] = json!(base_ref);
                }
                Ok(decision)
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    async fn retarget_base(
        &self,
        workspace_id: &str,
        base_branch: &str,
    ) -> Result<Option<String>, String> {
        let Some(mut workspace) = self
            .store
            .workspace(workspace_id)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        if base_branch.is_empty()
            || icarus_rules::facts::base_follows(workspace.base_ref.as_deref(), base_branch)
        {
            return Ok(None);
        }
        let (path, branch) = (workspace.path.clone(), base_branch.to_owned());
        tokio::task::spawn_blocking(move || fetch_target(&path, &branch))
            .await
            .map_err(|e| e.to_string())?;
        let base_ref = format!("origin/{base_branch}");
        workspace.base_ref = Some(base_ref.clone());
        self.store
            .update_workspace(&workspace)
            .map_err(|e| e.to_string())?;
        self.broadcast(
            "workspace.changed",
            json!({ "workspaces": [workspace.id], "source": "review.retarget" }),
        );
        Ok(Some(base_ref))
    }

    async fn start_review_agent(
        &self,
        workspace: &Workspace,
        agent: &ReviewAgent,
    ) -> Result<String, String> {
        let settings = self.resolved_settings(Some(&workspace.path));
        let choice = choose_agent(&settings, None, agent.provider.as_deref());
        let title = agent.title.as_deref().unwrap_or("Review");
        self.start_agent_session(workspace, choice, title, &agent.prompt)
            .await
            .map_err(|error| format!("{error:#}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_review_workspace(
        self: &Arc<Self>,
        project: &Project,
        pr: &PullRequest,
        since_review_commit: Option<&str>,
        name: &str,
        title: &str,
        initial_column: Option<&str>,
        agent: Option<ReviewAgent>,
    ) -> Result<Value, String> {
        let created = self
            .workspace_dispatch(
                "workspace.create",
                json!({
                    "project": project.id,
                    "name": name,
                    "branch": pr.head_ref_name,
                    "base": format!("origin/{}", pr.head_ref_name),
                    "fetch": true,
                    "status": initial_column,
                    "title": title,
                    "agent": false,
                    // Nobody is there to answer an "ask" project, so its setup runs, as Orca's review creation does.
                    "setup": "auto",
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
            Some(agent) if self.agent_waits_for_setup(&workspace.id) => {
                // `wait-for-setup`: the review agent starts once setup ends; the action doesn't wait for it.
                let server = Arc::clone(self);
                let workspace = workspace.clone();
                tokio::spawn(async move {
                    server.wait_for_setup(&workspace.id).await;
                    if let Err(error) = server.start_review_agent(&workspace, &agent).await {
                        tracing::warn!("review agent for {} after setup: {error}", workspace.name);
                    }
                });
                json!({ "deferred": "until setup finishes" })
            }
            Some(agent) => json!(self.start_review_agent(&workspace, &agent).await?),
            None => Value::Null,
        };
        Ok(json!({ "workspace": id, "agent": agent }))
    }

    async fn advance_stack_intents(&self) {
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
            match self
                .run_pr_operation(
                    &repo,
                    &details,
                    &pull,
                    metadata.as_ref(),
                    icarus_rules::model::PrOperation::MergeStack,
                )
                .await
            {
                Ok(_) => {
                    let _ = self.set_stack_intent(key, None);
                }
                Err(error) => tracing::warn!("stack auto-merge {key}: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        icarus_git::git(dir, args).expect("git").trim().to_owned()
    }

    fn commit(dir: &Path, file: &str, text: &str) -> String {
        std::fs::write(dir.join(file), text).expect("write");
        git(dir, &["add", file]);
        git(
            dir,
            &[
                "-c",
                "user.name=Author",
                "-c",
                "user.email=author@example.test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                file,
            ],
        );
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn a_review_with_stale_uncommitted_changes_moves_to_the_new_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let author = tmp.path().join("author");
        let review = tmp.path().join("review");
        git(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(tmp.path(), &["clone", "-q", "origin.git", "author"]);
        git(&author, &["checkout", "-q", "-b", "feature"]);
        commit(&author, "a.txt", "one\n");
        git(&author, &["push", "-q", "origin", "feature"]);
        git(
            tmp.path(),
            &["clone", "-q", "-b", "feature", "origin.git", "review"],
        );
        git(&review, &["config", "user.email", "me@example.test"]);
        let pushed = commit(&author, "a.txt", "two\n");
        git(&author, &["push", "-q", "origin", "feature"]);
        std::fs::write(review.join("a.txt"), "stale edit\n").expect("write");

        let decision = sync_to_head(&review, "feature", &pushed).expect("sync");

        assert_eq!(decision["kind"], "reset-hard");
        assert_eq!(git(&review, &["rev-parse", "HEAD"]), pushed);
        assert_eq!(git(&review, &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(review.join("a.txt")).expect("read"),
            "two\n"
        );
    }

    #[test]
    fn fetching_a_new_target_makes_its_remote_branch_usable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let author = tmp.path().join("author");
        let review = tmp.path().join("review");
        git(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        git(tmp.path(), &["clone", "-q", "origin.git", "author"]);
        commit(&author, "a.txt", "one\n");
        git(&author, &["push", "-q", "origin", "HEAD:main"]);
        git(tmp.path(), &["clone", "-q", "origin.git", "review"]);
        git(&author, &["checkout", "-q", "-b", "feature/parent"]);
        let parent = commit(&author, "b.txt", "parent\n");
        git(&author, &["push", "-q", "origin", "feature/parent"]);

        fetch_target(&review, "feature/parent");

        assert_eq!(
            git(&review, &["rev-parse", "origin/feature/parent"]),
            parent
        );
    }
}
