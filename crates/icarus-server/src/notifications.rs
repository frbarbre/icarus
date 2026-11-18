//! Server notifications (plan D7): made from agent events and automation scripts,
//! stored in `state.db` so none are lost while no client is connected, and pushed to clients live.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{AgentEvent, AgentInfo};
use crate::next_action::setting;
use crate::server::{Server, params, rpc, to_value};
use crate::state::Notification;
use crate::workspaces::UnreadKind;

pub const AGENT_TURN_DONE: &str = "agent_turn_done";
pub const AGENT_WAITING: &str = "agent_waiting";
pub const AGENT_FAILED: &str = "agent_failed";
pub const CI_FAILED: &str = "ci_failed";
pub const REVIEW_REQUESTED: &str = "review_requested";
pub const PR_MERGED: &str = "pr_merged";
pub const AUTOMATION: &str = "automation";
pub const UPDATE_AVAILABLE: &str = "update_available";

/// Every kind has a `notifications.<kind>` toggle, on by default.
pub const KINDS: [&str; 8] = [
    AGENT_TURN_DONE,
    AGENT_WAITING,
    AGENT_FAILED,
    CI_FAILED,
    REVIEW_REQUESTED,
    PR_MERGED,
    AUTOMATION,
    UPDATE_AVAILABLE,
];

/// Orca's burst cooldown for agent news.
const AGENT_WINDOW_MS: u64 = 5_000;
/// A script may send the same notification on consecutive runs.
const AUTOMATION_WINDOW_MS: u64 = 10 * 60 * 1000;
/// GitHub subjects name the exact state (head sha, merge, release), so one notification per state is enough.
const GITHUB_WINDOW_MS: u64 = 30 * 24 * 60 * 60 * 1000;

