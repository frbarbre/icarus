//! A scripted stand-in for `claude -p --input-format stream-json --output-format stream-json`.
//!
//! Each user message is answered with "echo: <text>". Prefixes change behaviour:
//! `sleep:<ms> <text>` waits first, `write:<path> <text>` emits a Write tool call and result,
//! `fail <text>` ends the turn as an error, `ask <question>` calls AskUserQuestion first,
//! `env:A,B` answers with those environment variables, `args` with its command-line arguments,
//! `exit` terminates the process.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

fn emit(out: &mut impl Write, value: Value) {
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    let session_id = std::env::args()
        .skip_while(|a| a != "--resume")
        .nth(1)
        .unwrap_or_else(|| format!("fake-{}", std::process::id()));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let init = json!({ "type": "system", "subtype": "init", "session_id": session_id, "model": "fake-model", "cwd": std::env::current_dir().ok() });
    // Real `claude -p` says nothing until the first message arrives.
    let mut lazy_init = std::env::var_os("ICARUS_FAKE_LAZY_INIT").is_some();
    if !lazy_init {
        emit(&mut out, init.clone());
    }
    let mut turn = 0u64;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["type"] != "user" {
            continue;
        }
        if lazy_init {
            emit(&mut out, init.clone());
            lazy_init = false;
        }
        let text = message["message"]["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
            .unwrap_or_default()
            .to_string();
        if text == "exit" {
            break;
        }
        turn += 1;
        let mut text = text;
        if let Some(rest) = text.strip_prefix("sleep:") {
            let (ms, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
            text = tail.to_string();
        }
        emit(
            &mut out,
            json!({ "type": "system", "subtype": "status", "status": "requesting", "session_id": session_id }),
        );
        if let Some(rest) = text.strip_prefix("write:") {
            let (path, content) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, content);
            let tool_id = format!("toolu_fake_{turn}");
            emit(
                &mut out,
                json!({ "type": "assistant", "message": { "id": format!("msg_tool_{turn}"), "role": "assistant", "content": [
                    { "type": "tool_use", "id": tool_id, "name": "Write", "input": { "file_path": path, "content": content } }
                ] }, "parent_tool_use_id": null, "session_id": session_id }),
            );
            emit(
                &mut out,
                json!({ "type": "user", "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": tool_id, "content": format!("File created successfully at: {path}") }
                ] }, "parent_tool_use_id": null, "session_id": session_id,
                "tool_use_result": { "type": "create", "filePath": path, "content": content, "originalFile": null } }),
            );
            text = format!("wrote {path}");
        }
        if let Some(question) = text.strip_prefix("ask ") {
            emit(
                &mut out,
                json!({ "type": "assistant", "message": { "id": format!("msg_ask_{turn}"), "role": "assistant", "content": [
                    { "type": "tool_use", "id": format!("toolu_ask_{turn}"), "name": "AskUserQuestion",
                      "input": { "questions": [{ "question": question }] } }
                ] }, "parent_tool_use_id": null, "session_id": session_id }),
            );
        }
        if text == "args" {
            text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        }
        if let Some(names) = text.strip_prefix("env:") {
            text = names
                .split(',')
                .map(|name| match std::env::var(name) {
                    Ok(value) => format!("{name}={value}"),
                    Err(_) => format!("{name} unset"),
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
        let reply = format!("echo: {text}");
        let message_id = format!("msg_{turn}");
        emit(
            &mut out,
            json!({ "type": "stream_event", "event": { "type": "message_start", "message": { "id": message_id } }, "session_id": session_id }),
        );
        for chunk in reply.as_bytes().chunks(4) {
            emit(
                &mut out,
                json!({ "type": "stream_event", "event": { "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": String::from_utf8_lossy(chunk) } }, "session_id": session_id }),
            );
        }
        emit(
            &mut out,
            json!({ "type": "assistant", "message": { "id": message_id, "role": "assistant",
                "content": [ { "type": "text", "text": reply } ] }, "parent_tool_use_id": null, "session_id": session_id }),
        );
        emit(
            &mut out,
            json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed", "unifiedWindows": {
                "five_hour": { "utilization": 0.1, "resetsAt": 1, "status": "allowed" },
                "seven_day": { "utilization": 0.2, "resetsAt": 2, "status": "allowed" } } }, "session_id": session_id }),
        );
        let failed = text.starts_with("fail");
        emit(
            &mut out,
            json!({ "type": "result", "subtype": if failed { "error_during_execution" } else { "success" },
                "is_error": failed, "result": reply, "session_id": session_id, "stop_reason": "end_turn",
                "duration_ms": 5, "total_cost_usd": 0.0, "num_turns": turn }),
        );
    }
}
