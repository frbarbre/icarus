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
        Some(label) => format!(
            "{label}  ({}; decided by run {})",
            v["action"]["kind"].as_str().unwrap_or_default(),
            v["decided_by"].as_str().unwrap_or("?")
        ),
        None if v["known"] == Value::Bool(false) => format!(
            "nothing yet: the pull request lookup has not answered ({})",
            v["error"].as_str().unwrap_or("unknown")
        ),
        None => "no button: the project's automation script gave this workspace none".into(),
    }
}

pub fn ran_next_action(v: &Value) -> String {
    let label = v["action"]["label"].as_str().unwrap_or("step");
    match v["started_agent"].as_str() {
        Some(agent) => format!("{label}: started agent {agent}"),
        None => format!("{label}: {}", v["github"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn next_action_explains_an_unanswered_lookup() {
        assert_eq!(
            next_action(
                &json!({ "action": { "kind": "script:merge", "label": "Merge" }, "decided_by": "run-1" })
            ),
            "Merge  (script:merge; decided by run run-1)"
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
