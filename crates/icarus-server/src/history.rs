//! Session history (plan D8): past agent threads from `threads/<id>.jsonl`, searchable over what the
//! user asked and what the agent answered, and resumable as live agents.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{AgentEvent, AgentInfo, AgentState, Meta};
use crate::server::{Server, params, rpc, to_value};

const DEFAULT_LIMIT: usize = 50;
const PROMPT_PREVIEW_CHARS: usize = 200;
const SNIPPET_RADIUS: usize = 60;
const LAST_MESSAGE_CHARS: usize = 300;
/// As the agent bridge caps a title taken from a prompt.
const TITLE_CHARS: usize = 200;

/// One saved session, folded from its log as the log grows.
#[derive(Debug, Clone, Default)]
struct Indexed {
    id: String,
    provider: String,
    title: Option<String>,
    workspace: Option<String>,
    account: Option<String>,
    cwd: String,
    created_at_ms: u64,
    last_activity_ms: u64,
    turns: u64,
    first_prompt: Option<String>,
    provider_session: Option<String>,
    killed: bool,
    prompt_title: Option<String>,
    last_message: Option<String>,
    messages: u64,
    /// The tool calls whose subagents wrote into this session.
    subagents: BTreeSet<String>,
    model: Option<String>,
    /// Lowercased user prompts and top-level assistant text, one entry per message.
    texts: Vec<String>,
    /// Bytes of the log already folded in.
    offset: u64,
}

impl Indexed {
    fn fold(&mut self, event: &AgentEvent) {
        self.last_activity_ms = self.last_activity_ms.max(event.at_ms);
        match event.kind.as_str() {
            "session" => {
                if let Some(session) = event.data["session_id"].as_str() {
                    self.provider_session = Some(session.to_owned());
                }
                if let Some(model) = event.data["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
            }
            "model_changed" => {
                if let Some(model) = event.data["model"].as_str() {
                    self.model = Some(model.to_owned());
                }
            }
            "user_message" => {
                self.turns += 1;
                self.messages += 1;
                let text = event.data["text"].as_str().unwrap_or_default();
                if self.first_prompt.is_none() {
                    self.first_prompt = Some(text.chars().take(PROMPT_PREVIEW_CHARS).collect());
                }
                if self.prompt_title.is_none() {
                    self.prompt_title = title_from_prompt(text);
                }
                self.texts.push(text.to_lowercase());
            }
            "message" if event.data["parent"].is_null() => {
                self.messages += 1;
                let mut said = Vec::new();
                for block in event.data["content"].as_array().into_iter().flatten() {
                    if block["type"] == "text" {
                        let text = block["text"].as_str().unwrap_or_default();
                        self.texts.push(text.to_lowercase());
                        if !text.trim().is_empty() {
                            said.push(text.trim());
                        }
                    }
                }
                if !said.is_empty() {
                    let text = said.join(" ");
                    let mut last: String = text.chars().take(LAST_MESSAGE_CHARS).collect();
                    if last.len() < text.len() {
                        last.push('…');
                    }
                    self.last_message = Some(last);
                }
            }
            "message" | "tool_call" => {
                if let Some(parent) = event.data["parent"].as_str() {
                    self.subagents.insert(parent.to_owned());
                }
            }
            "kill" => self.killed = true,
            "wake" => self.killed = false,
            _ => {}
        }
    }

    /// The first text holding every term, as a short excerpt around the first term.
    fn matching(&self, terms: &[String]) -> Option<String> {
        if terms.is_empty() {
            return Some(String::new());
        }
        let all = self.texts.join("\n");
        if !terms.iter().all(|t| all.contains(t.as_str())) {
            return None;
        }
        let text = self
            .texts
            .iter()
            .find(|text| text.contains(terms[0].as_str()))?;
        let at = text.find(terms[0].as_str())?;
        let start = text[..at]
            .char_indices()
            .rev()
            .nth(SNIPPET_RADIUS)
            .map_or(0, |(i, _)| i);
        let end = text[at..]
            .char_indices()
            .nth(SNIPPET_RADIUS + terms[0].len())
            .map_or(text.len(), |(i, _)| at + i);
        Some(text[start..end].replace('\n', " "))
    }

    fn title(&self) -> Option<String> {
        session_title(
            self.title.as_deref(),
            &self.provider,
            self.prompt_title.clone(),
        )
    }
}

/// A title someone chose (an automation, a launch), else the one the agent tab takes from the
/// first prompt; new sessions are all titled "Agent".
pub(crate) fn session_title(
    chosen: Option<&str>,
    provider: &str,
    prompt_title: Option<String>,
) -> Option<String> {
    let provider = match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        other => other,
    };
    chosen
        .map(str::trim)
        .filter(|title| !title.is_empty() && *title != "Agent" && *title != provider)
        .map(str::to_owned)
        .or(prompt_title)
}

