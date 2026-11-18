use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const SPAWN_TOOLS: &[&str] = &["Agent", "Task"];
const PREVIEW_KEYS: &[&str] = &[
    "command",
    "file_path",
    "path",
    "pattern",
    "url",
    "query",
    "skill",
    "description",
    "prompt",
];
const PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Working,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subagent {
    /// The spawning tool call's id; the subagent's own messages name it as their parent.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub description: Option<String>,
    pub activity: Option<String>,
    pub state: SubagentState,
    pub started_at_ms: u64,
    #[serde(skip)]
    task_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub activity: Option<String>,
    pub subagents: Vec<Subagent>,
    #[serde(skip)]
    in_turn: bool,
}

pub fn tool_preview(name: &str, input: &Value) -> String {
    let detail = PREVIEW_KEYS
        .iter()
        .filter_map(|key| input[*key].as_str())
        .map(|text| {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim()
        })
        .find(|text| !text.is_empty());
    match detail {
        Some(detail) => {
            let detail: String = detail.chars().take(PREVIEW_CHARS).collect();
            format!("{name}: {detail}")
        }
        None => name.to_string(),
    }
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl Activity {
    pub fn observe(&mut self, kind: &str, data: &Value, at_ms: u64, background: &BTreeSet<String>) {
        match kind {
            "user_message" => {
                self.in_turn = true;
                self.activity = None;
                self.subagents
                    .retain(|sub| sub.state == SubagentState::Working);
            }
            "tool_call" => self.tool_call(data, at_ms),
            "tool_result" => {
                let id = data["tool_use_id"].as_str().unwrap_or_default();
                if let Some(sub) = self.subagents.iter_mut().find(|sub| sub.id == id) {
                    let backgrounded = sub
                        .task_id
                        .as_ref()
                        .is_some_and(|task| background.contains(task));
                    if !backgrounded {
                        sub.state = SubagentState::Done;
                    }
                }
            }
            "task" => self.task(data),
            "turn_end" => {
                self.in_turn = false;
                self.activity = None;
                for sub in &mut self.subagents {
                    if !sub
                        .task_id
                        .as_ref()
                        .is_some_and(|task| background.contains(task))
                    {
                        sub.state = SubagentState::Done;
                    }
                }
            }
            "exit" => *self = Self::default(),
            _ => {}
        }
        // Between turns only subagents still running in the background are worth a row.
        if !self.in_turn {
            self.subagents
                .retain(|sub| sub.state == SubagentState::Working);
        }
    }

    fn tool_call(&mut self, data: &Value, at_ms: u64) {
        let name = data["name"].as_str().unwrap_or_default();
        let input = &data["input"];
        let preview = tool_preview(name, input);
        match data["parent"].as_str() {
            None => self.activity = Some(preview),
            Some(parent) => {
                if let Some(sub) = self.subagents.iter_mut().find(|sub| sub.id == parent) {
                    sub.activity = Some(preview);
                }
            }
        }
        if let (true, Some(id)) = (SPAWN_TOOLS.contains(&name), data["id"].as_str())
            && !self.subagents.iter().any(|sub| sub.id == id)
        {
            self.subagents.push(Subagent {
                id: id.to_string(),
                kind: text(&input["subagent_type"]).unwrap_or_else(|| "general-purpose".into()),
                description: text(&input["description"]),
                activity: None,
                state: SubagentState::Working,
                started_at_ms: at_ms,
                task_id: None,
            });
        }
    }

    fn task(&mut self, frame: &Value) {
        let task_id = frame["task_id"].as_str();
        let tool_use_id = frame["tool_use_id"]
            .as_str()
            .or_else(|| frame["patch"]["tool_use_id"].as_str());
        let Some(sub) = self.subagents.iter_mut().find(|sub| {
            tool_use_id == Some(sub.id.as_str())
                || (task_id.is_some() && sub.task_id.as_deref() == task_id)
        }) else {
            return;
        };
        match frame["subtype"].as_str().unwrap_or_default() {
            "task_started" => {
                sub.task_id = task_id.map(str::to_string);
                if let Some(kind) = text(&frame["subagent_type"]) {
                    sub.kind = kind;
                }
            }
            "task_notification" => sub.state = SubagentState::Done,
            "task_updated"
                if matches!(
                    frame["patch"]["status"].as_str(),
                    Some("completed" | "failed" | "killed" | "stopped" | "cancelled")
                ) =>
            {
                sub.state = SubagentState::Done;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spawn(id: &str, name: &str, input: Value, parent: Option<&str>) -> Value {
        json!({ "id": id, "name": name, "input": input, "parent": parent })
    }

    fn task(subtype: &str, extra: Value) -> Value {
        let mut frame = json!({ "type": "system", "subtype": subtype });
        for (key, value) in extra.as_object().into_iter().flatten() {
            frame[key] = value.clone();
        }
        frame
    }

    #[test]
    fn foreground_subagents_run_until_their_tool_result_and_leave_with_the_turn() {
        let none = BTreeSet::new();
        let mut live = Activity::default();
        live.observe("user_message", &json!({ "text": "review" }), 1, &none);
        live.observe(
            "tool_call",
            &spawn(
                "toolu_bash",
                "Bash",
                json!({ "command": "mise exec -- ./bin/flow\nmore" }),
                None,
            ),
            2,
            &none,
        );
        assert_eq!(
            live.activity.as_deref(),
            Some("Bash: mise exec -- ./bin/flow")
        );
        for id in ["toolu_a", "toolu_b"] {
            live.observe(
                "tool_call",
                &spawn(
                    id,
                    "Agent",
                    json!({ "description": "Look", "subagent_type": "Explore" }),
                    None,
                ),
                3,
                &none,
            );
        }
        live.observe(
            "tool_call",
            &spawn(
                "toolu_read",
                "Read",
                json!({ "file_path": "src/lib.rs" }),
                Some("toolu_a"),
            ),
            4,
            &none,
        );
        assert_eq!(live.subagents.len(), 2);
        assert_eq!(live.subagents[0].kind, "Explore");
        assert_eq!(live.subagents[0].description.as_deref(), Some("Look"));
        assert_eq!(
            live.subagents[0].activity.as_deref(),
            Some("Read: src/lib.rs")
        );
        assert_eq!(live.subagents[0].started_at_ms, 3);
        assert_eq!(
            live.activity.as_deref(),
            Some("Agent: Look"),
            "the parent's own line moves on"
        );

        live.observe(
            "tool_result",
            &json!({ "tool_use_id": "toolu_read" }),
            5,
            &none,
        );
        assert_eq!(live.subagents[0].state, SubagentState::Working);
        live.observe(
            "tool_result",
            &json!({ "tool_use_id": "toolu_a" }),
            6,
            &none,
        );
        assert_eq!(live.subagents[0].state, SubagentState::Done);
        assert_eq!(live.subagents[1].state, SubagentState::Working);

        live.observe("turn_end", &json!({}), 7, &none);
        assert!(live.subagents.is_empty());
        assert_eq!(live.activity, None);
    }

    #[test]
    fn a_backgrounded_subagent_outlives_its_turn_until_its_task_notification() {
        let mut background = BTreeSet::new();
        let mut live = Activity::default();
        live.observe("user_message", &json!({}), 1, &background);
        live.observe(
            "tool_call",
            &spawn(
                "toolu_agent",
                "Agent",
                json!({ "description": "Sleep", "run_in_background": true }),
                None,
            ),
            2,
            &background,
        );
        live.observe(
            "task",
            &task(
                "task_started",
                json!({ "task_id": "agent-1", "tool_use_id": "toolu_agent",
                "subagent_type": "general-purpose", "task_type": "local_agent" }),
            ),
            3,
            &background,
        );
        background.insert("agent-1".to_string());
        live.observe(
            "tool_result",
            &json!({ "tool_use_id": "toolu_agent" }),
            4,
            &background,
        );
        live.observe("turn_end", &json!({}), 5, &background);
        assert_eq!(live.subagents.len(), 1);
        assert_eq!(live.subagents[0].state, SubagentState::Working);

        background.clear();
        live.observe(
            "task",
            &task(
                "task_notification",
                json!({ "task_id": "agent-1", "status": "completed" }),
            ),
            6,
            &background,
        );
        assert!(
            live.subagents.is_empty(),
            "a finished background agent leaves an idle card"
        );
    }

    #[test]
    fn the_summary_shape_names_the_type_and_state() {
        let mut live = Activity::default();
        live.observe("user_message", &json!({}), 1, &BTreeSet::new());
        live.observe(
            "tool_call",
            &spawn("t", "Task", json!({}), None),
            2,
            &BTreeSet::new(),
        );
        let shown = serde_json::to_value(&live).expect("serializes");
        assert_eq!(shown["activity"], "Task");
        assert_eq!(shown["subagents"][0]["type"], "general-purpose");
        assert_eq!(shown["subagents"][0]["state"], "working");
        assert!(shown["subagents"][0].get("task_id").is_none());
    }
}
