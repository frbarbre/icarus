//! A scripted stand-in for `codex app-server`: JSON-RPC lines on stdio, no `jsonrpc` field.
//!
//! Each turn answers "echo: <text>". Prefixes change behaviour: `sleep:<ms> <text>` waits first
//! (and can be interrupted), `write:<path> <text>` reports a file change and writes the file,
//! `run:<command> <text>` reports a command execution, `plan:<a,b> <text>` updates the plan,
//! `fail <text>` fails the turn, `exit` terminates the process.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, value: Value) {
        let mut out = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "method": method, "params": params }));
    }
}

struct Turn {
    id: String,
    interrupted: Arc<AtomicBool>,
}

fn main() {
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut thread_id: Option<String> = None;
    let turns = Arc::new(AtomicU64::new(0));
    let active: Arc<Mutex<Option<Turn>>> = Arc::default();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(id), Some(method)) = (message.get("id").cloned(), message["method"].as_str())
        else {
            continue;
        };
        let params = &message["params"];
        match method {
            "initialize" => out.send(json!({ "id": id, "result": { "userAgent": "icarus-fake-codex" } })),
            "thread/start" | "thread/resume" => {
                let thread = params["threadId"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("fake-thread-{}", std::process::id()));
                thread_id = Some(thread.clone());
                out.send(json!({ "id": id, "result": {
                    "thread": { "id": thread, "cwd": cwd },
                    "model": params["model"].as_str().unwrap_or("fake-codex-model"),
                    "approvalPolicy": params["approvalPolicy"],
                    "sandbox": params["sandbox"],
                } }));
                out.notify("thread/started", json!({ "thread": { "id": thread } }));
            }
            "turn/start" => {
                let Some(thread) = thread_id.clone() else {
                    out.send(json!({ "id": id, "error": { "code": -32600, "message": "no thread" } }));
                    continue;
                };
                let text = params["input"]
                    .as_array()
                    .and_then(|items| items.iter().find_map(|i| i["text"].as_str()))
                    .unwrap_or_default()
                    .to_string();
                if text == "exit" {
                    break;
                }
                let n = turns.fetch_add(1, Ordering::Relaxed) + 1;
                let turn_id = format!("turn-{n}");
                let interrupted = Arc::new(AtomicBool::new(false));
                *active.lock().unwrap_or_else(|p| p.into_inner()) = Some(Turn {
                    id: turn_id.clone(),
                    interrupted: Arc::clone(&interrupted),
                });
                out.send(json!({ "id": id, "result": { "turn": { "id": turn_id, "status": "inProgress" } } }));
                let out = out.clone();
                let active = Arc::clone(&active);
                let bypass = params["sandboxPolicy"]["type"] == "dangerFullAccess";
                std::thread::spawn(move || {
                    let status = run_turn(&out, &thread, &turn_id, n, text, bypass, &interrupted);
                    active.lock().unwrap_or_else(|p| p.into_inner()).take();
                    let error = (status == "failed").then(|| json!({ "message": "fake failure" }));
                    out.notify("turn/completed", json!({ "threadId": thread,
                        "turn": { "id": turn_id, "status": status, "error": error } }));
                });
            }
            "turn/interrupt" => {
                let guard = active.lock().unwrap_or_else(|p| p.into_inner());
                match guard.as_ref() {
                    Some(turn) if params["turnId"].as_str() == Some(turn.id.as_str()) => {
                        turn.interrupted.store(true, Ordering::Relaxed);
                        out.send(json!({ "id": id, "result": {} }));
                    }
                    _ => out.send(json!({ "id": id, "error": { "code": -32600, "message": "no active turn to interrupt" } })),
                }
            }
            other => out.send(json!({ "id": id, "error": { "code": -32601, "message": format!("unknown method {other}") } })),
        }
    }
}

