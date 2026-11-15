use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::runner::{Gh, GhRequest, Method};
use crate::stack::is_git_oid;
use icarus_rules::facts::{RepoRef, RequestedReviewer};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    /// `comment`, `approve` or `request-changes` (also GitHub's `COMMENT`/`APPROVE`/`REQUEST_CHANGES`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "comment" | "only-comment" => Some(Self::Comment),
            "approve" => Some(Self::Approve),
            "request-changes" => Some(Self::RequestChanges),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Approve => "approve",
            Self::RequestChanges => "request-changes",
        }
    }

    pub fn event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "LEFT" | "OLD" | "BASE" => Some(Self::Left),
            "RIGHT" | "NEW" | "HEAD" => Some(Self::Right),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTarget {
    Line {
        line: u32,
        side: Side,
        start: Option<(u32, Side)>,
    },
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftComment {
    pub path: String,
    pub body: String,
    pub target: CommentTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SubmittedReview {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentRef {
    ReviewComment { node_id: String },
    ReviewSummary { node_id: String },
    IssueComment { id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Confused,
    Heart,
    Hooray,
    Rocket,
    Eyes,
}

impl Reaction {
    pub const ALL: [Self; 8] = [
        Self::ThumbsUp,
        Self::ThumbsDown,
        Self::Laugh,
        Self::Confused,
        Self::Heart,
        Self::Hooray,
        Self::Rocket,
        Self::Eyes,
    ];

    /// Accepts GitHub's GraphQL names (`THUMBS_UP`) and REST names (`+1`), case-insensitively.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL.into_iter().find(|reaction| {
            reaction.content().eq_ignore_ascii_case(value)
                || reaction.short().eq_ignore_ascii_case(value)
        })
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::ThumbsUp => "+1",
            Self::ThumbsDown => "-1",
            Self::Laugh => "laugh",
            Self::Confused => "confused",
            Self::Heart => "heart",
            Self::Hooray => "hooray",
            Self::Rocket => "rocket",
            Self::Eyes => "eyes",
        }
    }

    pub fn content(self) -> &'static str {
        match self {
            Self::ThumbsUp => "THUMBS_UP",
            Self::ThumbsDown => "THUMBS_DOWN",
            Self::Laugh => "LAUGH",
            Self::Confused => "CONFUSED",
            Self::Heart => "HEART",
            Self::Hooray => "HOORAY",
            Self::Rocket => "ROCKET",
            Self::Eyes => "EYES",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PostedComment {
    pub id: u64,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

pub fn draft_thread(comment: &DraftComment) -> Value {
    let mut thread = Map::new();
    thread.insert("path".to_owned(), json!(comment.path));
    thread.insert("body".to_owned(), json!(comment.body));
    match comment.target {
        CommentTarget::Line { line, side, start } => {
            thread.insert("line".to_owned(), json!(line));
            thread.insert("side".to_owned(), json!(side.as_str()));
            if let Some((start_line, start_side)) =
                start.filter(|(start_line, _)| *start_line != line)
            {
                thread.insert("startLine".to_owned(), json!(start_line));
                thread.insert("startSide".to_owned(), json!(start_side.as_str()));
            }
        }
        CommentTarget::File => {
            thread.insert("subjectType".to_owned(), json!("FILE"));
        }
    }
    Value::Object(thread)
}

fn rest_comment(comment: &DraftComment, commit_id: &str) -> Value {
    let mut body = Map::new();
    body.insert("body".to_owned(), json!(comment.body));
    body.insert("commit_id".to_owned(), json!(commit_id));
    body.insert("path".to_owned(), json!(comment.path));
    match comment.target {
        CommentTarget::Line { line, side, start } => {
            body.insert("line".to_owned(), json!(line));
            body.insert("side".to_owned(), json!(side.as_str()));
            if let Some((start_line, start_side)) =
                start.filter(|(start_line, _)| *start_line != line)
            {
                body.insert("start_line".to_owned(), json!(start_line));
                body.insert("start_side".to_owned(), json!(start_side.as_str()));
            }
        }
        CommentTarget::File => {
            body.insert("subject_type".to_owned(), json!("file"));
        }
    }
    Value::Object(body)
}

fn split_reviewers(reviewers: &[RequestedReviewer]) -> Result<Value> {
    if reviewers.is_empty() {
        return Err(GitHubError::Invalid(
            "Enter at least one reviewer".to_owned(),
        ));
    }
    let mut users = Vec::new();
    let mut teams = Vec::new();
    for reviewer in reviewers {
        match reviewer {
            RequestedReviewer::User { login, .. } => users.push(login.clone()),
            RequestedReviewer::Team { slug, .. } => {
                let slug = slug
                    .rsplit_once('/')
                    .map_or(slug.as_str(), |(_, slug)| slug);
                teams.push(slug.to_owned());
            }
        }
    }
    Ok(json!({ "reviewers": users, "team_reviewers": teams }))
}

fn non_empty(body: &str) -> Result<&str> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        Err(GitHubError::Invalid(
            "A comment cannot be empty.".to_owned(),
        ))
    } else {
        Ok(trimmed)
    }
}

fn posted(comment: PostedComment) -> Result<PostedComment> {
    if comment.id == 0 {
        return Err(GitHubError::Malformed(
            "GitHub did not return the new comment.".to_owned(),
        ));
    }
    Ok(comment)
}

const SUBMIT_REVIEW: &str = "mutation SubmitReview($input: AddPullRequestReviewInput!) {
  addPullRequestReview(input: $input) { pullRequestReview { id state url } }
}";

