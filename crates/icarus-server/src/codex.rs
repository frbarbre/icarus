//! Codex provider: the user's own `codex app-server`, driven over JSON-RPC on stdio as Orca's
//! native chat does (initialize, thread/start or thread/resume, turn/start, turn/interrupt).
//!
//! Auth stays with the user's `codex login`; Icarus never reads or stores its tokens.
//! Notifications are normalized into the same event kinds the Claude provider produces.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::agent::AgentOptions;

/// Overrides the `codex` binary, e.g. to point tests at a fake app-server.
pub const BIN_ENV: &str = "ICARUS_CODEX_BIN";

pub fn command(options: &AgentOptions) -> Vec<String> {
    let mut command = match &options.command {
        Some(command) => command.clone(),
        None => vec![std::env::var(BIN_ENV).unwrap_or_else(|_| "codex".into())],
    };
    command.push("app-server".into());
    command
}

/// Orca's Full access for bypass, else its Ask; every field is set so `config.toml` cannot widen it.
pub fn thread_policy(bypass: bool) -> Value {
    if bypass {
        json!({ "approvalPolicy": "never", "sandbox": "danger-full-access", "approvalsReviewer": "user" })
    } else {
        json!({ "approvalPolicy": "on-request", "sandbox": "read-only", "approvalsReviewer": "user" })
    }
}

pub fn turn_policy(bypass: bool) -> Value {
    if bypass {
        json!({ "approvalPolicy": "never", "approvalsReviewer": "user", "sandboxPolicy": { "type": "dangerFullAccess" } })
    } else {
        json!({ "approvalPolicy": "on-request", "approvalsReviewer": "user", "sandboxPolicy": { "type": "readOnly" } })
    }
}

pub fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "icarus", "title": "Icarus", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": {
            "experimentalApi": true,
            "requestAttestation": false,
            "mcpServerOpenaiFormElicitation": false,
            "extensions": {},
        },
    })
}

pub fn start_params(options: &AgentOptions) -> Value {
    let mut params = thread_policy(options.bypass_permissions);
    params["cwd"] = json!(options.cwd);
    if let Some(model) = &options.model {
        params["model"] = json!(model);
    }
    params
}

/// Omits the model so the resumed thread keeps its own model and effort.
pub fn resume_params(options: &AgentOptions, thread_id: &str) -> Value {
    let mut params = thread_policy(options.bypass_permissions);
    params["cwd"] = json!(options.cwd);
    params["threadId"] = json!(thread_id);
    params["excludeTurns"] = json!(true);
    params
}

pub fn turn_params(
    thread_id: &str,
    text: &str,
    images: &[(String, String)],
    bypass: bool,
) -> Value {
    let mut input: Vec<Value> = images
        .iter()
        .map(|(media_type, data)| json!({ "type": "image", "url": format!("data:{media_type};base64,{data}") }))
        .collect();
    input.push(json!({ "type": "text", "text": text, "text_elements": [] }));
    let mut params = turn_policy(bypass);
    params["threadId"] = json!(thread_id);
    params["input"] = json!(input);
    params
}

/// Only this thread-specific error proves the thread never persisted, so starting fresh loses nothing.
pub fn is_no_rollout(message: &str, thread_id: &str) -> bool {
    message.ends_with(&format!("no rollout found for thread id {thread_id}"))
}

/// The answer to a blocking app-server request: approvals follow the permission mode, the rest get Orca's safe replies.
pub fn reply_to_request(method: &str, bypass: bool) -> Result<Value, (i64, String)> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Ok(json!({ "decision": if bypass { "accept" } else { "decline" } }))
        }
        "execCommandApproval" | "applyPatchApproval" => {
            Ok(json!({ "decision": if bypass { "approved" } else { "abort" } }))
        }
        "mcpServer/elicitation/request" => {
            Ok(json!({ "action": "decline", "content": null, "_meta": null }))
        }
        "item/permissions/requestApproval" => {
            Ok(json!({ "permissions": {}, "scope": "turn", "strictAutoReview": true }))
        }
        "item/tool/call" => Ok(json!({ "contentItems": [], "success": false })),
        "account/chatgptAuthTokens/refresh" => {
            Err((-32001, "Icarus never handles Codex auth tokens".into()))
        }
        other => Err((
            -32000,
            format!("Icarus cannot answer {other} without a person"),
        )),
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(text_of)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => value["text"].as_str().unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

