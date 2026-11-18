//! GitHub through the user's `gh` login: pull request status per workspace and pull request operations.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::time::Instant;

use icarus_github::conversation::ConversationComment;
use icarus_github::merge::MergeResult;
use icarus_github::previews::{PrPreviews, PreviewConfig};
use icarus_github::rate::PollTier;
use icarus_github::repo::RepoMergeMetadata;
use icarus_github::review::GitHubUser;
use icarus_github::review_context::ReviewContext;
use icarus_github::review_threads::PrThread;
use icarus_github::snapshot::{PrDetails, PrTarget, ReviewInbox};
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

/// Longer than [`PR_POLL`], so requests between polls read what the poller fetched.
const PR_TTL: Duration = Duration::from_secs(90);
const METADATA_TTL: Duration = Duration::from_secs(1800);
/// Previews are cached per head commit, so a push shows its own at once; a deployment's status
/// change can wait a few minutes.
const PREVIEW_TTL: Duration = Duration::from_secs(330);
const USERS_TTL: Duration = Duration::from_secs(1800);
/// Threads are never polled: they're refetched when the PR's `updated_at` moves, on a manual
/// refresh or after a mutation; this only bounds how old a copy for an unchanged PR may get.
const THREADS_MAX_AGE: Duration = Duration::from_secs(1800);
/// The review inbox and team rosters feed the status rules, which tolerate a poll's delay.
const INBOX_TTL: Duration = PR_TTL;
const TEAMS_TTL: Duration = Duration::from_secs(3600);
pub(crate) const STACK_INTENTS_KEY: &str = "rules.stack_auto_merge";
const PR_CACHE_KEY: &str = "github.pr_cache";
/// A `gh` call slower than this is logged with its route and duration.
const SLOW_GH: Duration = Duration::from_millis(1500);
/// How often cards' pull requests refresh while a client is connected, as Orca's status rules do:
/// one batched query per repository costs about one GraphQL point.
pub(crate) const PR_POLL: Duration = Duration::from_secs(60);
/// Under 500 GraphQL points left the cards refresh rarely, keeping the rest for what the user does.
const PR_POLL_LOW_QUOTA: Duration = Duration::from_secs(300);
/// Wake-ups closer together than this share one poll.
const PR_POLL_GAP: Duration = Duration::from_secs(2);

/// Keeps every workspace card's pull request current while a client is connected. Every
/// [`PR_POLL`] it refetches every project; a wake-up (a client connected, worktrees changed) only
/// fetches projects whose copy is missing a branch or has expired.
pub(crate) fn spawn(server: Arc<Server>) {
    server.load_pr_cache();
    tokio::spawn(async move {
        let mut last: Option<Instant> = None;
        let mut next_full = Instant::now();
        loop {
            if server.client_count() == 0 {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(60)) => {}
                    () = server.github.poll.notified() => {}
                }
                continue;
            }
            if let Some(last) = last {
                tokio::time::sleep(PR_POLL_GAP.saturating_sub(last.elapsed())).await;
            }
            let full = Instant::now() >= next_full;
            let budget = server.github.client.budget();
            match budget.tier() {
                // Under the floor nothing is sent until GitHub's reset; cards keep their copies.
                PollTier::Stopped => {}
                PollTier::Full | PollTier::FingerprintOnly => {
                    last = Some(Instant::now());
                    server.pr_poll(full).await;
                }
            }
            if full {
                next_full = Instant::now() + poll_wait(&server.github.client.budget());
            }
            tokio::select! {
                () = tokio::time::sleep_until(next_full) => {}
                () = server.github.poll.notified() => {}
            }
        }
    });
}

/// How long until the next full poll: [`PR_POLL`], longer when the quota runs low, and never
/// before GitHub's reset once it's spent.
fn poll_wait(budget: &icarus_github::rate::RateBudget) -> Duration {
    match budget.tier() {
        PollTier::Full => PR_POLL,
        PollTier::FingerprintOnly => PR_POLL_LOW_QUOTA,
        PollTier::Stopped => budget
            .resume_at("graphql")
            .map(|at| Duration::from_secs(at.saturating_sub(icarus_github::rate::now_epoch()) + 5))
            .unwrap_or(PR_POLL_LOW_QUOTA)
            .max(PR_POLL),
    }
}

/// The `gh` runner the server uses: the real CLI, or fixtures when `ICARUS_GH_FIXTURES` names a file.
pub enum ServerGh {
    Cli(GhCli),
    Fixture(FixtureGh),
}

