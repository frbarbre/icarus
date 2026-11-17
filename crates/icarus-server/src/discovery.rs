//! Worktree discovery, as Orca does it: every linked worktree of a project's repository becomes a
//! workspace, tagged with where it came from and shown or hidden by the visibility settings.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use icarus_protocol::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::{Server, params, rpc};
use crate::state::{Discovery, ORIGIN_ADOPTED, ORIGIN_NATIVE, Project, Workspace, workspace_slug};

pub const ORIGIN_EXTERNAL: &str = "external";
/// The repository's main checkout, listed as Orca does with a "primary" badge; it can't be removed.
pub const ORIGIN_PRIMARY: &str = "primary";
const BUILT_IN_SOURCES: [(&str, &[&str]); 2] = [
    ("claude", &[".claude", "worktrees"]),
    ("gsd", &[".gsd-workspaces"]),
];
const MAX_CUSTOM_SOURCES: usize = 32;
const PREPARATION_LOCK_PREFIX: &str = "orca-create-preparation:";
const SLUG_MAX: usize = 63;
/// `workspace.list` rescans a project at most this often; `refresh` forces it.
const LIST_RESCAN: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_secs(15);
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// A scan that takes longer is abandoned; the project keeps its last known state.
const SCAN_DEADLINE: Duration = Duration::from_secs(12);
/// After a failed or abandoned scan, timer and list scans leave the project alone this long.
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// A scan slower than this logs where its time went.
const SLOW_SCAN: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct DiscoveryState {
    projects: Mutex<HashMap<String, ProjectScan>>,
    // Every scan resolves every workspace path of every project; each resolution touches the disk.
    canonical: Mutex<HashMap<PathBuf, PathBuf>>,
    // Applying results one project at a time keeps two scans from picking the same name.
    gate: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct ProjectScan {
    last: Option<Instant>,
    running: bool,
    backoff_until: Option<Instant>,
    policy: Option<Policy>,
}

impl DiscoveryState {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProjectScan>> {
        self.projects.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Claims the project for one scan; false while one runs, and (unless forced) when fresh or backing off.
    fn begin(&self, project_id: &str, force: bool) -> bool {
        let mut projects = self.lock();
        let scan = projects.entry(project_id.to_owned()).or_default();
        let now = Instant::now();
        let waiting = scan
            .last
            .is_some_and(|last| now.duration_since(last) < LIST_RESCAN)
            || scan.backoff_until.is_some_and(|until| now < until);
        if scan.running || (!force && waiting) {
            return false;
        }
        scan.running = true;
        scan.last = Some(now);
        true
    }

    fn finish(&self, project_id: &str) {
        if let Some(scan) = self.lock().get_mut(project_id) {
            scan.running = false;
        }
    }

    fn back_off(&self, project_id: &str) {
        if let Some(scan) = self.lock().get_mut(project_id) {
            scan.backoff_until = Some(Instant::now() + FAILURE_BACKOFF);
        }
    }

    fn succeeded(&self, project_id: &str, policy: Policy) {
        if let Some(scan) = self.lock().get_mut(project_id) {
            scan.backoff_until = None;
            scan.policy = Some(policy);
        }
    }

    /// A path's canonical form, resolved once; a path that doesn't resolve yet is tried again next time.
    fn canonical(&self, path: &Path) -> PathBuf {
        let known = || self.canonical.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(found) = known().get(path) {
            return found.clone();
        }
        match std::fs::canonicalize(path) {
            Ok(found) => {
                known().insert(path.to_path_buf(), found.clone());
                found
            }
            Err(_) => path.to_path_buf(),
        }
    }

    /// The policy of the project's last scan; reading settings in the project's folder can block.
    fn policy(&self, project_id: &str) -> Option<Policy> {
        self.lock().get(project_id)?.policy.clone()
    }
}

/// Releases the claim when the scan's blocking work ends, even after its caller stopped waiting.
struct Claim {
    server: Arc<Server>,
    project_id: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.server.discovery.finish(&self.project_id);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    rpc("internal", error)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn strictly_inside(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

fn expand_home(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

fn valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn lookup<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

fn shown(value: Option<&Value>) -> Option<bool> {
    match value.and_then(Value::as_str) {
        Some("show") => Some(true),
        Some("hide") => Some(false),
        _ => None,
    }
}

/// A project's resolved visibility settings.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    workspace_root: PathBuf,
    project_root: PathBuf,
    pub external_shown: bool,
    sources: Vec<(String, PathBuf)>,
    source_defaults: HashMap<String, bool>,
    project_built_in: HashMap<String, bool>,
    project_custom: HashMap<String, bool>,
    project_agent: Option<bool>,
}

impl Policy {
    /// From settings resolved for the project (`workspace.*` defaults, `project.*` overrides).
    pub(crate) fn new(settings: &Value, project_root: &Path, workspace_root: &Path) -> Self {
        let external_shown = shown(lookup(settings, &["project", "externalWorktreeVisibility"]))
            .or_else(|| shown(lookup(settings, &["workspace", "external"])))
            .unwrap_or(false);
        let mut ids = HashSet::new();
        let mut roots = HashSet::new();
        let sources = lookup(settings, &["workspace", "worktreeSources"])
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|source| {
                let id = source.get("id")?.as_str()?;
                let root = expand_home(source.get("root")?.as_str()?)?;
                let root = canonical(&root);
                (valid_source_id(id) && ids.insert(id.to_owned()) && roots.insert(root.clone()))
                    .then(|| (id.to_owned(), root))
            })
            .take(MAX_CUSTOM_SOURCES)
            .collect();
        let visibility_map = |value: Option<&Value>| -> HashMap<String, bool> {
            value
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(id, value)| Some((id.clone(), shown(Some(value))?)))
                .collect()
        };
        let preferences = lookup(
            settings,
            &["project", "worktreeVisibilitySourcePreferences"],
        );
        Self {
            workspace_root: canonical(workspace_root),
            project_root: canonical(project_root),
            external_shown,
            sources,
            source_defaults: visibility_map(lookup(settings, &["workspace", "sourceVisibility"])),
            project_built_in: visibility_map(preferences.and_then(|p| p.get("builtIn"))),
            project_custom: visibility_map(preferences.and_then(|p| p.get("custom"))),
            project_agent: shown(lookup(settings, &["project", "agentWorktreeVisibility"])),
        }
    }

    pub(crate) fn under_root(&self, canonical_path: &Path) -> bool {
        strictly_inside(canonical_path, &self.workspace_root)
    }

    /// The origin of a worktree found by listing: native, a built-in or custom source, or external.
    pub(crate) fn origin(&self, canonical_path: &Path) -> String {
        if self.under_root(canonical_path) {
            return ORIGIN_NATIVE.into();
        }
        for (id, segments) in BUILT_IN_SOURCES {
            let root = segments
                .iter()
                .fold(self.project_root.clone(), |root, segment| {
                    root.join(segment)
                });
            if strictly_inside(canonical_path, &root) {
                return id.into();
            }
        }
        for (id, root) in &self.sources {
            if strictly_inside(canonical_path, root) {
                return format!("custom:{id}");
            }
        }
        ORIGIN_EXTERNAL.into()
    }

    /// Orca's precedence: a project's preference, then (built-ins) its agent policy, then the default.
    pub(crate) fn source_shown(&self, origin: &str) -> bool {
        match origin {
            ORIGIN_NATIVE | ORIGIN_ADOPTED | ORIGIN_PRIMARY => true,
            ORIGIN_EXTERNAL => self.external_shown,
            "claude" | "gsd" => self
                .project_built_in
                .get(origin)
                .copied()
                .or(self.project_agent)
                .or_else(|| self.source_defaults.get(origin).copied())
                .unwrap_or(false),
            other => {
                let id = other.strip_prefix("custom:").unwrap_or(other);
                self.project_custom
                    .get(id)
                    .or_else(|| self.source_defaults.get(id))
                    .copied()
                    .unwrap_or(false)
            }
        }
    }

    pub(crate) fn hidden(&self, discovery: &Discovery) -> bool {
        match discovery.visibility.as_deref() {
            Some("hide") => true,
            Some("show") => false,
            _ => discovery.discovered && !self.source_shown(&discovery.origin),
        }
    }

    /// Orca's new-worktrees inbox: hidden external (or custom-source) worktrees nobody decided on,
    /// offered only while the project hides external worktrees.
    pub(crate) fn in_inbox(&self, discovery: &Discovery) -> bool {
        !self.external_shown
            && discovery.discovered
            && discovery.hidden
            && discovery.visibility.is_none()
            && !discovery.inbox_seen
            && discovery.missing_at_ms.is_none()
            && (discovery.origin == ORIGIN_EXTERNAL || discovery.origin.starts_with("custom:"))
    }
}

/// How a worktree's source reads in a list: external ones by their parent folder, as Orca groups them.
pub(crate) fn source_label(workspace: &Workspace) -> String {
    let origin = workspace.discovery.origin.as_str();
    match origin {
        ORIGIN_NATIVE => "Icarus".into(),
        ORIGIN_ADOPTED => "Adopted".into(),
        ORIGIN_PRIMARY => "Primary".into(),
        "claude" => "Claude Code".into(),
        "gsd" => "GSD".into(),
        ORIGIN_EXTERNAL => workspace
            .path
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_else(|| "Unknown location".into()),
        other => other.strip_prefix("custom:").unwrap_or(other).to_owned(),
    }
}

struct Listed {
    path: PathBuf,
    canonical: PathBuf,
    branch: Option<String>,
    main: bool,
}

impl Listed {
    fn origin(&self, policy: &Policy) -> String {
        if self.main {
            ORIGIN_PRIMARY.into()
        } else {
            policy.origin(&self.canonical)
        }
    }
}

/// The project's worktrees, its main checkout included, or None when git could not say (nothing
/// is marked missing then).
fn list_linked(root: &Path, paths: &DiscoveryState) -> Option<Vec<Listed>> {
    let worktrees = match icarus_git::list_worktrees_timeout(root, GIT_TIMEOUT) {
        Ok(worktrees) => worktrees,
        Err(error) => {
            tracing::warn!(
                "discovery: listing worktrees of {}: {error:#}",
                root.display()
            );
            return None;
        }
    };
    Some(
        worktrees
            .into_iter()
            .filter(|w| !w.bare && !w.prunable)
            // Orca's half-made worktrees carry this lock until they are ready.
            .filter(|w| {
                !w.locked
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with(PREPARATION_LOCK_PREFIX))
            })
            .map(|w| Listed {
                canonical: paths.canonical(&w.path),
                branch: if w.detached { None } else { w.branch },
                path: w.path,
                main: w.main,
            })
            .collect(),
    )
}

fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    let base = if base.is_empty() { "worktree" } else { base };
    if !taken.contains(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let stem: String = base
                .chars()
                .take(SLUG_MAX.saturating_sub(suffix.len()))
                .collect();
            format!("{stem}{suffix}")
        })
        .find(|name| !taken.contains(name))
        .unwrap_or_default()
}

