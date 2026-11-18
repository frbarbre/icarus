//! The Source Control panel's settings (`sourceControl.*`): the apps in a file's "Open in" menu and the
//! section layout (order and hidden sections, as Orca's `sourceControlSectionLayout`).

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext as _, Task};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenInApp {
    #[serde(default)]
    pub id: String,
    pub label: String,
    pub command: String,
}

/// Orca's `DEFAULT_OPEN_IN_APPLICATIONS`.
pub fn default_open_in() -> Vec<OpenInApp> {
    vec![OpenInApp {
        id: "vscode".into(),
        label: "VS Code".into(),
        command: "code".into(),
    }]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Review,
    Changes,
    AgentTurns,
    Commits,
}

impl Section {
    /// The default order.
    pub const ALL: [Section; 4] = [
        Section::Review,
        Section::Changes,
        Section::AgentTurns,
        Section::Commits,
    ];

    /// Orca's section ids.
    pub fn id(self) -> &'static str {
        match self {
            Section::Review => "pending-review",
            Section::Changes => "changes",
            Section::AgentTurns => "agent-turns",
            Section::Commits => "commits",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Review => "Review",
            Section::Changes => "Changes",
            Section::AgentTurns => "Agent turns",
            Section::Commits => "Commits",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.id() == id)
    }
}

/// The saved layout. Ids this panel doesn't draw (Orca's review sections) are kept, so saving never drops them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SectionLayout {
    pub order: Vec<String>,
    pub hidden: Vec<String>,
    /// The PR panel's own layout, which lives in the same setting and is written back untouched.
    pub pr: Option<Value>,
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

