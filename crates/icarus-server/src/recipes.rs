//! Orca's `environmentRecipes` (per-workspace environments, "Cloud VM"): a recipe's `create` command
//! provisions a machine and prints how to reach the Icarus server on it; `suspend`, `resume` and
//! `destroy` get the same instance back as JSON on stdin. Icarus runs the commands exactly as Orca's
//! `ephemeral-vm-recipe-process.ts` does; the client pairs with the result and opens the workspace there.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use icarus_protocol::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::server::{Server, rpc};

const MAX_RECIPES: usize = 256;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub create: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destroy_disabled: bool,
}

impl Recipe {
    fn provisioned_root(&self) -> bool {
        self.checkout_mode.as_deref() == Some("provisioned-root")
    }
}

/// Why an entry of `environmentRecipes` was dropped, as Orca's diagnostics say it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeProblem {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase() | bytes[0].is_ascii_digit()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// Orca's `normalizeVmRecipes`.
pub(crate) fn parse(value: &yaml_rust2::Yaml) -> (Vec<Recipe>, Vec<RecipeProblem>) {
    let mut problems = Vec::new();
    let Some(entries) = value.as_vec() else {
        return (Vec::new(), problems);
    };
    if entries.len() > MAX_RECIPES {
        problems.push(RecipeProblem {
            index: MAX_RECIPES,
            field: None,
            message: format!("At most {MAX_RECIPES} environment recipes are supported."),
        });
        return (Vec::new(), problems);
    }
    let text = |value: &yaml_rust2::Yaml| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let mut recipes: Vec<Recipe> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let mut problem = |field: Option<&str>, message: String| {
            problems.push(RecipeProblem {
                index,
                field: field.map(str::to_owned),
                message,
            });
        };
        if entry.as_hash().is_none() {
            problem(None, "Recipe entry must be a mapping.".into());
            continue;
        }
        let Some(id) = text(&entry["id"]) else {
            problem(Some("id"), "Recipe id is required.".into());
            continue;
        };
        if !valid_id(&id) {
            problem(
                Some("id"),
                format!(
                    "Invalid recipe id \"{id}\". Use 1-64 lowercase letters, numbers, dots, underscores, or hyphens, starting with a letter or number."
                ),
            );
            continue;
        }
        if recipes.iter().any(|recipe| recipe.id == id) {
            problem(
                Some("id"),
                format!("Duplicate recipe id \"{id}\". Recipe ids must be unique."),
            );
            continue;
        }
        let Some(name) = text(&entry["name"]) else {
            problem(Some("name"), format!("Recipe \"{id}\" is missing name."));
            continue;
        };
        let Some(create) = text(&entry["create"]).or_else(|| text(&entry["command"])) else {
            problem(
                Some("create"),
                format!("Recipe \"{id}\" is missing create."),
            );
            continue;
        };
        let checkout_mode = text(&entry["checkoutMode"]);
        if checkout_mode
            .as_deref()
            .is_some_and(|mode| mode != "orca-worktree" && mode != "provisioned-root")
        {
            problem(
                Some("checkoutMode"),
                format!(
                    "Recipe \"{id}\" checkoutMode must be \"orca-worktree\" or \"provisioned-root\"."
                ),
            );
            continue;
        }
        let destroy = text(&entry["destroy"]).or_else(|| text(&entry["cleanup"]));
        let destroy_disabled = destroy.as_deref() == Some("none");
        recipes.push(Recipe {
            id,
            name,
            create,
            checkout_mode,
            description: text(&entry["description"]),
            suspend: text(&entry["suspend"]),
            resume: text(&entry["resume"]),
            destroy: destroy.filter(|_| !destroy_disabled),
            destroy_disabled,
        });
    }
    (recipes, problems)
}

