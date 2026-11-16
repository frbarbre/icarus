//! Agent tabs in projects opened over SSH run `icarus acp` on that host. When the host has no
//! `icarus`, the tab offers to install the matching release's CLI and server into `~/.icarus/bin`
//! there: downloaded on the host when it has curl or wget, else through the app and sent over SSH.

use std::rc::Rc;

use acp_thread::AgentConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use gpui::{App, AppContext as _, AsyncApp, Entity, PromptLevel, Task};
use http_client::{AsyncBody, HttpClient};
use project::agent_server_store::AgentServerCommand;
use project::{AgentId, Project};
use remote::RemoteClient;
use remote::command::RemoteCommand;
use util::command::{Command, Stdio};

use crate::remote_agent::ICARUS_AGENT;

const RELEASES: &str = "https://github.com/frbarbre/icarus/releases/download";

/// Prints where `icarus` is on the host (empty if nowhere) and the host's OS and machine.
const PROBE: &str = r#"p=$(command -v icarus 2>/dev/null || "${SHELL:-/bin/sh}" -lc 'command -v icarus' 2>/dev/null | tail -n 1)
if [ -z "$p" ] && [ -x "$HOME/.icarus/bin/icarus" ]; then p="$HOME/.icarus/bin/icarus"; fi
echo "icarus=$p"
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
"#;

/// `install URL SHA256` fetches the release tarball on the host; `install - SHA256` reads it from
/// stdin. Either way it is checked, unpacked into `~/.icarus/bin` and the CLI's path printed.
/// Exits 3 when the host can't download (no curl or wget), so the app sends the file instead.
const INSTALL: &str = r#"set -eu
url="$1"; sha="$2"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/icarus-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [ "$url" = "-" ]; then
  cat > "$tmp/icarus.tar.gz"
elif command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/icarus.tar.gz"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/icarus.tar.gz" "$url"
else
  exit 3
fi
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/icarus.tar.gz" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp/icarus.tar.gz" | cut -d ' ' -f 1)
else
  got="$sha"
fi
if [ "$got" != "$sha" ]; then
  echo "checksum mismatch: expected $sha, got $got" >&2
  exit 4
fi
tar -xzf "$tmp/icarus.tar.gz" -C "$tmp"
dest="$HOME/.icarus/bin"
mkdir -p "$dest"
for bin in icarus icarus-server; do
  src=$(find "$tmp" -type f -name "$bin" | head -n 1)
  [ -n "$src" ] || { echo "the release has no $bin" >&2; exit 5; }
  cp "$src" "$dest/$bin.new"
  chmod 755 "$dest/$bin.new"
  mv -f "$dest/$bin.new" "$dest/$bin"
done
echo "$dest/icarus"
"#;

/// Starts the host's server if it isn't running (agents live there), then speaks ACP.
const LAUNCH: &str = r#""$1" server start >/dev/null 2>&1 || true
exec "$1" acp"#;

const NO_DOWNLOADER: i32 = 3;

#[derive(Debug, Default, PartialEq)]
pub(crate) struct HostProbe {
    pub icarus: Option<String>,
    pub os: String,
    pub arch: String,
}

pub(crate) fn parse_probe(output: &str) -> HostProbe {
    let mut probe = HostProbe::default();
    for line in output.lines() {
        match line.split_once('=') {
            Some(("icarus", path)) if !path.trim().is_empty() => {
                probe.icarus = Some(path.trim().to_owned())
            }
            Some(("os", os)) => probe.os = os.trim().to_owned(),
            Some(("arch", arch)) => probe.arch = arch.trim().to_owned(),
            _ => {}
        }
    }
    probe
}

/// The release's binaries tarball for a host, named as `release.yml` builds them.
pub(crate) fn release_target(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("Darwin", _) => Ok("universal-apple-darwin"),
        ("Linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-gnu"),
        _ => bail!("Icarus has no release for {os} on {arch}"),
    }
}

