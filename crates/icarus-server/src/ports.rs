mod classify;
mod labels;
mod probe;
mod scan;
mod urls;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use icarus_protocol::RpcError;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::server::{Server, params, rpc, to_value};
use crate::state::Workspace;
use crate::terminal::{TerminalEvent, TerminalInfo};
use classify::{Classification, Confidence, Kind};
use labels::LabelEntry;
use scan::{Attribution, Listener, Owner, Snapshot};
use urls::{LineBuffer, ParsedUrl};

const SNAPSHOT_TTL: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const LABELS_TTL: Duration = Duration::from_secs(300);
const LABELS_TIMEOUT: Duration = Duration::from_secs(15);
const URLS_PER_TERMINAL: usize = 32;

#[derive(Default)]
pub struct Ports {
    snapshot: tokio::sync::Mutex<Option<(Instant, Arc<Snapshot>)>>,
    probes: Mutex<HashMap<(u32, u16), Option<Classification>>>,
    labels: Mutex<HashMap<String, LabelsCache>>,
    watched: Mutex<HashSet<String>>,
    advertised: Arc<Mutex<HashMap<String, Vec<ParsedUrl>>>>,
    fingerprints: Mutex<HashMap<String, String>>,
}

struct LabelsCache {
    key: String,
    at: Instant,
    entries: Vec<LabelEntry>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Scan,
    Advertised,
    LabelsCommand,
}

#[derive(Debug, Clone, Serialize)]
pub struct Service {
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub command: Option<String>,
    pub hosts: Vec<String>,
    pub url: String,
    pub urls: Vec<String>,
    pub label: Option<String>,
    pub kind: Kind,
    pub framework: Option<String>,
    pub confidence: Confidence,
    pub primary: bool,
    pub source: Source,
    pub attribution: Option<Attribution>,
    pub terminal: Option<String>,
    #[serde(skip)]
    rule_primary: bool,
    #[serde(skip)]
    label_url: Option<String>,
    #[serde(skip)]
    advertised: Vec<ParsedUrl>,
    #[serde(skip)]
    probed: bool,
}

impl Service {
    fn new(listener: &Listener, snapshot: &Snapshot, attribution: Option<Attribution>) -> Self {
        let command = listener
            .pid
            .and_then(|pid| snapshot.processes.get(&pid))
            .map(|p| p.command.clone())
            .filter(|c| !c.is_empty());
        let found = classify::classify_command(
            command.as_deref().unwrap_or_default(),
            listener.process.as_deref().unwrap_or_default(),
            listener.port,
        );
        Self {
            port: listener.port,
            pid: listener.pid,
            process: listener.process.clone(),
            command,
            hosts: listener.hosts.clone(),
            url: String::new(),
            urls: Vec::new(),
            label: None,
            kind: found.kind,
            framework: found.framework,
            confidence: found.confidence,
            primary: false,
            source: Source::Scan,
            attribution,
            terminal: None,
            rule_primary: false,
            label_url: None,
            advertised: Vec::new(),
            probed: false,
        }
    }

    fn finish_urls(&mut self) {
        let fallback = format!("http://localhost:{}", self.port);
        let mut advertised: Vec<&ParsedUrl> = self.advertised.iter().collect();
        advertised.sort_by_key(|u| Reverse((u.host_score(), u.https)));
        let best_advertised = advertised
            .iter()
            .find(|u| !u.is_unspecified())
            .map(|u| u.url.clone());
        self.url = self
            .label_url
            .clone()
            .or(best_advertised)
            .unwrap_or_else(|| fallback.clone());
        let mut all = vec![self.url.clone()];
        all.extend(self.label_url.clone());
        all.extend(advertised.iter().map(|u| u.url.clone()));
        all.push(fallback);
        let mut seen = HashSet::new();
        all.retain(|u| seen.insert(u.trim_end_matches('/').to_string()));
        self.urls = all;
    }
}

#[derive(Debug)]
struct Rule {
    port: Option<u16>,
    command: Option<Regex>,
    label_match: Option<Regex>,
    label: Option<String>,
    kind: Option<Kind>,
    primary: bool,
}

