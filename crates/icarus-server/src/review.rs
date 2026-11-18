//! Review mode: a reviewer's draft comments, summary and verdict per workspace and pull request, the diff base,
//! and the GitHub calls that publish a review and act on its threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use icarus_git::review_base::{ReviewBase, commit_exists, resolve_review_base};
use icarus_github::GitHubError;
use icarus_github::attachment::{
    MAX_IMAGE_BYTES, MAX_VIDEO_BYTES, attachment_markdown, check_attachment, content_type_for,
    is_video,
};
use icarus_github::conversation::{ConversationComment, ConversationKind};
use icarus_github::diff_lines::{Placement, file_comment_body};
use icarus_github::review::{CommentRef, CommentTarget, DraftComment, Reaction, Side, Verdict};
use icarus_github::review_context::ReviewContext;
use icarus_github::review_threads::{PrComment, PrThread, ReactionSummary};
use icarus_protocol::RpcError;
use icarus_rules::facts::{RepoRef, RequestedReviewer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::{local_branch, repo_ref};
use crate::server::{Server, params, rpc};
use crate::state::Workspace;

/// GitHub's own cap on queued drafts in the fork; the oldest fall off.
pub const MAX_DRAFTS: usize = 200;
/// GitHub refuses comment bodies over 65,536 characters.
const MAX_BODY_CHARS: usize = 65_536;
pub const REVIEW_CHANGED_TOPIC: &str = "review.changed";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", error)
}

/// Now in GitHub's timestamp format, e.g. `2026-11-17T09:30:00Z`.
fn now_rfc3339() -> String {
    rfc3339(now_ms() / 1000)
}

fn rfc3339(seconds: u64) -> String {
    let (days, of_day) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Howard Hinnant's days-to-civil conversion.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

fn github_error(error: GitHubError) -> RpcError {
    let code = match &error {
        GitHubError::Invalid(_) => "invalid",
        GitHubError::NotFound(_) => "not_found",
        _ => "github_failed",
    };
    rpc(code, error)
}

fn default_side() -> String {
    "RIGHT".to_owned()
}

fn line_subject() -> String {
    "line".to_owned()
}

/// One queued review comment. `line` is the end line; `start_line` is set only for a range and is always below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default = "default_side")]
    pub side: String,
    #[serde(default)]
    pub start_side: Option<String>,
    /// `line`, or `file` for a comment GitHub only takes on the whole file.
    #[serde(default = "line_subject")]
    pub subject: String,
    pub body: String,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

impl Draft {
    fn to_github(&self) -> DraftComment {
        let side = Side::parse(&self.side).unwrap_or(Side::Right);
        let target = match (self.subject.as_str(), self.line) {
            ("file", _) | (_, None) => CommentTarget::File,
            (_, Some(line)) => CommentTarget::Line {
                line,
                side,
                start: self.start_line.map(|start| {
                    (
                        start,
                        self.start_side
                            .as_deref()
                            .and_then(Side::parse)
                            .unwrap_or(side),
                    )
                }),
            },
        };
        DraftComment {
            path: self.path.clone(),
            body: self.body.clone(),
            target,
        }
    }
}

/// Everything the reviewer has not sent yet for one pull request, persisted in `state.db`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// `since-review`, `whole`, `commit:<oid>` or `review:<oid>`; `None` uses the default.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub drafts: Vec<Draft>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub verdict: Option<String>,
}

impl Session {
    fn is_empty(&self) -> bool {
        self.base.is_none()
            && self.drafts.is_empty()
            && self.summary.trim().is_empty()
            && self.verdict.is_none()
    }

    fn from_value(value: Option<Value>) -> Self {
        value
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    fn into_value(self) -> Option<Value> {
        (!self.is_empty()).then(|| serde_json::to_value(self).unwrap_or(Value::Null))
    }
}

/// Approve is always submittable; comment and request changes need a draft or a non-blank body.
// Why request changes too: GitHub answers an empty one with "You need to leave a comment indicating the requested changes."
pub fn can_submit(verdict: Verdict, body: &str, drafts: usize) -> bool {
    match verdict {
        Verdict::Comment | Verdict::RequestChanges => drafts > 0 || !body.trim().is_empty(),
        Verdict::Approve => true,
    }
}

fn check_body(body: &str, allow_empty: bool) -> Result<(), RpcError> {
    if !allow_empty && body.trim().is_empty() {
        return Err(rpc("invalid", "A comment cannot be empty."));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(rpc("invalid", "Comment is too large to submit safely."));
    }
    // Why: a draft saved mid-upload holds a dead `blob:` link that would be posted as is.
    if body.contains("](blob:") {
        return Err(rpc(
            "invalid",
            "The comment still has an upload in progress (a blob: link).",
        ));
    }
    Ok(())
}

/// The review standing pill: drafts first, then the viewer's verdict on record, then a request.
pub fn standing(context: Option<&ReviewContext>, drafts: usize) -> Option<&'static str> {
    if drafts > 0 {
        return Some("pending");
    }
    let context = context?;
    match context.viewer_latest_review_state.as_deref() {
        Some("CHANGES_REQUESTED") => Some("changes-requested"),
        Some("APPROVED") => Some("approved"),
        _ if context.viewer_has_review_request => Some("requested"),
        Some("COMMENTED") => Some("commented"),
        Some("DISMISSED") => Some("dismissed"),
        _ => None,
    }
}

fn short(oid: &str) -> &str {
    &oid[..7.min(oid.len())]
}

fn review_tags(context: &ReviewContext, oid: &str) -> Vec<String> {
    context
        .viewer_reviews
        .iter()
        .filter(|r| r.commit_oid == oid)
        .map(|r| format!("Review {}", r.ordinal))
        .collect()
}

/// The base picker's options: since your last review, each PR commit newest first, rebased-away reviews, the target.
pub fn base_options(context: &ReviewContext, has_commit: &dyn Fn(&str) -> bool) -> Vec<Value> {
    let mut options = Vec::new();
    let latest = context.viewer_latest_review_commit.as_deref();
    let label = match context.viewer_reviews.last() {
        Some(last) if latest.is_some() => {
            format!("Since your last review (Review {})", last.ordinal)
        }
        _ => "Since last review".to_owned(),
    };
    options.push(json!({
        "value": "since-review",
        "label": label,
        "commit": latest,
        "available": latest.is_some_and(has_commit),
    }));
    let in_pr: Vec<&str> = context.commits.iter().map(|c| c.oid.as_str()).collect();
    for commit in context.commits.iter().rev() {
        options.push(json!({
            "value": format!("commit:{}", commit.oid),
            "label": format!("{} {}", short(&commit.oid), commit.headline),
            "commit": commit.oid,
            "short": short(&commit.oid),
            "headline": commit.headline,
            "tags": review_tags(context, &commit.oid),
            "available": has_commit(&commit.oid),
        }));
    }
    let mut seen = Vec::new();
    for review in &context.viewer_reviews {
        if in_pr.contains(&review.commit_oid.as_str()) || seen.contains(&review.commit_oid) {
            continue;
        }
        seen.push(review.commit_oid.clone());
        options.push(json!({
            "value": format!("review:{}", review.commit_oid),
            "label": format!("{} Reviewed before a rebase", short(&review.commit_oid)),
            "commit": review.commit_oid,
            "short": short(&review.commit_oid),
            "tags": review_tags(context, &review.commit_oid),
            "available": has_commit(&review.commit_oid),
        }));
    }
    options.push(json!({
        "value": "whole",
        "label": format!("{} · target branch", context.base_ref_name),
        "available": true,
    }));
    options
}

