//! `icarus acp`: an Agent Client Protocol (v1) agent on stdio, backed by server-owned agent sessions.
//!
//! An editor such as Zed starts this as a custom agent server. Sessions live on the Icarus server, so
//! when the editor quits only this bridge exits; the agent keeps working and can be loaded again.

use std::collections::{HashMap, HashSet, VecDeque};
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
        "user_message" => {
            let mut update = json!({ "sessionUpdate": "user_message_chunk", "content": text_block(data["text"].as_str().unwrap_or_default()) });
            // Without an id, back-to-back prompts would merge into one message in the editor.
            if let Some(seq) = event["seq"].as_u64() {
                update["messageId"] = json!(format!("icarus-user-{seq}"));
            }
            vec![update]
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

/// Matches the editor's `acp_thread::TYPED_ANSWER_OPTION_ID`.
const TYPED_ANSWER: &str = "answer:other";
const MODE_OPTION: &str = "mode";
const MODEL_OPTION: &str = "model";
/// Icarus's own command: Claude Code has no Remote Control in headless mode, so Icarus restarts it with it.
const REMOTE_CONTROL_COMMAND: &str = "remote-control";

fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        "" => "Agent",
        other => other,
    }
}

/// `agent.modes` and `agent.models` as ACP session config options (mode and model categories),
/// plus the older `modes` state for clients without config options.
pub fn session_state(modes: &Value, models: &Value) -> Value {
    let provider = provider_label(models["provider"].as_str().unwrap_or_default());
    let current_mode = modes["current"].as_str().unwrap_or("default");
    let mut mode_options: Vec<Value> = modes["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|mode| json!({ "value": mode["id"], "name": mode["name"], "description": mode["description"] }))
        .collect();
    if !mode_options
        .iter()
        .any(|option| option["value"] == current_mode)
    {
        mode_options.push(json!({ "value": current_mode, "name": current_mode }));
    }
    let current_model = models["current"].as_str().unwrap_or("default");
    let mut model_options: Vec<Value> = models["models"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|model| {
            json!({
                "value": model["id"],
                "name": format!("{provider} {}", model["name"].as_str().unwrap_or_default()),
            })
        })
        .collect();
    if !model_options
        .iter()
        .any(|option| option["value"] == current_model)
    {
        model_options
            .push(json!({ "value": current_model, "name": format!("{provider} {current_model}") }));
    }
    json!({
        "configOptions": [
            { "id": MODE_OPTION, "name": "Mode", "category": "mode", "type": "select",
              "currentValue": current_mode, "options": mode_options },
            { "id": MODEL_OPTION, "name": "Model", "category": "model", "type": "select",
              "currentValue": current_model, "options": model_options },
        ],
        "modes": {
            "currentModeId": current_mode,
            "availableModes": mode_options.iter().map(|option| json!({
                "id": option["value"], "name": option["name"], "description": option["description"],
            })).collect::<Vec<_>>(),
        },
    })
}

/// `agent.commands` as ACP available commands; Claude Code agents also get Icarus's `/remote-control`.
pub fn available_commands(listed: &Value) -> Vec<Value> {
    let mut commands: Vec<Value> = listed["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|command| {
            let mut available = json!({
                "name": command["name"],
                "description": command["description"].as_str().unwrap_or_default(),
            });
            if let Some(hint) = command["hint"].as_str() {
                available["input"] = json!({ "hint": hint });
            }
            available
        })
        .collect();
    if listed["provider"] == "claude"
        && !commands
            .iter()
            .any(|command| command["name"] == REMOTE_CONTROL_COMMAND)
    {
        commands.push(json!({
            "name": REMOTE_CONTROL_COMMAND,
            "description": "Follow this session on claude.ai or in the Claude app (restarts the agent on its session)",
            "input": { "hint": "on | off" },
        }));
    }
    commands
}

/// The `/remote-control` argument: `on`, `off`, or nothing to switch it.
fn remote_control_request(text: &str) -> Option<Option<bool>> {
    let rest = text
        .trim()
        .strip_prefix('/')?
        .strip_prefix(REMOTE_CONTROL_COMMAND)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim() {
        "" => Some(None),
        "on" => Some(Some(true)),
        "off" => Some(Some(false)),
        _ => None,
    }
}

