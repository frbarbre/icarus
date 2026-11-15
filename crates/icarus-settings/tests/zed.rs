mod common;

use common::fixture;
use icarus_settings::import::import_orca_text;
use icarus_settings::json::{self, Value};
use icarus_settings::layers::{self, KeyPath, LayerStack};
use icarus_settings::zed;

fn doc(text: &str) -> Value {
    json::parse(text).expect("json")
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    layers::get(value, &KeyPath::parse(key).expect("key"))
}

fn export_with(settings: &str) -> String {
    format!(r#"{{"format":"orca-settings","version":1,"exportedAt":"","settings":{settings}}}"#)
}

#[test]
fn orca_defaults_become_zed_extensions_and_language_settings() {
    let import = import_orca_text(&export_with("{}")).expect("valid");
    let zed = &import.zed;
    assert_eq!(
        get(zed, "auto_install_extensions"),
        Some(&doc(r#"{"tsgo":true,"oxc":true}"#))
    );
    assert_eq!(
        get(zed, "languages.Python"),
        Some(&doc(
            r#"{"language_servers":["pyrefly","ruff","!basedpyright","!pylsp","!pyright","!ty","..."],
                "formatter":{"language_server":{"name":"ruff"}},"format_on_save":"on"}"#
        ))
    );
    for language in ["TypeScript", "TSX", "JavaScript"] {
        assert_eq!(
            get(zed, &format!("languages.{language}.language_servers")),
            Some(&doc(
                r#"["typescript-ls","oxlint","oxfmt","!eslint","!typescript-language-server","!vtsls","..."]"#
            )),
            "{language}"
        );
    }
    assert!(get(zed, "lsp").is_none());
    assert!(get(&import.user, "languages").is_none());
    assert!(
        import.report.validation.is_empty(),
        "{:?}",
        import.report.validation
    );
}

#[test]
fn non_default_choices_and_commands_carry_over() {
    let import = import_orca_text(&export_with(
        r#"{
            "languageServers": {"python": {"enabled": true, "server": "pyright", "command": " /opt/bin/pyright-langserver "}, "typescript": false},
            "formatters": {"ruff": {"enabled": false}, "oxfmt": {"command": "/opt/bin/oxfmt"}},
            "linters": {"oxlint": {"enabled": true}}
        }"#,
    ))
    .expect("valid");
    let zed = &import.zed;
    assert_eq!(
        get(zed, "languages.Python"),
        Some(&doc(
            r#"{"language_servers":["pyright","!basedpyright","!pylsp","!pyrefly","!ruff","!ty","..."]}"#
        ))
    );
    assert_eq!(
        get(zed, "languages.TSX.language_servers"),
        Some(&doc(
            r#"["oxlint","oxfmt","!eslint","!typescript-language-server","!typescript-ls","!vtsls","..."]"#
        ))
    );
    assert_eq!(
        get(zed, "auto_install_extensions"),
        Some(&doc(r#"{"oxc":true}"#))
    );
    assert_eq!(
        get(zed, "lsp.pyright.binary"),
        Some(&doc(
            r#"{"path":"/opt/bin/pyright-langserver","arguments":["--stdio"]}"#
        ))
    );
    assert_eq!(
        get(zed, "lsp.oxfmt.binary.path"),
        Some(&Value::from("/opt/bin/oxfmt"))
    );
    assert!(
        import
            .report
            .mapped
            .iter()
            .any(|m| m.from == "settings.languageServers.python" && m.to == "zed: languages.Python")
    );
    assert!(
        !import
            .report
            .unmapped
            .iter()
            .any(|key| key == "settings.languageServers" || key == "settings.formatters")
    );
}

#[test]
fn default_appearance_follows_the_system_with_zed_themes() {
    let rendered = zed::render(&LayerStack::with_defaults().resolve());
    assert_eq!(
        rendered.settings,
        doc(r#"{"theme":{"mode":"system","light":"One Light","dark":"One Dark"}}"#)
    );
    assert!(rendered.theme_family.is_none());
}

#[test]
fn vscode_themes_convert_to_zed_style() {
    let style = Value::Object(zed::vscode_style(&doc(&fixture("vscode-theme.json"))));
    let expect = |key: &str, value: &str| {
        assert_eq!(style.get(key), Some(&doc(value)), "{key}");
    };
    expect("editor.background", r##""#1e1e1e""##);
    expect("editor.gutter.background", r##""#1e1e1e""##);
    expect("editor.foreground", r##""#dddddd""##);
    expect("text", r##""#dddddd""##);
    expect("panel.background", r##""#252526""##);
    expect("status_bar.background", r##""#007acc""##);
    expect("terminal.ansi.red", r##""#cd3131""##);
    expect("terminal.ansi.bright_blue", r##""#3b8eea""##);
    expect(
        "players",
        r##"[{"cursor":"#ffcc00","background":"#ffcc00","selection":"#264f7880"}]"##,
    );
    assert!(style.get("error").is_none(), "invalid colours are skipped");
    let syntax = style.get("syntax").expect("syntax");
    assert_eq!(
        syntax.get("comment"),
        Some(&doc(r##"{"color":"#6a9956","font_style":"italic"}"##)),
        "the last rule for a scope wins"
    );
    assert_eq!(syntax.get("string"), Some(&doc(r##"{"color":"#ce9178"}"##)));
    assert_eq!(
        syntax.get("keyword"),
        Some(&doc(r##"{"color":"#c586c0","font_weight":700}"##)),
        "a nested scope stands in for its parent"
    );
    assert_eq!(
        syntax.get("function"),
        Some(&doc(r##"{"color":"#dcdcaa"}"##))
    );
}

#[test]
fn an_orca_export_becomes_zed_settings_and_an_imported_theme() {
    let import = import_orca_text(&fixture("full-export.json")).expect("valid");
    let settings = &import.zed;
    assert_eq!(
        get(settings, "theme"),
        Some(&doc(
            r#"{"mode":"dark","light":"Icarus Imported Light","dark":"Icarus Imported Dark"}"#
        ))
    );
    assert_eq!(get(settings, "ui_font_family"), Some(&Value::from("Geist")));
    assert_eq!(
        get(settings, "buffer_font_family"),
        Some(&Value::from("Example Mono"))
    );
    assert_eq!(
        get(settings, "terminal"),
        Some(&doc(
            r#"{"font_family":"Example Mono","font_size":13,"line_height":{"custom":1.2}}"#
        ))
    );
    assert_eq!(
        get(settings, "active_pane_modifiers.inactive_opacity"),
        Some(&doc("0.85"))
    );
    assert!(get(settings, "languages.Python").is_some());

    let family = import.zed_theme.as_ref().expect("imported theme");
    assert_eq!(
        get(family, "$schema"),
        Some(&Value::from(zed::ZED_THEME_SCHEMA))
    );
    let themes = family
        .get("themes")
        .and_then(Value::as_array)
        .expect("themes");
    let theme = |appearance: &str| {
        themes
            .iter()
            .find(|t| t.get("appearance").and_then(Value::as_str) == Some(appearance))
            .unwrap_or_else(|| panic!("{appearance} theme"))
    };
    let dark = theme("dark");
    assert_eq!(
        get(dark, "name"),
        Some(&Value::from("Icarus Imported Dark"))
    );
    let style = dark.get("style").expect("style");
    assert_eq!(style.get("editor.background"), Some(&doc(r##""#101010""##)));
    assert_eq!(
        get(style, "syntax.comment"),
        Some(&doc(r#"{"font_style":"italic"}"#))
    );
    assert_eq!(
        style.get("terminal.background"),
        Some(&doc(r##""#111111""##))
    );
    assert_eq!(
        style.get("terminal.ansi.bright_black"),
        Some(&doc(r##""#555555""##))
    );
    assert_eq!(theme("light").get("style"), Some(&doc("{}")));
    assert!(
        import
            .report
            .notes
            .iter()
            .any(|n| n.starts_with("theme.terminal.light: Orca's built-in terminal theme")),
        "{:?}",
        import.report.notes
    );
}

#[test]
fn a_shared_terminal_theme_colours_both_appearances() {
    let import = import_orca_text(&export_with(
        r##"{
            "theme": "system",
            "terminalThemeDark": "custom:night",
            "terminalThemeLight": "Builtin Light",
            "terminalUseSeparateLightTheme": false,
            "terminalCustomThemes": [{"id": "night", "name": "Night", "terminal": {"background": "#000", "red": "#f00"}}]
        }"##,
    ))
    .expect("valid");
    assert_eq!(
        get(&import.zed, "theme"),
        Some(&doc(
            r#"{"mode":"system","light":"Icarus Imported Light","dark":"Icarus Imported Dark"}"#
        ))
    );
    let family = import.zed_theme.expect("family");
    for theme in family
        .get("themes")
        .and_then(Value::as_array)
        .expect("themes")
    {
        assert_eq!(
            theme.get("style"),
            Some(&doc(
                r##"{"terminal.background":"#000000","terminal.ansi.red":"#ff0000"}"##
            ))
        );
    }
}

#[test]
fn jsonc_comments_and_trailing_commas_are_dropped() {
    let text = "// Zed settings\n{\n  \"a\": \"x // not a comment\", /* gone */\n  \"b\": [1, 2,],\n  \"c\": \"quote \\\" ,}\",\n}\n";
    assert_eq!(
        json::parse(&zed::strip_jsonc(text)).expect("parses"),
        doc(r#"{"a":"x // not a comment","b":[1,2],"c":"quote \" ,}"}"#)
    );
}

#[test]
fn apply_merges_into_existing_zed_settings_without_overwriting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = dir.path().join("settings.json");
    let original = "// mine\n{\n  \"theme\": \"Ayu Dark\",\n  \"languages\": { \"Python\": { \"tab_size\": 4 } },\n}\n";
    std::fs::write(&settings, original).expect("write");
    let fragment = doc(
        r#"{"theme":{"mode":"system","light":"One Light","dark":"One Dark"},
            "languages":{"Python":{"language_servers":["pyrefly","..."],"tab_size":2}},
            "auto_install_extensions":{"oxc":true}}"#,
    );
    let theme = doc(r#"{"name":"Icarus Imported","themes":[]}"#);

    let preview = zed::apply(dir.path(), &fragment, Some(&theme), true).expect("dry run");
    assert_eq!(
        preview.added,
        [
            "languages.Python.language_servers",
            "auto_install_extensions"
        ]
    );
    assert_eq!(preview.kept, ["theme", "languages.Python.tab_size"]);
    assert_eq!(std::fs::read_to_string(&settings).expect("read"), original);
    assert!(!dir.path().join("themes").exists());

    let applied = zed::apply(dir.path(), &fragment, Some(&theme), false).expect("apply");
    let written = layers::read_document(&settings)
        .expect("valid")
        .expect("present");
    assert_eq!(written.get("theme"), Some(&Value::from("Ayu Dark")));
    assert_eq!(
        get(&written, "languages.Python"),
        Some(&doc(
            r#"{"tab_size":4,"language_servers":["pyrefly","..."]}"#
        ))
    );
    let backup = applied.backup.expect("comments were backed up");
    assert_eq!(std::fs::read_to_string(backup).expect("backup"), original);
    let theme_path = applied.theme.expect("theme path");
    assert_eq!(
        theme_path,
        dir.path().join("themes").join(zed::IMPORTED_THEME_FILE)
    );
    assert!(theme_path.exists());

    let again = zed::apply(dir.path(), &fragment, None, false).expect("idempotent");
    assert!(again.added.is_empty());
}

#[test]
fn apply_creates_missing_settings_and_refuses_broken_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fragment = doc(r#"{"buffer_font_family":"Example Mono"}"#);
    let applied = zed::apply(dir.path(), &fragment, None, false).expect("apply");
    assert_eq!(applied.added, ["buffer_font_family"]);
    assert!(applied.backup.is_none());
    assert_eq!(
        layers::read_document(&applied.settings).expect("valid"),
        Some(fragment.clone())
    );

    std::fs::write(&applied.settings, "{ broken").expect("write");
    assert!(matches!(
        zed::apply(dir.path(), &fragment, None, false),
        Err(zed::ApplyError::Parse { .. })
    ));
}