/// What the user trusts before a recipe runs, in Orca's `getVmRecipeTrustContent` form.
pub(crate) fn trust_content(recipes: &[Recipe]) -> String {
    recipes
        .iter()
        .map(|recipe| {
            let mut lines = vec![
                format!("# environmentRecipes.{}", recipe.id),
                format!("name: {}", recipe.name),
            ];
            if let Some(description) = &recipe.description {
                lines.push(format!("description: {description}"));
            }
            lines.push(format!("create: {}", recipe.create));
            if let Some(suspend) = &recipe.suspend {
                lines.push(format!("suspend: {suspend}"));
            }
            if let Some(resume) = &recipe.resume {
                lines.push(format!("resume: {resume}"));
            }
            if recipe.destroy_disabled {
                lines.push("destroy: none".into());
            } else if let Some(destroy) = &recipe.destroy {
                lines.push(format!("destroy: {destroy}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Create,
    Suspend,
    Resume,
    Destroy,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Create => "create",
            Mode::Suspend => "suspend",
            Mode::Resume => "resume",
            Mode::Destroy => "destroy",
        }
    }
}

/// Who a recipe instance belongs to: Orca's `EphemeralVmRecipeContext`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Context {
    pub instance_id: String,
    pub recipe_id: String,
    pub project_id: String,
    pub workspace_name: String,
    pub repo_path: String,
    #[serde(default)]
    pub repo_url: String,
    #[serde(default)]
    pub branch: String,
}

/// Orca's `buildRecipeEnv`, with the `ICARUS_` names beside the `ORCA_` ones.
pub(crate) fn recipe_env(mode: Mode, context: &Context, schema: u8) -> BTreeMap<String, String> {
    let pairs = [
        ("VM_MODE", mode.name().to_owned()),
        ("VM_INSTANCE_ID", context.instance_id.clone()),
        ("RECIPE_ID", context.recipe_id.clone()),
        ("PROJECT_ID", context.project_id.clone()),
        ("WORKSPACE_ID", String::new()),
        ("WORKSPACE_NAME", context.workspace_name.clone()),
        ("REPO_PATH", context.repo_path.clone()),
        ("REPO_URL", context.repo_url.clone()),
        ("REPO_BRANCH", context.branch.clone()),
        ("REPO_REF", String::new()),
        ("REPO_REF_HEAD", String::new()),
        ("RECIPE_RESULT_SCHEMA_VERSION", schema.to_string()),
        ("VERSION", env!("CARGO_PKG_VERSION").to_owned()),
    ];
    let mut env = BTreeMap::new();
    for (name, value) in pairs {
        env.insert(format!("ORCA_{name}"), value.clone());
        env.insert(format!("ICARUS_{name}"), value);
    }
    env
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Run {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

fn tail(bytes: &[u8]) -> String {
    let mut start = bytes.len().saturating_sub(MAX_CAPTURE_BYTES);
    while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Runs one lifecycle command through the shell in the repository, in its own process group.
pub(crate) async fn run(
    command: &str,
    repo: &Path,
    env: &BTreeMap<String, String>,
    stdin: Option<String>,
) -> Result<Run, String> {
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(repo)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start the recipe: {error}"))?;
    let mut input = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let write = async {
        if let (Some(input), Some(text)) = (input.as_mut(), stdin) {
            let _ = input.write_all(text.as_bytes()).await;
        }
        drop(input);
    };
    let read_out = async {
        let mut bytes = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut bytes).await;
        }
        bytes
    };
    let read_err = async {
        let mut bytes = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut bytes).await;
        }
        bytes
    };
    let ((), out, err) = tokio::join!(write, read_out, read_err);
    let status = child
        .wait()
        .await
        .map_err(|error| format!("waiting for the recipe: {error}"))?;
    Ok(Run {
        stdout: tail(&out),
        stderr: tail(&err),
        exit_code: status.code(),
    })
}

/// How to reach the environment's server, from a create or resume result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Connection {
    /// An Icarus server listening for pairing (Orca's `orca-server`): `icarus://pair?address=…&code=…`.
    #[serde(alias = "orca-server", rename = "icarus-server")]
    IcarusServer {
        #[serde(rename = "pairingCode")]
        pairing_code: String,
        #[serde(rename = "projectRoot")]
        project_root: String,
    },
    Ssh {
        target: Value,
        #[serde(rename = "projectRoot")]
        project_root: String,
    },
}

impl Connection {
    pub fn project_root(&self) -> &str {
        match self {
            Connection::IcarusServer { project_root, .. }
            | Connection::Ssh { project_root, .. } => project_root,
        }
    }
}

/// A pairing offer: the address to dial, the one-time code and, optionally, the certificate to pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PairingOffer {
    pub address: String,
    pub code: String,
    pub fingerprint: Option<String>,
}

/// Reads `icarus://pair?address=HOST:PORT&code=CODE[&fingerprint=SHA256]`.
pub fn parse_pairing_code(text: &str) -> Option<PairingOffer> {
    let query = text.trim().strip_prefix("icarus://pair?")?;
    let mut offer = PairingOffer {
        address: String::new(),
        code: String::new(),
        fingerprint: None,
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = value.replace("%3A", ":").replace("%3a", ":");
        match key {
            "address" => offer.address = value,
            "code" => offer.code = value,
            "fingerprint" => offer.fingerprint = Some(value),
            _ => {}
        }
    }
    (!offer.address.is_empty() && !offer.code.is_empty()).then_some(offer)
}

