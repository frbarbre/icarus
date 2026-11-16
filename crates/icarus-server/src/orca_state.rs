//! One-time import of Orca's per-workspace state (plan §4 "Workspace state"): statuses, display
//! names, linked Linear issues, diff notes and agent notes, matched to Icarus workspaces.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::notes::{NOTE_CHANGED_TOPIC, Note, NoteReply};
use crate::review::Draft;
use crate::server::{Server, params, rpc};
use crate::state::{Project, Workspace};

const IMPORTED_NOTE_KEY: &str = "orca_state.note:";
const IMPORTED_DRAFT_KEY: &str = "orca_state.draft:";

/// One Orca worktree entry (`worktreeMeta` key `<repoId>::<path>`), reduced to what can move over.
#[derive(Debug, Clone, PartialEq)]
pub struct OrcaWorktree {
    pub path: String,
    pub name: String,
    pub repo_remote: Option<String>,
    pub status: Option<String>,
    pub display_name: Option<String>,
    pub linear: Option<String>,
    pub notes: Vec<OrcaNote>,
    /// Unsent PR review comments from Orca's `pending-review-drafts.json`.
    pub drafts: Vec<OrcaDraft>,
    pub review_summary: Option<String>,
}

/// One queued review comment from Orca (`PendingReviewComment`).
#[derive(Debug, Clone, PartialEq)]
pub struct OrcaDraft {
    pub id: String,
    pub path: String,
    pub line: u32,
    pub start_line: Option<u32>,
    pub body: String,
    pub created_at: u64,
    pub file: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrcaNote {
    pub id: String,
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    pub side: Option<String>,
    pub body: String,
    pub agent: Option<String>,
    pub created_at: u64,
    pub reply_to: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub orca_worktrees: usize,
    pub matched: Vec<Matched>,
    pub unmatched: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Matched {
    pub workspace_id: String,
    pub workspace: String,
    pub matched_by: &'static str,
    pub status: Option<String>,
    pub title: Option<String>,
    pub linear: Option<String>,
    pub notes: usize,
    pub replies: usize,
    pub notes_already_imported: usize,
    pub drafts: usize,
    pub drafts_already_imported: usize,
    pub review_summary: bool,
    #[serde(skip)]
    pub new_notes: Vec<OrcaNote>,
    #[serde(skip)]
    pub new_drafts: Vec<OrcaDraft>,
    #[serde(skip)]
    pub summary_text: Option<String>,
}

/// Orca's documents: `worktreeMeta` (an object keyed by `<repoId>::<path>`) and `repos` (a list).
pub fn parse(worktree_meta: &Value, repos: &Value) -> Vec<OrcaWorktree> {
    parse_with_drafts(worktree_meta, repos, &Value::Null)
}

/// Orca's `pending-review-drafts.json` (`{version, drafts, summaries}`, keyed by worktree id).
fn parse_drafts(file: &Value) -> HashMap<String, (Vec<OrcaDraft>, Option<String>)> {
    let mut out: HashMap<String, (Vec<OrcaDraft>, Option<String>)> = HashMap::new();
    for (key, comments) in file["drafts"].as_object().into_iter().flatten() {
        let drafts = comments
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|comment| {
                let id = comment["id"].as_str()?.trim();
                let path = comment["path"].as_str()?.trim().trim_start_matches('/');
                let body = comment["body"].as_str()?;
                let line = comment["line"].as_f64().filter(|l| *l >= 1.0)? as u32;
                if id.is_empty() || path.is_empty() || body.trim().is_empty() {
                    return None;
                }
                Some(OrcaDraft {
                    id: id.to_owned(),
                    path: path.to_owned(),
                    line,
                    start_line: comment["startLine"]
                        .as_f64()
                        .map(|s| s as u32)
                        .filter(|s| *s >= 1 && *s < line),
                    body: body.to_owned(),
                    created_at: comment["createdAt"].as_f64().unwrap_or_default() as u64,
                    file: comment["subjectType"] == "file",
                })
            })
            .collect();
        out.entry(key.clone()).or_default().0 = drafts;
    }
    for (key, text) in file["summaries"].as_object().into_iter().flatten() {
        if let Some(text) = text.as_str().filter(|t| !t.trim().is_empty()) {
            out.entry(key.clone()).or_default().1 = Some(text.to_owned());
        }
    }
    out
}

pub fn parse_with_drafts(
    worktree_meta: &Value,
    repos: &Value,
    pending_review_drafts: &Value,
) -> Vec<OrcaWorktree> {
    let mut drafts = parse_drafts(pending_review_drafts);
    let remotes: HashMap<&str, String> = repos
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|repo| {
            let id = repo["id"].as_str()?;
            let remote = repo["gitRemoteIdentity"]["canonicalKey"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    repo["gitRemoteIdentity"]["remoteUrl"]
                        .as_str()
                        .and_then(icarus_git::canonical_remote)
                })?;
            Some((id, remote))
        })
        .collect();
    let mut out = Vec::new();
    let mut entries: Vec<(&String, &Value)> =
        worktree_meta.as_object().into_iter().flatten().collect();
    let mut draft_only: Vec<&String> = pending_review_drafts["drafts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, _)| key)
        .chain(
            pending_review_drafts["summaries"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(key, _)| key),
        )
        .filter(|key| worktree_meta.get(key.as_str()).is_none())
        .collect();
    draft_only.sort();
    draft_only.dedup();
    static NO_META: Value = Value::Null;
    entries.extend(draft_only.into_iter().map(|key| (key, &NO_META)));
    for (key, meta) in entries {
        let Some((repo_id, path)) = key.split_once("::") else {
            continue;
        };
        let (worktree_drafts, review_summary) = drafts.remove(key).unwrap_or_default();
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = |field: &str| {
            meta[field]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        let mut notes = Vec::new();
        for (field, agent) in [("diffComments", false), ("agentNotes", true)] {
            for note in meta[field].as_array().into_iter().flatten() {
                if let Some(note) = parse_note(note, path, agent) {
                    notes.push(note);
                }
            }
        }
        out.push(OrcaWorktree {
            path: path.to_owned(),
            name,
            repo_remote: remotes.get(repo_id).cloned(),
            status: text("workspaceStatus"),
            display_name: text("displayName").filter(|_| meta["displayNameIsPinned"] == true),
            linear: text("linkedLinearIssue"),
            notes,
            drafts: worktree_drafts,
            review_summary,
        });
    }
    out
}

fn parse_note(note: &Value, worktree: &str, agent: bool) -> Option<OrcaNote> {
    let id = note["id"].as_str()?.to_owned();
    let body = note["body"].as_str()?.trim().to_owned();
    let raw_path = note["filePath"].as_str()?;
    let path = Path::new(raw_path)
        .strip_prefix(worktree)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw_path.trim_start_matches('/').to_owned());
    let end = note["lineNumber"].as_u64()? as u32;
    let start = note["startLine"]
        .as_u64()
        .map_or(end, |s| s as u32)
        .min(end);
    if body.is_empty() || path.is_empty() || start == 0 {
        return None;
    }
    let side = note["side"].as_str().map(|side| match side {
        "left" | "old" | "base" => "old".to_owned(),
        _ => "new".to_owned(),
    });
    Some(OrcaNote {
        id,
        path,
        line: start,
        end_line: end,
        side,
        body,
        agent: agent.then(|| note["agentAuthor"].as_str().unwrap_or("agent").to_owned()),
        created_at: note["createdAt"].as_u64().unwrap_or_default(),
        reply_to: note["replyToNoteId"].as_str().map(str::to_owned),
    })
}