impl Gh for ServerGh {
    async fn execute(&self, request: &GhRequest) -> std::io::Result<GhOutput> {
        let started = Instant::now();
        let output = match self {
            Self::Cli(cli) => cli.execute(request).await,
            Self::Fixture(fixture) => fixture.execute(request).await,
        };
        let took = started.elapsed();
        if took > SLOW_GH {
            tracing::info!("gh {} took {took:.1?}", request.route());
        } else {
            tracing::debug!("gh {} took {took:.1?}", request.route());
        }
        output
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
        let raw = &fixtures["routes"][&route];
        if let Some(content_type) = raw["content_type"].as_str() {
            let response = FakeResponse::status(raw["status"].as_u64().unwrap_or(200) as u16)
                .header("Content-Type", content_type);
            let response = FakeResponse {
                body: raw["text"].as_str().unwrap_or_default().to_owned(),
                ..response
            };
            return FakeGh::new().on(route, response).execute(request).await;
        }
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
        "graphql PrConversation" => {
            let number = variables["number"].to_string();
            let connection = |key: &str, after: &Value| {
                let start = after
                    .as_str()
                    .and_then(|cursor| cursor.parse::<usize>().ok())
                    .unwrap_or(0);
                let nodes: Vec<Value> = fixtures[key][&number]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .skip(start)
                    .take(100)
                    .cloned()
                    .collect();
                let total = fixtures[key][&number].as_array().map_or(0, Vec::len);
                let next = (start + 100 < total).then(|| (start + 100).to_string());
                json!({ "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next }, "nodes": nodes })
            };
            let mut pull = json!({});
            if variables["withComments"] == json!(true) {
                pull["comments"] = connection("issue_comments", &variables["commentsAfter"]);
            }
            if variables["withReviews"] == json!(true) {
                pull["reviews"] = connection("reviews", &variables["reviewsAfter"]);
            }
            data(json!({ "repository": { "pullRequest": pull } }))
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
        "graphql ReviewerSuggestions" => {
            let reviews: Vec<Value> = fixtures["recent_reviewers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|login| json!({ "author": { "__typename": "User", "login": login } }))
                .collect();
            let author = fixture_pulls(fixtures)
                .find(|p| p["number"] == variables["number"])
                .map(|p| p["author"].clone())
                .unwrap_or(Value::Null);
            let suggested = match &fixtures["suggested_reviewers"] {
                Value::Array(users) => users
                    .iter()
                    .map(|user| json!({ "reviewer": user }))
                    .collect(),
                _ => Vec::new(),
            };
            data(json!({ "repository": {
                "pullRequest": { "author": author, "suggestedReviewers": suggested },
                "pullRequests": { "nodes": [ { "reviews": { "nodes": reviews } } ] } } }))
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
        ("POST", ["repos", owner, name, "issues", number, "comments"]) => (
            201,
            json!({ "id": 9002, "node_id": "IC_now",
                "html_url": format!("https://github.com/{owner}/{name}/pull/{number}#issuecomment-9002") }),
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
    fetched: HashMap<String, Instant>,
}

/// A copy read at an instant for a PR as of its `updated_at`; `None` for the instant marks a copy
/// that must be refetched but can stand in if that fails.
struct PrCopy<T> {
    at: Option<Instant>,
    updated_at: Option<String>,
    value: T,
}

impl<T> PrCopy<T> {
    /// Still good for the PR as last polled: not invalidated, not older than `max_age`, and read
    /// for the same `updated_at` (an unknown one can't prove it moved).
    fn current(&self, updated_at: Option<&str>, max_age: Duration) -> bool {
        self.at.is_some_and(|at| at.elapsed() < max_age)
            && (updated_at.is_none() || self.updated_at.as_deref() == updated_at)
    }
}

type ThreadsCache = HashMap<String, PrCopy<Vec<PrThread>>>;
type ConversationCache = HashMap<String, PrCopy<Vec<ConversationComment>>>;
type TeamRoster = BTreeMap<String, Vec<String>>;

pub struct GitHubService {
    pub(crate) client: GitHub<ServerGh>,
    prs: Mutex<HashMap<String, ProjectPrs>>,
    metadata: Mutex<HashMap<String, (Instant, RepoMergeMetadata)>>,
    stacks: Mutex<HashMap<String, (Instant, Option<StackView>)>>,
    previews: Mutex<HashMap<String, (Instant, PrPreviews)>>,
    users: Mutex<HashMap<String, (Instant, Vec<GitHubUser>)>>,
    threads: Mutex<ThreadsCache>,
    threads_loading: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    conversations: Mutex<ConversationCache>,
    contexts: Mutex<HashMap<String, PrCopy<ReviewContext>>>,
    inbox: Mutex<Option<(Instant, Vec<String>, ReviewInbox)>>,
    teams: Mutex<HashMap<String, (Instant, TeamRoster)>>,
    viewer_login: Mutex<Option<String>>,
    poll: tokio::sync::Notify,
    pub(crate) refreshes: Mutex<crate::pr_refresh::PrRefreshes>,
}

fn is_rate_limit(error: &GitHubError) -> bool {
    matches!(
        error,
        GitHubError::RateLimited { .. }
            | GitHubError::Http {
                retry_after: Some(_),
                ..
            }
    )
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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
            threads: Mutex::new(HashMap::new()),
            threads_loading: Mutex::new(HashMap::new()),
            conversations: Mutex::new(HashMap::new()),
            contexts: Mutex::new(HashMap::new()),
            inbox: Mutex::new(None),
            teams: Mutex::new(HashMap::new()),
            viewer_login: Mutex::new(None),
            poll: tokio::sync::Notify::new(),
            refreshes: Mutex::default(),
        }
    }

    /// The `updated_at` of a PR as the last poll saw it.
    pub(crate) fn cached_updated_at(&self, repo: &RepoRef, number: u64) -> Option<String> {
        lock(&self.prs)
            .values()
            .flat_map(|prs| prs.by_branch.values().flatten())
            .find(|details| details.pr.number == number && same_repo(&details.pr.repo, repo))
            .and_then(|details| details.updated_at.clone())
    }

    /// When GitHub calls may go out again (epoch seconds) while the GraphQL quota is spent or
    /// GitHub asked to back off.
    pub(crate) fn rate_limited_until(&self) -> Option<u64> {
        self.client.budget().resume_at("graphql")
    }

    /// The PR's review context (author, commits, the viewer's reviews), read again only when the
    /// PR's `updated_at` moves, after a mutation, or on `refresh`.
    pub(crate) async fn review_context(
        &self,
        repo: &RepoRef,
        number: u64,
        refresh: bool,
    ) -> Result<ReviewContext, GitHubError> {
        let key = repo.match_key(number);
        let updated_at = self.cached_updated_at(repo, number);
        if !refresh
            && let Some(copy) = lock(&self.contexts).get(&key)
            && copy.current(updated_at.as_deref(), PR_TTL)
        {
            return Ok(copy.value.clone());
        }
        match self.client.review_context(repo, number).await {
            Ok(context) => {
                lock(&self.contexts).insert(
                    key,
                    PrCopy {
                        at: Some(Instant::now()),
                        updated_at,
                        value: context.clone(),
                    },
                );
                Ok(context)
            }
            Err(error) => match lock(&self.contexts).get(&key) {
                Some(copy) if is_rate_limit(&error) => Ok(copy.value.clone()),
                _ => Err(error),
            },
        }
    }

    /// Pull requests waiting for the viewer's review in `repos`, shared by rule ticks for a poll.
    pub(crate) async fn review_inbox(&self, repos: &[RepoRef]) -> Result<ReviewInbox, GitHubError> {
        let slugs: Vec<String> = repos.iter().map(RepoRef::slug).collect();
        if let Some((at, known, inbox)) = &*lock(&self.inbox)
            && *known == slugs
            && at.elapsed() < INBOX_TTL
        {
            return Ok(inbox.clone());
        }
        let inbox = self.client.review_inbox(repos).await?;
        *lock(&self.inbox) = Some((Instant::now(), slugs, inbox.clone()));
        Ok(inbox)
    }

    pub(crate) async fn team_roster(
        &self,
        team: &str,
    ) -> Result<BTreeMap<String, Vec<String>>, GitHubError> {
        if let Some((at, roster)) = lock(&self.teams).get(team)
            && at.elapsed() < TEAMS_TTL
        {
            return Ok(roster.clone());
        }
        let roster = self.client.team_roster(&[team.to_owned()]).await?;
        lock(&self.teams).insert(team.to_owned(), (Instant::now(), roster.clone()));
        Ok(roster)
    }

    /// The signed-in login, asked of GitHub once; it can't change while `gh` stays logged in.
    pub(crate) async fn viewer_login(&self) -> Result<String, GitHubError> {
        if let Some(login) = self.viewer().or_else(|| lock(&self.viewer_login).clone()) {
            return Ok(login);
        }
        let login = self.client.viewer_login().await?;
        *lock(&self.viewer_login) = Some(login.clone());
        Ok(login)
    }

    /// Asks the poller to refresh every project's pull requests now (a client connected, worktrees changed).
    pub(crate) fn wake_poll(&self) {
        self.poll.notify_one();
    }

    /// A PR's review threads, shared by every open diff tab, file editor and panel until the PR's
    /// `updated_at` moves (a comment, review or push changes it) or a mutation marks them stale.
    pub(crate) async fn review_threads(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<PrThread>, GitHubError> {
        let key = repo.match_key(number);
        let updated_at = self.cached_updated_at(repo, number);
        let fresh = |cache: &ThreadsCache| {
            cache
                .get(&key)
                .filter(|copy| copy.current(updated_at.as_deref(), THREADS_MAX_AGE))
                .map(|copy| copy.value.clone())
        };
        if let Some(threads) = fresh(&lock(&self.threads)) {
            return Ok(threads);
        }
        // Every open diff, editor and panel asks at once; one fetch answers them all.
        let gate = lock(&self.threads_loading)
            .entry(key.clone())
            .or_default()
            .clone();
        let _loading = gate.lock().await;
        if let Some(threads) = fresh(&lock(&self.threads)) {
            return Ok(threads);
        }
        match self.client.pr_review_threads(repo, number).await {
            Ok(threads) => {
                lock(&self.threads).insert(
                    key,
                    PrCopy {
                        at: Some(Instant::now()),
                        updated_at,
                        value: threads.clone(),
                    },
                );
                Ok(threads)
            }
            // While GitHub refuses (rate limit, outage) the last copy beats an empty list.
            Err(error) => match lock(&self.threads).get_mut(&key) {
                Some(copy) => {
                    tracing::warn!("review threads kept from cache: {error}");
                    copy.at = Some(Instant::now());
                    copy.updated_at = updated_at;
                    Ok(copy.value.clone())
                }
                None => Err(error),
            },
        }
    }

    /// The PR's issue comments and review summaries, cached like [`Self::review_threads`]; only the
    /// PR panel's Comments section asks, and each refetch is conditional.
    pub(crate) async fn conversation(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ConversationComment>, GitHubError> {
        let key = repo.match_key(number);
        let updated_at = self.cached_updated_at(repo, number);
        if let Some(copy) = lock(&self.conversations).get(&key)
            && copy.current(updated_at.as_deref(), THREADS_MAX_AGE)
        {
            return Ok(copy.value.clone());
        }
        match self.client.pr_conversation(repo, number).await {
            Ok(entries) => {
                lock(&self.conversations).insert(
                    key,
                    PrCopy {
                        at: Some(Instant::now()),
                        updated_at,
                        value: entries.clone(),
                    },
                );
                Ok(entries)
            }
            Err(error) => match lock(&self.conversations).get_mut(&key) {
                Some(copy) => {
                    tracing::warn!("PR conversation kept from cache: {error}");
                    copy.at = Some(Instant::now());
                    copy.updated_at = updated_at;
                    Ok(copy.value.clone())
                }
                None => Err(error),
            },
        }
    }

    /// Marks every cached PR's threads, conversation and review context stale, keeping them to
    /// fall back on if the refetch fails.
    pub(crate) fn forget_review_threads(&self) {
        for copy in lock(&self.threads).values_mut() {
            copy.at = None;
        }
        for copy in lock(&self.conversations).values_mut() {
            copy.at = None;
        }
        for copy in lock(&self.contexts).values_mut() {
            copy.at = None;
        }
    }

    /// Applies a mutation GitHub confirmed to the cached thread, so the next read shows it without
    /// fetching every thread again; false when no cached PR holds the thread.
    pub(crate) fn patch_thread(&self, thread_id: &str, patch: impl FnOnce(&mut PrThread)) -> bool {
        let mut cached = lock(&self.threads);
        let found = cached
            .values_mut()
            .flat_map(|copy| copy.value.iter_mut())
            .find(|thread| thread.id == thread_id);
        match found {
            Some(thread) => {
                patch(thread);
                true
            }
            None => false,
        }
    }

    /// Applies a confirmed mutation to the cached conversation that `patch` finds its entry in
    /// (it answers true), so the panel's next read shows it without asking GitHub again.
    pub(crate) fn patch_conversation(
        &self,
        mut patch: impl FnMut(&mut Vec<ConversationComment>) -> bool,
    ) -> bool {
        lock(&self.conversations)
            .values_mut()
            .any(|copy| patch(&mut copy.value))
    }

    /// Adds a comment GitHub just accepted to the PR's cached conversation, if one is cached.
    pub(crate) fn push_conversation(
        &self,
        repo: &RepoRef,
        number: u64,
        comment: ConversationComment,
    ) -> bool {
        match lock(&self.conversations).get_mut(&repo.match_key(number)) {
            Some(copy) => {
                copy.value.push(comment);
                true
            }
            None => false,
        }
    }

    /// Adds a thread GitHub just accepted to the PR's cached threads. The REST reply carries no
    /// thread node id; the PR's moved `updated_at` makes the next poll read the real one.
    pub(crate) fn push_thread(&self, repo: &RepoRef, number: u64, thread: PrThread) -> bool {
        match lock(&self.threads).get_mut(&repo.match_key(number)) {
            Some(copy) => {
                copy.value.push(thread);
                true
            }
            None => false,
        }
    }

    /// The viewer's avatar, from any cached comment they wrote.
    pub(crate) fn viewer_avatar(&self, login: &str) -> Option<String> {
        let from_threads = lock(&self.threads).values().find_map(|copy| {
            copy.value
                .iter()
                .flat_map(|thread| &thread.comments)
                .filter(|comment| comment.author.as_deref() == Some(login))
                .find_map(|comment| comment.author_avatar_url.clone())
        });
        from_threads.or_else(|| {
            lock(&self.conversations).values().find_map(|copy| {
                copy.value
                    .iter()
                    .filter(|entry| entry.author.as_deref() == Some(login))
                    .find_map(|entry| entry.author_avatar_url.clone())
            })
        })
    }

    /// Like [`Self::patch_thread`], for one comment of a cached thread.
    pub(crate) fn patch_comment(
        &self,
        comment_id: &str,
        patch: impl FnOnce(&mut icarus_github::review_threads::PrComment),
    ) -> bool {
        let mut cached = lock(&self.threads);
        let found = cached
            .values_mut()
            .flat_map(|copy| copy.value.iter_mut())
            .flat_map(|thread| thread.comments.iter_mut())
            .find(|comment| comment.id == comment_id);
        match found {
            Some(comment) => {
                patch(comment);
                true
            }
            None => false,
        }
    }

    /// The signed-in GitHub user, as the last pull request poll reported it.
    pub(crate) fn viewer(&self) -> Option<String> {
        lock(&self.prs).values().find_map(|prs| prs.viewer.clone())
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
        self.forget_stacks();
    }

    pub(crate) fn forget_stacks(&self) {
        lock(&self.stacks).clear();
    }

    /// When the branch's pull request was last read from GitHub, by a poll or a targeted refresh.
    pub(crate) fn fetched_at(&self, project_id: &str, branch: &str) -> Option<Instant> {
        lock(&self.prs)
            .get(project_id)?
            .fetched
            .get(branch)
            .copied()
    }

    /// Replaces one branch's pull request in the project's copy, leaving the other branches' age alone.
    pub(crate) fn store_branch_pr(&self, project_id: &str, branch: &str, pull: Option<PrDetails>) {
        let now = Instant::now();
        let mut prs = lock(&self.prs);
        let project = prs
            .entry(project_id.to_owned())
            .or_insert_with(|| ProjectPrs {
                at: now,
                viewer: None,
                by_branch: HashMap::new(),
                fetched: HashMap::new(),
            });
        project.by_branch.insert(branch.to_owned(), pull);
        project.fetched.insert(branch.to_owned(), now);
    }

    /// The pull request of a branch as last polled, however old.
    pub(crate) fn cached_pr(&self, project_id: &str, branch: &str) -> Option<PullRequest> {
        lock(&self.prs)
            .get(project_id)?
            .by_branch
            .get(branch)?
            .as_ref()
            .map(|details| details.pr.clone())
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
                    "stack": match &pr.stack {
                        Some(stack) => json!({
                            "number": stack.number,
                            "position": stack.position,
                            "size": stack.size,
                        }),
                        None => branch_stack(&cached_pulls(&prs, project_id), pr)
                            .map(|(number, position, size)| json!({
                                "number": number,
                                "position": position,
                                "size": size,
                            }))
                            .unwrap_or(Value::Null),
                    },
                })
            })
            .unwrap_or(Value::Null)
    }
}

fn cached_pulls<'a>(
    prs: &'a HashMap<String, ProjectPrs>,
    project_id: &str,
) -> Vec<&'a PullRequest> {
    prs.get(project_id)
        .map(|cached| {
            cached
                .by_branch
                .values()
                .flatten()
                .map(|details| &details.pr)
                .collect()
        })
        .unwrap_or_default()
}

/// A stack made of plain branches, without GitHub's stacks feature: open PRs each based on the one
/// below's head branch. Returns (bottom PR number, 1-based position from the bottom, size) when
/// `pr` sits in a chain of two or more.
pub(crate) fn branch_stack(pulls: &[&PullRequest], pr: &PullRequest) -> Option<(u64, u32, u32)> {
    let open: Vec<&PullRequest> = pulls
        .iter()
        .copied()
        .filter(|pull| pull.state == icarus_rules::facts::PrState::Open)
        .collect();
    let based_on = |pull: &PullRequest| {
        let base = strip_remote(&pull.base_ref_name);
        open.iter()
            .copied()
            .find(|below| below.number != pull.number && below.head_ref_name == base)
    };
    let mut chain = vec![pr];
    let mut lowest = pr;
    while let Some(below) = based_on(lowest) {
        if chain.iter().any(|seen| seen.number == below.number) {
            break;
        }
        chain.push(below);
        lowest = below;
    }
    chain.reverse();
    let position = chain.len() as u32;
    let mut top = pr;
    loop {
        let above = open
            .iter()
            .copied()
            .filter(|pull| {
                pull.number != top.number
                    && strip_remote(&pull.base_ref_name) == top.head_ref_name
                    && !chain.iter().any(|seen| seen.number == pull.number)
            })
            .min_by_key(|pull| pull.number);
        match above {
            Some(above) => {
                chain.push(above);
                top = above;
            }
            None => break,
        }
    }
    (chain.len() >= 2).then_some((lowest.number, position, chain.len() as u32))
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
    /// While GitHub's quota is spent: when calls resume (epoch seconds); what's shown is cached.
    pub rate_limited_until: Option<u64>,
}

impl PrView {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "pull_request": self.pr,
            "known": self.known,
            "stack": self.stack.as_ref().map(|s| stack_json(s, self.pr.as_ref().and_then(|p| p.stack.as_ref()))),
            "merge_metadata": self.metadata.as_ref().map(metadata_json),
            "error": self.error,
            "rate_limited_until": self.rate_limited_until,
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
            "github.asset" => {
                #[derive(Deserialize)]
                struct AssetParams {
                    url: String,
                }
                let asset: AssetParams = params(p)?;
                self.github_asset(&asset.url).await
            }
            "pr.body" => {
                let key: WorkspaceKey = params(p)?;
                let workspace = self
                    .store
                    .workspace(&key.workspace)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", key.workspace)))?;
                let branch = local_branch(&workspace).await;
                let view = self.pr_view(&workspace, branch.as_deref(), false).await;
                let Some(pr) = view.pr else {
                    return Err(rpc("no_pr", "this workspace has no pull request"));
                };
                // Conditional, so asking again for an unchanged PR costs no quota (HTTP 304).
                let pull = self
                    .github
                    .client
                    .get_conditional(icarus_github::GhRequest::get(format!(
                        "repos/{}/{}/pulls/{}",
                        pr.repo.owner, pr.repo.name, pr.number
                    )))
                    .await
                    .map_err(|e| rpc("github_failed", e))?
                    .body;
                Ok(json!({
                    "number": pr.number,
                    "body": pull["body"].as_str().unwrap_or_default(),
                }))
            }
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
            "pr.operate" => {
                #[derive(Deserialize)]
                struct OperateParams {
                    workspace: String,
                    operation: String,
                }
                let op: OperateParams = params(p)?;
                let workspace = self
                    .store
                    .workspace(&op.workspace)
                    .map_err(|e| rpc("internal", e))?
                    .ok_or_else(|| rpc("not_found", format!("workspace {}", op.workspace)))?;
                let branch = local_branch(&workspace).await;
                let view = self.pr_view(&workspace, branch.as_deref(), true).await;
                let (Some(repo), Some(details), Some(pr)) = (&view.repo, &view.details, &view.pr)
                else {
                    return Err(rpc("no_pr", "this workspace has no pull request"));
                };
                let client = &self.github.client;
                let operation = match op.operation.as_str() {
                    "merge" => Some(PrOperation::Merge),
                    "enable_auto_merge" => Some(PrOperation::EnableAutoMerge),
                    "disable_auto_merge" => Some(PrOperation::DisableAutoMerge),
                    "add_to_merge_queue" => Some(PrOperation::AddToMergeQueue),
                    "merge_stack" => Some(PrOperation::MergeStack),
                    "disable_stack_auto_merge" => Some(PrOperation::DisableStackAutoMerge),
                    _ => None,
                };
                let result = match (op.operation.as_str(), operation) {
                    (_, Some(operation)) => {
                        self.run_pr_operation(repo, details, pr, view.metadata.as_ref(), operation)
                            .await
                    }
                    ("ready", None) => client
                        .mark_ready_for_review(&details.node_id)
                        .await
                        .map(|()| json!({ "operation": "ready", "draft": false }))
                        .map_err(|e| e.to_string()),
                    ("dequeue", None) => client
                        .dequeue(&details.node_id)
                        .await
                        .map(|()| json!({ "operation": "dequeue", "queued": false }))
                        .map_err(|e| e.to_string()),
                    ("enable_stack_auto_merge", None) => match pr.stack.as_ref() {
                        Some(stack) => {
                            self.enable_stack_auto_merge(repo, pr.number, stack.number, None)
                        }
                        None => Err("not in a stack".to_owned()),
                    },
                    (other, None) => {
                        return Err(rpc(
                            "invalid_params",
                            format!(
                                "unknown operation {other}: use ready, merge, enable_auto_merge, disable_auto_merge, add_to_merge_queue, dequeue, merge_stack, enable_stack_auto_merge or disable_stack_auto_merge"
                            ),
                        ));
                    }
                }
                .map_err(|e| rpc("github_failed", e))?;
                self.github.forget_stacks();
                self.schedule_pr_refresh(&workspace.id, crate::pr_refresh::RefreshReason::Action);
                Ok(result)
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
        // The base branch (the primary checkout's, usually) never heads a pull request of its own.
        let base_branch = project
            .base_ref
            .as_deref()
            .map(|base| base.rsplit('/').next().unwrap_or(base));
        branches.retain(|branch| Some(branch.as_str()) != base_branch);
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
            fetched: HashMap::new(),
        };
        if !branches.is_empty() {
            let targets: Vec<PrTarget> = branches.iter().cloned().map(PrTarget::Branch).collect();
            let snapshot = match self.github.client.pr_snapshot(&repo, &targets).await {
                Ok(snapshot) => snapshot,
                // Rate limited or GitHub down: the last copy, however old, beats no pull request.
                Err(error) => {
                    return match lock(&self.github.prs).get(&project.id) {
                        Some(cached) => {
                            tracing::warn!(
                                "{}: pull requests kept from cache: {error}",
                                project.name
                            );
                            Ok(cached.clone())
                        }
                        None => Err(error.to_string()),
                    };
                }
            };
            prs.viewer = snapshot.viewer_login;
            for entry in snapshot.entries {
                if let PrTarget::Branch(branch) = entry.target {
                    prs.fetched.insert(branch.clone(), prs.at);
                    prs.by_branch.insert(branch, entry.pull);
                }
            }
        }
        lock(&self.github.prs).insert(project.id.clone(), prs.clone());
        self.save_pr_cache();
        Ok(prs)
    }

