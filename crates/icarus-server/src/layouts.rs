//! Named tab layouts (plan D32): resolved on the server, stored on the workspace, applied by the app.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use icarus_protocol::RpcError;
use icarus_protocol::layout::{
    FIRST_CHANGED_FILE, LayoutNode, Tab, builtin_layouts, fill_placeholders, placeholders,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentOptions;
use crate::github::local_branch;
use crate::next_action;
use crate::server::{Server, params, rpc};
use crate::state::Workspace;

/// Orca's preset and older rules call the review layout by this name.
const LEGACY_REVIEW_NAME: &str = "review_split";

fn pending_key(workspace_id: &str) -> String {
    format!("layout.pending:{workspace_id}")
}

/// Built-in layouts overlaid with the user's and project's `layouts` settings.
pub(crate) fn layout_definitions(settings: &Value) -> Result<BTreeMap<String, LayoutNode>, String> {
    let mut layouts = builtin_layouts();
    if let Some(configured) = settings.get("layouts").and_then(Value::as_object) {
        for (name, value) in configured {
            let node: LayoutNode = serde_json::from_value(value.clone())
                .map_err(|e| format!("layouts.{name}: {e}"))?;
            node.validate()
                .map_err(|e| format!("layouts.{name}: {e}"))?;
            layouts.insert(name.clone(), node);
        }
    }
    Ok(layouts)
}

pub(crate) fn find_layout(
    layouts: &BTreeMap<String, LayoutNode>,
    name: &str,
) -> Option<LayoutNode> {
    let name = if name == LEGACY_REVIEW_NAME {
        icarus_protocol::layout::REVIEW_LAYOUT
    } else {
        name
    };
    layouts.get(name).cloned()
}

impl Server {
    fn project_root_of(&self, workspace: &Workspace) -> Option<std::path::PathBuf> {
        self.store
            .project(&workspace.project_id)
            .ok()
            .flatten()
            .map(|project| project.root)
    }

    fn layouts_for(&self, root: Option<&Path>) -> Result<BTreeMap<String, LayoutNode>, RpcError> {
        layout_definitions(&self.resolved_settings(root)).map_err(|e| rpc("settings_invalid", e))
    }

    /// Points `{first_changed_file}` diffs at the Changes list's first row; with no changes they fall back
    /// to showing the Source Control panel.
    async fn resolve_first_changed_file(
        self: &Arc<Self>,
        workspace: &Workspace,
        layout: &mut LayoutNode,
    ) {
        let wanted = layout.tabs().into_iter().any(
            |tab| matches!(tab, Tab::Diff { path: Some(path), .. } if path == FIRST_CHANGED_FILE),
        );
        if !wanted {
            return;
        }
        let first = self
            .changes_dispatch("workspace.changes", json!({ "id": workspace.id }))
            .await
            .ok()
            .and_then(|listing| listing["rows"][0]["path"].as_str().map(str::to_owned));
        for tab in layout.tabs_mut() {
            if let Tab::Diff { path, scope } = tab
                && path.as_deref() == Some(FIRST_CHANGED_FILE)
            {
                *path = first.clone();
                // The app opens the tab against this base; only the server knows it.
                if path.is_some() && scope.is_none() {
                    *scope = workspace
                        .base_ref
                        .as_ref()
                        .map(|base| format!("base:{base}"));
                }
            }
        }
    }

    /// Resolves `name` for `workspace`, binds its agent tabs, stores it as pending and pushes it to the app.
    pub(crate) async fn apply_layout(
        self: &Arc<Self>,
        workspace: &Workspace,
        name: &str,
        pr_url: Option<String>,
    ) -> Result<Value, RpcError> {
        let root = self.project_root_of(workspace);
        let layouts = self.layouts_for(root.as_deref())?;
        let mut layout = find_layout(&layouts, name).ok_or_else(|| {
            rpc(
                "not_found",
                format!(
                    "no layout named {name}; known: {}",
                    layouts.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            )
        })?;
        let values = self
            .placeholder_values(workspace, &placeholders(&layout), pr_url)
            .await;
        for tab in layout.tabs_mut() {
            if let Tab::Browser { url } = tab {
                *url = fill_placeholders(url, |key| values.get(key).cloned().flatten());
            }
        }
        self.resolve_first_changed_file(workspace, &mut layout)
            .await;
        self.bind_agent_tabs(workspace, root.as_deref(), &mut layout)
            .await?;
        if let Some(terminal) = self
            .setup_status(&workspace.id)
            .filter(|status| status.state == crate::hooks::SetupState::Running)
            .and_then(|status| status.terminal)
        {
            crate::hooks::show_setup_terminal(&mut layout, &terminal);
        }
        let payload = json!({
            "workspace_id": workspace.id,
            "path": workspace.path,
            "name": name,
            "layout": layout,
        });
        self.store
            .kv_set(&pending_key(&workspace.id), &payload)
            .map_err(|e| rpc("internal", e))?;
        let delivered = self.send_to_apps("open_layout", payload.clone());
        let mut result = payload;
        result["delivered"] = json!(delivered);
        Ok(result)
    }

    async fn placeholder_values(
        self: &Arc<Self>,
        workspace: &Workspace,
        names: &[String],
        pr_url: Option<String>,
    ) -> BTreeMap<String, Option<String>> {
        let mut values = BTreeMap::new();
        let wants = |key: &str| names.iter().any(|n| n == key);
        values.insert(
            "workspace_path".to_owned(),
            Some(workspace.path.display().to_string()),
        );
        if wants("pr_url") || wants("preview_url") {
            let branch = local_branch(workspace).await;
            let view = self.pr_view(workspace, branch.as_deref(), false).await;
            let url = pr_url.or_else(|| view.pr.as_ref().map(|pr| pr.url.clone()));
            values.insert("pr_url".to_owned(), url);
            if wants("preview_url") {
                let preview = self
                    .view_previews(workspace, &view, false)
                    .await
                    .ok()
                    .and_then(|found| found.primary)
                    .and_then(|preview| preview.url);
                values.insert("preview_url".to_owned(), preview);
            }
        }
        if wants("frontend_url") {
            let url = self
                .ports_dispatch(
                    "ports.primary",
                    json!({ "workspace": workspace.id, "kind": "frontend" }),
                )
                .await
                .ok()
                .and_then(|v| v["url"].as_str().map(str::to_owned));
            values.insert("frontend_url".to_owned(), url);
        }
        values
    }

    /// Agent tabs with a prompt start an agent; the rest show the newest agents not already shown.
    async fn bind_agent_tabs(
        self: &Arc<Self>,
        workspace: &Workspace,
        root: Option<&Path>,
        layout: &mut LayoutNode,
    ) -> Result<(), RpcError> {
        let mut existing: Vec<_> = self
            .agents
            .list()
            .into_iter()
            .filter(|a| a.workspace.as_deref() == Some(workspace.id.as_str()))
            .collect();
        existing.sort_by_key(|a| std::cmp::Reverse(a.created_at_ms));
        let mut used: HashSet<String> = HashSet::new();
        for tab in layout.tabs_mut() {
            let Tab::Agent {
                prompt,
                provider,
                agent_id,
            } = tab
            else {
                continue;
            };
            if let Some(id) = agent_id.as_ref() {
                used.insert(id.clone());
                continue;
            }
            let reuse = prompt
                .is_none()
                .then(|| existing.iter().find(|a| !used.contains(&a.id)))
                .flatten()
                .map(|a| a.id.clone());
            let id = match reuse {
                Some(id) => id,
                None => {
                    self.spawn_workspace_agent(workspace, root, provider.clone(), prompt.as_deref())
                        .await?
                }
            };
            used.insert(id.clone());
            *agent_id = Some(id);
        }
        Ok(())
    }

    pub(crate) async fn spawn_workspace_agent(
        self: &Arc<Self>,
        workspace: &Workspace,
        root: Option<&Path>,
        provider: Option<String>,
        prompt: Option<&str>,
    ) -> Result<String, RpcError> {
        let settings = self.resolved_settings(root);
        let provider = provider.unwrap_or_else(|| next_action::default_provider(&settings));
        let command = next_action::configured_command(&settings, &provider);
        let mut options = AgentOptions {
            provider,
            cwd: workspace.path.clone(),
            workspace: Some(workspace.id.clone()),
            title: Some("Agent".into()),
            model: None,
            bypass_permissions: true,
            resume: None,
            command,
            env: Default::default(),
            account: None,
            remote_control: next_action::remote_control_default(&settings),
        };
        self.apply_account(&mut options, None)?;
        let agent = self
            .agents
            .create(options)
            .await
            .map_err(|e| rpc("spawn_failed", format!("{e:#}")))?;
        if let Some(prompt) = prompt {
            agent
                .send(prompt, &[])
                .await
                .map_err(|e| rpc("send_failed", format!("{e:#}")))?;
        }
        Ok(agent.info().id)
    }

    pub(crate) async fn layouts_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct Scope {
            project: Option<String>,
            workspace: Option<String>,
            name: Option<String>,
        }
        match method {
            "layout.list" | "layout.get" => {
                let scope: Scope = params(p)?;
                let root = match (&scope.workspace, &scope.project) {
                    (Some(key), _) => self.project_root_of(&self.workspace_by_key(key)?),
                    (None, Some(key)) => self
                        .store
                        .project(key)
                        .map_err(|e| rpc("internal", e))?
                        .map(|project| project.root),
                    (None, None) => None,
                };
                let layouts = self.layouts_for(root.as_deref())?;
                if method == "layout.list" {
                    return Ok(json!(layouts));
                }
                let name = scope
                    .name
                    .ok_or_else(|| rpc("invalid_params", "name is required"))?;
                find_layout(&layouts, &name)
                    .map(|layout| json!(layout))
                    .ok_or_else(|| rpc("not_found", format!("layout {name}")))
            }
            "workspace.layout" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(alias = "workspace")]
                    id: String,
                    #[serde(alias = "name")]
                    layout: String,
                    pr_url: Option<String>,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                self.apply_layout(&workspace, &p.layout, p.pr_url).await
            }
            "workspace.take_layout" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(alias = "workspace")]
                    id: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let key = pending_key(&workspace.id);
                let pending = self.store.kv_get(&key).map_err(|e| rpc("internal", e))?;
                if pending.is_some() {
                    self.store.kv_delete(&key).map_err(|e| rpc("internal", e))?;
                }
                Ok(pending.unwrap_or(Value::Null))
            }
            "workspace.pending_layouts" => {
                let workspaces = self
                    .store
                    .workspaces(None)
                    .map_err(|e| rpc("internal", e))?;
                let mut pending = Vec::new();
                for workspace in workspaces {
                    if let Some(value) = self
                        .store
                        .kv_get(&pending_key(&workspace.id))
                        .map_err(|e| rpc("internal", e))?
                    {
                        pending.push(value);
                    }
                }
                Ok(json!(pending))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_override_and_extend_the_builtins() {
        let settings = json!({ "layouts": {
            "review": { "tabs": [{ "type": "agent" }] },
            "focus": { "split": "column", "children": [
                { "tabs": [{ "type": "agent" }] }, { "tabs": [{ "type": "terminal" }] }
            ]},
        }});
        let layouts = layout_definitions(&settings).expect("valid");
        assert_eq!(
            layouts.len(),
            3,
            "default, the overridden review, and focus"
        );
        assert_eq!(
            find_layout(&layouts, "review")
                .expect("review")
                .tabs()
                .len(),
            1
        );
        assert!(
            find_layout(&layouts, "review_split").is_some(),
            "older rules still find it"
        );
    }

    #[test]
    fn a_broken_layout_names_itself() {
        let settings = json!({ "layouts": { "bad": { "tabs": [] } } });
        let error = layout_definitions(&settings).expect_err("invalid");
        assert!(error.starts_with("layouts.bad"), "{error}");
    }
}
