use std::fs;
use std::path::{Path, PathBuf};

use icarus_settings::json::{self, Value};
use icarus_settings::layers::{self, KeyPath};
use icarus_settings::zed::ApplyError;
use icarus_settings::zed_import::{self, FileStatus, ZedImport, import};

const ZED_SETTINGS: &str = r#"// Zed settings
{
  "theme": { "mode": "system", "light": "One Light", "dark": "Catppuccin Mocha" },
  "icon_theme": "Catppuccin Mocha", // trailing comment
  "buffer_font_family": "JetBrains Mono",
  "buffer_font_size": 14,
  "vim_mode": true,
  /* block comment */
  "agent": { "dock": "left", "default_model": { "provider": "anthropic", "model": "claude" } },
  "disable_ai": true,
  "agent_servers": { "Icarus": { "command": "elsewhere" }, "Other": { "command": "other" } },
  "auto_install_extensions": { "ruff": false },
}
"#;

struct Dirs {
    _tmp: tempfile::TempDir,
    source: PathBuf,
    data: PathBuf,
    target: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Self {
            source: tmp.path().join("zed-config"),
            data: tmp.path().join("zed-data"),
            target: tmp.path().join("icarus-config"),
            _tmp: tmp,
        };
        fs::create_dir_all(&dirs.source).expect("source");
        dirs
    }

    fn write(&self, path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(path, text).expect("write");
    }

    fn extension(&self, dir: &str, id: &str) {
        self.write(
            self.data.join("extensions/installed").join(dir).join("extension.toml"),
            &format!("id = \"{id}\"\nname = \"{id} name\"\nversion = \"1.2.3\"\n\n[grammars.x]\nid = \"nested\"\n"),
        );
    }

    fn run(&self, dry_run: bool) -> ZedImport {
        import(&self.source, Some(&self.data), &self.target, dry_run).expect("import")
    }

    fn target_settings(&self) -> Value {
        let text = fs::read_to_string(self.target.join("settings.json")).expect("settings");
        json::parse(&text).expect("plain json")
    }
}

fn get<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    layers::get(doc, &KeyPath::parse(key).expect("key"))
}

fn doc(text: &str) -> Value {
    json::parse(text).expect("json")
}

fn statuses(import: &ZedImport, kind: &str) -> Vec<(String, FileStatus)> {
    import
        .files
        .iter()
        .filter(|file| file.kind == kind)
        .map(|file| (file.name.clone(), file.status))
        .collect()
}

