//! Orca's environment recipes from the CLI: the local server runs a recipe's commands (it owns the
//! project and its trust); this side pairs with the server the recipe started and opens the
//! workspace there, keeping it as a saved server named after the workspace.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use icarus_client::servers::{SavedServer, SavedServers};
use serde_json::{Value, json};

/// Recipes can boot a whole machine.
const RECIPE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Subcommand)]
pub enum RecipeCommand {
    /// List a project's environment recipes (`environmentRecipes` in its orca.yaml).
    List { project: String },
    /// List the environments recipes started that haven't been destroyed.
    Envs,
    /// Run a recipe's suspend command.
    Suspend { instance: String },
    /// Run a recipe's resume command and pair with the server again if it moved.
    Resume { instance: String },
    /// Run a recipe's destroy command and forget the saved server.
    Destroy { instance: String },
}

/// A pairing offer from `icarus://pair?address=HOST:PORT&code=CODE[&fingerprint=SHA256]`.
#[derive(Debug, PartialEq, Eq)]
pub struct Offer {
    pub address: String,
    pub code: String,
    pub fingerprint: Option<String>,
}

pub fn parse_offer(text: &str) -> Option<Offer> {
    let query = text.trim().strip_prefix("icarus://pair?")?;
    let mut offer = Offer {
        address: String::new(),
        code: String::new(),
        fingerprint: None,
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = value.replace("%3A", ":").replace("%3a", ":");
        match key {
            "address" => offer.address = value,
            "code" => offer.code = value,
            "fingerprint" => offer.fingerprint = Some(value),
            _ => {}
        }
    }
    (!offer.address.is_empty() && !offer.code.is_empty()).then_some(offer)
}

/// Pairs with a server's `--listen` address using a one-time code, returning what to save.
pub async fn pair(
    name: &str,
    address: &str,
    code: &str,
    fingerprint: Option<String>,
    label: Option<String>,
    ssh: Option<String>,
) -> Result<SavedServer> {
    let credentials = icarus_client::Credentials::PairCode {
        code: code.to_owned(),
        fingerprint,
        label: Some(label.unwrap_or_else(crate::host_name)),
    };
    let (client, _) = Client::connect_tcp(address, &credentials, "icarus-cli")
        .await
        .with_context(|| format!("pair with {address}"))?;
    Ok(SavedServer {
        name: name.to_owned(),
        address: address.to_owned(),
        token: Some(
            client
                .issued_token
                .clone()
                .context("the server accepted the code but sent no token")?,
        ),
        fingerprint: client.server_fingerprint.clone(),
        client_id: client.paired_client_id.clone(),
        command: None,
        ssh,
    })
}

/// The saved server for a recipe's connection: paired for an Icarus server, an `ssh://` one otherwise.
async fn saved_server(name: &str, connection: &Value) -> Result<SavedServer> {
    match connection["type"].as_str() {
        Some("icarus-server") => {
            let offer = connection["pairingCode"]
                .as_str()
                .and_then(parse_offer)
                .context("the recipe's pairing code is not icarus://pair?address=…&code=…")?;
            pair(
                name,
                &offer.address,
                &offer.code,
                offer.fingerprint,
                None,
                None,
            )
            .await
        }
        Some("ssh") => {
            let target = &connection["target"];
            let destination = match target["configHost"].as_str() {
                Some(host) => host.to_owned(),
                None => {
                    let host = target["host"].as_str().context("ssh target has no host")?;
                    if target["port"].as_u64().is_some_and(|port| port != 22) {
                        bail!(
                            "the recipe's SSH target uses port {}; give it a configHost from ~/.ssh/config instead",
                            target["port"]
                        );
                    }
                    match target["username"].as_str().filter(|user| !user.is_empty()) {
                        Some(user) => format!("{user}@{host}"),
                        None => host.to_owned(),
                    }
                }
            };
            Ok(SavedServer {
                name: name.to_owned(),
                address: format!("ssh://{destination}"),
                token: None,
                fingerprint: None,
                client_id: None,
                command: None,
                ssh: None,
            })
        }
        other => bail!("unknown recipe connection type {other:?}"),
    }
}

fn server_name(workspace: &str) -> String {
    let slug: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("env-{}", slug.trim_matches('-'))
}

fn save(server: &SavedServer) -> Result<()> {
    let mut saved = SavedServers::load()?;
    saved.upsert(server.clone());
    saved.save()
}

fn forget(name: &str) -> Result<()> {
    let mut saved = SavedServers::load()?;
    if saved.remove(name).is_some() {
        saved.save()?;
    }
    Ok(())
}

/// `workspace create --recipe`: start the environment, pair with it, and create the workspace there.
/// A failure after the environment exists destroys it again, as Orca's failed-start cleanup does.
pub async fn create(local: &Client, recipe: &str, mut create: Value, trust: bool) -> Result<Value> {
    let name = create["name"]
        .as_str()
        .context("a workspace in an environment needs a name")?
        .to_owned();
    let started = local
        .request_timeout(
            "recipe.create",
            json!({
                "project": create["project"],
                "recipe": recipe,
                "name": name,
                "branch": create["branch"],
                "trust": trust,
            }),
            RECIPE_TIMEOUT,
        )
        .await?;
    let environment = &started["environment"];
    let instance = environment["context"]["instance_id"]
        .as_str()
        .context("the server returned no environment id")?
        .to_owned();
    let opened = open(&name, environment, &mut create).await;
    match opened {
        Ok((server, workspace)) => {
            local
                .request(
                    "recipe.bind",
                    json!({ "instance": instance, "server": server, "workspace": workspace["id"] }),
                )
                .await?;
            Ok(json!({ "environment": instance, "server": server, "workspace": workspace }))
        }
        Err(error) => {
            if let Err(cleanup) = local
                .request_timeout(
                    "recipe.destroy",
                    json!({ "instance": instance }),
                    RECIPE_TIMEOUT,
                )
                .await
            {
                eprintln!("warning: destroying environment {instance} failed: {cleanup:#}");
            }
            forget(&server_name(&name))?;
            Err(error.context(format!("environment {instance} was destroyed again")))
        }
    }
}

