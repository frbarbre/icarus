use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

struct Skill {
    name: &'static str,
    description: &'static str,
    guide: &'static str,
}

const SKILLS: &[Skill] = &[
    Skill {
        name: "icarus-automations",
        description: "Set up, change or debug an Icarus automation script: the per-project script that moves cards between statuses, starts agents and shows card buttons. Use when the user wants something to happen automatically in Icarus, or asks why an automation did or didn't act.",
        guide: include_str!("../templates/skills/icarus-automations.md"),
    },
    Skill {
        name: "icarus-linear",
        description: "Read, search or assign Linear issues from an Icarus workspace (ICARUS_WORKSPACE_ID is set) with the `icarus issue` CLI. Use whenever work starts from a Linear issue in Icarus; prefer it over Orca's Linear CLI or a Linear connector there.",
        guide: include_str!("../templates/skills/icarus-linear.md"),
    },
];

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Skills that ship with Icarus, and whether each is installed.
    List,
    /// Print a skill's full guide.
    Get { name: String },
    /// Install skill stubs into ~/.agents/skills, linked from ~/.claude/skills.
    Install {
        /// Skills to install; all when omitted.
        names: Vec<String>,
        /// Replace an edited copy, keeping it as SKILL.md.bak.
        #[arg(long)]
        force: bool,
    },
}

fn stub(skill: &Skill) -> String {
    format!(
        "---\nname: {}\ndescription: {}\n---\n\nRun `icarus skill get {}` and follow the guide it prints; it always matches the installed Icarus.\n",
        skill.name, skill.description, skill.name
    )
}

fn find(name: &str) -> Result<&'static Skill> {
    SKILLS
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no skill named {name}; see `icarus skill list`"))
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("no home directory")
}

fn shared_dir() -> Result<PathBuf> {
    Ok(home()?.join(".agents/skills"))
}

fn claude_dir() -> Result<PathBuf> {
    Ok(match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => home()?.join(".claude"),
    }
    .join("skills"))
}

fn state(skill: &Skill) -> Result<&'static str> {
    let path = shared_dir()?.join(skill.name).join("SKILL.md");
    Ok(match fs::read_to_string(&path) {
        Ok(text) if text == stub(skill) => "installed",
        Ok(_) => "edited",
        Err(_) => "not installed",
    })
}

fn link(target: &Path, at: &Path, force: bool) -> Result<&'static str> {
    if fs::read_link(at).is_ok_and(|current| current == target) {
        return Ok("linked");
    }
    if fs::symlink_metadata(at).is_ok() {
        if !force {
            return Ok("left alone (exists; --force replaces it)");
        }
        let backup = at.with_extension("bak");
        fs::rename(at, &backup).with_context(|| format!("backing up {}", at.display()))?;
    }
    if let Some(parent) = at.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, at).with_context(|| format!("linking {}", at.display()))?;
    #[cfg(not(unix))]
    anyhow::bail!("linking skills is only supported on macOS and Linux");
    Ok("linked")
}

fn install(skill: &Skill, force: bool) -> Result<serde_json::Value> {
    let dir = shared_dir()?.join(skill.name);
    let file = dir.join("SKILL.md");
    let result = match state(skill)? {
        "installed" => "up to date",
        "edited" if !force => "edited; left alone (--force replaces it, keeping SKILL.md.bak)",
        previous => {
            fs::create_dir_all(&dir)?;
            if previous == "edited" {
                fs::rename(&file, dir.join("SKILL.md.bak"))?;
            }
            fs::write(&file, stub(skill)).with_context(|| format!("writing {}", file.display()))?;
            "installed"
        }
    };
    let claude = link(&dir, &claude_dir()?.join(skill.name), force)?;
    Ok(json!({ "name": skill.name, "path": file, "result": result, "claude": claude }))
}

pub fn run(cli: &super::Cli, command: &SkillCommand) -> Result<()> {
    match command {
        SkillCommand::List => {
            let list = SKILLS
                .iter()
                .map(|s| {
                    Ok(json!({ "name": s.name, "state": state(s)?, "description": s.description }))
                })
                .collect::<Result<Vec<_>>>()?;
            super::print(cli, &json!(list), |v| {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        format!(
                            "{}  ({})",
                            s["name"].as_str().unwrap_or("?"),
                            s["state"].as_str().unwrap_or("?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        SkillCommand::Get { name } => print!("{}", find(name)?.guide),
        SkillCommand::Install { names, force } => {
            let chosen = if names.is_empty() {
                SKILLS.iter().collect::<Vec<_>>()
            } else {
                names.iter().map(|n| find(n)).collect::<Result<Vec<_>>>()?
            };
            let results = chosen
                .into_iter()
                .map(|s| install(s, *force))
                .collect::<Result<Vec<_>>>()?;
            super::print(cli, &json!(results), |v| {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        format!(
                            "{}: {}; ~/.claude/skills {}",
                            s["name"].as_str().unwrap_or("?"),
                            s["result"].as_str().unwrap_or("?"),
                            s["claude"].as_str().unwrap_or("?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
    }
    Ok(())
}
