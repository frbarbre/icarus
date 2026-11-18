//! Named tab layouts (plan D32): resolved on the server, stored on the workspace, applied by the app.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

const TAB_PENDING: &str = "tab.pending:";
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Ids that sort in the order they were made, so queued tab and pane changes apply in order.
fn request_id() -> String {
    static LAST: Mutex<u64> = Mutex::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    *last = now.max(*last + 1);
    format!("{:016x}", *last)
}

/// Requests queued before ops existed are tab opens.
fn with_op(mut pending: Value) -> Value {
    if pending.get("op").is_none() {
        pending["op"] = json!("open");
    }
    pending
}

type Answers = tokio::sync::mpsc::UnboundedSender<Value>;

fn queries() -> std::sync::MutexGuard<'static, HashMap<String, Answers>> {
    static QUERIES: LazyLock<Mutex<HashMap<String, Answers>>> = LazyLock::new(Default::default);
    QUERIES.lock().unwrap_or_else(|p| p.into_inner())
}

/// A `layout_query` waiting for `pane.answer`; dropping it stops listening.
struct PendingQuery(String);

impl PendingQuery {
    fn register(id: &str, sender: Answers) -> Self {
        queries().insert(id.to_owned(), sender);
        Self(id.to_owned())
    }

    fn answer(id: &str, layout: Value) -> bool {
        queries()
            .get(id)
            .is_some_and(|sender| sender.send(layout).is_ok())
    }
}

impl Drop for PendingQuery {
    fn drop(&mut self) {
        queries().remove(&self.0);
    }
}

/// `active`, `p<id>`, or a path through the split tree: `left`, `right`, `0.1`, `right.top`.
fn check_pane_ref(pane: &str) -> Result<(), RpcError> {
    let word = |s: &str| {
        matches!(s, "left" | "right" | "top" | "bottom" | "first" | "last")
            || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    };
    let id = pane
        .strip_prefix('p')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if pane == "active" || id || pane.split('.').all(word) {
        Ok(())
    } else {
        Err(rpc(
            "invalid_params",
            format!("pane `{pane}`: use active, an id like p12, or a path like left, right or 0.1"),
        ))
    }
}

fn check_tab_ref(tab: &str) -> Result<(), RpcError> {
    if tab == "active" || (!tab.is_empty() && tab.bytes().all(|b| b.is_ascii_digit())) {
        Ok(())
    } else {
        Err(rpc(
            "invalid_params",
            format!("tab `{tab}`: use an id from `tab list`, or active"),
        ))
    }
}

fn tab_op(
    method: &str,
    tab: Option<String>,
    pane: Option<String>,
    index: Option<usize>,
    direction: Option<String>,
) -> Result<Value, RpcError> {
    let required = |value: Option<String>, name: &str| {
        value.ok_or_else(|| rpc("invalid_params", format!("{method} needs {name}")))
    };
    if let Some(pane) = pane.as_deref() {
        check_pane_ref(pane)?;
    }
    Ok(match method {
        "tab.focus" | "tab.close" => {
            let tab = required(tab, "tab")?;
            check_tab_ref(&tab)?;
            json!({ "op": method.trim_start_matches("tab."), "tab": tab })
        }
        "tab.move" => {
            let tab = required(tab, "tab")?;
            check_tab_ref(&tab)?;
            json!({ "op": "move", "tab": tab, "pane": required(pane, "pane")?, "index": index })
        }
        "pane.split" => {
            let direction = required(direction, "direction")?;
            if !matches!(direction.as_str(), "right" | "down") {
                return Err(rpc("invalid_params", "direction is right or down"));
            }
            json!({ "op": "split", "pane": pane, "direction": direction })
        }
        _ => json!({ "op": "close_pane", "pane": required(pane, "pane")? }),
    })
}

