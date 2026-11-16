use icarus_settings::Value;
use icarus_settings::json;
use icarus_settings::theme_import::{Source, convert, file_name, install};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

fn style(family: &Value) -> &Value {
    family
        .get("themes")
        .and_then(Value::as_array)
        .and_then(|themes| themes.first())
        .and_then(|theme| theme.get("style"))
        .expect("one theme with a style")
}

fn read_json(path: &std::path::Path) -> Value {
    json::parse(&std::fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn a_vscode_theme_becomes_a_zed_theme_with_its_name_and_appearance() {
    let converted = convert(
        Source::Vscode,
        &fixture("vscode-theme.json"),
        None,
        "fallback",
    )
    .expect("convert");
    assert_eq!(converted.name, "Example Dusk");
    assert_eq!(converted.appearance, "dark");
    let style = style(&converted.family);
    assert_eq!(
        style.get("editor.background").and_then(Value::as_str),
        Some("#1e1e1e")
    );
    assert_eq!(style.get("text").and_then(Value::as_str), Some("#dddddd"));
    assert!(style.get("syntax").is_some(), "token colours become syntax");
}

#[test]
fn a_warp_theme_maps_its_colours_gradient_and_palette() {
    let converted =
        convert(Source::Warp, &fixture("warp-theme.yaml"), None, "fallback").expect("convert");
    assert_eq!(converted.name, "Harbor Night");
    assert_eq!(converted.appearance, "dark");
    let style = style(&converted.family);
    assert_eq!(
        style.get("editor.background").and_then(Value::as_str),
        Some("#10141c"),
        "a gradient becomes its first stop"
    );
    assert_eq!(
        style.get("terminal.foreground").and_then(Value::as_str),
        Some("#d7dce4")
    );
    assert_eq!(
        style.get("border.focused").and_then(Value::as_str),
        Some("#3fa7ff")
    );
    assert_eq!(
        style
            .get("terminal.ansi.bright_white")
            .and_then(Value::as_str),
        Some("#cdd9e5")
    );
    assert!(converted.notes.iter().any(|note| note.contains("gradient")));
}

#[test]
fn a_light_warp_theme_and_a_name_override() {
    let text = "background: '#ffffff'\nforeground: '#111111'\ndetails: lighter\n";
    let converted = convert(Source::Warp, text, Some("Paper"), "fallback").expect("convert");
    assert_eq!(converted.name, "Paper");
    assert_eq!(converted.appearance, "light");
}

#[test]
fn a_theme_without_usable_colours_is_refused() {
    assert!(convert(Source::Warp, "name: Empty\n", None, "x").is_err());
    assert!(convert(Source::Vscode, "not json", None, "x").is_err());
}

#[test]
fn installing_writes_the_theme_and_selecting_keeps_mode_and_the_other_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("settings.json"),
        "// mine\n{ \"theme\": { \"mode\": \"dark\", \"light\": \"Ayu Light\", \"dark\": \"One Dark\" }, \"vim_mode\": true }\n",
    )
    .expect("write settings");
    let converted =
        convert(Source::Warp, &fixture("warp-theme.yaml"), None, "fallback").expect("convert");

    let dry = install(dir.path(), &converted, true, true).expect("dry run");
    assert!(!dry.theme.exists(), "a dry run writes nothing");

    let installed = install(dir.path(), &converted, true, false).expect("install");
    assert_eq!(
        installed.theme,
        dir.path().join("themes").join(file_name("Harbor Night"))
    );
    assert_eq!(read_json(&installed.theme), converted.family);
    let settings = read_json(&dir.path().join("settings.json"));
    let theme = settings.get("theme").expect("theme");
    assert_eq!(
        theme.get("dark").and_then(Value::as_str),
        Some("Harbor Night")
    );
    assert_eq!(
        theme.get("light").and_then(Value::as_str),
        Some("Ayu Light")
    );
    assert_eq!(theme.get("mode").and_then(Value::as_str), Some("dark"));
    assert_eq!(
        settings.get("vim_mode").and_then(Value::as_bool),
        Some(true)
    );
    let backup = installed
        .backup
        .expect("comments were dropped, so a backup exists");
    assert!(
        std::fs::read_to_string(backup)
            .expect("backup")
            .contains("// mine")
    );

    let again = install(dir.path(), &converted, false, false).expect("re-import");
    assert_eq!(
        again.theme, installed.theme,
        "re-importing replaces the same file"
    );
}