#[test]
fn commented_settings_are_imported_without_the_keys_icarus_owns() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    let result = dirs.run(false);
    let settings = dirs.target_settings();

    assert_eq!(get(&settings, "vim_mode"), Some(&Value::Bool(true)));
    assert_eq!(
        get(&settings, "theme.dark"),
        Some(&doc(r#""Catppuccin Mocha""#))
    );
    assert_eq!(get(&settings, "buffer_font_size"), Some(&doc("14")));
    assert_eq!(
        get(&settings, "agent.default_model.model"),
        Some(&doc(r#""claude""#))
    );
    assert_eq!(
        get(&settings, "agent_servers.Other.command"),
        Some(&doc(r#""other""#))
    );
    assert!(get(&settings, "agent.dock").is_none());
    assert!(get(&settings, "disable_ai").is_none());
    assert!(get(&settings, "agent_servers.Icarus").is_none());
    assert_eq!(
        result.protected,
        ["agent.dock", "disable_ai", "agent_servers.Icarus"]
    );
    assert_eq!(
        result.appearance.keys().collect::<Vec<_>>(),
        [
            "theme",
            "icon_theme",
            "buffer_font_family",
            "buffer_font_size"
        ]
    );
    assert!(result.added.contains(&"vim_mode".to_owned()));
    assert!(result.kept.is_empty());
    assert!(result.headline().contains("settings"));
    assert!(result.headline().contains("theme"));
}

#[test]
fn a_real_import_is_remembered_and_a_dry_run_is_not() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    dirs.run(true);
    assert!(!zed_import::already_imported(&dirs.target));
    dirs.run(false);
    assert!(zed_import::already_imported(&dirs.target));
}

#[test]
fn the_explorer_dock_is_not_imported() {
    let dirs = Dirs::new();
    dirs.write(
        dirs.source.join("settings.json"),
        r#"{"project_panel":{"dock":"left","indent_size":12}}"#,
    );
    let result = dirs.run(false);
    assert_eq!(result.protected, ["project_panel.dock"]);
    assert_eq!(
        dirs.target_settings(),
        doc(r#"{"project_panel":{"indent_size":12}}"#)
    );
}

#[test]
fn a_protected_key_alone_leaves_no_empty_parent() {
    let dirs = Dirs::new();
    dirs.write(
        dirs.source.join("settings.json"),
        r#"{"agent":{"dock":"left"},"tab_size":2}"#,
    );
    dirs.run(false);
    assert_eq!(dirs.target_settings(), doc(r#"{"tab_size":2}"#));
}

#[test]
fn keys_already_in_the_icarus_settings_win() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    let icarus = "// mine\n{\"vim_mode\": false, \"agent_servers\": {\"Icarus\": {\"command\": \"icarus\"}}}\n";
    dirs.write(dirs.target.join("settings.json"), icarus);
    let result = dirs.run(false);
    let settings = dirs.target_settings();

    assert_eq!(get(&settings, "vim_mode"), Some(&Value::Bool(false)));
    assert_eq!(
        get(&settings, "agent_servers.Icarus.command"),
        Some(&doc(r#""icarus""#))
    );
    assert_eq!(result.kept, ["vim_mode"]);
    let backup = result.backup.expect("commented file backed up");
    assert_eq!(fs::read_to_string(backup).expect("backup"), icarus);
}

#[cfg(unix)]
#[test]
fn installed_extensions_become_auto_installs_and_dev_extensions_are_reported() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    dirs.extension("catppuccin", "catppuccin");
    dirs.extension("ruff", "ruff");
    dirs.extension("toml-folder", "toml");
    dirs.extension("marked-dev", "marked-dev");
    let checkout = dirs.data.join("checkouts/my-ext");
    dirs.write(checkout.join("extension.toml"), "id = \"my-ext\"\n");
    std::os::unix::fs::symlink(&checkout, dirs.data.join("extensions/installed/my-ext"))
        .expect("symlink");
    dirs.write(
        dirs.data.join("extensions/index.json"),
        r#"{"extensions":{"marked-dev":{"manifest":{"id":"marked-dev"},"dev":true},"ruff":{"dev":false}}}"#,
    );

    let result = dirs.run(false);
    let settings = dirs.target_settings();

    let ids: Vec<&str> = result
        .extensions
        .iter()
        .map(|ext| ext.id.as_str())
        .collect();
    assert_eq!(ids, ["catppuccin", "ruff", "toml"]);
    assert_eq!(result.extensions[0].version.as_deref(), Some("1.2.3"));
    assert_eq!(
        get(&settings, "auto_install_extensions"),
        Some(&doc(r#"{"ruff":false,"catppuccin":true,"toml":true}"#))
    );
    let dev: Vec<(&str, &Path)> = result
        .dev_extensions
        .iter()
        .map(|dev| (dev.id.as_str(), dev.path.as_path()))
        .collect();
    assert_eq!(dev.len(), 2);
    assert!(dev.contains(&("my-ext", checkout.as_path())));
    assert!(dev.iter().any(|(id, _)| *id == "marked-dev"));
    assert!(!fs::exists(dirs.target.join("extensions")).expect("exists"));
    assert!(result.headline().contains("3 extensions"));
}

#[test]
fn the_extension_index_is_read_when_no_installed_folder_exists() {
    let dirs = Dirs::new();
    dirs.write(
        dirs.data.join("extensions/index.json"),
        r#"{"extensions":{"html":{"manifest":{"id":"html","name":"HTML","version":"0.3.2"},"dev":false},"local":{"dev":true}}}"#,
    );
    let result = dirs.run(false);
    assert_eq!(result.extensions.len(), 1);
    assert_eq!(result.extensions[0].name.as_deref(), Some("HTML"));
    assert_eq!(result.extensions_added, ["html"]);
    assert_eq!(result.dev_extensions[0].id, "local");
}

#[test]
fn an_icarus_auto_install_list_gets_the_missing_ids() {
    let dirs = Dirs::new();
    dirs.extension("ruff", "ruff");
    dirs.extension("toml", "toml");
    dirs.write(
        dirs.target.join("settings.json"),
        r#"{"auto_install_extensions":{"ruff":false}}"#,
    );
    let result = dirs.run(false);
    assert_eq!(result.extensions_added, ["toml"]);
    assert_eq!(
        get(&dirs.target_settings(), "auto_install_extensions"),
        Some(&doc(r#"{"ruff":false,"toml":true}"#))
    );
}

#[test]
fn keymap_snippets_and_themes_are_copied_when_icarus_has_none() {
    let dirs = Dirs::new();
    dirs.write(
        dirs.source.join("keymap.json"),
        r#"[{"bindings":{"cmd-k":"workspace::Save"}}]"#,
    );
    dirs.write(dirs.source.join("snippets/rust.json"), "{}");
    dirs.write(dirs.source.join("themes/mine.json"), r#"{"name":"Mine"}"#);
    dirs.write(dirs.source.join("themes/.DS_Store"), "");
    // Zed's own commented-out keymap template and the Orca import's theme aren't the user's own.
    dirs.write(
        dirs.target.join("keymap.json"),
        "[\n  {\n    \"context\": \"Workspace\",\n    \"bindings\": {\n      // \"shift shift\": \"file_finder::Toggle\"\n    }\n  }\n]\n",
    );
    dirs.write(dirs.target.join("themes/icarus-imported.json"), "{}");

    let result = dirs.run(false);
    assert_eq!(
        statuses(&result, "keymap"),
        [("keymap.json".into(), FileStatus::Copied)]
    );
    assert_eq!(
        statuses(&result, "snippet"),
        [("rust.json".into(), FileStatus::Copied)]
    );
    assert_eq!(
        statuses(&result, "theme"),
        [("mine.json".into(), FileStatus::Copied)]
    );
    assert_eq!(
        fs::read_to_string(dirs.target.join("keymap.json")).expect("keymap"),
        r#"[{"bindings":{"cmd-k":"workspace::Save"}}]"#
    );
    assert!(fs::exists(dirs.target.join("themes/mine.json")).expect("exists"));
}

#[test]
fn icarus_files_of_its_own_are_never_replaced() {
    let dirs = Dirs::new();
    dirs.write(
        dirs.source.join("keymap.json"),
        r#"[{"bindings":{"cmd-k":"a"}}]"#,
    );
    dirs.write(dirs.source.join("snippets/rust.json"), "{}");
    dirs.write(dirs.source.join("snippets/go.json"), "{}");
    dirs.write(
        dirs.target.join("keymap.json"),
        r#"[{"bindings":{"cmd-j":"b"}}]"#,
    );
    dirs.write(dirs.target.join("snippets/python.json"), "{}");

    let result = dirs.run(false);
    assert_eq!(
        statuses(&result, "keymap"),
        [("keymap.json".into(), FileStatus::Skipped)]
    );
    assert_eq!(
        statuses(&result, "snippet"),
        [
            ("go.json".into(), FileStatus::Skipped),
            ("rust.json".into(), FileStatus::Skipped)
        ]
    );
    assert_eq!(
        fs::read_to_string(dirs.target.join("keymap.json")).expect("keymap"),
        r#"[{"bindings":{"cmd-j":"b"}}]"#
    );
    assert!(!fs::exists(dirs.target.join("snippets/rust.json")).expect("exists"));
}

#[test]
fn an_empty_zed_keymap_is_not_imported() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("keymap.json"), "[\n  // nothing yet\n]\n");
    let result = dirs.run(false);
    assert!(result.files.is_empty());
    assert!(!result.found_anything());
}

#[test]
fn importing_twice_changes_nothing_the_second_time() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    dirs.write(
        dirs.source.join("keymap.json"),
        r#"[{"bindings":{"cmd-k":"a"}}]"#,
    );
    dirs.write(dirs.source.join("snippets/rust.json"), "{}");
    dirs.extension("ruff", "ruff");
    dirs.extension("toml", "toml");
    let first = dirs.run(false);
    assert!(first.changes_anything());
    let settings = fs::read_to_string(dirs.target.join("settings.json")).expect("settings");

    let second = dirs.run(false);
    assert!(!second.changes_anything(), "{:?}", second.summary_lines());
    assert!(second.added.is_empty());
    assert!(second.extensions_added.is_empty());
    assert!(
        second
            .files
            .iter()
            .all(|file| file.status == FileStatus::AlreadyImported)
    );
    assert_eq!(
        fs::read_to_string(dirs.target.join("settings.json")).expect("settings"),
        settings
    );
    assert!(
        second
            .summary_lines()
            .iter()
            .any(|line| line == "already up to date")
    );

    // A snippet added to Zed later still comes over: the earlier copies aren't Icarus's own.
    dirs.write(dirs.source.join("snippets/go.json"), "[]");
    let third = dirs.run(false);
    assert_eq!(
        statuses(&third, "snippet"),
        [
            ("go.json".into(), FileStatus::Copied),
            ("rust.json".into(), FileStatus::AlreadyImported)
        ]
    );
}

#[test]
fn a_dry_run_reports_and_writes_nothing() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), ZED_SETTINGS);
    dirs.write(
        dirs.source.join("keymap.json"),
        r#"[{"bindings":{"cmd-k":"a"}}]"#,
    );
    dirs.extension("ruff", "ruff");
    let result = dirs.run(true);
    assert!(result.changes_anything());
    assert!(result.added.contains(&"vim_mode".to_owned()));
    assert_eq!(
        statuses(&result, "keymap"),
        [("keymap.json".into(), FileStatus::WouldCopy)]
    );
    assert!(!fs::exists(&dirs.target).expect("exists"));
    assert!(
        result
            .summary_lines()
            .iter()
            .any(|line| line == "dry run: nothing written")
    );
}

#[test]
fn a_missing_zed_install_imports_nothing() {
    let dirs = Dirs::new();
    let result = dirs.run(false);
    assert!(!result.found_anything());
    assert!(!fs::exists(&dirs.target).expect("exists"));
    assert_eq!(result.summary_lines().len(), 1);
}

#[test]
fn broken_zed_settings_stop_the_import() {
    let dirs = Dirs::new();
    dirs.write(dirs.source.join("settings.json"), "{ \"vim_mode\": ");
    let error = import(&dirs.source, None, &dirs.target, false).expect_err("invalid");
    assert!(matches!(error, ApplyError::Parse { .. }));
    assert!(!fs::exists(&dirs.target).expect("exists"));
}
