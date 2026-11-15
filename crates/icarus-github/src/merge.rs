use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::raw::RawQueueEntry;
use crate::runner::{Gh, GhRequest, Method};
use crate::stack::{is_git_oid, method_name};
use icarus_rules::facts::{MergeMethod, MergeQueueEntry, RepoRef};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    pub merged: bool,
    pub sha: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunScope {
    Failed,
    All,
}

fn graphql_method(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "MERGE",
        MergeMethod::Squash => "SQUASH",
        MergeMethod::Rebase => "REBASE",
    }
}

fn check_oid(oid: &str) -> Result<()> {
    if is_git_oid(oid) {
        Ok(())
    } else {
        Err(GitHubError::Invalid(format!("Not a commit id: {oid}")))
    }
}

const ENABLE_AUTO_MERGE: &str = "mutation EnableAutoMerge($pullRequestId: ID!, $mergeMethod: PullRequestMergeMethod!, $expectedHeadOid: GitObjectID) {
  enablePullRequestAutoMerge(input: {pullRequestId: $pullRequestId, mergeMethod: $mergeMethod, expectedHeadOid: $expectedHeadOid}) {
    pullRequest { autoMergeRequest { mergeMethod } } }
}";

const DISABLE_AUTO_MERGE: &str = "mutation DisableAutoMerge($pullRequestId: ID!) {
  disablePullRequestAutoMerge(input: {pullRequestId: $pullRequestId}) { pullRequest { autoMergeRequest { mergeMethod } } }
}";

const ENQUEUE: &str = "mutation Enqueue($pullRequestId: ID!, $expectedHeadOid: GitObjectID, $jump: Boolean) {
  enqueuePullRequest(input: {pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid, jump: $jump}) {
    mergeQueueEntry { state position estimatedTimeToMerge } }
}";

const DEQUEUE: &str = "mutation Dequeue($id: ID!) {
  dequeuePullRequest(input: {id: $id}) { mergeQueueEntry { state } }
}";

const READY: &str = "mutation MarkReady($pullRequestId: ID!) {
  markPullRequestReadyForReview(input: {pullRequestId: $pullRequestId}) { pullRequest { isDraft } }
}";

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct WorkflowRuns {
    #[serde(default)]
    workflow_runs: Vec<WorkflowRun>,
}

impl<G: Gh> GitHub<G> {
    pub async fn merge_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        method: MergeMethod,
        expected_head: &str,
    ) -> Result<MergeResult> {
        check_oid(expected_head)?;
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            merged: bool,
            #[serde(default)]
            sha: Option<String>,
            #[serde(default)]
            message: String,
        }
        let raw: Raw = self
            .rest_json(
                GhRequest::new(
                    Method::Put,
                    format!("repos/{}/{}/pulls/{number}/merge", repo.owner, repo.name),
                )
                .json(json!({ "merge_method": method_name(method), "sha": expected_head })),
            )
            .await?;
        Ok(MergeResult {
            merged: raw.merged,
            sha: raw.sha,
            message: raw.message,
        })
    }

    pub async fn enable_auto_merge(
        &self,
        pull_request_id: &str,
        method: MergeMethod,
        expected_head: Option<&str>,
    ) -> Result<()> {
        if let Some(oid) = expected_head {
            check_oid(oid)?;
        }
        let result = self
            .graphql_data::<Value>(
                ENABLE_AUTO_MERGE,
                json!({
                    "pullRequestId": pull_request_id,
                    "mergeMethod": graphql_method(method),
                    "expectedHeadOid": expected_head,
                }),
            )
            .await;
        match result {
            Err(GitHubError::GraphQl(messages))
                if messages
                    .iter()
                    .any(|message| message.contains("clean status")) =>
            {
                Err(GitHubError::AlreadyMergeable)
            }
            other => other.map(drop),
        }
    }

    pub async fn disable_auto_merge(&self, pull_request_id: &str) -> Result<()> {
        self.graphql_data::<Value>(
            DISABLE_AUTO_MERGE,
            json!({ "pullRequestId": pull_request_id }),
        )
        .await
        .map(drop)
    }

    pub async fn enqueue(
        &self,
        pull_request_id: &str,
        expected_head: Option<&str>,
        jump: bool,
    ) -> Result<Option<MergeQueueEntry>> {
        if let Some(oid) = expected_head {
            check_oid(oid)?;
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            merge_queue_entry: Option<RawQueueEntry>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            enqueue_pull_request: Option<Payload>,
        }
        let data: Data = self
            .graphql_data(
                ENQUEUE,
                json!({
                    "pullRequestId": pull_request_id,
                    "expectedHeadOid": expected_head,
                    "jump": jump,
                }),
            )
            .await?;
        Ok(data
            .enqueue_pull_request
            .and_then(|payload| payload.merge_queue_entry)
            .map(|entry| MergeQueueEntry {
                state: entry.state,
                position: entry.position,
                eta_seconds: entry.estimated_time_to_merge,
            }))
    }

    pub async fn dequeue(&self, pull_request_id: &str) -> Result<()> {
        self.graphql_data::<Value>(DEQUEUE, json!({ "id": pull_request_id }))
            .await
            .map(drop)
    }

    pub async fn mark_ready_for_review(&self, pull_request_id: &str) -> Result<()> {
        self.graphql_data::<Value>(READY, json!({ "pullRequestId": pull_request_id }))
            .await
            .map(drop)
    }

    pub async fn rerun_checks(
        &self,
        repo: &RepoRef,
        head_sha: &str,
        scope: RerunScope,
    ) -> Result<Vec<u64>> {
        check_oid(head_sha)?;
        let runs: WorkflowRuns = self
            .rest_json(GhRequest::get(format!(
                "repos/{}/{}/actions/runs?head_sha={head_sha}&per_page=100",
                repo.owner, repo.name
            )))
            .await?;
        let mut rerun = Vec::new();
        for run in runs.workflow_runs {
            if run.status.as_deref() != Some("completed") {
                continue;
            }
            let failed = matches!(
                run.conclusion.as_deref(),
                Some("failure" | "timed_out" | "cancelled" | "startup_failure")
            );
            let action = match scope {
                RerunScope::Failed if failed => "rerun-failed-jobs",
                RerunScope::Failed => continue,
                RerunScope::All => "rerun",
            };
            self.rest(GhRequest::new(
                Method::Post,
                format!(
                    "repos/{}/{}/actions/runs/{}/{action}",
                    repo.owner, repo.name, run.id
                ),
            ))
            .await?;
            rerun.push(run.id);
        }
        Ok(rerun)
    }

    pub async fn rerequest_check_run(&self, repo: &RepoRef, check_run_id: u64) -> Result<()> {
        self.rest(GhRequest::new(
            Method::Post,
            format!(
                "repos/{}/{}/check-runs/{check_run_id}/rerequest",
                repo.owner, repo.name
            ),
        ))
        .await
        .map(drop)
    }
}
