//! `icarus terminal attach` driven through a real pty, as the app's terminal tabs run it.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;

struct Home {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.icarus(&["server", "stop"]);
    }
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("home");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config");
        let home = Self { dir, config };
        home.ok(&["server", "start"]);
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_icarus"));
        command
            .arg("--home")
            .arg(self.path())
            .args(args)
            .env("ICARUS_CONFIG_DIR", &self.config)
            .env("ICARUS_GH_FIXTURES", self.path().join("no-gh"))
            .env("ICARUS_HANDOFF_HOLD_SECS", "30");
        command
    }

    fn icarus(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("icarus")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.icarus(args);
        assert!(
            output.status.success(),
            "icarus {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        serde_json::from_str(&self.ok(&full)).expect("json")
    }
}

/// `icarus terminal attach …` running inside a pty, with everything it prints collected.
struct Attached {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Attached {
    fn spawn(home: &Home, args: &[&str]) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_icarus"));
        command.arg("--home");
        command.arg(home.path());
        command.args(args);
        command.env("ICARUS_CONFIG_DIR", &home.config);
        command.env("ICARUS_GH_FIXTURES", home.path().join("no-gh"));
        let child = pty.slave.spawn_command(command).expect("spawn attach");
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().expect("reader");
        let writer = pty.master.take_writer().expect("writer");
        let output = Arc::new(Mutex::new(Vec::new()));
        std::thread::spawn({
            let output = output.clone();
            move || {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = reader.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    output
                        .lock()
                        .expect("output")
                        .extend_from_slice(&buffer[..read]);
                }
            }
        });
        Self {
            master: pty.master,
            writer,
            child,
            output,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("output")).into_owned()
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never saw {needle:?} in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r").as_bytes())
            .expect("type");
        self.writer.flush().expect("flush");
    }

    fn wait_exit(&mut self) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "attach did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

const ECHO_LOOP: &str = r#"stty size; while IFS= read -r line; do echo "got:$line"; if [ "$line" = done ]; then exit 7; fi; done"#;

fn only_terminal(home: &Home) -> Value {
    let list = home.json(&["terminal", "list"]);
    let items = list.as_array().expect("list");
    assert_eq!(items.len(), 1, "{list}");
    items[0].clone()
}

#[test]
fn attach_types_resizes_survives_a_server_restart_and_ends_when_the_shell_does() {
    let home = Home::new();
    let cwd = tempfile::tempdir().expect("cwd");
    let cwd_text = cwd.path().to_str().expect("utf8").to_owned();
    let mut attached = Attached::spawn(
        &home,
        &[
            "terminal", "attach", "--create", "--cwd", &cwd_text, "--", "sh", "-c", ECHO_LOOP,
        ],
    );
    attached.wait_for("24 80");

    attached.type_line("hello");
    attached.wait_for("got:hello");

    attached
        .master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = only_terminal(&home);
        if info["columns"] == 100 && info["rows"] == 30 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resize never reached the server: {info}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let pid = only_terminal(&home)["pid"].clone();
    home.ok(&["server", "restart"]);
    attached.type_line("after-restart");
    attached.wait_for("got:after-restart");
    assert_eq!(
        only_terminal(&home)["pid"],
        pid,
        "the same process kept running"
    );
    assert_eq!(
        attached.text().matches("got:hello").count(),
        1,
        "resuming by sequence repeats nothing"
    );

    attached.type_line("done");
    // An adopted process isn't the new server's child, so its exit code is unknown; attach still ends.
    attached.wait_exit();
}

#[test]
fn attaching_to_an_existing_terminal_replays_its_screen_and_leaves_it_running() {
    let home = Home::new();
    let cwd = tempfile::tempdir().expect("cwd");
    let created = home.json(&[
        "terminal",
        "create",
        "--cwd",
        cwd.path().to_str().expect("utf8"),
        "--",
        "sh",
        "-c",
        r#"echo before-attach; while IFS= read -r line; do echo "got:$line"; if [ "$line" = done ]; then exit 7; fi; done"#,
    ]);
    let id = created["id"].as_str().expect("id").to_owned();
    std::thread::sleep(Duration::from_millis(300));

    let mut first = Attached::spawn(&home, &["terminal", "attach", &id]);
    first.wait_for("before-attach");
    first.type_line("one");
    first.wait_for("got:one");
    first.child.kill().expect("close the viewer");
    first.wait_exit();

    let info = only_terminal(&home);
    assert_eq!(info["id"], id.as_str());
    assert_eq!(
        info["running"], true,
        "closing a viewer leaves the shell: {info}"
    );

    let mut second = Attached::spawn(&home, &["terminal", "attach", &id]);
    second.wait_for("got:one");
    second.type_line("done");
    assert_eq!(
        second.wait_exit(),
        7,
        "attach ends with the shell's exit code"
    );
}

#[test]
fn attaching_to_an_unknown_terminal_fails_clearly() {
    let home = Home::new();
    let output = home.icarus(&["terminal", "attach", "term-nope"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("term-nope"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
