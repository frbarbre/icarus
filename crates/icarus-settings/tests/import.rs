mod common;

use common::{FIXTURES, fixture};
use icarus_settings::import::{OrcaImport, export_orca, import_orca, import_orca_text};
use icarus_settings::json::{self, Value};
use icarus_settings::layers::{self, KeyPath, Layer, LayerDoc, LayerStack};
use icarus_settings::orca::{OrcaExport, Presence, WorkspaceStatusRuleConfig};
use indexmap::IndexMap;

fn get<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    layers::get(doc, &KeyPath::parse(key).expect("key"))
}

fn import_fixture(name: &str) -> OrcaImport {
    import_orca_text(&fixture(name)).expect("valid fixture")
}

fn through_files(import: &OrcaImport) -> (Value, IndexMap<String, Value>) {
    let reparse =
        |doc: &Value| json::parse(&format!("{}\n", doc.to_pretty_string())).expect("json");
    let projects = import
        .projects
        .iter()
        .map(|(remote, doc)| (remote.clone(), reparse(doc)))
        .collect();
    (reparse(&import.user), projects)
}

#[test]
fn export_after_import_reproduces_the_original_bytes() {
    for name in FIXTURES {
        let text = fixture(name);
        let import = import_fixture(name);
        let export = export_orca(&import.user, &import.projects).expect("valid export");
        assert_eq!(export.to_text(), text, "{name}");

        let (user, projects) = through_files(&import);
        let export = export_orca(&user, &projects).expect("valid export");
        assert_eq!(export.to_text(), text, "{name} via files");
    }
}