fn command_line(item: &Value) -> Value {
    match &item["command"] {
        Value::Array(parts) => json!(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        other => other.clone(),
    }
}

fn plan_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "inProgress" | "in_progress" => "in_progress",
        _ => "pending",
    }
}

/// A Codex thread item as a tool call in Claude's shape, so every client renders both alike.
fn tool_call(item: &Value) -> Option<(String, Value)> {
    let (name, input) = match item["type"].as_str()? {
        "commandExecution" => (
            "Bash".to_string(),
            json!({ "command": command_line(item), "cwd": item["cwd"] }),
        ),
        "fileChange" => {
            let changes = item["changes"].clone();
            let mut input = json!({ "changes": changes });
            if let [only] = changes.as_array().map(Vec::as_slice).unwrap_or_default() {
                input["file_path"] = only["path"].clone();
            }
            ("apply_patch".to_string(), input)
        }
        "mcpToolCall" => (
            format!(
                "mcp__{}__{}",
                item["server"].as_str().unwrap_or("mcp"),
                item["tool"].as_str().unwrap_or("tool")
            ),
            item["arguments"].clone(),
        ),
        "webSearch" => ("WebSearch".to_string(), json!({ "query": item["query"] })),
        _ => return None,
    };
    Some((name, input))
}

fn tool_result(item: &Value) -> Option<Value> {
    let (name, input) = tool_call(item)?;
    let status = item["status"].as_str().unwrap_or("completed");
    let failed = status == "failed"
        || status == "declined"
        || item["exitCode"].as_i64().is_some_and(|code| code != 0)
        || !item["error"].is_null();
    let content = match item["type"].as_str() {
        Some("commandExecution") => json!(item["aggregatedOutput"].as_str().unwrap_or_default()),
        Some("mcpToolCall") if !item["error"].is_null() => item["error"]["message"].clone(),
        Some("mcpToolCall") => item["result"]["content"].clone(),
        Some("fileChange") => json!(
            item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["path"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ),
        _ => json!(text_of(&item["query"])),
    };
    Some(json!({
        "tool_use_id": item["id"],
        "name": name,
        "input": input,
        "content": content,
        "is_error": failed,
        "detail": { "status": status, "exit_code": item["exitCode"], "duration_ms": item["durationMs"] },
        "parent": null,
    }))
}

/// Maps one app-server notification to zero or more normalized events.
pub fn normalize(method: &str, params: &Value) -> Vec<(String, Value)> {
    let item = &params["item"];
    match method {
        "item/agentMessage/delta" => vec![(
            "text_delta".into(),
            json!({ "index": params["itemId"], "text": params["delta"], "parent": null }),
        )],
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => vec![(
            "thinking_delta".into(),
            json!({ "index": params["itemId"], "text": params["delta"], "parent": null }),
        )],
        "item/started" => match item["type"].as_str() {
            Some("agentMessage") => vec![(
                "message_start".into(),
                json!({ "message_id": item["id"], "parent": null }),
            )],
            _ => tool_call(item)
                .map(|(name, input)| {
                    (
                        "tool_call".into(),
                        json!({ "id": item["id"], "name": name, "input": input, "parent": null }),
                    )
                })
                .into_iter()
                .collect(),
        },
        "item/completed" => match item["type"].as_str() {
            Some("agentMessage") => vec![(
                "message".into(),
                json!({
                    "message_id": item["id"],
                    "content": [{ "type": "text", "text": text_of(&item["text"]) }],
                    "parent": null,
                }),
            )],
            Some("reasoning") => {
                let summary = text_of(&item["summary"]);
                let text = if summary.is_empty() {
                    text_of(&item["content"])
                } else {
                    summary
                };
                if text.is_empty() {
                    return Vec::new();
                }
                vec![(
                    "message".into(),
                    json!({ "message_id": item["id"], "content": [{ "type": "thinking", "thinking": text }], "parent": null }),
                )]
            }
            _ => tool_result(item)
                .map(|data| ("tool_result".into(), data))
                .into_iter()
                .collect(),
        },
        "turn/plan/updated" => vec![(
            "plan".into(),
            json!({
                "explanation": params["explanation"],
                "todos": params["plan"].as_array().into_iter().flatten().map(|step| json!({
                    "content": step["step"],
                    "status": plan_status(step["status"].as_str().unwrap_or_default()),
                })).collect::<Vec<_>>(),
            }),
        )],
        "turn/started" => vec![(
            "status".into(),
            json!({ "status": "running", "turn_id": params["turn"]["id"] }),
        )],
        "turn/completed" => {
            let turn = &params["turn"];
            let status = turn["status"].as_str().unwrap_or("completed");
            vec![(
                "turn_end".into(),
                json!({
                    "subtype": status,
                    "is_error": status == "failed",
                    "result": turn["error"]["message"],
                    "stop_reason": status,
                    "turn_id": turn["id"],
                }),
            )]
        }
        "thread/tokenUsage/updated" => vec![("usage".into(), params["tokenUsage"].clone())],
        "account/rateLimits/updated" => vec![("rate_limit".into(), params["rateLimits"].clone())],
        "error" => vec![("notice".into(), params.clone())],
        _ => Vec::new(),
    }
}

type Reply = Result<Value, String>;

/// Request bookkeeping for one app-server process.
#[derive(Default)]
pub struct Rpc {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, (String, oneshot::Sender<Reply>)>>,
    turn: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub enum Incoming {
    Response,
    Request { id: Value, method: String },
    Notification { method: String, params: Value },
    Other,
}

impl Rpc {
    pub fn request(&self, method: &str, params: Value) -> (Value, oneshot::Receiver<Reply>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, (method.to_string(), sender));
        (
            json!({ "id": id, "method": method, "params": params }),
            receiver,
        )
    }

    pub fn running_turn(&self) -> Option<String> {
        lock(&self.turn).clone()
    }

    /// Fails every outstanding request, e.g. when the process exits.
    pub fn reset(&self) {
        lock(&self.pending).clear();
        *lock(&self.turn) = None;
    }

    /// Settles responses and tracks the running turn; the caller handles the rest.
    pub fn incoming(&self, message: Value) -> Incoming {
        let method = message["method"].as_str().map(str::to_string);
        match (message.get("id"), method) {
            (Some(id), None) => {
                let Some((method, sender)) =
                    id.as_u64().and_then(|id| lock(&self.pending).remove(&id))
                else {
                    return Incoming::Response;
                };
                let reply = match message.get("error") {
                    Some(error) => Err(error["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| error.to_string())),
                    None => Ok(message["result"].clone()),
                };
                // Set here, in read order, so a fast `turn/completed` cannot be overwritten later.
                if method == "turn/start"
                    && let Ok(result) = &reply
                    && let Some(turn) = result["turn"]["id"].as_str()
                {
                    *lock(&self.turn) = Some(turn.to_string());
                }
                let _ = sender.send(reply);
                Incoming::Response
            }
            (Some(id), Some(method)) => Incoming::Request {
                id: id.clone(),
                method,
            },
            (None, Some(method)) => {
                let params = message["params"].clone();
                match method.as_str() {
                    "turn/started" => {
                        *lock(&self.turn) = params["turn"]["id"].as_str().map(str::to_string);
                    }
                    "turn/completed" => {
                        let mut turn = lock(&self.turn);
                        if turn.as_deref() == params["turn"]["id"].as_str() {
                            *turn = None;
                        }
                    }
                    _ => {}
                }
                Incoming::Notification { method, params }
            }
            (None, None) => Incoming::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn options(bypass: bool) -> AgentOptions {
        AgentOptions {
            provider: "codex".into(),
            cwd: PathBuf::from("/w"),
            workspace: None,
            title: None,
            model: Some("gpt-5".into()),
            bypass_permissions: bypass,
            resume: None,
            command: None,
            env: BTreeMap::new(),
            account: None,
            remote_control: true,
        }
    }

    #[test]
    fn bypass_maps_to_full_access_and_ask_to_a_read_only_sandbox() {
        let start = start_params(&options(true));
        assert_eq!(start["approvalPolicy"], "never");
        assert_eq!(start["sandbox"], "danger-full-access");
        assert_eq!(start["model"], "gpt-5");
        let ask = turn_params("t", "hi", &[], false);
        assert_eq!(ask["approvalPolicy"], "on-request");
        assert_eq!(ask["sandboxPolicy"]["type"], "readOnly");
        let resume = resume_params(&options(true), "thread-1");
        assert!(
            resume.get("model").is_none(),
            "a resumed thread keeps its model"
        );
        assert_eq!(resume["threadId"], "thread-1");
    }

    #[test]
    fn approvals_follow_the_permission_mode_and_tokens_are_refused() {
        assert_eq!(
            reply_to_request("item/commandExecution/requestApproval", true).expect("reply")["decision"],
            "accept"
        );
        assert_eq!(
            reply_to_request("item/fileChange/requestApproval", false).expect("reply")["decision"],
            "decline"
        );
        assert!(reply_to_request("account/chatgptAuthTokens/refresh", true).is_err());
    }

    #[test]
    fn items_become_claude_shaped_tool_calls_text_and_plans() {
        let started = normalize(
            "item/started",
            &json!({ "item": { "type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/w", "status": "inProgress" } }),
        );
        assert_eq!(started[0].0, "tool_call");
        assert_eq!(started[0].1["name"], "Bash");
        assert_eq!(started[0].1["input"]["command"], "ls");
        let done = normalize(
            "item/completed",
            &json!({ "item": { "type": "commandExecution", "id": "c1", "command": "false", "status": "completed", "exitCode": 1, "aggregatedOutput": "no" } }),
        );
        assert_eq!(done[0].0, "tool_result");
        assert_eq!(done[0].1["is_error"], true);
        assert_eq!(done[0].1["content"], "no");

        let delta = normalize(
            "item/agentMessage/delta",
            &json!({ "itemId": "m1", "delta": "hi" }),
        );
        assert_eq!(delta[0].0, "text_delta");
        let message = normalize(
            "item/completed",
            &json!({ "item": { "type": "agentMessage", "id": "m1", "text": "hi there" } }),
        );
        assert_eq!(message[0].1["content"][0]["text"], "hi there");

        let plan = normalize(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "a", "status": "inProgress" }] }),
        );
        assert_eq!(plan[0].1["todos"][0]["status"], "in_progress");

        let end = normalize(
            "turn/completed",
            &json!({ "turn": { "id": "t1", "status": "failed", "error": { "message": "boom" } } }),
        );
        assert_eq!(end[0].0, "turn_end");
        assert_eq!(end[0].1["is_error"], true);
        assert!(normalize("thread/status/changed", &json!({})).is_empty());
    }

    #[test]
    fn turn_start_replies_set_the_running_turn_until_it_completes() {
        let rpc = Rpc::default();
        let (request, _reply) = rpc.request("turn/start", json!({}));
        rpc.incoming(json!({ "id": request["id"], "result": { "turn": { "id": "turn-1" } } }));
        assert_eq!(rpc.running_turn().as_deref(), Some("turn-1"));
        rpc.incoming(
            json!({ "method": "turn/completed", "params": { "turn": { "id": "turn-1" } } }),
        );
        assert_eq!(rpc.running_turn(), None);
    }
}
