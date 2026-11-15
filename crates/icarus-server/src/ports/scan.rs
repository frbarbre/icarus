use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ANCESTRY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub hosts: Vec<String>,
    pub pid: Option<u32>,
    pub process: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Process {
    pub ppid: Option<u32>,
    pub command: String,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub listeners: Vec<Listener>,
    pub processes: HashMap<u32, Process>,
}

pub async fn scan() -> Result<Snapshot> {
    if cfg!(target_os = "linux") {
        tokio::task::spawn_blocking(scan_proc)
            .await
            .context("proc scan")?
    } else {
        scan_lsof().await
    }
}

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        COMMAND_TIMEOUT,
        Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .with_context(|| format!("{program} timed out"))?
    .with_context(|| format!("run {program}"))?;
    // lsof exits 1 when nothing matches, which is a valid empty answer.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn scan_lsof() -> Result<Snapshot> {
    let listing = run(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn", "-b", "-w"],
    )
    .await?;
    let listeners = parse_lsof_listeners(&listing);
    let table = parse_ps(&run("ps", &["-axo", "pid=,ppid=,command="]).await?);
    let wanted = with_ancestors(listeners.iter().filter_map(|l| l.pid), |pid| {
        table.get(&pid).and_then(|(ppid, _)| *ppid)
    });
    let mut cwds = HashMap::new();
    if !wanted.is_empty() {
        let list: Vec<String> = wanted.iter().map(u32::to_string).collect();
        let joined = list.join(",");
        let output = run(
            "lsof",
            &["-O", "-b", "-w", "-a", "-p", &joined, "-d", "cwd", "-Fn"],
        )
        .await
        .unwrap_or_default();
        cwds = parse_lsof_cwds(&output);
    }
    let processes = wanted
        .into_iter()
        .map(|pid| {
            let (ppid, command) = table.get(&pid).cloned().unwrap_or_default();
            let cwd = cwds.remove(&pid);
            (pid, Process { ppid, command, cwd })
        })
        .collect();
    Ok(Snapshot {
        listeners,
        processes,
    })
}

fn with_ancestors(
    pids: impl Iterator<Item = u32>,
    parent: impl Fn(u32) -> Option<u32>,
) -> HashSet<u32> {
    let mut all = HashSet::new();
    for pid in pids {
        let mut current = Some(pid);
        for _ in 0..MAX_ANCESTRY {
            match current {
                Some(pid) if pid > 1 && all.insert(pid) => current = parent(pid),
                _ => break,
            }
        }
    }
    all
}

pub fn parse_lsof_listeners(output: &str) -> Vec<Listener> {
    let mut found: BTreeMap<(u16, Option<u32>), Listener> = BTreeMap::new();
    let mut pid = None;
    let mut process = None;
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                process = None;
            }
            'c' => process = Some(value.to_string()),
            'n' => {
                if let Some((host, port)) = split_host_port(value) {
                    let listener = found.entry((port, pid)).or_insert_with(|| Listener {
                        port,
                        hosts: Vec::new(),
                        pid,
                        process: process.clone(),
                    });
                    if !listener.hosts.contains(&host) {
                        listener.hosts.push(host);
                    }
                }
            }
            _ => {}
        }
    }
    found.into_values().collect()
}

fn split_host_port(address: &str) -> Option<(String, u16)> {
    let address = address.split("->").next()?;
    let (host, port) = address.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    Some((host.to_string(), port))
}

pub fn parse_lsof_cwds(output: &str) -> HashMap<u32, PathBuf> {
    let mut cwds = HashMap::new();
    let mut pid = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<u32>().ok();
        } else if let (Some(value), Some(pid)) = (line.strip_prefix('n'), pid)
            && !value.is_empty()
        {
            cwds.insert(pid, PathBuf::from(value));
        }
    }
    cwds
}

pub fn parse_ps(output: &str) -> HashMap<u32, (Option<u32>, String)> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok();
            let command = fields.collect::<Vec<_>>().join(" ");
            Some((pid, (ppid, command)))
        })
        .collect()
}

pub fn parse_proc_net_tcp(content: &str) -> Vec<(String, u16, u64)> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (host, port) = fields[1].split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok().filter(|p| *p > 0)?;
            let inode = fields[9].parse().ok().filter(|i| *i > 0)?;
            Some((proc_host(host)?, port, inode))
        })
        .collect()
}