const REPLY_IN_THREAD: &str = "mutation ReplyInThread($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $threadId, body: $body}) {
    comment { id databaseId url } }
}";

/// A reply posted through GraphQL, which names the thread it joined.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReply {
    pub id: String,
    #[serde(default)]
    pub database_id: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

const THREAD_RESOLVE: &str = "mutation ResolveThread($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) { thread { isResolved } }
}";

const THREAD_UNRESOLVE: &str = "mutation UnresolveThread($threadId: ID!) {
  unresolveReviewThread(input: {threadId: $threadId}) { thread { isResolved } }
}";

const UPDATE_REVIEW_COMMENT: &str = "mutation UpdateReviewComment($id: ID!, $body: String!) {
  updatePullRequestReviewComment(input: {pullRequestReviewCommentId: $id, body: $body}) {
    pullRequestReviewComment { body lastEditedAt } }
}";

const UPDATE_REVIEW: &str = "mutation UpdateReview($id: ID!, $body: String!) {
  updatePullRequestReview(input: {pullRequestReviewId: $id, body: $body}) { pullRequestReview { body lastEditedAt } }
}";

const DELETE_REVIEW_COMMENT: &str = "mutation DeleteReviewComment($id: ID!) {
  deletePullRequestReviewComment(input: {id: $id}) { clientMutationId }
}";

const ADD_REACTION: &str = "mutation AddReaction($subjectId: ID!, $content: ReactionContent!) {
  addReaction(input: {subjectId: $subjectId, content: $content}) { subject { id } }
}";

const REMOVE_REACTION: &str =
    "mutation RemoveReaction($subjectId: ID!, $content: ReactionContent!) {
  removeReaction(input: {subjectId: $subjectId, content: $content}) { subject { id } }
}";

impl<G: Gh> GitHub<G> {
    pub async fn request_reviewers(
        &self,
        repo: &RepoRef,
        number: u64,
        reviewers: &[RequestedReviewer],
    ) -> Result<()> {
        let body = split_reviewers(reviewers)?;
        self.rest(
            GhRequest::new(
                Method::Post,
                format!(
                    "repos/{}/{}/pulls/{number}/requested_reviewers",
                    repo.owner, repo.name
                ),
            )
            .json(body),
        )
        .await
        .map(drop)
    }

    pub async fn remove_reviewers(
        &self,
        repo: &RepoRef,
        number: u64,
        reviewers: &[RequestedReviewer],
    ) -> Result<()> {
        let body = split_reviewers(reviewers)?;
        self.rest(
            GhRequest::new(
                Method::Delete,
                format!(
                    "repos/{}/{}/pulls/{number}/requested_reviewers",
                    repo.owner, repo.name
                ),
            )
            .json(body),
        )
        .await
        .map(drop)
    }

