//! Layered settings over RPC: defaults < presets < user < project (`icarus.json`) < project-local.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use icarus_protocol::RpcError;
use icarus_settings::import::{export_orca, import_orca_text};
use icarus_settings::layers::{self, KeyPath, Layer, LayerStack, SettingsPaths};
use icarus_settings::zed;
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::server::{Server, params, rpc};

pub(crate) fn to_json(value: &icarus_settings::Value) -> Json {
    serde_json::from_str(&value.to_compact_string()).unwrap_or(Json::Null)
}

fn from_json(value: &Json) -> Result<icarus_settings::Value, RpcError> {
    icarus_settings::json::parse(&value.to_string())
        .map_err(|e| rpc("invalid_params", format!("{e:?}")))
}

fn layer_name(name: Option<&str>) -> Result<Layer, RpcError> {
    match name.unwrap_or("user") {
        "user" => Ok(Layer::User),
        "project" => Ok(Layer::Project),
        "project-local" | "local" => Ok(Layer::ProjectLocal),
        other => Err(rpc(
            "invalid_params",
            format!("cannot write the {other} layer"),
        )),
    }
}

#[derive(Deserialize)]
struct Scope {
    project: Option<String>,
}

#[derive(Deserialize)]
struct KeyWrite {
    key: String,
    value: Option<Json>,
    layer: Option<String>,
    project: Option<String>,
}

impl Server {
    /// Sets (or with `None` unsets) a key in one writable layer.
    fn write_key(
        &self,
        key: &str,
        value: Option<Json>,
        layer: Option<&str>,
        project: Option<&str>,
    ) -> Result<Json, RpcError> {
        let layer = layer_name(layer)?;
        let root = self.project_root(project)?;
        if layer != Layer::User && root.is_none() {
            return Err(rpc("invalid_params", "project layers need --project"));
        }
        let paths = SettingsPaths::discover(root.as_deref());
        let path = paths
            .path(layer)
            .ok_or_else(|| rpc("internal", "no settings path"))?
            .to_path_buf();
        let mut doc = layers::read_layer(layer, &path)
            .map_err(|e| rpc("settings_invalid", format!("{e:?}")))?
            .unwrap_or_else(icarus_settings::Value::object);
        let key = KeyPath::parse(key).map_err(|e| rpc("invalid_key", format!("{e:?}")))?;
        match value {
            Some(value) => {
                let value = from_json(&value)?;
                layers::set(&mut doc, &key, value)
                    .map_err(|e| rpc("invalid_key", format!("{e:?}")))?;
            }
            None => {
                layers::unset(&mut doc, &key);
            }
        }
        self.write_layer(layer, &path, &doc)?;
        Ok(json!({ "layer": layer.name(), "path": path }))
    }

    fn write_layer(
        &self,
        layer: Layer,
        path: &Path,
        doc: &icarus_settings::Value,
    ) -> Result<(), RpcError> {
        let errors = layers::validate(doc, Some(layer));
        if !errors.is_empty() {
            let described: Vec<String> = errors.iter().map(|e| format!("{e:?}")).collect();
            return Err(rpc("settings_invalid", described.join("; ")));
        }
        if layer == Layer::User {
            for name in layers::preset_names(doc) {
                if layers::preset(&name).is_none() {
                    return Err(rpc("settings_invalid", format!("unknown preset `{name}`")));
                }
            }
        }
        layers::write_document(path, doc).map_err(|e| rpc("io", e))
    }

    fn project_root(&self, project: Option<&str>) -> Result<Option<PathBuf>, RpcError> {
        match project {
            None => Ok(None),
            Some(key) => self
                .store
                .project(key)
                .map_err(|e| rpc("internal", e))?
                .map(|p| Some(p.root))
                .ok_or_else(|| rpc("not_found", format!("project {key}"))),
        }
    }