/// Where a version's assets are; `ICARUS_RELEASE_BASE_URL` points tests and mirrors elsewhere.
pub(crate) fn release_base(version: &str, base_override: Option<&str>) -> String {
    match base_override {
        Some(base) => base.trim_end_matches('/').to_owned(),
        None => format!("{RELEASES}/v{version}"),
    }
}

pub(crate) fn asset_name(version: &str, target: &str) -> String {
    format!("icarus-{version}-{target}.tar.gz")
}

/// The checksum `SHA256SUMS` lists for `name` (`sha256sum` format, with or without `*`).
pub(crate) fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_lowercase())
    })
}

/// Runs `sh -c SCRIPT sh ARGS…` on the host, feeding `stdin`; returns its exit code and stdout.
fn run_on_host(
    remote: &Entity<RemoteClient>,
    script: &str,
    args: &[String],
    stdin: Vec<u8>,
    cx: &mut AsyncApp,
) -> Task<Result<(Option<i32>, String)>> {
    let command = remote.read_with(cx, |remote, _| {
        remote.build_stdio_command(RemoteCommand {
            program: "sh".into(),
            args: ["-c".to_owned(), script.to_owned(), "sh".to_owned()]
                .into_iter()
                .chain(args.iter().cloned())
                .collect(),
            env: Default::default(),
            working_dir: None,
        })
    });
    cx.background_spawn(async move {
        let (template, preamble) = command?;
        let mut command = util::command::new_command(&template.program);
        command.args(&template.args).envs(&template.env);
        run_local(command, [preamble, stdin].concat()).await
    })
}

async fn run_local(mut command: Command, stdin: Vec<u8>) -> Result<(Option<i32>, String)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start the command on the host")?;
    if let Some(mut input) = child.stdin.take() {
        input
            .write_all(&stdin)
            .await
            .context("send input to the host")?;
    }
    let output = child.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let code = output.status.code();
    if !matches!(code, Some(0) | Some(NO_DOWNLOADER)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }
    Ok((code, stdout))
}

async fn download(http: &dyn HttpClient, url: &str) -> Result<Vec<u8>> {
    let mut response = http
        .get(url, AsyncBody::default(), true)
        .await
        .with_context(|| format!("download {url}"))?;
    if !response.status().is_success() {
        bail!("download {url}: {}", response.status());
    }
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    Ok(body)
}

/// Installs the release into the host's `~/.icarus/bin`; returns the installed CLI's path.
async fn install(
    remote: &Entity<RemoteClient>,
    probe: &HostProbe,
    http: std::sync::Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let version = release_channel::ICARUS_VERSION;
    let target = release_target(&probe.os, &probe.arch)?;
    let base = release_base(
        version,
        std::env::var("ICARUS_RELEASE_BASE_URL").ok().as_deref(),
    );
    let name = asset_name(version, target);
    let sums = download(http.as_ref(), &format!("{base}/SHA256SUMS")).await?;
    let sha = expected_sha256(&String::from_utf8_lossy(&sums), &name)
        .ok_or_else(|| anyhow!("the {version} release lists no {name}"))?;
    let url = format!("{base}/{name}");
    let (code, stdout) =
        run_on_host(remote, INSTALL, &[url.clone(), sha.clone()], Vec::new(), cx).await?;
    let stdout = if code == Some(NO_DOWNLOADER) {
        let tarball = download(http.as_ref(), &url).await?;
        run_on_host(remote, INSTALL, &["-".into(), sha], tarball, cx)
            .await?
            .1
    } else {
        stdout
    };
    stdout
        .lines()
        .last()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the install printed no path"))
}

fn launch_command(icarus: &str) -> AgentServerCommand {
    AgentServerCommand {
        path: "sh".into(),
        args: vec!["-c".into(), LAUNCH.into(), "sh".into(), icarus.into()],
        env: None,
    }
}

