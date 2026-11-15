//! `icarus acp`: an Agent Client Protocol (v1) agent on stdio, backed by server-owned agent sessions.
//!
//! An editor such as Zed starts this as a custom agent server. Sessions live on the Icarus server, so
//! when the editor quits only this bridge exits; the agent keeps working and can be loaded again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use icarus_client::{Client, Event};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

const TOOL_TEXT_LIMIT: usize = 8_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Streaming a turn as it happens: text comes from deltas.
    Live,
    /// Replaying history on `session/load`: text comes from complete messages.
    Replay,
}

/// Remembers tool inputs so results can be shown as diffs.
#[derive(Default)]
pub struct ToolState {
    inputs: HashMap<String, (String, Value)>,
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "Read" | "NotebookRead" => "read",
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "apply_patch" => "edit",
        "Bash" | "BashOutput" | "KillShell" => "execute",
        "Grep" | "Glob" | "LS" | "ToolSearch" => "search",
        "WebFetch" | "WebSearch" => "fetch",
        "TodoWrite" => "think",
        _ => "other",
    }
}

fn tool_title(name: &str, input: &Value) -> String {
    let path = input["file_path"]
        .as_str()
        .or(input["path"].as_str())
        .or(input["notebook_path"].as_str());
    match (name, path) {
        ("Bash", _) => input["description"]
            .as_str()
            .or(input["command"].as_str())
            .map(|c| c.lines().next().unwrap_or(c).to_string())
            .unwrap_or_else(|| "Run command".into()),
        ("apply_patch", Some(path)) => format!("Edit {path}"),
        ("apply_patch", None) => format!(
            "Edit {} files",
            input["changes"].as_array().map_or(0, Vec::len)
        ),
        (_, Some(path)) => format!("{name} {path}"),
        ("Grep" | "Glob", None) => {
            format!("{name} {}", input["pattern"].as_str().unwrap_or_default())
        }
        _ => name.to_string(),
    }
}

fn tool_content_from_input(name: &str, input: &Value) -> Vec<Value> {
    let path = input["file_path"].as_str();
    match (name, path) {
        ("Write", Some(path)) => vec![json!({ "type": "diff", "path": path, "oldText": null,
            "newText": input["content"].as_str().unwrap_or_default() })],
        ("Edit", Some(path)) => vec![json!({ "type": "diff", "path": path,
            "oldText": input["old_string"].as_str().unwrap_or_default(),
            "newText": input["new_string"].as_str().unwrap_or_default() })],
        ("MultiEdit", Some(path)) => input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|edit| {
                json!({ "type": "diff", "path": path,
                    "oldText": edit["old_string"].as_str().unwrap_or_default(),
                    "newText": edit["new_string"].as_str().unwrap_or_default() })
            })
            .collect(),
        ("apply_patch", _) => input["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(patch_content)
            .collect(),
        _ => Vec::new(),
    }
}

/// A Codex file change: added and deleted files as diffs, updates as their unified diff.
fn patch_content(change: &Value) -> Value {
    let path = change["path"].as_str().unwrap_or_default();
    let diff = change["diff"].as_str().unwrap_or_default();
    let kind = change["kind"]["type"]
        .as_str()
        .or(change["kind"].as_str())
        .unwrap_or_default();
    match kind {
        "add" => json!({ "type": "diff", "path": path, "oldText": null, "newText": diff }),
        "delete" => json!({ "type": "diff", "path": path, "oldText": diff, "newText": "" }),
        _ => {
            json!({ "type": "content", "content": text_block(&format!("{path}\n```diff\n{diff}\n```")) })
        }
    }
}

fn result_text(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if text.len() > TOOL_TEXT_LIMIT {
        let mut cut = TOOL_TEXT_LIMIT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n…", &text[..cut])
    } else {
        text
    }
}

fn plan_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "in_progress" => "in_progress",
        _ => "pending",
    }
}

