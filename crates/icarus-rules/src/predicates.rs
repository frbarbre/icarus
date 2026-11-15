use crate::facts::{PullRequest, ReviewState};
use std::collections::BTreeSet;

pub fn lowercase_login_set<'a>(logins: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
    logins
        .into_iter()
        .map(|login| login.to_lowercase())
        .collect()
}

// Why the pending check: GitHub keeps a CHANGES_REQUESTED review after a re-request answers it.
pub fn has_unanswered_change_request(pr: &PullRequest) -> bool {
    let pending: BTreeSet<String> = pr.pending_user_reviewers().into_iter().collect();
    pr.opinionated_or_latest().iter().any(|review| {
        review.state == ReviewState::ChangesRequested
            && !pending.contains(&review.login.to_lowercase())
    })
}

pub fn has_passing_named_check(pr: &PullRequest, check_name: &str) -> bool {
    let wanted = check_name.trim().to_lowercase();
    if wanted.is_empty() {
        return false;
    }
    pr.checks.iter().any(|check| {
        check.name.trim().to_lowercase() == wanted
            && check.state() == crate::facts::CheckState::Success
    })
}

pub fn awaits_only_team(pr: &PullRequest, team: &BTreeSet<String>) -> bool {
    if team.is_empty() {
        return false;
    }
    let pending = pr.pending_user_reviewers();
    !pending.is_empty() && pending.iter().all(|login| team.contains(login))
}

pub fn approved_by_team_and_everyone(pr: &PullRequest, team: &BTreeSet<String>) -> bool {
    if team.is_empty() || !pr.pending_user_reviewers().is_empty() {
        return false;
    }
    let author = pr.author.as_deref().map(str::to_lowercase);
    let verdicts: Vec<_> = pr
        .opinionated_or_latest()
        .iter()
        .filter(|review| Some(review.login.to_lowercase()) != author)
        .collect();
    !verdicts.is_empty()
        && verdicts
            .iter()
            .all(|review| review.state == ReviewState::Approved)
        && verdicts
            .iter()
            .any(|review| team.contains(&review.login.to_lowercase()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewerStanding {
    pub is_finished: bool,
    pub is_owed: bool,
    pub commit_oid: Option<String>,
}

// Why both halves: a finished verdict never expires on its own; only a re-request ends it.
pub fn viewer_standing(pr: &PullRequest, viewer_login: Option<&str>) -> ViewerStanding {
    let Some(viewer) = viewer_login.map(str::to_lowercase) else {
        return ViewerStanding::default();
    };
    let by_viewer = |login: &str| login.to_lowercase() == viewer;
    let latest = pr
        .latest_reviews
        .iter()
        .find(|review| by_viewer(&review.login));
    let opinionated = pr
        .latest_opinionated_reviews
        .as_deref()
        .and_then(|reviews| reviews.iter().find(|review| by_viewer(&review.login)));
    let verdict = latest.or(opinionated).map(|review| review.state);
    let is_owed = pr.pending_user_reviewers().contains(&viewer);
    ViewerStanding {
        is_finished: !is_owed
            && matches!(
                verdict,
                Some(ReviewState::Approved | ReviewState::ChangesRequested)
            ),
        is_owed,
        commit_oid: latest
            .and_then(|review| review.commit_oid.clone())
            .or_else(|| opinionated.and_then(|review| review.commit_oid.clone()))
            .or_else(|| {
                pr.viewer_latest_review
                    .as_ref()
                    .and_then(|review| review.commit_oid.clone())
            }),
    }
}

pub fn viewer_in_review_lists(pr: &PullRequest, viewer_login: Option<&str>) -> bool {
    let Some(viewer) = viewer_login.map(str::to_lowercase) else {
        return false;
    };
    let by_viewer = |login: &str| login.to_lowercase() == viewer;
    pr.latest_reviews
        .iter()
        .any(|review| by_viewer(&review.login))
        || pr
            .latest_opinionated_reviews
            .as_deref()
            .is_some_and(|reviews| reviews.iter().any(|review| by_viewer(&review.login)))
}
