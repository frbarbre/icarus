use icarus_settings::json::{self, Value};
use icarus_settings::layers::{
    self, KeyError, KeyPath, Layer, LayerDoc, LayerStack, LoadError, SettingsPaths,
};

fn doc(text: &str) -> Value {
    json::parse(text).expect("valid json")
}

fn layer(layer: Layer, text: &str) -> LayerDoc {
    LayerDoc {
        layer,
        label: layer.name().to_owned(),
        origin: None,
        doc: doc(text),
    }
}

fn stack() -> LayerStack {
    let mut stack = LayerStack::with_defaults();
    stack.push(layer(
        Layer::ProjectLocal,
        r#"{"sidebar":{"sortBy":"name"},"theme":{"fonts":{"terminalSize":15}}}"#,
    ));
    stack.push(layer(
        Layer::User,
        r#"{"sidebar":{"groupBy":"workspace-status","sortBy":"smart"},"theme":{"mode":"dark","fonts":{"terminalSize":13,"ui":"Geist"}},"workspace":{"cardFields":["status"]}}"#,
    ));
    stack.push(layer(
        Layer::Project,
        r#"{"sidebar":{"sortBy":"repo"},"workspace":{"cardFields":["pr","ports"]}}"#,
    ));
    stack
}

#[test]
fn later_layers_win_and_objects_merge_deeply() {
    let stack = stack();
    let order: Vec<_> = stack.docs().iter().map(|d| d.layer).collect();
    assert_eq!(
        order,
        [
            Layer::Defaults,
            Layer::User,
            Layer::Project,
            Layer::ProjectLocal
        ]
    );
    let get = |key: &str| stack.get(key).expect("key").expect("value");
    assert_eq!(get("sidebar.sortBy"), Value::from("name"));
    assert_eq!(get("sidebar.groupBy"), Value::from("workspace-status"));
    assert_eq!(get("sidebar.projectOrderBy"), Value::from("manual"));
    assert_eq!(get("theme.fonts.terminalSize"), doc("15"));
    assert_eq!(get("theme.fonts.ui"), Value::from("Geist"));
    assert_eq!(get("workspace.cardFields"), doc(r#"["pr","ports"]"#));
    assert_eq!(get("workspace.statuses.1.id"), Value::from("in-progress"));
    assert!(stack.get("sidebar.nothing").expect("key").is_none());
}

#[test]
fn explain_names_the_supplying_layer() {
    let stack = stack();
    let explained = stack.explain("sidebar.sortBy").expect("key");
    assert_eq!(explained.value, Some(Value::from("name")));
    assert_eq!(
        explained.winner().map(|s| s.layer),
        Some(Layer::ProjectLocal)
    );
    let overridden: Vec<_> = explained.overridden.iter().map(|s| s.layer).collect();
    assert_eq!(overridden, [Layer::Defaults, Layer::User, Layer::Project]);

    let explained = stack.explain("theme.fonts").expect("key");
    let contributors: Vec<_> = explained.contributors.iter().map(|s| s.layer).collect();
    assert_eq!(contributors, [Layer::User, Layer::ProjectLocal]);
    assert_eq!(
        explained.value,
        Some(doc(r#"{"terminalSize":15,"ui":"Geist"}"#))
    );

    let explained = stack.explain("workspace.statuses").expect("key");
    assert_eq!(explained.winner().map(|s| s.layer), Some(Layer::Defaults));

    let explained = stack.explain("missing.key").expect("key");
    assert!(explained.value.is_none() && explained.winner().is_none());
}

#[test]
fn explain_tracks_a_parent_replaced_by_a_scalar() {
    let mut stack = LayerStack::empty();
    stack.push(layer(Layer::User, r#"{"a":{"b":1}}"#));
    stack.push(layer(Layer::Project, r#"{"a":5}"#));
    let explained = stack.explain("a.b").expect("key");
    assert!(explained.value.is_none());
    assert!(explained.contributors.is_empty());
    assert_eq!(explained.overridden[0].layer, Layer::User);
}

#[test]
fn presets_sit_between_defaults_and_user() {
    let mut stack = LayerStack::with_defaults();
    stack.push(layer(
        Layer::User,
        r#"{"rules":{"orca_compat":{"newWorkspaceStatus":"todo"}}}"#,
    ));
    stack.add_preset("github-pr-flow").expect("preset");
    assert!(matches!(
        stack.add_preset("nope"),
        Err(LoadError::UnknownPreset(_))
    ));
    let rules = stack.get("rules.orca_compat").expect("key").expect("value");
    assert_eq!(rules.get("enabled"), Some(&Value::Bool(true)));
    assert_eq!(rules.get("newWorkspaceStatus"), Some(&Value::from("todo")));
    assert_eq!(rules.get("holdStatus"), Some(&Value::Null));
    let explained = stack
        .explain("rules.orca_compat.statusByCondition.review")
        .expect("key");
    assert_eq!(
        explained.winner().map(|s| s.label.as_str()),
        Some("preset github-pr-flow")
    );
    for preset in &layers::PRESETS {
        assert!(
            layers::validate(&preset.document(), None).is_empty(),
            "{}",
            preset.name
        );
    }
    assert!(layers::validate(&layers::defaults(), None).is_empty());
}

#[test]
fn dotted_keys_get_set_and_unset() {
    let mut value = Value::object();
    let key = |k: &str| KeyPath::parse(k).expect("key");
    layers::set(&mut value, &key("theme.editor.dark.name"), "X".into()).expect("set");
    layers::set(
        &mut value,
        &key("orca.files.editor-dark\\.json"),
        true.into(),
    )
    .expect("set");
    layers::set(&mut value, &key("list"), doc("[1,2]")).expect("set");
    layers::set(&mut value, &key("list.2"), doc("3")).expect("append");
    layers::set(&mut value, &key("list.0"), doc("0")).expect("replace");
    assert_eq!(
        value.to_compact_string(),
        r#"{"theme":{"editor":{"dark":{"name":"X"}}},"orca":{"files":{"editor-dark.json":true}},"list":[0,2,3]}"#
    );
    assert_eq!(
        layers::get(&value, &key("orca.files.editor-dark\\.json")),
        Some(&Value::Bool(true))
    );
    assert_eq!(key("a.b\\.c").to_string(), "a.b\\.c");

    assert!(matches!(
        layers::set(&mut value, &key("list.9"), doc("1")),
        Err(KeyError::BadIndex { .. })
    ));
    let err = layers::set(&mut value, &key("theme.editor.dark.name.x"), doc("1"))
        .expect_err("scalar parent");
    assert_eq!(
        err.to_string(),
        "`theme.editor.dark.name.x`: `theme.editor.dark.name` is a string, not an object or array"
    );

    assert_eq!(layers::unset(&mut value, &key("list.1")), Some(doc("2")));
    assert_eq!(
        layers::unset(&mut value, &key("theme.editor")),
        Some(doc(r#"{"dark":{"name":"X"}}"#))
    );
    assert_eq!(layers::unset(&mut value, &key("theme.nothing")), None);
    assert_eq!(
        value.to_compact_string(),
        r#"{"theme":{},"orca":{"files":{"editor-dark.json":true}},"list":[0,3]}"#
    );
    assert!(matches!(KeyPath::parse(""), Err(KeyError::Empty)));
    assert!(matches!(
        KeyPath::parse("a..b"),
        Err(KeyError::EmptySegment(_))
    ));
}

#[test]
fn cli_values_parse_as_json_or_fall_back_to_strings() {
    assert_eq!(layers::parse_cli_value("13"), doc("13"));
    assert_eq!(layers::parse_cli_value("true"), Value::Bool(true));
    assert_eq!(layers::parse_cli_value("[\"a\"]"), doc("[\"a\"]"));
    assert_eq!(layers::parse_cli_value("dark"), Value::from("dark"));
}

#[test]
fn validation_errors_name_the_key() {
    let bad = doc(
        r#"{"theme":{"mode":"neon","fonts":{"terminalSize":"big"}},"workspace":{"statuses":[{"id":"Todo","label":"x"},{"label":"y"}],"boardColumnWidth":1.5},"sidebar":{"groupby":"repo"},"rules":{"orca_compat":{"holdStatus":null,"enabled":"yes","futureKey":1}},"mystery":1}"#,
    );
    let errors: Vec<String> = layers::validate(&bad, Some(Layer::User))
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        errors,
        [
            "user settings: `theme.mode`: `neon` is not one of system, dark, light",
            "user settings: `theme.fonts.terminalSize`: expected a number, found string",
            "user settings: `workspace.statuses.0.id`: `Todo` is not a valid id (lowercase letters, digits, `-`, `_`)",
            "user settings: `workspace.boardColumnWidth`: expected an integer, found 1.5",
            "user settings: `sidebar.groupby`: unknown setting",
            "user settings: `rules.orca_compat.enabled`: expected a boolean, found string",
            "user settings: `mystery`: unknown setting",
        ]
    );
}

#[test]
fn json_schema_describes_the_document() {
    let schema = layers::json_schema();
    assert_eq!(
        schema.get("$schema").and_then(Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema")
    );
    let props = schema.get("properties").expect("properties");
    for key in [
        "workspace",
        "rules",
        "keybindings",
        "quickCommands",
        "presets",
        "theme",
        "project",
        "orca",
    ] {
        assert!(props.get(key).is_some(), "{key}");
    }
    let hold = layers::get(
        &schema,
        &KeyPath::parse("properties.rules.properties.orca_compat.properties.holdStatus")
            .expect("key"),
    )
    .expect("holdStatus");
    assert_eq!(
        hold.get("anyOf").map(Value::to_compact_string).as_deref(),
        Some(r#"[{"type":"string"},{"type":"null"}]"#)
    );
    assert!(json::parse(&schema.to_pretty_string()).is_ok());
}

#[test]
fn layer_files_load_and_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = SettingsPaths {
        user: Some(dir.path().join("config/icarus/icarus.json")),
        project: Some(dir.path().join("repo").join(layers::PROJECT_FILE)),
        project_local: Some(dir.path().join("repo").join(layers::PROJECT_LOCAL_FILE)),
    };
    let user = doc(r#"{"theme":{"mode":"light"},"prompt":"é — ’"}"#);
    layers::write_document(paths.user.as_deref().expect("user"), &user).expect("write");
    let written = std::fs::read_to_string(paths.user.as_deref().expect("user")).expect("read");
    assert_eq!(
        written,
        "{\n  \"theme\": {\n    \"mode\": \"light\"\n  },\n  \"prompt\": \"é — ’\"\n}\n"
    );
    layers::write_document(
        paths.project.as_deref().expect("project"),
        &doc(r#"{"theme":{"mode":"dark"}}"#),
    )
    .expect("write");

    let stack = LayerStack::load(&paths, &["review-inbox"]).expect("load");
    let layers_loaded: Vec<_> = stack.docs().iter().map(|d| d.layer).collect();
    assert_eq!(
        layers_loaded,
        [Layer::Defaults, Layer::Presets, Layer::User, Layer::Project]
    );
    let explained = stack.explain("theme.mode").expect("key");
    assert_eq!(explained.value, Some(Value::from("dark")));
    assert_eq!(
        explained.winner().and_then(|s| s.origin.clone()),
        paths.project.clone()
    );

    std::fs::write(paths.project_local.as_deref().expect("local"), "{ nope").expect("write");
    let err = LayerStack::load(&paths, &[]).expect_err("bad json");
    assert!(matches!(err, LoadError::Parse { .. }));
    assert!(
        err.to_string()
            .contains("icarus.local.json is not valid JSON")
    );
    std::fs::write(paths.project_local.as_deref().expect("local"), "[]").expect("write");
    assert!(matches!(
        LayerStack::load(&paths, &[]),
        Err(LoadError::NotObject { .. })
    ));
}

#[test]
fn discovers_standard_paths() {
    let paths = SettingsPaths::discover(Some(std::path::Path::new("/repo")));
    assert_eq!(
        paths.project.as_deref(),
        Some(std::path::Path::new("/repo/icarus.json"))
    );
    assert_eq!(
        paths.project_local.as_deref(),
        Some(std::path::Path::new("/repo/icarus.local.json"))
    );
    assert!(
        paths
            .user
            .as_deref()
            .is_none_or(|p| p.ends_with("icarus/icarus.json"))
    );
    assert_eq!(Layer::from_name("project-local"), Some(Layer::ProjectLocal));
}

#[test]
fn user_layer_falls_back_to_legacy_settings_json_only_when_it_holds_icarus_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let user = dir.path().join(layers::USER_FILE);
    let legacy = dir.path().join("settings.json");
    let paths = SettingsPaths {
        user: Some(user.clone()),
        ..SettingsPaths::default()
    };

    std::fs::write(&legacy, "// Zed settings\n{ \"ui_font_size\": 16 }").expect("write");
    assert_eq!(layers::layer_source(Layer::User, &user), user);
    assert_eq!(layers::read_layer(Layer::User, &user).expect("read"), None);
    std::fs::write(&legacy, r#"{"ui_font_size": 16, "theme": "One Dark"}"#).expect("write");
    assert_eq!(layers::layer_source(Layer::User, &user), user);

    std::fs::write(
        &legacy,
        r#"{"theme":{"mode":"light"},"sidebar":{"groupBy":"repo"},"languages":{}}"#,
    )
    .expect("write");
    assert_eq!(layers::layer_source(Layer::User, &user), legacy);
    assert_eq!(layers::layer_source(Layer::Project, &user), user);
    let stack = LayerStack::load(&paths, &[]).expect("load");
    let explained = stack.explain("theme.mode").expect("key");
    assert_eq!(explained.value, Some(Value::from("light")));
    assert_eq!(
        explained.winner().and_then(|s| s.origin.clone()),
        Some(legacy.clone())
    );

    layers::write_document(&user, &doc(r#"{"theme":{"mode":"dark"}}"#)).expect("write");
    assert_eq!(layers::layer_source(Layer::User, &user), user);
    let stack = LayerStack::load(&paths, &[]).expect("load");
    assert_eq!(
        stack.get("theme.mode").expect("key"),
        Some(Value::from("dark"))
    );
}