/// The first line of what was asked, without attached context, as the agent bridge names a tab.
/// Slash commands name nothing.
pub(crate) fn title_from_prompt(text: &str) -> Option<String> {
    let mut in_context = false;
    let line = text.lines().map(str::trim).find(|line| {
        if in_context {
            in_context = *line != "</context>";
            return false;
        }
        if line.starts_with("<context ") {
            in_context = !line.ends_with("</context>");
            return false;
        }
        !line.is_empty()
    })?;
    if line.starts_with('/') {
        return None;
    }
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TITLE_CHARS {
        return Some(collapsed);
    }
    let cut: String = collapsed.chars().take(TITLE_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub id: String,
    pub title: Option<String>,
    pub provider: String,
    pub workspace: Option<String>,
    /// The workspace's project, by name.
    pub project: Option<String>,
    pub account: Option<String>,
    pub cwd: String,
    pub first_prompt: Option<String>,
    /// The agent's last top-level answer, cut short.
    pub last_message: Option<String>,
    pub created_at_ms: u64,
    pub last_activity_ms: u64,
    pub turns: u64,
    /// User prompts and top-level agent messages.
    pub messages: u64,
    pub subagents: u64,
    pub model: Option<String>,
    /// `sleeping`, `exited` or `killed`.
    pub state: String,
    pub resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    pub workspace: Option<String>,
    /// A project (id, name or remote), or any of its workspaces: sessions of all its workspaces.
    pub project: Option<String>,
    /// The project's workspace ids, resolved from `project`.
    #[serde(skip)]
    pub workspaces: Option<BTreeSet<String>>,
    pub query: Option<String>,
    pub limit: Option<usize>,
    /// Only sessions whose last activity is older than this (ms), for paging.
    pub before: Option<u64>,
}

/// An in-memory index over the saved sessions, brought up to date before each query.
#[derive(Default)]
pub struct HistoryIndex {
    entries: Mutex<BTreeMap<String, Indexed>>,
}

fn read_meta(path: &Path) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

impl HistoryIndex {
    /// Folds in new sessions and the new tail of each log.
    pub fn refresh(&self, dir: &Path) {
        let Ok(listing) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        for file in listing.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".meta.json") else {
                continue;
            };
            if !entries.contains_key(id) {
                let Some(meta) = read_meta(&file.path()) else {
                    continue;
                };
                entries.insert(
                    id.to_owned(),
                    Indexed {
                        id: meta.id,
                        provider: meta.options.provider,
                        title: meta.options.title,
                        // Note reply sessions run outside the workspace's agents but belong to its history.
                        workspace: meta
                            .options
                            .workspace
                            .or_else(|| meta.options.env.get("ICARUS_WORKSPACE_ID").cloned()),
                        account: meta.options.account,
                        cwd: meta.options.cwd.to_string_lossy().into_owned(),
                        created_at_ms: meta.created_at_ms,
                        last_activity_ms: meta.created_at_ms,
                        ..Indexed::default()
                    },
                );
            }
            if let Some(entry) = entries.get_mut(id) {
                fold_log(entry, &dir.join(format!("{id}.jsonl")));
            }
        }
    }

    pub fn query(
        &self,
        query: &HistoryQuery,
        live: &BTreeMap<String, AgentInfo>,
    ) -> Vec<HistoryEntry> {
        let terms: Vec<String> = query
            .query
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let mut found: Vec<HistoryEntry> = entries
            .values()
            .filter(|e| e.turns > 0)
            .filter(|e| query.workspace.is_none() || e.workspace == query.workspace)
            .filter(|e| {
                query.workspaces.as_ref().is_none_or(|workspaces| {
                    e.workspace
                        .as_ref()
                        .is_some_and(|id| workspaces.contains(id))
                })
            })
            .filter(|e| {
                query
                    .before
                    .is_none_or(|before| e.last_activity_ms < before)
            })
            .filter_map(|e| {
                let info = live.get(&e.id);
                let state = match info.map(|i| i.state) {
                    Some(AgentState::Starting | AgentState::Idle | AgentState::Working) => {
                        return None;
                    }
                    Some(AgentState::Sleeping) => "sleeping",
                    _ if e.killed => "killed",
                    _ => "exited",
                };
                let snippet = e.matching(&terms)?;
                Some(HistoryEntry {
                    id: e.id.clone(),
                    title: e.title(),
                    provider: e.provider.clone(),
                    workspace: e.workspace.clone(),
                    project: None,
                    account: e.account.clone(),
                    cwd: e.cwd.clone(),
                    first_prompt: e.first_prompt.clone(),
                    last_message: e.last_message.clone(),
                    created_at_ms: e.created_at_ms,
                    last_activity_ms: info.map_or(e.last_activity_ms, |i| {
                        i.last_activity_ms.max(e.last_activity_ms)
                    }),
                    turns: e.turns,
                    messages: e.messages,
                    subagents: e.subagents.len() as u64,
                    model: e.model.clone(),
                    state: state.to_owned(),
                    resumable: e.provider_session.is_some(),
                    snippet: (!terms.is_empty()).then_some(snippet),
                })
            })
            .collect();
        found.sort_by(|a, b| {
            b.last_activity_ms
                .cmp(&a.last_activity_ms)
                .then(a.id.cmp(&b.id))
        });
        found.truncate(query.limit.unwrap_or(DEFAULT_LIMIT).max(1));
        found
    }
}