/// Longest title taken from a prompt; tabs cut it shorter and show it whole in their tooltip.
const TITLE_LIMIT: usize = 200;

/// Names a conversation after its first prompt, as Orca seeds a chat's name: the first line of
/// what was asked, without attached context. Slash commands name nothing.
pub fn title_from_prompt(text: &str) -> Option<String> {
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
    if collapsed.chars().count() <= TITLE_LIMIT {
        return Some(collapsed);
    }
    let cut: String = collapsed.chars().take(TITLE_LIMIT - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// The server's title when someone chose it (an automation, a launch); new sessions are all "Agent".
pub fn chosen_title(info: &Value) -> Option<String> {
    let title = info["title"].as_str()?.trim();
    let provider = provider_label(info["provider"].as_str().unwrap_or_default());
    (!title.is_empty() && title != "Agent" && title != provider).then(|| title.to_string())
}

/// The session's title and, in `_meta.icarus.provider`, the harness running it, which the editor
/// uses for the tab's name and logo.
pub fn session_info_update(provider: &str, title: Option<&str>) -> Value {
    let mut update = json!({
        "sessionUpdate": "session_info_update",
        "_meta": { "icarus": { "provider": provider } },
    });
    if let Some(title) = title {
        update["title"] = json!(title);
    }
    update
}

/// An agent's `can_use_tool` request as an ACP `session/request_permission`, with what each option answers.
pub fn permission_request(
    session_id: &str,
    data: &Value,
) -> (Value, Vec<(String, String, Option<Value>)>) {
    let tool = data["tool_name"].as_str().unwrap_or("tool");
    let input = &data["input"];
    let question = &input["questions"][0];
    let labels: Vec<&str> = question["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option["label"].as_str().or(option.as_str()))
        .collect();
    let mut options = Vec::new();
    let mut answers = Vec::new();
    if data["reason"] == "plan" || tool == "ExitPlanMode" {
        for (id, name, kind) in [
            (
                "allow_accept_edits",
                "Yes, and auto-accept edits",
                "allow_always",
            ),
            ("allow", "Yes, and manually approve edits", "allow_once"),
            ("deny", "No, keep planning", "reject_once"),
        ] {
            options.push(json!({ "optionId": id, "name": name, "kind": kind }));
            answers.push((id.to_string(), id.to_string(), None));
        }
        let plan = data["plan"].as_str().or(input["plan"].as_str());
        let params = json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": data["tool_use_id"],
                "title": "Ready to code?",
                "kind": "switch_mode",
                "status": "pending",
                "rawInput": input,
                "content": [{ "type": "content",
                    "content": { "type": "text", "text": plan.unwrap_or_default() } }],
            },
            "options": options,
        });
        return (params, answers);
    }
    if data["question"] == json!(true) && !labels.is_empty() {
        let asked = question["question"].as_str().unwrap_or_default();
        for (index, label) in labels.iter().enumerate() {
            let id = format!("answer:{index}");
            options.push(json!({ "optionId": id, "name": label, "kind": "allow_once" }));
            answers.push((id, "allow".to_string(), Some(json!({ asked: label }))));
        }
        // The editor shows this one as a text field and sends what was typed in `_meta.answer`.
        options.push(json!({ "optionId": TYPED_ANSWER, "name": "Other", "kind": "allow_once" }));
        answers.push((
            TYPED_ANSWER.into(),
            "allow".into(),
            Some(json!({ asked: "" })),
        ));
        options.push(json!({ "optionId": "deny", "name": "Dismiss", "kind": "reject_once" }));
        answers.push(("deny".into(), "deny".into(), None));
    } else {
        for (id, name, kind) in [
            ("allow", "Allow", "allow_once"),
            ("allow_always", "Always allow", "allow_always"),
            ("deny", "Deny", "reject_once"),
        ] {
            options.push(json!({ "optionId": id, "name": name, "kind": kind }));
            answers.push((id.to_string(), id.to_string(), None));
        }
    }
    let title = question["question"]
        .as_str()
        .map_or_else(|| tool_title(tool, input), str::to_string);
    let params = json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": data["tool_use_id"],
            "title": title,
            "kind": tool_kind(tool),
            "status": "pending",
            "rawInput": input,
        },
        "options": options,
    });
    (params, answers)
}

type Writer = mpsc::UnboundedSender<Value>;
type Connection = (Arc<Client>, mpsc::UnboundedReceiver<Event>);

