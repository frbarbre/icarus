mod path;
mod schema;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::json::{self, ParseError, Value};

pub use path::{KeyError, KeyPath, get, parse_cli_value, set, unset};
pub use schema::{Field, Schema, ValidationError, document_schema, json_schema, validate};

pub const PROJECT_FILE: &str = "icarus.json";
pub const PROJECT_LOCAL_FILE: &str = "icarus.local.json";
pub const USER_FILE: &str = "icarus.json";
// User settings lived here until the app took this directory for its Zed `settings.json`.
const LEGACY_USER_FILE: &str = "settings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    Defaults,
    User,
    Project,
    ProjectLocal,
}

impl Layer {
    pub const ALL: [Layer; 4] = [
        Layer::Defaults,
        Layer::User,
        Layer::Project,
        Layer::ProjectLocal,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Layer::Defaults => "defaults",
            Layer::User => "user",
            Layer::Project => "project",
            Layer::ProjectLocal => "project-local",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layer| layer.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerDoc {
    pub layer: Layer,
    pub label: String,
    pub origin: Option<PathBuf>,
    pub doc: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub layer: Layer,
    pub label: String,
    pub origin: Option<PathBuf>,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub key: KeyPath,
    pub value: Option<Value>,
    pub contributors: Vec<Source>,
    pub overridden: Vec<Source>,
}

impl Explanation {
    pub fn winner(&self) -> Option<&Source> {
        self.contributors.last()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid JSON: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: ParseError,
    },
    #[error("{} must contain a JSON object", path.display())]
    NotObject { path: PathBuf },
}

pub fn defaults() -> Value {
    json::parse(include_str!("defaults.json")).unwrap_or_else(|_| Value::object())
}

// Objects merge key by key; arrays, scalars and null replace whatever was there.
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

pub fn config_dir() -> Option<PathBuf> {
    // Lets a separate profile use its own settings without moving other tools' config (e.g. `gh`).
    if let Some(dir) = std::env::var_os("ICARUS_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Some(dir);
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("icarus"))
}

pub fn user_settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(USER_FILE))
}

// Reads only: writes always target `path`, so the first write moves legacy settings into `icarus.json`.
pub fn layer_source(layer: Layer, path: &Path) -> PathBuf {
    if layer != Layer::User || path.file_name() != Some(USER_FILE.as_ref()) || path.exists() {
        return path.to_owned();
    }
    let Some(legacy) = path.parent().map(|dir| dir.join(LEGACY_USER_FILE)) else {
        return path.to_owned();
    };
    match read_document(&legacy) {
        Ok(Some(doc)) if holds_icarus_settings(&doc) => legacy,
        _ => path.to_owned(),
    }
}

// The app's Zed `settings.json` shares the name, and old Icarus files may hold stale keys, so look for an Icarus-only key.
fn holds_icarus_settings(doc: &Value) -> bool {
    let Schema::Object { fields, .. } = document_schema() else {
        return false;
    };
    fields
        .iter()
        .any(|field| !matches!(field.name, "$schema" | "theme") && doc.get(field.name).is_some())
}

pub fn read_layer(layer: Layer, path: &Path) -> Result<Option<Value>, LoadError> {
    read_document(&layer_source(layer, path))
}

pub fn read_document(path: &Path) -> Result<Option<Value>, LoadError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LoadError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let doc = json::parse(&text).map_err(|source| LoadError::Parse {
        path: path.to_owned(),
        source,
    })?;
    if !doc.is_object() {
        return Err(LoadError::NotObject {
            path: path.to_owned(),
        });
    }
    Ok(Some(doc))
}

pub fn write_document(path: &Path, doc: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = doc.to_pretty_string();
    text.push('\n');
    fs::write(path, text)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettingsPaths {
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub project_local: Option<PathBuf>,
}

impl SettingsPaths {
    pub fn discover(project_root: Option<&Path>) -> Self {
        Self {
            user: user_settings_path(),
            project: project_root.map(|root| root.join(PROJECT_FILE)),
            project_local: project_root.map(|root| root.join(PROJECT_LOCAL_FILE)),
        }
    }

    pub fn path(&self, layer: Layer) -> Option<&Path> {
        match layer {
            Layer::User => self.user.as_deref(),
            Layer::Project => self.project.as_deref(),
            Layer::ProjectLocal => self.project_local.as_deref(),
            Layer::Defaults => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayerStack {
    docs: Vec<LayerDoc>,
}

impl LayerStack {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        let mut stack = Self::empty();
        stack.push(LayerDoc {
            layer: Layer::Defaults,
            label: "built-in defaults".to_owned(),
            origin: None,
            doc: defaults(),
        });
        stack
    }

    pub fn load(paths: &SettingsPaths) -> Result<Self, LoadError> {
        let mut stack = Self::with_defaults();
        for layer in [Layer::User, Layer::Project, Layer::ProjectLocal] {
            let Some(path) = paths.path(layer) else {
                continue;
            };
            let source = layer_source(layer, path);
            if let Some(doc) = read_document(&source)? {
                stack.push(LayerDoc {
                    layer,
                    label: source.display().to_string(),
                    origin: Some(source),
                    doc,
                });
            }
        }
        Ok(stack)
    }

    // Documents of the same layer apply in the order they were pushed.
    pub fn push(&mut self, doc: LayerDoc) {
        let index = self.docs.partition_point(|d| d.layer <= doc.layer);
        self.docs.insert(index, doc);
    }

    pub fn docs(&self) -> &[LayerDoc] {
        &self.docs
    }

    pub fn layer_doc(&self, layer: Layer) -> Option<&LayerDoc> {
        self.docs.iter().rev().find(|d| d.layer == layer)
    }

    pub fn layer_doc_mut(&mut self, layer: Layer) -> Option<&mut LayerDoc> {
        self.docs.iter_mut().rev().find(|d| d.layer == layer)
    }

    pub fn resolve(&self) -> Value {
        let mut merged = Value::object();
        for doc in &self.docs {
            deep_merge(&mut merged, &doc.doc);
        }
        merged
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>, KeyError> {
        let path = KeyPath::parse(key)?;
        Ok(get(&self.resolve(), &path).cloned())
    }

    pub fn explain(&self, key: &str) -> Result<Explanation, KeyError> {
        let path = KeyPath::parse(key)?;
        let mut merged = Value::object();
        let mut contributors: Vec<Source> = Vec::new();
        let mut overridden: Vec<Source> = Vec::new();
        for doc in &self.docs {
            let before = get(&merged, &path).cloned();
            deep_merge(&mut merged, &doc.doc);
            let after = get(&merged, &path).cloned();
            let own = get(&doc.doc, &path);
            let merges = matches!(
                (&before, own, &after),
                (
                    Some(Value::Object(_)),
                    Some(Value::Object(_)),
                    Some(Value::Object(_))
                )
            );
            let source = |value: Value| Source {
                layer: doc.layer,
                label: doc.label.clone(),
                origin: doc.origin.clone(),
                value,
            };
            if merges {
                contributors.push(source(own.cloned().unwrap_or_default()));
            } else if own.is_some() || before != after {
                overridden.append(&mut contributors);
                if let Some(value) = after {
                    contributors.push(source(value));
                }
            }
        }
        Ok(Explanation {
            value: get(&merged, &path).cloned(),
            key: path,
            contributors,
            overridden,
        })
    }

    pub fn validate(&self) -> Vec<ValidationError> {
        self.docs
            .iter()
            .flat_map(|doc| validate(&doc.doc, Some(doc.layer)))
            .collect()
    }
}