    pub async fn submit_review(
        &self,
        pull_request_id: &str,
        verdict: Verdict,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<SubmittedReview> {
        let mut input = Map::new();
        input.insert("pullRequestId".to_owned(), json!(pull_request_id));
        input.insert("event".to_owned(), json!(verdict.event()));
        input.insert("body".to_owned(), json!(body));
        if !comments.is_empty() {
            input.insert(
                "threads".to_owned(),
                Value::Array(comments.iter().map(draft_thread).collect()),
            );
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            pull_request_review: Option<SubmittedReview>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            add_pull_request_review: Option<Payload>,
        }
        let data: Data = self
            .graphql_data(SUBMIT_REVIEW, json!({ "input": input }))
            .await?;
        data.add_pull_request_review
            .and_then(|payload| payload.pull_request_review)
            .ok_or_else(|| GitHubError::Malformed("GitHub returned no review".to_owned()))
    }

    pub async fn post_review_comment(
        &self,
        repo: &RepoRef,
        number: u64,
        commit_id: &str,
        comment: &DraftComment,
    ) -> Result<PostedComment> {
        if !is_git_oid(commit_id) {
            return Err(GitHubError::Invalid(format!(
                "Not a commit id: {commit_id}"
            )));
        }
        let comment: PostedComment = self
            .rest_json(
                GhRequest::new(
                    Method::Post,
                    format!("repos/{}/{}/pulls/{number}/comments", repo.owner, repo.name),
                )
                .json(rest_comment(comment, commit_id)),
            )
            .await?;
        posted(comment)
    }

    pub async fn reply_to_thread(
        &self,
        repo: &RepoRef,
        number: u64,
        root_comment_id: u64,
        body: &str,
    ) -> Result<PostedComment> {
        let body = non_empty(body)?;
        let comment: PostedComment = self
            .rest_json(
                GhRequest::new(
                    Method::Post,
                    format!(
                        "repos/{}/{}/pulls/{number}/comments/{root_comment_id}/replies",
                        repo.owner, repo.name
                    ),
                )
                .json(json!({ "body": body })),
            )
            .await?;
        posted(comment)
    }

    pub async fn reply_in_thread(&self, thread_id: &str, body: &str) -> Result<ThreadReply> {
        let body = non_empty(body)?;
        #[derive(Deserialize)]
        struct Payload {
            comment: Option<ThreadReply>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            add_pull_request_review_thread_reply: Option<Payload>,
        }
        let data: Data = self
            .graphql_data(
                REPLY_IN_THREAD,
                json!({ "threadId": thread_id, "body": body }),
            )
            .await?;
        data.add_pull_request_review_thread_reply
            .and_then(|payload| payload.comment)
            .ok_or_else(|| {
                GitHubError::Malformed("GitHub did not return the new comment.".to_owned())
            })
    }

    pub async fn add_issue_comment(
        &self,
        repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<PostedComment> {
        let body = non_empty(body)?;
        let comment: PostedComment = self
            .rest_json(
                GhRequest::new(
                    Method::Post,
                    format!(
                        "repos/{}/{}/issues/{number}/comments",
                        repo.owner, repo.name
                    ),
                )
                .json(json!({ "body": body })),
            )
            .await?;
        posted(comment)
    }

    pub async fn set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<bool> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Thread {
            is_resolved: bool,
        }
        #[derive(Deserialize)]
        struct Payload {
            thread: Thread,
        }
        let query = if resolved {
            THREAD_RESOLVE
        } else {
            THREAD_UNRESOLVE
        };
        let data: Map<String, Value> = self
            .graphql_data(query, json!({ "threadId": thread_id }))
            .await?;
        let payload = data
            .into_iter()
            .next()
            .map(|(_, value)| value)
            .ok_or_else(|| GitHubError::Malformed("thread mutation returned nothing".to_owned()))?;
        let payload: Payload = crate::client::decode(payload)?;
        Ok(payload.thread.is_resolved)
    }

    pub async fn edit_comment(
        &self,
        repo: &RepoRef,
        target: &CommentRef,
        body: &str,
    ) -> Result<()> {
        let body = non_empty(body)?;
        match target {
            CommentRef::ReviewComment { node_id } => self
                .graphql_data::<Value>(
                    UPDATE_REVIEW_COMMENT,
                    json!({ "id": node_id, "body": body }),
                )
                .await
                .map(drop),
            CommentRef::ReviewSummary { node_id } => self
                .graphql_data::<Value>(UPDATE_REVIEW, json!({ "id": node_id, "body": body }))
                .await
                .map(drop),
            CommentRef::IssueComment { id } => self
                .rest(
                    GhRequest::new(
                        Method::Patch,
                        format!("repos/{}/{}/issues/comments/{id}", repo.owner, repo.name),
                    )
                    .json(json!({ "body": body })),
                )
                .await
                .map(drop),
        }
    }

    pub async fn delete_comment(&self, repo: &RepoRef, target: &CommentRef) -> Result<()> {
        match target {
            CommentRef::ReviewComment { node_id } => self
                .graphql_data::<Value>(DELETE_REVIEW_COMMENT, json!({ "id": node_id }))
                .await
                .map(drop),
            CommentRef::ReviewSummary { .. } => Err(GitHubError::Invalid(
                "GitHub cannot delete a submitted review.".to_owned(),
            )),
            CommentRef::IssueComment { id } => self
                .rest(GhRequest::new(
                    Method::Delete,
                    format!("repos/{}/{}/issues/comments/{id}", repo.owner, repo.name),
                ))
                .await
                .map(drop),
        }
    }

    pub async fn set_reaction(
        &self,
        subject_id: &str,
        reaction: Reaction,
        add: bool,
    ) -> Result<()> {
        let query = if add { ADD_REACTION } else { REMOVE_REACTION };
        self.graphql_data::<Value>(
            query,
            json!({ "subjectId": subject_id, "content": reaction.content() }),
        )
        .await
        .map(drop)
    }
}