/// Orca's `parseEphemeralVmRecipeResult` plus its checkout-mode check, returning the connection.
pub(crate) fn parse_result(recipe: &Recipe, stdout: &str) -> Result<(Value, Connection), String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("Recipe produced no JSON result.".into());
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|_| "Recipe stdout must be one JSON object.".to_owned())?;
    let object = value
        .as_object()
        .ok_or("Recipe stdout must be one JSON object.")?;
    let schema = object.get("schemaVersion").and_then(Value::as_u64);
    let allowed: &[&str] = match schema {
        Some(1) => &[
            "schemaVersion",
            "pairingCode",
            "projectRoot",
            "connection",
            "userData",
        ],
        Some(2) => &[
            "schemaVersion",
            "checkoutMode",
            "pairingCode",
            "projectRoot",
            "connection",
            "userData",
        ],
        _ => return Err("Recipe result schemaVersion must be 1 or 2.".into()),
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("Recipe result has an unknown key \"{key}\"."));
    }
    if schema == Some(2) && object.get("checkoutMode") != Some(&json!("provisioned-root")) {
        return Err(
            "Recipe result schemaVersion 2 needs checkoutMode \"provisioned-root\".".into(),
        );
    }
    let connection = match object.get("connection") {
        Some(connection) => serde_json::from_value::<Connection>(connection.clone())
            .map_err(|error| format!("Invalid recipe connection: {error}"))?,
        None => Connection::IcarusServer {
            pairing_code: object
                .get("pairingCode")
                .and_then(Value::as_str)
                .filter(|code| !code.is_empty())
                .ok_or("Recipe result needs connection or pairingCode.")?
                .to_owned(),
            project_root: object
                .get("projectRoot")
                .and_then(Value::as_str)
                .ok_or("Recipe result needs projectRoot.")?
                .to_owned(),
        },
    };
    if let Connection::IcarusServer { pairing_code, .. } = &connection
        && parse_pairing_code(pairing_code).is_none()
    {
        return Err("Recipe result pairingCode is not a valid Icarus pairing code (icarus://pair?address=…&code=…).".into());
    }
    if !connection.project_root().starts_with('/') {
        return Err("Recipe result projectRoot must be an absolute runtime path.".into());
    }
    let provisioned = schema == Some(2);
    if recipe.provisioned_root() != provisioned {
        return Err(if recipe.provisioned_root() {
            "Provisioned-root recipes must return schemaVersion 2 with checkoutMode \"provisioned-root\".".into()
        } else {
            "Recipe result requests provisioned-root checkout, but the recipe is not configured for it.".into()
        });
    }
    Ok((value, connection))
}

/// A running environment, kept so its suspend, resume and destroy commands get it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Environment {
    pub context: Context,
    pub result: Value,
    pub connection: Connection,
    /// The saved server and workspace the client opened on it, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub suspended: bool,
    pub created_at_ms: u64,
}

fn env_key(instance_id: &str) -> String {
    format!("recipe.environment.{instance_id}")
}

const ENV_PREFIX: &str = "recipe.environment.";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Orca's lifecycle stdin payload (`buildEphemeralVmRecipeLifecyclePayload`).
fn lifecycle_payload(mode: Mode, environment: &Environment) -> String {
    let payload = json!({
        "schemaVersion": 1,
        "mode": mode.name(),
        "recipeId": environment.context.recipe_id,
        "instanceId": environment.context.instance_id,
        "projectId": environment.context.project_id,
        "workspaceName": environment.context.workspace_name,
        "recipeResult": environment.result,
    });
    format!("{payload}\n")
}

fn failure(mode: Mode, run: &Run) -> RpcError {
    let mut message = format!(
        "{} exited with code {}.",
        match mode {
            Mode::Create => "Recipe",
            Mode::Suspend => "Suspend",
            Mode::Resume => "Resume",
            Mode::Destroy => "Destroy",
        },
        run.exit_code
            .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
    );
    let stderr = run.stderr.trim();
    if !stderr.is_empty() {
        let lines: Vec<&str> = stderr.lines().collect();
        let start = lines.len().saturating_sub(20);
        message = format!("{message}\n{}", lines[start..].join("\n"));
    }
    rpc("recipe_failed", message)
}

