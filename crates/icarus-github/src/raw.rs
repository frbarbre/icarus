use crate::snapshot::PrDetails;
use icarus_rules::facts::{
    AutoMergeRequest, Check, CheckConclusion, CheckRunStatus, CheckState, MergeMethod,
    MergeQueueEntry, MergeQueueEntryState, MergeStateStatus, Mergeable, PrState, PullRequest,
    RepoRef, RequestedReviewer, Review, ReviewDecision, ReviewState, StatusContextState,
    ViewerReview,
};
use serde::Deserialize;
use serde::de::IgnoredAny;

#[derive(Debug, Deserialize)]
pub(crate) struct Nodes<T> {
    #[serde(default = "Vec::new")]
    pub nodes: Vec<Option<T>>,
}

impl<T> Nodes<T> {
    pub fn items(self) -> impl Iterator<Item = T> {
        self.nodes.into_iter().flatten()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Login {
    pub login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Actor {
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Oid {
    pub oid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawRepo {
    pub name: String,
    pub owner: Login,
    #[serde(default)]
    pub auto_merge_allowed: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawQueueEntry {
    pub state: MergeQueueEntryState,
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub estimated_time_to_merge: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawAutoMerge {
    #[serde(default)]
    pub merge_method: Option<MergeMethod>,
    #[serde(default)]
    pub enabled_by: Option<Login>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub(crate) enum RawReviewer {
    User {
        login: String,
        #[serde(default, rename = "avatarUrl")]
        avatar_url: Option<String>,
    },
    Team {
        slug: String,
        #[serde(default, rename = "combinedSlug")]
        combined_slug: Option<String>,
        #[serde(default, rename = "avatarUrl")]
        avatar_url: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawReviewRequest {
    #[serde(default)]
    pub as_code_owner: bool,
    #[serde(default)]
    pub requested_reviewer: Option<RawReviewer>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawReview {
    pub state: ReviewState,
    #[serde(default)]
    pub author: Option<Actor>,
    #[serde(default)]
    pub commit: Option<Oid>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawViewerReview {
    pub state: String,
    #[serde(default)]
    pub commit: Option<Oid>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub(crate) enum RawContext {
    CheckRun {
        name: String,
        status: CheckRunStatus,
        #[serde(default)]
        conclusion: Option<CheckConclusion>,
        #[serde(default, rename = "detailsUrl")]
        details_url: Option<String>,
        #[serde(default, rename = "startedAt")]
        started_at: Option<String>,
        #[serde(default, rename = "completedAt")]
        completed_at: Option<String>,
    },
    StatusContext {
        context: String,
        state: StatusContextState,
        #[serde(default, rename = "targetUrl")]
        target_url: Option<String>,
        #[serde(default, rename = "createdAt")]
        created_at: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawRollup {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub contexts: Option<Nodes<RawContext>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawCommit {
    #[serde(default)]
    pub status_check_rollup: Option<RawRollup>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawCommitNode {
    pub commit: RawCommit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPr {
    pub id: String,
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    pub state: PrState,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub changed_files: u64,
    #[serde(default)]
    pub is_cross_repository: bool,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub base_ref_oid: Option<String>,
    #[serde(default)]
    pub head_ref: Option<IgnoredAny>,
    #[serde(default)]
    pub base_ref: Option<IgnoredAny>,
    #[serde(default)]
    pub mergeable: Option<Mergeable>,
    #[serde(default)]
    pub merge_state_status: Option<MergeStateStatus>,
    #[serde(default)]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub is_in_merge_queue: bool,
    #[serde(default)]
    pub merge_queue_entry: Option<RawQueueEntry>,
    #[serde(default)]
    pub auto_merge_request: Option<RawAutoMerge>,
    #[serde(default)]
    pub author: Option<Actor>,
    #[serde(default)]
    pub repository: Option<RawRepo>,
    #[serde(default)]
    pub review_requests: Option<Nodes<RawReviewRequest>>,
    #[serde(default)]
    pub latest_reviews: Option<Nodes<RawReview>>,
    #[serde(default)]
    pub latest_opinionated_reviews: Option<Nodes<RawReview>>,
    #[serde(default)]
    pub viewer_latest_review: Option<RawViewerReview>,
    #[serde(default)]
    pub commits: Option<Nodes<RawCommitNode>>,
}

pub(crate) fn rollup_state(state: &str) -> Option<CheckState> {
    match state {
        "SUCCESS" => Some(CheckState::Success),
        "FAILURE" | "ERROR" => Some(CheckState::Failure),
        "PENDING" | "EXPECTED" => Some(CheckState::Pending),
        "" => None,
        _ => Some(CheckState::Neutral),
    }
}

fn reviews(nodes: Option<Nodes<RawReview>>) -> Vec<Review> {
    nodes
        .into_iter()
        .flat_map(Nodes::items)
        .filter_map(|review| {
            let author = review.author?;
            Some(Review {
                login: author.login,
                state: review.state,
                commit_oid: review.commit.map(|commit| commit.oid),
                author_avatar_url: author.avatar_url,
            })
        })
        .collect()
}

impl RawPr {
    pub fn into_details(self, fallback_repo: &RepoRef) -> PrDetails {
        let repo = self
            .repository
            .as_ref()
            .map(|repo| RepoRef::new(repo.owner.login.clone(), repo.name.clone()))
            .unwrap_or_else(|| fallback_repo.clone());
        let auto_merge_allowed = self
            .repository
            .as_ref()
            .and_then(|repo| repo.auto_merge_allowed);
        let mut requested_reviewers = Vec::new();
        let mut code_owner_requests = Vec::new();
        for request in self.review_requests.into_iter().flat_map(Nodes::items) {
            let reviewer = match request.requested_reviewer {
                Some(RawReviewer::User { login, avatar_url }) => {
                    RequestedReviewer::user(login).with_avatar_url(avatar_url)
                }
                Some(RawReviewer::Team {
                    slug,
                    combined_slug,
                    avatar_url,
                }) => RequestedReviewer::team(combined_slug.unwrap_or(slug))
                    .with_avatar_url(avatar_url),
                Some(RawReviewer::Other) | None => continue,
            };
            if request.as_code_owner {
                code_owner_requests.push(reviewer.clone());
            }
            requested_reviewers.push(reviewer);
        }
        let rollup = self
            .commits
            .and_then(|commits| commits.items().last())
            .and_then(|node| node.commit.status_check_rollup);
        let checks_rollup = rollup
            .as_ref()
            .and_then(|rollup| rollup.state.as_deref())
            .and_then(rollup_state);
        let checks = rollup
            .and_then(|rollup| rollup.contexts)
            .into_iter()
            .flat_map(Nodes::items)
            .filter_map(|context| match context {
                RawContext::CheckRun {
                    name,
                    status,
                    conclusion,
                    details_url,
                    started_at,
                    completed_at,
                } => Some(
                    Check::run(name, status, conclusion, details_url.as_deref())
                        .with_times(started_at, completed_at),
                ),
                RawContext::StatusContext {
                    context,
                    state,
                    target_url,
                    created_at,
                } => Some(
                    Check::status(context, state, target_url.as_deref())
                        .with_times(created_at, None),
                ),
                RawContext::Other => None,
            })
            .collect();
        let (author, author_avatar_url) = self.author.map_or((None, None), |author| {
            (Some(author.login), author.avatar_url)
        });
        let pr = PullRequest {
            repo,
            number: self.number,
            title: self.title,
            url: self.url,
            author,
            author_avatar_url,
            state: self.state,
            is_draft: self.is_draft,
            head_ref_name: self.head_ref_name,
            base_ref_name: self.base_ref_name,
            head_ref_oid: self.head_ref_oid,
            base_ref_oid: self.base_ref_oid,
            mergeable: self.mergeable,
            merge_state_status: self.merge_state_status,
            review_decision: self.review_decision,
            requested_reviewers,
            latest_reviews: reviews(self.latest_reviews),
            latest_opinionated_reviews: self
                .latest_opinionated_reviews
                .map(|nodes| reviews(Some(nodes))),
            viewer_latest_review: self.viewer_latest_review.map(|review| ViewerReview {
                state: review.state,
                commit_oid: review.commit.map(|commit| commit.oid),
            }),
            checks,
            checks_rollup,
            is_in_merge_queue: self.is_in_merge_queue,
            merge_queue_entry: self.merge_queue_entry.map(|entry| MergeQueueEntry {
                state: entry.state,
                position: entry.position,
                eta_seconds: entry.estimated_time_to_merge,
            }),
            merge_queue_required: None,
            auto_merge: self.auto_merge_request.map(|request| AutoMergeRequest {
                method: request.merge_method,
                enabled_by: request.enabled_by.map(|login| login.login),
            }),
            auto_merge_allowed,
            stack: None,
            updated_at: self.updated_at.clone(),
        };
        PrDetails {
            node_id: self.id,
            pr,
            updated_at: self.updated_at,
            additions: self.additions,
            deletions: self.deletions,
            changed_files: self.changed_files,
            is_cross_repository: self.is_cross_repository,
            head_ref_deleted: self.head_ref.is_none(),
            base_ref_deleted: self.base_ref.is_none(),
            code_owner_requests,
        }
    }
}
