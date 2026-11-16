use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::cards::SortBy;

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
    /// The pull request as the server last cached it; listing never waits on GitHub.
    #[serde(default)]
    pub pr: Option<PrSummary>,
    /// The project's setup script run for this workspace, if it has one.
    #[serde(default)]
    pub setup: Option<SetupSummary>,
    #[serde(default)]
    pub sleeping: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SetupSummary {
    /// running, succeeded, failed or skipped.
    pub state: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// What a card says about setup: only while it runs or after it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupBadge {
    Running,
    Failed(String),
}

impl SetupSummary {
    pub fn badge(&self) -> Option<SetupBadge> {
        match self.state.as_str() {
            "running" => Some(SetupBadge::Running),
            "failed" => Some(SetupBadge::Failed(match (&self.error, self.exit_code) {
                (Some(error), _) => error.clone(),
                (None, Some(code)) => format!("The setup script exited with code {code}."),
                (None, None) => "The setup script failed.".to_owned(),
            })),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PrAuthor {
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PrSummary {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    /// open, draft, closed or merged.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    /// success, failure, pending or neutral.
    #[serde(default)]
    pub checks: Option<String>,
    #[serde(default)]
    pub in_merge_queue: bool,
    #[serde(default)]
    pub stack: Option<StackPlace>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct StackPlace {
    pub number: u64,
    /// 1 is the PR on the stack's base; the highest is furthest from main.
    pub position: u32,
    #[serde(default)]
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IssueLabel {
    pub name: String,
    #[serde(default)]
    pub color: String,
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

    /// The linked issue's labels from the server's Linear cache.
    pub fn issue_labels(&self) -> Vec<IssueLabel> {
        self.issue
            .as_ref()
            .and_then(|issue| issue.get("labels").cloned())
            .and_then(|labels| serde_json::from_value(labels).ok())
            .unwrap_or_default()
    }

    /// The linked issue's page, once the server has fetched it.
    pub fn issue_url(&self) -> Option<String> {
        self.issue
            .as_ref()
            .and_then(|issue| issue["url"].as_str())
            .filter(|url| !url.is_empty())
            .map(str::to_string)
    }

    pub fn stack(&self) -> Option<StackPlace> {
        self.pr.as_ref().and_then(|pr| pr.stack)
    }
}

/// Which lane a workspace takes when the list is grouped by stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StackLane {
    /// Someone else's pull request (the status the rules give `reviewing`), stacked or not.
    Reviewing,
    Stack {
        project_id: String,
        number: u64,
    },
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackGroup {
    pub lane: StackLane,
    pub workspaces: Vec<WorkspaceSummary>,
}

/// Orca's stack grouping: reviewing first, then newest stack first (each listed from the PR
/// furthest from main down, or up when `ascending`), then everything outside a stack in status order.
pub fn group_by_stack(
    workspaces: &[WorkspaceSummary],
    status_order: &[String],
    reviewing_status: Option<&str>,
    ascending: bool,
) -> Vec<StackGroup> {
    let lane_of = |workspace: &WorkspaceSummary| {
        if reviewing_status.is_some() && workspace.status.as_deref() == reviewing_status {
            return StackLane::Reviewing;
        }
        match workspace.stack() {
            Some(stack) => StackLane::Stack {
                project_id: workspace.project_id.clone(),
                number: stack.number,
            },
            None => StackLane::None,
        }
    };
    let mut reviewing = Vec::new();
    let mut stacks: BTreeMap<(u64, String), Vec<WorkspaceSummary>> = BTreeMap::new();
    let mut loose = Vec::new();
    for workspace in workspaces {
        match lane_of(workspace) {
            StackLane::Reviewing => reviewing.push(workspace.clone()),
            StackLane::Stack { project_id, number } => stacks
                .entry((number, project_id))
                .or_default()
                .push(workspace.clone()),
            StackLane::None => loose.push(workspace.clone()),
        }
    }
    let position = |workspace: &WorkspaceSummary| workspace.stack().map_or(0, |s| s.position);
    let status_rank = |workspace: &WorkspaceSummary| {
        workspace
            .status
            .as_ref()
            .and_then(|status| status_order.iter().position(|id| id == status))
            .unwrap_or(status_order.len())
    };
    loose.sort_by_key(|workspace| status_rank(workspace));
    let mut groups = Vec::new();
    if !reviewing.is_empty() {
        groups.push(StackGroup {
            lane: StackLane::Reviewing,
            workspaces: reviewing,
        });
    }
    for ((number, project_id), mut items) in stacks.into_iter().rev() {
        items.sort_by_key(|workspace| position(workspace));
        if !ascending {
            items.reverse();
        }
        groups.push(StackGroup {
            lane: StackLane::Stack { project_id, number },
            workspaces: items,
        });
    }
    if !loose.is_empty() {
        groups.push(StackGroup {
            lane: StackLane::None,
            workspaces: loose,
        });
    }
    groups
}

/// Orca's PR lanes (`PR_GROUP_ORDER`), in the order the list shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrGroup {
    Done,
    InReview,
    InProgress,
    Closed,
}

impl PrGroup {
    /// Orca's `getPRGroupKeyForState`: no PR and drafts are in progress, an open PR is in review.
    pub fn of(workspace: &WorkspaceSummary) -> Self {
        match workspace.pr.as_ref().map(|pr| pr.state.as_str()) {
            None | Some("draft") => Self::InProgress,
            Some("merged") => Self::Done,
            Some("closed") => Self::Closed,
            Some(_) => Self::InReview,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::InReview => "In review",
            Self::InProgress => "In progress",
            Self::Closed => "Closed",
        }
    }
}

/// Groups by PR lane, keeping the incoming order inside each lane and leaving empty lanes out.
pub fn group_by_pr(workspaces: &[WorkspaceSummary]) -> Vec<(PrGroup, Vec<WorkspaceSummary>)> {
    let mut lanes: BTreeMap<PrGroup, Vec<WorkspaceSummary>> = BTreeMap::new();
    for workspace in workspaces {
        lanes
            .entry(PrGroup::of(workspace))
            .or_default()
            .push(workspace.clone());
    }
    lanes.into_iter().collect()
}

/// Whether the sidebar's text filter keeps a card: every word must appear in the name, title,
/// branch, PR number or title, or the linked issue's id or title (case-insensitive).
pub fn matches_filter(workspace: &WorkspaceSummary, query: &str) -> bool {
    let mut haystack = vec![
        workspace.name.clone(),
        workspace.display_title(),
        workspace.title.clone().unwrap_or_default(),
        workspace.subtitle().unwrap_or_default(),
        workspace.branch.clone().unwrap_or_default(),
    ];
    if let Some(pr) = &workspace.pr {
        haystack.push(format!("#{}", pr.number));
        haystack.push(pr.title.clone());
    }
    let haystack = haystack.join("\n").to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

/// Orca's sort comparators, each falling back to the card title and then the id. Recent has no
/// activity clock on the server yet, so it ranks by creation time.
pub fn sort_workspaces(
    workspaces: &mut [WorkspaceSummary],
    sort_by: SortBy,
    project_name: impl Fn(&str) -> String,
) {
    let label = |workspace: &WorkspaceSummary| workspace.display_title().to_lowercase();
    let by_label = |a: &WorkspaceSummary, b: &WorkspaceSummary| {
        label(a).cmp(&label(b)).then_with(|| a.id.cmp(&b.id))
    };
    let newest_first =
        |a: &WorkspaceSummary, b: &WorkspaceSummary| b.created_at_ms.cmp(&a.created_at_ms);
    match sort_by {
        SortBy::Manual => {}
        SortBy::Name => workspaces.sort_by(by_label),
        SortBy::Recent => {
            workspaces.sort_by(|a, b| newest_first(a, b).then_with(|| by_label(a, b)))
        }
        SortBy::Smart => workspaces.sort_by(|a, b| {
            (b.working_agents() > 0)
                .cmp(&(a.working_agents() > 0))
                .then_with(|| newest_first(a, b))
                .then_with(|| by_label(a, b))
        }),
        SortBy::Repo => workspaces.sort_by(|a, b| {
            project_name(&a.project_id)
                .to_lowercase()
                .cmp(&project_name(&b.project_id).to_lowercase())
                .then_with(|| by_label(a, b))
        }),
    }
}

/// Pinned cards move to the top of a group; the order among pinned and unpinned cards stays.
pub fn pinned_first(workspaces: &mut [WorkspaceSummary]) {
    workspaces.sort_by_key(|workspace| !workspace.pinned);
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
            pr: None,
            setup: None,
            sleeping: false,
            pinned: false,
            created_at_ms: 0,
        }
    }

    fn with_pr(name: &str, state: &str) -> WorkspaceSummary {
        let mut w = workspace(name, None);
        w.pr = Some(PrSummary {
            number: 42,
            title: "Speed up the login page".into(),
            state: state.into(),
            ..PrSummary::default()
        });
        w
    }

    fn order(workspaces: &[WorkspaceSummary]) -> Vec<&str> {
        workspaces.iter().map(|w| w.name.as_str()).collect()
    }

    #[test]
    fn pr_lanes_follow_orcas_order_and_states() {
        let workspaces = [
            workspace("no-pr", None),
            with_pr("draft", "draft"),
            with_pr("open", "open"),
            with_pr("merged", "merged"),
            with_pr("closed", "closed"),
            with_pr("open-2", "open"),
        ];
        let grouped = group_by_pr(&workspaces);
        let lanes: Vec<(&str, Vec<&str>)> = grouped
            .iter()
            .map(|(lane, items)| (lane.label(), order(items)))
            .collect();
        assert_eq!(
            lanes,
            [
                ("Done", vec!["merged"]),
                ("In review", vec!["open", "open-2"]),
                ("In progress", vec!["no-pr", "draft"]),
                ("Closed", vec!["closed"]),
            ]
        );
    }

    #[test]
    fn the_filter_matches_name_branch_pr_and_issue_words() {
        let mut w = with_pr("fix-login", "open");
        w.branch = Some("feature/auth-cache".into());
        w.issue = Some(serde_json::json!({ "id": "ABC-12", "title": "Flaky sign-in" }));
        for query in [
            "",
            "fix",
            "AUTH-cache",
            "#42",
            "speed login",
            "abc-12",
            "flaky",
        ] {
            assert!(matches_filter(&w, query), "{query}");
        }
        for query in ["#43", "speed checkout", "xyz"] {
            assert!(!matches_filter(&w, query), "{query}");
        }
    }

    #[test]
    fn sort_orders_match_orcas_comparators() {
        let mut beta = workspace("beta", None);
        beta.created_at_ms = 3;
        beta.project_id = "a".into();
        let mut alpha = workspace("Alpha", None);
        alpha.created_at_ms = 1;
        alpha.project_id = "b".into();
        let mut gamma = workspace("gamma", None);
        gamma.created_at_ms = 2;
        gamma.project_id = "a".into();
        gamma.agents.push(AgentSummary {
            id: "a1".into(),
            title: None,
            state: "working".into(),
            provider: "claude".into(),
        });
        let listed = vec![beta, alpha, gamma];
        let projects = |id: &str| if id == "a" { "zeta" } else { "api" }.to_string();
        let sorted = |sort_by| {
            let mut items = listed.clone();
            sort_workspaces(&mut items, sort_by, projects);
            order(&items).join(",")
        };
        assert_eq!(sorted(SortBy::Name), "Alpha,beta,gamma");
        assert_eq!(sorted(SortBy::Recent), "beta,gamma,Alpha");
        assert_eq!(
            sorted(SortBy::Smart),
            "gamma,beta,Alpha",
            "working agents first"
        );
        assert_eq!(sorted(SortBy::Repo), "Alpha,beta,gamma");
        assert_eq!(
            sorted(SortBy::Manual),
            "beta,Alpha,gamma",
            "the server's order"
        );

        let mut items = listed.clone();
        items[2].pinned = true;
        items[1].pinned = true;
        pinned_first(&mut items);
        assert_eq!(order(&items), ["Alpha", "gamma", "beta"]);
    }

    #[test]
    fn cards_mention_setup_only_while_it_runs_or_after_it_failed() {
        let setup = |state: &str, exit_code: Option<i64>, error: Option<&str>| SetupSummary {
            state: state.into(),
            exit_code,
            error: error.map(str::to_owned),
        };
        assert_eq!(
            setup("running", None, None).badge(),
            Some(SetupBadge::Running)
        );
        assert_eq!(setup("succeeded", Some(0), None).badge(), None);
        assert_eq!(setup("skipped", None, None).badge(), None);
        assert_eq!(
            setup("failed", Some(3), None).badge(),
            Some(SetupBadge::Failed(
                "The setup script exited with code 3.".into()
            ))
        );
        assert_eq!(
            setup("failed", None, Some("Setup timed out after 500ms.")).badge(),
            Some(SetupBadge::Failed("Setup timed out after 500ms.".into()))
        );
        let parsed: WorkspaceSummary = serde_json::from_value(serde_json::json!({
            "id": "ws-1", "project_id": "p", "name": "n", "path": "/w/n",
            "branch": null, "status": null, "title": null, "issue": null,
            "setup": { "state": "running", "terminal": "term-1", "started_at_ms": 1 },
        }))
        .expect("parses the server's listing");
        assert_eq!(
            parsed.setup.and_then(|s| s.badge()),
            Some(SetupBadge::Running)
        );
    }

    fn stacked(name: &str, status: Option<&str>, stack: u64, position: u32) -> WorkspaceSummary {
        let mut w = workspace(name, status);
        w.pr = Some(PrSummary {
            number: u64::from(position) + stack * 100,
            stack: Some(StackPlace {
                number: stack,
                position,
                size: 3,
            }),
            ..PrSummary::default()
        });
        w
    }

    fn names(groups: &[StackGroup]) -> Vec<(StackLane, Vec<&str>)> {
        groups
            .iter()
            .map(|g| {
                (
                    g.lane.clone(),
                    g.workspaces.iter().map(|w| w.name.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn stacks_list_newest_first_furthest_from_main_down_then_the_rest() {
        let workspaces = [
            workspace("loose", Some("todo")),
            stacked("bottom", None, 7, 1),
            stacked("top", None, 7, 3),
            stacked("other", None, 9, 1),
            stacked("middle", None, 7, 2),
        ];
        let stack = |number| StackLane::Stack {
            project_id: "p".into(),
            number,
        };
        assert_eq!(
            names(&group_by_stack(&workspaces, &[], None, false)),
            [
                (stack(9), vec!["other"]),
                (stack(7), vec!["top", "middle", "bottom"]),
                (StackLane::None, vec!["loose"]),
            ]
        );
        assert_eq!(
            names(&group_by_stack(&workspaces, &[], None, true))[1],
            (stack(7), vec!["bottom", "middle", "top"]),
            "ascending lists a stack from the PR on main up"
        );
    }

    #[test]
    fn reviewing_comes_first_even_when_stacked_and_the_rest_follow_status_order() {
        let workspaces = [
            workspace("done", Some("completed")),
            stacked("top", Some("in-review"), 7, 3),
            workspace("waiting", Some("todo")),
            stacked("middle", None, 7, 2),
        ];
        let groups = group_by_stack(
            &workspaces,
            &["todo".into(), "completed".into()],
            Some("in-review"),
            false,
        );
        assert_eq!(
            names(&groups),
            [
                (StackLane::Reviewing, vec!["top"]),
                (
                    StackLane::Stack {
                        project_id: "p".into(),
                        number: 7
                    },
                    vec!["middle"]
                ),
                (StackLane::None, vec!["waiting", "done"]),
            ]
        );
    }

    #[test]
    fn without_pr_data_everything_is_outside_a_stack() {
        let groups = group_by_stack(&[workspace("a", None)], &[], None, false);
        assert_eq!(names(&groups), [(StackLane::None, vec!["a"])]);
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