struct PortSettings {
    labels_command: Option<String>,
    rules: Vec<Rule>,
    probe: bool,
}

fn port_settings(resolved: &Value) -> PortSettings {
    let ports = &resolved["ports"];
    let non_empty = |v: &Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let labels_command = non_empty(&ports["labels_command"])
        .or_else(|| non_empty(&resolved["project"]["portLabelsCommand"]));
    let regex = |v: &Value| {
        v.as_str().and_then(|pattern| match Regex::new(pattern) {
            Ok(regex) => Some(regex),
            Err(error) => {
                tracing::warn!("ports.rules: bad regex {pattern}: {error}");
                None
            }
        })
    };
    let rules = ports["rules"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|rule| {
            let when = &rule["match"];
            Rule {
                port: when["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
                command: regex(&when["command_regex"]),
                label_match: regex(&when["label_regex"]),
                label: non_empty(&rule["label"]),
                kind: rule["kind"].as_str().and_then(Kind::parse),
                primary: rule["primary"].as_bool().unwrap_or(false),
            }
        })
        .collect();
    PortSettings {
        labels_command,
        rules,
        probe: ports["probe"].as_bool().unwrap_or(true),
    }
}

fn apply_rules(services: &mut [Service], rules: &[Rule]) {
    for rule in rules {
        if rule.port.is_none() && rule.command.is_none() && rule.label_match.is_none() {
            continue;
        }
        for service in services.iter_mut() {
            let matches = rule.port.is_none_or(|p| p == service.port)
                && rule.command.as_ref().is_none_or(|r| {
                    let haystack = format!(
                        "{} {}",
                        service.process.as_deref().unwrap_or_default(),
                        service.command.as_deref().unwrap_or_default()
                    );
                    r.is_match(&haystack)
                })
                && rule
                    .label_match
                    .as_ref()
                    .is_none_or(|r| service.label.as_deref().is_some_and(|l| r.is_match(l)));
            if !matches {
                continue;
            }
            if let Some(label) = &rule.label {
                service.label = Some(label.clone());
            }
            if let Some(kind) = rule.kind {
                service.kind = kind;
                service.confidence = Confidence::High;
            }
            service.rule_primary |= rule.primary;
        }
    }
}

fn pick_primary(services: &[Service], kind: Option<Kind>) -> Option<usize> {
    let best = |filter: &dyn Fn(&Service) -> bool| {
        services
            .iter()
            .enumerate()
            .filter(|(_, s)| filter(s))
            .max_by_key(|(_, s)| (s.rule_primary, s.source, s.confidence, Reverse(s.port)))
            .map(|(i, _)| i)
    };
    match kind {
        Some(kind) => best(&|s| s.kind == kind),
        None => best(&|s| s.rule_primary && s.kind.is_http())
            .or_else(|| best(&|s| s.kind == Kind::Frontend))
            .or_else(|| best(&|s| s.kind.is_http())),
    }
}

fn roots(path: &Path) -> Vec<PathBuf> {
    let mut roots = vec![path.to_path_buf()];
    if let Ok(real) = std::fs::canonicalize(path)
        && real != path
    {
        roots.push(real);
    }
    roots
}

fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

async fn run_labels_command(command: &str, cwd: &Path) -> Vec<LabelEntry> {
    let shell = crate::terminal::default_shell();
    let run = tokio::process::Command::new(shell)
        .args(["-lc", command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(LABELS_TIMEOUT, run).await {
        Ok(Ok(output)) => labels::parse_labels(&String::from_utf8_lossy(&output.stdout)),
        Ok(Err(error)) => {
            tracing::debug!("ports.labels_command failed: {error}");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!("ports.labels_command timed out");
            Vec::new()
        }
    }
}

async fn probe(hosts: Vec<String>, port: u16) -> Option<Classification> {
    let page = probe::get(&hosts, port, "/").await;
    let found = page.as_ref().and_then(classify::classify_probe);
    let generic = found
        .as_ref()
        .is_none_or(|c| c.framework.is_none() && c.kind != Kind::Frontend);
    if page.is_some() && generic {
        let spec = probe::get(&hosts, port, "/openapi.json").await;
        if let Some(api) = spec.as_ref().and_then(classify::classify_openapi) {
            return Some(api);
        }
    }
    found
}

pub(crate) fn spawn(server: Arc<Server>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if server.client_count() == 0 {
                continue;
            }
            if let Err(error) = server.ports_poll().await {
                tracing::debug!("ports poll failed: {}", error.message);
            }
        }
    });
}

