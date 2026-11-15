//! `icarus update check|install`: the server checks GitHub releases; installing happens on this machine.

use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use icarus_client::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Cli, connect, home, print, service};

const REPO: &str = "frbarbre/icarus";
const SUMS: &str = "SHA256SUMS";
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Subcommand)]
pub enum UpdateCommand {
    /// Compare the server's version with the newest Icarus release.
    Check,
    /// Install the newest release and restart the server; terminals keep running, agents sleep
    /// and resume.
    ///
    /// Homebrew installs run `brew upgrade --cask icarus`. Otherwise the release asset is
    /// downloaded, checked against SHA256SUMS and swapped in: the app bundle on macOS, else the
    /// `icarus` and `icarus-server` binaries. The old copy stays next to it as `*.previous`.
    Install {
        /// Reinstall even when already up to date.
        #[arg(long)]
        force: bool,
    },
}

pub async fn run(cli: &Cli, command: &UpdateCommand) -> Result<()> {
    match command {
        UpdateCommand::Check => {
            let check = check(cli).await?;
            print(cli, &check, describe);
            Ok(())
        }
        UpdateCommand::Install { force } => install(cli, *force).await,
    }
}

async fn check(cli: &Cli) -> Result<Value> {
    connect(cli)
        .await?
        .request_timeout("update.check", json!({}), CHECK_TIMEOUT)
        .await
}

fn describe(check: &Value) -> String {
    let current = check["current"].as_str().unwrap_or("?");
    match check["latest"].as_str() {
        Some(latest) if check["update_available"] == json!(true) => {
            let mut text = format!(
                "Icarus {latest} is available (you have {current}).\n{}\nInstall it with `icarus update install`.",
                check["url"].as_str().unwrap_or_default()
            );
            if let Some(notes) = check["notes"].as_str() {
                text = format!("{text}\n\n{notes}");
            }
            text
        }
        Some(latest) => format!("Icarus {current} is up to date (latest release: {latest})."),
        None => format!("No Icarus release is published yet (you have {current})."),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Method {
    Homebrew,
    AppBundle(PathBuf),
    Binaries(PathBuf),
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Process::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The `.app` bundle an executable lives in, if any.
fn enclosing_app(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|dir| {
            dir.extension().is_some_and(|ext| ext == "app") && dir.join("Contents").is_dir()
        })
        .map(Path::to_path_buf)
}

fn detect(exe: &Path) -> Result<Method> {
    if cfg!(target_os = "macos") && succeeds("brew", &["list", "--cask", "icarus"]) {
        return Ok(Method::Homebrew);
    }
    if cfg!(target_os = "macos")
        && let Some(app) = enclosing_app(exe)
    {
        return Ok(Method::AppBundle(app));
    }
    let text = exe.to_string_lossy();
    if text.contains("/target/debug/") || text.contains("/target/release/") {
        bail!(
            "{} is a development build; update it with git and cargo",
            exe.display()
        );
    }
    let dir = exe.parent().context("the CLI has no parent directory")?;
    Ok(Method::Binaries(dir.to_path_buf()))
}

fn binaries_target() -> String {
    if cfg!(target_os = "macos") {
        "universal-apple-darwin".to_owned()
    } else {
        format!("{}-unknown-linux-gnu", std::env::consts::ARCH)
    }
}

fn asset_name(method: &Method, version: &str) -> Option<String> {
    match method {
        Method::Homebrew => None,
        Method::AppBundle(_) => Some(format!(
            "Icarus-{version}-macos-{}.zip",
            std::env::consts::ARCH
        )),
        Method::Binaries(_) => Some(format!("icarus-{version}-{}.tar.gz", binaries_target())),
    }
}

/// The hash `sha256sum` printed for `name`.
fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// `gh` first, since it can read the private repository; `curl` for public downloads.
fn download(tag: &str, name: &str, url: &str, dir: &Path) -> Result<PathBuf> {
    let dest = dir.join(name);
    let dir_text = dir.to_string_lossy();
    if succeeds(
        "gh",
        &[
            "release",
            "download",
            tag,
            "--repo",
            REPO,
            "--pattern",
            name,
            "--dir",
            &dir_text,
            "--clobber",
        ],
    ) && dest.is_file()
    {
        return Ok(dest);
    }
    let status = Process::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&dest)
        .arg(url)
        .status()
        .context("run curl")?;
    if !status.success() || !dest.is_file() {
        bail!("could not download {name} from {url}");
    }
    Ok(dest)
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Renames `new` onto `target`, keeping the old one as `<target>.previous` and restoring it on failure.
fn swap_into_place(target: &Path, new: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("target has no file name")?
        .to_string_lossy();
    let previous = target.with_file_name(format!("{name}.previous"));
    remove_path(&previous).with_context(|| format!("remove {}", previous.display()))?;
    let had_old = std::fs::symlink_metadata(target).is_ok();
    if had_old {
        std::fs::rename(target, &previous)
            .with_context(|| format!("move {} aside", target.display()))?;
    }
    if let Err(error) = std::fs::rename(new, target) {
        if had_old {
            let _ = std::fs::rename(&previous, target);
        }
        return Err(error).with_context(|| format!("move the update into {}", target.display()));
    }
    Ok(previous)
}

fn staging_dir(beside: &Path) -> Result<PathBuf> {
    let dir = beside.join(format!(".icarus-update-{}", std::process::id()));
    remove_path(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "create {} (is {} writable?)",
            dir.display(),
            beside.display()
        )
    })?;
    Ok(dir)
}

