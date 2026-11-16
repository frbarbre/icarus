//! `icarus settings import-keybindings` on made-up Orca keybindings; it needs no running server.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn icarus(home: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_icarus"))
        .args(["--home"])
        .arg(home.join("no-server"))
        .arg("--json")
        .args(args)
        .env_remove("ICARUS_ZED_CONFIG_DIR")
        .output()
        .expect("run icarus");
    assert!(
        output.status.success(),
        "icarus {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json output")
}

#[test]
fn imports_orca_keybindings_into_the_app_keymap_and_reimports_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("keybindings.json");
    fs::write(
        &source,
        json!({
            "version": 1,
            "keybindings": {},
            "platforms": { "darwin": {
                "editor.nextChange": ["ArrowDown"],
                "tab.closeOthers": ["Ctrl+W"],
                "tab.rename": [],
            } },
        })
        .to_string(),
    )
    .expect("write source");
    let zed_dir = dir.path().join("zed");
    let source_arg = source.to_str().expect("utf8");
    let zed_arg = zed_dir.to_str().expect("utf8");

    let dry = icarus(
        dir.path(),
        &[
            "settings",
            "import-keybindings",
            source_arg,
            "--platform",
            "mac",
            "--zed-dir",
            zed_arg,
            "--dry-run",
        ],
    );
    assert_eq!(dry["dry_run"], true);
    assert!(
        !zed_dir.join("keymap.json").exists(),
        "a dry run writes nothing"
    );

    let applied = icarus(
        dir.path(),
        &[
            "settings",
            "import-keybindings",
            source_arg,
            "--platform",
            "mac",
            "--zed-dir",
            zed_arg,
        ],
    );
    let mapped: Vec<&str> = applied["translation"]["mapped"]
        .as_array()
        .expect("mapped")
        .iter()
        .filter_map(|m| m["orca"].as_str())
        .collect();
    assert_eq!(mapped, ["editor.nextChange", "tab.closeOthers"]);
    assert_eq!(applied["translation"]["skipped"][0]["orca"], "tab.rename");
    let keymap: Value =
        serde_json::from_str(&fs::read_to_string(zed_dir.join("keymap.json")).expect("keymap"))
            .expect("json");
    let sections = keymap.as_array().expect("sections");
    assert_eq!(sections[0]["unbind"]["f7"], "icarus::NextChange");
    assert!(
        sections
            .iter()
            .any(|s| s["context"] == "IcarusDiffTab && diff_mode > Editor"
                && s["bindings"]["down"] == "icarus::NextChange")
    );

    let again = icarus(
        dir.path(),
        &[
            "settings",
            "import-keybindings",
            source_arg,
            "--platform",
            "mac",
            "--zed-dir",
            zed_arg,
        ],
    );
    assert_eq!(again["applied"]["replaced"], applied["applied"]["added"]);
    let after: Value =
        serde_json::from_str(&fs::read_to_string(zed_dir.join("keymap.json")).expect("keymap"))
            .expect("json");
    assert_eq!(
        after, keymap,
        "importing the same file again changes nothing"
    );
}

#[test]
fn reads_the_keybindings_embedded_in_an_orca_settings_export() {
    let dir = tempfile::tempdir().expect("tempdir");
    let inner =
        json!({ "version": 1, "platforms": { "linux": { "editor.nextFile": ["Mod+ArrowDown"] } } });
    let export = json!({
        "format": "orca-settings",
        "version": 1,
        "exportedAt": "",
        "settings": {},
        "keybindings": inner.to_string(),
    });
    let source = dir.path().join("export.json");
    fs::write(&source, export.to_string()).expect("write");
    let zed_dir = dir.path().join("zed");
    let result = icarus(
        dir.path(),
        &[
            "settings",
            "import-keybindings",
            source.to_str().expect("utf8"),
            "--platform",
            "linux",
            "--zed-dir",
            zed_dir.to_str().expect("utf8"),
            "--dry-run",
        ],
    );
    assert_eq!(
        result["translation"]["mapped"][0]["keystrokes"],
        json!(["ctrl-down"])
    );
}
