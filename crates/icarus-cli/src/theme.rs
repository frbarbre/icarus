//! `icarus theme import`: VS Code and Warp themes as Zed themes for the app (plan D10).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use icarus_settings::theme_import::{self, Source};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum ThemeCommand {
    /// Convert a VS Code theme (JSON) or a Warp theme (YAML) into a theme the app can use.
    /// Runs on this machine, where the app lives, even with `--server`.
    Import {
        kind: ThemeKind,
        file: PathBuf,
        /// The theme's name in the app (default: the name in the file, else the file's name).
        #[arg(long)]
        name: Option<String>,
        /// Also make it the app's theme for its appearance (light or dark).
        #[arg(long)]
        select: bool,
        /// The app's config directory (default: `~/.config/icarus`).
        #[arg(long, value_name = "DIR")]
        zed_dir: Option<PathBuf>,
        /// Report what would be written; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ThemeKind {
    Vscode,
    Warp,
}

pub fn run(command: &ThemeCommand) -> Result<Value> {
    let ThemeCommand::Import {
        kind,
        file,
        name,
        select,
        zed_dir,
        dry_run,
    } = command;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let source = match kind {
        ThemeKind::Vscode => Source::Vscode,
        ThemeKind::Warp => Source::Warp,
    };
    let fallback = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Imported".to_owned());
    let converted = theme_import::convert(source, &text, name.as_deref(), &fallback)?;
    let dir = zed_dir
        .clone()
        .or_else(icarus_settings::zed::config_dir)
        .context("no app config directory; pass --zed-dir")?;
    let installed = theme_import::install(&dir, &converted, *select, *dry_run)?;
    Ok(json!({
        "name": converted.name,
        "appearance": converted.appearance,
        "theme": installed.theme,
        "selected": select,
        "settings": installed.settings,
        "backup": installed.backup,
        "dry_run": dry_run,
        "notes": converted.notes,
    }))
}

pub fn summary(value: &Value) -> String {
    let name = value["name"].as_str().unwrap_or_default();
    let appearance = value["appearance"].as_str().unwrap_or_default();
    let path = value["theme"].as_str().unwrap_or_default();
    let verb = if value["dry_run"] == true {
        "Would write"
    } else {
        "Wrote"
    };
    let mut lines = vec![format!("{verb} {appearance} theme \"{name}\" to {path}")];
    if value["selected"] == true {
        lines.push(format!("Selected it as the {appearance} theme."));
    } else {
        lines.push("Pick it in the app's theme selector, or pass --select.".to_owned());
    }
    if let Some(backup) = value["backup"].as_str() {
        lines.push(format!(
            "Your settings with comments were saved to {backup}"
        ));
    }
    for note in value["notes"].as_array().into_iter().flatten() {
        if let Some(note) = note.as_str() {
            lines.push(format!("note: {note}"));
        }
    }
    lines.join("\n")
}
