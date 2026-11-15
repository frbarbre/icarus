use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::raw::Nodes;
use crate::runner::Gh;
use icarus_rules::facts::RepoRef;
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_PAGES: usize = 10;

pub const REVIEW_THREADS_QUERY: &str =
    "query ReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 50, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { id isResolved isOutdated path line originalLine startLine
          comments(first: 50) { nodes { id body url createdAt author { __typename login } } } }
      }
    }
  }
}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThreadComment {
    pub id: String,
    pub author: Option<String>,
    pub is_bot: bool,
    pub body: String,
    pub url: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewThread {
    pub id: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
}

impl ReviewThread {
    pub fn url(&self) -> Option<&str> {
        self.comments.first().and_then(|c| c.url.as_deref())
    }

    pub fn has_human_comment(&self) -> bool {
        self.comments.iter().any(|c| !c.is_bot)
    }
}

#[derive(Deserialize)]
struct RawAuthor {
    #[serde(default, rename = "__typename")]
    typename: Option<String>,
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    author: Option<RawAuthor>,
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
    path: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    original_line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
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

impl From<RawThread> for ReviewThread {
    fn from(raw: RawThread) -> Self {
        Self {
            id: raw.id,
            path: raw.path,
            line: raw.line.or(raw.original_line),
            start_line: raw.start_line,
            is_resolved: raw.is_resolved,
            is_outdated: raw.is_outdated,
            comments: raw
                .comments
                .into_iter()
                .flat_map(Nodes::items)
                .map(|comment| {
                    let is_bot = comment
                        .author
                        .as_ref()
                        .is_some_and(|a| a.typename.as_deref() == Some("Bot"));
                    ThreadComment {
                        id: comment.id,
                        author: comment.author.map(|a| a.login),
                        is_bot,
                        body: comment.body,
                        url: comment.url,
                        created_at: comment.created_at,
                    }
                })
                .collect(),
        }
    }
}

impl<G: Gh> GitHub<G> {
    pub async fn review_threads(&self, repo: &RepoRef, number: u64) -> Result<Vec<ReviewThread>> {
        let mut threads = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let data: Data = self
                .graphql_data(
                    REVIEW_THREADS_QUERY,
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
            threads.extend(page.nodes.into_iter().flatten().map(ReviewThread::from));
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
