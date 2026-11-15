//! Human-readable output for `icarus pr`, `icarus workspace next-action` and `icarus rules`.

use serde_json::Value;

pub fn pr_status(v: &Value) -> String {
    let pr = &v["pull_request"];
    if pr.is_null() {
        return if v["known"] == Value::Bool(true) {
            "no pull request".into()
        } else {
            format!(
                "pull request unknown: {}",
                v["error"].as_str().unwrap_or("lookup failed")
            )
        };
    }
    let state = match (pr["state"].as_str(), pr["is_draft"].as_bool()) {
        (Some("OPEN"), Some(true)) => "draft",
        (Some(state), _) => state,
        _ => "?",
    };
    let mut lines = vec![
        format!(
            "#{} {}  [{}]",
            pr["number"],
            pr["title"].as_str().unwrap_or_default(),
            state.to_lowercase()
        ),
        pr["url"].as_str().unwrap_or_default().to_owned(),
        format!(
            "{} -> {}  mergeable: {}  merge state: {}  review: {}  checks: {}",
            pr["head_ref_name"].as_str().unwrap_or_default(),
            pr["base_ref_name"].as_str().unwrap_or_default(),
            pr["mergeable"].as_str().unwrap_or("-"),
            pr["merge_state_status"].as_str().unwrap_or("-"),
            pr["review_decision"].as_str().unwrap_or("-"),
            pr["checks_rollup"].as_str().unwrap_or("-"),
        ),
    ];
    if pr["is_in_merge_queue"] == Value::Bool(true) {
        lines.push("in the merge queue".into());
    }
    if !pr["auto_merge"].is_null() {
        lines.push("auto-merge is on".into());
    }
    if let Some(stack) = v["stack"].as_object() {
        lines.push(format!(
            "stack #{}: position {} of {}",
            stack.get("number").unwrap_or(&Value::Null),
            stack.get("position").unwrap_or(&Value::Null),
            stack.get("size").unwrap_or(&Value::Null)
        ));
    }
    if let Some(url) = v["primary_preview"]["url"].as_str() {
        lines.push(format!(
            "preview: {url}  ({})",
            v["primary_preview"]["app"].as_str().unwrap_or_default()
        ));
    }
    lines.join("\n")
}