    /// Every project's pull requests in one batched query per repository, all repositories at once;
    /// returns the workspaces whose card changed and tells clients about them. Unless `full`, only
    /// projects whose copy expired or lacks a branch are asked.
    pub(crate) async fn pr_poll(self: &Arc<Self>, full: bool) -> Vec<String> {
        let started = Instant::now();
        let projects: Vec<Project> = match self.store.projects() {
            Ok(projects) => projects
                .into_iter()
                .filter(|p| repo_ref(p).is_some())
                .collect(),
            Err(error) => {
                tracing::warn!("pr poll: {error:#}");
                return Vec::new();
            }
        };
        let workspaces = self.store.workspaces(None).unwrap_or_default();
        let cards = |server: &Self| -> HashMap<String, Value> {
            workspaces
                .iter()
                .filter_map(|w| {
                    let branch = w.branch.as_deref()?;
                    Some((
                        w.id.clone(),
                        server.github.cached_pr_summary(&w.project_id, branch),
                    ))
                })
                .collect()
        };
        let before = cards(self);
        let fetches: Vec<_> = projects
            .into_iter()
            .map(|project| {
                let server = Arc::clone(self);
                tokio::spawn(async move {
                    let started = Instant::now();
                    let result = server.project_prs(&project, None, full).await;
                    (project, started.elapsed(), result)
                })
            })
            .collect();
        let mut fetched = 0;
        for fetch in fetches {
            match fetch.await {
                Ok((project, took, Ok(prs))) => {
                    fetched += 1;
                    tracing::debug!(
                        "pr poll: {} ({} branches) in {took:.1?}",
                        project.name,
                        prs.by_branch.len()
                    );
                }
                Ok((project, took, Err(error))) => {
                    tracing::warn!("pr poll: {} failed after {took:.1?}: {error}", project.name)
                }
                Err(error) => tracing::warn!("pr poll: {error}"),
            }
        }
        let after = cards(self);
        let changed: Vec<String> = after
            .iter()
            .filter(|(id, card)| before.get(*id) != Some(*card))
            .map(|(id, _)| id.clone())
            .collect();
        if !changed.is_empty() {
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": changed, "source": "github" }),
            );
        }
        let took = started.elapsed();
        if took > Duration::from_secs(2) {
            tracing::info!("pr poll took {took:.1?} for {fetched} project(s)");
        }
        changed
    }

    pub(crate) fn save_pr_cache(&self) {
        let saved: HashMap<String, HashMap<String, Option<PrDetails>>> = lock(&self.github.prs)
            .iter()
            .map(|(project, prs)| (project.clone(), prs.by_branch.clone()))
            .collect();
        match serde_json::to_value(&saved) {
            Ok(value) => {
                if let Err(error) = self.store.kv_set(PR_CACHE_KEY, &value) {
                    tracing::warn!("saving pull requests: {error:#}");
                }
            }
            Err(error) => tracing::warn!("saving pull requests: {error}"),
        }
    }

    /// The pull requests the last server knew, so cards show them before the first poll answers.
    pub(crate) fn load_pr_cache(&self) {
        let Ok(Some(value)) = self.store.kv_get(PR_CACHE_KEY) else {
            return;
        };
        let Ok(saved) =
            serde_json::from_value::<HashMap<String, HashMap<String, Option<PrDetails>>>>(value)
        else {
            return;
        };
        // Stale on purpose: shown at once, but the first request or poll fetches afresh.
        let at = Instant::now()
            .checked_sub(PR_TTL)
            .unwrap_or_else(Instant::now);
        let mut prs = lock(&self.github.prs);
        for (project, by_branch) in saved {
            prs.entry(project).or_insert(ProjectPrs {
                at,
                viewer: None,
                by_branch,
                fetched: HashMap::new(),
            });
        }
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
            rate_limited_until: None,
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
        let prs = self.project_prs(&project, Some(branch), force).await;
        view.rate_limited_until = self.github.rate_limited_until();
        let prs = match prs {
            Ok(prs) => prs,
            Err(error) => {
                view.error = Some(error);
                return view;
            }
        };
        // A copy kept through a failed fetch may predate this branch.
        view.known = prs.by_branch.contains_key(branch);
        if !view.known {
            view.error = Some("GitHub didn't answer for this branch yet".into());
        }
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

/// Total size of fetched comment media kept on disk; the oldest files go first past it.
const ASSET_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

fn asset_cache_key(url: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(url.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Deletes the least recently written cache files until the folder fits `limit`.
fn prune_asset_cache(dir: &std::path::Path, limit: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry
                .metadata()
                .ok()
                .filter(|metadata| metadata.is_file())?;
            Some((metadata.modified().ok()?, metadata.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    files.sort();
    for (_, size, path) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

impl Server {
    /// A comment's image or video, fetched with the user's `gh` login and cached under the server home.
    pub(crate) async fn github_asset(&self, url: &str) -> Result<Value, RpcError> {
        use base64::Engine as _;
        if !icarus_github::attachment::is_github_asset_url(url) {
            return Err(rpc(
                "invalid",
                "only media GitHub hosts for comments can be fetched",
            ));
        }
        let dir = self.paths.home.join("assets");
        let key = asset_cache_key(url);
        let (body_path, type_path) = (dir.join(&key), dir.join(format!("{key}.type")));
        let cached = match (
            std::fs::read(&body_path),
            std::fs::read_to_string(&type_path),
        ) {
            (Ok(bytes), Ok(content_type)) => Some((content_type, bytes)),
            _ => None,
        };
        let (content_type, bytes, hit) = match cached {
            Some((content_type, bytes)) => (content_type, bytes, true),
            None => {
                let asset = self
                    .github
                    .client
                    .fetch_asset(url)
                    .await
                    .map_err(|error| rpc("github_failed", error))?;
                let written = std::fs::create_dir_all(&dir)
                    .and_then(|()| std::fs::write(&body_path, &asset.bytes))
                    .and_then(|()| std::fs::write(&type_path, &asset.content_type));
                if let Err(error) = written {
                    tracing::warn!("caching {url} failed: {error}");
                }
                prune_asset_cache(&dir, ASSET_CACHE_BYTES);
                (asset.content_type, asset.bytes, false)
            }
        };
        Ok(json!({
            "url": url,
            "content_type": content_type,
            "size": bytes.len(),
            "cached": hit,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }))
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

    fn pull(number: u64, head: &str, base: &str) -> PullRequest {
        PullRequest {
            number,
            head_ref_name: head.into(),
            base_ref_name: base.into(),
            ..PullRequest::default()
        }
    }

    #[test]
    fn prs_based_on_each_others_branches_form_a_stack() {
        let bottom = pull(
            4147,
            "feature/e-7225-player-reports-impressions-from-one-playback",
            "main",
        );
        let top = pull(
            4148,
            "feature/e-6052-playlist-stats-tab",
            "feature/e-7225-player-reports-impressions-from-one-playback",
        );
        let loose = pull(4100, "fix/typo", "main");
        let pulls = [&top, &loose, &bottom];
        assert_eq!(branch_stack(&pulls, &bottom), Some((4147, 1, 2)));
        assert_eq!(branch_stack(&pulls, &top), Some((4147, 2, 2)));
        assert_eq!(branch_stack(&pulls, &loose), None);

        let a = pull(1, "a", "main");
        let b = pull(2, "b", "origin/a");
        let c = pull(3, "c", "refs/heads/b");
        let three = [&c, &a, &b];
        assert_eq!(branch_stack(&three, &a), Some((1, 1, 3)));
        assert_eq!(branch_stack(&three, &b), Some((1, 2, 3)));
        assert_eq!(branch_stack(&three, &c), Some((1, 3, 3)));

        let merged = PullRequest {
            state: icarus_rules::facts::PrState::Merged,
            ..a.clone()
        };
        assert_eq!(
            branch_stack(&[&merged, &b, &c], &b),
            Some((2, 1, 2)),
            "a merged bottom leaves the stack"
        );
    }

    #[test]
    fn a_card_summary_shows_a_branch_stack() {
        let service = GitHubService::new(ServerGh::Fixture(FixtureGh::new(PathBuf::from(
            "/nonexistent",
        ))));
        let details = |pr: PullRequest| {
            Some(PrDetails {
                node_id: String::new(),
                pr,
                updated_at: None,
                additions: 0,
                deletions: 0,
                changed_files: 0,
                is_cross_repository: false,
                head_ref_deleted: false,
                base_ref_deleted: false,
                code_owner_requests: Vec::new(),
            })
        };
        let by_branch = HashMap::from([
            ("bottom".to_owned(), details(pull(10, "bottom", "main"))),
            ("top".to_owned(), details(pull(11, "top", "bottom"))),
        ]);
        lock(&service.prs).insert(
            "project".into(),
            ProjectPrs {
                at: Instant::now(),
                viewer: None,
                by_branch,
                fetched: HashMap::new(),
            },
        );
        assert_eq!(
            service.cached_pr_summary("project", "top")["stack"],
            json!({ "number": 10, "position": 2, "size": 2 })
        );
    }

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

    fn fixture_pull(repo: &str, number: u64, branch: &str, updated_at: &str) -> Value {
        json!({
            "id": format!("PR_{repo}_{number}"), "number": number, "title": format!("PR {number}"),
            "url": format!("https://github.com/acme/{repo}/pull/{number}"),
            "state": "OPEN", "isDraft": false, "updatedAt": updated_at,
            "headRefName": branch, "baseRefName": "main", "headRefOid": format!("{branch}-head"),
            "headRef": { "name": branch }, "baseRef": { "name": "main" },
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "author": { "login": "me" },
            "repository": { "name": repo, "owner": { "login": "acme" } },
            "commits": { "nodes": [] },
        })
    }

    fn write_fixtures(path: &std::path::Path, first_updated_at: &str) {
        let pulls = vec![
            fixture_pull("widgets", 1, "w-1", first_updated_at),
            fixture_pull("widgets", 2, "w-2", "2026-11-15T12:00:00Z"),
            fixture_pull("gadgets", 3, "g-1", "2026-11-15T12:00:00Z"),
            fixture_pull("gadgets", 4, "g-2", "2026-11-15T12:00:00Z"),
        ];
        std::fs::write(path, json!({ "viewer": "me", "pulls": pulls }).to_string())
            .expect("fixtures");
    }

    fn calls(log: &std::path::Path) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
            let route: Value = serde_json::from_str(line).expect("logged request");
            let route = route["route"].as_str().unwrap_or_default();
            let kind = route.split('/').take(1).collect::<String>();
            *counts.entry(kind).or_default() += 1;
        }
        counts
    }

    /// Thirty simulated minutes of a connected app: the PR panel asks for `pr.status` every 5 s,
    /// the links bar for previews every 30 s, the changes tab for thread counts every 5 s.
    #[tokio::test(start_paused = true)]
    async fn polling_github_stays_within_its_budget() {
        let home = tempfile::tempdir().expect("home");
        let (server, fixtures, log) = fixture_server(home.path()).await;
        let _client = server.connect_test_client();
        spawn(Arc::clone(&server));
        server.github.wake_poll();
        let selected = server
            .store
            .workspace("ws-w-1")
            .expect("store")
            .expect("workspace");

        let app = |server: Arc<Server>, minutes: u64, threads: bool| {
            let selected = selected.clone();
            async move {
                let workspace = json!({ "workspace": "ws-w-1" });
                for tick in 0..minutes * 12 {
                    server
                        .github_dispatch("pr.status", workspace.clone())
                        .await
                        .expect("pr.status");
                    server.note_selected(&selected).await;
                    if tick % 6 == 0 {
                        server
                            .github_dispatch("pr.previews", workspace.clone())
                            .await
                            .expect("pr.previews");
                    }
                    if threads {
                        server
                            .review_dispatch(
                                "review.threads",
                                json!({ "workspace": "ws-w-1", "unresolved": true }),
                            )
                            .await
                            .expect("review.threads");
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        app(Arc::clone(&server), 10, false).await;
        let first = calls(&log);
        assert_eq!(
            first.get("graphql PrReviewThreads"),
            None,
            "nothing polls review threads: {first:?}"
        );

        app(Arc::clone(&server), 10, true).await;
        write_fixtures(&fixtures, "2026-11-15T12:30:00Z");
        app(Arc::clone(&server), 10, true).await;
        let counts = calls(&log);

        let repos = 2;
        let minutes = 30;
        let polls = (minutes * 60 / PR_POLL.as_secs()) as usize + 1;
        let preview_reads = (minutes * 60 / PREVIEW_TTL.as_secs()) as usize + 1;
        let at_most = |route: &str, limit: usize| {
            let made = counts.get(route).copied().unwrap_or_default();
            assert!(
                made <= limit,
                "{made} {route} calls, at most {limit}: {counts:?}"
            );
        };
        at_most("graphql PrSnapshot", repos * polls);
        at_most("graphql PrPreviews", preview_reads);
        at_most("graphql RepoMergeMetadata", 1);
        at_most("GET repos", polls);
        assert_eq!(
            counts.get("graphql PrReviewThreads"),
            Some(&2),
            "threads are read once, then again only after the PR's updated_at moved: {counts:?}"
        );
        let total: usize = counts.values().sum();
        assert!(
            total <= repos * polls * 3,
            "{total} GitHub calls: {counts:?}"
        );
    }

    fn single_pr_queries(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["route"] == "graphql PrSnapshot")
            .filter(|line| line["variables"].get("b1").is_none())
            .filter_map(|line| line["variables"]["b0"].as_str().map(str::to_owned))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn selecting_a_stale_workspace_refreshes_only_its_pull_request_once() {
        let home = tempfile::tempdir().expect("home");
        let (server, _fixtures, log) = fixture_server(home.path()).await;
        let selected = server
            .store
            .workspace("ws-w-1")
            .expect("store")
            .expect("workspace");
        let settle = || tokio::time::sleep(Duration::from_secs(1));

        server.note_selected(&selected).await;
        server.note_selected(&selected).await;
        settle().await;
        assert_eq!(
            single_pr_queries(&log),
            ["w-1"],
            "never fetched: one query: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );

        server.note_selected(&selected).await;
        settle().await;
        assert_eq!(single_pr_queries(&log).len(), 1, "fresh: no query");

        tokio::time::sleep(crate::pr_refresh::SELECTED_STALE).await;
        server.note_selected(&selected).await;
        server.note_selected(&selected).await;
        settle().await;
        assert_eq!(
            single_pr_queries(&log),
            ["w-1", "w-1"],
            "stale again: one more"
        );
        assert!(
            server
                .github
                .fetched_at(&selected.project_id, "w-1")
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        );
    }

    /// Projects widgets and gadgets with two workspaces each, against fixture GitHub.
    async fn fixture_server(
        home: &std::path::Path,
    ) -> (Arc<Server>, std::path::PathBuf, std::path::PathBuf) {
        let fixtures = home.join("gh.json");
        write_fixtures(&fixtures, "2026-11-15T12:00:00Z");
        let gh = FixtureGh::new(fixtures.clone());
        let log = gh.log_path();
        let (server, _listener) = Server::bind_with(
            crate::paths::ServerPaths {
                home: home.join("server"),
            },
            GitHubService::new(ServerGh::Fixture(gh)),
        )
        .await
        .expect("server");
        for repo in ["widgets", "gadgets"] {
            let root = home.join(repo);
            let project = server
                .store
                .add_project(
                    repo,
                    &root,
                    Some(&format!("github.com/acme/{repo}")),
                    Some("main"),
                )
                .expect("project");
            let prefix = &repo[..1];
            for index in 1..=2 {
                let branch = format!("{prefix}-{index}");
                let workspace = Workspace {
                    id: format!("ws-{branch}"),
                    project_id: project.id.clone(),
                    name: branch.clone(),
                    path: root.join(&branch),
                    branch: Some(branch),
                    base_ref: Some("main".into()),
                    status: None,
                    title: None,
                    issue: None,
                    pull_request: None,
                    created_at_ms: 0,
                    pinned: false,
                    manual_order: None,
                    activity_at_ms: None,
                    discovery: crate::state::Discovery::default(),
                };
                server
                    .store
                    .insert_workspace(&workspace)
                    .expect("workspace");
            }
        }
        (server, fixtures, log)
    }
}
