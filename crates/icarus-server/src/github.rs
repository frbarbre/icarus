//! GitHub through the user's `gh` login: pull request status per workspace and pull request operations.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use icarus_github::merge::MergeResult;
use icarus_github::previews::{PrPreviews, PreviewConfig};
use icarus_github::repo::RepoMergeMetadata;
use icarus_github::review::GitHubUser;
use icarus_github::snapshot::{PrDetails, PrTarget};
use icarus_github::stack::{PollPolicy, StackMergeAction, StackMergeStatus, StackView};
use icarus_github::{FakeGh, FakeResponse, Gh, GhCli, GhOutput, GhRequest, GitHub, GitHubError};
use icarus_protocol::RpcError;
use icarus_rules::facts::{
    MergeMethod, PullRequest, RepoRef, StackAutoMerge, StackAutoMergeMode, StackEntry,
};
use icarus_rules::model::PrOperation;
use icarus_rules::next_action::stack_merge_target;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, params, rpc};
use crate::state::{Project, Workspace};

const PR_TTL: Duration = Duration::from_secs(15);
const METADATA_TTL: Duration = Duration::from_secs(600);
const PREVIEW_TTL: Duration = Duration::from_secs(30);
const USERS_TTL: Duration = Duration::from_secs(600);
pub(crate) const STACK_INTENTS_KEY: &str = "rules.stack_auto_merge";

/// The `gh` runner the server uses: the real CLI, or fixtures when `ICARUS_GH_FIXTURES` names a file.
pub enum ServerGh {
    Cli(GhCli),
    Fixture(FixtureGh),
}

impl Gh for ServerGh {
    async fn execute(&self, request: &GhRequest) -> std::io::Result<GhOutput> {
        match self {
            Self::Cli(cli) => cli.execute(request).await,
            Self::Fixture(fixture) => fixture.execute(request).await,
        }
    }
}

impl ServerGh {
    /// The failed steps' log of a GitHub Actions run, via `gh run view --log-failed`.
    pub async fn run_failed_log(&self, repo: &RepoRef, run_id: u64) -> Result<String, String> {
        match self {
            Self::Cli(cli) => {
                let mut command = tokio::process::Command::new(&cli.program);
                command
                    .args(["run", "view", &run_id.to_string(), "--log-failed", "-R"])
                    .arg(repo.slug())
                    .env("GH_PROMPT_DISABLED", "1")
                    .env("GH_NO_UPDATE_NOTIFIER", "1")
                    .env("NO_COLOR", "1")
                    .env("GH_PAGER", "")
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true);
                let output = tokio::time::timeout(Duration::from_secs(120), command.output())
                    .await
                    .map_err(|_| "gh run view timed out".to_owned())?
                    .map_err(|e| format!("could not run gh: {e}"))?;
                if !output.status.success() {
                    return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
                }
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Self::Fixture(fixture) => fixture.run_failed_log(run_id),
        }
    }
}

/// Answers `gh api` calls from a JSON fixture file, re-read on every call, and logs each request.
pub struct FixtureGh {
    path: PathBuf,
}

impl FixtureGh {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn log_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.requests.jsonl", self.path.display()))
    }

    fn log(&self, line: &Value) {
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
        {
            let _ = writeln!(log, "{line}");
        }
    }

    fn run_failed_log(&self, run_id: u64) -> Result<String, String> {
        self.log(&json!({ "route": format!("run view {run_id} --log-failed") }));
        let text = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let fixtures: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        fixtures["run_logs"][run_id.to_string()]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("no fixture log for run {run_id}"))
    }

    async fn execute(&self, request: &GhRequest) -> std::io::Result<GhOutput> {
        let text = std::fs::read_to_string(&self.path)?;
        let fixtures: Value = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let route = request.route();
        let line = json!({
            "route": route,
            "variables": request.graphql_variables(),
            "body": request.json_body(),
        });
        self.log(&line);
        let (status, body) = fixture_response(&fixtures, request);
        FakeGh::new()
            .on(route, FakeResponse::json(status, body.to_string()))
            .execute(request)
            .await
    }
}

fn fixture_pulls(fixtures: &Value) -> impl Iterator<Item = &Value> {
    ["pulls", "inbox"]
        .into_iter()
        .flat_map(|key| fixtures[key].as_array().into_iter().flatten())
}