struct Scanned {
    policy: Policy,
    listed: Option<Vec<Listed>>,
    /// Every row with its canonical path and whether its folder exists.
    rows: Vec<(Workspace, PathBuf, bool)>,
}

/// Everything that touches the disk or runs git, so it can run off the async threads under a deadline.
fn scan(server: &Server, project: &Project) -> anyhow::Result<Scanned> {
    let started = Instant::now();
    let policy = Policy::new(
        &server.resolved_settings(Some(&project.root)),
        &project.root,
        &server.workspace_root,
    );
    let settings_done = started.elapsed();
    // Rows are read before git lists, so a workspace created meanwhile is never marked missing.
    let everything = server.store.all_workspaces(None)?;
    let rows_done = started.elapsed();
    let listed = list_linked(&project.root, &server.discovery);
    let git_done = started.elapsed();
    let rows = everything
        .into_iter()
        .map(|w| {
            let canonical = server.discovery.canonical(&w.path);
            // Only an adopted worktree git didn't list needs its folder checked.
            let exists = w.project_id == project.id
                && w.discovery.origin == ORIGIN_ADOPTED
                && w.path.is_dir();
            (w, canonical, exists)
        })
        .collect();
    let elapsed = started.elapsed();
    if elapsed > SLOW_SCAN {
        tracing::info!(
            "discovery: scanning {} took {elapsed:.1?} (settings {settings_done:.1?}, \
             stored rows {:.1?}, git worktree list {:.1?}, paths {:.1?})",
            project.root.display(),
            rows_done - settings_done,
            git_done - rows_done,
            elapsed - git_done,
        );
    }
    Ok(Scanned {
        policy,
        listed,
        rows,
    })
}

