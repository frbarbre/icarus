use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadSyncObservation {
    pub head: String,
    pub symbolic_branch: Option<String>,
    pub dirty_tracked: bool,
    pub target_reachable: bool,
    pub viewer_email: Option<String>,
    pub local_only_authors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadSyncSkip {
    Dirty,
    OwnCommits,
    OtherBranch,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HeadSyncDecision {
    Current,
    Skipped { reason: HeadSyncSkip },
    ResetHard { previous_head: String },
}

pub fn is_full_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn decide_head_sync(
    branch: &str,
    target_oid: &str,
    observed: &HeadSyncObservation,
) -> HeadSyncDecision {
    let skip = |reason| HeadSyncDecision::Skipped { reason };
    if !is_full_oid(target_oid) {
        return skip(HeadSyncSkip::Unreachable);
    }
    if observed.head == target_oid {
        return HeadSyncDecision::Current;
    }
    // Why: a detached HEAD or another branch is a rebase in progress or a checkout the reviewer chose.
    if observed.symbolic_branch.as_deref() != Some(branch) {
        return skip(HeadSyncSkip::OtherBranch);
    }
    if observed.dirty_tracked {
        return skip(HeadSyncSkip::Dirty);
    }
    if !observed.target_reachable {
        return skip(HeadSyncSkip::Unreachable);
    }
    let authors: Vec<String> = observed
        .local_only_authors
        .iter()
        .map(|author| author.trim().to_lowercase())
        .filter(|author| !author.is_empty())
        .collect();
    let viewer = observed.viewer_email.as_deref().map(str::to_lowercase);
    // Why by author: after a force-push every old commit is local-only, but only the reviewer's own is lost work.
    if !authors.is_empty()
        && viewer
            .as_ref()
            .is_none_or(|viewer| authors.contains(viewer))
    {
        return skip(HeadSyncSkip::OwnCommits);
    }
    HeadSyncDecision::ResetHard {
        previous_head: observed.head.clone(),
    }
}