fn find_named(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let candidate = root.join(name);
    if std::fs::symlink_metadata(&candidate).is_ok() {
        return Some(candidate);
    }
    if depth == 0 {
        return None;
    }
    std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .find_map(|dir| find_named(&dir, name, depth - 1))
}

fn run_checked(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let status = Process::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} failed");
    }
    Ok(())
}

fn replace_app(app: &Path, zip: &Path) -> Result<()> {
    let parent = app.parent().context("the app bundle has no parent")?;
    let staging = staging_dir(parent)?;
    let result = (|| {
        // ditto, not unzip: it keeps the bundle's symlinks and permissions.
        run_checked(
            "ditto",
            &[
                "-x".as_ref(),
                "-k".as_ref(),
                zip.as_os_str(),
                staging.as_os_str(),
            ],
        )?;
        let name = app.file_name().context("app has no name")?;
        let new_app = find_named(&staging, &name.to_string_lossy(), 0)
            .or_else(|| find_named(&staging, "Icarus.app", 0))
            .context("the download holds no Icarus.app")?;
        if !new_app.join("Contents/Info.plist").is_file() {
            bail!("the downloaded app has no Contents/Info.plist");
        }
        let _ = Process::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&new_app)
            .status();
        swap_into_place(app, &new_app)
    })();
    let _ = remove_path(&staging);
    result.map(|_| ())
}

fn replace_binaries(dir: &Path, tarball: &Path) -> Result<()> {
    let staging = staging_dir(dir)?;
    let result = (|| {
        run_checked(
            "tar",
            &[
                "-xzf".as_ref(),
                tarball.as_os_str(),
                "-C".as_ref(),
                staging.as_os_str(),
            ],
        )?;
        let found = ["icarus", "icarus-server"]
            .into_iter()
            .map(|bin| {
                find_named(&staging, bin, 2)
                    .map(|path| (bin, path))
                    .with_context(|| format!("the download holds no {bin}"))
            })
            .collect::<Result<Vec<_>>>()?;
        for (bin, path) in found {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            swap_into_place(&dir.join(bin), &path)?;
        }
        Ok(())
    })();
    let _ = remove_path(&staging);
    result
}

fn find_asset<'a>(check: &'a Value, name: &str) -> Result<&'a str> {
    check["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|asset| asset["name"] == name))
        .and_then(|asset| asset["browser_download_url"].as_str())
        .with_context(|| format!("the release has no {name}"))
}

fn download_verified(check: &Value, name: &str, dir: &Path) -> Result<PathBuf> {
    let tag = check["tag"].as_str().context("the release has no tag")?;
    let sums_path = download(tag, SUMS, find_asset(check, SUMS)?, dir)?;
    let sums = std::fs::read_to_string(&sums_path)?;
    let expected =
        expected_sha256(&sums, name).with_context(|| format!("{SUMS} does not list {name}"))?;
    let path = download(tag, name, find_asset(check, name)?, dir)?;
    let actual = sha256_file(&path)?;
    if actual != expected {
        bail!(
            "{name} failed its checksum (expected {expected}, got {actual}); nothing was changed"
        );
    }
    Ok(path)
}

