//! `icarus settings import-zed` on temp copies of a Zed install; it needs no running server.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, text).expect("write");
}

fn icarus(home: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_icarus"));
    command
        .args(["--home"])
        .arg(home.join("no-server"))
        .args(args)
        .env_remove("ICARUS_ZED_CONFIG_DIR")
        .env_remove("ICARUS_ZED_SOURCE_DIR")
        .env_remove("ICARUS_ZED_SOURCE_DATA_DIR");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("run icarus");
    assert!(
        output.status.success(),
        "icarus {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn imports_a_zed_install_once_and_reports_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let zed = tmp.path().join("zed");
    let data = tmp.path().join("zed-data");
    let app = tmp.path().join("icarus");
    write(
        &zed.join("settings.json"),
        "// mine\n{\"theme\": \"Gruvbox Dark\", \"vim_mode\": true, \"agent\": {\"dock\": \"left\"},}\n",
    );
    write(&zed.join("keymap.json"), r#"[{"bindings":{"cmd-k":"a"}}]"#);
    write(
        &data.join("extensions/installed/gruvbox/extension.toml"),
        "id = \"gruvbox\"\n",
    );
    let from = zed.to_str().expect("utf8");
    let from_data = data.to_str().expect("utf8");
    let zed_dir = app.to_str().expect("utf8");
    let base = [
        "settings",
        "import-zed",
        "--from",
        from,
        "--from-data",
        from_data,
        "--zed-dir",
        zed_dir,
    ];

    let dry = stdout(&icarus(
        tmp.path(),
        &[&base[..], &["--dry-run"]].concat(),
        &[],
    ));
    assert!(dry.contains("would be added"), "{dry}");
    assert!(
        dry.contains("not imported (Icarus needs it): agent.dock"),
        "{dry}"
    );
    assert!(dry.contains("gruvbox"), "{dry}");
    assert!(dry.contains("keymap keymap.json: would copy"), "{dry}");
    assert!(!app.exists());

    let report: Value = serde_json::from_str(&stdout(&icarus(
        tmp.path(),
        &[&base[..], &["--json"]].concat(),
        &[],
    )))
    .expect("json");
    assert_eq!(report["extensions_added"], serde_json::json!(["gruvbox"]));
    assert_eq!(report["appearance"]["theme"], "Gruvbox Dark");
    assert_eq!(report["files"][0]["status"], "copied");
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(app.join("settings.json")).expect("settings"))
            .expect("plain json");
    assert_eq!(settings["vim_mode"], true);
    assert_eq!(settings["auto_install_extensions"]["gruvbox"], true);
    assert!(settings.get("agent").is_none());
    assert!(app.join("keymap.json").exists());

    let again = stdout(&icarus(tmp.path(), &base, &[]));
    assert!(again.contains("already up to date"), "{again}");
    assert!(
        again.contains("keymap keymap.json: already imported"),
        "{again}"
    );
}

#[test]
fn defaults_come_from_the_environment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let zed = tmp.path().join("zed");
    let app = tmp.path().join("icarus");
    write(&zed.join("settings.json"), r#"{"tab_size": 2}"#);
    let empty = tmp.path().join("no-data");
    let out = stdout(&icarus(
        tmp.path(),
        &["settings", "import-zed"],
        &[
            ("ICARUS_ZED_SOURCE_DIR", &zed),
            ("ICARUS_ZED_SOURCE_DATA_DIR", &empty),
            ("ICARUS_ZED_CONFIG_DIR", &app),
        ],
    ));
    assert!(out.contains("1 keys added"), "{out}");
    assert!(
        fs::read_to_string(app.join("settings.json"))
            .expect("settings")
            .contains("tab_size")
    );
}
