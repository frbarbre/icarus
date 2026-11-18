//! A pull request's conversation outside review threads: issue comments and submitted reviews' summaries.

use crate::client::GitHub;
use crate::error::{GitHubError, Result};
use crate::review_threads::{RawAuthor, RawReactionGroup, ReactionSummary, reactions};
use crate::runner::Gh;
use icarus_rules::facts::RepoRef;
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_PAGES: usize = 10;

// One query reads both lists with the viewer's reactions and permissions, like Orca's; a list
// whose last page arrived is skipped on the next request.
pub const PR_CONVERSATION_QUERY: &str =
    "query PrConversation($owner: String!, $name: String!, $number: Int!,
  $commentsAfter: String, $reviewsAfter: String, $withComments: Boolean!, $withReviews: Boolean!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      comments(first: 100, after: $commentsAfter) @include(if: $withComments) {
        pageInfo { hasNextPage endCursor }
        nodes { id databaseId body url createdAt viewerCanUpdate viewerCanDelete
          author { __typename login avatarUrl(size: 48) }
          reactionGroups { content viewerHasReacted
            reactors(first: 3) { totalCount nodes { ... on Actor { login } } } } }
      }
      reviews(first: 100, after: $reviewsAfter) @include(if: $withReviews) {
        pageInfo { hasNextPage endCursor }
        nodes { id databaseId state body url createdAt submittedAt viewerCanUpdate viewerCanDelete
          author { __typename login avatarUrl(size: 48) }
          reactionGroups { content viewerHasReacted
            reactors(first: 3) { totalCount nodes { ... on Actor { login } } } } }
      }
    }
  }
}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Comment,
    Review,
}

/// One entry of the conversation: an issue comment, or a review submitted with a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationComment {
    /// The GraphQL node id, which reactions and review summary edits address.
    pub id: String,
    /// The REST id, which issue comment edits and deletes address.
    pub database_id: Option<u64>,
    pub kind: ConversationKind,
    pub author: Option<String>,
    pub author_avatar_url: Option<String>,
    pub is_bot: bool,
    pub body: String,
    pub url: Option<String>,
    pub created_at: Option<String>,
    /// Reviews only: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED` or `PENDING`.
    pub review_state: Option<String>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    pub reactions: Vec<ReactionSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
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
    submitted_at: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    viewer_can_update: bool,
    #[serde(default)]
    viewer_can_delete: bool,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    reaction_groups: Option<Vec<RawReactionGroup>>,
}

impl RawEntry {
    fn into_comment(self, kind: ConversationKind) -> ConversationComment {
        let is_bot = self.author.as_ref().is_some_and(|author| {
            author.typename.as_deref() == Some("Bot") || author.login.ends_with("[bot]")
        });
        let (author, author_avatar_url) = match self.author {
            Some(author) => (Some(author.login), author.avatar_url),
            None => (None, None),
        };
        ConversationComment {
            id: self.id,
            database_id: self.database_id,
            kind,
            author,
            author_avatar_url,
            is_bot,
            body: self.body,
            url: self.url,
            created_at: self.submitted_at.or(self.created_at),
            review_state: match kind {
                ConversationKind::Review => self.state,
                ConversationKind::Comment => None,
            },
            viewer_can_update: self.viewer_can_update,
            viewer_can_delete: self.viewer_can_delete,
            reactions: reactions(self.reaction_groups),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPage {
    page_info: Option<PageInfo>,
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<RawEntry>>,
}

impl RawPage {
    fn next(&self) -> Option<String> {
        match &self.page_info {
            Some(PageInfo {
                has_next_page: true,
                end_cursor: Some(cursor),
            }) => Some(cursor.clone()),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct RawPull {
    #[serde(default)]
    comments: Option<RawPage>,
    #[serde(default)]
    reviews: Option<RawPage>,
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

impl<G: Gh> GitHub<G> {
    /// Issue comments and review summaries (reviews submitted without a body add nothing to read),
    /// oldest first.
    pub async fn pr_conversation(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ConversationComment>> {
        let mut entries = Vec::new();
        let (mut comments_after, mut reviews_after) = (None::<String>, None::<String>);
        let (mut with_comments, mut with_reviews) = (true, true);
        for _ in 0..MAX_PAGES {
            let data: Data = self
                .graphql_data(
                    PR_CONVERSATION_QUERY,
                    json!({
                        "owner": repo.owner, "name": repo.name, "number": number,
                        "commentsAfter": comments_after, "reviewsAfter": reviews_after,
                        "withComments": with_comments, "withReviews": with_reviews,
                    }),
                )
                .await?;
            let pull = data
                .repository
                .and_then(|r| r.pull_request)
                .ok_or_else(|| {
                    GitHubError::NotFound(format!("Could not find pull request #{number}."))
                })?;
            comments_after = pull.comments.as_ref().and_then(RawPage::next);
            reviews_after = pull.reviews.as_ref().and_then(RawPage::next);
            if let Some(page) = pull.comments {
                entries.extend(
                    page.nodes
                        .into_iter()
                        .flatten()
                        .map(|raw| raw.into_comment(ConversationKind::Comment)),
                );
            }
            if let Some(page) = pull.reviews {
                entries.extend(
                    page.nodes
                        .into_iter()
                        .flatten()
                        .filter(|raw| !raw.body.trim().is_empty())
                        .map(|raw| raw.into_comment(ConversationKind::Review)),
                );
            }
            with_comments = with_comments && comments_after.is_some();
            with_reviews = with_reviews && reviews_after.is_some();
            if !with_comments && !with_reviews {
                break;
            }
        }
        entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(entries)
    }
}