/// Maps one server agent event to ACP `session/update` payloads.
pub fn event_to_updates(event: &Value, mode: Mode, tools: &mut ToolState) -> Vec<Value> {
    let data = &event["data"];
    let top_level = data["parent"].is_null();
    match event["kind"].as_str().unwrap_or_default() {
        "user_message" if mode == Mode::Replay => {
            vec![json!({ "sessionUpdate": "user_message_chunk", "content": text_block(data["text"].as_str().unwrap_or_default()) })]
        }
        "text_delta" if mode == Mode::Live && top_level => {
            vec![json!({ "sessionUpdate": "agent_message_chunk", "content": text_block(data["text"].as_str().unwrap_or_default()) })]
        }
        "thinking_delta" if mode == Mode::Live && top_level => {
            vec![json!({ "sessionUpdate": "agent_thought_chunk", "content": text_block(data["text"].as_str().unwrap_or_default()) })]
        }
        "message" if mode == Mode::Replay && top_level => data["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => Some(json!({ "sessionUpdate": "agent_message_chunk", "content": text_block(block["text"].as_str().unwrap_or_default()) })),
                Some("thinking") => Some(json!({ "sessionUpdate": "agent_thought_chunk", "content": text_block(block["thinking"].as_str().unwrap_or_default()) })),
                _ => None,
            })
            .collect(),
        "tool_call" => {
            let id = data["id"].as_str().unwrap_or_default().to_string();
            let name = data["name"].as_str().unwrap_or("tool").to_string();
            let input = data["input"].clone();
            tools.inputs.insert(id.clone(), (name.clone(), input.clone()));
            let mut update = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": tool_title(&name, &input),
                "kind": tool_kind(&name),
                "status": "in_progress",
                "rawInput": input,
            });
            if let Some(path) = input["file_path"].as_str().or(input["notebook_path"].as_str()) {
                update["locations"] = json!([{ "path": path }]);
            }
            vec![update]
        }
        "tool_result" => {
            let id = data["tool_use_id"].as_str().unwrap_or_default().to_string();
            let failed = data["is_error"] == json!(true);
            // Codex reports a file change's patches only once it completes, so its result carries the input.
            if let (Some(name), false) = (data["name"].as_str(), data["input"].is_null()) {
                tools
                    .inputs
                    .insert(id.clone(), (name.to_string(), data["input"].clone()));
            }
            let mut content = match tools.inputs.get(&id) {
                Some((name, input)) if !failed => tool_content_from_input(name, input),
                _ => Vec::new(),
            };
            if content.is_empty() {
                let text = result_text(&data["content"]);
                if !text.is_empty() {
                    content.push(json!({ "type": "content", "content": text_block(&text) }));
                }
            }
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "status": if failed { "failed" } else { "completed" },
                "content": content,
            })]
        }
        "plan" => vec![json!({
            "sessionUpdate": "plan",
            "entries": data["todos"].as_array().into_iter().flatten().map(|todo| json!({
                "content": todo["content"].as_str().unwrap_or_default(),
                "priority": "medium",
                "status": plan_status(todo["status"].as_str().unwrap_or_default()),
            })).collect::<Vec<_>>(),
        })],
        _ => Vec::new(),
    }
}

type Writer = mpsc::UnboundedSender<Value>;

struct Bridge {
    client: Arc<Client>,
    out: Writer,
    provider: Option<String>,
    /// Prompts waiting for their turn to end, by session id.
    pending: Mutex<HashMap<String, (Value, bool)>>,
    tools: Mutex<HashMap<String, ToolState>>,
    attached: Mutex<HashMap<String, ()>>,
    /// Sessions this bridge already handed out from `session/new`, so two new tabs never share an agent.
    claimed: Mutex<HashSet<String>>,
}