fn pull_in_repo(pull: &Value, owner: &str, name: &str) -> bool {
    let repo = &pull["repository"];
    repo.is_null()
        || (repo["owner"]["login"]
            .as_str()
            .is_some_and(|o| o.eq_ignore_ascii_case(owner))
            && repo["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

fn fixture_response(fixtures: &Value, request: &GhRequest) -> (u16, Value) {
    let route = request.route();
    if let Some(over) = fixtures["routes"].get(&route) {
        let status = over["status"].as_u64().unwrap_or(200) as u16;
        return (status, over.get("body").cloned().unwrap_or(Value::Null));
    }
    let viewer = json!({ "login": fixtures["viewer"].as_str().unwrap_or("viewer") });
    let variables = request.graphql_variables().cloned().unwrap_or(Value::Null);
    let data = |data: Value| (200, json!({ "data": data }));
    match route.as_str() {
        "graphql Viewer" => data(json!({ "viewer": viewer })),
        "graphql PrSnapshot" => {
            let owner = variables["owner"].as_str().unwrap_or_default();
            let name = variables["name"].as_str().unwrap_or_default();
            let mut repository = serde_json::Map::new();
            for (key, value) in variables.as_object().into_iter().flatten() {
                let (kind, index) = key.split_at(1);
                if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let mut pulls = fixture_pulls(fixtures).filter(|p| pull_in_repo(p, owner, name));
                let answer = match kind {
                    "n" => pulls
                        .find(|p| p["number"] == *value)
                        .cloned()
                        .unwrap_or(Value::Null),
                    _ => {
                        json!({ "nodes": pulls.filter(|p| p["headRefName"] == *value).take(1).cloned().collect::<Vec<_>>() })
                    }
                };
                repository.insert(format!("t{index}"), answer);
            }
            data(json!({ "rateLimit": null, "viewer": viewer, "repository": repository }))
        }
        "graphql ReviewInbox" => {
            let nodes: Vec<Value> = fixtures["inbox"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|pull| {
                    let mut pull = pull.clone();
                    pull["__typename"] = json!("PullRequest");
                    pull
                })
                .collect();
            data(json!({ "viewer": viewer, "search": { "nodes": nodes } }))
        }
        "graphql TeamMembers" => {
            let team = format!(
                "{}/{}",
                variables["org"].as_str().unwrap_or_default(),
                variables["slug"].as_str().unwrap_or_default()
            );
            let nodes: Vec<Value> = fixtures["teams"][&team]
                .as_array()
                .into_iter()
                .flatten()
                .map(|login| json!({ "login": login }))
                .collect();
            data(json!({ "organization": { "team": { "members": {
                "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } } }))
        }
        "graphql RepoMergeMetadata" => {
            let repo = match &fixtures["repo"] {
                Value::Null => json!({ "squashMergeAllowed": true, "mergeCommitAllowed": true,
                    "rebaseMergeAllowed": false, "autoMergeAllowed": true, "mergeQueue": null }),
                repo => repo.clone(),
            };
            data(json!({ "repository": repo }))
        }
        "graphql ReviewThreads" => {
            let number = variables["number"].to_string();
            let nodes = match &fixtures["review_threads"][&number] {
                Value::Array(nodes) => nodes.clone(),
                _ => Vec::new(),
            };
            data(json!({ "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } } }))
        }
        "graphql PrReviewThreads" => {
            let number = variables["number"].to_string();
            let nodes = match &fixtures["review_threads"][&number] {
                Value::Array(nodes) => nodes.clone(),
                _ => Vec::new(),
            };
            data(json!({ "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } } }))
        }
        "graphql ReviewContext" => {
            let number = &variables["number"];
            let Some(pull) = fixture_pulls(fixtures).find(|p| p["number"] == *number) else {
                return data(json!({ "viewer": viewer, "repository": { "pullRequest": null } }));
            };
            let mut context = json!({
                "id": pull["id"], "number": pull["number"], "title": pull["title"], "url": pull["url"],
                "state": pull["state"], "isDraft": pull["isDraft"], "headRefName": pull["headRefName"],
                "headRefOid": pull["headRefOid"], "baseRefName": pull["baseRefName"], "author": pull["author"],
                "viewerDidAuthor": pull["author"]["login"] == viewer["login"],
                "commits": { "nodes": [] }, "reviews": { "nodes": [] },
            });
            for (key, value) in fixtures["review_context"][number.to_string()]
                .as_object()
                .into_iter()
                .flatten()
            {
                context[key] = value.clone();
            }
            data(json!({ "viewer": viewer, "repository": { "pullRequest": context } }))
        }
        "graphql SubmitReview" => {
            let state = match variables["input"]["event"].as_str() {
                Some("APPROVE") => "APPROVED",
                Some("REQUEST_CHANGES") => "CHANGES_REQUESTED",
                _ => "COMMENTED",
            };
            data(json!({ "addPullRequestReview": { "pullRequestReview": {
                "id": "PRR_fixture", "state": state, "url": "https://github.com/review/fixture" } } }))
        }
        "graphql ReplyInThread" => data(json!({ "addPullRequestReviewThreadReply": { "comment": {
            "id": "PRRC_reply", "databaseId": 77, "url": "https://github.com/reply/fixture" } } })),
        "graphql ResolveThread" => {
            data(json!({ "resolveReviewThread": { "thread": { "isResolved": true } } }))
        }
        "graphql UnresolveThread" => {
            data(json!({ "unresolveReviewThread": { "thread": { "isResolved": false } } }))
        }
        "graphql AssignableUsers" => {
            let nodes = match &fixtures["assignable_users"] {
                Value::Array(users) => users.clone(),
                _ => Vec::new(),
            };
            data(json!({ "repository": { "assignableUsers": {
                "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } }))
        }
        "graphql UpdateReviewComment" => data(json!({ "updatePullRequestReviewComment": {
            "pullRequestReviewComment": { "body": variables["body"], "lastEditedAt": "2026-01-01T00:00:00Z" } } })),
        "graphql UpdateReview" => data(json!({ "updatePullRequestReview": {
            "pullRequestReview": { "body": variables["body"], "lastEditedAt": "2026-01-01T00:00:00Z" } } })),
        "graphql DeleteReviewComment" => {
            data(json!({ "deletePullRequestReviewComment": { "clientMutationId": null } }))
        }
        "graphql AddReaction" | "graphql RemoveReaction" => {
            data(json!({ "reaction": { "subject": { "id": variables["subjectId"] } } }))
        }
        "graphql EnableAutoMerge"
        | "graphql DisableAutoMerge"
        | "graphql MarkReady"
        | "graphql Dequeue" => data(json!({})),
        "graphql Enqueue" => data(json!({ "enqueuePullRequest": {
            "mergeQueueEntry": { "state": "QUEUED", "position": 1 } } })),
        "graphql PrPreviews" => {
            let number = variables["number"].to_string();
            let head = fixture_pulls(fixtures)
                .find(|p| p["number"] == variables["number"])
                .map(|p| p["headRefOid"].clone())
                .filter(|oid| oid.is_string())
                .unwrap_or_else(|| json!(""));
            let pull = match &fixtures["previews"][&number] {
                Value::Null => {
                    json!({ "headRefOid": head, "headRefName": "", "isCrossRepository": false,
                    "comments": { "nodes": [] }, "commits": { "nodes": [] } })
                }
                pull => pull.clone(),
            };
            data(json!({ "repository": { "pullRequest": pull } }))
        }
        _ => rest_fixture(fixtures, request),
    }
}

fn rest_fixture(fixtures: &Value, request: &GhRequest) -> (u16, Value) {
    if let Some(query) = request
        .endpoint
        .strip_prefix(icarus_github::attachment::UPLOAD_ENDPOINT)
    {
        let name = query
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix("name="))
            .unwrap_or("file");
        return (
            201,
            json!({ "url": format!("https://github.com/user-attachments/assets/fixture-{name}") }),
        );
    }
    let (path, _query) = request
        .endpoint
        .split_once('?')
        .unwrap_or((request.endpoint.as_str(), ""));
    let parts: Vec<&str> = path.split('/').collect();
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["repos", _, _]) => (
            200,
            json!({ "id": 4242, "permissions": { "push": fixtures["repo_push"].as_bool().unwrap_or(true) } }),
        ),
        ("GET", ["repos", _, _, "pulls", number, "files"]) => {
            match fixtures["pr_files"].get(*number) {
                Some(files) => (200, files.clone()),
                None => (404, json!({ "message": "no pr_files fixture" })),
            }
        }
        ("GET", ["repos", _, _, "pulls", number]) => {
            if let Some(body) = fixtures["stacks"].get(*number) {
                return (200, body.clone());
            }
            let head = fixture_pulls(fixtures)
                .find(|p| number.parse::<u64>().ok() == p["number"].as_u64())
                .map(|p| p["headRefOid"].clone())
                .unwrap_or(Value::Null);
            (200, json!({ "head": { "sha": head }, "stack": null }))
        }
        ("PUT", ["repos", _, _, "pulls", _, "merge"]) => {
            let sha = request.json_body().map(|b| b["sha"].clone());
            (
                200,
                json!({ "merged": true, "sha": sha, "message": "Pull Request successfully merged" }),
            )
        }
        ("PUT", ["repos", _, _, "pulls", _, "merge-async"]) => (200, json!({ "status": "merged" })),
        ("POST", ["repos", owner, name, "pulls", number, "comments"]) => (
            201,
            json!({ "id": 9001, "node_id": "PRRC_now",
                "html_url": format!("https://github.com/{owner}/{name}/pull/{number}#discussion_r9001") }),
        ),
        ("PATCH" | "DELETE", ["repos", _, _, "issues", "comments", _]) => (200, json!({})),
        ("POST" | "DELETE", ["repos", _, _, "pulls", number, "requested_reviewers"]) => {
            (200, json!({ "number": number.parse::<u64>().ok() }))
        }
        ("GET", ["repos", _, _, rest @ ..])
            if rest.first().is_some_and(|p| p.starts_with("deployments")) =>
        {
            (200, json!([]))
        }
        _ => (
            404,
            json!({ "message": format!("no fixture for {}", request.route()) }),
        ),
    }
}

#[derive(Clone)]
pub(crate) struct ProjectPrs {
    at: Instant,
    pub viewer: Option<String>,
    pub by_branch: HashMap<String, Option<PrDetails>>,
}

pub struct GitHubService {
    pub(crate) client: GitHub<ServerGh>,
    prs: Mutex<HashMap<String, ProjectPrs>>,
    metadata: Mutex<HashMap<String, (Instant, RepoMergeMetadata)>>,
    stacks: Mutex<HashMap<String, (Instant, Option<StackView>)>>,
    previews: Mutex<HashMap<String, (Instant, PrPreviews)>>,
    users: Mutex<HashMap<String, (Instant, Vec<GitHubUser>)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl GitHubService {
    pub fn new(gh: ServerGh) -> Self {
        Self {
            client: GitHub::new(gh),
            prs: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
            stacks: Mutex::new(HashMap::new()),
            previews: Mutex::new(HashMap::new()),
            users: Mutex::new(HashMap::new()),
        }
    }

    /// The repository's assignable users, cached for ten minutes (mentions and the reviewer picker).
    pub(crate) async fn assignable_users(
        &self,
        repo: &RepoRef,
        refresh: bool,
    ) -> Result<Vec<GitHubUser>, GitHubError> {
        let key = repo.slug();
        if !refresh
            && let Some((at, users)) = lock(&self.users).get(&key)
            && at.elapsed() < USERS_TTL
        {
            return Ok(users.clone());
        }
        let users = self.client.assignable_users(repo).await?;
        lock(&self.users).insert(key, (Instant::now(), users.clone()));
        Ok(users)
    }

    pub fn from_env() -> Self {
        match std::env::var_os("ICARUS_GH_FIXTURES") {
            Some(path) => Self::new(ServerGh::Fixture(FixtureGh::new(PathBuf::from(path)))),
            None => Self::new(ServerGh::Cli(GhCli::new())),
        }
    }

    pub(crate) fn invalidate(&self, project_id: &str) {
        lock(&self.prs).remove(project_id);
        lock(&self.stacks).clear();
    }

    pub(crate) fn cached_pr_author(&self, project_id: &str, branch: &str) -> Value {
        let prs = lock(&self.prs);
        prs.get(project_id)
            .and_then(|cached| cached.by_branch.get(branch)?.as_ref())
            .and_then(|details| {
                let pr = &details.pr;
                Some(json!({ "login": pr.author.as_ref()?, "avatar_url": pr.author_avatar_url }))
            })
            .unwrap_or(Value::Null)
    }

    /// What a workspace card shows about its pull request, from the cache only.
    pub(crate) fn cached_pr_summary(&self, project_id: &str, branch: &str) -> Value {
        let prs = lock(&self.prs);
        prs.get(project_id)
            .and_then(|cached| cached.by_branch.get(branch)?.as_ref())
            .map(|details| {
                let pr = &details.pr;
                json!({
                    "number": pr.number,
                    "title": pr.title,
                    "url": pr.url,
                    "state": pr.lifecycle(),
                    "additions": details.additions,
                    "deletions": details.deletions,
                    "checks": pr.checks_rollup,
                    "in_merge_queue": pr.is_in_merge_queue,
                    "stack": pr.stack.as_ref().map(|stack| json!({
                        "number": stack.number,
                        "position": stack.position,
                        "size": stack.size,
                    })),
                })
            })
            .unwrap_or(Value::Null)
    }
}

/// `github.com/owner/name` → the repository on GitHub; other hosts have no GitHub repository.
pub(crate) fn repo_ref(project: &Project) -> Option<RepoRef> {
    let (owner, name) = project
        .remote
        .as_deref()?
        .strip_prefix("github.com/")?
        .split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| RepoRef::new(owner, name))
}

pub(crate) fn same_repo(a: &RepoRef, b: &RepoRef) -> bool {
    a.owner.eq_ignore_ascii_case(&b.owner) && a.name.eq_ignore_ascii_case(&b.name)
}

fn strip_remote(base: &str) -> &str {
    base.strip_prefix("refs/heads/")
        .or_else(|| base.strip_prefix("origin/"))
        .unwrap_or(base)
}

pub(crate) fn metadata_json(meta: &RepoMergeMetadata) -> Value {
    json!({
        "allowed_methods": meta.allowed_methods,
        "default_method": meta.default_method(),
        "viewer_default_method": meta.viewer_default_method,
        "auto_merge_allowed": meta.auto_merge_allowed,
        "merge_queue_required": meta.merge_queue_required,
    })
}

// Why both spellings: the panel reads `in_merge_queue`/`checks_status`, the pr.status contract names `is_in_merge_queue`/`checks_rollup`.
fn stack_entry_json(entry: &StackEntry) -> Value {
    json!({
        "position": entry.position,
        "number": entry.number,
        "title": entry.title,
        "head_ref_name": entry.head_ref_name,
        "url": entry.url,
        "state": entry.state,
        "is_draft": entry.is_draft,
        "mergeable": entry.mergeable,
        "merge_state_status": entry.merge_state_status,
        "review_decision": entry.review_decision,
        "checks_rollup": entry.checks_rollup,
        "checks_status": entry.checks_rollup,
        "is_in_merge_queue": entry.in_merge_queue,
        "in_merge_queue": entry.in_merge_queue,
        "head_oid": entry.head_oid,
        "author": entry.author,
        "author_avatar_url": entry.author_avatar_url,
    })
}

fn stack_json(view: &StackView, fact: Option<&icarus_rules::facts::Stack>) -> Value {
    json!({
        "number": view.summary.number,
        "position": view.summary.position,
        "size": view.summary.size,
        "base_ref": view.summary.base_ref,
        "base_sha": view.summary.base_sha,
        "base_tip": view.base_tip,
        "out_of_date": view.out_of_date(),
        "entries": view.entries.as_ref().map(|entries| entries.iter().map(stack_entry_json).collect::<Vec<_>>()),
        "auto_merge": fact.and_then(|s| s.auto_merge.clone()),
        "merge_target": fact.and_then(stack_merge_target).map(|t| json!({ "number": t.number, "count": t.count })),
    })
}

/// Everything known about a workspace's pull request.
pub(crate) struct PrView {
    pub repo: Option<RepoRef>,
    pub details: Option<PrDetails>,
    /// The PR facts with repo metadata and stack applied.
    pub pr: Option<PullRequest>,
    pub known: bool,
    pub stack: Option<StackView>,
    pub metadata: Option<RepoMergeMetadata>,
    pub error: Option<String>,
}

impl PrView {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "pull_request": self.pr,
            "known": self.known,
            "stack": self.stack.as_ref().map(|s| stack_json(s, self.pr.as_ref().and_then(|p| p.stack.as_ref()))),
            "merge_metadata": self.metadata.as_ref().map(metadata_json),
            "error": self.error,
        })
    }
}

#[derive(Deserialize)]
struct WorkspaceKey {
    workspace: String,
    #[serde(default)]
    refresh: bool,
}

impl Server {
    pub(crate) async fn github_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "pr.status" => {
                let key: WorkspaceKey = params(p)?;
                let workspace = self
                    .store
                    .workspace(&key.workspace)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", key.workspace)))?;
                let branch = local_branch(&workspace).await;
                let view = self
                    .pr_view(&workspace, branch.as_deref(), key.refresh)
                    .await;
                let previews = self.view_previews(&workspace, &view, key.refresh).await;
                let mut value = view.to_json();
                value["previews"] = json!(
                    previews
                        .as_ref()
                        .map(|p| &p.previews)
                        .unwrap_or(&Vec::new())
                );
                value["primary_preview"] =
                    json!(previews.as_ref().ok().and_then(|p| p.primary.as_ref()));
                if let Err(error) = previews {
                    value["previews_error"] = json!(error);
                }
                Ok(value)
            }
            "pr.previews" => {
                let key: WorkspaceKey = params(p)?;
                let workspace = self
                    .store
                    .workspace(&key.workspace)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", key.workspace)))?;
                let branch = local_branch(&workspace).await;
                let view = self
                    .pr_view(&workspace, branch.as_deref(), key.refresh)
                    .await;
                let found = self
                    .view_previews(&workspace, &view, key.refresh)
                    .await
                    .map_err(|e| rpc("github_failed", e))?;
                Ok(json!({
                    "number": view.pr.as_ref().map(|pr| pr.number),
                    "previews": found.previews,
                    "primary": found.primary,
                }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    /// One batched snapshot of every workspace branch in the project, cached for a few seconds.
    pub(crate) async fn project_prs(
        &self,
        project: &Project,
        extra_branch: Option<&str>,
        force: bool,
    ) -> Result<ProjectPrs, String> {
        let mut branches: Vec<String> = self
            .store
            .workspaces(Some(&project.id))
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|w| w.branch)
            .chain(extra_branch.map(str::to_owned))
            .collect();
        branches.sort();
        branches.dedup();
        if !force
            && let Some(cached) = lock(&self.github.prs).get(&project.id)
            && cached.at.elapsed() < PR_TTL
            && branches.iter().all(|b| cached.by_branch.contains_key(b))
        {
            return Ok(cached.clone());
        }
        let repo = repo_ref(project).ok_or_else(|| {
            format!(
                "project {} has no GitHub remote ({})",
                project.name,
                project.remote.as_deref().unwrap_or("none")
            )
        })?;
        let mut prs = ProjectPrs {
            at: Instant::now(),
            viewer: None,
            by_branch: HashMap::new(),
        };
        if !branches.is_empty() {
            let targets: Vec<PrTarget> = branches.iter().cloned().map(PrTarget::Branch).collect();
            let snapshot = self
                .github
                .client
                .pr_snapshot(&repo, &targets)
                .await
                .map_err(|e| e.to_string())?;
            prs.viewer = snapshot.viewer_login;
            for entry in snapshot.entries {
                if let PrTarget::Branch(branch) = entry.target {
                    prs.by_branch.insert(branch, entry.pull);
                }
            }
        }
        lock(&self.github.prs).insert(project.id.clone(), prs.clone());
        Ok(prs)
    }

    pub(crate) async fn merge_metadata(
        &self,
        repo: &RepoRef,
        base: &str,
    ) -> Result<RepoMergeMetadata, String> {
        let base = strip_remote(base);
        let key = format!("{}#{base}", repo.match_key(0));
        if let Some((at, meta)) = lock(&self.github.metadata).get(&key)
            && at.elapsed() < METADATA_TTL
        {
            return Ok(meta.clone());
        }
        let meta = self
            .github
            .client
            .repo_merge_metadata(repo, base)
            .await
            .map_err(|e| e.to_string())?;
        lock(&self.github.metadata).insert(key, (Instant::now(), meta.clone()));
        Ok(meta)
    }

    pub(crate) async fn stack_view(&self, repo: &RepoRef, number: u64) -> Option<StackView> {
        self.stack_view_cached(repo, number, false).await
    }

    async fn stack_view_cached(
        &self,
        repo: &RepoRef,
        number: u64,
        force: bool,
    ) -> Option<StackView> {
        let key = repo.match_key(number);
        if !force
            && let Some((at, view)) = lock(&self.github.stacks).get(&key)
            && at.elapsed() < PR_TTL
        {
            return view.clone();
        }
        let view = self.github.client.stack_view(repo, number).await.ok()?;
        lock(&self.github.stacks).insert(key, (Instant::now(), view.clone()));
        view
    }

    /// Preview deployments of the workspace's pull request, cached per head commit.
    pub(crate) async fn view_previews(
        &self,
        workspace: &Workspace,
        view: &PrView,
        force: bool,
    ) -> Result<PrPreviews, String> {
        let (Some(repo), Some(pr)) = (&view.repo, &view.pr) else {
            return Ok(PrPreviews::default());
        };
        let key = format!("{}@{}", repo.match_key(pr.number), pr.head_ref_oid);
        if !force
            && let Some((at, cached)) = lock(&self.github.previews).get(&key)
            && at.elapsed() < PREVIEW_TTL
        {
            return Ok(cached.clone());
        }
        let root = self
            .store
            .project(&workspace.project_id)
            .ok()
            .flatten()
            .map(|project| project.root);
        let settings = self.resolved_settings(root.as_deref());
        let config = PreviewConfig::from_settings(settings.get("previews").unwrap_or(&Value::Null))
            .map_err(|e| e.to_string())?;
        let found = self
            .github
            .client
            .pr_previews(repo, pr.number, &config)
            .await
            .map_err(|e| e.to_string())?;
        let mut cache = lock(&self.github.previews);
        cache.retain(|_, (at, _)| at.elapsed() < PREVIEW_TTL);
        cache.insert(key, (Instant::now(), found.clone()));
        Ok(found)
    }

    pub(crate) fn stack_intents(&self) -> serde_json::Map<String, Value> {
        match self.store.kv_get(STACK_INTENTS_KEY) {
            Ok(Some(Value::Object(map))) => map,
            _ => serde_json::Map::new(),
        }
    }

    pub(crate) fn set_stack_intent(
        &self,
        key: String,
        intent: Option<Value>,
    ) -> Result<(), String> {
        let mut intents = self.stack_intents();
        match intent {
            Some(value) => intents.insert(key, value),
            None => intents.remove(&key),
        };
        self.store
            .kv_set(STACK_INTENTS_KEY, &Value::Object(intents))
            .map_err(|e| e.to_string())
    }

    /// The workspace's PR (looked up by branch), with repo metadata and stack applied.
    pub(crate) async fn pr_view(
        &self,
        workspace: &Workspace,
        branch: Option<&str>,
        force: bool,
    ) -> PrView {
        let mut view = PrView {
            repo: None,
            details: None,
            pr: None,
            known: false,
            stack: None,
            metadata: None,
            error: None,
        };
        let project = match self.store.project(&workspace.project_id) {
            Ok(Some(project)) => project,
            Ok(None) => {
                view.error = Some("project not found".into());
                return view;
            }
            Err(e) => {
                view.error = Some(e.to_string());
                return view;
            }
        };
        view.repo = repo_ref(&project);
        let Some(branch) = branch.or(workspace.branch.as_deref()) else {
            view.error = Some("workspace has no branch".into());
            return view;
        };
        let prs = match self.project_prs(&project, Some(branch), force).await {
            Ok(prs) => prs,
            Err(error) => {
                view.error = Some(error);
                return view;
            }
        };
        view.known = true;
        view.details = prs.by_branch.get(branch).cloned().flatten();
        let Some(repo) = view.repo.clone() else {
            return view;
        };
        let base = view
            .details
            .as_ref()
            .map(|d| d.pr.base_ref_name.clone())
            .or_else(|| workspace.base_ref.clone())
            .or_else(|| project.base_ref.clone());
        if let Some(base) = base {
            match self.merge_metadata(&repo, &base).await {
                Ok(meta) => view.metadata = Some(meta),
                Err(error) => view.error = Some(error),
            }
        }
        if let Some(details) = &view.details {
            let mut pr = details.pr.clone();
            if pr.state == icarus_rules::facts::PrState::Open {
                view.stack = self.stack_view_cached(&repo, pr.number, force).await;
            }
            if let Some(stack) = &view.stack {
                let intent_key = repo.match_key(stack.summary.number);
                let auto_merge =
                    self.stack_intents()
                        .contains_key(&intent_key)
                        .then_some(StackAutoMerge {
                            mode: StackAutoMergeMode::AppManaged,
                            enabled_by: None,
                        });
                pr.stack = Some(stack.to_fact(None, auto_merge));
            }
            if let Some(meta) = &view.metadata {
                meta.apply(&mut pr);
            }
            view.pr = Some(pr);
        }
        view
    }

    /// Runs one pull request operation through `gh`, pinned to the PR's head commit.
    pub(crate) async fn run_pr_operation(
        &self,
        repo: &RepoRef,
        details: &PrDetails,
        pr: &PullRequest,
        metadata: Option<&RepoMergeMetadata>,
        operation: PrOperation,
    ) -> Result<Value, String> {
        let client = &self.github.client;
        let method = metadata
            .and_then(RepoMergeMetadata::default_method)
            .unwrap_or(MergeMethod::Squash);
        let head = pr.head_ref_oid.as_str();
        let error = |e: GitHubError| e.to_string();
        let result = match operation {
            PrOperation::Merge => {
                let MergeResult {
                    merged,
                    sha,
                    message,
                } = client
                    .merge_pull_request(repo, pr.number, method, head)
                    .await
                    .map_err(error)?;
                json!({ "operation": "merge", "merged": merged, "sha": sha, "message": message, "method": method })
            }
            PrOperation::EnableAutoMerge => {
                client
                    .enable_auto_merge(&details.node_id, method, Some(head))
                    .await
                    .map_err(error)?;
                json!({ "operation": "enable_auto_merge", "auto_merge": true, "method": method })
            }
            PrOperation::DisableAutoMerge => {
                client
                    .disable_auto_merge(&details.node_id)
                    .await
                    .map_err(error)?;
                json!({ "operation": "disable_auto_merge", "auto_merge": false })
            }
            PrOperation::AddToMergeQueue => {
                let entry = client
                    .enqueue(&details.node_id, Some(head), false)
                    .await
                    .map_err(error)?;
                json!({ "operation": "add_to_merge_queue", "queued": true, "entry": entry })
            }
            PrOperation::MergeStack => {
                let stack = pr
                    .stack
                    .as_ref()
                    .ok_or("the pull request is not in a stack")?;
                let target = stack_merge_target(stack).ok_or("the stack is not ready to merge")?;
                let target_head = stack
                    .entries
                    .iter()
                    .find(|e| e.number == target.number)
                    .and_then(|e| e.head_oid.clone())
                    .ok_or("the stack's top pull request has no head commit")?;
                let action = if pr.merge_queue_required == Some(true)
                    || stack.merge_queue_required == Some(true)
                {
                    StackMergeAction::Queue
                } else {
                    StackMergeAction::Direct(method)
                };
                let policy = PollPolicy {
                    interval: Duration::from_secs(1),
                    max_polls: 60,
                };
                let status = client
                    .merge_stack(repo, target.number, action, &target_head, policy)
                    .await
                    .map_err(error)?;
                let status = match status {
                    StackMergeStatus::Merged => "merged".to_owned(),
                    StackMergeStatus::Enqueued => "enqueued".to_owned(),
                    StackMergeStatus::Pending { uuid } => format!("pending {uuid}"),
                    StackMergeStatus::Failed(message) => return Err(message),
                };
                json!({ "operation": "merge_stack", "target": target.number, "count": target.count, "status": status })
            }
            PrOperation::DisableStackAutoMerge => {
                let stack = pr
                    .stack
                    .as_ref()
                    .ok_or("the pull request is not in a stack")?;
                self.set_stack_intent(repo.match_key(stack.number), None)?;
                json!({ "operation": "disable_stack_auto_merge", "stack_auto_merge": false })
            }
        };
        Ok(result)
    }

    /// Stack auto-merge is app-managed: remember the intent, and the rules loop merges once the stack is ready.
    pub(crate) fn enable_stack_auto_merge(
        &self,
        repo: &RepoRef,
        pr_number: u64,
        stack_number: u64,
        method: Option<MergeMethod>,
    ) -> Result<Value, String> {
        self.set_stack_intent(
            repo.match_key(stack_number),
            Some(json!({ "owner": repo.owner, "name": repo.name, "pr": pr_number, "stack": stack_number, "method": method })),
        )?;
        Ok(
            json!({ "operation": "enable_stack_auto_merge", "stack_auto_merge": true, "stack": stack_number }),
        )
    }
}

/// The branch checked out in the worktree, falling back to the recorded one.
pub(crate) async fn local_branch(workspace: &Workspace) -> Option<String> {
    let path = workspace.path.clone();
    let local = tokio::task::spawn_blocking(move || icarus_git::local_state(&path).ok())
        .await
        .ok()
        .flatten();
    local
        .and_then(|l| l.branch)
        .or_else(|| workspace.branch.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(remote: Option<&str>) -> Project {
        Project {
            id: "p".into(),
            name: "p".into(),
            root: PathBuf::from("/r"),
            remote: remote.map(str::to_owned),
            base_ref: None,
            created_at_ms: 0,
        }
    }

    #[test]
    fn only_github_remotes_map_to_a_repository() {
        assert_eq!(
            repo_ref(&project(Some("github.com/acme/widgets"))),
            Some(RepoRef::new("acme", "widgets"))
        );
        assert_eq!(repo_ref(&project(Some("gitlab.com/acme/widgets"))), None);
        assert_eq!(repo_ref(&project(Some("github.com/acme"))), None);
        assert_eq!(repo_ref(&project(None)), None);
    }

    #[test]
    fn fixture_snapshot_answers_branch_and_number_targets() {
        let fixtures = json!({
            "viewer": "me",
            "pulls": [{ "id": "PR_1", "number": 5, "state": "OPEN", "headRefName": "feat",
                "repository": { "name": "widgets", "owner": { "login": "acme" } } }],
        });
        let request = GhRequest::graphql(
            "query PrSnapshot($owner: String!) { x }",
            json!({ "owner": "acme", "name": "widgets", "b0": "feat", "n1": 5, "b2": "none" }),
        );
        let (status, body) = fixture_response(&fixtures, &request);
        assert_eq!(status, 200);
        let repo = &body["data"]["repository"];
        assert_eq!(repo["t0"]["nodes"][0]["number"], 5);
        assert_eq!(repo["t1"]["number"], 5);
        assert_eq!(repo["t2"]["nodes"], json!([]));
        assert_eq!(body["data"]["viewer"]["login"], "me");
    }
}