/// The server connection, swapped for a new one when the server restarts.
struct SharedClient(std::sync::RwLock<Arc<Client>>);

impl SharedClient {
    fn current(&self) -> Arc<Client> {
        Arc::clone(&self.0.read().unwrap_or_else(|p| p.into_inner()))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.current().request(method, params).await
    }

    fn replace(&self, client: Arc<Client>) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = client;
    }
}

struct Bridge {
    client: SharedClient,
    out: Writer,
    provider: Option<String>,
    /// Prompts waiting for their turn to end, by session id.
    pending: Mutex<HashMap<String, (Value, bool)>>,
    /// Prompts this editor sent and already shows, by session id, until the server logs them.
    own_prompts: Mutex<HashMap<String, VecDeque<String>>>,
    tools: Mutex<HashMap<String, ToolState>>,
    /// Attached sessions and the next event each expects, to resume from after a reconnect.
    attached: Mutex<HashMap<String, u64>>,
    /// Sessions this bridge already handed out from `session/new`, so two new tabs never share an agent.
    claimed: Mutex<HashSet<String>>,
    /// Each opened session's provider and title, so its first prompt can name it once.
    labels: Mutex<HashMap<String, (String, Option<String>)>>,
    /// First-prompt titles found (in a replay, or a prompt racing the label) before a session is labelled.
    pending_titles: Mutex<HashMap<String, String>>,
    /// Requests this bridge sent the editor, waiting for its answer, by JSON-RPC id.
    outgoing: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    next_outgoing: std::sync::atomic::AtomicU64,
    /// Permission requests already put to the editor, so a reattach does not ask twice.
    asked: Mutex<HashSet<String>>,
}