fn item(out: &Out, thread: &str, turn: &str, started: Value, completed: Value) {
    out.notify(
        "item/started",
        json!({ "threadId": thread, "turnId": turn, "item": started }),
    );
    out.notify(
        "item/completed",
        json!({ "threadId": thread, "turnId": turn, "item": completed }),
    );
}

fn run_turn(
    out: &Out,
    thread: &str,
    turn: &str,
    n: u64,
    mut text: String,
    bypass: bool,
    interrupted: &AtomicBool,
) -> &'static str {
    out.notify(
        "turn/started",
        json!({ "threadId": thread, "turn": { "id": turn, "status": "inProgress" } }),
    );
    if let Some(rest) = text.strip_prefix("sleep:") {
        let (ms, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let until = Instant::now() + Duration::from_millis(ms.parse().unwrap_or(0));
        while Instant::now() < until {
            if interrupted.load(Ordering::Relaxed) {
                return "interrupted";
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        text = tail.to_string();
    }
    if let Some(rest) = text.strip_prefix("plan:") {
        let (steps, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let plan: Vec<Value> = steps
            .split(',')
            .enumerate()
            .map(|(i, step)| json!({ "step": step, "status": if i == 0 { "inProgress" } else { "pending" } }))
            .collect();
        out.notify(
            "turn/plan/updated",
            json!({ "threadId": thread, "turnId": turn, "plan": plan }),
        );
        text = tail.to_string();
    }
    if let Some(rest) = text.strip_prefix("run:") {
        let (command, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let id = format!("cmd-{n}");
        item(
            out,
            thread,
            turn,
            json!({ "type": "commandExecution", "id": id, "command": command, "cwd": std::env::current_dir().ok(), "status": "inProgress" }),
            json!({ "type": "commandExecution", "id": id, "command": command, "status": "completed",
                "exitCode": 0, "aggregatedOutput": format!("ran {command}\n"), "durationMs": 1 }),
        );
        text = tail.to_string();
    }
    if let Some(rest) = text.strip_prefix("write:") {
        let (path, content) = rest.split_once(' ').unwrap_or((rest, ""));
        let id = format!("patch-{n}");
        let changes = json!([{ "path": path, "kind": { "type": "add" }, "diff": content }]);
        out.notify(
            "item/started",
            json!({ "threadId": thread, "turnId": turn,
            "item": { "type": "fileChange", "id": id, "changes": [], "status": "inProgress" } }),
        );
        if bypass {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, content);
        }
        let status = if bypass { "completed" } else { "declined" };
        out.notify(
            "item/completed",
            json!({ "threadId": thread, "turnId": turn,
            "item": { "type": "fileChange", "id": id, "changes": changes, "status": status } }),
        );
        text = format!("wrote {path}");
    }
    let reply = format!("echo: {text}");
    let message_id = format!("msg-{n}");
    out.notify(
        "item/started",
        json!({ "threadId": thread, "turnId": turn,
        "item": { "type": "agentMessage", "id": message_id, "text": "" } }),
    );
    for chunk in reply.as_bytes().chunks(4) {
        out.notify(
            "item/agentMessage/delta",
            json!({ "threadId": thread, "turnId": turn,
            "itemId": message_id, "delta": String::from_utf8_lossy(chunk) }),
        );
    }
    out.notify(
        "item/completed",
        json!({ "threadId": thread, "turnId": turn,
        "item": { "type": "agentMessage", "id": message_id, "text": reply } }),
    );
    out.notify("thread/tokenUsage/updated", json!({ "threadId": thread, "turnId": turn, "tokenUsage": {
        "total": { "totalTokens": 100 * n }, "last": { "totalTokens": 100 }, "modelContextWindow": 1000 } }));
    out.notify(
        "account/rateLimits/updated",
        json!({ "rateLimits": { "limitId": "codex",
        "primary": { "usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1 },
        "secondary": { "usedPercent": 20, "windowDurationMins": 10080, "resetsAt": 2 } } }),
    );
    if text.starts_with("fail") {
        "failed"
    } else {
        "completed"
    }
}
