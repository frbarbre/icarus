use std::io::{BufRead as _, Read as _, Write as _};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum LinearCommand {
    /// Save a Linear personal API key on the server (Keychain on macOS), after checking it with Linear.
    /// Reads the key from a hidden prompt, or from stdin when it isn't a terminal.
    Login,
    /// Remove the saved key. A key from LINEAR_API_KEY or `linear.api_key_command` is left alone.
    Logout,
    /// Whether Linear has a key, where it comes from and whose it is.
    Status,
}

pub async fn run(cli: &super::Cli, client: &Client, command: &LinearCommand) -> Result<()> {
    match command {
        LinearCommand::Login => {
            let key = read_key()?;
            let value = client
                .request_timeout(
                    "linear.set_api_key",
                    json!({ "key": key }),
                    Duration::from_secs(60),
                )
                .await?;
            super::print(cli, &value, |v| {
                let mut text = format!(
                    "Signed in to Linear as {}",
                    v["name"].as_str().unwrap_or("?")
                );
                if let Some(other) = v["source"].as_str().filter(|s| *s != "stored") {
                    text.push_str(&format!(
                        "\n(saved, but {} wins while it is set)",
                        source_text(other)
                    ));
                }
                text
            });
        }
        LinearCommand::Logout => {
            let value = client.request("linear.clear_api_key", json!({})).await?;
            super::print(cli, &value, |v| {
                let mut text = "Removed the saved Linear key".to_owned();
                if let Some(other) = v["source"].as_str() {
                    text.push_str(&format!("; Linear still uses {}", source_text(other)));
                }
                text
            });
        }
        LinearCommand::Status => {
            let value = client
                .request_timeout("linear.status", json!({}), Duration::from_secs(30))
                .await?;
            super::print(cli, &value, status_text);
        }
    }
    Ok(())
}

fn source_text(source: &str) -> &'static str {
    match source {
        "env" => "LINEAR_API_KEY",
        "command" => "`linear.api_key_command`",
        _ => "the saved key",
    }
}

fn status_text(v: &Value) -> String {
    let source = v["source"].as_str();
    if v["rejected"].as_bool() == Some(true) {
        return format!(
            "Linear rejected the key from {}: run `icarus linear login` with a new one",
            source_text(source.unwrap_or_default())
        );
    }
    if v["configured"].as_bool() != Some(true) {
        return "Linear is not connected: run `icarus linear login`".to_owned();
    }
    let from = match source {
        Some("stored") => format!("saved key ({})", v["store"].as_str().unwrap_or("?")),
        other => source_text(other.unwrap_or_default()).to_owned(),
    };
    match v["viewer"].as_str() {
        Some(name) => format!("Signed in to Linear as {name} via {from}"),
        None => format!("Linear configured via {from}"),
    }
}

fn read_key() -> Result<String> {
    let stdin = std::io::stdin();
    let key = if rustix::termios::isatty(&stdin) {
        prompt_hidden("Linear API key: ")?
    } else {
        let mut text = String::new();
        stdin
            .lock()
            .read_to_string(&mut text)
            .context("read stdin")?;
        text
    };
    let key = key.trim().to_owned();
    if key.is_empty() {
        bail!(
            "no key given; create a personal API key in Linear under Settings → Security & access"
        );
    }
    Ok(key)
}

fn prompt_hidden(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    let original = rustix::termios::tcgetattr(&stdin).context("read terminal mode")?;
    let mut quiet = original.clone();
    quiet.local_modes.remove(rustix::termios::LocalModes::ECHO);
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &quiet)
        .context("hide input")?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let restored =
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &original);
    eprintln!();
    read.context("read the key")?;
    restored.context("restore the terminal")?;
    Ok(line)
}