impl Bridge {
    fn notify(&self, session_id: &str, update: Value) {
        let _ = self.out.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": session_id, "update": update },
        }));
    }

    fn respond(&self, id: Value, result: Value) {
        let _ = self
            .out
            .send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn error(&self, id: Value, code: i64, message: String) {
        let _ = self.out.send(
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        );
    }

    /// Picks the session for a new editor thread: the workspace's idle agent if it never worked, else a new one.
    async fn new_session(&self, cwd: PathBuf) -> Result<String> {
        let mut claimed = self.claimed.lock().await;
        let agents = self.client.request("agent.list", json!({})).await?;
        let fresh = agents.as_array().into_iter().flatten().find(|a| {
            a["cwd"].as_str() == Some(&cwd.to_string_lossy())
                && a["turns"] == json!(0)
                && a["state"] != "exited"
                && a["id"].as_str().is_some_and(|id| !claimed.contains(id))
                && self
                    .provider
                    .as_deref()
                    .is_none_or(|p| a["provider"].as_str() == Some(p))
        });
        if let Some(id) = fresh.and_then(|agent| agent["id"].as_str()) {
            claimed.insert(id.to_string());
            return Ok(id.to_string());
        }
        let workspaces = self.client.request("workspace.list", json!({})).await?;
        let workspace = workspaces
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["path"].as_str() == Some(&cwd.to_string_lossy()))
            .and_then(|w| w["id"].as_str().map(str::to_string));
        let info = self
            .client
            .request(
                "agent.create",
                json!({ "cwd": cwd, "provider": self.provider, "workspace": workspace, "title": "Agent" }),
            )
            .await?;
        let id = info["id"].as_str().unwrap_or_default().to_string();
        claimed.insert(id.clone());
        Ok(id)
    }

    async fn attach(&self, session_id: &str, replay: bool) -> Result<u64> {
        let since = if replay {
            0
        } else {
            self.client
                .request("agent.info", json!({ "id": session_id }))
                .await?["next_seq"]
                .as_u64()
                .unwrap_or(0)
        };
        let attached = self
            .client
            .request("agent.attach", json!({ "id": session_id, "since": since }))
            .await?;
        if replay {
            let mut tools = ToolState::default();
            for event in attached["events"].as_array().into_iter().flatten() {
                for update in event_to_updates(event, Mode::Replay, &mut tools) {
                    self.notify(session_id, update);
                }
            }
            self.tools
                .lock()
                .await
                .insert(session_id.to_string(), tools);
        }
        self.attached
            .lock()
            .await
            .insert(session_id.to_string(), ());
        Ok(attached["next_seq"].as_u64().unwrap_or(0))
    }

    async fn on_server_event(&self, event: Event) {
        if event.topic != "agent.event" {
            return;
        }
        let session_id = event.payload["id"].as_str().unwrap_or_default().to_string();
        let agent_event = &event.payload["event"];
        let updates = {
            let mut tools = self.tools.lock().await;
            let state = tools.entry(session_id.clone()).or_default();
            event_to_updates(agent_event, Mode::Live, state)
        };
        for update in updates {
            self.notify(&session_id, update);
        }
        let kind = agent_event["kind"].as_str().unwrap_or_default();
        if (kind == "turn_end" || kind == "exit")
            && let Some((id, cancelled)) = self.pending.lock().await.remove(&session_id)
        {
            let stop = if cancelled { "cancelled" } else { "end_turn" };
            if kind == "exit" && !cancelled {
                self.error(id, -32603, "agent process exited".into());
            } else {
                self.respond(id, json!({ "stopReason": stop }));
            }
        }
    }

    async fn handle(self: &Arc<Self>, message: Value) {
        let id = message.get("id").cloned();
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let params = message["params"].clone();
        let Some(id) = id else {
            if method == "session/cancel" {
                let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                if let Some(entry) = self.pending.lock().await.get_mut(&session_id) {
                    entry.1 = true;
                }
                let _ = self
                    .client
                    .request("agent.interrupt", json!({ "id": session_id }))
                    .await;
            }
            return;
        };
        let result: Result<Option<Value>> = async {
            match method.as_str() {
                "initialize" => Ok(Some(json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
                        "sessionCapabilities": { "list": {} },
                    },
                    "authMethods": [],
                    "agentInfo": { "name": "icarus", "title": "Icarus", "version": env!("CARGO_PKG_VERSION") },
                }))),
                "authenticate" => Ok(Some(json!({}))),
                "session/new" => {
                    let cwd = PathBuf::from(params["cwd"].as_str().unwrap_or("."));
                    let session_id = self.new_session(cwd).await?;
                    self.attach(&session_id, false).await?;
                    Ok(Some(json!({ "sessionId": session_id })))
                }
                "session/load" | "session/resume" => {
                    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                    self.claimed.lock().await.insert(session_id.clone());
                    self.attach(&session_id, method == "session/load").await?;
                    Ok(Some(json!({})))
                }
                "session/list" => {
                    let agents = self.client.request("agent.list", json!({})).await?;
                    let cwd = params["cwd"].as_str().map(str::to_string);
                    let sessions: Vec<Value> = agents
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|a| cwd.as_deref().is_none_or(|c| a["cwd"].as_str() == Some(c)))
                        .map(|a| json!({ "sessionId": a["id"], "cwd": a["cwd"], "title": a["title"] }))
                        .collect();
                    Ok(Some(json!({ "sessions": sessions })))
                }
                "session/prompt" => {
                    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                    if !self.attached.lock().await.contains_key(&session_id) {
                        self.attach(&session_id, false).await?;
                    }
                    let mut text = Vec::new();
                    let mut images = Vec::new();
                    for block in params["prompt"].as_array().into_iter().flatten() {
                        match block["type"].as_str() {
                            Some("text") => text.push(block["text"].as_str().unwrap_or_default().to_string()),
                            Some("image") => images.push(json!({ "media_type": block["mimeType"], "data": block["data"] })),
                            Some("resource_link") => text.push(format!("@{}", block["uri"].as_str().unwrap_or_default())),
                            Some("resource") => {
                                let resource = &block["resource"];
                                text.push(format!(
                                    "<context uri=\"{}\">\n{}\n</context>",
                                    resource["uri"].as_str().unwrap_or_default(),
                                    resource["text"].as_str().unwrap_or_default()
                                ));
                            }
                            _ => {}
                        }
                    }
                    self.pending.lock().await.insert(session_id.clone(), (id.clone(), false));
                    if let Err(error) = self
                        .client
                        .request("agent.send", json!({ "id": session_id, "text": text.join("\n\n"), "images": images }))
                        .await
                    {
                        self.pending.lock().await.remove(&session_id);
                        return Err(error);
                    }
                    Ok(None)
                }
                other => Err(anyhow::anyhow!("method not found: {other}")),
            }
        }
        .await;
        match result {
            Ok(Some(value)) => self.respond(id, value),
            Ok(None) => {}
            Err(error) => {
                let code = if format!("{error}").starts_with("method not found") {
                    -32601
                } else {
                    -32603
                };
                self.error(id, code, format!("{error:#}"));
            }
        }
    }
}