impl Server {
    /// Scans one project; returns the ids of workspaces that appeared, changed or dropped out.
    /// A scan already running, git timing out or a scan past its deadline leaves everything as it was.
    pub(crate) async fn discover_project(
        self: &Arc<Self>,
        project: &Project,
        force: bool,
    ) -> Result<Vec<String>, RpcError> {
        let waiting_since = Instant::now();
        while !self.discovery.begin(&project.id, force) {
            // A forced scan waits for one in flight, so it sees the latest settings and worktrees.
            if !force || waiting_since.elapsed() > SCAN_DEADLINE {
                return Ok(Vec::new());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let claim = Claim {
            server: Arc::clone(self),
            project_id: project.id.clone(),
        };
        let scanning = {
            let project = project.clone();
            tokio::task::spawn_blocking(move || {
                let scanned = scan(&claim.server, &project);
                drop(claim);
                scanned
            })
        };
        let scanned = match tokio::time::timeout(SCAN_DEADLINE, scanning).await {
            Ok(joined) => joined.map_err(internal)?.map_err(internal)?,
            Err(_) => {
                tracing::warn!(
                    "discovery: scanning {} took over {}s; keeping its workspaces as they were",
                    project.root.display(),
                    SCAN_DEADLINE.as_secs()
                );
                self.discovery.back_off(&project.id);
                return Ok(Vec::new());
            }
        };
        let Scanned {
            policy,
            listed,
            rows,
        } = scanned;
        let Some(listed) = listed else {
            self.discovery.back_off(&project.id);
            return Ok(Vec::new());
        };
        self.discovery.succeeded(&project.id, policy.clone());
        let _gate = self.discovery.gate.lock().await;
        let now = now_ms();
        let mut changed = Vec::new();
        let mut matched: HashSet<PathBuf> = HashSet::new();
        let mut taken: HashSet<String> = HashSet::new();
        for (workspace, canonical_path, exists) in &rows {
            if workspace.project_id != project.id {
                matched.insert(canonical_path.clone());
                continue;
            }
            taken.insert(workspace.name.clone());
            let found = listed.iter().find(|l| &l.canonical == canonical_path);
            let mut discovery = workspace.discovery.clone();
            let mut branch = workspace.branch.clone();
            if !discovery.discovered
                && discovery.origin == ORIGIN_NATIVE
                && !policy.under_root(canonical_path)
            {
                // Adopted before origins were stored.
                discovery.origin = ORIGIN_ADOPTED.into();
            }
            match found {
                Some(listed) => {
                    matched.insert(canonical_path.clone());
                    discovery.missing_at_ms = None;
                    if discovery.discovered {
                        discovery.origin = listed.origin(&policy);
                        branch = listed.branch.clone();
                    }
                }
                None if discovery.missing_at_ms.is_none() => {
                    // An adopted worktree may belong to another clone, which this listing can't see.
                    if discovery.origin != ORIGIN_ADOPTED || !exists {
                        discovery.missing_at_ms = Some(now);
                    }
                }
                None => {}
            }
            discovery.hidden = policy.hidden(&discovery);
            if discovery != workspace.discovery || branch != workspace.branch {
                self.store
                    .update_discovery(&workspace.id, branch.as_deref(), &discovery)
                    .map_err(internal)?;
                changed.push(workspace.id.clone());
            }
        }
        for listed in listed.iter().filter(|l| !matched.contains(&l.canonical)) {
            let folder = listed
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = unique_name(&workspace_slug(&folder, SLUG_MAX), &taken);
            let mut discovery = Discovery {
                origin: listed.origin(&policy),
                discovered: true,
                ..Discovery::default()
            };
            discovery.hidden = policy.hidden(&discovery);
            let workspace = Workspace {
                id: format!("ws-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
                project_id: project.id.clone(),
                name: name.clone(),
                path: listed.path.clone(),
                branch: listed.branch.clone(),
                base_ref: project.base_ref.clone(),
                status: None,
                title: None,
                issue: None,
                pull_request: None,
                created_at_ms: now,
                pinned: false,
                manual_order: Some(now as i64),
                activity_at_ms: None,
                discovery,
            };
            if self
                .store
                .insert_discovered_workspace(&workspace)
                .map_err(internal)?
            {
                taken.insert(name);
                changed.push(workspace.id);
            }
        }
        Ok(changed)
    }

    /// Scans the given projects (all when None) side by side and tells clients what changed.
    pub(crate) async fn discover(
        self: &Arc<Self>,
        project_id: Option<&str>,
        force: bool,
    ) -> Vec<String> {
        let projects = match self.store.projects() {
            Ok(projects) => projects,
            Err(error) => {
                tracing::warn!("discovery: {error:#}");
                return Vec::new();
            }
        };
        let scans: Vec<_> = projects
            .into_iter()
            .filter(|p| project_id.is_none_or(|id| id == p.id))
            .map(|project| {
                let server = Arc::clone(self);
                tokio::spawn(async move {
                    let result = server.discover_project(&project, force).await;
                    (project, result)
                })
            })
            .collect();
        let mut changed = Vec::new();
        for scan in scans {
            match scan.await {
                Ok((_, Ok(ids))) => changed.extend(ids),
                Ok((project, Err(error))) => {
                    tracing::warn!("discovery in {}: {}", project.name, error.message)
                }
                Err(error) => tracing::warn!("discovery: {error}"),
            }
        }
        if !changed.is_empty() {
            self.broadcast(
                "workspace.changed",
                json!({ "workspaces": changed, "source": "discovery" }),
            );
            self.github.wake_poll();
            self.linear.wake_poll();
        }
        changed
    }

    fn inbox_json(&self, project: &Project) -> Result<Option<Value>, RpcError> {
        let Some(policy) = self.discovery.policy(&project.id) else {
            return Ok(None);
        };
        let workspaces: Vec<Value> = self
            .store
            .known_workspaces(Some(&project.id))
            .map_err(internal)?
            .iter()
            .filter(|w| policy.in_inbox(&w.discovery))
            .map(|w| {
                json!({
                    "id": w.id,
                    "name": w.name,
                    "path": w.path,
                    "branch": w.branch,
                    "origin": w.discovery.origin,
                    "source_label": source_label(w),
                })
            })
            .collect();
        Ok((!workspaces.is_empty()).then(|| {
            json!({ "project": project.id, "project_name": project.name, "workspaces": workspaces })
        }))
    }

    pub(crate) async fn discovery_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct Scope {
            project: Option<String>,
        }
        let project_of = |key: &str| -> Result<Project, RpcError> {
            self.store
                .project(key)
                .map_err(internal)?
                .ok_or_else(|| rpc("not_found", format!("project {key}")))
        };
        match method {
            "workspace.discover" => {
                let scope: Scope = params(p)?;
                let project = scope.project.as_deref().map(project_of).transpose()?;
                let changed = self
                    .discover(project.as_ref().map(|p| p.id.as_str()), true)
                    .await;
                Ok(json!({ "changed": changed }))
            }
            "workspace.inbox" => {
                let scope: Scope = params(p)?;
                let projects = match scope.project.as_deref() {
                    Some(key) => vec![project_of(key)?],
                    None => self.store.projects().map_err(internal)?,
                };
                let mut inbox = Vec::new();
                for project in &projects {
                    if let Some(entry) = self.inbox_json(project)? {
                        inbox.push(entry);
                    }
                }
                Ok(json!(inbox))
            }
            "workspace.visibility" => {
                #[derive(Deserialize)]
                struct P {
                    id: String,
                    /// `show`, `hide`, or null to follow the settings again.
                    visibility: Option<String>,
                }
                let p: P = params(p)?;
                if p.visibility
                    .as_deref()
                    .is_some_and(|v| v != "show" && v != "hide")
                {
                    return Err(rpc("invalid_params", "visibility is show, hide or null"));
                }
                let workspace = self.workspace_by_key(&p.id)?;
                let mut discovery = workspace.discovery.clone();
                discovery.visibility = p.visibility.clone();
                let hidden = match (
                    p.visibility.as_deref(),
                    self.discovery.policy(&workspace.project_id),
                ) {
                    (Some(visibility), _) => visibility == "hide",
                    (None, Some(policy)) => policy.hidden(&discovery),
                    // No scan yet: the next one settles it.
                    (None, None) => workspace.discovery.hidden,
                };
                self.store
                    .set_visibility(&workspace.id, p.visibility.as_deref(), hidden)
                    .map_err(internal)?;
                self.broadcast(
                    "workspace.changed",
                    json!({ "workspaces": [workspace.id], "source": "visibility" }),
                );
                Ok(json!({ "id": workspace.id, "hidden": hidden, "visibility": p.visibility }))
            }
            "workspace.show_source" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    /// A workspace origin: external, claude, gsd or custom:<id>.
                    source: String,
                }
                let p: P = params(p)?;
                let project = project_of(&p.project)?;
                let key = match p.source.as_str() {
                    ORIGIN_EXTERNAL => "project.externalWorktreeVisibility".to_owned(),
                    "claude" | "gsd" => format!(
                        "project.worktreeVisibilitySourcePreferences.builtIn.{}",
                        p.source
                    ),
                    other => match other
                        .strip_prefix("custom:")
                        .filter(|id| valid_source_id(id))
                    {
                        Some(id) => {
                            format!("project.worktreeVisibilitySourcePreferences.custom.{id}")
                        }
                        None => {
                            return Err(rpc(
                                "invalid_params",
                                format!("{other} is not a worktree source"),
                            ));
                        }
                    },
                };
                // Personal like the imported project settings, so it goes to the untracked local file.
                let written =
                    self.write_key(&key, Some(json!("show")), Some("local"), Some(&project.id))?;
                let changed = self.discover(Some(&project.id), true).await;
                Ok(json!({ "key": key, "written": written, "changed": changed }))
            }
            "workspace.inbox_dismiss" => {
                #[derive(Deserialize)]
                struct P {
                    project: String,
                    /// Only these; all of the project's inbox when absent.
                    ids: Option<Vec<String>>,
                }
                let p: P = params(p)?;
                let project = project_of(&p.project)?;
                let Some(policy) = self.discovery.policy(&project.id) else {
                    return Ok(json!({ "dismissed": 0 }));
                };
                let ids: Vec<String> = self
                    .store
                    .known_workspaces(Some(&project.id))
                    .map_err(internal)?
                    .into_iter()
                    .filter(|w| policy.in_inbox(&w.discovery))
                    .filter(|w| {
                        p.ids
                            .as_ref()
                            .is_none_or(|ids| ids.iter().any(|id| id == &w.id || id == &w.name))
                    })
                    .map(|w| w.id)
                    .collect();
                let seen = self.store.mark_inbox_seen(&ids).map_err(internal)?;
                if seen > 0 {
                    self.broadcast(
                        "workspace.changed",
                        json!({ "workspaces": ids, "source": "inbox" }),
                    );
                }
                Ok(json!({ "dismissed": seen }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }
}

/// Rescans every project while a client is connected, so worktrees made elsewhere show up on their own.
pub(crate) fn spawn(server: Arc<Server>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if server.client_count() == 0 {
                continue;
            }
            server.discover(None, false).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(settings: Value) -> Policy {
        Policy::new(&settings, Path::new("/src/widgets"), Path::new("/ws"))
    }

    fn discovered(origin: &str) -> Discovery {
        Discovery {
            origin: origin.into(),
            discovered: true,
            ..Discovery::default()
        }
    }

    #[test]
    fn origins_follow_orca_sources() {
        let policy = policy(json!({
            "workspace": { "worktreeSources": [
                { "id": "conductor", "root": "/conductor" },
                { "id": "bad id", "root": "/x" },
                { "id": "relative", "root": "x" }
            ] }
        }));
        let origin = |path: &str| policy.origin(Path::new(path));
        assert_eq!(origin("/ws/widgets/a"), "native");
        assert_eq!(origin("/src/widgets/.claude/worktrees/a"), "claude");
        assert_eq!(origin("/src/widgets/.claude/worktrees"), "external");
        assert_eq!(origin("/src/widgets/.gsd-workspaces/b"), "gsd");
        assert_eq!(origin("/other/.claude/worktrees/a"), "external");
        assert_eq!(origin("/conductor/widgets/c"), "custom:conductor");
        assert_eq!(origin("/x/d"), "external");
        assert_eq!(origin("/elsewhere/e"), "external");
    }

    #[test]
    fn visibility_precedence_matches_orca() {
        let defaults = policy(json!({ "workspace": { "external": "hide" } }));
        assert!(defaults.hidden(&discovered("external")));
        assert!(
            defaults.hidden(&discovered("claude")),
            "sources hide by default"
        );
        assert!(!defaults.hidden(&discovered("native")));
        assert!(!defaults.hidden(&Discovery::default()), "created by Icarus");
        let mut chosen = discovered("external");
        chosen.visibility = Some("show".into());
        assert!(!defaults.hidden(&chosen));

        let project = policy(json!({
            "workspace": { "external": "hide", "sourceVisibility": { "claude": "show", "conductor": "show" } },
            "project": {
                "externalWorktreeVisibility": "show",
                "agentWorktreeVisibility": "hide",
                "worktreeVisibilitySourcePreferences": { "builtIn": { "gsd": "show" } }
            }
        }));
        assert!(!project.hidden(&discovered("external")), "the project wins");
        assert!(
            project.hidden(&discovered("claude")),
            "agent policy beats the default"
        );
        assert!(
            !project.hidden(&discovered("gsd")),
            "a source preference beats both"
        );
        assert!(!project.hidden(&discovered("custom:conductor")));
        let mut hidden = discovered("external");
        hidden.visibility = Some("hide".into());
        assert!(project.hidden(&hidden));
    }

    #[test]
    fn the_inbox_holds_undecided_hidden_external_worktrees_while_external_is_hidden() {
        let policy = policy(json!({ "workspace": { "external": "hide" } }));
        let mut external = discovered("external");
        external.hidden = policy.hidden(&external);
        assert!(policy.in_inbox(&external));
        let mut custom = discovered("custom:conductor");
        custom.hidden = true;
        assert!(policy.in_inbox(&custom));
        let mut claude = discovered("claude");
        claude.hidden = true;
        assert!(
            !policy.in_inbox(&claude),
            "agent scratch stays out, as in Orca"
        );
        let mut seen = external.clone();
        seen.inbox_seen = true;
        assert!(!policy.in_inbox(&seen));
        let showing = Policy::new(
            &json!({ "workspace": { "external": "show" } }),
            Path::new("/src/widgets"),
            Path::new("/ws"),
        );
        assert!(
            !showing.in_inbox(&custom),
            "no inbox while external worktrees show"
        );
    }

    #[test]
    fn names_stay_unique_within_a_project() {
        let taken: HashSet<String> = ["fix".to_owned(), "fix-2".to_owned()].into();
        assert_eq!(unique_name("fix", &taken), "fix-3");
        assert_eq!(unique_name("other", &taken), "other");
    }
}