pub fn previews(v: &Value) -> String {
    let list = v["previews"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if list.is_empty() {
        return "no preview deployments".into();
    }
    let primary = &v["primary"];
    list.iter()
        .map(|p| {
            let marker = if p["app"] == primary["app"] && p["source"] == primary["source"] {
                "*"
            } else {
                " "
            };
            let sha = p["sha"].as_str().map_or("", |s| s.get(..7).unwrap_or(s));
            let head = if p["is_head"] == Value::Bool(true) {
                "head"
            } else {
                "older"
            };
            format!(
                "{marker} {:<14} {:<11} {:<10} {sha:<7} {head:<5}  {}",
                p["app"].as_str().unwrap_or_default(),
                p["state"].as_str().unwrap_or_default(),
                p["source"].as_str().unwrap_or_default(),
                p["url"]
                    .as_str()
                    .or_else(|| p["log_url"].as_str())
                    .unwrap_or("-"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The named app's preview, or the primary one.
pub fn pick_preview<'a>(v: &'a Value, app: Option<&str>) -> Result<&'a Value, String> {
    let Some(app) = app else {
        return match &v["primary"] {
            Value::Null => Err("no preview deployment is ready".into()),
            primary => Ok(primary),
        };
    };
    let list = v["previews"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    list.iter()
        .find(|p| {
            p["app"]
                .as_str()
                .is_some_and(|a| a.eq_ignore_ascii_case(app))
        })
        .ok_or_else(|| {
            let apps: Vec<&str> = list.iter().filter_map(|p| p["app"].as_str()).collect();
            format!("no preview for {app} (apps: {})", apps.join(", "))
        })
}

pub fn next_action(v: &Value) -> String {
    match v["action"]["label"].as_str() {
        Some(label) => {
            let how = if v["action"]["agent_step"] == Value::Bool(true) {
                "starts a new agent session"
            } else {
                "calls GitHub"
            };
            format!(
                "{label}  ({}; {how})",
                v["action"]["kind"].as_str().unwrap_or_default()
            )
        }
        None if v["known"] == Value::Bool(false) => format!(
            "nothing yet: the pull request lookup has not answered ({})",
            v["error"].as_str().unwrap_or("unknown")
        ),
        None => "nothing to do".into(),
    }
}

pub fn ran_next_action(v: &Value) -> String {
    let label = v["action"]["label"].as_str().unwrap_or("step");
    match v["started_agent"].as_str() {
        Some(agent) => format!("{label}: started agent {agent}"),
        None => format!("{label}: {}", v["github"]),
    }
}

fn actions(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let action = if a["action"].is_object() {
                &a["action"]
            } else {
                a
            };
            let kind = action["action"].as_str().unwrap_or("?");
            let target = action["workspace_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| action["name"].as_str().map(str::to_owned))
                .or_else(|| action["pr"]["number"].as_u64().map(|n| format!("#{n}")))
                .unwrap_or_default();
            let detail = action["status"]
                .as_str()
                .map(|s| format!(" -> {s}"))
                .unwrap_or_default();
            let ok = match a["ok"].as_bool() {
                Some(true) => "  ok",
                Some(false) => "  FAILED",
                None => "",
            };
            format!("  {kind} {target}{detail}{ok}")
        })
        .collect()
}

pub fn tick(v: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(reason) = v["skipped"].as_str() {
        lines.push(format!("skipped: {reason}"));
    } else {
        let mode = if v["dry_run"] == Value::Bool(true) {
            "dry run (set rules.armed to true to execute)"
        } else {
            "executed"
        };
        lines.push(format!("{mode}, {} ms", v["duration_ms"]));
        let planned = actions(&v["planned"]);
        lines.push(format!("planned: {}", planned.len()));
        lines.extend(planned);
        if v["dry_run"] != Value::Bool(true) {
            lines.push("executed:".into());
            lines.extend(actions(&v["executed"]));
        }
    }
    for error in v["errors"].as_array().into_iter().flatten() {
        lines.push(format!("error: {}", error.as_str().unwrap_or_default()));
    }
    lines.join("\n")
}

pub fn status(v: &Value) -> String {
    let projects = v["projects"]
        .as_array()
        .map(|p| {
            p.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "none".into());
    let mut lines = vec![format!(
        "enabled: {}  armed: {}  every {} ms  projects: {projects}  ticks: {}",
        v["enabled"], v["armed"], v["interval_ms"], v["ticks"],
    )];
    if v["last_tick"].is_object() {
        lines.push(format!("last tick at {}:", v["last_tick"]["at_ms"]));
        lines.push(tick(&v["last_tick"]));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_dry_run_lists_the_plan_and_says_how_to_arm() {
        let text = tick(&json!({
            "dry_run": true, "duration_ms": 12, "errors": ["viewer: offline"],
            "planned": [{ "action": "set_column", "workspace_id": "ws-1", "status": "in-review" }],
        }));
        assert!(text.contains("rules.armed"), "{text}");
        assert!(text.contains("set_column ws-1 -> in-review"), "{text}");
        assert!(text.contains("error: viewer: offline"), "{text}");
    }

    #[test]
    fn next_action_explains_an_unanswered_lookup() {
        assert_eq!(
            next_action(
                &json!({ "action": { "kind": "merge", "label": "Merge", "agent_step": false } })
            ),
            "Merge  (merge; calls GitHub)"
        );
        assert!(next_action(&json!({ "action": null, "known": false })).starts_with("nothing yet"));
        assert_eq!(
            pr_status(&json!({ "pull_request": null, "known": true })),
            "no pull request"
        );
    }

    #[test]
    fn previews_pick_the_primary_or_the_named_app() {
        let web = json!({ "app": "frontend", "url": "https://web.dev", "state": "success",
            "source": "deployment", "sha": "a8f65e7f2f", "is_head": true });
        let api = json!({ "app": "api", "url": null, "log_url": "https://ci/1", "state": "failure",
            "source": "status", "sha": "a8f65e7f2f", "is_head": true });
        let found = json!({ "previews": [web, api], "primary": web });
        assert_eq!(pick_preview(&found, None), Ok(&web));
        assert_eq!(pick_preview(&found, Some("API")), Ok(&api));
        assert!(pick_preview(&found, Some("docs")).is_err_and(|e| e.contains("frontend, api")));
        assert!(pick_preview(&json!({ "previews": [], "primary": null }), None).is_err());
        let text = previews(&found);
        assert!(text.starts_with("* frontend"), "{text}");
        assert!(text.contains("a8f65e7 head"), "{text}");
        assert!(text.contains("https://ci/1"), "{text}");
    }
}
