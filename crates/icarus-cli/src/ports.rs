use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum PortsCommand {
    /// List the workspace's listening services: port, kind, framework, label and URL.
    List { workspace: String },
    /// Print the best URL for a kind (frontend by default); `--browser` opens it.
    Open {
        workspace: String,
        /// frontend, api, docs, storybook, database, cache, other or any.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        browser: bool,
    },
}

pub async fn run(cli: &super::Cli, client: &Client, command: &PortsCommand) -> Result<()> {
    match command {
        PortsCommand::List { workspace } => {
            let value = client
                .request_timeout(
                    "ports.list",
                    json!({ "workspace": workspace }),
                    Duration::from_secs(60),
                )
                .await?;
            super::print(cli, &value, list_text);
        }
        PortsCommand::Open {
            workspace,
            kind,
            browser,
        } => {
            let value = client
                .request_timeout(
                    "ports.primary",
                    json!({ "workspace": workspace, "kind": kind }),
                    Duration::from_secs(60),
                )
                .await?;
            let Some(url) = value["url"].as_str() else {
                let wanted = kind.as_deref().unwrap_or("frontend");
                bail!("no {wanted} service is running in {workspace}");
            };
            if *browser {
                super::open_in_browser(url)?;
            }
            super::print(cli, &value, |_| url.to_string());
        }
    }
    Ok(())
}

fn list_text(value: &Value) -> String {
    let rows: Vec<String> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let kind = match s["framework"].as_str() {
                Some(framework) => format!("{} ({framework})", s["kind"].as_str().unwrap_or("?")),
                None => s["kind"].as_str().unwrap_or("?").to_string(),
            };
            let pid = s["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into());
            format!(
                "{} {:>5}  {:<22} {:<20} {}  pid {} {}",
                if s["primary"] == true { "*" } else { " " },
                s["port"],
                kind,
                s["label"].as_str().unwrap_or("-"),
                s["url"].as_str().unwrap_or_default(),
                pid,
                s["process"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    if rows.is_empty() {
        "no services running".into()
    } else {
        rows.join("\n")
    }
}