pub async fn run(
    (client, mut events): (Arc<Client>, mpsc::UnboundedReceiver<icarus_client::Event>),
    provider: Option<String>,
) -> Result<()> {
    let (out, mut outbox) = mpsc::unbounded_channel::<Value>();
    let bridge = Arc::new(Bridge {
        client,
        out,
        provider,
        pending: Mutex::default(),
        tools: Mutex::default(),
        attached: Mutex::default(),
        claimed: Mutex::default(),
    });
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = outbox.recv().await {
            let line = format!("{message}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let listening = Arc::clone(&bridge);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            listening.on_server_event(event).await;
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let handling = Arc::clone(&bridge);
        tokio::spawn(async move { handling.handle(message).await });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::SessionUpdate;

    fn parse(update: &Value) -> SessionUpdate {
        serde_json::from_value(update.clone())
            .unwrap_or_else(|e| panic!("invalid ACP update {update}: {e}"))
    }

    #[test]
    fn live_text_and_thinking_stream_as_chunks() {
        let mut tools = ToolState::default();
        let text = event_to_updates(
            &json!({ "kind": "text_delta", "data": { "text": "hi", "parent": null } }),
            Mode::Live,
            &mut tools,
        );
        assert!(matches!(
            parse(&text[0]),
            SessionUpdate::AgentMessageChunk(_)
        ));
        let thought = event_to_updates(
            &json!({ "kind": "thinking_delta", "data": { "text": "hmm", "parent": null } }),
            Mode::Live,
            &mut tools,
        );
        assert!(matches!(
            parse(&thought[0]),
            SessionUpdate::AgentThoughtChunk(_)
        ));
        let subagent = event_to_updates(
            &json!({ "kind": "text_delta", "data": { "text": "x", "parent": "toolu_1" } }),
            Mode::Live,
            &mut tools,
        );
        assert!(
            subagent.is_empty(),
            "subagent text stays inside its tool call"
        );
        let message = event_to_updates(
            &json!({ "kind": "message", "data": { "content": [{ "type": "text", "text": "hi" }], "parent": null } }),
            Mode::Live,
            &mut tools,
        );
        assert!(
            message.is_empty(),
            "live text comes from deltas, not the final message"
        );
    }

    #[test]
    fn replay_uses_complete_messages_and_user_turns() {
        let mut tools = ToolState::default();
        let user = event_to_updates(
            &json!({ "kind": "user_message", "data": { "text": "fix it" } }),
            Mode::Replay,
            &mut tools,
        );
        assert!(matches!(
            parse(&user[0]),
            SessionUpdate::UserMessageChunk(_)
        ));
        let message = event_to_updates(
            &json!({ "kind": "message", "data": { "content": [{ "type": "thinking", "thinking": "plan" }, { "type": "text", "text": "done" }], "parent": null } }),
            Mode::Replay,
            &mut tools,
        );
        assert_eq!(message.len(), 2);
        assert!(
            event_to_updates(
                &json!({ "kind": "text_delta", "data": { "text": "d", "parent": null } }),
                Mode::Replay,
                &mut tools
            )
            .is_empty()
        );
    }

    #[test]
    fn edits_become_diffs_and_commands_become_execute_calls() {
        let mut tools = ToolState::default();
        let call = event_to_updates(
            &json!({ "kind": "tool_call", "data": { "id": "t1", "name": "Edit", "parent": null,
                "input": { "file_path": "/w/a.rs", "old_string": "a", "new_string": "b" } } }),
            Mode::Live,
            &mut tools,
        );
        match parse(&call[0]) {
            SessionUpdate::ToolCall(tool) => assert_eq!(tool.title, "Edit /w/a.rs"),
            other => panic!("{other:?}"),
        }
        let result = event_to_updates(
            &json!({ "kind": "tool_result", "data": { "tool_use_id": "t1", "content": "ok", "is_error": false } }),
            Mode::Live,
            &mut tools,
        );
        assert_eq!(result[0]["content"][0]["type"], "diff");
        assert!(matches!(
            parse(&result[0]),
            SessionUpdate::ToolCallUpdate(_)
        ));

        let bash = event_to_updates(
            &json!({ "kind": "tool_call", "data": { "id": "t2", "name": "Bash", "parent": null,
                "input": { "command": "cargo test", "description": "Run tests" } } }),
            Mode::Live,
            &mut tools,
        );
        assert_eq!(bash[0]["kind"], "execute");
        assert_eq!(bash[0]["title"], "Run tests");
        let failed = event_to_updates(
            &json!({ "kind": "tool_result", "data": { "tool_use_id": "t2", "content": [{ "type": "text", "text": "boom" }], "is_error": true } }),
            Mode::Live,
            &mut tools,
        );
        assert_eq!(failed[0]["status"], "failed");
        assert_eq!(failed[0]["content"][0]["content"]["text"], "boom");
        parse(&failed[0]);
    }

    #[test]
    fn codex_file_changes_render_from_their_completed_patches() {
        let mut tools = ToolState::default();
        let call = event_to_updates(
            &json!({ "kind": "tool_call", "data": { "id": "f1", "name": "apply_patch", "parent": null,
                "input": { "changes": [] } } }),
            Mode::Live,
            &mut tools,
        );
        assert_eq!(call[0]["kind"], "edit");
        parse(&call[0]);
        let result = event_to_updates(
            &json!({ "kind": "tool_result", "data": { "tool_use_id": "f1", "name": "apply_patch", "is_error": false,
                "content": "a.txt", "input": { "file_path": "/w/a.txt", "changes": [
                    { "path": "/w/a.txt", "kind": { "type": "add" }, "diff": "hi\n" },
                    { "path": "/w/b.txt", "kind": { "type": "update" }, "diff": "@@ -1 +1 @@\n-a\n+b" }
                ] } } }),
            Mode::Live,
            &mut tools,
        );
        assert_eq!(result[0]["content"][0]["type"], "diff");
        assert_eq!(result[0]["content"][0]["newText"], "hi\n");
        assert_eq!(result[0]["content"][1]["type"], "content");
        parse(&result[0]);
    }

    #[test]
    fn todo_writes_become_plans() {
        let mut tools = ToolState::default();
        let plan = event_to_updates(
            &json!({ "kind": "plan", "data": { "todos": [
                { "content": "a", "status": "completed" }, { "content": "b", "status": "in_progress" }, { "content": "c", "status": "pending" }
            ] } }),
            Mode::Live,
            &mut tools,
        );
        match parse(&plan[0]) {
            SessionUpdate::Plan(plan) => assert_eq!(plan.entries.len(), 3),
            other => panic!("{other:?}"),
        }
    }
}