const BODY_MAX_CHARS: usize = 280;
const DEFAULT_LIMIT: usize = 50;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn window_ms(kind: &str) -> u64 {
    match kind {
        CI_FAILED | REVIEW_REQUESTED | PR_MERGED | UPDATE_AVAILABLE => GITHUB_WINDOW_MS,
        AUTOMATION => AUTOMATION_WINDOW_MS,
        _ => AGENT_WINDOW_MS,
    }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(BODY_MAX_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewNotification {
    pub kind: &'static str,
    pub subject: String,
    pub title: String,
    pub body: String,
    pub workspace_id: Option<String>,
    pub data: Value,
}

/// `notifications.enabled` and `notifications.<kind>` default to on.
pub fn kind_enabled(settings: &Value, kind: &str) -> bool {
    let on = |key: &str| {
        setting(settings, &["notifications", key]).and_then(Value::as_bool) != Some(false)
    };
    on("enabled") && on(kind)
}

pub fn sound_enabled(settings: &Value) -> bool {
    setting(settings, &["notifications", "sound"]).and_then(Value::as_bool) != Some(false)
}

/// What one agent event means to the user, if anything. `label` names the agent (its workspace, else its title).
pub fn from_agent_event(
    info: &AgentInfo,
    label: &str,
    event: &AgentEvent,
) -> Option<NewNotification> {
    let data = &event.data;
    let (kind, title, body) = match event.kind.as_str() {
        "turn_end" if data["is_error"] == json!(true) => (
            AGENT_FAILED,
            format!("{label}: turn failed"),
            data["result"].as_str().unwrap_or_default().to_owned(),
        ),
        "turn_end" => (
            AGENT_TURN_DONE,
            format!("{label}: finished"),
            data["result"].as_str().unwrap_or_default().to_owned(),
        ),
        "exit" if data["unexpected"] == json!(true) && data["code"] != json!(0) => (
            AGENT_FAILED,
            format!("{label}: agent stopped"),
            match data["code"].as_i64() {
                Some(code) => format!("The agent process exited with code {code}."),
                None => "The agent process ended unexpectedly.".to_owned(),
            },
        ),
        "notice" if data["subtype"] == "permission_denied" => (
            AGENT_WAITING,
            format!("{label}: needs permission"),
            data["tool_name"]
                .as_str()
                .map(|tool| format!("Permission to use {tool} was denied."))
                .unwrap_or_else(|| "A permission request was denied.".to_owned()),
        ),
        "notice" if data["request"].is_string() && declined(&data["reply"]) => (
            AGENT_WAITING,
            format!("{label}: needs permission"),
            format!(
                "Icarus could not approve {}.",
                data["request"].as_str().unwrap_or("a request")
            ),
        ),
        "permission_request" if data["question"] != json!(true) => (
            AGENT_WAITING,
            format!("{label}: needs permission"),
            data["tool_name"]
                .as_str()
                .map(|tool| format!("Allow {tool}?"))
                .unwrap_or_else(|| "The agent asks for permission.".to_owned()),
        ),
        "tool_call" if data["parent"].is_null() && data["name"] == "AskUserQuestion" => (
            AGENT_WAITING,
            format!("{label}: has a question"),
            data["input"]["questions"][0]["question"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
        _ => return None,
    };
    Some(NewNotification {
        kind,
        subject: info.id.clone(),
        title,
        body: clip(&body),
        workspace_id: info.workspace.clone(),
        data: json!({ "agent_id": info.id, "provider": info.provider, "seq": event.seq }),
    })
}

fn declined(reply: &Value) -> bool {
    matches!(reply["decision"].as_str(), Some("decline" | "abort"))
        || reply["action"] == "decline"
        || reply.get("code").is_some()
}

/// Turns agent signals into notifications for as long as the server runs.
pub(crate) fn spawn(server: Arc<Server>) {
    let Some(mut signals) = server.agents.take_signals() else {
        return;
    };
    tokio::spawn(async move {
        while let Some((id, event)) = signals.recv().await {
            if event.kind == crate::agent::STATE_SIGNAL {
                server.broadcast("agent.state", event.data);
                continue;
            }
            let Some(agent) = server.agents.get(&id) else {
                continue;
            };
            if event.kind == "turn_end" {
                let info = agent.info();
                server.usage_seen(&info);
                if let Some(workspace) = &info.workspace {
                    server.agent_turn_ended(&id, workspace);
                }
            }
            // A note reply's answer shows up in its thread; it is not a turn the user waits on.
            if server.store.is_note_reply_agent(&id).unwrap_or(false) {
                continue;
            }
            let info = agent.info();
            if event.kind == "turn_end"
                && let Some(workspace) = &info.workspace
            {
                server.touch_activity(workspace);
            }
            let label = info
                .workspace
                .as_deref()
                .and_then(|ws| server.store.workspace(ws).ok().flatten())
                .map(|ws| ws.title.unwrap_or(ws.name))
                .or_else(|| info.title.clone())
                .unwrap_or_else(|| "Agent".to_owned());
            if let Some(new) = from_agent_event(&info, &label, &event) {
                if let Some(workspace) = &info.workspace {
                    let kind = if new.kind == AGENT_WAITING {
                        UnreadKind::NeedsInput
                    } else {
                        UnreadKind::Done
                    };
                    server.agent_news(workspace, kind);
                }
                server.notify(new);
            }
        }
    });
}

impl Server {
    /// Stores and broadcasts a notification unless its kind is off or it repeats one inside its window.
    pub(crate) fn notify(&self, new: NewNotification) -> Option<Notification> {
        let settings = self.resolved_settings(None);
        if !kind_enabled(&settings, new.kind) {
            return None;
        }
        let now = now_ms();
        let key = format!("{}|{}", new.kind, new.subject);
        match self
            .store
            .reserve_notification_key(&key, now, window_ms(new.kind))
        {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                tracing::warn!("notification dedupe failed: {error:#}");
                return None;
            }
        }
        let notification = Notification {
            id: format!("ntf-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            kind: new.kind.to_owned(),
            subject: new.subject,
            title: new.title,
            body: new.body,
            workspace_id: new.workspace_id,
            data: Some(new.data),
            created_at: now,
            read: false,
        };
        if let Err(error) = self.store.add_notification(&notification) {
            tracing::warn!("cannot store a notification: {error:#}");
            return None;
        }
        let mut payload = serde_json::to_value(&notification).unwrap_or_default();
        payload["sound"] = json!(sound_enabled(&settings));
        self.broadcast("notification", payload);
        Some(notification)
    }

    fn notifications_changed(&self) {
        let unread = self.store.unread_notifications().unwrap_or_default();
        self.broadcast("notifications.changed", json!({ "unread": unread }));
    }

    pub(crate) async fn notifications_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        let internal = |e: anyhow::Error| rpc("internal", format!("{e:#}"));
        match method {
            "notification.send" => {
                #[derive(Deserialize)]
                struct P {
                    title: String,
                    #[serde(default)]
                    body: String,
                    workspace: Option<String>,
                    subject: Option<String>,
                    kind: Option<String>,
                }
                let p: P = params(p)?;
                let kind = match p.kind.as_deref() {
                    None => AUTOMATION,
                    Some(kind) => [CI_FAILED, REVIEW_REQUESTED, PR_MERGED, AUTOMATION]
                        .into_iter()
                        .find(|k| *k == kind)
                        .ok_or_else(|| {
                            rpc(
                                "invalid_params",
                                "kind is ci_failed, review_requested, pr_merged or automation",
                            )
                        })?,
                };
                let workspace_id = match &p.workspace {
                    Some(key) => Some(self.workspace_by_key(key)?.id),
                    None => None,
                };
                let subject = p.subject.unwrap_or_else(|| {
                    format!("{}:{}", p.title, workspace_id.clone().unwrap_or_default())
                });
                let stored = self.notify(NewNotification {
                    kind,
                    subject,
                    title: p.title,
                    body: p.body,
                    workspace_id,
                    data: json!({}),
                });
                Ok(json!({ "sent": stored.is_some(), "notification": stored }))
            }
            "notification.list" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(default)]
                    unread_only: bool,
                    limit: Option<usize>,
                }
                let p: P = params(p)?;
                let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000);
                to_value(
                    self.store
                        .notifications(p.unread_only, limit)
                        .map_err(internal)?,
                )
            }
            "notification.read" => {
                #[derive(Deserialize)]
                struct P {
                    id: Option<String>,
                    #[serde(default)]
                    all: bool,
                }
                let p: P = params(p)?;
                let id = match (p.id, p.all) {
                    (_, true) => None,
                    (Some(id), false) => {
                        if !self.store.notification_exists(&id).map_err(internal)? {
                            return Err(rpc("not_found", format!("notification {id}")));
                        }
                        Some(id)
                    }
                    (None, false) => return Err(rpc("invalid_params", "id or all required")),
                };
                let updated = self
                    .store
                    .mark_notifications_read(id.as_deref())
                    .map_err(internal)?;
                self.notifications_changed();
                Ok(json!({ "updated": updated }))
            }
            "notification.clear" => {
                let cleared = self.store.clear_notifications().map_err(internal)?;
                self.notifications_changed();
                Ok(json!({ "cleared": cleared }))
            }
            "notification.kinds" => {
                let settings = self.resolved_settings(None);
                let kinds: BTreeMap<&str, bool> = KINDS
                    .iter()
                    .map(|kind| (*kind, kind_enabled(&settings, kind)))
                    .collect();
                Ok(json!({ "kinds": kinds, "sound": sound_enabled(&settings) }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::AgentState;

    fn info() -> AgentInfo {
        AgentInfo {
            id: "agent-1".into(),
            provider: "claude".into(),
            title: Some("Agent".into()),
            workspace: Some("ws-1".into()),
            cwd: PathBuf::from("/w"),
            state: AgentState::Idle,
            provider_session: None,
            model: None,
            mode: "default".into(),
            turns: 1,
            created_at_ms: 0,
            last_activity_ms: 0,
            next_seq: 0,
            rate_limits: None,
            rate_limits_at_ms: None,
            account: None,
            remote_control: false,
            remote_control_url: None,
            background_tasks: 0,
            live: Default::default(),
            permission_requests: Vec::new(),
            revision: 0,
        }
    }

    fn event(kind: &str, data: Value) -> AgentEvent {
        AgentEvent {
            seq: 3,
            at_ms: 0,
            kind: kind.into(),
            data,
        }
    }

    fn kind_of(kind: &str, data: Value) -> Option<&'static str> {
        from_agent_event(&info(), "feat-a", &event(kind, data)).map(|n| n.kind)
    }

    #[test]
    fn agent_events_map_to_turn_done_waiting_and_failed() {
        let done = from_agent_event(
            &info(),
            "feat-a",
            &event(
                "turn_end",
                json!({ "is_error": false, "result": "All green" }),
            ),
        )
        .expect("done");
        assert_eq!(done.kind, AGENT_TURN_DONE);
        assert_eq!(done.title, "feat-a: finished");
        assert_eq!(done.body, "All green");
        assert_eq!(done.subject, "agent-1");
        assert_eq!(done.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(
            kind_of("turn_end", json!({ "is_error": true })),
            Some(AGENT_FAILED)
        );
        assert_eq!(
            kind_of("exit", json!({ "code": 1, "unexpected": true })),
            Some(AGENT_FAILED)
        );
        assert_eq!(
            kind_of("exit", json!({ "code": null, "unexpected": false })),
            None,
            "sleep and kill are not failures"
        );
        assert_eq!(
            kind_of("exit", json!({ "code": 0, "unexpected": true })),
            None
        );
        assert_eq!(
            kind_of("notice", json!({ "subtype": "permission_denied" })),
            Some(AGENT_WAITING)
        );
        assert_eq!(
            kind_of(
                "notice",
                json!({ "request": "item/commandExecution/requestApproval", "reply": { "decision": "decline" } })
            ),
            Some(AGENT_WAITING)
        );
        assert_eq!(
            kind_of(
                "notice",
                json!({ "request": "item/commandExecution/requestApproval", "reply": { "decision": "accept" } })
            ),
            None
        );
        assert_eq!(
            kind_of(
                "tool_call",
                json!({ "name": "AskUserQuestion", "parent": null, "input": {} })
            ),
            Some(AGENT_WAITING)
        );
        assert_eq!(
            kind_of("tool_call", json!({ "name": "Bash", "parent": null })),
            None
        );
    }

    #[test]
    fn bodies_are_clipped_on_a_char_boundary() {
        let long = "é".repeat(BODY_MAX_CHARS + 10);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), BODY_MAX_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn toggles_default_on() {
        assert!(kind_enabled(&Value::Null, CI_FAILED));
        assert!(sound_enabled(&Value::Null));
        let settings = json!({ "notifications": { "ci_failed": false, "sound": false } });
        assert!(!kind_enabled(&settings, CI_FAILED));
        assert!(kind_enabled(&settings, PR_MERGED));
        assert!(!sound_enabled(&settings));
        assert!(!kind_enabled(
            &json!({ "notifications": { "enabled": false } }),
            PR_MERGED
        ));
    }
}