/// `tab.list` gives each pane's tabs; `pane.list` the split tree and a summary of each pane.
fn shape_listing(method: &str, workspace_id: &str, layout: Value) -> Value {
    let panes = layout["panes"].as_array().cloned().unwrap_or_default();
    if method == "tab.list" {
        return json!({ "workspace": workspace_id, "panes": panes });
    }
    let panes: Vec<Value> = panes
        .into_iter()
        .map(|pane| {
            let tabs = pane["tabs"].as_array().cloned().unwrap_or_default();
            let active_tab = tabs.iter().find(|t| t["active"] == true).cloned();
            json!({
                "id": pane["id"],
                "path": pane["path"],
                "active": pane["active"],
                "tabs": tabs.len(),
                "active_tab": active_tab.map(|t| json!({ "id": t["id"], "type": t["type"], "title": t["title"] })),
            })
        })
        .collect();
    json!({ "workspace": workspace_id, "tree": layout["tree"], "panes": panes })
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
        self.apply_layout_with(workspace, name, pr_url, &[]).await
    }

    /// `apply_layout`, plus existing server terminals (e.g. `defaultTabs`) added after the first pane's tabs.
    pub(crate) async fn apply_layout_with(
        self: &Arc<Self>,
        workspace: &Workspace,
        name: &str,
        pr_url: Option<String>,
        extra_terminals: &[String],
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
        for terminal in extra_terminals {
            crate::hooks::add_terminal_tab(&mut layout, terminal);
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
            permission_mode: None,
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

    /// Queues `op` for the workspace's window and tells connected apps; it waits in kv until a window claims it.
    fn queue_tab_op(&self, workspace: &Workspace, mut op: Value) -> Result<Value, RpcError> {
        op["workspace_id"] = json!(workspace.id);
        op["path"] = json!(workspace.path);
        let id = request_id();
        self.store
            .kv_set(&format!("{TAB_PENDING}{id}"), &op)
            .map_err(|e| rpc("internal", format!("{e:#}")))?;
        let delivered = self.send_to_apps("open_tab", json!({ "request": id }));
        Ok(json!({ "request": id, "delivered": delivered }))
    }

    /// Asks the apps for the workspace window's panes and tabs; the first app showing it answers.
    async fn query_layout(&self, workspace: &Workspace) -> Result<Value, RpcError> {
        let id = request_id();
        let (sender, mut answers) = tokio::sync::mpsc::unbounded_channel();
        let _query = PendingQuery::register(&id, sender);
        let asked = self.send_to_apps(
            "layout_query",
            json!({ "request": id, "workspace_id": workspace.id, "path": workspace.path }),
        );
        if asked == 0 {
            return Err(rpc("not_open", "no app is connected"));
        }
        let first = async {
            for _ in 0..asked {
                match answers.recv().await {
                    Some(Value::Null) => continue,
                    Some(layout) => return Some(layout),
                    None => break,
                }
            }
            None
        };
        tokio::time::timeout(QUERY_TIMEOUT, first)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| rpc("not_open", "the workspace has no window open"))
    }

    /// Tabs and panes in a workspace's window. Changes wait in kv until an app window claims them, like layouts.
    pub(crate) async fn tabs_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        const PREFIX: &str = TAB_PENDING;
        let internal = |e: anyhow::Error| rpc("internal", format!("{e:#}"));
        #[derive(Deserialize)]
        struct Target {
            workspace: String,
            tab: Option<String>,
            pane: Option<String>,
            index: Option<usize>,
            direction: Option<String>,
        }
        match method {
            "tab.list" | "pane.list" => {
                let target: Target = params(p)?;
                let workspace = self.workspace_by_key(&target.workspace)?;
                let layout = self.query_layout(&workspace).await?;
                Ok(shape_listing(method, &workspace.id, layout))
            }
            "pane.answer" => {
                #[derive(Deserialize)]
                struct P {
                    request: String,
                    #[serde(default)]
                    layout: Value,
                }
                let p: P = params(p)?;
                Ok(json!(PendingQuery::answer(&p.request, p.layout)))
            }
            "tab.focus" | "tab.move" | "tab.close" | "pane.split" | "pane.close" => {
                let target: Target = params(p)?;
                let workspace = self.workspace_by_key(&target.workspace)?;
                let op = tab_op(
                    method,
                    target.tab,
                    target.pane,
                    target.index,
                    target.direction,
                )?;
                self.queue_tab_op(&workspace, op)
            }
            "tab.open" => {
                #[derive(Deserialize)]
                struct P {
                    workspace: String,
                    split: Option<String>,
                    pane: Option<String>,
                    index: Option<usize>,
                    #[serde(default)]
                    background: bool,
                    #[serde(default)]
                    focus: bool,
                    mode: Option<String>,
                    title: Option<String>,
                }
                let request: P = params(p.clone())?;
                if let Some(split) = request.split.as_deref()
                    && !matches!(split, "right" | "down")
                {
                    return Err(rpc("invalid_params", "split is right or down"));
                }
                if request.background && request.focus {
                    return Err(rpc(
                        "invalid_params",
                        "focus and background exclude each other",
                    ));
                }
                if let Some(pane) = request.pane.as_deref() {
                    check_pane_ref(pane)?;
                }
                let workspace = self.workspace_by_key(&request.workspace)?;
                let mut tab: Tab = serde_json::from_value(p)
                    .map_err(|e| rpc("invalid_params", format!("tab: {e}")))?;
                // The agent starts here, so it runs even before a window shows its tab.
                if let Tab::Agent {
                    prompt,
                    provider,
                    agent_id: agent_id @ None,
                } = &mut tab
                {
                    let created = self
                        .call_internal(
                            "agent.create",
                            json!({
                                "workspace": workspace.id,
                                "prompt": prompt.take(),
                                "provider": provider.take(),
                                "mode": request.mode,
                                "title": request.title,
                            }),
                        )
                        .await?;
                    *agent_id = created["id"].as_str().map(str::to_owned);
                }
                let tab = json!(tab);
                let mut queued = self.queue_tab_op(
                    &workspace,
                    json!({
                        "op": "open",
                        "tab": tab,
                        "split": request.split,
                        "pane": request.pane,
                        "index": request.index,
                        "background": request.background,
                        "focus": request.focus,
                    }),
                )?;
                queued["tab"] = tab;
                Ok(queued)
            }
            "tab.pending" => {
                let mut pending = self.store.kv_prefix(PREFIX).map_err(internal)?;
                pending.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(json!(
                    pending
                        .into_iter()
                        .map(|(key, tab)| json!({
                            "request": key.trim_start_matches(PREFIX),
                            "tab": with_op(tab),
                        }))
                        .collect::<Vec<_>>()
                ))
            }
            "tab.take" => {
                #[derive(Deserialize)]
                struct P {
                    request: String,
                }
                let p: P = params(p)?;
                let key = format!("{PREFIX}{}", p.request);
                let pending = self.store.kv_get(&key).map_err(internal)?;
                if pending.is_some() {
                    self.store.kv_delete(&key).map_err(internal)?;
                }
                Ok(pending.map_or(Value::Null, with_op))
            }
            other => Err(rpc("unknown_method", other)),
        }
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
