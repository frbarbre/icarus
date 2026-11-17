use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use icarus_git::filters::{self, Classifier, FilterConfig, Hidden, ListQuery, Scope};
use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, params, rpc, to_value};
use crate::state::Workspace;

type Selection = BTreeMap<String, Vec<String>>;

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", error)
}

fn hidden_key(workspace: &str) -> String {
    format!("changes.hidden.{workspace}")
}

fn to_hidden(selection: Selection) -> Hidden {
    selection
        .into_iter()
        .map(|(facet, values)| (facet, values.into_iter().collect::<BTreeSet<_>>()))
        .filter(|(_, values)| !values.is_empty())
        .collect()
}

impl Server {
    fn classifier(&self, workspace: &Workspace) -> Result<Classifier, RpcError> {
        let root = self
            .store
            .project(&workspace.project_id)
            .map_err(internal)?
            .map(|p| p.root);
        let settings = self.resolved_settings(root.as_deref());
        FilterConfig::preset()
            .with_overlay(&settings["changes"])
            .and_then(Classifier::new)
            .map_err(|e| rpc("filters_invalid", format!("{e:#}")))
    }

    // Persisted so hidden values survive restarts and the CLI and every client agree (§8.2).
    fn stored_hidden(&self, workspace: &str) -> Hidden {
        self.store
            .kv_get(&hidden_key(workspace))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    pub(crate) async fn changes_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "workspace.changes" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    scope: Option<String>,
                    hide: Option<Selection>,
                    text: Option<String>,
                    base: Option<String>,
                    #[serde(default)]
                    uncommitted_only: bool,
                    #[serde(default)]
                    all_rows: bool,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let scope = match p.scope.as_deref() {
                    Some(text) => {
                        Scope::parse(text).map_err(|e| rpc("invalid_params", format!("{e:#}")))?
                    }
                    None if p.uncommitted_only => Scope::Uncommitted,
                    None => Scope::Base,
                };
                let query = ListQuery {
                    base: p.base.or(workspace.base_ref.clone()),
                    scope,
                    hidden: match p.hide {
                        Some(selection) => to_hidden(selection),
                        None => self.stored_hidden(&workspace.id),
                    },
                    text: p.text,
                    all_rows: p.all_rows,
                };
                let classifier = self.classifier(&workspace)?;
                let path = workspace.path.clone();
                let listing =
                    tokio::task::spawn_blocking(move || filters::list(&path, &classifier, &query))
                        .await
                        .map_err(internal)?
                        .map_err(|e| rpc("git_failed", format!("{e:#}")))?;
                to_value(listing)
            }
            "workspace.changes.explain" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    path: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let classifier = self.classifier(&workspace)?;
                let (root, base) = (workspace.path.clone(), workspace.base_ref.clone());
                let explanation = tokio::task::spawn_blocking(move || {
                    filters::explain(&root, &classifier, base, &p.path)
                })
                .await
                .map_err(internal)?
                .map_err(|e| rpc("git_failed", format!("{e:#}")))?;
                to_value(explanation)
            }
            "workspace.changes.filter" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    #[serde(default)]
                    reset: bool,
                    set: Option<Selection>,
                    #[serde(default)]
                    hide: Selection,
                    #[serde(default)]
                    show: Selection,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let mut hidden = if p.reset {
                    Hidden::new()
                } else {
                    self.stored_hidden(&workspace.id)
                };
                if let Some(set) = p.set {
                    hidden = to_hidden(set);
                }
                for (facet, values) in p.hide {
                    hidden.entry(facet).or_default().extend(values);
                }
                for (facet, values) in p.show {
                    if let Some(set) = hidden.get_mut(&facet) {
                        for value in values {
                            set.remove(&value);
                        }
                    }
                }
                hidden.retain(|_, values| !values.is_empty());
                self.store
                    .kv_set(&hidden_key(&workspace.id), &json!(hidden))
                    .map_err(internal)?;
                Ok(json!({ "hidden": hidden }))
            }
            "workspace.turns" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                }
                let p: P = params(p)?;
                let workspace = self.workspace_by_key(&p.id)?;
                let path = workspace.path.clone();
                let turns =
                    tokio::task::spawn_blocking(move || icarus_git::turns::list_turns(&path))
                        .await
                        .map_err(internal)?
                        .map_err(|e| rpc("git_failed", format!("{e:#}")))?;
                to_value(turns)
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}
