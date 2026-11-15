use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum IssueCommand {
    /// Show a Linear issue: title, state, assignee, labels, branch and URL.
    Show {
        /// Identifier, e.g. ABC-12.
        id: String,
        /// Skip the server's cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Open a Linear issue in the browser and print its URL.
    Open {
        id: String,
        /// Only print the URL.
        #[arg(long)]
        no_browser: bool,
    },
    /// Refresh linked issues now (all, or one), re-running `linear.api_key_command` if it failed.
    Refresh { id: Option<String> },
    /// Whether Linear is configured on the server.
    Status,
}

pub async fn run(cli: &super::Cli, client: &Client, command: &IssueCommand) -> Result<()> {
    let timeout = Duration::from_secs(60);
    match command {
        IssueCommand::Show { id, refresh } => {
            let value = client
                .request_timeout(
                    "issue.get",
                    json!({ "id": id, "refresh": refresh }),
                    timeout,
                )
                .await?;
            super::print(cli, &value, show_text);
        }
        IssueCommand::Open { id, no_browser } => {
            let value = client
                .request_timeout("issue.get", json!({ "id": id }), timeout)
                .await?;
            let Some(url) = value["url"].as_str().filter(|u| !u.is_empty()) else {
                bail!("Linear returned no URL for {id}");
            };
            if !no_browser {
                super::open_in_browser(url)?;
            }
            super::print(cli, &json!({ "url": url }), |_| url.to_owned());
        }
        IssueCommand::Refresh { id } => {
            let value = client
                .request_timeout("issue.refresh", json!({ "id": id }), timeout)
                .await?;
            super::print(cli, &value, |v| match id {
                Some(_) => show_text(v),
                None => format!(
                    "refreshed {} issue(s); {} workspace card(s) changed",
                    v["refreshed"].as_u64().unwrap_or(0),
                    v["changed"].as_array().map(Vec::len).unwrap_or(0)
                ),
            });
        }
        IssueCommand::Status => {
            let value = client.request("issue.status", json!({})).await?;
            super::print(cli, &value, |v| {
                if v["configured"].as_bool() == Some(true) {
                    format!("Linear configured ({})", v["key"].as_str().unwrap_or("?"))
                } else {
                    format!(
                        "Linear not configured ({}): set LINEAR_API_KEY or `linear.api_key_command`",
                        v["key"].as_str().unwrap_or("?")
                    )
                }
            });
        }
    }
    Ok(())
}

fn show_text(issue: &Value) -> String {
    let text = |key: &str| issue[key].as_str().unwrap_or_default().to_owned();
    let mut lines = vec![format!("{}  {}", text("identifier"), text("title"))];
    let mut facts = Vec::new();
    if let Some(state) = issue["state"]["name"].as_str() {
        facts.push(state.to_owned());
    }
    if let Some(priority) = issue["priority_label"].as_str().filter(|p| !p.is_empty()) {
        facts.push(priority.to_owned());
    }
    if let Some(assignee) = issue["assignee"]["name"].as_str() {
        facts.push(format!("@{assignee}"));
    }
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["name"].as_str())
        .collect();
    if !labels.is_empty() {
        facts.push(format!("[{}]", labels.join(", ")));
    }
    if !facts.is_empty() {
        lines.push(facts.join("  "));
    }
    if let Some(branch) = issue["branch_name"].as_str() {
        lines.push(format!("branch: {branch}"));
    }
    lines.push(text("url"));
    if let Some(description) = issue["description"].as_str() {
        lines.push(String::new());
        lines.push(description.to_owned());
    }
    if issue["stale"].as_bool() == Some(true) {
        lines.push(format!(
            "(cached; Linear is unreachable: {})",
            issue["error"].as_str().unwrap_or("unknown error")
        ));
    }
    lines.join("\n")
}
