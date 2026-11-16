//! Worktrees the server discovered: the origin badge on cards and Orca's "New worktrees" inbox row,
//! where newly found external worktrees wait for Show, Hide or Show all from their source.

use std::sync::Arc;

use collections::HashMap;
use gpui::{AnyElement, Context, SharedString};
use gpui_tokio::Tokio;
use icarus_client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use ui::{Tooltip, prelude::*};

use crate::WorkspacesPanel;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InboxWorktree {
    pub id: String,
    pub name: String,
    pub path: String,
    pub origin: String,
    #[serde(default)]
    pub source_label: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InboxEntry {
    pub project: String,
    #[serde(default)]
    pub project_name: String,
    pub workspaces: Vec<InboxWorktree>,
}

#[derive(Debug, Clone, PartialEq)]
struct Origin {
    origin: String,
    label: String,
}

/// What the workspace list and `workspace.inbox` say about where worktrees came from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovered {
    origins: HashMap<String, Origin>,
    inbox: Vec<InboxEntry>,
}

/// The project's inbox; an older server without discovery has none.
pub async fn fetch_inbox(client: &Client) -> Value {
    client
        .request("workspace.inbox", json!({}))
        .await
        .unwrap_or_default()
}

impl Discovered {
    pub fn new(workspaces: &Value, inbox: Value) -> Self {
        let origins = workspaces
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| {
                let origin = w["origin"].as_str()?;
                Some((
                    w["id"].as_str()?.to_owned(),
                    Origin {
                        origin: origin.to_owned(),
                        label: w["source_label"].as_str().unwrap_or(origin).to_owned(),
                    },
                ))
            })
            .collect();
        Self {
            origins,
            inbox: serde_json::from_value(inbox).unwrap_or_default(),
        }
    }

    fn forget(&mut self, ids: &[String]) {
        for entry in &mut self.inbox {
            entry.workspaces.retain(|w| !ids.contains(&w.id));
        }
        self.inbox.retain(|entry| !entry.workspaces.is_empty());
    }

    /// Whether the card's worktree was found on disk rather than made by Icarus.
    pub fn is_discovered(&self, workspace_id: &str) -> bool {
        self.origins
            .get(workspace_id)
            .is_some_and(|origin| origin.origin != "native")
    }

    /// A small tag naming where a card's worktree came from; native ones (Icarus's own) have none.
    pub fn origin_badge(&self, workspace_id: &str) -> Option<AnyElement> {
        let origin = self.origins.get(workspace_id)?;
        let text = match origin.origin.as_str() {
            "native" => return None,
            "adopted" => "adopted".to_owned(),
            "claude" => "claude".to_owned(),
            "gsd" => "gsd".to_owned(),
            "external" => "external".to_owned(),
            other => other.strip_prefix("custom:").unwrap_or(other).to_owned(),
        };
        Some(
            div()
                .id(SharedString::from(format!("icarus-origin-{workspace_id}")))
                .tooltip(Tooltip::text(format!("Worktree from {}", origin.label)))
                .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
                .into_any_element(),
        )
    }

    /// The inbox row of a project shown as its own group.
    pub fn render_project_inbox(
        &self,
        project: &str,
        cx: &mut Context<WorkspacesPanel>,
    ) -> Vec<AnyElement> {
        self.inbox
            .iter()
            .filter(|entry| entry.project == project)
            .map(|entry| render_entry(entry, false, cx))
            .collect()
    }

    /// Inbox rows, named by project, for the projects without a group of their own on screen.
    pub fn render_other_inboxes(
        &self,
        grouped_projects: &[String],
        cx: &mut Context<WorkspacesPanel>,
    ) -> Vec<AnyElement> {
        self.inbox
            .iter()
            .filter(|entry| !grouped_projects.contains(&entry.project))
            .map(|entry| render_entry(entry, true, cx))
            .collect()
    }
}