async fn open(name: &str, environment: &Value, create: &mut Value) -> Result<(String, Value)> {
    let connection = &environment["connection"];
    let server = saved_server(&server_name(name), connection).await?;
    save(&server)?;
    let (remote, _) = Client::connect_saved(&server, "icarus-cli").await?;
    let root = connection["projectRoot"]
        .as_str()
        .context("the recipe returned no projectRoot")?;
    let project = remote
        .request("project.add", json!({ "path": root }))
        .await
        .with_context(|| format!("add {root} on the environment's server"))?;
    let provisioned = environment["result"]["checkoutMode"] == "provisioned-root";
    if provisioned {
        // The checkout the recipe made is the workspace; there is no worktree to add.
        return Ok((server.name, json!({ "project": project })));
    }
    create["project"] = project["id"].clone();
    let workspace = remote
        .request_timeout("workspace.create", create.clone(), Duration::from_secs(300))
        .await?;
    Ok((server.name, workspace))
}

pub async fn run(cli: &crate::Cli, command: &RecipeCommand) -> Result<()> {
    let client = crate::connect(cli).await?;
    match command {
        RecipeCommand::List { project } => {
            let value = client
                .request("recipe.list", json!({ "project": project }))
                .await?;
            crate::print(cli, &value, |v| {
                let mut lines: Vec<String> = v["recipes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|recipe| {
                        format!(
                            "{}\t{}\t{}",
                            recipe["id"].as_str().unwrap_or("?"),
                            recipe["name"].as_str().unwrap_or(""),
                            recipe["create"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                for problem in v["problems"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "recipe {}: {}",
                        problem["index"],
                        problem["message"].as_str().unwrap_or("")
                    ));
                }
                if lines.is_empty() {
                    lines.push("no environment recipes".into());
                } else if v["trusted"] != true {
                    lines.push(format!(
                        "not trusted yet: review with `icarus project scripts {0}`, then `icarus project trust {0}`",
                        v["project"].as_str().unwrap_or("<id>")
                    ));
                }
                lines.join("\n")
            });
        }
        RecipeCommand::Envs => {
            let value = client.request("recipe.environments", json!({})).await?;
            crate::print(cli, &value, |v| {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .map(|env| {
                        format!(
                            "{}\t{}\t{}\t{}{}",
                            env["context"]["instance_id"].as_str().unwrap_or("?"),
                            env["context"]["recipe_id"].as_str().unwrap_or("?"),
                            env["context"]["workspace_name"].as_str().unwrap_or(""),
                            env["server"].as_str().unwrap_or("-"),
                            if env["suspended"] == true {
                                "\tsuspended"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        RecipeCommand::Suspend { instance }
        | RecipeCommand::Resume { instance }
        | RecipeCommand::Destroy { instance } => {
            let method = match command {
                RecipeCommand::Suspend { .. } => "recipe.suspend",
                RecipeCommand::Resume { .. } => "recipe.resume",
                _ => "recipe.destroy",
            };
            let value = client
                .request_timeout(method, json!({ "instance": instance }), RECIPE_TIMEOUT)
                .await?;
            let environment = &value["environment"];
            if let Some(server) = environment["server"].as_str() {
                match method {
                    "recipe.destroy" => forget(server)?,
                    "recipe.resume" if value["skipped"] != true => {
                        let resumed = saved_server(server, &environment["connection"]).await?;
                        save(&resumed)?;
                    }
                    _ => {}
                }
            }
            crate::print(cli, &value, |v| {
                let verb = method.trim_start_matches("recipe.");
                if v["skipped"] == true {
                    format!("the recipe has no {verb} command")
                } else {
                    format!("{verb}: done")
                }
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_and_server_names() {
        assert_eq!(
            parse_offer("icarus://pair?address=10.0.0.2%3A7878&code=AB-CD"),
            Some(Offer {
                address: "10.0.0.2:7878".into(),
                code: "AB-CD".into(),
                fingerprint: None,
            })
        );
        assert_eq!(parse_offer("orca://pair?code=x"), None);
        assert_eq!(server_name("Fix Login!"), "env-fix-login");
    }

    #[tokio::test]
    async fn an_ssh_connection_becomes_an_ssh_server() {
        let server = saved_server(
            "env-a",
            &json!({ "type": "ssh", "target": { "label": "vm", "host": "10.0.0.3", "port": 22, "username": "dev" }, "projectRoot": "/r" }),
        )
        .await
        .expect("ssh");
        assert_eq!(server.address, "ssh://dev@10.0.0.3");
        let refused = saved_server(
            "env-a",
            &json!({ "type": "ssh", "target": { "label": "vm", "host": "h", "port": 2222, "username": "dev" }, "projectRoot": "/r" }),
        )
        .await;
        assert!(refused.is_err());
    }
}