/// Asks in the active window whether to install; false when there's no window or the user declines.
async fn ask_to_install(host: &str, cx: &mut AsyncApp) -> bool {
    let Some(window) = cx.update(|cx| cx.active_window()) else {
        return false;
    };
    let message = format!("Install Icarus on {host}?");
    let detail = format!(
        "Agent tabs here run `icarus acp` on {host}, which doesn't have it. Icarus {} \
         (CLI and server) can be downloaded into ~/.icarus/bin there.",
        release_channel::ICARUS_VERSION
    );
    let answer = window.update(cx, |_, window, cx| {
        window.prompt(
            PromptLevel::Info,
            &message,
            Some(&detail),
            &["Install", "Cancel"],
            cx,
        )
    });
    match answer {
        Ok(answer) => answer.await == Ok(0),
        Err(_) => false,
    }
}

pub struct IcarusOverSsh {
    host: String,
}

impl IcarusOverSsh {
    pub fn new(host: String) -> Self {
        Self { host }
    }
}

impl AgentServer for IcarusOverSsh {
    fn logo(&self) -> ui::IconName {
        ui::IconName::Terminal
    }

    fn agent_id(&self) -> AgentId {
        AgentId::new(ICARUS_AGENT)
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let Some(remote) = project.read(cx).remote_client() else {
            return Task::ready(Err(anyhow!("this project isn't open over SSH")));
        };
        let host = self.host.clone();
        let http = cx.http_client();
        let store = project.read(cx).agent_server_store().downgrade();
        let agent_id = self.agent_id();
        cx.spawn(async move |cx| {
            let (_, output) = run_on_host(&remote, PROBE, &[], Vec::new(), cx).await?;
            let probe = parse_probe(&output);
            let icarus = match &probe.icarus {
                Some(icarus) => icarus.clone(),
                None => {
                    if !ask_to_install(&host, cx).await {
                        bail!(
                            "{host} has no `icarus` CLI. Open a new agent tab to install it, \
                             or install Icarus there yourself."
                        );
                    }
                    install(&remote, &probe, http, cx)
                        .await
                        .with_context(|| format!("install Icarus on {host}"))?
                }
            };
            agent_servers::connect_acp(
                agent_id,
                project,
                launch_command(&icarus),
                store,
                None,
                Default::default(),
                cx,
            )
            .await
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_probe_reports_the_cli_and_the_host() {
        let probe = parse_probe("icarus=/home/me/.icarus/bin/icarus\nos=Linux\narch=aarch64\n");
        assert_eq!(
            probe,
            HostProbe {
                icarus: Some("/home/me/.icarus/bin/icarus".into()),
                os: "Linux".into(),
                arch: "aarch64".into(),
            }
        );
        assert_eq!(parse_probe("icarus=\nos=Darwin\narch=arm64\n").icarus, None);
    }

    #[test]
    fn hosts_map_to_the_release_tarballs() {
        assert_eq!(
            release_target("Linux", "x86_64").ok(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            release_target("Linux", "arm64").ok(),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            release_target("Darwin", "x86_64").ok(),
            Some("universal-apple-darwin")
        );
        assert!(release_target("FreeBSD", "amd64").is_err());
        assert_eq!(
            asset_name("0.2.0", "universal-apple-darwin"),
            "icarus-0.2.0-universal-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_base("0.2.0", None),
            "https://github.com/frbarbre/icarus/releases/download/v0.2.0"
        );
        assert_eq!(
            release_base("0.2.0", Some("file:///tmp/mock/")),
            "file:///tmp/mock"
        );
    }

    #[test]
    fn checksums_are_read_in_sha256sum_format() {
        let sums = "ABC  Icarus-0.2.0-macos-aarch64.zip\ndef *icarus-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            expected_sha256(sums, "icarus-0.2.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("def")
        );
        assert_eq!(expected_sha256(sums, "missing.tar.gz"), None);
    }

    /// A release tarball laid out as `release.yml` packs it, plus its checksum.
    fn mock_release(dir: &Path) -> (std::path::PathBuf, String) {
        let staging = dir.join("icarus-9.9.9-x86_64-unknown-linux-gnu");
        std::fs::create_dir_all(&staging).expect("staging");
        for bin in ["icarus", "icarus-server"] {
            std::fs::write(staging.join(bin), format!("#!/bin/sh\necho {bin}\n")).expect("bin");
        }
        let tarball = dir.join("icarus-9.9.9-x86_64-unknown-linux-gnu.tar.gz");
        let tarball_arg = tarball.display().to_string();
        let dir_arg = dir.display().to_string();
        run(
            "tar",
            &[
                "-czf",
                &tarball_arg,
                "-C",
                &dir_arg,
                "icarus-9.9.9-x86_64-unknown-linux-gnu",
            ],
        );
        let sha = run("shasum", &["-a", "256", &tarball_arg])
            .split_whitespace()
            .next()
            .expect("hash")
            .to_owned();
        (tarball, sha)
    }

    /// Runs a local program to completion and returns its stdout, failing the test if it fails.
    fn run(program: &str, args: &[&str]) -> String {
        let output =
            futures::executor::block_on(util::command::new_command(program).args(args).output())
                .expect(program);
        assert!(output.status.success(), "{program} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn install_locally(
        home: &Path,
        args: &[&str],
        stdin: Vec<u8>,
    ) -> Result<(Option<i32>, String)> {
        let mut command = util::command::new_command("sh");
        command
            .args(["-c", INSTALL, "sh"])
            .args(args)
            .env("HOME", home);
        futures::executor::block_on(run_local(command, stdin))
    }

    #[test]
    fn the_host_downloads_checks_and_installs_the_release() {
        let release = tempfile::tempdir().expect("release dir");
        let home = tempfile::tempdir().expect("home");
        let (tarball, sha) = mock_release(release.path());
        let url = format!("file://{}", tarball.display());

        let (code, stdout) =
            install_locally(home.path(), &[&url, &sha], Vec::new()).expect("install");
        assert_eq!(code, Some(0));
        let installed = home.path().join(".icarus/bin/icarus");
        assert_eq!(stdout.trim(), installed.display().to_string());
        assert_eq!(run(&installed.display().to_string(), &[]), "icarus\n");
        assert!(home.path().join(".icarus/bin/icarus-server").is_file());

        let error = install_locally(home.path(), &[&url, "0000"], Vec::new())
            .expect_err("a wrong checksum fails");
        assert!(format!("{error}").contains("checksum mismatch"), "{error}");
    }

    #[test]
    fn a_host_without_a_downloader_takes_the_release_over_stdin() {
        let release = tempfile::tempdir().expect("release dir");
        let home = tempfile::tempdir().expect("home");
        let (tarball, sha) = mock_release(release.path());
        let bytes = std::fs::read(&tarball).expect("tarball");
        let (code, stdout) = install_locally(home.path(), &["-", &sha], bytes).expect("install");
        assert_eq!(code, Some(0));
        assert!(stdout.trim().ends_with(".icarus/bin/icarus"));
        assert!(home.path().join(".icarus/bin/icarus").is_file());
    }

    #[test]
    fn the_launch_starts_the_server_then_speaks_acp() {
        let home = tempfile::tempdir().expect("home");
        let cli = home.path().join("icarus");
        std::fs::write(
            &cli,
            "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/calls\"\n",
        )
        .expect("cli");
        let cli = cli.display().to_string();
        run("chmod", &["755", &cli]);
        let command = launch_command(&cli);
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        run(&command.path.display().to_string(), &args);
        let calls = std::fs::read_to_string(home.path().join("calls")).expect("calls");
        assert_eq!(calls, "server start\nacp\n");
    }
}