fn render_entry(
    entry: &InboxEntry,
    name_project: bool,
    cx: &mut Context<WorkspacesPanel>,
) -> AnyElement {
    let project = entry.project.clone();
    let title = if name_project {
        format!("New worktrees in {}", entry.project_name)
    } else {
        "New worktrees".to_owned()
    };
    let all: Vec<String> = entry.workspaces.iter().map(|w| w.id.clone()).collect();
    let header = h_flex()
        .gap_1p5()
        .child(
            Icon::new(IconName::Plus)
                .size(IconSize::XSmall)
                .color(Color::Accent),
        )
        .child(Label::new(title).size(LabelSize::Small))
        .child(
            Label::new(entry.workspaces.len().to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(div().flex_1())
        .child(
            Button::new(
                SharedString::from(format!("icarus-inbox-dismiss-{project}")),
                "Keep hidden",
            )
            .label_size(LabelSize::XSmall)
            .tooltip(Tooltip::text("Hide these and stop asking about them"))
            .on_click(cx.listener({
                let project = project.clone();
                move |panel, _, _, cx| {
                    act(
                        panel,
                        "workspace.inbox_dismiss",
                        json!({ "project": project }),
                        all.clone(),
                        cx,
                    )
                }
            })),
        );
    let rows: Vec<AnyElement> = entry
        .workspaces
        .iter()
        .map(|worktree| render_worktree(&project, worktree, cx))
        .collect();
    v_flex()
        .mx_1()
        .px_2()
        .py_1()
        .gap_0p5()
        .rounded_md()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border)
        .child(header)
        .children(rows)
        .into_any_element()
}

fn render_worktree(
    project: &str,
    worktree: &InboxWorktree,
    cx: &mut Context<WorkspacesPanel>,
) -> AnyElement {
    let id = worktree.id.clone();
    let button = |action: &str, label: &'static str| {
        Button::new(
            SharedString::from(format!("icarus-inbox-{action}-{id}")),
            label,
        )
        .label_size(LabelSize::XSmall)
    };
    let visibility = |visibility: &'static str| {
        let id = id.clone();
        cx.listener(move |panel, _, _, cx| {
            act(
                panel,
                "workspace.visibility",
                json!({ "id": id, "visibility": visibility }),
                vec![id.clone()],
                cx,
            )
        })
    };
    let show = visibility("show");
    let hide = visibility("hide");
    let source = {
        let (project, origin, id) = (project.to_owned(), worktree.origin.clone(), id.clone());
        cx.listener(move |panel, _, _, cx| {
            act(
                panel,
                "workspace.show_source",
                json!({ "project": project, "source": origin }),
                vec![id.clone()],
                cx,
            )
        })
    };
    h_flex()
        .id(SharedString::from(format!("icarus-inbox-row-{id}")))
        .gap_1()
        .tooltip(Tooltip::text(worktree.path.clone()))
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(
                    Label::new(worktree.name.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(
                    Label::new(worktree.source_label.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .child(button("show", "Show").on_click(show))
        .child(button("hide", "Hide").on_click(hide))
        .child(
            button("source", "Show all")
                .tooltip(Tooltip::text(
                    "Show every worktree from this source in the project",
                ))
                .on_click(source),
        )
        .into_any_element()
}

fn act(
    panel: &mut WorkspacesPanel,
    method: &'static str,
    params: Value,
    answered: Vec<String>,
    cx: &mut Context<WorkspacesPanel>,
) {
    let Some(client): Option<Arc<Client>> = panel.client.clone() else {
        return;
    };
    panel.discovered.forget(&answered);
    cx.notify();
    let request = Tokio::spawn_result(cx, async move { client.request(method, params).await });
    cx.spawn(async move |this, cx| {
        request.await?;
        this.update(cx, |panel, cx| panel.refresh(cx))
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_inbox_come_from_the_server_lists() {
        let workspaces = json!([
            { "id": "a", "origin": "native", "source_label": "Icarus" },
            { "id": "b", "origin": "custom:conductor", "source_label": "conductor" },
            { "id": "c" }
        ]);
        let inbox = json!([{
            "project": "p",
            "project_name": "widgets",
            "workspaces": [
                { "id": "x", "name": "fix", "path": "/o/fix", "origin": "external", "source_label": "/o" },
                { "id": "y", "name": "poke", "path": "/o/poke", "origin": "external", "source_label": "/o" }
            ]
        }]);
        let mut discovered = Discovered::new(&workspaces, inbox);
        assert!(
            discovered.origin_badge("a").is_none(),
            "Icarus's own need no badge"
        );
        assert!(discovered.origin_badge("b").is_some());
        assert!(
            discovered.origin_badge("c").is_none(),
            "an older server sends no origin"
        );
        assert_eq!(discovered.inbox[0].workspaces.len(), 2);
        discovered.forget(&["x".to_owned()]);
        assert_eq!(discovered.inbox[0].workspaces.len(), 1);
        discovered.forget(&["y".to_owned()]);
        assert!(discovered.inbox.is_empty(), "an answered inbox goes away");
    }
}
