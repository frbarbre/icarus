use crate::client::{GitHub, STACKS_API_VERSION, http_error};
use crate::error::{GitHubError, Result};
use crate::http::{HttpResponse, encode_component};
use crate::raw::{Actor, Nodes, rollup_state};
use crate::runner::{Gh, GhRequest, Method};
use icarus_rules::facts::{
    MergeMethod, MergeStateStatus, Mergeable, PrLifecycle, PrState, RepoRef, ReviewDecision, Stack,
    StackAutoMerge, StackEntry,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackSummary {
    pub number: u64,
    pub position: u32,
    pub size: u32,
    pub base_ref: String,
    pub base_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullStackProbe {
    pub head_sha: Option<String>,
    pub stack: Option<StackSummary>,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackView {
    pub summary: StackSummary,
    pub base_tip: Option<String>,
    pub entries: Option<Vec<StackEntry>>,
}

impl StackView {
    pub fn out_of_date(&self) -> bool {
        match (&self.summary.base_sha, &self.base_tip) {
            (Some(base), Some(tip)) => !base.eq_ignore_ascii_case(tip),
            _ => false,
        }
    }

    pub fn to_fact(
        &self,
        merge_queue_required: Option<bool>,
        auto_merge: Option<StackAutoMerge>,
    ) -> Stack {
        Stack {
            number: self.summary.number,
            position: self.summary.position,
            size: self.summary.size,
            base_ref_name: self.summary.base_ref.clone(),
            entries: self.entries.clone().unwrap_or_default(),
            merge_queue_required,
            auto_merge,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackRecord {
    pub number: u64,
    pub open: bool,
    pub pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StacksForPr {
    Unavailable,
    Stacks(Vec<StackRecord>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMergeAction {
    Direct(MergeMethod),
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackMergeStatus {
    Pending { uuid: String },
    Merged,
    Enqueued,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollPolicy {
    pub interval: Duration,
    pub max_polls: u32,
}

impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            max_polls: 180,
        }
    }
}

pub fn is_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn method_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}

fn stacks_request(method: Method, endpoint: String) -> GhRequest {
    GhRequest::new(method, endpoint)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", STACKS_API_VERSION)
}

fn parse_summary(value: &Value) -> Result<Option<StackSummary>> {
    let stack = match value.get("stack") {
        None | Some(Value::Null) => return Ok(None),
        Some(stack) => stack,
    };
    let positive = |key: &str| {
        stack
            .get(key)
            .and_then(Value::as_u64)
            .filter(|number| *number > 0)
    };
    let base = stack.get("base");
    let base_ref = base
        .and_then(|base| base.get("ref"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|base_ref| !base_ref.is_empty());
    let base_sha = match base.and_then(|base| base.get("sha")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(sha)) if is_git_oid(sha) => Ok(Some(sha.clone())),
        Some(_) => Err(GitHubError::StackMetadataUnavailable),
    }?;
    let (Some(number), Some(position), Some(size), Some(base_ref)) = (
        positive("number"),
        positive("position"),
        positive("size"),
        base_ref,
    ) else {
        return Err(GitHubError::StackMetadataUnavailable);
    };
    let (Ok(position), Ok(size)) = (u32::try_from(position), u32::try_from(size)) else {
        return Err(GitHubError::StackMetadataUnavailable);
    };
    if position > size {
        return Err(GitHubError::StackMetadataUnavailable);
    }
    Ok(Some(StackSummary {
        number,
        position,
        size,
        base_ref: base_ref.to_owned(),
        base_sha,
    }))
}

const DETAILS_QUERY: &str = "query StackDetails($owner: String!, $name: String!, $pr: Int!, $baseRef: String!) {
  repository(owner: $owner, name: $name) {
    baseTip: ref(qualifiedName: $baseRef) { target { oid } }
    pullRequest(number: $pr) { stack { number size baseRefName
      entries(first: 100) { nodes { position pullRequest {
        number title url headRefName state isDraft headRefOid mergeable reviewDecision mergeStateStatus isInMergeQueue
        author { login avatarUrl(size: 64) }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } } } } } }
  }
}";

#[derive(Deserialize)]
struct RawEntryRollup {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntryCommit {
    #[serde(default)]
    status_check_rollup: Option<RawEntryRollup>,
}

#[derive(Deserialize)]
struct RawEntryCommitNode {
    commit: RawEntryCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntryPr {
    number: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    head_ref_name: Option<String>,
    #[serde(default)]
    commits: Option<Nodes<RawEntryCommitNode>>,
    state: PrState,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    head_ref_oid: Option<String>,
    #[serde(default)]
    mergeable: Option<Mergeable>,
    #[serde(default)]
    review_decision: Option<ReviewDecision>,
    #[serde(default)]
    merge_state_status: Option<MergeStateStatus>,
    #[serde(default)]
    is_in_merge_queue: bool,
    #[serde(default)]
    author: Option<Actor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    position: u32,
    pull_request: Option<RawEntryPr>,
}

#[derive(Deserialize)]
struct RawStack {
    number: u64,
    #[serde(default)]
    entries: Option<Nodes<RawEntry>>,
}

#[derive(Deserialize)]
struct RawTarget {
    oid: String,
}

#[derive(Deserialize)]
struct RawRef {
    target: Option<RawTarget>,
}

#[derive(Deserialize)]
struct RawStackPr {
    stack: Option<RawStack>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    base_tip: Option<RawRef>,
    pull_request: Option<RawStackPr>,
}

#[derive(Deserialize)]
struct DetailsData {
    repository: Option<RawRepository>,
}

fn lifecycle(state: PrState, is_draft: bool) -> PrLifecycle {
    match state {
        PrState::Merged => PrLifecycle::Merged,
        PrState::Closed => PrLifecycle::Closed,
        PrState::Open if is_draft => PrLifecycle::Draft,
        PrState::Open => PrLifecycle::Open,
    }
}

#[derive(Deserialize)]
struct RawRecord {
    number: u64,
    #[serde(default)]
    open: bool,
    #[serde(default)]
    pull_requests: Vec<RawRecordPr>,
}

#[derive(Deserialize)]
struct RawRecordPr {
    number: u64,
}

impl From<RawRecord> for StackRecord {
    fn from(raw: RawRecord) -> Self {
        Self {
            number: raw.number,
            open: raw.open,
            pull_requests: raw.pull_requests.into_iter().map(|pr| pr.number).collect(),
        }
    }
}

#[derive(Deserialize)]
struct AsyncDetails {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct AsyncResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    details: Option<AsyncDetails>,
}

fn classify_async(response: &HttpResponse) -> Result<StackMergeStatus> {
    if response.status == 404 {
        return Err(GitHubError::StacksUnavailable);
    }
    let Ok(body) = serde_json::from_slice::<AsyncResponse>(&response.body) else {
        return Err(if response.is_success() {
            GitHubError::Malformed("invalid stack merge response".to_owned())
        } else {
            http_error(response)
        });
    };
    let details = body.details.unwrap_or(AsyncDetails {
        uuid: None,
        message: None,
    });
    Ok(match (body.status.as_deref(), details.uuid) {
        (Some("merged"), _) => StackMergeStatus::Merged,
        (Some("enqueued"), _) => StackMergeStatus::Enqueued,
        (Some("pending"), Some(uuid)) if !uuid.is_empty() => StackMergeStatus::Pending { uuid },
        _ => StackMergeStatus::Failed(
            details
                .message
                .unwrap_or_else(|| "GitHub could not merge this stack.".to_owned()),
        ),
    })
}

impl<G: Gh> GitHub<G> {
    pub async fn probe_pull_stack(&self, repo: &RepoRef, number: u64) -> Result<PullStackProbe> {
        let conditional = self
            .get_conditional(stacks_request(
                Method::Get,
                format!("repos/{}/{}/pulls/{number}", repo.owner, repo.name),
            ))
            .await?;
        let head_sha = conditional
            .body
            .get("head")
            .and_then(|head| head.get("sha"))
            .and_then(Value::as_str)
            .filter(|sha| is_git_oid(sha))
            .map(str::to_owned);
        Ok(PullStackProbe {
            head_sha,
            stack: parse_summary(&conditional.body)?,
            changed: conditional.changed,
        })
    }

    pub async fn stack_details(
        &self,
        repo: &RepoRef,
        number: u64,
        summary: &StackSummary,
    ) -> Result<(Option<String>, Vec<StackEntry>)> {
        let data: DetailsData = self
            .graphql_data(
                DETAILS_QUERY,
                json!({
                    "owner": repo.owner,
                    "name": repo.name,
                    "pr": number,
                    "baseRef": format!("refs/heads/{}", summary.base_ref),
                }),
            )
            .await?;
        let repository = data.repository.ok_or_else(|| {
            GitHubError::NotFound(format!("Could not find repository {}.", repo.slug()))
        })?;
        let base_tip = repository
            .base_tip
            .and_then(|reference| reference.target)
            .map(|target| target.oid);
        let stack = repository
            .pull_request
            .and_then(|pr| pr.stack)
            .filter(|stack| stack.number == summary.number)
            .ok_or_else(|| {
                GitHubError::Malformed("stack details do not match the pull request".to_owned())
            })?;
        let mut entries: Vec<StackEntry> = stack
            .entries
            .into_iter()
            .flat_map(Nodes::items)
            .filter_map(|entry| {
                let pr = entry.pull_request?;
                let checks_rollup = pr
                    .commits
                    .and_then(|commits| commits.items().last())
                    .and_then(|node| node.commit.status_check_rollup)
                    .and_then(|rollup| rollup.state)
                    .as_deref()
                    .and_then(rollup_state);
                let (author, author_avatar_url) = pr.author.map_or((None, None), |author| {
                    (Some(author.login), author.avatar_url)
                });
                Some(StackEntry {
                    position: entry.position,
                    number: pr.number,
                    state: lifecycle(pr.state, pr.is_draft),
                    mergeable: pr.mergeable,
                    merge_state_status: pr.merge_state_status,
                    review_decision: pr.review_decision,
                    in_merge_queue: pr.is_in_merge_queue,
                    head_oid: pr.head_ref_oid,
                    title: pr.title,
                    head_ref_name: pr.head_ref_name,
                    url: pr.url,
                    is_draft: pr.is_draft,
                    checks_rollup,
                    author,
                    author_avatar_url,
                })
            })
            .collect();
        entries.sort_by_key(|entry| entry.position);
        Ok((base_tip, entries))
    }

    pub async fn stack_view(&self, repo: &RepoRef, number: u64) -> Result<Option<StackView>> {
        let Some(summary) = self.probe_pull_stack(repo, number).await?.stack else {
            return Ok(None);
        };
        let (base_tip, entries) = match self.stack_details(repo, number, &summary).await {
            Ok((base_tip, entries)) => (base_tip, Some(entries)),
            Err(_) => (None, None),
        };
        Ok(Some(StackView {
            summary,
            base_tip,
            entries,
        }))
    }

    pub async fn stacks_for_pr(&self, repo: &RepoRef, number: u64) -> Result<StacksForPr> {
        let response = self
            .send(stacks_request(
                Method::Get,
                format!(
                    "repos/{}/{}/stacks?pull_request={number}",
                    repo.owner, repo.name
                ),
            ))
            .await?;
        if response.status == 404 {
            return Ok(StacksForPr::Unavailable);
        }
        if !response.is_success() {
            return Err(http_error(&response));
        }
        let records: Vec<RawRecord> = response.json()?;
        Ok(StacksForPr::Stacks(
            records.into_iter().map(StackRecord::from).collect(),
        ))
    }

    pub async fn get_stack(&self, repo: &RepoRef, stack: u64) -> Result<StackRecord> {
        let response = self
            .send(stacks_request(
                Method::Get,
                format!("repos/{}/{}/stacks/{stack}", repo.owner, repo.name),
            ))
            .await?;
        if response.status == 404 {
            return Err(GitHubError::NotFound(format!(
                "Could not find stack #{stack}."
            )));
        }
        if !response.is_success() {
            return Err(http_error(&response));
        }
        Ok(response.json::<RawRecord>()?.into())
    }

    pub async fn create_stack(&self, repo: &RepoRef, pulls: &[u64]) -> Result<StackRecord> {
        self.stack_write(format!("repos/{}/{}/stacks", repo.owner, repo.name), pulls)
            .await
    }

    pub async fn add_to_stack(
        &self,
        repo: &RepoRef,
        stack: u64,
        pulls: &[u64],
    ) -> Result<StackRecord> {
        self.stack_write(
            format!("repos/{}/{}/stacks/{stack}/add", repo.owner, repo.name),
            pulls,
        )
        .await
    }

    async fn stack_write(&self, endpoint: String, pulls: &[u64]) -> Result<StackRecord> {
        let response = self
            .send(stacks_request(Method::Post, endpoint).json(json!({ "pull_requests": pulls })))
            .await?;
        if response.status == 404 {
            return Err(GitHubError::StacksUnavailable);
        }
        if !response.is_success() {
            return Err(http_error(&response));
        }
        let record: StackRecord = response.json::<RawRecord>()?.into();
        if record.number == 0 {
            return Err(GitHubError::Malformed(
                "stack number is not positive".to_owned(),
            ));
        }
        Ok(record)
    }

    pub async fn submit_stack_merge(
        &self,
        repo: &RepoRef,
        top: u64,
        action: StackMergeAction,
        head_sha: &str,
    ) -> Result<StackMergeStatus> {
        if !is_git_oid(head_sha) {
            return Err(GitHubError::StackMetadataUnavailable);
        }
        let body = match action {
            StackMergeAction::Direct(method) => json!({
                "merge_action": "direct_merge",
                "merge_method": method_name(method),
                "sha": head_sha,
            }),
            StackMergeAction::Queue => json!({ "merge_action": "merge_queue", "sha": head_sha }),
        };
        let response = self
            .send(
                stacks_request(
                    Method::Put,
                    format!("repos/{}/{}/pulls/{top}/merge-async", repo.owner, repo.name),
                )
                .json(body),
            )
            .await?;
        classify_async(&response)
    }

    pub async fn stack_merge_status(
        &self,
        repo: &RepoRef,
        top: u64,
        uuid: &str,
    ) -> Result<StackMergeStatus> {
        let response = self
            .send(stacks_request(
                Method::Get,
                format!(
                    "repos/{}/{}/pulls/{top}/merge-async/{}",
                    repo.owner,
                    repo.name,
                    encode_component(uuid)
                ),
            ))
            .await?;
        classify_async(&response)
    }

    pub async fn merge_stack(
        &self,
        repo: &RepoRef,
        top: u64,
        action: StackMergeAction,
        head_sha: &str,
        policy: PollPolicy,
    ) -> Result<StackMergeStatus> {
        let uuid = match self.submit_stack_merge(repo, top, action, head_sha).await? {
            StackMergeStatus::Pending { uuid } => uuid,
            StackMergeStatus::Failed(message) => return Err(GitHubError::StackMerge(message)),
            done => return Ok(done),
        };
        for _ in 0..policy.max_polls {
            tokio::time::sleep(policy.interval).await;
            // Why errors are skipped: the merge was submitted; a transient poll failure cannot make it fail.
            match self.stack_merge_status(repo, top, &uuid).await {
                Ok(StackMergeStatus::Pending { .. }) | Err(_) => continue,
                Ok(StackMergeStatus::Failed(message)) => {
                    return Err(GitHubError::StackMerge(message));
                }
                Ok(done) => return Ok(done),
            }
        }
        Ok(StackMergeStatus::Pending { uuid })
    }
}