    pub(crate) fn settings_stack(
        &self,
        project_root: Option<&Path>,
    ) -> Result<LayerStack, RpcError> {
        let paths = SettingsPaths::discover(project_root);
        let user = match paths
            .user
            .as_deref()
            .map(|path| layers::read_layer(Layer::User, path))
        {
            Some(Ok(Some(doc))) => Some(doc),
            Some(Err(error)) => return Err(rpc("settings_invalid", format!("{error:?}"))),
            _ => None,
        };
        let presets: Vec<String> = user
            .as_ref()
            .and_then(|doc| doc.get("presets"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let preset_refs: Vec<&str> = presets.iter().map(String::as_str).collect();
        LayerStack::load(&paths, &preset_refs)
            .map_err(|e| rpc("settings_invalid", format!("{e:?}")))
    }

    /// Resolved settings as JSON, for other server modules.
    pub(crate) fn resolved_settings(&self, project_root: Option<&Path>) -> Json {
        self.settings_stack(project_root)
            .map(|s| to_json(&s.resolve()))
            .unwrap_or(Json::Null)
    }

    pub(crate) async fn settings_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Json,
    ) -> Result<Json, RpcError> {
        match method {
            "config.get" => {
                #[derive(Deserialize)]
                struct P {
                    key: Option<String>,
                    project: Option<String>,
                }
                let p: P = params(p)?;
                let root = self.project_root(p.project.as_deref())?;
                let stack = self.settings_stack(root.as_deref())?;
                match p.key.filter(|k| !k.is_empty()) {
                    None => Ok(to_json(&stack.resolve())),
                    Some(key) => {
                        let value = stack
                            .get(&key)
                            .map_err(|e| rpc("invalid_key", format!("{e:?}")))?;
                        Ok(value.as_ref().map(to_json).unwrap_or(Json::Null))
                    }
                }
            }
            "config.explain" => {
                #[derive(Deserialize)]
                struct P {
                    key: String,
                    project: Option<String>,
                }
                let p: P = params(p)?;
                let root = self.project_root(p.project.as_deref())?;
                let stack = self.settings_stack(root.as_deref())?;
                let explanation = stack
                    .explain(&p.key)
                    .map_err(|e| rpc("invalid_key", format!("{e:?}")))?;
                let source = |s: &layers::Source| json!({ "layer": s.layer.name(), "label": s.label, "path": s.origin, "value": to_json(&s.value) });
                Ok(json!({
                    "key": p.key,
                    "value": explanation.value.as_ref().map(to_json),
                    "from": explanation.winner().map(source),
                    "contributors": explanation.contributors.iter().map(source).collect::<Vec<_>>(),
                    "overridden": explanation.overridden.iter().map(source).collect::<Vec<_>>(),
                }))
            }
            "config.set" | "config.unset" => {
                let p: KeyWrite = params(p)?;
                let value = if method == "config.set" {
                    Some(p.value.unwrap_or(Json::Null))
                } else {
                    None
                };
                self.write_key(&p.key, value, p.layer.as_deref(), p.project.as_deref())
            }
            "config.schema" => Ok(to_json(&layers::json_schema())),
            "config.paths" => {
                #[derive(Deserialize)]
                struct P {
                    project: Option<String>,
                }
                let p: P = params(p)?;
                let root = self.project_root(p.project.as_deref())?;
                let paths = SettingsPaths::discover(root.as_deref());
                Ok(json!({
                    "user": paths.user,
                    "project": paths.project,
                    "local": paths.project_local,
                }))
            }
            "settings.import_orca" => {
                #[derive(Deserialize)]
                struct P {
                    path: Option<PathBuf>,
                    text: Option<String>,
                    #[serde(default)]
                    dry_run: bool,
                    /// Leave the Zed settings and themes alone.
                    #[serde(default)]
                    skip_zed: bool,
                    /// The Zed config directory to merge into; defaults to the app's.
                    zed_dir: Option<PathBuf>,
                }
                let p: P = params(p)?;
                let text = match (p.text, p.path) {
                    (Some(text), _) => text,
                    (None, Some(path)) => {
                        std::fs::read_to_string(&path).map_err(|e| rpc("io", e))?
                    }
                    (None, None) => return Err(rpc("invalid_params", "path or text required")),
                };
                let import = import_orca_text(&text)
                    .map_err(|e| rpc("invalid_orca_export", format!("{e:?}")))?;
                let mut written = Vec::new();
                let mut unmatched = Vec::new();
                let user_path = layers::user_settings_path()
                    .ok_or_else(|| rpc("io", "no user settings path"))?;
                let mut user = layers::read_layer(Layer::User, &user_path)
                    .map_err(|e| rpc("settings_invalid", format!("{e:?}")))?
                    .unwrap_or_else(icarus_settings::Value::object);
                layers::deep_merge(&mut user, &import.user);
                let zed_dir = match p.zed_dir {
                    Some(dir) => dir,
                    None => {
                        zed::config_dir().ok_or_else(|| rpc("io", "no Zed config directory"))?
                    }
                };
                let applied = if p.skip_zed {
                    None
                } else {
                    Some(
                        zed::apply(&zed_dir, &import.zed, import.zed_theme.as_ref(), p.dry_run)
                            .map_err(|e| rpc("zed_settings", e))?,
                    )
                };
                // The export carries Orca's keybindings file; it becomes the app's keymap.
                let keybindings = import.user.get("keybindings").and_then(|value| {
                    serde_json::from_str::<Json>(&value.to_compact_string()).ok()
                });
                let keymap = match (&keybindings, p.skip_zed) {
                    (Some(orca), false) => {
                        let translation = icarus_settings::keymap::translate(
                            orca,
                            icarus_settings::keymap::Platform::current(),
                        );
                        let applied =
                            icarus_settings::keymap::apply(&zed_dir, &translation, p.dry_run)
                                .map_err(|e| rpc("zed_keymap", e))?;
                        json!({ "translation": translation, "applied": applied })
                    }
                    _ => Json::Null,
                };
                let zed_result = json!({
                    "settings": applied.as_ref().map(|a| &a.settings),
                    "theme": applied.as_ref().and_then(|a| a.theme.as_ref()),
                    "added": applied.as_ref().map(|a| &a.added),
                    "kept": applied.as_ref().map(|a| &a.kept),
                    "backup": applied.as_ref().and_then(|a| a.backup.as_ref()),
                    "fragment": to_json(&import.zed),
                    "keymap": keymap,
                });
                if !p.dry_run {
                    if let Some(parent) = user_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| rpc("io", e))?;
                    }
                    layers::write_document(&user_path, &user).map_err(|e| rpc("io", e))?;
                    written.push(json!({ "layer": "user", "path": user_path }));
                    if let Some(applied) = &applied {
                        if !applied.added.is_empty() {
                            written.push(json!({ "layer": "zed", "path": applied.settings }));
                        }
                        if let Some(theme) = &applied.theme {
                            written.push(json!({ "layer": "zed-theme", "path": theme }));
                        }
                    }
                    let projects = self.store.projects().map_err(|e| rpc("internal", e))?;
                    for (remote, doc) in &import.projects {
                        // Imported project settings are personal, so they go to the untracked local file.
                        match projects
                            .iter()
                            .find(|p| p.remote.as_deref() == Some(remote.as_str()))
                        {
                            Some(project) => {
                                let path = project.root.join(layers::PROJECT_LOCAL_FILE);
                                let mut local = layers::read_document(&path)
                                    .map_err(|e| rpc("settings_invalid", format!("{e:?}")))?
                                    .unwrap_or_else(icarus_settings::Value::object);
                                layers::deep_merge(&mut local, doc);
                                layers::write_document(&path, &local).map_err(|e| rpc("io", e))?;
                                written.push(json!({ "layer": "project-local", "project": project.name, "path": path }));
                            }
                            None => unmatched.push(remote.clone()),
                        }
                    }
                }
                Ok(json!({
                    "written": written,
                    "unmatched_projects": unmatched,
                    "mapped": import.report.mapped.len(),
                    "mappings": import.report.mapped.iter().map(|m| json!({ "from": m.from, "to": m.to })).collect::<Vec<_>>(),
                    "unmapped": import.report.unmapped,
                    "notes": import.report.notes,
                    "zed": zed_result,
                    "validation": import.report.validation.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>(),
                }))
            }
            "settings.export_orca" => {
                let scope: Scope = params(p)?;
                let user_path = layers::user_settings_path()
                    .ok_or_else(|| rpc("io", "no user settings path"))?;
                let user = layers::read_layer(Layer::User, &user_path)
                    .map_err(|e| rpc("settings_invalid", format!("{e:?}")))?
                    .unwrap_or_else(icarus_settings::Value::object);
                let mut projects = indexmap::IndexMap::new();
                for project in self.store.projects().map_err(|e| rpc("internal", e))? {
                    if scope
                        .project
                        .as_deref()
                        .is_some_and(|key| key != project.name && key != project.id)
                    {
                        continue;
                    }
                    if let (Some(remote), Ok(Some(doc))) = (
                        project.remote.clone(),
                        layers::read_document(&project.root.join(layers::PROJECT_LOCAL_FILE)),
                    ) {
                        projects.insert(remote, doc);
                    }
                }
                let export = export_orca(&user, &projects)
                    .map_err(|e| rpc("export_failed", format!("{e:?}")))?;
                Ok(json!({ "text": export.to_text() }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}
