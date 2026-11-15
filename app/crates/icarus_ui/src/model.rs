use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub title: Option<String>,
    pub state: String,
    pub provider: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorkspaceSummary {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub issue: Option<serde_json::Value>,
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
    #[serde(default)]
    pub pr_author: Option<PrAuthor>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PrAuthor {
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl WorkspaceSummary {
    /// The card title: the linked issue's id, then the workspace title, then its name.
    pub fn display_title(&self) -> String {
        self.issue
            .as_ref()
            .and_then(|issue| issue["id"].as_str())
            .map(str::to_string)
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.name.clone())
    }

    /// The linked issue's title, shown under a card titled by the issue id.
    pub fn subtitle(&self) -> Option<String> {
        self.issue
            .as_ref()
            .and_then(|issue| issue["title"].as_str())
            .map(str::to_string)
    }

    pub fn working_agents(&self) -> usize {
        self.agents.iter().filter(|a| a.state == "working").count()
    }
}

/// A user-defined workspace status column from `workspace.statuses`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StatusColumn {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusGroup {
    pub status: Option<String>,
    pub workspaces: Vec<WorkspaceSummary>,
}

/// Groups workspaces by status in `order`; unknown statuses follow in name order, then "No status".
pub fn group_by_status(workspaces: &[WorkspaceSummary], order: &[String]) -> Vec<StatusGroup> {
    let mut by_status: BTreeMap<Option<String>, Vec<WorkspaceSummary>> = BTreeMap::new();
    for workspace in workspaces {
        by_status
            .entry(workspace.status.clone())
            .or_default()
            .push(workspace.clone());
    }
    let mut groups = Vec::new();
    for status in order {
        if let Some(items) = by_status.remove(&Some(status.clone())) {
            groups.push(StatusGroup {
                status: Some(status.clone()),
                workspaces: items,
            });
        }
    }
    let unstatused = by_status.remove(&None);
    for (status, items) in by_status {
        groups.push(StatusGroup {
            status,
            workspaces: items,
        });
    }
    if let Some(items) = unstatused {
        groups.push(StatusGroup {
            status: None,
            workspaces: items,
        });
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str, status: Option<&str>) -> WorkspaceSummary {
        WorkspaceSummary {
            id: format!("ws-{name}"),
            project_id: "p".into(),
            name: name.into(),
            path: PathBuf::from(format!("/w/{name}")),
            branch: None,
            status: status.map(str::to_string),
            title: None,
            issue: None,
            agents: Vec::new(),
            pr_author: None,
        }
    }

    #[test]
    fn groups_follow_the_configured_order_then_unknown_then_none() {
        let workspaces = [
            workspace("a", Some("review")),
            workspace("b", None),
            workspace("c", Some("draft")),
            workspace("d", Some("custom")),
            workspace("e", Some("review")),
        ];
        let groups = group_by_status(&workspaces, &["draft".into(), "review".into()]);
        let statuses: Vec<Option<&str>> = groups.iter().map(|g| g.status.as_deref()).collect();
        assert_eq!(
            statuses,
            [Some("draft"), Some("review"), Some("custom"), None]
        );
        assert_eq!(groups[1].workspaces.len(), 2);
    }

    #[test]
    fn statuses_without_workspaces_are_not_shown() {
        let workspaces = [workspace("a", Some("review"))];
        let groups = group_by_status(
            &workspaces,
            &["draft".into(), "review".into(), "done".into()],
        );
        let statuses: Vec<Option<&str>> = groups.iter().map(|g| g.status.as_deref()).collect();
        assert_eq!(statuses, [Some("review")]);
    }

    #[test]
    fn issue_id_is_the_card_title() {
        let mut w = workspace("abc-12-fix-login", Some("todo"));
        w.issue =
            Some(serde_json::json!({ "provider": "linear", "id": "ABC-12", "title": "Fix login" }));
        assert_eq!(w.display_title(), "ABC-12");
        assert_eq!(w.subtitle().as_deref(), Some("Fix login"));
    }
}