fn proc_host(hex: &str) -> Option<String> {
    let words: Option<Vec<[u8; 4]>> = (0..hex.len() / 8)
        .map(|i| {
            u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16)
                .ok()
                .map(u32::to_le_bytes)
        })
        .collect();
    let bytes: Vec<u8> = words?.into_iter().flatten().collect();
    match bytes.len() {
        4 => Some(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
        16 => {
            let octets: [u8; 16] = bytes.try_into().ok()?;
            Some(format!("[{}]", std::net::Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

fn scan_proc() -> Result<Snapshot> {
    let mut sockets = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = std::fs::read_to_string(file) {
            sockets.extend(parse_proc_net_tcp(&content));
        }
    }
    let inodes: HashSet<u64> = sockets.iter().map(|(_, _, inode)| *inode).collect();
    let mut owners: HashMap<u64, u32> = HashMap::new();
    if !inodes.is_empty() {
        for entry in std::fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(link) = std::fs::read_link(fd.path()) else {
                    continue;
                };
                let inode = link
                    .to_str()
                    .and_then(|l| l.strip_prefix("socket:["))
                    .and_then(|l| l.strip_suffix(']'))
                    .and_then(|l| l.parse::<u64>().ok());
                if let Some(inode) = inode.filter(|i| inodes.contains(i)) {
                    owners.insert(inode, pid);
                }
            }
        }
    }
    let mut found: BTreeMap<(u16, Option<u32>), Listener> = BTreeMap::new();
    for (host, port, inode) in sockets {
        let pid = owners.get(&inode).copied();
        let listener = found.entry((port, pid)).or_insert_with(|| Listener {
            port,
            hosts: Vec::new(),
            pid,
            process: pid.and_then(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .ok()
                    .map(|c| c.trim().to_string())
            }),
        });
        if !listener.hosts.contains(&host) {
            listener.hosts.push(host);
        }
    }
    let listeners: Vec<Listener> = found.into_values().collect();
    let wanted = with_ancestors(listeners.iter().filter_map(|l| l.pid), proc_ppid);
    let processes = wanted
        .into_iter()
        .map(|pid| {
            let command = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|raw| {
                    String::from_utf8_lossy(&raw)
                        .split('\0')
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let process = Process {
                ppid: proc_ppid(pid),
                command,
                cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok(),
            };
            (pid, process)
        })
        .collect();
    Ok(Snapshot {
        listeners,
        processes,
    })
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may itself contain spaces or parens.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

#[derive(Debug, Clone)]
pub struct Owner {
    pub id: String,
    pub roots: Vec<PathBuf>,
    pub terminal_pids: HashSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    Cwd,
    Terminal,
    AncestorCwd,
    Command,
}

fn deepest(owners: &[Owner], matches: impl Fn(&Path) -> bool) -> Option<&Owner> {
    owners
        .iter()
        .flat_map(|owner| owner.roots.iter().map(move |root| (owner, root)))
        .filter(|(_, root)| matches(root))
        .max_by_key(|(_, root)| root.as_os_str().len())
        .map(|(owner, _)| owner)
}

fn mentions_path(command: &str, root: &Path) -> bool {
    let Some(root) = root.to_str() else {
        return false;
    };
    command.match_indices(root).any(|(index, _)| {
        let before = command[..index].chars().next_back();
        let after = command[index + root.len()..].chars().next();
        before.is_none_or(|c| c.is_whitespace() || "\"'=".contains(c))
            && after.is_none_or(|c| c.is_whitespace() || "\"'/:".contains(c))
    })
}

// The walk stops at `stop` (the server) because every terminal descends from it and its cwd is arbitrary.
pub fn attribute<'a>(
    pid: u32,
    processes: &HashMap<u32, Process>,
    owners: &'a [Owner],
    stop: u32,
) -> Option<(&'a Owner, Attribution)> {
    let process = processes.get(&pid);
    if let Some(cwd) = process.and_then(|p| p.cwd.as_deref())
        && let Some(owner) = deepest(owners, |root| cwd.starts_with(root))
    {
        return Some((owner, Attribution::Cwd));
    }
    let mut ancestor_cwd = None;
    let mut current = Some(pid);
    for _ in 0..MAX_ANCESTRY {
        let Some(at) = current.filter(|p| *p > 1 && *p != stop) else {
            break;
        };
        if let Some(owner) = owners.iter().find(|o| o.terminal_pids.contains(&at)) {
            return Some((owner, Attribution::Terminal));
        }
        let entry = processes.get(&at);
        if at != pid
            && ancestor_cwd.is_none()
            && let Some(cwd) = entry.and_then(|p| p.cwd.as_deref())
        {
            ancestor_cwd = deepest(owners, |root| cwd.starts_with(root));
        }
        current = entry.and_then(|p| p.ppid);
    }
    if let Some(owner) = ancestor_cwd {
        return Some((owner, Attribution::AncestorCwd));
    }
    let command = process.map(|p| p.command.as_str()).unwrap_or_default();
    deepest(owners, |root| mentions_path(command, root)).map(|owner| (owner, Attribution::Command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_listener_records() {
        let output = "p501\ncnode\nf23\nn*:5173\nf24\nn[::1]:5173\np88\ncpostgres\nf5\nn127.0.0.1:5432\nf6\nn[::1]:5432\n";
        let listeners = parse_lsof_listeners(output);
        assert_eq!(
            listeners,
            [
                Listener {
                    port: 5173,
                    hosts: vec!["*".into(), "[::1]".into()],
                    pid: Some(501),
                    process: Some("node".into()),
                },
                Listener {
                    port: 5432,
                    hosts: vec!["127.0.0.1".into(), "[::1]".into()],
                    pid: Some(88),
                    process: Some("postgres".into()),
                },
            ]
        );
    }

    #[test]
    fn parses_cwds_and_ps_rows() {
        let cwds = parse_lsof_cwds("p501\nfcwd\nn/Users/me/ws/web\np502\nfcwd\nn/\n");
        assert_eq!(cwds.get(&501), Some(&PathBuf::from("/Users/me/ws/web")));
        let rows = parse_ps("  501   400 node /x/vite --port 1\n    1     0 /sbin/launchd\n");
        assert_eq!(
            rows.get(&501),
            Some(&(Some(400), "node /x/vite --port 1".into()))
        );
    }

    #[test]
    fn parses_proc_net_tcp() {
        let content = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n   1: 00000000:0016 00000000:0000 01 00000000:00000000 00:00000000 00000000     0        0 999 1\n";
        assert_eq!(
            parse_proc_net_tcp(content),
            [("127.0.0.1".to_string(), 8080, 12345)]
        );
        let v6 = "header\n 0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 777 1\n";
        assert_eq!(parse_proc_net_tcp(v6), [("[::1]".to_string(), 8080, 777)]);
    }

    fn owner(id: &str, root: &str, terminals: &[u32]) -> Owner {
        Owner {
            id: id.into(),
            roots: vec![PathBuf::from(root)],
            terminal_pids: terminals.iter().copied().collect(),
        }
    }

    fn process(ppid: u32, cwd: &str, command: &str) -> Process {
        Process {
            ppid: Some(ppid),
            command: command.into(),
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
        }
    }

    #[test]
    fn attribution_prefers_cwd_then_terminal_then_ancestor_then_command() {
        let owners = [
            owner("repo", "/ws/repo", &[]),
            owner("nested", "/ws/repo/nested", &[]),
            owner("term", "/ws/term", &[300]),
        ];
        let processes: HashMap<u32, Process> = [
            (10, process(1, "/ws/repo/nested/web", "node vite")),
            (20, process(300, "/tmp", "python3 -m http.server")),
            (300, process(1, "/ws/term", "zsh")),
            (30, process(31, "/tmp", "node server.js")),
            (31, process(1, "/ws/repo/app", "agent")),
            (40, process(1, "/", "node /ws/term/bin/server.js")),
            (50, process(1, "/", "node /ws/terminal/server.js")),
            (60, process(999, "/elsewhere", "x")),
            (999, process(1, "/ws/repo", "icarus-server")),
        ]
        .into_iter()
        .collect();
        let who =
            |pid| attribute(pid, &processes, &owners, 999).map(|(o, how)| (o.id.as_str(), how));
        assert_eq!(who(10), Some(("nested", Attribution::Cwd)));
        assert_eq!(who(20), Some(("term", Attribution::Terminal)));
        assert_eq!(who(30), Some(("repo", Attribution::AncestorCwd)));
        assert_eq!(who(40), Some(("term", Attribution::Command)));
        assert_eq!(who(50), None);
        assert_eq!(who(60), None, "the server's own cwd never attributes");
    }
}
