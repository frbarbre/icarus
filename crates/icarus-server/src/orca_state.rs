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
use crate::server::{Server, params, rpc};
use crate::state::{Project, Workspace};

const IMPORTED_NOTE_KEY: &str = "orca_state.note:";

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
    #[serde(skip)]
    pub new_notes: Vec<OrcaNote>,
}

/// Orca's documents: `worktreeMeta` (an object keyed by `<repoId>::<path>`) and `repos` (a list).
pub fn parse(worktree_meta: &Value, repos: &Value) -> Vec<OrcaWorktree> {
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
    for (key, meta) in worktree_meta.as_object().into_iter().flatten() {
        let Some((repo_id, path)) = key.split_once("::") else {
            continue;
        };
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
        result.matched.push(Matched {
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
    pub(crate) fn orca_state_dispatch(self: &Arc<Self>, p: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            worktree_meta: Value,
            #[serde(default)]
            repos: Value,
            #[serde(default)]
            dry_run: bool,
        }
        let p: P = params(p)?;
        let internal = |e: anyhow::Error| rpc("internal", format!("{e:#}"));
        let orca = parse(&p.worktree_meta, &p.repos);
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
        }
        let plan = plan(&orca, &workspaces, &projects, &imported);
        if !p.dry_run {
            for matched in &plan.matched {
                self.apply_orca_state(matched).map_err(internal)?;
            }
        }
        let mut value = serde_json::to_value(&plan).map_err(|e| rpc("internal", e))?;
        value["dry_run"] = json!(p.dry_run);
        Ok(value)
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
}