/// What importing would change; `imported` holds Orca note ids already brought over.
pub fn plan(
    orca: &[OrcaWorktree],
    workspaces: &[Workspace],
    projects: &[Project],
    imported: &HashSet<String>,
) -> Plan {
    let remote_of: HashMap<&str, String> = projects
        .iter()
        .filter_map(|p| {
            let remote = p.remote.as_deref()?;
            Some((
                p.id.as_str(),
                icarus_git::canonical_remote(remote).unwrap_or_else(|| remote.to_owned()),
            ))
        })
        .collect();
    let mut result = Plan {
        orca_worktrees: orca.len(),
        ..Plan::default()
    };
    let mut taken = HashSet::new();
    for entry in orca {
        let by_path = workspaces
            .iter()
            .find(|w| w.path == Path::new(&entry.path) && !taken.contains(&w.id));
        let by_name = || {
            workspaces.iter().find(|w| {
                !taken.contains(&w.id)
                    && entry.repo_remote.is_some()
                    && remote_of.get(w.project_id.as_str()) == entry.repo_remote.as_ref()
                    && (w.name == entry.name
                        || w.branch.as_deref() == Some(entry.name.as_str())
                        || w.name == crate::state::workspace_slug(&entry.name, 63))
            })
        };
        let (workspace, matched_by) = match by_path {
            Some(w) => (w, "path"),
            None => match by_name() {
                Some(w) => (w, "repo and name"),
                None => {
                    result.unmatched += 1;
                    continue;
                }
            },
        };
        taken.insert(workspace.id.clone());
        let status = entry
            .status
            .clone()
            .filter(|s| workspace.status.as_deref() != Some(s.as_str()));
        let title = entry
            .display_name
            .clone()
            .filter(|_| workspace.title.is_none());
        let linear = entry.linear.clone().filter(|_| workspace.issue.is_none());
        let new_notes: Vec<OrcaNote> = entry
            .notes
            .iter()
            .filter(|n| !imported.contains(&n.id))
            .cloned()
            .collect();
        let replies = new_notes.iter().filter(|n| n.reply_to.is_some()).count();
        let new_drafts: Vec<OrcaDraft> = entry
            .drafts
            .iter()
            .filter(|d| !imported.contains(&format!("draft:{}", d.id)))
            .cloned()
            .collect();
        result.matched.push(Matched {
            drafts: new_drafts.len(),
            drafts_already_imported: entry.drafts.len() - new_drafts.len(),
            review_summary: entry.review_summary.is_some(),
            new_drafts,
            summary_text: entry.review_summary.clone(),
            workspace_id: workspace.id.clone(),
            workspace: workspace.name.clone(),
            matched_by,
            status,
            title,
            linear,
            notes: new_notes.len() - replies,
            replies,
            notes_already_imported: entry.notes.len() - new_notes.len(),
            new_notes,
        });
    }
    result
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl Server {
    pub(crate) async fn orca_state_dispatch(self: &Arc<Self>, p: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            worktree_meta: Value,
            #[serde(default)]
            repos: Value,
            #[serde(default)]
            pending_review_drafts: Value,
            #[serde(default)]
            dry_run: bool,
        }
        let p: P = params(p)?;
        let internal = |e: anyhow::Error| rpc("internal", format!("{e:#}"));
        let orca = parse_with_drafts(&p.worktree_meta, &p.repos, &p.pending_review_drafts);
        let workspaces = self.store.workspaces(None).map_err(internal)?;
        let projects = self.store.projects().map_err(internal)?;
        let mut imported = HashSet::new();
        for entry in &orca {
            for note in &entry.notes {
                if self
                    .store
                    .kv_get(&format!("{IMPORTED_NOTE_KEY}{}", note.id))
                    .map_err(internal)?
                    .is_some()
                {
                    imported.insert(note.id.clone());
                }
            }
            for draft in &entry.drafts {
                if self
                    .store
                    .kv_get(&format!("{IMPORTED_DRAFT_KEY}{}", draft.id))
                    .map_err(internal)?
                    .is_some()
                {
                    imported.insert(format!("draft:{}", draft.id));
                }
            }
        }
        let plan = plan(&orca, &workspaces, &projects, &imported);
        let mut value = serde_json::to_value(&plan).map_err(|e| rpc("internal", e))?;
        if !p.dry_run {
            for (index, matched) in plan.matched.iter().enumerate() {
                self.apply_orca_state(matched).map_err(internal)?;
                match self.apply_orca_drafts(matched).await {
                    Ok(summary_set) => {
                        value["matched"][index]["review_summary"] = json!(summary_set)
                    }
                    Err(error) => {
                        value["matched"][index]["drafts_error"] = json!(error.message);
                        value["matched"][index]["review_summary"] = json!(false);
                    }
                }
            }
        }
        value["dry_run"] = json!(p.dry_run);
        Ok(value)
    }

    /// Queues Orca's unsent review comments on the workspace's pull request; ids already brought over are skipped.
    /// Returns whether the summary was set; one already written is kept.
    async fn apply_orca_drafts(&self, matched: &Matched) -> Result<bool, RpcError> {
        if matched.new_drafts.is_empty() && matched.summary_text.is_none() {
            return Ok(false);
        }
        let internal = |e: anyhow::Error| rpc("internal", format!("{e:#}"));
        let Some(workspace) = self
            .store
            .workspace(&matched.workspace_id)
            .map_err(internal)?
        else {
            return Ok(false);
        };
        let drafts: Vec<Draft> = matched
            .new_drafts
            .iter()
            .map(|draft| Draft {
                id: format!("draft-{}", uuid::Uuid::new_v4().simple()),
                path: draft.path.clone(),
                line: (!draft.file).then_some(draft.line),
                start_line: if draft.file { None } else { draft.start_line },
                side: "RIGHT".to_owned(),
                start_side: None,
                subject: if draft.file { "file" } else { "line" }.to_owned(),
                body: draft.body.clone(),
                created_at: if draft.created_at > 0 {
                    draft.created_at
                } else {
                    now_ms()
                },
                updated_at: None,
            })
            .collect();
        let summary_set = self
            .import_review_drafts(&workspace, drafts, matched.summary_text.clone())
            .await?;
        for draft in &matched.new_drafts {
            self.store
                .kv_set(&format!("{IMPORTED_DRAFT_KEY}{}", draft.id), &json!(true))
                .map_err(internal)?;
        }
        Ok(summary_set)
    }

    fn apply_orca_state(&self, matched: &Matched) -> anyhow::Result<()> {
        let Some(mut workspace) = self.store.workspace(&matched.workspace_id)? else {
            return Ok(());
        };
        let mut changed = false;
        if let Some(status) = &matched.status {
            workspace.status = Some(status.clone());
            changed = true;
        }
        if let Some(title) = &matched.title {
            workspace.title = Some(title.clone());
            changed = true;
        }
        if let Some(linear) = &matched.linear {
            workspace.issue = Some(json!({ "provider": "linear", "id": linear }));
            changed = true;
        }
        if changed {
            self.store.update_workspace(&workspace)?;
        }
        // Orca ids → Icarus note ids, so replies find their parent.
        let mut parents: HashMap<String, String> = HashMap::new();
        let (roots, replies): (Vec<&OrcaNote>, Vec<&OrcaNote>) =
            matched.new_notes.iter().partition(|n| n.reply_to.is_none());
        let now = now_ms();
        for note in roots {
            let id = format!("note-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
            let created = if note.created_at > 0 {
                note.created_at
            } else {
                now
            };
            self.store.add_note(&Note {
                id: id.clone(),
                workspace_id: workspace.id.clone(),
                path: note.path.clone(),
                line: note.line,
                end_line: note.end_line,
                side: note.side.clone(),
                base: None,
                author: if note.agent.is_some() {
                    "agent"
                } else {
                    "user"
                }
                .to_owned(),
                agent_id: note.agent.clone(),
                body: note.body.clone(),
                excerpt: None,
                created_at: created,
                updated_at: created,
                resolved: false,
                resolved_at: None,
                replies: Vec::new(),
            })?;
            self.store
                .kv_set(&format!("{IMPORTED_NOTE_KEY}{}", note.id), &json!(id))?;
            parents.insert(note.id.clone(), id);
        }
        for reply in replies {
            let parent = match reply.reply_to.as_ref().and_then(|p| parents.get(p)) {
                Some(parent) => parent.clone(),
                None => match reply.reply_to.as_ref().and_then(|p| {
                    self.store
                        .kv_get(&format!("{IMPORTED_NOTE_KEY}{p}"))
                        .ok()
                        .flatten()
                }) {
                    Some(Value::String(parent)) => parent,
                    _ => continue,
                },
            };
            let created = if reply.created_at > 0 {
                reply.created_at
            } else {
                now
            };
            let id = format!("reply-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
            self.store.add_note_reply(&NoteReply {
                id: id.clone(),
                note_id: parent,
                author: if reply.agent.is_some() {
                    "agent"
                } else {
                    "user"
                }
                .to_owned(),
                agent_id: reply.agent.clone(),
                body: reply.body.clone(),
                status: None,
                error: None,
                changed_files: Vec::new(),
                created_at: created,
                updated_at: created,
            })?;
            self.store
                .kv_set(&format!("{IMPORTED_NOTE_KEY}{}", reply.id), &json!(id))?;
        }
        self.broadcast(
            NOTE_CHANGED_TOPIC,
            json!({ "workspace": workspace.id, "id": null, "path": null, "removed": false }),
        );
        self.broadcast("workspace.changed", json!({ "workspaces": [workspace.id] }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace(id: &str, project: &str, name: &str, path: &str) -> Workspace {
        Workspace {
            id: id.into(),
            project_id: project.into(),
            name: name.into(),
            path: PathBuf::from(path),
            branch: Some(name.into()),
            base_ref: None,
            status: None,
            title: None,
            issue: None,
            pull_request: None,
            created_at_ms: 0,
            pinned: false,
            manual_order: None,
            activity_at_ms: None,
            discovery: Default::default(),
        }
    }

    fn project(id: &str, remote: &str) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            root: PathBuf::from("/repo"),
            remote: Some(remote.into()),
            base_ref: Some("main".into()),
            created_at_ms: 0,
        }
    }

    fn orca_documents() -> (Value, Value) {
        let meta = json!({
            "r1::/elsewhere/workspaces/abc-12-fix-login": {
                "workspaceStatus": "in-review",
                "displayName": "Fix login",
                "displayNameIsPinned": true,
                "linkedLinearIssue": "ABC-12",
                "diffComments": [
                    { "id": "c1", "filePath": "src/a.rs", "lineNumber": 4, "startLine": 2, "body": "Why?", "side": "right", "createdAt": 10 },
                    { "id": "c2", "filePath": "src/a.rs", "lineNumber": 4, "body": "Because.", "replyToNoteId": "c1", "createdAt": 11 },
                ],
                "agentNotes": [
                    { "id": "n1", "filePath": "/elsewhere/workspaces/abc-12-fix-login/src/b.rs", "lineNumber": 7, "body": "Look here", "agentAuthor": "claude" },
                ],
            },
            "r1::/elsewhere/workspaces/gone": { "workspaceStatus": "done" },
            "r2::/here/ws/exact": { "workspaceStatus": "todo", "displayName": "not pinned" },
        });
        let repos = json!([
            { "id": "r1", "gitRemoteIdentity": { "canonicalKey": "github.com/acme/app" } },
            { "id": "r2", "gitRemoteIdentity": { "remoteUrl": "git@github.com:acme/tools.git" } },
        ]);
        (meta, repos)
    }

    #[test]
    fn worktrees_match_by_path_or_by_repo_and_name() {
        let (meta, repos) = orca_documents();
        let orca = parse(&meta, &repos);
        assert_eq!(orca.len(), 3);
        let workspaces = [
            workspace(
                "w1",
                "p1",
                "abc-12-fix-login",
                "/icarus/app/abc-12-fix-login",
            ),
            workspace("w2", "p2", "exact", "/here/ws/exact"),
        ];
        let projects = [
            project("p1", "https://github.com/acme/app.git"),
            project("p2", "git@github.com:acme/tools.git"),
        ];
        let plan = plan(&orca, &workspaces, &projects, &HashSet::new());
        assert_eq!(plan.orca_worktrees, 3);
        assert_eq!(plan.unmatched, 1, "the deleted worktree has no workspace");
        let by_name = plan
            .matched
            .iter()
            .find(|m| m.workspace_id == "w1")
            .expect("w1");
        assert_eq!(by_name.matched_by, "repo and name");
        assert_eq!(by_name.status.as_deref(), Some("in-review"));
        assert_eq!(by_name.title.as_deref(), Some("Fix login"));
        assert_eq!(by_name.linear.as_deref(), Some("ABC-12"));
        assert_eq!((by_name.notes, by_name.replies), (2, 1));
        let note = &by_name.new_notes[0];
        assert_eq!((note.line, note.end_line), (2, 4));
        assert_eq!(note.side.as_deref(), Some("new"));
        let agent_note = by_name
            .new_notes
            .iter()
            .find(|n| n.id == "n1")
            .expect("agent note");
        assert_eq!(
            agent_note.path, "src/b.rs",
            "absolute paths become worktree-relative"
        );
        assert_eq!(agent_note.agent.as_deref(), Some("claude"));

        let exact = plan
            .matched
            .iter()
            .find(|m| m.workspace_id == "w2")
            .expect("w2");
        assert_eq!(exact.matched_by, "path");
        assert_eq!(exact.title, None, "an unpinned Orca name isn't a title");
    }

    #[test]
    fn existing_values_and_imported_notes_are_left_alone() {
        let (meta, repos) = orca_documents();
        let orca = parse(&meta, &repos);
        let mut w1 = workspace("w1", "p1", "abc-12-fix-login", "/x");
        w1.status = Some("in-review".into());
        w1.title = Some("Mine".into());
        w1.issue = Some(json!({ "provider": "linear", "id": "ABC-12" }));
        let imported: HashSet<String> = ["c1".to_owned(), "c2".to_owned()].into();
        let plan = plan(
            &orca,
            &[w1],
            &[project("p1", "github.com/acme/app")],
            &imported,
        );
        let matched = &plan.matched[0];
        assert_eq!(
            (
                matched.status.clone(),
                matched.title.clone(),
                matched.linear.clone()
            ),
            (None, None, None)
        );
        assert_eq!((matched.notes, matched.notes_already_imported), (1, 2));
    }

    #[test]
    fn pending_review_drafts_join_their_worktree_or_stand_alone() {
        let (meta, repos) = orca_documents();
        let drafts = json!({ "version": 1,
            "drafts": {
                "r1::/elsewhere/workspaces/abc-12-fix-login": [
                    { "id": "d1", "path": "src/a.rs", "line": 9, "startLine": 7, "body": "Rename?", "createdAt": 5 },
                    { "id": "d2", "path": "src/a.rs", "line": 0, "body": "bad line" },
                    { "id": "d3", "path": "src/b.rs", "line": 2, "body": "Whole file", "subjectType": "file" },
                ],
                "r2::/here/ws/drafts-only": [ { "id": "d4", "path": "x.rs", "line": 1, "body": "Hi" } ],
            },
            "summaries": { "r1::/elsewhere/workspaces/abc-12-fix-login": "Looks good overall" },
        });
        let orca = parse_with_drafts(&meta, &repos, &drafts);
        assert_eq!(orca.len(), 4, "a worktree with only drafts still counts");
        let login = orca
            .iter()
            .find(|w| w.name == "abc-12-fix-login")
            .expect("login");
        assert_eq!(
            login.drafts.len(),
            2,
            "a draft without a real line is dropped"
        );
        assert_eq!(
            (login.drafts[0].line, login.drafts[0].start_line),
            (9, Some(7))
        );
        assert!(login.drafts[1].file);
        assert_eq!(login.review_summary.as_deref(), Some("Looks good overall"));

        let workspaces = [workspace("w1", "p1", "abc-12-fix-login", "/x")];
        let imported: HashSet<String> = ["draft:d1".to_owned()].into();
        let plan = plan(
            &orca,
            &workspaces,
            &[project("p1", "github.com/acme/app")],
            &imported,
        );
        let matched = &plan.matched[0];
        assert_eq!((matched.drafts, matched.drafts_already_imported), (1, 1));
        assert!(matched.review_summary);
    }
}