/// `since-review` when the last-reviewed commit is here, else the whole PR against its target.
pub fn default_base(context: &ReviewContext, has_commit: &dyn Fn(&str) -> bool) -> &'static str {
    match context.viewer_latest_review_commit.as_deref() {
        Some(oid) if has_commit(oid) => "since-review",
        _ => "whole",
    }
}

fn valid_pick(pick: &str) -> bool {
    match pick {
        "since-review" | "whole" => true,
        other => other
            .strip_prefix("commit:")
            .or_else(|| other.strip_prefix("review:"))
            .is_some_and(|oid| {
                (7..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
            }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedBase {
    pub pick: String,
    /// The workspace base the Changes list now reads against.
    pub base_ref: String,
    /// `reviewed`, `interdiff` or `target`.
    pub kind: &'static str,
    pub conflicted: bool,
    /// The pick could not be honoured (the reviewed commit is gone), so the whole PR is shown.
    pub fell_back: bool,
}

fn try_fetch(worktree: &Path, what: &[&str]) {
    let mut args = vec!["fetch", "--quiet", "--no-tags", "origin"];
    args.extend_from_slice(what);
    let _ = icarus_git::git(worktree, &args);
}

/// Resolves a base pick to a workspace base ref, replaying the reviewed version after a rebase.
pub fn apply_base(
    worktree: &Path,
    pick: &str,
    context: &ReviewContext,
) -> Result<AppliedBase, String> {
    let whole = |fell_back: bool| AppliedBase {
        pick: pick.to_owned(),
        base_ref: format!("origin/{}", context.base_ref_name),
        kind: "target",
        conflicted: false,
        fell_back,
    };
    if pick == "whole" {
        return Ok(whole(false));
    }
    let target = format!("refs/remotes/origin/{}", context.base_ref_name);
    if !commit_exists(worktree, &target) {
        try_fetch(worktree, &[&context.base_ref_name]);
    }
    let (reviewed, parent) = match pick {
        "since-review" => match context.viewer_latest_review_commit.as_deref() {
            Some(oid) => (oid.to_owned(), false),
            None => return Ok(whole(true)),
        },
        other => match (other.strip_prefix("commit:"), other.strip_prefix("review:")) {
            (Some(oid), _) => (oid.to_owned(), true),
            (_, Some(oid)) => (oid.to_owned(), false),
            _ => return Err(format!("unknown base {other:?}")),
        },
    };
    if !commit_exists(worktree, &reviewed) {
        try_fetch(worktree, &[&reviewed]);
    }
    // Why the parent: picking a commit includes that commit's own changes in the diff.
    let base_commit = if parent {
        format!("{reviewed}^")
    } else {
        reviewed.clone()
    };
    let resolved =
        resolve_review_base(worktree, &base_commit, &target).map_err(|e| format!("{e:#}"))?;
    match resolved {
        ReviewBase::Reviewed { base } => Ok(AppliedBase {
            pick: pick.to_owned(),
            base_ref: base,
            kind: "reviewed",
            conflicted: false,
            fell_back: false,
        }),
        ReviewBase::Interdiff {
            base_ref,
            conflicted,
        } => Ok(AppliedBase {
            pick: pick.to_owned(),
            base_ref,
            kind: "interdiff",
            conflicted,
            fell_back: false,
        }),
        ReviewBase::Missing if pick == "since-review" => Ok(whole(true)),
        ReviewBase::Missing => Err(format!(
            "commit {} is not in this repository (or shares no history with {})",
            short(&reviewed),
            context.base_ref_name
        )),
    }
}

/// Where a thread sits in the current file: on its line, or stacked at the end as outdated.
pub fn thread_placement(thread: &PrThread, line_count: Option<usize>) -> Value {
    if thread.subject == "file" {
        return json!({ "file": true, "outdated": false, "line": null, "label": "File comment" });
    }
    let past_end = thread.side == "RIGHT"
        && thread
            .line
            .zip(line_count)
            .is_some_and(|(line, count)| line as usize > count);
    let outdated = thread.is_outdated || thread.line.is_none_or(|line| line < 1) || past_end;
    let label = match (outdated, thread.original_line) {
        (true, Some(line)) => format!("Outdated · was on line {line}"),
        (true, None) => "Outdated".to_owned(),
        (false, _) => match (thread.start_line, thread.line) {
            (Some(start), Some(end)) if start != end => format!("Lines {start}-{end}"),
            (_, Some(line)) => format!("Line {line}"),
            _ => String::new(),
        },
    };
    json!({
        "file": false,
        "outdated": outdated,
        "line": if outdated { None } else { thread.line },
        "start_line": if outdated { None } else { thread.start_line },
        "label": label,
    })
}

fn toggle_reaction(reactions: &mut Vec<ReactionSummary>, content: &str, reacted: bool) {
    match reactions.iter_mut().find(|r| r.content == content) {
        Some(summary) if summary.viewer_has_reacted != reacted => {
            summary.viewer_has_reacted = reacted;
            summary.count = if reacted {
                summary.count + 1
            } else {
                summary.count.saturating_sub(1)
            };
        }
        Some(_) => {}
        None if reacted => reactions.push(ReactionSummary {
            content: content.to_owned(),
            count: 1,
            viewer_has_reacted: true,
            reactors: Vec::new(),
        }),
        None => {}
    }
    reactions.retain(|r| r.count > 0);
}

fn thread_json(thread: &PrThread, line_count: Option<usize>) -> Value {
    let mut value = serde_json::to_value(thread).unwrap_or(Value::Null);
    value["placement"] = thread_placement(thread, line_count);
    value
}

struct Target {
    repo: RepoRef,
    number: u64,
    key: String,
}

#[derive(Deserialize)]
struct At {
    workspace: String,
    #[serde(default)]
    pr: Option<u64>,
    #[serde(default)]
    refresh: bool,
}

#[derive(Deserialize)]
struct AddComment {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    start_side: Option<String>,
    /// Ask for a file comment outright, with no line.
    #[serde(default)]
    file: bool,
    body: String,
}

/// A comment already on GitHub: a review comment or review summary by node id, or a PR conversation comment by id.
#[derive(Deserialize)]
struct Published {
    kind: String,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    body: Option<String>,
}

impl Published {
    fn comment_ref(&self) -> Result<CommentRef, RpcError> {
        let node_id = || {
            self.node_id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| rpc("invalid", "node_id is required"))
        };
        match self.kind.as_str() {
            "review_comment" => Ok(CommentRef::ReviewComment {
                node_id: node_id()?,
            }),
            "review_summary" => Ok(CommentRef::ReviewSummary {
                node_id: node_id()?,
            }),
            "issue_comment" => Ok(CommentRef::IssueComment {
                id: self.id.ok_or_else(|| rpc("invalid", "id is required"))?,
            }),
            _ => Err(rpc(
                "invalid",
                "kind must be review_comment, review_summary or issue_comment",
            )),
        }
    }
}

#[derive(Deserialize)]
struct PostConversation {
    body: String,
    /// The author of the comment this answers.
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
struct EditComment {
    id: String,
    body: String,
}

#[derive(Deserialize)]
struct CommentId {
    id: String,
}

#[derive(Deserialize)]
struct SetBase {
    base: String,
}

#[derive(Deserialize)]
struct SetSummary {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    verdict: Option<String>,
}

#[derive(Deserialize)]
struct Submit {
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct ThreadsQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    unresolved: bool,
    /// Also the PR's issue comments and review summaries, which only the PR panel lists.
    #[serde(default)]
    conversation: bool,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    workspace: Option<String>,
    thread: String,
    body: String,
}

#[derive(Deserialize)]
struct ThreadRef {
    #[serde(default)]
    workspace: Option<String>,
    thread: String,
}

#[derive(Deserialize)]
struct React {
    #[serde(default)]
    workspace: Option<String>,
    subject: String,
    reaction: String,
    #[serde(default)]
    remove: bool,
}

#[derive(Deserialize)]
struct Reviewers {
    /// Logins, or `org/team` for a team.
    reviewers: Vec<String>,
}

/// `org/team` (or `@org/team`) names a team; anything else is a user login.
pub fn parse_reviewers(names: &[String]) -> Result<Vec<RequestedReviewer>, RpcError> {
    let reviewers: Vec<RequestedReviewer> = names
        .iter()
        .flat_map(|name| name.split([',', ' ']))
        .map(|name| name.trim().trim_start_matches('@'))
        .filter(|name| !name.is_empty())
        .map(|name| {
            if name.contains('/') {
                RequestedReviewer::team(name)
            } else {
                RequestedReviewer::user(name)
            }
        })
        .collect();
    if reviewers.is_empty() {
        return Err(rpc("invalid", "Enter at least one reviewer"));
    }
    Ok(reviewers)
}

#[derive(Deserialize)]
struct Upload {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    data_base64: Option<String>,
}

fn file_lines(worktree: &Path, path: &str) -> Option<Vec<String>> {
    let full = worktree.join(path);
    if !full.starts_with(worktree) || path.split('/').any(|part| part == "..") {
        return None;
    }
    std::fs::read_to_string(full)
        .ok()
        .map(|text| text.lines().map(str::to_owned).collect())
}

impl Server {
    pub(crate) async fn review_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "review.get" => {
                let at: At = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                self.review_view(&workspace, &target).await
            }
            "review.set_base" => {
                let at: At = params(p.clone())?;
                let SetBase { base } = params(p)?;
                let base = base.trim().to_owned();
                if !valid_pick(&base) {
                    return Err(rpc(
                        "invalid",
                        "base must be since-review, whole, commit:<sha> or review:<sha>",
                    ));
                }
                let (workspace, target) = self.review_target(&at).await?;
                let context = self
                    .github
                    .review_context(&target.repo, target.number, false)
                    .await
                    .map_err(github_error)?;
                let applied = self
                    .apply_review_base(&workspace, &base, &context)
                    .await
                    .map_err(|e| rpc("not_found", e))?;
                self.update_session(&workspace, &target, |session| {
                    session.base = Some(base.clone());
                    Ok(())
                })?;
                Ok(serde_json::to_value(applied).map_err(internal)?)
            }
            "review.comment.add" => {
                let at: At = params(p.clone())?;
                let add: AddComment = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                self.add_draft(&workspace, &target, add).await
            }
            "review.comment.post" => {
                let at: At = params(p.clone())?;
                let add: AddComment = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                self.post_comment_now(&workspace, &target, add).await
            }
            "review.conversation.post" => {
                let at: At = params(p.clone())?;
                let post: PostConversation = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                self.post_conversation_comment(&workspace, &target, post)
                    .await
            }
            "review.comment.edit_published" | "review.comment.delete_published" => {
                let at: At = params(p.clone())?;
                let published: Published = params(p)?;
                let comment = published.comment_ref()?;
                let (workspace, target) = self.review_target(&at).await?;
                let client = &self.github.client;
                let edited = if method == "review.comment.edit_published" {
                    let body = published
                        .body
                        .as_deref()
                        .ok_or_else(|| rpc("invalid", "body is required"))?;
                    check_body(body, false)?;
                    client
                        .edit_comment(&target.repo, &comment, body)
                        .await
                        .map_err(github_error)?;
                    Some(body.to_owned())
                } else {
                    client
                        .delete_comment(&target.repo, &comment)
                        .await
                        .map_err(github_error)?;
                    None
                };
                if self.patch_published(&comment, edited) {
                    self.broadcast(
                        REVIEW_CHANGED_TOPIC,
                        json!({ "workspace": workspace.id, "thread": null }),
                    );
                } else {
                    self.review_changed(Some(&workspace.id), None);
                }
                Ok(json!({
                    "edited": method == "review.comment.edit_published",
                    "deleted": method == "review.comment.delete_published",
                }))
            }
            "review.mentionables" => {
                let at: At = params(p)?;
                let (_workspace, target) = self.review_target(&at).await?;
                let users = self
                    .github
                    .assignable_users(&target.repo, at.refresh)
                    .await
                    .map_err(github_error)?;
                let author = self
                    .github
                    .review_context(&target.repo, target.number, false)
                    .await
                    .ok()
                    .and_then(|context| context.author_login);
                Ok(json!({ "users": users, "author": author }))
            }
            "review.reviewer_suggestions" => {
                let at: At = params(p)?;
                let (_workspace, target) = self.review_target(&at).await?;
                let suggested = self
                    .github
                    .client
                    .reviewer_suggestions(&target.repo, target.number)
                    .await
                    .map_err(github_error)?;
                // Why fail open: the suggestions alone still make a usable picker.
                let everyone = self
                    .github
                    .assignable_users(&target.repo, at.refresh)
                    .await
                    .unwrap_or_default();
                let author = self
                    .github
                    .review_context(&target.repo, target.number, false)
                    .await
                    .ok()
                    .and_then(|context| context.author_login);
                let everyone: Vec<_> = everyone
                    .into_iter()
                    .filter(|user| {
                        !suggested
                            .iter()
                            .any(|known| known.login.eq_ignore_ascii_case(&user.login))
                            && author
                                .as_deref()
                                .is_none_or(|author| !author.eq_ignore_ascii_case(&user.login))
                    })
                    .collect();
                Ok(json!({ "suggested": suggested, "everyone": everyone, "author": author }))
            }
            "review.comment.edit" => {
                let at: At = params(p.clone())?;
                let edit: EditComment = params(p)?;
                check_body(&edit.body, false)?;
                let (workspace, target) = self.review_target(&at).await?;
                let draft = self.update_session(&workspace, &target, |session| {
                    let draft = session
                        .drafts
                        .iter_mut()
                        .find(|d| d.id == edit.id)
                        .ok_or_else(|| rpc("not_found", format!("draft {}", edit.id)))?;
                    draft.body = edit.body.clone();
                    draft.updated_at = Some(now_ms());
                    Ok(draft.clone())
                })?;
                Ok(json!({ "draft": draft }))
            }
            "review.comment.remove" => {
                let at: At = params(p.clone())?;
                let CommentId { id } = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                let left = self.update_session(&workspace, &target, |session| {
                    let before = session.drafts.len();
                    session.drafts.retain(|d| d.id != id);
                    if session.drafts.len() == before {
                        return Err(rpc("not_found", format!("draft {id}")));
                    }
                    Ok(session.drafts.len())
                })?;
                Ok(json!({ "removed": id, "drafts": left }))
            }
            "review.comment.clear" => {
                let at: At = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                let cleared = self.update_session(&workspace, &target, |session| {
                    Ok(std::mem::take(&mut session.drafts).len())
                })?;
                Ok(json!({ "cleared": cleared }))
            }
            "review.comment.list" => {
                let at: At = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                let session = self.session(&workspace, &target)?;
                Ok(json!({ "drafts": session.drafts }))
            }
            "review.set_summary" => {
                let at: At = params(p.clone())?;
                let set: SetSummary = params(p)?;
                if let Some(body) = &set.body {
                    check_body(body, true)?;
                }
                let verdict = match set.verdict.as_deref().map(str::trim) {
                    None => None,
                    Some("") => Some(None),
                    Some(name) => Some(Some(
                        Verdict::parse(name)
                            .ok_or_else(|| {
                                rpc(
                                    "invalid",
                                    "verdict must be comment, approve or request-changes",
                                )
                            })?
                            .name()
                            .to_owned(),
                    )),
                };
                let (workspace, target) = self.review_target(&at).await?;
                let session = self.update_session(&workspace, &target, |session| {
                    if let Some(body) = set.body.clone() {
                        session.summary = body;
                    }
                    if let Some(verdict) = verdict.clone() {
                        session.verdict = verdict;
                    }
                    Ok(session.clone())
                })?;
                Ok(json!({ "summary": session.summary, "verdict": session.verdict }))
            }
            "review.submit" => {
                let at: At = params(p.clone())?;
                let submit: Submit = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                self.submit_review(&workspace, &target, submit).await
            }
            "review.threads" => {
                let at: At = params(p.clone())?;
                let query: ThreadsQuery = params(p)?;
                let (workspace, target) = self.review_target(&at).await?;
                if at.refresh {
                    self.github.forget_review_threads();
                }
                let threads = self
                    .github
                    .review_threads(&target.repo, target.number)
                    .await
                    .map_err(github_error)?;
                let threads: Vec<Value> = threads
                    .iter()
                    .filter(|t| query.path.as_deref().is_none_or(|p| t.path == p))
                    .filter(|t| !query.unresolved || !t.is_resolved)
                    .map(|t| {
                        let count = file_lines(&workspace.path, &t.path).map(|l| l.len());
                        thread_json(t, count)
                    })
                    .collect();
                let mut reply = json!({
                    "pr": { "repo": target.repo.slug(), "number": target.number },
                    "threads": threads,
                });
                if query.conversation {
                    // The threads still count when the other sources fail, as in Orca.
                    let entries = self
                        .github
                        .conversation(&target.repo, target.number)
                        .await
                        .inspect_err(|error| tracing::warn!("PR conversation failed: {error}"))
                        .unwrap_or_default();
                    reply["conversation"] = json!(entries);
                }
                Ok(reply)
            }
            "review.reply" => {
                let reply: Reply = params(p)?;
                check_body(&reply.body, false)?;
                let posted = self
                    .github
                    .client
                    .reply_in_thread(&reply.thread, &reply.body)
                    .await
                    .map_err(github_error)?;
                let comment = PrComment {
                    id: posted.id.clone(),
                    database_id: posted.database_id,
                    author: self.github.viewer(),
                    author_avatar_url: None,
                    is_bot: false,
                    body: reply.body.clone(),
                    url: posted.url.clone(),
                    created_at: Some(now_rfc3339()),
                    last_edited_at: None,
                    viewer_can_update: true,
                    viewer_can_delete: true,
                    reactions: Vec::new(),
                };
                let patched = self
                    .github
                    .patch_thread(&reply.thread, |thread| thread.comments.push(comment));
                self.review_mutated(reply.workspace.as_deref(), &reply.thread, patched);
                Ok(json!({ "thread": reply.thread, "comment": posted }))
            }
            "review.resolve" | "review.unresolve" => {
                let thread: ThreadRef = params(p)?;
                let resolved = self
                    .github
                    .client
                    .set_thread_resolved(&thread.thread, method == "review.resolve")
                    .await
                    .map_err(github_error)?;
                let patched = self.github.patch_thread(&thread.thread, |cached| {
                    cached.is_resolved = resolved;
                    cached.viewer_can_resolve = !resolved;
                    cached.viewer_can_unresolve = resolved;
                });
                self.review_mutated(thread.workspace.as_deref(), &thread.thread, patched);
                Ok(json!({ "thread": thread.thread, "resolved": resolved }))
            }
            "review.react" => {
                let react: React = params(p)?;
                let reaction = Reaction::parse(&react.reaction).ok_or_else(|| {
                    rpc(
                        "invalid",
                        "reaction must be one of +1, -1, laugh, confused, heart, hooray, rocket, eyes",
                    )
                })?;
                self.github
                    .client
                    .set_reaction(&react.subject, reaction, !react.remove)
                    .await
                    .map_err(github_error)?;
                let content = reaction.short();
                let reacted = !react.remove;
                let patched = self.github.patch_comment(&react.subject, |comment| {
                    toggle_reaction(&mut comment.reactions, content, reacted)
                }) || self.github.patch_conversation(|entries| {
                    match entries.iter_mut().find(|entry| entry.id == react.subject) {
                        Some(entry) => {
                            toggle_reaction(&mut entry.reactions, content, reacted);
                            true
                        }
                        None => false,
                    }
                });
                if !patched {
                    self.github.forget_review_threads();
                }
                self.broadcast(
                    REVIEW_CHANGED_TOPIC,
                    json!({ "workspace": react.workspace, "thread": null }),
                );
                Ok(json!({
                    "subject": react.subject,
                    "reaction": reaction.short(),
                    "reacted": !react.remove,
                }))
            }
            "review.request_reviewers" | "review.remove_reviewers" => {
                let at: At = params(p.clone())?;
                let names: Reviewers = params(p)?;
                let reviewers = parse_reviewers(&names.reviewers)?;
                let (workspace, target) = self.review_target(&at).await?;
                let client = &self.github.client;
                if method == "review.request_reviewers" {
                    client
                        .request_reviewers(&target.repo, target.number, &reviewers)
                        .await
                } else {
                    client
                        .remove_reviewers(&target.repo, target.number, &reviewers)
                        .await
                }
                .map_err(github_error)?;
                // The panel's reviewer rows come from the cached PR snapshot.
                self.github.invalidate(&workspace.project_id);
                self.review_changed(Some(&workspace.id), None);
                Ok(
                    json!({ "reviewers": reviewers, "requested": method == "review.request_reviewers" }),
                )
            }
            "review.upload" => {
                let at: At = params(p.clone())?;
                let upload: Upload = params(p)?;
                let workspace = self.review_workspace(&at.workspace)?;
                self.upload_media(&workspace, upload).await
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    /// A workspace by id, name or path, or the workspace whose folder contains the path (a CLI's cwd).
    pub(crate) fn review_workspace(&self, key: &str) -> Result<Workspace, RpcError> {
        if let Some(workspace) = self.store.workspace(key).map_err(internal)? {
            return Ok(workspace);
        }
        let path = Path::new(key);
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.store
            .workspaces(None)
            .map_err(internal)?
            .into_iter()
            .filter(|w| {
                path.starts_with(&w.path)
                    || std::fs::canonicalize(&w.path).is_ok_and(|root| real.starts_with(root))
            })
            .max_by_key(|w| w.path.as_os_str().len())
            .ok_or_else(|| rpc("not_found", format!("workspace {key}")))
    }

    async fn review_target(&self, at: &At) -> Result<(Workspace, Target), RpcError> {
        let workspace = self.review_workspace(&at.workspace)?;
        let project = self
            .store
            .project(&workspace.project_id)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", "project"))?;
        let repo = repo_ref(&project).ok_or_else(|| {
            rpc(
                "no_github",
                "This workspace has no GitHub repository to review against.",
            )
        })?;
        let number = match at.pr {
            Some(number) => number,
            None => {
                let branch = local_branch(&workspace).await;
                let view = self
                    .pr_view(&workspace, branch.as_deref(), at.refresh)
                    .await;
                match view.details {
                    Some(details) => details.pr.number,
                    None => {
                        return Err(rpc(
                            "no_pr",
                            view.error.unwrap_or_else(|| {
                                "This workspace's branch has no pull request.".to_owned()
                            }),
                        ));
                    }
                }
            }
        };
        let key = repo.match_key(number);
        Ok((workspace, Target { repo, number, key }))
    }

    fn session(&self, workspace: &Workspace, target: &Target) -> Result<Session, RpcError> {
        Ok(Session::from_value(
            self.store
                .review_session(&workspace.id, &target.key)
                .map_err(internal)?,
        ))
    }

    fn update_session<R>(
        &self,
        workspace: &Workspace,
        target: &Target,
        f: impl FnOnce(&mut Session) -> Result<R, RpcError>,
    ) -> Result<R, RpcError> {
        let result = self
            .store
            .update_review_session(&workspace.id, &target.key, |value| {
                let mut session = Session::from_value(value);
                let result = f(&mut session)?;
                Ok::<_, RpcError>((session.into_value(), result))
            })
            .map_err(internal)??;
        // A session is local (drafts, summary, base), so GitHub's threads stay cached.
        self.broadcast(
            REVIEW_CHANGED_TOPIC,
            json!({ "workspace": workspace.id, "thread": null }),
        );
        Ok(result)
    }

    /// Like [`Self::review_changed`], but keeps the cached threads when the mutation was applied to them.
    fn review_mutated(&self, workspace: Option<&str>, thread: &str, patched: bool) {
        if !patched {
            self.github.forget_review_threads();
        }
        self.broadcast(
            REVIEW_CHANGED_TOPIC,
            json!({ "workspace": workspace, "thread": thread }),
        );
    }

    /// Applies an edit (`Some(body)`) or delete GitHub confirmed to the cached comment; false when
    /// no cache holds it and it must be read again.
    fn patch_published(&self, comment: &CommentRef, edited: Option<String>) -> bool {
        let matches = |entry: &ConversationComment| match comment {
            CommentRef::IssueComment { id } => entry.database_id == Some(*id),
            CommentRef::ReviewSummary { node_id } => entry.id == *node_id,
            CommentRef::ReviewComment { .. } => false,
        };
        if let CommentRef::ReviewComment { node_id } = comment {
            return match edited {
                Some(body) => self
                    .github
                    .patch_comment(node_id, |cached| cached.body = body),
                None => false,
            };
        }
        self.github.patch_conversation(|entries| {
            let Some(index) = entries.iter().position(&matches) else {
                return false;
            };
            match &edited {
                Some(body) => entries[index].body = body.clone(),
                None => {
                    entries.remove(index);
                }
            }
            true
        })
    }

    /// Posts a comment on the PR's conversation, `@author`-addressed when it answers one, as Orca
    /// does: GitHub has no replies to conversation comments.
    async fn post_conversation_comment(
        &self,
        workspace: &Workspace,
        target: &Target,
        post: PostConversation,
    ) -> Result<Value, RpcError> {
        check_body(&post.body, false)?;
        let handle = post
            .reply_to
            .as_deref()
            .map(|author| author.trim().trim_end_matches("[bot]").trim())
            .filter(|author| !author.is_empty());
        let body = match handle {
            Some(handle) => format!("@{handle} {}", post.body.trim()),
            None => post.body.trim().to_owned(),
        };
        let posted = self
            .github
            .client
            .add_issue_comment(&target.repo, target.number, &body)
            .await
            .map_err(github_error)?;
        let comment = ConversationComment {
            id: posted.node_id.clone().unwrap_or_default(),
            database_id: Some(posted.id),
            kind: ConversationKind::Comment,
            author: self.github.viewer(),
            author_avatar_url: None,
            is_bot: false,
            body,
            url: posted.html_url.clone(),
            created_at: Some(now_rfc3339()),
            review_state: None,
            viewer_can_update: true,
            viewer_can_delete: true,
            reactions: Vec::new(),
        };
        if self
            .github
            .push_conversation(&target.repo, target.number, comment.clone())
        {
            self.broadcast(
                REVIEW_CHANGED_TOPIC,
                json!({ "workspace": workspace.id, "thread": null }),
            );
        } else {
            self.review_changed(Some(&workspace.id), None);
        }
        Ok(json!({ "comment": comment }))
    }

    fn review_changed(&self, workspace: Option<&str>, thread: Option<&str>) {
        self.github.forget_review_threads();
        self.broadcast(
            REVIEW_CHANGED_TOPIC,
            json!({ "workspace": workspace, "thread": thread }),
        );
    }

    /// Adds already-built drafts (an import) to the review of the workspace's pull request, and the
    /// summary when the review has none yet.
    pub(crate) async fn import_review_drafts(
        &self,
        workspace: &Workspace,
        drafts: Vec<Draft>,
        summary: Option<String>,
    ) -> Result<bool, RpcError> {
        let at = At {
            workspace: workspace.id.clone(),
            pr: workspace
                .pull_request
                .as_ref()
                .and_then(|pr| pr["number"].as_u64()),
            refresh: false,
        };
        let (workspace, target) = self.review_target(&at).await?;
        self.update_session(&workspace, &target, |session| {
            session.drafts.extend(drafts);
            if session.drafts.len() > MAX_DRAFTS {
                let excess = session.drafts.len() - MAX_DRAFTS;
                session.drafts.drain(..excess);
            }
            let mut summary_set = false;
            if let Some(summary) = summary
                && session.summary.trim().is_empty()
            {
                session.summary = summary;
                summary_set = true;
            }
            Ok(summary_set)
        })
    }

    /// Unsent draft comments across the workspace's reviews; a workspace holding any is not removed silently.
    pub(crate) fn unsent_drafts(&self, workspace_id: &str) -> usize {
        self.store
            .review_sessions(workspace_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, value)| Session::from_value(Some(value)).drafts.len())
            .sum()
    }

    async fn apply_review_base(
        &self,
        workspace: &Workspace,
        pick: &str,
        context: &ReviewContext,
    ) -> Result<AppliedBase, String> {
        let path = workspace.path.clone();
        let (pick_c, context_c) = (pick.to_owned(), context.clone());
        let applied = tokio::task::spawn_blocking(move || apply_base(&path, &pick_c, &context_c))
            .await
            .map_err(|e| e.to_string())??;
        if workspace.base_ref.as_deref() != Some(applied.base_ref.as_str()) {
            let mut updated = workspace.clone();
            updated.base_ref = Some(applied.base_ref.clone());
            self.store
                .update_workspace(&updated)
                .map_err(|e| e.to_string())?;
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": [workspace.id], "source": "review.base" }),
            );
        }
        Ok(applied)
    }

    async fn review_view(&self, workspace: &Workspace, target: &Target) -> Result<Value, RpcError> {
        let session = self.session(workspace, target)?;
        // Why fail open: a lookup that did not answer must not hide what the reviewer is entitled to do.
        let (context, context_error) = match self
            .github
            .review_context(&target.repo, target.number, false)
            .await
        {
            Ok(context) => (Some(context), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let mut base = json!({ "selected": session.base, "applied_ref": workspace.base_ref });
        if let Some(context) = &context {
            let path = workspace.path.clone();
            let context_c = context.clone();
            let (options, default) = tokio::task::spawn_blocking(move || {
                let has = |oid: &str| commit_exists(&path, oid);
                (
                    base_options(&context_c, &has),
                    default_base(&context_c, &has),
                )
            })
            .await
            .map_err(internal)?;
            base["options"] = json!(options);
            base["default"] = json!(default);
            base["selected"] = json!(session.base.as_deref().unwrap_or(default));
            if let Some(pick) = session.base.as_deref() {
                match self.apply_review_base(workspace, pick, context).await {
                    Ok(applied) => {
                        base["applied_ref"] = json!(applied.base_ref);
                        base["applied"] = json!(applied);
                    }
                    Err(error) => base["error"] = json!(error),
                }
            }
        }
        let own = context.as_ref().is_some_and(|c| c.viewer_did_author);
        let verdicts: Vec<&str> = if own {
            vec!["comment"]
        } else {
            vec!["comment", "request-changes", "approve"]
        };
        let drafts = session.drafts.len();
        let can: serde_json::Map<String, Value> = verdicts
            .iter()
            .filter_map(|name| Verdict::parse(name))
            .map(|v| {
                (
                    v.name().to_owned(),
                    json!(can_submit(v, &session.summary, drafts)),
                )
            })
            .collect();
        Ok(json!({
            "workspace": workspace.id,
            "pr": {
                "repo": target.repo.slug(),
                "number": target.number,
                "id": context.as_ref().map(|c| &c.pull_request_id),
                "title": context.as_ref().map(|c| &c.title),
                "url": context.as_ref().map(|c| &c.url),
                "state": context.as_ref().map(|c| &c.state),
                "head_ref_name": context.as_ref().map(|c| &c.head_ref_name),
                "head_oid": context.as_ref().map(|c| &c.head_ref_oid),
                "base_ref_name": context.as_ref().map(|c| &c.base_ref_name),
                "author": context.as_ref().and_then(|c| c.author_login.as_ref()),
            },
            "viewer": context.as_ref().and_then(|c| c.viewer_login.as_ref()),
            "viewer_did_author": own,
            "viewer_latest_review": context.as_ref().map(|c| json!({
                "state": c.viewer_latest_review_state, "commit": c.viewer_latest_review_commit })),
            "viewer_has_review_request": context.as_ref().is_some_and(|c| c.viewer_has_review_request),
            "viewer_reviews": context.as_ref().map(|c| &c.viewer_reviews),
            "standing": standing(context.as_ref(), drafts),
            "verdicts": verdicts,
            "can_submit": can,
            "base": base,
            "drafts": session.drafts,
            "summary": session.summary,
            "verdict": session.verdict,
            "context_error": context_error,
        }))
    }

    async fn add_draft(
        &self,
        workspace: &Workspace,
        target: &Target,
        add: AddComment,
    ) -> Result<Value, RpcError> {
        let (draft, note) = self.build_draft(workspace, target, add).await?;
        let count = self.update_session(workspace, target, |session| {
            session.drafts.push(draft.clone());
            if session.drafts.len() > MAX_DRAFTS {
                let excess = session.drafts.len() - MAX_DRAFTS;
                session.drafts.drain(..excess);
            }
            Ok(session.drafts.len())
        })?;
        Ok(json!({
            "draft": draft,
            "drafts": count,
            "file_comment": draft.subject == "file" && note.is_some(),
            "note": note,
        }))
    }

    /// Posts one comment straight away ("Comment now"), outside the pending review.
    async fn post_comment_now(
        &self,
        workspace: &Workspace,
        target: &Target,
        add: AddComment,
    ) -> Result<Value, RpcError> {
        let (draft, note) = self.build_draft(workspace, target, add).await?;
        let context = self
            .github
            .review_context(&target.repo, target.number, false)
            .await
            .map_err(github_error)?;
        let posted = self
            .github
            .client
            .post_review_comment(
                &target.repo,
                target.number,
                &context.head_ref_oid,
                &draft.to_github(),
            )
            .await
            .map_err(github_error)?;
        self.review_changed(Some(&workspace.id), None);
        Ok(json!({
            "comment": { "id": posted.id, "node_id": posted.node_id, "url": posted.html_url },
            "path": draft.path,
            "line": draft.line,
            "file_comment": draft.subject == "file" && note.is_some(),
            "note": note,
        }))
    }

    async fn build_draft(
        &self,
        workspace: &Workspace,
        target: &Target,
        add: AddComment,
    ) -> Result<(Draft, Option<String>), RpcError> {
        check_body(&add.body, false)?;
        let path = add.path.trim().trim_start_matches("./").to_owned();
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|p| p == "..") {
            return Err(rpc(
                "invalid",
                "path must be relative to the repository root",
            ));
        }
        let side_name = add.side.as_deref().unwrap_or("RIGHT");
        let side =
            Side::parse(side_name).ok_or_else(|| rpc("invalid", "side must be LEFT or RIGHT"))?;
        let start_side = match add.start_side.as_deref() {
            Some(name) => Some(
                Side::parse(name)
                    .ok_or_else(|| rpc("invalid", "start_side must be LEFT or RIGHT"))?,
            ),
            None => None,
        };
        let line = if add.file { None } else { add.line };
        if line.is_none() && add.start_line.is_some() && !add.file {
            return Err(rpc("invalid", "start_line needs line"));
        }
        if line == Some(0) || add.start_line == Some(0) {
            return Err(rpc("invalid", "lines start at 1"));
        }
        let start_line = match (add.start_line, line) {
            (Some(start), Some(end)) if start > end => {
                return Err(rpc("invalid", "start_line must not be after line"));
            }
            (Some(start), Some(end)) if start == end => None,
            (start, Some(_)) => start,
            (_, None) => None,
        };
        let mut body = add.body.clone();
        let mut subject = if line.is_some() { "line" } else { "file" };
        let mut note = None;
        if let Some(end) = line {
            let start = start_line.unwrap_or(end);
            let placement = match self
                .github
                .client
                .pr_diff(&target.repo, target.number)
                .await
            {
                Ok(diff) => diff.place(&path, side, start, end),
                Err(_) => Placement::Unknown,
            };
            match placement {
                Placement::InDiff | Placement::Unknown => {}
                Placement::NotInPr => {
                    return Err(rpc(
                        "not_in_pr",
                        format!(
                            "{path} is not changed in pull request #{}, so GitHub cannot take a comment on it.",
                            target.number
                        ),
                    ));
                }
                Placement::OutsideDiff => {
                    let lines = if side == Side::Right {
                        file_lines(&workspace.path, &path).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let code: Vec<&str> = lines
                        .iter()
                        .skip(start as usize - 1)
                        .take((end - start + 1) as usize)
                        .map(String::as_str)
                        .collect();
                    body = file_comment_body(start, end, &code, &add.body);
                    subject = "file";
                    note = Some(format!(
                        "Line {start}{} is not in the pull request's diff, so it will post as a file comment quoting the code.",
                        if start == end {
                            String::new()
                        } else {
                            format!("-{end}")
                        }
                    ));
                }
            }
        }
        check_body(&body, false)?;
        let draft = Draft {
            id: format!("draft-{}", uuid::Uuid::new_v4().simple()),
            path,
            line: if subject == "file" { None } else { line },
            start_line: if subject == "file" { None } else { start_line },
            side: side.as_str().to_owned(),
            start_side: if subject == "file" || start_line.is_none() {
                None
            } else {
                start_side.map(|s| s.as_str().to_owned())
            },
            subject: subject.to_owned(),
            body,
            created_at: now_ms(),
            updated_at: None,
        };
        Ok((draft, note))
    }

    async fn submit_review(
        &self,
        workspace: &Workspace,
        target: &Target,
        submit: Submit,
    ) -> Result<Value, RpcError> {
        let session = self.session(workspace, target)?;
        let verdict_name = submit
            .verdict
            .clone()
            .or_else(|| session.verdict.clone())
            .ok_or_else(|| {
                rpc(
                    "invalid",
                    "choose a verdict: comment, approve or request-changes",
                )
            })?;
        let verdict = Verdict::parse(&verdict_name).ok_or_else(|| {
            rpc(
                "invalid",
                "verdict must be comment, approve or request-changes",
            )
        })?;
        let body = submit
            .body
            .clone()
            .unwrap_or_else(|| session.summary.clone());
        check_body(&body, true)?;
        for draft in &session.drafts {
            check_body(&draft.body, false)?;
        }
        if !can_submit(verdict, &body, session.drafts.len()) {
            return Err(rpc(
                "invalid",
                "Nothing to submit: add a comment or a summary first.",
            ));
        }
        let context = self
            .github
            .review_context(&target.repo, target.number, false)
            .await
            .map_err(github_error)?;
        if context.viewer_did_author && verdict != Verdict::Comment {
            return Err(rpc(
                "own_pull_request",
                "GitHub does not let the author of a pull request approve it or request changes on it; submit with comment.",
            ));
        }
        let comments: Vec<DraftComment> = session.drafts.iter().map(Draft::to_github).collect();
        let review = self
            .github
            .client
            .submit_review(&context.pull_request_id, verdict, body.trim(), &comments)
            .await
            .map_err(github_error)?;
        let sent: Vec<String> = session.drafts.iter().map(|d| d.id.clone()).collect();
        // Why by id: drafts added while the submit was in flight stay queued.
        self.update_session(workspace, target, |current| {
            current.drafts.retain(|d| !sent.contains(&d.id));
            current.summary.clear();
            current.verdict = None;
            Ok(())
        })?;
        self.github.invalidate(&workspace.project_id);
        self.github.forget_review_threads();
        Ok(json!({
            "id": review.id,
            "state": review.state,
            "url": review.url,
            "verdict": verdict.name(),
            "comments": comments.len(),
            "message": format!("Review submitted ({}).", review.state.to_lowercase().replace('_', " ")),
        }))
    }

    async fn upload_media(&self, workspace: &Workspace, upload: Upload) -> Result<Value, RpcError> {
        let project = self
            .store
            .project(&workspace.project_id)
            .map_err(internal)?
            .ok_or_else(|| rpc("not_found", "project"))?;
        let repo = repo_ref(&project).ok_or_else(|| {
            rpc(
                "no_github",
                "This workspace has no GitHub repository to attach the file to.",
            )
        })?;
        let (name, bytes) = match (&upload.path, &upload.data_base64) {
            (Some(path), _) => {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    workspace.path.join(path)
                };
                let name = upload.name.clone().unwrap_or_else(|| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".to_owned())
                });
                let content_type = content_type_for(&name)
                    .ok_or_else(|| rpc("invalid", "Only images and videos can be attached."))?;
                let limit = if is_video(content_type) {
                    MAX_VIDEO_BYTES
                } else {
                    MAX_IMAGE_BYTES
                };
                let size = std::fs::metadata(&path)
                    .map_err(|e| rpc("not_found", format!("{}: {e}", path.display())))?
                    .len();
                check_attachment(content_type, size.min(limit as u64 + 1) as usize)
                    .map_err(github_error)?;
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| rpc("not_found", format!("{}: {e}", path.display())))?;
                (name, bytes)
            }
            (None, Some(data)) => {
                let name = upload
                    .name
                    .clone()
                    .ok_or_else(|| rpc("invalid", "name is required with data_base64"))?;
                let bytes = B64
                    .decode(data.trim())
                    .map_err(|e| rpc("invalid", format!("data_base64: {e}")))?;
                (name, bytes)
            }
            (None, None) => return Err(rpc("invalid", "pass path or data_base64")),
        };
        let content_type = content_type_for(&name)
            .ok_or_else(|| rpc("invalid", "Only images and videos can be attached."))?;
        let size = bytes.len();
        let url = self
            .github
            .client
            .upload_attachment(&repo, &name, content_type, bytes)
            .await
            .map_err(github_error)?;
        Ok(json!({
            "url": url,
            "markdown": attachment_markdown(&name, content_type, &url),
            "name": name,
            "content_type": content_type,
            "kind": if is_video(content_type) { "video" } else { "image" },
            "size": size,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use icarus_github::review_context::{PrCommit, ViewerReview};

    #[test]
    fn timestamps_read_like_githubs() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    fn context() -> ReviewContext {
        ReviewContext {
            pull_request_id: "PR_1".into(),
            number: 5,
            base_ref_name: "main".into(),
            viewer_latest_review_state: Some("COMMENTED".into()),
            viewer_latest_review_commit: Some("b".repeat(40)),
            commits: vec![
                PrCommit {
                    oid: "a".repeat(40),
                    headline: "first".into(),
                    committed_at: None,
                },
                PrCommit {
                    oid: "b".repeat(40),
                    headline: "second".into(),
                    committed_at: None,
                },
            ],
            viewer_reviews: vec![
                ViewerReview {
                    ordinal: 1,
                    commit_oid: "9".repeat(40),
                    state: "COMMENTED".into(),
                    submitted_at: "2026-11-01".into(),
                },
                ViewerReview {
                    ordinal: 2,
                    commit_oid: "b".repeat(40),
                    state: "COMMENTED".into(),
                    submitted_at: "2026-11-02".into(),
                },
            ],
            ..ReviewContext::default()
        }
    }

    #[test]
    fn options_list_since_review_commits_newest_first_rebased_reviews_and_target() {
        let all = |_: &str| true;
        let options = base_options(&context(), &all);
        let values: Vec<&str> = options
            .iter()
            .map(|o| o["value"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            values,
            vec![
                "since-review".to_owned(),
                format!("commit:{}", "b".repeat(40)),
                format!("commit:{}", "a".repeat(40)),
                format!("review:{}", "9".repeat(40)),
                "whole".to_owned(),
            ]
        );
        assert_eq!(options[0]["label"], "Since your last review (Review 2)");
        assert_eq!(options[1]["tags"], json!(["Review 2"]));
        assert_eq!(options[3]["tags"], json!(["Review 1"]));
        assert_eq!(options[4]["label"], "main · target branch");
        assert_eq!(default_base(&context(), &all), "since-review");
        let none = |_: &str| false;
        assert_eq!(default_base(&context(), &none), "whole");
        assert_eq!(base_options(&context(), &none)[0]["available"], false);
    }

    #[test]
    fn standing_follows_the_pill_priority() {
        let mut c = context();
        assert_eq!(standing(Some(&c), 1), Some("pending"));
        assert_eq!(standing(Some(&c), 0), Some("commented"));
        c.viewer_has_review_request = true;
        assert_eq!(standing(Some(&c), 0), Some("requested"));
        c.viewer_latest_review_state = Some("APPROVED".into());
        assert_eq!(standing(Some(&c), 0), Some("approved"));
        c.viewer_latest_review_state = Some("CHANGES_REQUESTED".into());
        assert_eq!(standing(Some(&c), 0), Some("changes-requested"));
        assert_eq!(standing(None, 0), None);
    }

    #[test]
    fn comment_and_request_changes_need_a_draft_or_a_body_and_approve_never_does() {
        assert!(!can_submit(Verdict::Comment, "  ", 0));
        assert!(can_submit(Verdict::Comment, "x", 0));
        assert!(can_submit(Verdict::Comment, "", 1));
        assert!(can_submit(Verdict::Approve, "", 0));
        assert!(!can_submit(Verdict::RequestChanges, "", 0));
        assert!(can_submit(Verdict::RequestChanges, "", 1));
    }

    #[test]
    fn picks_are_validated() {
        assert!(valid_pick("since-review"));
        assert!(valid_pick("whole"));
        assert!(valid_pick(&format!("commit:{}", "a".repeat(40))));
        assert!(valid_pick("review:abcdef1"));
        assert!(!valid_pick("commit:xyz"));
        assert!(!valid_pick("main"));
    }

    #[test]
    fn a_file_draft_keeps_its_subject_through_storage() {
        let draft = Draft {
            id: "d".into(),
            path: "a.rs".into(),
            line: None,
            start_line: None,
            side: "RIGHT".into(),
            start_side: None,
            subject: "file".into(),
            body: "b".into(),
            created_at: 1,
            updated_at: None,
        };
        let session = Session {
            drafts: vec![draft.clone()],
            ..Session::default()
        };
        let back = Session::from_value(session.into_value());
        assert_eq!(back.drafts, vec![draft]);
        assert_eq!(back.drafts[0].to_github().target, CommentTarget::File);
    }

    #[test]
    fn ranges_keep_their_start_and_sides() {
        let draft = Draft {
            id: "d".into(),
            path: "a.rs".into(),
            line: Some(9),
            start_line: Some(4),
            side: "RIGHT".into(),
            start_side: Some("LEFT".into()),
            subject: "line".into(),
            body: "b".into(),
            created_at: 1,
            updated_at: None,
        };
        assert_eq!(
            draft.to_github().target,
            CommentTarget::Line {
                line: 9,
                side: Side::Right,
                start: Some((4, Side::Left)),
            }
        );
    }

    #[test]
    fn an_empty_session_is_deleted_not_stored() {
        assert_eq!(Session::default().into_value(), None);
        let summary_only = Session {
            summary: "x".into(),
            ..Session::default()
        };
        assert!(summary_only.into_value().is_some());
    }

    #[test]
    fn bodies_with_pending_uploads_or_too_long_are_refused() {
        assert!(check_body("![x](blob:abc)", false).is_err());
        assert!(check_body("", false).is_err());
        assert!(check_body("", true).is_ok());
        assert!(check_body(&"a".repeat(MAX_BODY_CHARS + 1), false).is_err());
    }

    fn thread(line: Option<u32>, outdated: bool, subject: &str) -> PrThread {
        PrThread {
            id: "T".into(),
            path: "a.rs".into(),
            side: "RIGHT".into(),
            start_side: None,
            subject: subject.into(),
            line,
            start_line: None,
            original_line: Some(7),
            original_start_line: None,
            is_resolved: false,
            is_outdated: outdated,
            viewer_can_resolve: true,
            viewer_can_unresolve: false,
            viewer_can_reply: true,
            comments: Vec::new(),
        }
    }

    #[test]
    fn threads_are_placed_on_their_line_or_stacked_as_outdated() {
        let on_line = thread_placement(&thread(Some(3), false, "line"), Some(10));
        assert_eq!(on_line["line"], 3);
        assert_eq!(on_line["outdated"], false);
        let gone = thread_placement(&thread(None, true, "line"), Some(10));
        assert_eq!(gone["outdated"], true);
        assert_eq!(gone["label"], "Outdated · was on line 7");
        let past_end = thread_placement(&thread(Some(30), false, "line"), Some(10));
        assert_eq!(past_end["outdated"], true);
        let file = thread_placement(&thread(None, false, "file"), Some(10));
        assert_eq!(file["file"], true);
        assert_eq!(file["outdated"], false);
    }
}
