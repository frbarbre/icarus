use crate::client::{GitHub, decode};
use crate::error::{GitHubError, Result};
use crate::raw::{Login, Nodes, RawPr};
use crate::runner::Gh;
use icarus_rules::facts::{PullRequest, RepoRef, RequestedReviewer, Snapshot};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const MAX_TARGETS_PER_QUERY: usize = 50;
pub const INBOX_PAGE_SIZE: usize = 30;
const MAX_TEAM_PAGES: usize = 20;

pub const PR_FRAGMENT: &str = "fragment SnapshotPr on PullRequest {
  id number title url state isDraft updatedAt additions deletions changedFiles isCrossRepository
  headRefName baseRefName headRefOid baseRefOid
  headRef { name } baseRef { name }
  mergeable mergeStateStatus reviewDecision isInMergeQueue
  mergeQueueEntry { state position estimatedTimeToMerge }
  autoMergeRequest { mergeMethod enabledBy { login } }
  author { login avatarUrl(size: 64) }
  repository { name owner { login } autoMergeAllowed }
  reviewRequests(first: 30) { nodes { asCodeOwner requestedReviewer { __typename
    ... on User { login avatarUrl(size: 64) } ... on Team { slug combinedSlug avatarUrl(size: 64) } } } }
  latestReviews(first: 30) { nodes { state author { login avatarUrl(size: 64) } commit { oid } } }
  latestOpinionatedReviews(first: 30) { nodes { state author { login avatarUrl(size: 64) } commit { oid } } }
  viewerLatestReview { state commit { oid } }
  commits(last: 1) { nodes { commit { oid statusCheckRollup { state
    contexts(first: 100) { nodes { __typename
      ... on CheckRun { name status conclusion detailsUrl startedAt completedAt }
      ... on StatusContext { context state targetUrl createdAt } } } } } } }
}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrTarget {
    Number(u64),
    Branch(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrDetails {
    pub node_id: String,
    pub pr: PullRequest,
    pub updated_at: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub is_cross_repository: bool,
    pub head_ref_deleted: bool,
    pub base_ref_deleted: bool,
    pub code_owner_requests: Vec<RequestedReviewer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphQlRateLimit {
    pub cost: u64,
    pub remaining: u64,
    pub limit: u64,
    pub reset_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEntry {
    pub target: PrTarget,
    pub pull: Option<PrDetails>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrSnapshot {
    pub viewer_login: Option<String>,
    pub entries: Vec<SnapshotEntry>,
    pub errors: Vec<String>,
    pub rate_limit: Option<GraphQlRateLimit>,
}

impl PrSnapshot {
    pub fn pulls(&self) -> impl Iterator<Item = &PrDetails> {
        self.entries.iter().filter_map(|entry| entry.pull.as_ref())
    }

    pub fn missing(&self) -> impl Iterator<Item = &PrTarget> {
        self.entries
            .iter()
            .filter(|entry| entry.pull.is_none())
            .map(|entry| &entry.target)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewInbox {
    pub viewer_login: Option<String>,
    pub pulls: Vec<PrDetails>,
}

pub fn snapshot_query(targets: &[PrTarget]) -> String {
    let mut params = vec!["$owner: String!".to_owned(), "$name: String!".to_owned()];
    let mut selections = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        match target {
            PrTarget::Number(_) => {
                params.push(format!("$n{index}: Int!"));
                selections.push(format!(
                    "t{index}: pullRequest(number: $n{index}) {{ ...SnapshotPr }}"
                ));
            }
            PrTarget::Branch(_) => {
                params.push(format!("$b{index}: String!"));
                selections.push(format!(
                    "t{index}: pullRequests(headRefName: $b{index}, first: 1, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{ nodes {{ ...SnapshotPr }} }}"
                ));
            }
        }
    }
    format!(
        "query PrSnapshot({}) {{\n  rateLimit {{ cost remaining limit resetAt }}\n  viewer {{ login }}\n  repository(owner: $owner, name: $name) {{\n    {}\n  }}\n}}\n{PR_FRAGMENT}",
        params.join(", "),
        selections.join("\n    ")
    )
}

fn snapshot_variables(repo: &RepoRef, targets: &[PrTarget]) -> Value {
    let mut variables = Map::new();
    variables.insert("owner".to_owned(), json!(repo.owner));
    variables.insert("name".to_owned(), json!(repo.name));
    for (index, target) in targets.iter().enumerate() {
        match target {
            PrTarget::Number(number) => variables.insert(format!("n{index}"), json!(number)),
            PrTarget::Branch(branch) => variables.insert(format!("b{index}"), json!(branch)),
        };
    }
    Value::Object(variables)
}

#[derive(Deserialize)]
struct Viewer {
    viewer: Option<Login>,
}

fn decode_pr(value: Value, repo: &RepoRef, errors: &mut Vec<String>) -> Option<PrDetails> {
    if value.is_null() {
        return None;
    }
    match decode::<RawPr>(value) {
        Ok(raw) => Some(raw.into_details(repo)),
        Err(error) => {
            errors.push(error.to_string());
            None
        }
    }
}

fn first_node(value: Value) -> Value {
    match value {
        Value::Object(mut object) if object.contains_key("nodes") => match object.remove("nodes") {
            Some(Value::Array(nodes)) => nodes.into_iter().next().unwrap_or(Value::Null),
            _ => Value::Null,
        },
        other => other,
    }
}

pub fn team_key(org: &str, slug: &str) -> String {
    format!("{org}/{slug}")
}

pub fn parse_team(team: &str) -> Result<(&str, &str)> {
    team.split_once('/')
        .filter(|(org, slug)| !org.is_empty() && !slug.is_empty() && !slug.contains('/'))
        .ok_or_else(|| GitHubError::Invalid(format!("Team must look like org/team: {team}")))
}

pub fn build_snapshot(
    viewer_login: Option<String>,
    linked: &[PrDetails],
    inbox: &[PrDetails],
    team_members: BTreeMap<String, Vec<String>>,
) -> Snapshot {
    Snapshot {
        viewer_login,
        linked: linked.iter().map(|details| details.pr.clone()).collect(),
        review_requested: inbox.iter().map(|details| details.pr.clone()).collect(),
        team_members,
    }
}

impl<G: Gh> GitHub<G> {
    pub async fn pr_snapshot(&self, repo: &RepoRef, targets: &[PrTarget]) -> Result<PrSnapshot> {
        let mut snapshot = PrSnapshot::default();
        for chunk in targets.chunks(MAX_TARGETS_PER_QUERY) {
            let response = self
                .graphql(&snapshot_query(chunk), snapshot_variables(repo, chunk))
                .await?;
            for error in &response.errors {
                if error.kind.as_deref() != Some("NOT_FOUND") {
                    snapshot.errors.push(error.message.clone());
                }
            }
            let Value::Object(mut data) = response.data else {
                return Err(GitHubError::Malformed(
                    "snapshot data is not an object".to_owned(),
                ));
            };
            if let Some(viewer) = data.remove("viewer") {
                snapshot.viewer_login = decode::<Option<Login>>(viewer)?.map(|login| login.login);
            }
            if let Some(limit) = data.remove("rateLimit").filter(|value| !value.is_null()) {
                snapshot.rate_limit = Some(decode(limit)?);
            }
            let mut repository = match data.remove("repository") {
                Some(Value::Object(repository)) => repository,
                Some(Value::Null) | None => {
                    return Err(GitHubError::NotFound(format!(
                        "Could not find repository {}.",
                        repo.slug()
                    )));
                }
                Some(_) => {
                    return Err(GitHubError::Malformed(
                        "repository is not an object".to_owned(),
                    ));
                }
            };
            for (index, target) in chunk.iter().enumerate() {
                let value = repository
                    .remove(&format!("t{index}"))
                    .unwrap_or(Value::Null);
                let value = match target {
                    PrTarget::Number(_) => value,
                    PrTarget::Branch(_) => first_node(value),
                };
                snapshot.entries.push(SnapshotEntry {
                    target: target.clone(),
                    pull: decode_pr(value, repo, &mut snapshot.errors),
                });
            }
        }
        Ok(snapshot)
    }

    pub async fn pull_request(&self, repo: &RepoRef, number: u64) -> Result<PrDetails> {
        self.pr_snapshot(repo, &[PrTarget::Number(number)])
            .await?
            .entries
            .into_iter()
            .find_map(|entry| entry.pull)
            .ok_or_else(|| GitHubError::NotFound(format!("Could not find pull request #{number}.")))
    }

    pub async fn review_inbox(&self, repos: &[RepoRef]) -> Result<ReviewInbox> {
        if repos.is_empty() {
            return Ok(ReviewInbox::default());
        }
        let mut search = "is:pr is:open review-requested:@me archived:false".to_owned();
        for repo in repos {
            search.push_str(&format!(" repo:{}", repo.slug()));
        }
        let query = format!(
            "query ReviewInbox($q: String!, $first: Int!) {{\n  viewer {{ login }}\n  search(query: $q, type: ISSUE, first: $first) {{ nodes {{ __typename ...SnapshotPr }} }}\n}}\n{PR_FRAGMENT}"
        );
        #[derive(Deserialize)]
        struct Inbox {
            viewer: Option<Login>,
            search: Nodes<Value>,
        }
        let inbox: Inbox = self
            .graphql_data(&query, json!({"q": search, "first": INBOX_PAGE_SIZE}))
            .await?;
        let fallback = RepoRef::default();
        let mut pulls = Vec::new();
        for node in inbox.search.items() {
            if node.get("__typename").and_then(Value::as_str) != Some("PullRequest") {
                continue;
            }
            pulls.push(decode::<RawPr>(node)?.into_details(&fallback));
        }
        Ok(ReviewInbox {
            viewer_login: inbox.viewer.map(|login| login.login),
            pulls,
        })
    }

    pub async fn viewer_login(&self) -> Result<String> {
        let viewer: Viewer = self
            .graphql_data("query Viewer { viewer { login } }", json!({}))
            .await?;
        viewer
            .viewer
            .map(|login| login.login)
            .ok_or_else(|| GitHubError::Malformed("viewer is missing".to_owned()))
    }

    pub async fn team_members(&self, team: &str) -> Result<Vec<String>> {
        let (org, slug) = parse_team(team)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
            end_cursor: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Members {
            page_info: PageInfo,
            #[serde(default = "Vec::new")]
            nodes: Vec<Option<Login>>,
        }
        #[derive(Deserialize)]
        struct Team {
            members: Members,
        }
        #[derive(Deserialize)]
        struct Org {
            team: Option<Team>,
        }
        #[derive(Deserialize)]
        struct Data {
            organization: Option<Org>,
        }
        const QUERY: &str = "query TeamMembers($org: String!, $slug: String!, $after: String) {
  organization(login: $org) { team(slug: $slug) {
    members(first: 100, after: $after, membership: ALL) { pageInfo { hasNextPage endCursor } nodes { login } } } }
}";
        let mut logins = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_TEAM_PAGES {
            let data: Data = self
                .graphql_data(QUERY, json!({"org": org, "slug": slug, "after": after}))
                .await?;
            let members = data
                .organization
                .and_then(|org| org.team)
                .ok_or_else(|| GitHubError::NotFound(format!("Could not find team {team}.")))?
                .members;
            logins.extend(members.nodes.into_iter().flatten().map(|login| login.login));
            match (
                members.page_info.has_next_page,
                members.page_info.end_cursor,
            ) {
                (true, Some(cursor)) => after = Some(cursor),
                _ => break,
            }
        }
        Ok(logins)
    }

    pub async fn team_roster(&self, teams: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
        let mut roster = BTreeMap::new();
        for team in teams {
            let (org, slug) = parse_team(team)?;
            roster.insert(team_key(org, slug), self.team_members(team).await?);
        }
        Ok(roster)
    }
}