fn fold_log(entry: &mut Indexed, path: &Path) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    if len < entry.offset {
        // A log that shrank was rewritten; start over.
        *entry = Indexed {
            id: std::mem::take(&mut entry.id),
            provider: std::mem::take(&mut entry.provider),
            title: entry.title.take(),
            workspace: entry.workspace.take(),
            account: entry.account.take(),
            cwd: std::mem::take(&mut entry.cwd),
            created_at_ms: entry.created_at_ms,
            last_activity_ms: entry.created_at_ms,
            ..Indexed::default()
        };
    }
    if len == entry.offset || file.seek(SeekFrom::Start(entry.offset)).is_err() {
        return;
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        return;
    }
    // Only whole lines; a line still being written is read next time.
    let Some(end) = tail.iter().rposition(|b| *b == b'\n') else {
        return;
    };
    for line in tail[..end].split(|b| *b == b'\n') {
        if let Ok(event) = serde_json::from_slice::<AgentEvent>(line) {
            entry.fold(&event);
        }
    }
    entry.offset += end as u64 + 1;
}

impl Server {
    pub(crate) async fn history_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "agent.history" => {
                let mut query: HistoryQuery = params(p)?;
                if let Some(key) = &query.workspace
                    && let Ok(Some(workspace)) = self.store.workspace(key)
                {
                    query.workspace = Some(workspace.id);
                }
                if let Some(key) = &query.project {
                    let project = match self.store.project(key) {
                        Ok(Some(project)) => Some(project.id),
                        _ => self
                            .store
                            .workspace(key)
                            .ok()
                            .flatten()
                            .map(|w| w.project_id),
                    };
                    query.workspaces = Some(
                        project
                            .and_then(|id| self.store.all_workspaces(Some(&id)).ok())
                            .unwrap_or_default()
                            .into_iter()
                            .map(|w| w.id)
                            .collect(),
                    );
                }
                let Some(dir) = self.agents.log_dir().map(Path::to_path_buf) else {
                    return Ok(json!([]));
                };
                let index = Arc::clone(&self.history);
                let live: BTreeMap<String, AgentInfo> = self
                    .agents
                    .list()
                    .into_iter()
                    .map(|info| (info.id.clone(), info))
                    .collect();
                let mut entries = tokio::task::spawn_blocking(move || {
                    index.refresh(&dir);
                    index.query(&query, &live)
                })
                .await
                .map_err(|e| rpc("internal", e))?;
                if entries.iter().any(|e| e.workspace.is_some()) {
                    let projects: HashMap<String, String> = self
                        .store
                        .projects()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|p| (p.id, p.name))
                        .collect();
                    let names: HashMap<String, String> = self
                        .store
                        .all_workspaces(None)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|w| Some((w.id, projects.get(&w.project_id)?.clone())))
                        .collect();
                    for entry in &mut entries {
                        entry.project = entry
                            .workspace
                            .as_ref()
                            .and_then(|id| names.get(id).cloned());
                    }
                }
                to_value(entries)
            }
            "agent.resume" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let thread = match self.agents.get(&p.id) {
                    Some(thread) if thread.state() != AgentState::Exited => thread,
                    _ => self
                        .agents
                        .restore_one(&p.id)
                        .map_err(|e| rpc("not_found", format!("{e:#}")))?,
                };
                if thread.state() == AgentState::Exited {
                    return Err(rpc(
                        "not_resumable",
                        format!("agent {} never started a provider session", p.id),
                    ));
                }
                if !thread.options().cwd.is_dir() {
                    return Err(rpc(
                        "workspace_gone",
                        format!("{} no longer exists", thread.options().cwd.display()),
                    ));
                }
                thread
                    .wake()
                    .await
                    .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
                to_value(thread.info())
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, id: &str, workspace: Option<&str>, events: &[(&str, Value)]) {
        let meta = json!({
            "id": id,
            "created_at_ms": 1,
            "options": {
                "provider": "claude", "cwd": "/w", "workspace": workspace, "title": "Agent",
                "model": null, "bypass_permissions": true, "resume": null, "command": null,
            },
        });
        std::fs::write(dir.join(format!("{id}.meta.json")), meta.to_string()).expect("meta");
        append(dir, id, 0, events);
    }

    fn append(dir: &Path, id: &str, first_seq: u64, events: &[(&str, Value)]) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{id}.jsonl")))
            .expect("log");
        for (i, (kind, data)) in events.iter().enumerate() {
            let seq = first_seq + i as u64;
            let event = AgentEvent {
                seq,
                at_ms: 100 + seq,
                kind: (*kind).to_owned(),
                data: data.clone(),
            };
            writeln!(file, "{}", serde_json::to_string(&event).expect("json")).expect("write");
        }
    }

    fn turn(prompt: &str, answer: &str) -> Vec<(&'static str, Value)> {
        vec![
            ("user_message", json!({ "text": prompt })),
            (
                "message",
                json!({ "parent": null, "content": [{ "type": "text", "text": answer }] }),
            ),
            ("turn_end", json!({ "is_error": false })),
        ]
    }

    fn ids(entries: &[HistoryEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn searches_prompts_and_answers_and_filters_by_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let mut login = vec![("session", json!({ "session_id": "s1" }))];
        login.extend(turn("Fix the login redirect", "Patched the OAuth callback"));
        write_session(dir.path(), "agent-a", Some("ws-1"), &login);
        write_session(
            dir.path(),
            "agent-b",
            Some("ws-2"),
            &turn("Bump dependencies", "Updated serde"),
        );
        write_session(dir.path(), "agent-empty", Some("ws-1"), &[]);
        let index = HistoryIndex::default();
        index.refresh(dir.path());
        let live = BTreeMap::new();
        let all = index.query(&HistoryQuery::default(), &live);
        assert_eq!(
            ids(&all),
            ["agent-a", "agent-b"],
            "empty sessions are left out"
        );
        assert_eq!(
            all[0].first_prompt.as_deref(),
            Some("Fix the login redirect")
        );
        assert!(all[0].resumable && !all[1].resumable);
        assert_eq!(all[0].state, "exited");

        let search = |q: &str| {
            index.query(
                &HistoryQuery {
                    query: Some(q.into()),
                    ..HistoryQuery::default()
                },
                &live,
            )
        };
        let oauth = search("OAUTH callback");
        assert_eq!(ids(&oauth), ["agent-a"], "assistant text, any case");
        assert!(
            oauth[0]
                .snippet
                .as_deref()
                .unwrap_or_default()
                .contains("oauth callback")
        );
        assert_eq!(
            ids(&search("bump serde")),
            ["agent-b"],
            "all terms, across messages"
        );
        assert!(search("nothing-like-this").is_empty());

        let ws = index.query(
            &HistoryQuery {
                workspace: Some("ws-2".into()),
                ..HistoryQuery::default()
            },
            &live,
        );
        assert_eq!(ids(&ws), ["agent-b"]);
        let page = index.query(
            &HistoryQuery {
                before: Some(all[0].last_activity_ms),
                limit: Some(1),
                ..HistoryQuery::default()
            },
            &live,
        );
        assert_eq!(page.len(), 1);
        assert!(page[0].last_activity_ms < all[0].last_activity_ms);
    }

    #[test]
    fn entries_carry_title_last_answer_counts_and_model() {
        let dir = tempfile::tempdir().expect("dir");
        let mut events = vec![(
            "session",
            json!({ "session_id": "s1", "model": "claude-opus" }),
        )];
        events.extend(turn(
            "<context path=\"a.rs\">\nfn a() {}\n</context>\nFix the login\nredirect please",
            "Looking",
        ));
        events.extend([
            (
                "tool_call",
                json!({ "id": "t1", "name": "Task", "input": {}, "parent": null }),
            ),
            (
                "message",
                json!({ "parent": "t1", "content": [{ "type": "text", "text": "sub" }] }),
            ),
            (
                "tool_call",
                json!({ "id": "t2", "name": "Read", "input": {}, "parent": "t1" }),
            ),
            (
                "message",
                json!({ "parent": "t3", "content": [{ "type": "text", "text": "other sub" }] }),
            ),
            ("model_changed", json!({ "model": "claude-sonnet" })),
        ]);
        let long = "word ".repeat(100);
        events.extend(turn("/compact", &long));
        write_session(dir.path(), "agent-a", Some("ws-1"), &events);
        write_session(dir.path(), "agent-b", Some("ws-2"), &turn("Bump", "Done"));
        write_session(dir.path(), "agent-c", Some("ws-3"), &turn("Other", "Done"));
        let index = HistoryIndex::default();
        index.refresh(dir.path());
        let live = BTreeMap::new();
        let all = index.query(&HistoryQuery::default(), &live);
        let a = all.iter().find(|e| e.id == "agent-a").expect("agent-a");
        assert_eq!(
            a.title.as_deref(),
            Some("Fix the login"),
            "the first prompt's first line, without context, names an untitled session"
        );
        assert_eq!(a.messages, 4, "two prompts and two top-level answers");
        assert_eq!(a.subagents, 2);
        assert_eq!(a.model.as_deref(), Some("claude-sonnet"));
        let last = a.last_message.as_deref().expect("last message");
        assert!(last.starts_with("word word") && last.ends_with('…'));
        assert_eq!(last.chars().count(), LAST_MESSAGE_CHARS + 1);

        let project = index.query(
            &HistoryQuery {
                workspaces: Some(BTreeSet::from(["ws-1".to_owned(), "ws-2".to_owned()])),
                ..HistoryQuery::default()
            },
            &live,
        );
        let mut found = ids(&project);
        found.sort();
        assert_eq!(found, ["agent-a", "agent-b"]);
    }

    #[test]
    fn a_chosen_title_wins_over_the_prompt() {
        let dir = tempfile::tempdir().expect("dir");
        write_session(dir.path(), "agent-a", None, &turn("Fix it", "ok"));
        let meta_path = dir.path().join("agent-a.meta.json");
        let mut meta: Value =
            serde_json::from_slice(&std::fs::read(&meta_path).expect("meta")).expect("json");
        meta["options"]["title"] = json!("Nightly triage");
        std::fs::write(&meta_path, meta.to_string()).expect("meta");
        let index = HistoryIndex::default();
        index.refresh(dir.path());
        let entry = &index.query(&HistoryQuery::default(), &BTreeMap::new())[0];
        assert_eq!(entry.title.as_deref(), Some("Nightly triage"));
    }

    #[test]
    fn logs_are_folded_incrementally_and_states_come_from_live_agents() {
        let dir = tempfile::tempdir().expect("dir");
        write_session(dir.path(), "agent-a", None, &turn("first", "one"));
        let index = HistoryIndex::default();
        index.refresh(dir.path());
        let live = BTreeMap::new();
        assert_eq!(index.query(&HistoryQuery::default(), &live)[0].turns, 1);

        let mut more = turn("second question", "two");
        more.push(("kill", json!({})));
        append(dir.path(), "agent-a", 3, &more);
        index.refresh(dir.path());
        let entry = &index.query(&HistoryQuery::default(), &live)[0];
        assert_eq!(entry.turns, 2);
        assert_eq!(entry.state, "killed");
        let found = index.query(
            &HistoryQuery {
                query: Some("second".into()),
                ..HistoryQuery::default()
            },
            &live,
        );
        assert_eq!(found.len(), 1, "new text is searchable");

        let info = |state| AgentInfo {
            id: "agent-a".into(),
            provider: "claude".into(),
            title: None,
            workspace: None,
            cwd: "/w".into(),
            state,
            provider_session: None,
            model: None,
            mode: "default".into(),
            turns: 2,
            created_at_ms: 1,
            last_activity_ms: 9_999,
            next_seq: 0,
            rate_limits: None,
            rate_limits_at_ms: None,
            account: None,
            remote_control: false,
            remote_control_url: None,
        };
        let sleeping = BTreeMap::from([("agent-a".to_owned(), info(AgentState::Sleeping))]);
        let entry = &index.query(&HistoryQuery::default(), &sleeping)[0];
        assert_eq!(entry.state, "sleeping");
        assert_eq!(entry.last_activity_ms, 9_999);
        let working = BTreeMap::from([("agent-a".to_owned(), info(AgentState::Working))]);
        assert!(
            index.query(&HistoryQuery::default(), &working).is_empty(),
            "live sessions are not history"
        );
    }
}