impl Server {
    pub(crate) async fn ports_dispatch(
        self: &Arc<Self>,
        method: &str,
        p: Value,
    ) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        struct P {
            #[serde(alias = "id")]
            workspace: String,
            kind: Option<String>,
        }
        let p: P = params(p)?;
        let workspace = self.workspace_by_key(&p.workspace)?;
        let services = self
            .ports_compute(std::slice::from_ref(&workspace))
            .await?
            .pop()
            .map(|(_, services)| services)
            .unwrap_or_default();
        match method {
            "ports.list" => to_value(services),
            "ports.primary" => {
                let kind = match p.kind.as_deref() {
                    None | Some("" | "any") => None,
                    Some(text) => Some(
                        Kind::parse(text)
                            .ok_or_else(|| rpc("invalid_params", format!("unknown kind {text}")))?,
                    ),
                };
                let chosen = pick_primary(&services, kind).map(|i| &services[i]);
                Ok(json!({
                    "workspace": workspace.id,
                    "url": chosen.map(|s| s.url.clone()),
                    "service": chosen,
                }))
            }
            other => Err(rpc("unknown_method", other)),
        }
    }

    async fn ports_poll(self: &Arc<Self>) -> Result<(), RpcError> {
        let workspaces = self
            .store
            .workspaces(None)
            .map_err(|e| rpc("internal", e))?;
        let results = self.ports_compute(&workspaces).await?;
        let live: HashSet<&str> = workspaces.iter().map(|w| w.id.as_str()).collect();
        let mut changed = Vec::new();
        {
            let mut fingerprints = lock(&self.ports.fingerprints);
            fingerprints.retain(|id, _| live.contains(id.as_str()));
            for (id, services) in &results {
                let fingerprint = serde_json::to_string(services).unwrap_or_default();
                let previous = fingerprints.insert(id.clone(), fingerprint.clone());
                if previous.unwrap_or_else(|| "[]".into()) != fingerprint {
                    changed.push((id.clone(), services));
                }
            }
        }
        for (id, services) in changed {
            self.broadcast(
                "ports.changed",
                json!({ "workspace": id, "ports": services }),
            );
        }
        Ok(())
    }

    async fn ports_snapshot(&self) -> Result<Arc<Snapshot>, RpcError> {
        let mut cached = self.ports.snapshot.lock().await;
        if let Some((at, snapshot)) = cached.as_ref()
            && at.elapsed() < SNAPSHOT_TTL
        {
            return Ok(Arc::clone(snapshot));
        }
        let snapshot = Arc::new(
            scan::scan()
                .await
                .map_err(|e| rpc("scan_failed", format!("{e:#}")))?,
        );
        *cached = Some((Instant::now(), Arc::clone(&snapshot)));
        Ok(snapshot)
    }

    fn ports_watch_terminals(&self, terminals: &[TerminalInfo]) {
        let live: HashSet<&str> = terminals.iter().map(|t| t.id.as_str()).collect();
        lock(&self.ports.advertised).retain(|id, _| live.contains(id.as_str()));
        let mut watched = lock(&self.ports.watched);
        watched.retain(|id| live.contains(id.as_str()));
        for terminal in terminals {
            if !watched.insert(terminal.id.clone()) {
                continue;
            }
            let Some(session) = self.terminals.get(&terminal.id) else {
                continue;
            };
            let (attachment, mut receiver) = session.attach(Some(0));
            drop(session);
            let store = Arc::clone(&self.ports.advertised);
            let id = terminal.id.clone();
            tokio::spawn(async move {
                let mut buffer = LineBuffer::default();
                let record = |found: Vec<ParsedUrl>| {
                    if found.is_empty() {
                        return;
                    }
                    let mut store = lock(&store);
                    let urls = store.entry(id.clone()).or_default();
                    for url in found {
                        urls.retain(|u| u.url != url.url);
                        urls.push(url);
                    }
                    let excess = urls.len().saturating_sub(URLS_PER_TERMINAL);
                    urls.drain(..excess);
                };
                if let Some(snapshot) = &attachment.snapshot {
                    record(buffer.ingest(snapshot.as_bytes()));
                    record(buffer.ingest(b"\n"));
                }
                for (_, chunk) in &attachment.chunks {
                    record(buffer.ingest(chunk));
                }
                loop {
                    match receiver.recv().await {
                        Ok(TerminalEvent::Output { data, .. }) => record(buffer.ingest(&data)),
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }
    }

    async fn ports_labels(
        &self,
        workspace: &Workspace,
        command: &str,
        ports: &[u16],
    ) -> Vec<LabelEntry> {
        let key = format!("{command}\0{ports:?}");
        if let Some(hit) = lock(&self.ports.labels).get(&workspace.id)
            && hit.key == key
            && hit.at.elapsed() < LABELS_TTL
        {
            return hit.entries.clone();
        }
        let entries = run_labels_command(command, &workspace.path).await;
        lock(&self.ports.labels).insert(
            workspace.id.clone(),
            LabelsCache {
                key,
                at: Instant::now(),
                entries: entries.clone(),
            },
        );
        entries
    }

    async fn ports_probe_all(&self, wanted: Vec<(u32, u16, Vec<String>)>) {
        let mut tasks = tokio::task::JoinSet::new();
        for (pid, port, hosts) in wanted {
            tasks.spawn(async move { ((pid, port), probe(hosts, port).await) });
        }
        while let Some(done) = tasks.join_next().await {
            if let Ok((key, found)) = done {
                lock(&self.ports.probes).insert(key, found);
            }
        }
    }

    async fn ports_refine(&self, services: &mut [Service]) {
        let wanted: Vec<(u32, u16, Vec<String>)> = {
            let probes = lock(&self.ports.probes);
            services
                .iter()
                .filter(|s| !s.probed && s.confidence < Confidence::High && s.kind.is_http())
                .filter_map(|s| Some((s.pid?, s.port, s.hosts.clone())))
                .filter(|(pid, port, _)| !probes.contains_key(&(*pid, *port)))
                .collect()
        };
        self.ports_probe_all(wanted).await;
        let probes = lock(&self.ports.probes);
        for service in services.iter_mut().filter(|s| !s.probed) {
            let Some(found) = service.pid.and_then(|pid| probes.get(&(pid, service.port))) else {
                continue;
            };
            service.probed = true;
            let current = Classification {
                kind: service.kind,
                framework: service.framework.clone(),
                confidence: service.confidence,
            };
            let merged = classify::merge(current, found.clone());
            if service.source != Source::LabelsCommand {
                service.kind = merged.kind;
                service.confidence = merged.confidence;
            }
            service.framework = merged.framework;
        }
    }

    // Attributes against every workspace, not just the targets, so none claims another's listener.
    async fn ports_compute(
        self: &Arc<Self>,
        targets: &[Workspace],
    ) -> Result<Vec<(String, Vec<Service>)>, RpcError> {
        let terminals = self.terminals.list();
        self.ports_watch_terminals(&terminals);
        let snapshot = self.ports_snapshot().await?;
        let all = self
            .store
            .workspaces(None)
            .map_err(|e| rpc("internal", e))?;
        let owners: Vec<Owner> = all
            .iter()
            .map(|w| Owner {
                id: w.id.clone(),
                roots: roots(&w.path),
                terminal_pids: terminals
                    .iter()
                    .filter(|t| t.running && t.workspace.as_deref() == Some(w.id.as_str()))
                    .filter_map(|t| t.pid)
                    .collect(),
            })
            .collect();
        let me = std::process::id();
        let owned: Vec<Option<(String, Attribution)>> = snapshot
            .listeners
            .iter()
            .map(|l| {
                let pid = l.pid?;
                scan::attribute(pid, &snapshot.processes, &owners, me)
                    .map(|(owner, how)| (owner.id.clone(), how))
            })
            .collect();
        {
            let present: HashSet<(u32, u16)> = snapshot
                .listeners
                .iter()
                .filter_map(|l| Some((l.pid?, l.port)))
                .collect();
            lock(&self.ports.probes).retain(|key, _| present.contains(key));
        }

        let mut results = Vec::new();
        for workspace in targets {
            let project_root = self
                .store
                .project(&workspace.project_id)
                .ok()
                .flatten()
                .map(|p| p.root);
            let settings = port_settings(&self.resolved_settings(project_root.as_deref()));
            let mut services: Vec<Service> = snapshot
                .listeners
                .iter()
                .zip(&owned)
                .filter(|(_, owner)| owner.as_ref().is_some_and(|(id, _)| *id == workspace.id))
                .map(|(l, owner)| Service::new(l, &snapshot, owner.as_ref().map(|(_, how)| *how)))
                .collect();
            let adoptable = |port: u16| {
                snapshot
                    .listeners
                    .iter()
                    .zip(&owned)
                    .find(|(l, owner)| l.port == port && owner.is_none())
                    .map(|(l, _)| l)
            };

            let workspace_roots = roots(&workspace.path);
            for terminal in terminals.iter().filter(|t| {
                t.workspace.as_deref() == Some(workspace.id.as_str())
                    || (t.workspace.is_none() && inside(&t.cwd, &workspace_roots))
            }) {
                let advertised = lock(&self.ports.advertised)
                    .get(&terminal.id)
                    .cloned()
                    .unwrap_or_default();
                for url in advertised {
                    let index = match services.iter().position(|s| s.port == url.port) {
                        Some(index) => index,
                        None => match adoptable(url.port) {
                            Some(listener) => {
                                services.push(Service::new(listener, &snapshot, None));
                                services.len() - 1
                            }
                            None => continue,
                        },
                    };
                    let service = &mut services[index];
                    service.source = service.source.max(Source::Advertised);
                    service.terminal.get_or_insert_with(|| terminal.id.clone());
                    service.advertised.push(url);
                }
            }

            if settings.probe {
                self.ports_refine(&mut services).await;
            }

            if let Some(command) = &settings.labels_command
                && !services.is_empty()
            {
                let mut live: Vec<u16> = services.iter().map(|s| s.port).collect();
                live.sort_unstable();
                live.dedup();
                for entry in self.ports_labels(workspace, command, &live).await {
                    let index = match services.iter().position(|s| s.port == entry.url.port) {
                        Some(index) => index,
                        None => match adoptable(entry.url.port) {
                            Some(listener) => {
                                services.push(Service::new(listener, &snapshot, None));
                                services.len() - 1
                            }
                            None => continue,
                        },
                    };
                    let service = &mut services[index];
                    if service.source == Source::LabelsCommand {
                        continue;
                    }
                    service.source = Source::LabelsCommand;
                    if let Some(kind) = entry
                        .kind
                        .or_else(|| classify::kind_from_label(&entry.label))
                    {
                        service.kind = kind;
                        service.confidence = service.confidence.max(Confidence::Medium);
                    }
                    service.label = Some(entry.label);
                    service.label_url = Some(entry.url.url);
                }
            }

            if settings.probe {
                self.ports_refine(&mut services).await;
            }

            apply_rules(&mut services, &settings.rules);
            for service in &mut services {
                service.finish_urls();
            }
            services.sort_by_key(|s| (s.kind.rank(), s.port, s.pid));
            if let Some(index) = pick_primary(&services, None) {
                services[index].primary = true;
            }
            results.push((workspace.id.clone(), services));
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(port: u16, kind: Kind, source: Source) -> Service {
        Service {
            port,
            pid: Some(u32::from(port)),
            process: Some("node".into()),
            command: Some(format!("node server-{port}")),
            hosts: vec!["*".into()],
            url: String::new(),
            urls: Vec::new(),
            label: None,
            kind,
            framework: None,
            confidence: Confidence::Medium,
            primary: false,
            source,
            attribution: Some(Attribution::Cwd),
            terminal: None,
            rule_primary: false,
            label_url: None,
            advertised: Vec::new(),
            probed: false,
        }
    }

    #[test]
    fn primary_prefers_rules_then_labels_then_advertised_then_scan() {
        let mut services = vec![
            service(3000, Kind::Frontend, Source::Scan),
            service(5173, Kind::Frontend, Source::Advertised),
            service(8000, Kind::Api, Source::LabelsCommand),
            service(5432, Kind::Database, Source::Scan),
        ];
        assert_eq!(pick_primary(&services, None), Some(1));
        assert_eq!(pick_primary(&services, Some(Kind::Api)), Some(2));
        services[1].source = Source::Scan;
        assert_eq!(
            pick_primary(&services, None),
            Some(0),
            "lower port on a tie"
        );
        services[2].rule_primary = true;
        assert_eq!(pick_primary(&services, None), Some(2));
        assert_eq!(pick_primary(&services, Some(Kind::Frontend)), Some(0));
        let databases = vec![service(5432, Kind::Database, Source::Scan)];
        assert_eq!(pick_primary(&databases, None), None);
    }

    #[test]
    fn settings_read_rules_and_fall_back_to_orca_labels_command() {
        let resolved = json!({
            "project": { "portLabelsCommand": "mise run print-urls" },
            "ports": {
                "probe": false,
                "rules": [
                    { "match": { "port": 8000 }, "label": "Backend", "kind": "api", "primary": true },
                    { "match": { "command_regex": "vite" }, "kind": "frontend" },
                    { "match": { "command_regex": "(" }, "label": "broken" }
                ]
            }
        });
        let settings = port_settings(&resolved);
        assert_eq!(
            settings.labels_command.as_deref(),
            Some("mise run print-urls")
        );
        assert!(!settings.probe);
        let mut services = vec![
            service(8000, Kind::Other, Source::Scan),
            service(5173, Kind::Other, Source::Scan),
        ];
        services[1].command = Some("node node_modules/.bin/vite".into());
        apply_rules(&mut services, &settings.rules);
        assert_eq!(services[0].label.as_deref(), Some("Backend"));
        assert_eq!(services[0].kind, Kind::Api);
        assert!(services[0].rule_primary);
        assert_eq!(services[1].kind, Kind::Frontend);
        assert!(
            services[1].label.is_none(),
            "an invalid regex never matches"
        );

        let own = json!({ "ports": { "labels_command": "make urls" }, "project": { "portLabelsCommand": "x" } });
        assert_eq!(
            port_settings(&own).labels_command.as_deref(),
            Some("make urls")
        );
        assert!(port_settings(&json!({})).probe);
    }

    #[test]
    fn urls_prefer_labels_then_named_hosts_then_localhost() {
        let mut s = service(5173, Kind::Frontend, Source::Advertised);
        s.advertised = [
            "http://0.0.0.0:5173/",
            "http://localhost:5173/",
            "http://feat.localhost:5173/",
        ]
        .iter()
        .filter_map(|u| urls::parse_url(u))
        .collect();
        s.finish_urls();
        assert_eq!(s.url, "http://feat.localhost:5173/");
        assert_eq!(
            s.urls,
            [
                "http://feat.localhost:5173/",
                "http://localhost:5173/",
                "http://0.0.0.0:5173/"
            ]
        );

        s.label_url = Some("http://feat.localhost:1355".into());
        s.finish_urls();
        assert_eq!(s.url, "http://feat.localhost:1355");

        let mut bare = service(9000, Kind::Api, Source::Scan);
        bare.finish_urls();
        assert_eq!(bare.url, "http://localhost:9000");
        assert_eq!(bare.urls, ["http://localhost:9000"]);
    }
}