#[test]
fn statuses_rules_and_prompts_map_verbatim() {
    let import = import_fixture("full-export.json");
    let original = OrcaExport::parse(&fixture("full-export.json")).expect("valid");
    let user = &import.user;

    assert_eq!(
        get(user, "workspace.statuses"),
        original.ui_value("workspaceStatuses")
    );
    let ids: Vec<_> = get(user, "workspace.statuses")
        .and_then(Value::as_array)
        .expect("statuses")
        .iter()
        .filter_map(|s| s.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids[4], "status-3-2");

    let rules = get(user, "rules.pr").expect("rules");
    assert_eq!(Some(rules), original.ui_value("workspaceStatusRules"));
    let typed = WorkspaceStatusRuleConfig::from_value(rules).expect("engine-readable");
    assert_eq!(typed.hold_status(), Some("status-9"));
    assert_eq!(
        typed
            .review_inbox
            .as_ref()
            .and_then(|i| i.rereview_prompt_template.as_deref()),
        Some("Re-review #{{prNumber}} since {{sinceCommit}}; focus on what changed’s impact.")
    );
    assert!(
        typed
            .conflict_prompt_template
            .as_deref()
            .is_some_and(|p| p.contains("bell \u{7} survive"))
    );

    assert_eq!(
        get(user, "workspace.cardFields"),
        original.ui_value("worktreeCardProperties")
    );
    assert_eq!(
        get(user, "quickCommands"),
        original.setting("terminalQuickCommands")
    );
    assert_eq!(
        get(user, "sourceControl.sections"),
        original.setting("sourceControlSectionLayout"),
        "the panels' section order and hidden sections, PR panel included"
    );
    assert_eq!(get(user, "presets"), None);
    assert_eq!(get(user, "languages"), None);
    assert_eq!(
        get(&import.zed, "auto_install_extensions"),
        Some(&json::parse(r#"{"tsgo":true,"oxc":true}"#).expect("json"))
    );
    for skipped in [
        "settings.languageServers.rust",
        "settings.formatters.typescript",
        "settings.linters.typescript",
    ] {
        assert!(
            import.report.notes.iter().any(|n| n.starts_with(skipped)),
            "{skipped}: {:?}",
            import.report.notes
        );
    }
    assert_eq!(
        get(user, "orca.settings.languageServers"),
        original.setting("languageServers")
    );
    assert_eq!(
        get(user, "theme.inactivePaneOpacity"),
        original.setting("terminalInactivePaneOpacity")
    );
    assert_eq!(get(user, "theme.mode"), Some(&Value::from("dark")));
    assert_eq!(
        get(user, "theme.terminal.dark"),
        Some(&Value::from("Example Night"))
    );
    assert_eq!(
        get(user, "theme.editor.dark.name"),
        Some(&Value::from("Example Dark"))
    );
    assert_eq!(
        get(user, "theme.editor.light.orcaFile"),
        Some(&Value::from("editor-light.json"))
    );
    assert_eq!(
        get(user, "keybindings.platforms.darwin.editor\\.previousChange"),
        Some(&json::parse(r#"["ArrowUp"]"#).expect("json"))
    );
    assert_eq!(
        get(user, "sidebar.groupBy"),
        Some(&Value::from("workspace-status"))
    );
    assert!(
        import.report.validation.is_empty(),
        "{:?}",
        import.report.validation
    );
}

#[test]
fn absent_hold_status_stays_absent() {
    let import = import_fixture("old-build-export.json");
    let rules = get(&import.user, "rules.pr").expect("rules");
    assert!(rules.get("holdStatus").is_none());
    let typed = WorkspaceStatusRuleConfig::from_value(rules).expect("shape");
    assert_eq!(typed.hold_status, Presence::Absent);
    assert!(
        import
            .report
            .notes
            .iter()
            .any(|n| n.contains("no holdStatus"))
    );
}

#[test]
fn projects_become_documents_keyed_by_remote() {
    let import = import_fixture("full-export.json");
    let remotes: Vec<_> = import.projects.keys().cloned().collect();
    assert_eq!(
        remotes,
        [
            "github.com/example-org/widgets",
            "gitlab.example.com/group/sub/tools"
        ]
    );
    let widgets = &import.projects["github.com/example-org/widgets"];
    assert_eq!(
        get(widgets, "project.hooks.scripts.setup"),
        Some(&Value::from("make worktree-setup"))
    );
    assert_eq!(
        get(widgets, "project.hooks.scripts.archive"),
        Some(&Value::from("make worktree-archive"))
    );
    assert_eq!(
        get(widgets, "project.hooks.setupAgentStartupPolicy"),
        Some(&Value::from("wait-for-setup"))
    );
    assert_eq!(
        get(widgets, "project.hooks.setupRunPolicy"),
        Some(&Value::from("run-by-default"))
    );
    assert_eq!(
        get(widgets, "project.ciTriage.prompt"),
        Some(&Value::from(
            "Triage the failure. Prefer reruns for flakes — never force-push."
        ))
    );
    assert_eq!(
        get(widgets, "project.sourceControlAi.prCreationDefaults.draft"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        get(widgets, "orca.project.settings.staleRepoField"),
        Some(&Value::from("kept verbatim"))
    );

    let mut stack = LayerStack::with_defaults();
    stack.push(LayerDoc {
        layer: Layer::User,
        label: "user".into(),
        origin: None,
        doc: import.user.clone(),
    });
    stack.push(LayerDoc {
        layer: Layer::ProjectLocal,
        label: "widgets".into(),
        origin: None,
        doc: widgets.clone(),
    });
    let invalid = stack.validate();
    assert!(invalid.iter().all(|e| e.key == "rules"), "{invalid:?}");
    assert_eq!(
        stack.get("project.portLabelsCommand").expect("key"),
        Some(Value::from("make print-urls"))
    );
    assert_eq!(
        stack.get("ports.labels_command").expect("key"),
        Some(Value::from("make print-urls"))
    );
}

#[test]
fn ports_labels_command_alone_exports_as_port_labels_command() {
    let mut doc = Value::object();
    layers::set(
        &mut doc,
        &KeyPath::parse("ports.labels_command").expect("key"),
        "mise run print-urls".into(),
    )
    .expect("set");
    let mut projects = IndexMap::new();
    projects.insert("github.com/a/b".to_string(), doc);
    let export = export_orca(&Value::object(), &projects).expect("valid");
    let exported = export.projects().expect("shape");
    assert_eq!(
        exported[0].settings.port_labels_command.as_deref(),
        Some("mise run print-urls")
    );
}

#[test]
fn report_lists_what_was_not_mapped() {
    let import = import_fixture("full-export.json");
    let report = &import.report;
    for key in [
        "settings.experimentalWorktreeSymlinks",
        "settings.notifications",
        "settings.legacyNumberedMap",
        "ui.hideTemporaryCheckoutWorkspaces",
        "projects[github.com/example-org/widgets].settings.staleRepoField",
        "projects[github.com/example-org/widgets].note",
    ] {
        assert!(report.unmapped.iter().any(|u| u == key), "{key}");
    }
    assert!(
        report
            .mapped
            .iter()
            .any(|m| m.from == "ui.workspaceStatusRules" && m.to == "rules.pr")
    );
    assert!(report.notes.iter().any(|n| n.contains("3 bindings")));
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("1 commands are scoped"))
    );
    assert!(report.notes.iter().any(|n| n.contains("no project scope")));
    assert!(
        get(&import.user, "orca.settings.experimentalWorktreeSymlinks").is_some(),
        "stale keys are carried"
    );

    let old = import_fixture("old-build-export.json");
    assert!(old.report.unmapped.iter().any(|u| u == "futureEnvelopeKey"));
}

#[test]
fn edits_in_icarus_show_up_in_the_export() {
    let mut import = import_fixture("full-export.json");
    layers::set(
        &mut import.user,
        &KeyPath::parse("workspace.statuses.8").expect("key"),
        json::parse(r#"{"id":"status-10","label":"New"}"#).expect("json"),
    )
    .expect("append");
    layers::set(
        &mut import.user,
        &KeyPath::parse("rules.pr.holdStatus").expect("key"),
        Value::Null,
    )
    .expect("set");
    layers::unset(
        &mut import.user,
        &KeyPath::parse("keybindings").expect("key"),
    );
    layers::set(
        &mut import.user,
        &KeyPath::parse("statusBar.visible").expect("key"),
        Value::Bool(false),
    )
    .expect("set");
    let widgets = import
        .projects
        .get_mut("github.com/example-org/widgets")
        .expect("widgets");
    layers::set(
        widgets,
        &KeyPath::parse("project.portLabelsCommand").expect("key"),
        "make urls".into(),
    )
    .expect("set");
    let mut fresh = Value::object();
    layers::set(
        &mut fresh,
        &KeyPath::parse("project.worktreeBaseRef").expect("key"),
        "main".into(),
    )
    .expect("set");
    import
        .projects
        .insert("github.com/example-org/new".into(), fresh);

    let export = export_orca(&import.user, &import.projects).expect("valid");
    let statuses = export
        .workspace_statuses()
        .expect("shape")
        .expect("present");
    assert_eq!(statuses.last().map(|s| s.id.as_str()), Some("status-10"));
    let rules = export.status_rules().expect("shape").expect("present");
    assert_eq!(rules.hold_status, Presence::Null);
    assert_eq!(export.root().get("keybindings"), Some(&Value::Null));
    assert_eq!(
        export.ui_value("statusBarVisible"),
        Some(&Value::Bool(false))
    );
    let projects = export.projects().expect("shape");
    assert_eq!(
        projects[0].settings.port_labels_command.as_deref(),
        Some("make urls")
    );
    assert_eq!(projects[2].remote, "github.com/example-org/new");
    assert_eq!(projects[2].name, "");
    assert_eq!(
        projects[2].settings.worktree_base_ref.as_deref(),
        Some("main")
    );
}

#[test]
fn hand_edited_oddities_survive_the_round_trip() {
    let text = r#"{
  "format": "orca-settings",
  "version": 1,
  "settings": "not an object",
  "projects": [
    {
      "remote": "github.com/a/b",
      "settings": {
        "portLabelsCommand": "x"
      }
    },
    {
      "remote": "github.com/a/b",
      "name": "duplicate",
      "settings": {}
    },
    5
  ],
  "keybindings": "{ \"version\": 1 }",
  "editorThemes": {
    "editor-dark.json": "not json",
    "../evil.json": "x",
    "editor-light.json": 3
  }
}
"#;
    let import = import_orca_text(text).expect("valid");
    assert_eq!(import.projects.len(), 1);
    assert!(get(&import.user, "keybindings").is_none());
    assert!(
        import
            .report
            .notes
            .iter()
            .any(|n| n.contains("repeats a remote"))
    );
    let export = export_orca(&import.user, &import.projects).expect("valid");
    assert_eq!(export.to_text(), text);
}

#[test]
fn a_fresh_icarus_document_exports_a_valid_orca_file() {
    let user = json::parse(
        r#"{"workspace":{"statuses":[{"id":"todo","label":"Todo"}]},"theme":{"mode":"light"}}"#,
    )
    .expect("json");
    let export = export_orca(&user, &IndexMap::new()).expect("valid");
    assert_eq!(
        Value::Object(export.root().clone()).to_compact_string(),
        r#"{"format":"orca-settings","version":1,"exportedAt":"","settings":{"theme":"light"},"ui":{"workspaceStatuses":[{"id":"todo","label":"Todo"}]},"projects":[],"keybindings":null,"editorThemes":{}}"#
    );
    assert!(OrcaExport::parse(&export.to_text()).is_ok());
    let again = import_orca(&export);
    assert_eq!(
        export_orca(&again.user, &again.projects)
            .expect("valid")
            .to_text(),
        export.to_text()
    );
}

#[test]
fn worktree_visibility_defaults_become_discovery_settings() {
    let text = r#"{
  "format": "orca-settings",
  "version": 1,
  "settings": {
    "worktreeVisibilityDefaults": {
      "external": "hide",
      "customSources": [
        { "id": "conductor", "rootPath": "~/conductor/workspaces" },
        { "id": "scrubbed" },
        { "id": "bad id", "rootPath": "/x" }
      ],
      "sourcePreferences": {
        "builtIn": { "claude": "show", "other": "show" },
        "custom": { "conductor": "show", "scrubbed": "maybe" }
      }
    }
  },
  "projects": [
    {
      "remote": "github.com/acme/widgets",
      "settings": {
        "externalWorktreeVisibility": "show",
        "worktreeVisibilitySourcePreferences": { "builtIn": { "gsd": "hide" } }
      }
    }
  ]
}
"#;
    let import = import_orca_text(text).expect("valid");
    let user = &import.user;
    assert_eq!(
        get(user, "workspace.external").and_then(Value::as_str),
        Some("hide")
    );
    assert_eq!(
        get(user, "workspace.worktreeSources"),
        Some(
            &json::parse(r#"[{"id":"conductor","root":"~/conductor/workspaces"}]"#).expect("json")
        )
    );
    assert_eq!(
        get(user, "workspace.sourceVisibility"),
        Some(&json::parse(r#"{"claude":"show","conductor":"show"}"#).expect("json"))
    );
    assert!(
        import
            .report
            .notes
            .iter()
            .any(|n| n.contains("2 source(s)")),
        "{:?}",
        import.report.notes
    );
    assert!(
        !import
            .report
            .unmapped
            .contains(&"settings.worktreeVisibilityDefaults".to_owned())
    );
    let project = &import.projects["github.com/acme/widgets"];
    assert_eq!(
        get(project, "project.externalWorktreeVisibility").and_then(Value::as_str),
        Some("show"),
        "a repo that showed external worktrees in Orca keeps showing them"
    );
    assert!(
        import.report.validation.is_empty(),
        "{:?}",
        import.report.validation
    );
    let export = export_orca(&import.user, &import.projects).expect("valid");
    assert_eq!(
        json::parse(&export.to_text()).expect("json"),
        json::parse(text).expect("json"),
        "the original comes back on export"
    );
}
