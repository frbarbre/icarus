//! Named tab layouts (plan D32): a split tree whose leaves are panes of tabs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const REVIEW_LAYOUT: &str = "review";
/// A diff path the server replaces with the workspace's first changed file.
pub const FIRST_CHANGED_FILE: &str = "{first_changed_file}";
/// What a new workspace opens with when nothing else is asked for: one agent tab.
pub const DEFAULT_LAYOUT: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    /// Children side by side, left to right.
    Row,
    /// Children stacked, top to bottom.
    Column,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LayoutNode {
    Split {
        split: SplitAxis,
        children: Vec<LayoutNode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sizes: Option<Vec<f32>>,
    },
    Pane {
        tabs: Vec<Tab>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tab {
    Agent {
        /// Starts a new agent with this prompt; without one the tab shows the workspace's newest agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Filled in by the server once the tab is bound to an agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    Browser {
        url: String,
    },
    /// Without a path, the Source Control panel; with one, that file's diff tab
    /// (`{first_changed_file}` is the first file in the workspace's Changes list).
    Diff {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Explorer,
    File {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
    },
    Terminal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
}

impl LayoutNode {
    pub fn tabs(&self) -> Vec<&Tab> {
        let mut out = Vec::new();
        self.visit(&mut |tab| out.push(tab));
        out
    }

    fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Tab)) {
        match self {
            LayoutNode::Split { children, .. } => children.iter().for_each(|c| c.visit(f)),
            LayoutNode::Pane { tabs, .. } => tabs.iter().for_each(f),
        }
    }

    pub fn tabs_mut(&mut self) -> Vec<&mut Tab> {
        match self {
            LayoutNode::Split { children, .. } => {
                children.iter_mut().flat_map(LayoutNode::tabs_mut).collect()
            }
            LayoutNode::Pane { tabs, .. } => tabs.iter_mut().collect(),
        }
    }

    /// Structural problems a settings author should hear about before the app tries to build it.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            LayoutNode::Split {
                children, sizes, ..
            } => {
                if children.is_empty() {
                    return Err("a split needs at least one child".into());
                }
                if let Some(sizes) = sizes
                    && sizes.len() != children.len()
                {
                    return Err(format!(
                        "a split has {} children but {} sizes",
                        children.len(),
                        sizes.len()
                    ));
                }
                children.iter().try_for_each(LayoutNode::validate)
            }
            LayoutNode::Pane { tabs, active } => {
                if tabs.is_empty() {
                    return Err("a pane needs at least one tab".into());
                }
                if let Some(active) = active
                    && *active >= tabs.len()
                {
                    return Err(format!("active tab {active} is out of range"));
                }
                Ok(())
            }
        }
    }
}

/// Layouts every install has; settings can override them by name.
pub fn builtin_layouts() -> BTreeMap<String, LayoutNode> {
    let agent_pane = || LayoutNode::Pane {
        tabs: vec![Tab::Agent {
            prompt: None,
            provider: None,
            agent_id: None,
        }],
        active: None,
    };
    BTreeMap::from([
        (DEFAULT_LAYOUT.to_string(), agent_pane()),
        (
            REVIEW_LAYOUT.to_string(),
            LayoutNode::Split {
                split: SplitAxis::Row,
                children: vec![
                    agent_pane(),
                    // github-review.md: the first changed file plus the PR, which stays in front.
                    LayoutNode::Pane {
                        tabs: vec![
                            Tab::Diff {
                                scope: None,
                                path: Some(FIRST_CHANGED_FILE.into()),
                            },
                            Tab::Browser {
                                url: "{pr_url}".into(),
                            },
                        ],
                        active: Some(1),
                    },
                ],
                sizes: None,
            },
        ),
    ])
}

/// Replaces `{name}` placeholders; unknown or missing ones resolve to an empty string.
pub fn fill_placeholders(template: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if is_placeholder_name(&after[..end]) => {
                out.push_str(&lookup(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_placeholder_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Placeholder names used anywhere in the layout's browser URLs.
pub fn placeholders(node: &LayoutNode) -> Vec<String> {
    let mut names = Vec::new();
    for tab in node.tabs() {
        if let Tab::Browser { url } = tab {
            fill_placeholders(url, |name| {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
                None
            });
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_layout_is_an_agent_left_and_the_changes_and_pr_right() {
        let review = &builtin_layouts()[REVIEW_LAYOUT];
        assert_eq!(
            serde_json::to_value(review).expect("serialize"),
            json!({ "split": "row", "children": [
                { "tabs": [{ "type": "agent" }] },
                { "tabs": [
                    { "type": "diff", "path": "{first_changed_file}" },
                    { "type": "browser", "url": "{pr_url}" },
                ], "active": 1 },
            ]})
        );
    }

    #[test]
    fn a_hand_written_layout_round_trips() {
        let text = json!({ "split": "row", "sizes": [0.25, 0.75], "children": [
            { "tabs": [{ "type": "file", "path": "src/main.rs", "line": 3 }] },
            { "split": "column", "children": [
                { "tabs": [{ "type": "agent", "prompt": "hi" }, { "type": "diff" }], "active": 1 },
                { "tabs": [{ "type": "explorer" }, { "type": "terminal", "command": "pnpm dev" }] },
            ]},
        ]});
        let node: LayoutNode = serde_json::from_value(text.clone()).expect("parse");
        node.validate().expect("valid");
        assert_eq!(node.tabs().len(), 5);
        assert_eq!(serde_json::to_value(&node).expect("serialize"), text);
    }

    #[test]
    fn broken_layouts_are_reported() {
        let empty: LayoutNode = serde_json::from_value(json!({ "tabs": [] })).expect("parse");
        assert!(empty.validate().is_err());
        let sizes: LayoutNode =
            serde_json::from_value(json!({ "split": "row", "sizes": [1.0], "children": [
                { "tabs": [{ "type": "explorer" }] }, { "tabs": [{ "type": "diff" }] }
            ]}))
            .expect("parse");
        assert!(sizes.validate().is_err());
        let active: LayoutNode =
            serde_json::from_value(json!({ "tabs": [{ "type": "explorer" }], "active": 1 }))
                .expect("parse");
        assert!(active.validate().is_err());
        assert!(
            serde_json::from_value::<LayoutNode>(json!({ "tabs": [{ "type": "nope" }] })).is_err()
        );
    }

    #[test]
    fn placeholders_fill_and_leave_other_braces_alone() {
        let filled = fill_placeholders("{pr_url}/files?x={y z}&{missing}", |name| {
            (name == "pr_url").then(|| "https://github.com/o/r/pull/1".to_string())
        });
        assert_eq!(filled, "https://github.com/o/r/pull/1/files?x={y z}&");
        let node = &builtin_layouts()[REVIEW_LAYOUT];
        assert_eq!(placeholders(node), ["pr_url"]);
    }
}