impl SectionLayout {
    pub fn from_settings(value: &Value) -> Self {
        Self {
            order: strings(&value["order"]),
            hidden: strings(&value["hidden"]),
            pr: value.get("pr").filter(|pr| pr.is_object()).cloned(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut value = json!({ "order": self.order, "hidden": self.hidden });
        if let Some(pr) = &self.pr {
            value["pr"] = pr.clone();
        }
        value
    }

    /// Saved ids first; a section the save doesn't name goes right after its default predecessor, so
    /// Review lands on top of a layout saved before it existed.
    fn full_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        for id in &self.order {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
        for (index, section) in Section::ALL.iter().enumerate() {
            if order.iter().any(|id| id == section.id()) {
                continue;
            }
            let at = Section::ALL[..index]
                .iter()
                .rev()
                .find_map(|previous| order.iter().position(|id| id == previous.id()))
                .map_or(0, |position| position + 1);
            order.insert(at, section.id().to_owned());
        }
        order
    }

    pub fn ordered(&self) -> Vec<Section> {
        self.full_order()
            .iter()
            .filter_map(|id| Section::parse(id))
            .collect()
    }

    pub fn visible(&self) -> Vec<Section> {
        self.ordered()
            .into_iter()
            .filter(|section| !self.is_hidden(*section))
            .collect()
    }

    pub fn is_hidden(&self, section: Section) -> bool {
        self.hidden.iter().any(|id| id == section.id())
    }

    pub fn toggled_hidden(&self, section: Section) -> Self {
        let mut layout = self.clone();
        if layout.is_hidden(section) {
            layout.hidden.retain(|id| id != section.id());
        } else {
            layout.hidden.push(section.id().to_owned());
        }
        layout
    }

    /// Orca's `moveSourceControlSection`: `section` goes before `before`, or last.
    pub fn moved(&self, section: Section, before: Option<Section>) -> Self {
        let mut order = self.full_order();
        order.retain(|id| id != section.id());
        let at = before
            .filter(|before| *before != section)
            .and_then(|before| order.iter().position(|id| id == before.id()))
            .unwrap_or(order.len());
        order.insert(at, section.id().to_owned());
        Self {
            order,
            hidden: self.hidden.clone(),
            pr: self.pr.clone(),
        }
    }

    /// A section header dropped on another: it takes the target's place, so dragging down lands after it
    /// and dragging up lands before it.
    pub fn dropped(&self, dragged: Section, target: Section) -> Self {
        let ordered = self.ordered();
        let (Some(from), Some(to)) = (
            ordered.iter().position(|item| *item == dragged),
            ordered.iter().position(|item| *item == target),
        ) else {
            return self.clone();
        };
        if from == to {
            return self.clone();
        }
        if from < to {
            self.moved(dragged, ordered.get(to + 1).copied())
        } else {
            self.moved(dragged, Some(target))
        }
    }

    /// Swaps `section` with its visible neighbour above (`up`) or below; `None` at an end.
    pub fn moved_by_one(&self, section: Section, up: bool) -> Option<Self> {
        let visible = self.visible();
        let index = visible.iter().position(|item| *item == section)?;
        if up {
            let above = *visible.get(index.checked_sub(1)?)?;
            Some(self.moved(section, Some(above)))
        } else {
            let below = *visible.get(index + 1)?;
            Some(self.moved(below, Some(section)))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceControlSettings {
    pub open_in: Vec<OpenInApp>,
    pub sections: SectionLayout,
}

impl Default for SourceControlSettings {
    fn default() -> Self {
        Self {
            open_in: default_open_in(),
            sections: SectionLayout::default(),
        }
    }
}

impl SourceControlSettings {
    /// Reads the `sourceControl` object; a missing `openIn` means Orca's default, an empty one no apps.
    pub fn from_settings(value: &Value) -> Self {
        let open_in = match &value["openIn"] {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| serde_json::from_value::<OpenInApp>(item.clone()).ok())
                .filter(|app| !app.command.trim().is_empty())
                .collect(),
            _ => default_open_in(),
        };
        Self {
            open_in,
            sections: SectionLayout::from_settings(&value["sections"]),
        }
    }
}

/// What the OS calls showing a file in its file manager, as Orca labels it.
pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(target_os = "windows") {
        "Reveal in File Explorer"
    } else {
        "Open Containing Folder"
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The program and arguments that open `path` in an app: a plain command gets the path as its argument,
/// and a command with spaces (e.g. `open -a Xcode`) runs through the shell, as Orca launches them.
pub fn launch_command(command: &str, path: &Path) -> (String, Vec<String>) {
    let command = command.trim();
    let path = path.to_string_lossy().into_owned();
    if command.contains(char::is_whitespace) {
        (
            "/bin/sh".into(),
            vec!["-c".into(), format!("{command} {}", shell_quote(&path))],
        )
    } else {
        (command.to_owned(), vec![path])
    }
}

async fn run(program: &str, args: &[String]) -> std::io::Result<std::process::Output> {
    util::command::new_command(program)
        .args(args)
        .output()
        .await
}

/// Opens `path` in `app`. A command missing from PATH falls back to `open -a <label>` on macOS.
pub fn open_in(app: &OpenInApp, path: &Path, cx: &App) -> Task<Result<()>> {
    let (program, args) = launch_command(&app.command, path);
    let label = app.label.clone();
    let path = path.to_path_buf();
    cx.background_spawn(async move {
        let output = match run(&program, &args).await {
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && cfg!(target_os = "macos") =>
            {
                let args = vec![
                    "-a".to_owned(),
                    label.clone(),
                    path.to_string_lossy().into_owned(),
                ];
                run("open", &args)
                    .await
                    .with_context(|| format!("`{program}` isn't on PATH"))?
            }
            other => other.with_context(|| format!("couldn't run `{program}`"))?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if stderr.is_empty() {
                bail!("`{program}` exited with {}", output.status);
            }
            bail!("{stderr}");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_keep_unknown_ids_and_fill_in_missing_sections() {
        let layout = SectionLayout::from_settings(&json!({
            "order": ["pending-review", "commits", "agent-turns"],
            "hidden": ["agent-turns"],
        }));
        assert_eq!(
            layout.ordered(),
            [
                Section::Review,
                Section::Changes,
                Section::Commits,
                Section::AgentTurns
            ],
            "a section the save doesn't name follows its default predecessor"
        );
        assert_eq!(
            layout.visible(),
            [Section::Review, Section::Changes, Section::Commits]
        );
        assert_eq!(
            SectionLayout::from_settings(
                &json!({ "order": ["changes", "agent-turns", "commits"] })
            )
            .ordered(),
            Section::ALL,
            "a layout saved before Review existed gets it on top"
        );

        let moved = layout.moved(Section::Changes, Some(Section::Commits));
        assert_eq!(
            moved.order,
            ["pending-review", "changes", "commits", "agent-turns"],
            "Orca's ids stay where they were"
        );
        let last = layout.moved(Section::Commits, None);
        assert_eq!(
            last.ordered(),
            [
                Section::Review,
                Section::Changes,
                Section::AgentTurns,
                Section::Commits
            ]
        );

        let shown = layout.toggled_hidden(Section::AgentTurns);
        assert!(shown.hidden.is_empty());
        assert!(
            shown
                .toggled_hidden(Section::Changes)
                .is_hidden(Section::Changes)
        );

        let with_pr = SectionLayout::from_settings(&json!({ "pr": { "order": ["pr-checks"] } }));
        assert_eq!(
            with_pr
                .moved(Section::Commits, None)
                .toggled_hidden(Section::Changes)
                .to_value()["pr"],
            json!({ "order": ["pr-checks"] }),
            "saving the Source Control layout keeps the PR panel's"
        );

        let default = SectionLayout::default();
        assert_eq!(default.ordered(), Section::ALL);
        assert_eq!(
            default
                .moved_by_one(Section::Commits, true)
                .map(|layout| layout.ordered()),
            Some(vec![
                Section::Review,
                Section::Changes,
                Section::Commits,
                Section::AgentTurns
            ])
        );
        assert_eq!(
            default
                .moved_by_one(Section::Changes, false)
                .map(|layout| layout.ordered()),
            Some(vec![
                Section::Review,
                Section::AgentTurns,
                Section::Changes,
                Section::Commits
            ])
        );
        assert_eq!(default.moved_by_one(Section::Review, true), None);
        assert_eq!(
            default
                .dropped(Section::Changes, Section::Commits)
                .ordered(),
            [
                Section::Review,
                Section::AgentTurns,
                Section::Commits,
                Section::Changes
            ],
            "dragged down onto the last section, it goes last"
        );
        assert_eq!(
            default
                .dropped(Section::Commits, Section::Changes)
                .ordered(),
            [
                Section::Review,
                Section::Commits,
                Section::Changes,
                Section::AgentTurns
            ],
            "dragged up, it goes before the target"
        );
        assert_eq!(default.dropped(Section::Commits, Section::Commits), default);
        let gapped = SectionLayout::from_settings(&json!({
            "order": ["pending-review", "commits", "agent-turns", "changes"],
            "hidden": ["agent-turns"],
        }));
        assert_eq!(
            gapped
                .moved_by_one(Section::Changes, true)
                .map(|layout| layout.visible()),
            Some(vec![Section::Review, Section::Changes, Section::Commits]),
            "hidden sections are skipped"
        );
    }

    #[test]
    fn open_in_apps_default_to_vs_code_and_launch_like_orca() {
        assert_eq!(
            SourceControlSettings::from_settings(&Value::Null).open_in,
            default_open_in()
        );
        let settings = SourceControlSettings::from_settings(&json!({
            "openIn": [
                { "id": "zed", "label": "Zed", "command": "zed" },
                { "label": "Broken", "command": " " },
            ],
        }));
        assert_eq!(settings.open_in.len(), 1);
        assert!(
            SourceControlSettings::from_settings(&json!({ "openIn": [] }))
                .open_in
                .is_empty()
        );

        assert_eq!(
            launch_command("cursor", Path::new("/w/a b.rs")),
            ("cursor".to_owned(), vec!["/w/a b.rs".to_owned()])
        );
        assert_eq!(
            launch_command("open -a Xcode", Path::new("/w/it's.swift")),
            (
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    r"open -a Xcode '/w/it'\''s.swift'".to_owned()
                ]
            )
        );
    }
}
