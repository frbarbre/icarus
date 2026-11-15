//! Review threads with everything the review UI acts on: database and node ids, side, subject, reactions and permissions.

use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::raw::Nodes;
use crate::review::Reaction;
use crate::runner::Gh;
use icarus_rules::facts::RepoRef;
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_PAGES: usize = 20;

pub const PR_REVIEW_THREADS_QUERY: &str =
    "query PrReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 50, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { id isResolved isOutdated path line startLine originalLine originalStartLine
          diffSide startDiffSide subjectType viewerCanResolve viewerCanUnresolve viewerCanReply
          comments(first: 100) { nodes { id databaseId body url createdAt lastEditedAt
            viewerCanUpdate viewerCanDelete
            author { __typename login avatarUrl(size: 48) }
            reactionGroups { content viewerHasReacted
              reactors(first: 3) { totalCount nodes { ... on Actor { login } } } } } } }
      }
    }
  }
}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReactionSummary {
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket` or `eyes`.
    pub content: String,
    pub count: u64,
    pub viewer_has_reacted: bool,
    /// Up to three logins, for "alice, bob and 2 others reacted".
    pub reactors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrComment {
    pub id: String,
    pub database_id: Option<u64>,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    pub is_bot: bool,
    pub body: String,
    pub url: Option<String>,
    pub created_at: Option<String>,
    pub last_edited_at: Option<String>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    pub reactions: Vec<ReactionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrThread {
    pub id: String,
    pub path: String,
    /// `RIGHT` (the new file) or `LEFT` (the old file).
    pub side: String,
    pub start_side: Option<String>,
    /// `line` or `file`.
    pub subject: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub viewer_can_reply: bool,
    pub comments: Vec<PrComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAuthor {
    #[serde(default, rename = "__typename")]
    typename: Option<String>,
    login: String,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct RawLogin {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReactors {
    #[serde(default)]
    total_count: u64,
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<RawLogin>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReactionGroup {
    content: String,
    #[serde(default)]
    viewer_has_reacted: bool,
    #[serde(default)]
    reactors: Option<RawReactors>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    id: String,
    #[serde(default)]
    database_id: Option<u64>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    last_edited_at: Option<String>,
    #[serde(default)]
    viewer_can_update: bool,
    #[serde(default)]
    viewer_can_delete: bool,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    reaction_groups: Option<Vec<RawReactionGroup>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    #[serde(default)]
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    original_line: Option<u32>,
    #[serde(default)]
    original_start_line: Option<u32>,
    #[serde(default)]
    diff_side: Option<String>,
    #[serde(default)]
    start_diff_side: Option<String>,
    #[serde(default)]
    subject_type: Option<String>,
    #[serde(default)]
    viewer_can_resolve: bool,
    #[serde(default)]
    viewer_can_unresolve: bool,
    #[serde(default)]
    viewer_can_reply: bool,
    #[serde(default)]
    comments: Option<Nodes<RawComment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThreads {
    page_info: Option<PageInfo>,
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<RawThread>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPull {
    review_threads: RawThreads,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    pull_request: Option<RawPull>,
}

#[derive(Deserialize)]
struct Data {
    repository: Option<RawRepository>,
}

fn reactions(groups: Option<Vec<RawReactionGroup>>) -> Vec<ReactionSummary> {
    let mut out: Vec<(usize, ReactionSummary)> = groups
        .into_iter()
        .flatten()
        .filter_map(|group| {
            let reaction = Reaction::parse(&group.content)?;
            let reactors = group.reactors.unwrap_or(RawReactors {
                total_count: 0,
                nodes: Vec::new(),
            });
            (reactors.total_count > 0).then(|| {
                (
                    reaction as usize,
                    ReactionSummary {
                        content: reaction.short().to_owned(),
                        count: reactors.total_count,
                        viewer_has_reacted: group.viewer_has_reacted,
                        reactors: reactors
                            .nodes
                            .into_iter()
                            .flatten()
                            .filter_map(|n| n.login)
                            .collect(),
                    },
                )
            })
        })
        .collect();
    out.sort_by_key(|(order, _)| *order);
    out.into_iter().map(|(_, summary)| summary).collect()
}

impl From<RawThread> for PrThread {
    fn from(raw: RawThread) -> Self {
        let subject = match raw.subject_type.as_deref() {
            Some("FILE") => "file",
            _ => "line",
        };
        Self {
            id: raw.id,
            path: raw.path,
            side: raw.diff_side.unwrap_or_else(|| "RIGHT".to_owned()),
            start_side: raw.start_diff_side,
            subject: subject.to_owned(),
            line: raw.line,
            start_line: raw.start_line,
            original_line: raw.original_line,
            original_start_line: raw.original_start_line,
            is_resolved: raw.is_resolved,
            is_outdated: raw.is_outdated,
            viewer_can_resolve: raw.viewer_can_resolve,
            viewer_can_unresolve: raw.viewer_can_unresolve,
            viewer_can_reply: raw.viewer_can_reply,
            comments: raw
                .comments
                .into_iter()
                .flat_map(Nodes::items)
                .map(|c| {
                    let is_bot = c.author.as_ref().is_some_and(|a| {
                        a.typename.as_deref() == Some("Bot") || a.login.ends_with("[bot]")
                    });
                    let (author, author_avatar_url) = match c.author {
                        Some(a) => (Some(a.login), a.avatar_url),
                        None => (None, None),
                    };
                    PrComment {
                        id: c.id,
                        database_id: c.database_id,
                        author,
                        author_avatar_url,
                        is_bot,
                        body: c.body,
                        url: c.url,
                        created_at: c.created_at,
                        last_edited_at: c.last_edited_at,
                        viewer_can_update: c.viewer_can_update,
                        viewer_can_delete: c.viewer_can_delete,
                        reactions: reactions(c.reaction_groups),
                    }
                })
                .collect(),
        }
    }
}

impl<G: Gh> GitHub<G> {
    /// Every review thread of a pull request, paginated.
    pub async fn pr_review_threads(&self, repo: &RepoRef, number: u64) -> Result<Vec<PrThread>> {
        let mut threads = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let data: Data = self
                .graphql_data(
                    PR_REVIEW_THREADS_QUERY,
                    json!({ "owner": repo.owner, "name": repo.name, "number": number, "after": after }),
                )
                .await?;
            let page = data
                .repository
                .and_then(|r| r.pull_request)
                .ok_or_else(|| {
                    GitHubError::NotFound(format!("Could not find pull request #{number}."))
                })?
                .review_threads;
            threads.extend(page.nodes.into_iter().flatten().map(PrThread::from));
            match page.page_info {
                Some(PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                }) => after = Some(cursor),
                _ => break,
            }
        }
        Ok(threads)
    }
}
