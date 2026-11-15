//! What the Review section needs about a pull request: its id, the viewer's standing and review history, and its commits.

use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::raw::Nodes;
use crate::runner::Gh;
use icarus_rules::facts::RepoRef;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const REVIEW_CONTEXT_QUERY: &str =
    "query ReviewContext($owner: String!, $name: String!, $number: Int!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id number title url state isDraft headRefName headRefOid baseRefName viewerDidAuthor
      author { login }
      viewerLatestReviewRequest { __typename }
      viewerLatestReview { state commit { oid } }
      commits(last: 100) { nodes { commit { oid messageHeadline committedDate } } }
      reviews(last: 100) { nodes { state body submittedAt author { login } commit { oid }
        comments(first: 10) { nodes { replyTo { id } } } } }
    }
  }
}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrCommit {
    pub oid: String,
    pub headline: String,
    pub committed_at: Option<String>,
}

/// One of the viewer's submitted reviews, numbered 1… in submit order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewerReview {
    pub ordinal: usize,
    pub commit_oid: String,
    pub state: String,
    pub submitted_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewContext {
    pub pull_request_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub viewer_login: Option<String>,
    pub viewer_did_author: bool,
    pub author_login: Option<String>,
    pub viewer_latest_review_state: Option<String>,
    pub viewer_latest_review_commit: Option<String>,
    pub viewer_has_review_request: bool,
    pub commits: Vec<PrCommit>,
    pub viewer_reviews: Vec<ViewerReview>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawReview {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub author: Option<RawLogin>,
    #[serde(default)]
    pub commit: Option<RawOid>,
    #[serde(default)]
    pub comments: Option<RawReviewComments>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawReviewComments {
    #[serde(default)]
    pub nodes: Vec<Option<RawReviewComment>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawLogin {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawOid {
    pub oid: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawReviewComment {
    #[serde(default)]
    pub reply_to: Option<serde_json::Value>,
}

impl RawReview {
    // Why: GitHub records each thread reply as its own bodiless COMMENTED review.
    fn is_reply_only(&self) -> bool {
        let comments: Vec<&RawReviewComment> = self
            .comments
            .iter()
            .flat_map(|c| c.nodes.iter().flatten())
            .collect();
        self.state == "COMMENTED"
            && self.body.as_deref().unwrap_or_default().trim().is_empty()
            && !comments.is_empty()
            && comments
                .iter()
                .all(|c| c.reply_to.as_ref().is_some_and(|reply| !reply.is_null()))
    }
}

/// The viewer's reviews that count as "Review N": submitted, on a commit, and not reply-only.
pub fn number_viewer_reviews(reviews: &[RawReview], viewer: &str) -> Vec<ViewerReview> {
    let mut kept: Vec<ViewerReview> = reviews
        .iter()
        .filter(|r| {
            r.author
                .as_ref()
                .is_some_and(|a| a.login.eq_ignore_ascii_case(viewer))
        })
        .filter(|r| r.state != "PENDING" && !r.is_reply_only())
        .filter_map(|r| {
            Some(ViewerReview {
                ordinal: 0,
                commit_oid: r.commit.as_ref()?.oid.clone(),
                state: r.state.clone(),
                submitted_at: r.submitted_at.clone()?,
            })
        })
        .collect();
    kept.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
    for (index, review) in kept.iter_mut().enumerate() {
        review.ordinal = index + 1;
    }
    kept
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLatest {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    commit: Option<RawOid>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    oid: String,
    #[serde(default)]
    message_headline: String,
    #[serde(default)]
    committed_date: Option<String>,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPull {
    id: String,
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    viewer_did_author: bool,
    #[serde(default)]
    author: Option<RawLogin>,
    #[serde(default)]
    viewer_latest_review_request: Option<serde_json::Value>,
    #[serde(default)]
    viewer_latest_review: Option<RawLatest>,
    #[serde(default)]
    commits: Option<Nodes<RawCommitNode>>,
    #[serde(default)]
    reviews: Option<Nodes<RawReview>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    pull_request: Option<RawPull>,
}

#[derive(Deserialize)]
struct Data {
    #[serde(default)]
    viewer: Option<RawLogin>,
    repository: Option<RawRepository>,
}

impl<G: Gh> GitHub<G> {
    pub async fn review_context(&self, repo: &RepoRef, number: u64) -> Result<ReviewContext> {
        let data: Data = self
            .graphql_data(
                REVIEW_CONTEXT_QUERY,
                json!({ "owner": repo.owner, "name": repo.name, "number": number }),
            )
            .await?;
        let viewer_login = data.viewer.map(|v| v.login);
        let pull = data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or_else(|| {
                GitHubError::NotFound(format!("Could not find pull request #{number}."))
            })?;
        let reviews: Vec<RawReview> = pull.reviews.into_iter().flat_map(Nodes::items).collect();
        let viewer_reviews = viewer_login
            .as_deref()
            .map(|viewer| number_viewer_reviews(&reviews, viewer))
            .unwrap_or_default();
        let latest = pull.viewer_latest_review;
        Ok(ReviewContext {
            pull_request_id: pull.id,
            number: pull.number,
            title: pull.title,
            url: pull.url,
            state: pull.state,
            is_draft: pull.is_draft,
            head_ref_name: pull.head_ref_name,
            head_ref_oid: pull.head_ref_oid,
            base_ref_name: pull.base_ref_name,
            viewer_login,
            viewer_did_author: pull.viewer_did_author,
            author_login: pull.author.map(|a| a.login),
            viewer_latest_review_state: latest.as_ref().and_then(|l| l.state.clone()),
            viewer_latest_review_commit: latest.and_then(|l| l.commit).map(|c| c.oid),
            viewer_has_review_request: pull
                .viewer_latest_review_request
                .is_some_and(|r| !r.is_null()),
            commits: pull
                .commits
                .into_iter()
                .flat_map(Nodes::items)
                .map(|node| PrCommit {
                    oid: node.commit.oid,
                    headline: node.commit.message_headline,
                    committed_at: node.commit.committed_date,
                })
                .collect(),
            viewer_reviews,
        })
    }
}
