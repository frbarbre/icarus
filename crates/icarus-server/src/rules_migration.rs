//! Turns the removed built-in rules (`rules.*`, from Icarus or an Orca import) into an automation script.

use icarus_rules::prompts::{
    ActionPromptId, DEFAULT_REREVIEW_PROMPT_TEMPLATE, DEFAULT_REVIEW_PROMPT_TEMPLATE,
    ci_triage_instructions, parse_ci_triage_config,
};
use icarus_settings::automation_template::{orca_flow, orca_flow_defaults};
use icarus_settings::layers::{self, KeyPath, Layer, SettingsPaths};
use serde_json::{Map, Value, json};

use crate::next_action::setting;
use crate::notifications::NewNotification;
use crate::server::Server;

const ENGINE_STATE_KEY: &str = "rules.engine_state";
const SCRIPT: &str = "automations/orca-flow.ts";
const BACKUP: &str = "automations/rules-backup.json";
const CONDITIONS: [&str; 10] = [
    "merged",
    "closed",
    "reviewing",
    "merge-queue",
    "conflicts",
    "draft",
    "changes-requested",
    "merging",
    "pm-approval",
    "review",
];

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The script options that reproduce the rules in `pr` (`rules.pr`, Orca's `workspaceStatusRules`).
pub(crate) fn script_config(
    pr: &Value,
    handled: Vec<String>,
    ci_triage: Map<String, Value>,
    now_ms: u64,
) -> Value {
    let mut config = orca_flow_defaults();
    let statuses: Map<String, Value> = CONDITIONS
        .iter()
        .filter_map(|c| Some((c.to_string(), json!(text(&pr["statusByCondition"][c])?))))
        .collect();
    config["statusByCondition"] = Value::Object(statuses);
    config["newWorkspaceStatus"] = json!(text(&pr["newWorkspaceStatus"]));
    config["newWorkspaceStatusSince"] = json!(
        pr["newWorkspaceStatusSince"]
            .as_f64()
            .map_or(now_ms, |s| s as u64)
    );
    for key in ["onResolved", "onReviewed"] {
        config[key] = json!(if pr[key] == "delete" {
            "delete"
        } else {
            "keep"
        });
    }
    if let Some(name) = text(&pr["mergingCheckName"]) {
        config["mergingCheckName"] = json!(name);
    }
    config["pmApprovalTeam"] = json!(text(&pr["pmApprovalTeam"]).unwrap_or_default());
    config["resolveConflictsWithAgent"] = json!(pr["resolveConflictsWithAgent"] == true);
    if let Some(prompt) = text(&pr["conflictPromptTemplate"]) {
        config["conflictPrompt"] = json!(prompt);
    }
    config["reviewEnteredCommand"] = json!(text(&pr["reviewEnteredCommand"]).unwrap_or_default());
    let inbox = &pr["reviewInbox"];
    if let Some(agent) = text(&inbox["agent"]) {
        config["agent"] = json!(agent);
    }
    config["reviewInbox"] = json!({
        "enabled": inbox["enabled"] == true,
        "prompt": text(&inbox["promptTemplate"]).unwrap_or_else(|| DEFAULT_REVIEW_PROMPT_TEMPLATE.to_owned()),
        "rereviewPrompt": text(&inbox["rereviewPromptTemplate"]).unwrap_or_else(|| DEFAULT_REREVIEW_PROMPT_TEMPLATE.to_owned()),
        "handled": handled,
    });
    config["ciTriage"] = Value::Object(ci_triage);
    config
}

impl Server {
    /// Once, when `rules` is still in the user settings: writes the Orca flow script from it, points the
    /// rule projects at it and removes `rules`.
    pub(crate) fn migrate_rules(&self) {
        let settings = self.resolved_settings(None);
        let toggle = setting(&settings, &["notifications", "rules"]).cloned();
        let rules = setting(&settings, &["rules"]).cloned();
        let presets = setting(&settings, &["presets"]).is_some();
        if toggle.is_none() && rules.is_none() && !presets {
            return;
        }
        if let Some(rules) = &rules
            && let Err(error) = backup(rules)
        {
            tracing::warn!("saving the built-in rules before migrating: {error}");
            return;
        }
        if let Err(error) = strip_user_keys(&["rules", "notifications.rules", "presets"]) {
            tracing::warn!("removing the built-in rules settings: {error}");
            return;
        }
        if let Some(toggle) = toggle
            && let Err(error) =
                self.write_key("notifications.automation", Some(toggle), Some("user"), None)
        {
            tracing::warn!("moving notifications.rules: {}", error.message);
        }
        let Some(rules) = rules else {
            return;
        };
        match self.migrate(&settings, &rules) {
            Ok(summary) => {
                tracing::info!("built-in rules migrated: {summary}");
                self.notify(NewNotification {
                    kind: "automation",
                    subject: "rules-migrated".to_owned(),
                    title: "Built-in rules are now an automation script".to_owned(),
                    body: summary,
                    workspace_id: None,
                    data: json!({ "script": SCRIPT }),
                });
            }
            Err(error) => tracing::warn!("migrating the built-in rules: {error}"),
        }
    }

    fn migrate(&self, settings: &Value, rules: &Value) -> Result<String, String> {
        let dir = icarus_settings::layers::config_dir().ok_or("no config directory")?;
        let pr = &rules["pr"];
        let enabled = rules["enabled"].as_bool().unwrap_or(pr["enabled"] == true);
        let projects: Vec<_> = rules["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|key| self.automation_project(key).ok())
            .collect();
        let mut handled: Vec<String> = pr["handledPullRequests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        if let Ok(Some(state)) = self.store.kv_get(ENGINE_STATE_KEY) {
            handled.extend(
                state["ledger"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned)),
            );
        }
        handled.sort();
        handled.dedup();
        let mut ci_triage = Map::new();
        for project in &projects {
            let project_settings = self.resolved_settings(Some(&project.root));
            if let Some(triage) = setting(&project_settings, &["project", "ciTriage"])
                .filter(|v| !v.is_null())
                .and_then(parse_ci_triage_config)
            {
                ci_triage.insert(project.name.clone(), json!(ci_triage_instructions(&triage)));
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let config = script_config(pr, handled, ci_triage, now);

        let script = dir.join(SCRIPT);
        let wrote_script = !script.exists();
        if wrote_script {
            std::fs::write(&script, orca_flow(&config)).map_err(|e| e.to_string())?;
        }

        let set = |key: &str, value: Option<Value>| {
            self.write_key(key, value, Some("user"), None)
                .map(|_| ())
                .map_err(|e| format!("{key}: {}", e.message))
        };
        let _ = self.store.kv_delete(ENGINE_STATE_KEY);
        let mut assigned = Vec::new();
        if enabled {
            let scripts = setting(settings, &["automations", "scripts"]).cloned();
            for project in &projects {
                let taken = scripts
                    .as_ref()
                    .is_some_and(|s| !s[&project.name].is_null() || !s[&project.id].is_null());
                if !taken && !project.name.contains('.') {
                    set(
                        &format!("automations.scripts.{}", project.name),
                        Some(json!(SCRIPT)),
                    )?;
                    assigned.push(project.name.clone());
                }
            }
            if rules["armed"] == true {
                set("automations.armed", Some(json!(true)))?;
            }
        }
        if let Some(interval) = rules["interval_ms"].as_u64() {
            set("automations.interval_ms", Some(json!(interval)))?;
        }
        if let Some(hold) = text(&pr["holdStatus"]) {
            set("automations.hold_status", Some(json!(hold)))?;
        }
        if let Some(status) = text(&pr["statusByCondition"]["reviewing"])
            && setting(settings, &["sidebar", "reviewingStatus"]).is_none()
        {
            set("sidebar.reviewingStatus", Some(json!(status)))?;
        }
        if let Some(team) = text(&pr["pmApprovalTeam"]) {
            set("automations.teams", Some(json!([team])))?;
        }
        for id in ActionPromptId::ALL {
            let key = id.key();
            if let Some(prompt) = text(&pr["actionPrompts"][key])
                && setting(settings, &["prompts", key]).is_none()
            {
                set(&format!("prompts.{key}"), Some(json!(prompt)))?;
            }
        }
        Ok(format!(
            "{} {}; {}. The old settings are saved in {}.",
            if wrote_script {
                "Wrote"
            } else {
                "Kept the existing"
            },
            script.display(),
            if assigned.is_empty() {
                "no project uses it yet (set automations.scripts.<project>)".to_owned()
            } else {
                format!("used by {}", assigned.join(", "))
            },
            dir.join(BACKUP).display()
        ))
    }
}

fn backup(rules: &Value) -> Result<(), String> {
    let dir = icarus_settings::layers::config_dir().ok_or("no config directory")?;
    std::fs::create_dir_all(dir.join("automations")).map_err(|e| e.to_string())?;
    let saved = serde_json::to_string_pretty(rules).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(BACKUP), saved).map_err(|e| e.to_string())
}

/// Removes keys the schema no longer knows; a validated write would refuse the layer while they remain.
fn strip_user_keys(keys: &[&str]) -> Result<(), String> {
    let paths = SettingsPaths::discover(None);
    let path = paths
        .path(Layer::User)
        .ok_or("no user settings path")?
        .to_path_buf();
    let Some(mut doc) = layers::read_layer(Layer::User, &path).map_err(|e| format!("{e:?}"))?
    else {
        return Ok(());
    };
    for key in keys {
        let key = KeyPath::parse(key).map_err(|e| format!("{e:?}"))?;
        layers::unset(&mut doc, &key);
    }
    layers::write_document(&path, &doc).map_err(|e| e.to_string())?;
    crate::settings_cache::forget(&path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_settings_become_script_options() {
        let pr = json!({
            "enabled": true,
            "statusByCondition": { "merged": "done", "review": "  ", "draft": "wip" },
            "onResolved": "delete",
            "onReviewed": "none",
            "pmApprovalTeam": "acme/pm",
            "reviewInbox": { "enabled": true, "agent": "codex", "promptTemplate": "Look at #{{prNumber}}" },
        });
        let config = script_config(&pr, vec!["acme/app#4".into()], Map::new(), 7);
        assert_eq!(
            config["statusByCondition"],
            json!({ "merged": "done", "draft": "wip" })
        );
        assert_eq!(config["onResolved"], "delete");
        assert_eq!(config["onReviewed"], "keep");
        assert_eq!(config["newWorkspaceStatusSince"], 7);
        assert_eq!(config["pmApprovalTeam"], "acme/pm");
        assert_eq!(config["agent"], "codex");
        assert_eq!(config["reviewInbox"]["prompt"], "Look at #{{prNumber}}");
        assert_eq!(
            config["reviewInbox"]["rereviewPrompt"],
            DEFAULT_REREVIEW_PROMPT_TEMPLATE
        );
        assert_eq!(config["reviewInbox"]["handled"], json!(["acme/app#4"]));
        assert!(!orca_flow(&config).contains("__ICARUS_CONFIG__"));
    }
}