/// Stops the server with its terminals parked, then starts the new one, which takes them back.
pub async fn restart_server(
    cli: &Cli,
    client: &Client,
    exe: &Path,
    server: &Path,
) -> Result<String> {
    let home = home(cli)?;
    let _ = client
        .request("server.shutdown", json!({ "keep_terminals": true }))
        .await;
    let deadline = Instant::now() + Duration::from_secs(30);
    // A server busy parking still holds its socket but no longer answers.
    while !matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            Client::connect(&home, "icarus-update")
        )
        .await,
        Ok(Err(_))
    ) {
        if Instant::now() > deadline {
            bail!(
                "the old server did not stop; restart it with `icarus server stop && icarus server start`"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if service::installed_for(&home) {
        service::install(server, &home)?;
        return Ok("restarted the login service".into());
    }
    let status = Process::new(exe)
        .arg("--home")
        .arg(&home)
        .args(["server", "start"])
        .stdout(Stdio::null())
        .status()
        .context("start the new server")?;
    if !status.success() {
        bail!(
            "the new server did not start; see {}",
            home.join("server.log").display()
        );
    }
    Ok("restarted the server".into())
}

async fn install(cli: &Cli, force: bool) -> Result<()> {
    if cli.server.is_some() {
        bail!("--server can't install updates; run `icarus update install` on that machine");
    }
    let client = connect(cli).await?;
    let check = client
        .request_timeout("update.check", json!({}), CHECK_TIMEOUT)
        .await?;
    let Some(latest) = check["latest"].as_str() else {
        bail!("no Icarus release is published yet");
    };
    if check["update_available"] != json!(true) && !force {
        print(cli, &json!({ "installed": false, "check": check }), |_| {
            describe(&check)
        });
        return Ok(());
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let server = exe.with_file_name("icarus-server");
    let method = detect(&exe)?;
    match &method {
        Method::Homebrew => {
            let status = Process::new("brew")
                .args(["upgrade", "--cask", "icarus"])
                .status()
                .context("run brew")?;
            if !status.success() {
                bail!("brew upgrade --cask icarus failed");
            }
        }
        Method::AppBundle(app) => {
            let name = asset_name(&method, latest).context("no asset for this install")?;
            let scratch = tempfile::tempdir()?;
            let zip = download_verified(&check, &name, scratch.path())?;
            replace_app(app, &zip)?;
        }
        Method::Binaries(dir) => {
            let name = asset_name(&method, latest).context("no asset for this install")?;
            let scratch = tempfile::tempdir()?;
            let tarball = download_verified(&check, &name, scratch.path())?;
            replace_binaries(dir, &tarball)?;
        }
    }
    let restarted = restart_server(cli, &client, &exe, &server).await?;
    let how = match &method {
        Method::Homebrew => "homebrew".to_owned(),
        Method::AppBundle(app) => app.display().to_string(),
        Method::Binaries(dir) => dir.display().to_string(),
    };
    print(
        cli,
        &json!({ "installed": true, "version": latest, "method": how, "server": restarted }),
        |_| {
            let mut text = format!("Installed Icarus {latest} ({how}); {restarted}.");
            if !matches!(method, Method::Binaries(_)) {
                text += "\nQuit and reopen the Icarus app to finish.";
            }
            text
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_are_read_in_sha256sum_format() {
        let sums = "ABC123  Icarus-0.2.0-macos-aarch64.zip\ndef456 *icarus-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            expected_sha256(sums, "Icarus-0.2.0-macos-aarch64.zip").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            expected_sha256(sums, "icarus-0.2.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_sha256(sums, "Icarus-0.2.0-macos-x86_64.zip"), None);
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("f");
        std::fs::write(&path, b"abc").expect("write");
        assert_eq!(
            sha256_file(&path).expect("hash"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn swap_keeps_the_previous_copy_and_replaces_it_next_time() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("Icarus.app");
        std::fs::create_dir_all(target.join("Contents")).expect("old");
        std::fs::write(target.join("Contents/v"), "1").expect("v1");
        for version in ["2", "3"] {
            let new = dir.path().join(format!("staged-{version}"));
            std::fs::create_dir_all(new.join("Contents")).expect("new");
            std::fs::write(new.join("Contents/v"), version).expect("v");
            let previous = swap_into_place(&target, &new).expect("swap");
            assert_eq!(
                std::fs::read_to_string(target.join("Contents/v")).expect("read"),
                version
            );
            assert!(previous.ends_with("Icarus.app.previous"));
            assert!(!new.exists());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Icarus.app.previous/Contents/v"))
                .expect("previous"),
            "2"
        );
    }

    #[test]
    fn a_failed_swap_puts_the_old_copy_back() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("icarus");
        std::fs::write(&target, "old").expect("old");
        let missing = dir.path().join("not-there");
        assert!(swap_into_place(&target, &missing).is_err());
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "old");
    }

    #[test]
    fn swap_installs_where_nothing_was() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("icarus-server");
        let new = dir.path().join("new");
        std::fs::write(&new, "new").expect("new");
        swap_into_place(&target, &new).expect("swap");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "new");
    }

    #[test]
    fn the_enclosing_app_bundle_is_found_from_its_cli() {
        let dir = tempfile::tempdir().expect("dir");
        let app = dir.path().join("Icarus.app");
        let exe = app.join("Contents/MacOS/icarus");
        std::fs::create_dir_all(exe.parent().expect("parent")).expect("mkdir");
        assert_eq!(enclosing_app(&exe), Some(app));
        assert_eq!(enclosing_app(&dir.path().join("bin/icarus")), None);
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        let app = Method::AppBundle(PathBuf::from("/Applications/Icarus.app"));
        assert_eq!(
            asset_name(&app, "0.2.0"),
            Some(format!("Icarus-0.2.0-macos-{}.zip", std::env::consts::ARCH))
        );
        let bins =
            asset_name(&Method::Binaries(PathBuf::from("/usr/local/bin")), "0.2.0").expect("name");
        assert!(bins.starts_with("icarus-0.2.0-") && bins.ends_with(".tar.gz"));
        assert_eq!(asset_name(&Method::Homebrew, "0.2.0"), None);
    }

    #[test]
    fn binaries_are_swapped_out_of_a_release_tarball() {
        let dir = tempfile::tempdir().expect("dir");
        let install = dir.path().join("bin");
        std::fs::create_dir_all(&install).expect("bin");
        std::fs::write(install.join("icarus"), "old cli").expect("old");
        let packed = dir.path().join("icarus-0.2.0-test");
        std::fs::create_dir_all(&packed).expect("packed");
        std::fs::write(packed.join("icarus"), "new cli").expect("cli");
        std::fs::write(packed.join("icarus-server"), "new server").expect("server");
        let tarball = dir.path().join("release.tar.gz");
        let status = Process::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(dir.path())
            .arg("icarus-0.2.0-test")
            .status()
            .expect("tar");
        assert!(status.success());
        replace_binaries(&install, &tarball).expect("replace");
        assert_eq!(
            std::fs::read_to_string(install.join("icarus")).expect("cli"),
            "new cli"
        );
        assert_eq!(
            std::fs::read_to_string(install.join("icarus-server")).expect("server"),
            "new server"
        );
        assert_eq!(
            std::fs::read_to_string(install.join("icarus.previous")).expect("previous"),
            "old cli"
        );
        assert!(std::fs::read_dir(&install).expect("list").all(|e| {
            !e.expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".icarus-update")
        }));
    }

    #[test]
    fn describe_covers_newer_current_and_unpublished() {
        let newer = json!({ "current": "0.1.0", "latest": "0.2.0", "update_available": true, "url": "u", "notes": "n" });
        assert!(describe(&newer).starts_with("Icarus 0.2.0 is available (you have 0.1.0)."));
        assert!(describe(&newer).ends_with("\n\nn"));
        let same = json!({ "current": "0.2.0", "latest": "0.2.0", "update_available": false });
        assert_eq!(
            describe(&same),
            "Icarus 0.2.0 is up to date (latest release: 0.2.0)."
        );
        let none = json!({ "current": "0.1.0", "latest": null, "update_available": false });
        assert_eq!(
            describe(&none),
            "No Icarus release is published yet (you have 0.1.0)."
        );
    }
}