impl Bridge {
    fn notify(&self, session_id: &str, update: Value) {
        let _ = self.out.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": session_id, "update": update },
        }));
    }

    /// Sends the editor a request and waits for its reply (`result` or `error`).
    async fn ask_editor(&self, method: &str, params: Value) -> Value {
        let id = self
            .next_outgoing
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, reply) = tokio::sync::oneshot::channel();
        self.outgoing.lock().await.insert(id, sender);
        let _ = self.out.send(
            json!({ "jsonrpc": "2.0", "id": format!("icarus-{id}"), "method": method, "params": params }),
        );
        reply.await.unwrap_or(Value::Null)
    }

    async fn editor_replied(&self, message: Value) {
        let Some(id) = message["id"]
            .as_str()
            .and_then(|id| id.strip_prefix("icarus-"))
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return;
        };
        if let Some(sender) = self.outgoing.lock().await.remove(&id) {
            let _ = sender.send(message);
        }
    }

    /// Puts an agent's permission or question request to the editor and sends its answer back.
    fn ask_permission(self: &Arc<Self>, session_id: &str, data: &Value) {
        let bridge = Arc::clone(self);
        let session_id = session_id.to_string();
        let data = data.clone();
        tokio::spawn(async move {
            let Some(request_id) = data["request_id"].as_str().map(str::to_string) else {
                return;
            };
            if !bridge.asked.lock().await.insert(request_id.clone()) {
                return;
            }
            let (params, answers) = permission_request(&session_id, &data);
            let reply = bridge
                .ask_editor("session/request_permission", params)
                .await;
            let outcome = &reply["result"]["outcome"];
            let chosen = (outcome["outcome"] == "selected")
                .then(|| outcome["optionId"].as_str())
                .flatten()
                .and_then(|id| answers.iter().find(|(option, _, _)| option == id));
            let typed = outcome["_meta"]["answer"].as_str().map(str::trim);
            let (decision, answers) = match chosen {
                Some((id, _, Some(Value::Object(asked)))) if id == TYPED_ANSWER => {
                    match typed.filter(|text| !text.is_empty()) {
                        Some(text) => (
                            "allow",
                            Some(Value::Object(
                                asked
                                    .keys()
                                    .map(|question| (question.clone(), json!(text)))
                                    .collect(),
                            )),
                        ),
                        None => ("deny", None),
                    }
                }
                Some((_, decision, answers)) => (decision.as_str(), answers.clone()),
                None => ("deny", None),
            };
            let _ = bridge
                .client
                .request(
                    "agent.answer",
                    json!({ "id": session_id, "request_id": request_id, "decision": decision, "answers": answers }),
                )
                .await;
        });
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
                json!({ "cwd": cwd, "provider": self.provider, "workspace": workspace, "title": "Agent", "offer_tab": false }),
            )
            .await?;
        let id = info["id"].as_str().unwrap_or_default().to_string();
        claimed.insert(id.clone());
        Ok(id)
    }

    /// The session's mode and model pickers; empty if the server can't say.
    async fn session_state(&self, session_id: &str) -> Value {
        let id = json!({ "id": session_id });
        let (modes, models) = tokio::join!(
            self.client.request("agent.modes", id.clone()),
            self.client.request("agent.models", id),
        );
        match (modes, models) {
            (Ok(modes), Ok(models)) => session_state(&modes, &models),
            _ => json!({}),
        }
    }

    async fn update_config_options(&self, session_id: &str) {
        let state = self.session_state(session_id).await;
        if let Some(options) = state.get("configOptions") {
            self.notify(
                session_id,
                json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
            );
        }
    }

    async fn advertise_commands(&self, session_id: &str) {
        if let Ok(listed) = self
            .client
            .request("agent.commands", json!({ "id": session_id }))
            .await
        {
            self.notify(
                session_id,
                json!({ "sessionUpdate": "available_commands_update", "availableCommands": available_commands(&listed) }),
            );
        }
    }

    /// Answers a session request, then tells the editor the session's commands, which it only
    /// accepts for a session it knows.
    async fn open_session(self: &Arc<Self>, id: Value, session_id: &str, mut result: Value) {
        let state = self.session_state(session_id).await;
        for (key, value) in state.as_object().into_iter().flatten() {
            result[key] = value.clone();
        }
        self.respond(id, result);
        self.advertise_commands(session_id).await;
        self.label_session(session_id).await;
        // Asked once the editor has the session, or it could not show the request.
        if let Ok(info) = self
            .client
            .request("agent.info", json!({ "id": session_id }))
            .await
        {
            for request in info["permission_requests"].as_array().into_iter().flatten() {
                self.ask_permission(session_id, request);
            }
        }
    }

    /// Tells the editor the session's harness and title (the server's, else its first prompt's).
    async fn label_session(&self, session_id: &str) {
        let Ok(info) = self
            .client
            .request("agent.info", json!({ "id": session_id }))
            .await
        else {
            return;
        };
        let provider = info["provider"].as_str().unwrap_or_default().to_string();
        let mut labels = self.labels.lock().await;
        let pending = self.pending_titles.lock().await.remove(session_id);
        let title = chosen_title(&info).or(pending);
        self.notify(session_id, session_info_update(&provider, title.as_deref()));
        labels.insert(session_id.to_string(), (provider, title));
    }

    /// Names a still-untitled session after a prompt it was just sent.
    async fn name_from_prompt(&self, session_id: &str, text: &str) {
        let Some(named) = title_from_prompt(text) else {
            return;
        };
        let mut labels = self.labels.lock().await;
        match labels.get_mut(session_id) {
            Some((provider, title @ None)) => {
                self.notify(session_id, session_info_update(provider, Some(&named)));
                *title = Some(named);
            }
            Some(_) => {}
            // Not labelled yet: the label, when it comes, takes this name.
            None => {
                self.pending_titles
                    .lock()
                    .await
                    .entry(session_id.to_string())
                    .or_insert(named);
            }
        }
    }

    /// Runs Icarus's `/remote-control` for a Claude Code agent; None if the agent handles the text.
    async fn remote_control(&self, session_id: &str, text: &str) -> Result<Option<String>> {
        let Some(wanted) = remote_control_request(text) else {
            return Ok(None);
        };
        let listed = self
            .client
            .request("agent.commands", json!({ "id": session_id }))
            .await?;
        let handled_by_agent = listed["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|command| command["name"] == REMOTE_CONTROL_COMMAND);
        if listed["provider"] != "claude" || handled_by_agent {
            return Ok(None);
        }
        let info = self
            .client
            .request("agent.info", json!({ "id": session_id }))
            .await?;
        let enabled = wanted.unwrap_or(info["remote_control"] != json!(true));
        if info["remote_control"] == json!(enabled) {
            return Ok(Some(match (enabled, info["remote_control_url"].as_str()) {
                (true, Some(url)) => format!("Remote Control is already on: {url}"),
                (true, None) => "Remote Control is already on.".into(),
                (false, _) => "Remote Control is already off.".into(),
            }));
        }
        let switched = self
            .client
            .request(
                "agent.remote_control",
                json!({ "id": session_id, "enabled": enabled }),
            )
            .await?;
        if !enabled {
            return Ok(Some("Remote Control is off.".into()));
        }
        let restarted = switched["agents"][0]["restarted"] == json!(true);
        let mut url = switched["agents"][0]["remote_control_url"]
            .as_str()
            .map(str::to_string);
        // Claude Code registers the session a moment after it restarts.
        for _ in 0..12 {
            if url.is_some() || !restarted {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            url = self
                .client
                .request("agent.info", json!({ "id": session_id }))
                .await?["remote_control_url"]
                .as_str()
                .map(str::to_string);
        }
        Ok(Some(match (url, restarted) {
            (Some(url), _) => format!("Remote Control is on: {url}"),
            (None, true) => "Remote Control is on. The claude.ai link shows in `icarus agent list` once Claude Code registers the session.".into(),
            (None, false) => "Remote Control is on from the agent's next start.".into(),
        }))
    }

    async fn attach(self: &Arc<Self>, session_id: &str, replay: bool) -> Result<u64> {
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
            let first_prompt = attached["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|event| event["kind"] == "user_message")
                .find_map(|event| title_from_prompt(event["data"]["text"].as_str()?));
            if let Some(title) = first_prompt {
                self.pending_titles
                    .lock()
                    .await
                    .insert(session_id.to_string(), title);
            }
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
        let next = attached["next_seq"].as_u64().unwrap_or(0);
        self.attached
            .lock()
            .await
            .insert(session_id.to_string(), next);
        Ok(next)
    }

    /// After the server came back: attaches every session again and plays what was missed live.
    async fn reattach(self: &Arc<Self>) {
        let sessions: Vec<(String, u64)> = self
            .attached
            .lock()
            .await
            .iter()
            .map(|(id, next)| (id.clone(), *next))
            .collect();
        for (session_id, since) in sessions {
            let attached = match self
                .client
                .request("agent.attach", json!({ "id": session_id, "since": since }))
                .await
            {
                Ok(attached) => attached,
                Err(error) => {
                    eprintln!("icarus acp: reattaching {session_id}: {error:#}");
                    continue;
                }
            };
            for event in attached["events"].as_array().into_iter().flatten() {
                self.on_agent_event(&session_id, event).await;
            }
        }
    }

    async fn on_server_event(self: &Arc<Self>, event: Event) {
        if event.topic != "agent.event" {
            return;
        }
        let session_id = event.payload["id"].as_str().unwrap_or_default().to_string();
        self.on_agent_event(&session_id, &event.payload["event"])
            .await;
    }

    async fn take_own_prompt(&self, session_id: &str, text: Option<&str>) -> bool {
        let mut own = self.own_prompts.lock().await;
        let Some(sent) = own.get_mut(session_id) else {
            return false;
        };
        let Some(index) = sent.iter().position(|prompt| Some(prompt.as_str()) == text) else {
            return false;
        };
        sent.remove(index);
        if sent.is_empty() {
            own.remove(session_id);
        }
        true
    }

    async fn on_agent_event(self: &Arc<Self>, session_id: &str, agent_event: &Value) {
        let session_id = session_id.to_owned();
        if let Some(seq) = agent_event["seq"].as_u64()
            && let Some(next) = self.attached.lock().await.get_mut(&session_id)
        {
            if seq < *next {
                return;
            }
            *next = seq + 1;
        }
        let kind = agent_event["kind"].as_str().unwrap_or_default();
        let own_prompt = kind == "user_message"
            && self
                .take_own_prompt(&session_id, agent_event["data"]["text"].as_str())
                .await;
        let updates = if own_prompt {
            Vec::new()
        } else {
            let mut tools = self.tools.lock().await;
            let state = tools.entry(session_id.clone()).or_default();
            event_to_updates(agent_event, Mode::Live, state)
        };
        for update in updates {
            self.notify(&session_id, update);
        }
        if kind == "user_message"
            && let Some(text) = agent_event["data"]["text"].as_str()
        {
            self.name_from_prompt(&session_id, text).await;
        }
        let reports_mode = agent_event["data"]["permission_mode"].is_string();
        match kind {
            "permission_request" => self.ask_permission(&session_id, &agent_event["data"]),
            "capabilities" => {
                self.advertise_commands(&session_id).await;
                self.update_config_options(&session_id).await;
            }
            "mode_changed" | "model_changed" => self.update_config_options(&session_id).await,
            "status" | "session" if reports_mode => self.update_config_options(&session_id).await,
            _ => {}
        }
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
                    self.open_session(id.clone(), &session_id, json!({ "sessionId": session_id })).await;
                    Ok(None)
                }
                "session/load" | "session/resume" => {
                    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                    self.claimed.lock().await.insert(session_id.clone());
                    self.attach(&session_id, method == "session/load").await?;
                    self.open_session(id.clone(), &session_id, json!({})).await;
                    Ok(None)
                }
                "session/set_config_option" | "session/set_mode" => {
                    let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                    let (rpc, key, value) = match (method.as_str(), params["configId"].as_str()) {
                        ("session/set_mode", _) => ("agent.set_mode", "mode", params["modeId"].as_str()),
                        (_, Some(MODE_OPTION)) => ("agent.set_mode", "mode", params["value"].as_str()),
                        (_, Some(MODEL_OPTION)) => ("agent.set_model", "model", params["value"].as_str()),
                        _ => anyhow::bail!("unknown config option {}", params["configId"]),
                    };
                    let value = value.ok_or_else(|| anyhow::anyhow!("no {key} given"))?;
                    self.client
                        .request(rpc, json!({ "id": session_id, key: value }))
                        .await?;
                    if method == "session/set_config_option" {
                        let state = self.session_state(&session_id).await;
                        Ok(Some(json!({ "configOptions": state["configOptions"] })))
                    } else {
                        Ok(Some(json!({})))
                    }
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
                    if let [only] = text.as_slice()
                        && let Some(reply) = self.remote_control(&session_id, only).await?
                    {
                        self.notify(
                            &session_id,
                            json!({ "sessionUpdate": "agent_message_chunk", "content": text_block(&reply) }),
                        );
                        return Ok(Some(json!({ "stopReason": "end_turn" })));
                    }
                    let text = text.join("\n\n");
                    self.pending.lock().await.insert(session_id.clone(), (id.clone(), false));
                    // Registered before sending: the server logs the prompt before it answers.
                    self.own_prompts
                        .lock()
                        .await
                        .entry(session_id.clone())
                        .or_default()
                        .push_back(text.clone());
                    if let Err(error) = self
                        .client
                        .request("agent.send", json!({ "id": session_id, "text": text, "images": images }))
                        .await
                    {
                        self.pending.lock().await.remove(&session_id);
                        self.take_own_prompt(&session_id, Some(&text)).await;
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

/// How often a lost server connection is retried; a restarting server is back within a second or two.
const RECONNECT_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

pub async fn run<F, C>(
    (client, mut events): Connection,
    provider: Option<String>,
    reconnect: C,
) -> Result<()>
where
    C: Fn() -> F + Send + 'static,
    F: std::future::Future<Output = Result<Connection>> + Send,
{
    let (out, mut outbox) = mpsc::unbounded_channel::<Value>();
    let bridge = Arc::new(Bridge {
        client: SharedClient(std::sync::RwLock::new(client)),
        out,
        provider,
        pending: Mutex::default(),
        own_prompts: Mutex::default(),
        tools: Mutex::default(),
        attached: Mutex::default(),
        claimed: Mutex::default(),
        labels: Mutex::default(),
        pending_titles: Mutex::default(),
        outgoing: Mutex::default(),
        next_outgoing: std::sync::atomic::AtomicU64::new(0),
        asked: Mutex::default(),
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
        loop {
            while let Some(event) = events.recv().await {
                listening.on_server_event(event).await;
            }
            let lost = std::time::Instant::now();
            let (client, reconnected) = loop {
                match reconnect().await {
                    Ok(connection) => break connection,
                    Err(_) => tokio::time::sleep(RECONNECT_EVERY).await,
                }
            };
            eprintln!(
                "icarus acp: reconnected to the server after {:.0?}",
                lost.elapsed()
            );
            listening.client.replace(client);
            events = reconnected;
            listening.reattach().await;
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let handling = Arc::clone(&bridge);
        if message.get("method").is_none() {
            tokio::spawn(async move { handling.editor_replied(message).await });
            continue;
        }
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
    fn permission_and_question_requests_become_acp_permission_requests() {
        use agent_client_protocol_schema::v1::RequestPermissionRequest;
        let (params, answers) = permission_request(
            "s1",
            &json!({ "request_id": "r1", "tool_name": "Bash", "tool_use_id": "t1",
                "input": { "command": "ls" }, "question": false }),
        );
        let parsed: RequestPermissionRequest =
            serde_json::from_value(params.clone()).unwrap_or_else(|e| panic!("{params}: {e}"));
        assert_eq!(parsed.options.len(), 3);
        assert_eq!(answers[1].1, "allow_always");

        let (params, answers) = permission_request(
            "s1",
            &json!({ "request_id": "r2", "tool_name": "AskUserQuestion", "tool_use_id": "t2",
                "question": true, "input": { "questions": [{ "question": "Which base?",
                "options": [{ "label": "main" }, { "label": "dev" }] }] } }),
        );
        serde_json::from_value::<RequestPermissionRequest>(params.clone())
            .unwrap_or_else(|e| panic!("{params}: {e}"));
        assert_eq!(params["toolCall"]["title"], "Which base?");
        assert_eq!(answers[1].0, "answer:1");
        assert_eq!(answers[1].2, Some(json!({ "Which base?": "dev" })));
        assert_eq!(
            answers[2].0, TYPED_ANSWER,
            "a typed answer comes last before dismissing"
        );
        assert_eq!(answers.last().map(|a| a.1.as_str()), Some("deny"));
    }

    #[test]
    fn a_plan_request_asks_whether_to_start_coding() {
        use agent_client_protocol_schema::v1::RequestPermissionRequest;
        let (params, answers) = permission_request(
            "s1",
            &json!({ "request_id": "r3", "tool_name": "ExitPlanMode", "tool_use_id": "t3",
                "question": false, "reason": "plan", "plan": "# Plan\n- step",
                "input": { "plan": "# Plan\n- step" } }),
        );
        serde_json::from_value::<RequestPermissionRequest>(params.clone())
            .unwrap_or_else(|e| panic!("{params}: {e}"));
        let call = &params["toolCall"];
        assert_eq!(call["kind"], "switch_mode");
        assert_eq!(call["title"], "Ready to code?");
        assert_eq!(call["content"][0]["content"]["text"], "# Plan\n- step");
        let options: Vec<(&str, &str)> = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| {
                (
                    o["optionId"].as_str().unwrap_or_default(),
                    o["kind"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            options,
            [
                ("allow_accept_edits", "allow_always"),
                ("allow", "allow_once"),
                ("deny", "reject_once")
            ]
        );
        assert!(
            answers
                .iter()
                .all(|(id, decision, extra)| id == decision && extra.is_none())
        );
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
        let live = event_to_updates(
            &json!({ "seq": 7, "kind": "user_message", "data": { "text": "from a button" } }),
            Mode::Live,
            &mut tools,
        );
        let SessionUpdate::UserMessageChunk(chunk) = parse(&live[0]) else {
            panic!("a prompt from elsewhere shows live: {live:?}");
        };
        assert_eq!(
            chunk.message_id.map(|id| id.to_string()).as_deref(),
            Some("icarus-user-7"),
            "each prompt is its own message"
        );
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
    fn modes_models_and_commands_are_valid_acp() {
        use agent_client_protocol_schema::v1::{
            AvailableCommand, SessionConfigOption, SessionModeState,
        };
        let state = session_state(
            &json!({ "current": "dontAsk", "modes": [{ "id": "plan", "name": "Plan", "description": "Plans" }] }),
            &json!({ "provider": "claude", "current": "opus", "models": [{ "id": "opus", "name": "Opus" }] }),
        );
        let options: Vec<SessionConfigOption> =
            serde_json::from_value(state["configOptions"].clone()).expect("config options");
        assert_eq!(options.len(), 2);
        assert_eq!(state["configOptions"][0]["category"], "mode");
        assert_eq!(
            state["configOptions"][0]["options"][1]["value"], "dontAsk",
            "a mode the agent reported but Icarus doesn't list still shows"
        );
        assert_eq!(
            state["configOptions"][1]["options"][0]["name"],
            "Claude Opus"
        );
        let modes: SessionModeState =
            serde_json::from_value(state["modes"].clone()).expect("modes");
        assert_eq!(modes.available_modes.len(), 2);

        let commands = available_commands(&json!({ "provider": "claude", "commands": [
            { "name": "compact", "description": "Summarize", "hint": "<focus>" },
        ] }));
        let parsed: Vec<AvailableCommand> =
            serde_json::from_value(json!(commands)).expect("commands");
        assert_eq!(parsed.len(), 2);
        assert_eq!(commands[0]["input"]["hint"], "<focus>");
        assert_eq!(commands[1]["name"], REMOTE_CONTROL_COMMAND);
        assert!(available_commands(&json!({ "provider": "codex", "commands": [] })).is_empty());
    }

    #[test]
    fn remote_control_takes_on_off_or_nothing() {
        assert_eq!(remote_control_request("/remote-control"), Some(None));
        assert_eq!(
            remote_control_request(" /remote-control on "),
            Some(Some(true))
        );
        assert_eq!(
            remote_control_request("/remote-control off"),
            Some(Some(false))
        );
        assert_eq!(remote_control_request("/remote-controls"), None);
        assert_eq!(remote_control_request("/remote-control maybe"), None);
        assert_eq!(
            remote_control_request("tell me about /remote-control"),
            None
        );
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

    #[test]
    fn the_model_picker_lists_the_sessions_own_harness_models() {
        let modes = json!({ "current": "default", "modes": [] });
        let codex = session_state(
            &modes,
            &json!({ "provider": "codex", "current": "gpt-5.5", "models": [
                { "id": "gpt-5.5", "name": "GPT-5.5" }, { "id": "gpt-5.5-mini", "name": "GPT-5.5 mini" }
            ] }),
        );
        let names: Vec<&str> = codex["configOptions"][1]["options"]
            .as_array()
            .expect("model options")
            .iter()
            .filter_map(|option| option["name"].as_str())
            .collect();
        assert_eq!(names, ["Codex GPT-5.5", "Codex GPT-5.5 mini"]);
        let claude = session_state(
            &modes,
            &json!({ "provider": "claude", "current": "opus", "models": [{ "id": "opus", "name": "Opus" }] }),
        );
        assert_eq!(
            claude["configOptions"][1]["options"][0]["name"],
            "Claude Opus"
        );
    }

    #[test]
    fn a_conversation_is_named_after_its_first_prompt() {
        assert_eq!(
            title_from_prompt("\n  Fix the   login bug\nand add a test").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(
            title_from_prompt(
                "<context uri=\"file:///a.rs\">\nfn main() {}\n</context>\n\nExplain this"
            )
            .as_deref(),
            Some("Explain this"),
            "attached context is not what was asked"
        );
        assert_eq!(title_from_prompt("/compact"), None, "commands name nothing");
        assert_eq!(title_from_prompt("  \n "), None);
        let long = title_from_prompt(&"word ".repeat(100)).expect("a title");
        assert_eq!(long.chars().count(), TITLE_LIMIT);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn only_a_chosen_server_title_names_a_session() {
        assert_eq!(
            chosen_title(&json!({ "provider": "codex", "title": "Review #12" })).as_deref(),
            Some("Review #12")
        );
        assert_eq!(
            chosen_title(&json!({ "provider": "claude", "title": "Agent" })),
            None
        );
        assert_eq!(
            chosen_title(&json!({ "provider": "codex", "title": "Codex" })),
            None
        );
        assert_eq!(
            chosen_title(&json!({ "provider": "codex", "title": null })),
            None
        );
    }

    #[test]
    fn session_info_carries_the_title_and_harness() {
        let update = session_info_update("codex", Some("Fix the login bug"));
        match parse(&update) {
            SessionUpdate::SessionInfoUpdate(info) => {
                assert_eq!(
                    info.title,
                    agent_client_protocol_schema::MaybeUndefined::Value("Fix the login bug".into())
                );
                let meta = info.meta.expect("meta");
                assert_eq!(meta["icarus"]["provider"], "codex");
            }
            other => panic!("{other:?}"),
        }
        match parse(&session_info_update("claude", None)) {
            SessionUpdate::SessionInfoUpdate(info) => assert!(info.title.is_undefined()),
            other => panic!("{other:?}"),
        }
    }
}