impl Server {
    fn recipe_project(
        &self,
        key: &str,
    ) -> Result<(crate::state::Project, crate::hooks::ProjectHooks), RpcError> {
        let project = self
            .store
            .project(key)
            .map_err(|e| rpc("internal", e))?
            .ok_or_else(|| rpc("not_found", format!("project {key}")))?;
        let hooks = self.hooks_for(&project.root, &project.root);
        Ok((project, hooks))
    }

    fn environment(&self, instance_id: &str) -> Result<Environment, RpcError> {
        self.store
            .kv_get(&env_key(instance_id))
            .map_err(|e| rpc("internal", e))?
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| rpc("not_found", format!("environment {instance_id}")))
    }

    fn store_environment(&self, environment: &Environment) -> Result<(), RpcError> {
        self.store
            .kv_set(
                &env_key(&environment.context.instance_id),
                &json!(environment),
            )
            .map_err(|e| rpc("internal", e))
    }

    fn environments(&self) -> Vec<Environment> {
        self.store
            .kv_prefix(ENV_PREFIX)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, value)| serde_json::from_value(value).ok())
            .collect()
    }

    fn recipe_of(
        &self,
        environment: &Environment,
    ) -> Result<(Recipe, std::path::PathBuf), RpcError> {
        let (project, hooks) = self.recipe_project(&environment.context.project_id)?;
        let recipe = hooks
            .recipes
            .into_iter()
            .find(|recipe| recipe.id == environment.context.recipe_id)
            .ok_or_else(|| {
                rpc(
                    "not_found",
                    format!(
                        "recipe {} is no longer in {}'s orca.yaml",
                        environment.context.recipe_id, project.name
                    ),
                )
            })?;
        Ok((recipe, project.root))
    }

    pub(crate) async fn recipes_dispatch(&self, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            "recipe.list" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                }
                let p: P = serde_json::from_value(p).map_err(|e| rpc("invalid_params", e))?;
                let (project, hooks) = self.recipe_project(&p.project)?;
                let content = trust_content(&hooks.recipes);
                let trusted = content.is_empty()
                    || self.is_trusted(&project.id, crate::hooks::ScriptKind::VmRecipe, &content);
                Ok(json!({
                    "project": project.id,
                    "recipes": hooks.recipes,
                    "problems": hooks.recipe_problems,
                    "trusted": trusted,
                }))
            }
            "recipe.create" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    recipe: String,
                    name: String,
                    branch: Option<String>,
                    #[serde(default)]
                    trust: bool,
                }
                let p: P = serde_json::from_value(p).map_err(|e| rpc("invalid_params", e))?;
                let (project, hooks) = self.recipe_project(&p.project)?;
                let content = trust_content(&hooks.recipes);
                if p.trust {
                    self.trust_hooks(&project.id, &hooks)?;
                } else if !self.is_trusted(
                    &project.id,
                    crate::hooks::ScriptKind::VmRecipe,
                    &content,
                ) {
                    return Err(rpc(
                        "untrusted_scripts",
                        crate::hooks::untrusted_message(&project.id, "environment recipes"),
                    ));
                }
                let recipe = hooks
                    .recipes
                    .iter()
                    .find(|recipe| recipe.id == p.recipe)
                    .cloned()
                    .ok_or_else(|| {
                        rpc(
                            "not_found",
                            format!("{} has no recipe {}", project.name, p.recipe),
                        )
                    })?;
                let context = Context {
                    instance_id: format!("icarus-{}", uuid::Uuid::new_v4()),
                    recipe_id: recipe.id.clone(),
                    project_id: project.id.clone(),
                    workspace_name: p.name,
                    repo_path: project.root.to_string_lossy().into_owned(),
                    repo_url: project.remote.clone().unwrap_or_default(),
                    branch: p.branch.unwrap_or_default(),
                };
                let schema = if recipe.provisioned_root() { 2 } else { 1 };
                let ran = run(
                    &recipe.create,
                    &project.root,
                    &recipe_env(Mode::Create, &context, schema),
                    None,
                )
                .await
                .map_err(|e| rpc("recipe_failed", e))?;
                if ran.exit_code != Some(0) {
                    return Err(failure(Mode::Create, &ran));
                }
                let (result, connection) =
                    parse_result(&recipe, &ran.stdout).map_err(|e| rpc("recipe_failed", e))?;
                let environment = Environment {
                    context,
                    result,
                    connection,
                    server: None,
                    workspace: None,
                    suspended: false,
                    created_at_ms: now_ms(),
                };
                self.store_environment(&environment)?;
                Ok(json!({ "environment": environment, "stderr": ran.stderr }))
            }
            "recipe.environments" => Ok(json!(self.environments())),
            "recipe.bind" => {
                #[derive(Deserialize)]
                struct P {
                    instance: String,
                    server: Option<String>,
                    workspace: Option<String>,
                }
                let p: P = serde_json::from_value(p).map_err(|e| rpc("invalid_params", e))?;
                let mut environment = self.environment(&p.instance)?;
                if p.server.is_some() {
                    environment.server = p.server;
                }
                if p.workspace.is_some() {
                    environment.workspace = p.workspace;
                }
                self.store_environment(&environment)?;
                Ok(json!(environment))
            }
            "recipe.suspend" | "recipe.resume" | "recipe.destroy" => {
                #[derive(Deserialize)]
                struct P {
                    instance: String,
                }
                let p: P = serde_json::from_value(p).map_err(|e| rpc("invalid_params", e))?;
                let mut environment = self.environment(&p.instance)?;
                let (recipe, root) = self.recipe_of(&environment)?;
                let (mode, command) = match method {
                    "recipe.suspend" => (Mode::Suspend, recipe.suspend.clone()),
                    "recipe.resume" => (Mode::Resume, recipe.resume.clone()),
                    _ => (
                        Mode::Destroy,
                        recipe.destroy.clone().filter(|_| !recipe.destroy_disabled),
                    ),
                };
                let schema = if recipe.provisioned_root() { 2 } else { 1 };
                let ran = match &command {
                    Some(command) => {
                        let ran = run(
                            command,
                            &root,
                            &recipe_env(mode, &environment.context, schema),
                            Some(lifecycle_payload(mode, &environment)),
                        )
                        .await
                        .map_err(|e| rpc("recipe_failed", e))?;
                        if ran.exit_code != Some(0) {
                            return Err(failure(mode, &ran));
                        }
                        Some(ran)
                    }
                    None => None,
                };
                match mode {
                    Mode::Destroy => {
                        self.store
                            .kv_delete(&env_key(&environment.context.instance_id))
                            .map_err(|e| rpc("internal", e))?;
                    }
                    Mode::Resume => {
                        if let Some(ran) = &ran {
                            let (result, connection) = parse_result(&recipe, &ran.stdout)
                                .map_err(|e| rpc("recipe_failed", e))?;
                            environment.result = result;
                            environment.connection = connection;
                        }
                        environment.suspended = false;
                        self.store_environment(&environment)?;
                    }
                    _ => {
                        environment.suspended = true;
                        self.store_environment(&environment)?;
                    }
                }
                Ok(json!({
                    "environment": environment,
                    "skipped": command.is_none(),
                    "stderr": ran.map(|ran| ran.stderr),
                }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipes(yaml: &str) -> (Vec<Recipe>, Vec<RecipeProblem>) {
        let docs = yaml_rust2::YamlLoader::load_from_str(yaml).expect("yaml");
        parse(&docs[0]["environmentRecipes"])
    }

    #[test]
    fn recipes_parse_with_orcas_diagnostics() {
        let (parsed, problems) = recipes(
            "environmentRecipes:\n  - id: sandbox\n    name: Sandbox\n    create: ./create.sh\n    destroy: ./destroy.sh\n  - id: Bad\n    name: x\n    create: y\n  - id: sandbox\n    name: Again\n    create: z\n  - id: nameless\n    create: z\n  - id: keep\n    name: Keep\n    command: ./c.sh\n    cleanup: none\n    checkoutMode: provisioned-root\n  - just text\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].destroy.as_deref(), Some("./destroy.sh"));
        assert_eq!(parsed[1].create, "./c.sh", "command is create's old name");
        assert!(parsed[1].destroy_disabled && parsed[1].destroy.is_none());
        let messages: Vec<_> = problems
            .iter()
            .map(|p| (p.index, p.message.as_str()))
            .collect();
        assert_eq!(messages.len(), 4);
        assert!(messages[0].1.starts_with("Invalid recipe id \"Bad\""));
        assert_eq!(
            messages[1].1,
            "Duplicate recipe id \"sandbox\". Recipe ids must be unique."
        );
        assert_eq!(messages[2].1, "Recipe \"nameless\" is missing name.");
        assert_eq!(messages[3], (5, "Recipe entry must be a mapping."));
        assert_eq!(
            trust_content(&parsed[..1]),
            "# environmentRecipes.sandbox\nname: Sandbox\ncreate: ./create.sh\ndestroy: ./destroy.sh"
        );
    }

    #[test]
    fn results_need_a_pairing_or_ssh_connection_and_matching_checkout_mode() {
        let (parsed, _) = recipes(
            "environmentRecipes:\n  - id: a\n    name: A\n    create: c\n  - id: b\n    name: B\n    create: c\n    checkoutMode: provisioned-root\n",
        );
        let plain = &parsed[0];
        let (_, connection) = parse_result(
            plain,
            r#"{"schemaVersion":1,"pairingCode":"icarus://pair?address=10.0.0.2:7878&code=ABCD-EFGH","projectRoot":"/srv/repo"}"#,
        )
        .expect("legacy result");
        assert_eq!(connection.project_root(), "/srv/repo");
        let (_, connection) = parse_result(
            plain,
            r#"{"schemaVersion":1,"connection":{"type":"ssh","target":{"label":"vm","host":"10.0.0.3","port":22,"username":"dev"},"projectRoot":"/home/dev/repo"}}"#,
        )
        .expect("ssh result");
        assert!(matches!(connection, Connection::Ssh { .. }));
        for (stdout, wanted) in [
            ("", "Recipe produced no JSON result."),
            ("not json", "Recipe stdout must be one JSON object."),
            (
                r#"{"schemaVersion":1,"pairingCode":"nope","projectRoot":"/r"}"#,
                "not a valid Icarus pairing code",
            ),
            (
                r#"{"schemaVersion":1,"pairingCode":"icarus://pair?address=h:1&code=C","projectRoot":"rel"}"#,
                "absolute runtime path",
            ),
            (
                r#"{"schemaVersion":2,"checkoutMode":"provisioned-root","pairingCode":"icarus://pair?address=h:1&code=C","projectRoot":"/r"}"#,
                "not configured for it",
            ),
        ] {
            let error = parse_result(plain, stdout).expect_err(stdout);
            assert!(error.contains(wanted), "{stdout}: {error}");
        }
        assert!(
            parse_result(
                &parsed[1],
                r#"{"schemaVersion":1,"pairingCode":"icarus://pair?address=h:1&code=C","projectRoot":"/r"}"#
            )
            .is_err()
        );
        assert!(
            parse_result(
                &parsed[1],
                r#"{"schemaVersion":2,"checkoutMode":"provisioned-root","pairingCode":"icarus://pair?address=h:1&code=C","projectRoot":"/r"}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn pairing_codes_carry_the_address_code_and_certificate() {
        assert_eq!(
            parse_pairing_code(
                "icarus://pair?address=127.0.0.1%3A7878&code=AB12-CD34&fingerprint=ff00"
            ),
            Some(PairingOffer {
                address: "127.0.0.1:7878".into(),
                code: "AB12-CD34".into(),
                fingerprint: Some("ff00".into()),
            })
        );
        assert_eq!(parse_pairing_code("orca://pair?code=x"), None);
        assert_eq!(parse_pairing_code("icarus://pair?code=x"), None);
    }

    #[tokio::test]
    async fn lifecycle_commands_get_orcas_env_and_the_instance_on_stdin() {
        let dir = tempfile::tempdir().expect("dir");
        let context = Context {
            instance_id: "icarus-1".into(),
            recipe_id: "sandbox".into(),
            project_id: "p1".into(),
            workspace_name: "feat".into(),
            repo_path: dir.path().to_string_lossy().into_owned(),
            ..Context::default()
        };
        let ran = run(
            "printf '%s %s %s ' \"$ORCA_VM_MODE\" \"$ICARUS_VM_INSTANCE_ID\" \"$ORCA_RECIPE_RESULT_SCHEMA_VERSION\"; cat; echo oops >&2; exit 3",
            dir.path(),
            &recipe_env(Mode::Destroy, &context, 1),
            Some("{\"a\":1}\n".into()),
        )
        .await
        .expect("runs");
        assert_eq!(ran.stdout, "destroy icarus-1 1 {\"a\":1}\n");
        assert_eq!(ran.stderr, "oops\n");
        assert_eq!(ran.exit_code, Some(3));
        assert_eq!(
            failure(Mode::Destroy, &ran).message,
            "Destroy exited with code 3.\noops"
        );
    }
}
