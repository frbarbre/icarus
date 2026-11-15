mod common;

use common::{FIXTURES, fixture, node_stringify};
use icarus_settings::json::{self, Value};
use icarus_settings::orca::{
    OrcaExport, OrcaParseError, Presence, WorkspaceStatusRuleCondition, WorkspaceStatusRuleConfig,
    is_portable_setting_key,
};

fn full() -> OrcaExport {
    OrcaExport::parse(&fixture("full-export.json")).expect("valid fixture")
}

#[test]
fn fixtures_round_trip_byte_for_byte() {
    for name in FIXTURES {
        let text = fixture(name);
        let export = OrcaExport::parse(&text).expect("valid fixture");
        assert_eq!(export.to_text(), text, "{name}");
        assert_eq!(export.to_gui_text(), text.trim_end_matches('\n'), "{name}");
    }
}

#[test]
fn output_matches_javascript_json_stringify() {
    let inputs = [
        fixture("full-export.json"),
        fixture("old-build-export.json"),
        r#"{"format":"orca-settings","version":1.0,"settings":{"b":1,"10":2,"2":[],"x":{},"s":"\u00e9\u2028\u0001\/"}}"#
            .to_owned(),
    ];
    for input in inputs {
        let Some(expected) = node_stringify(&input) else {
            return;
        };
        let export = OrcaExport::parse(&input).expect("valid");
        let mut ours = export.to_text();
        if input.contains("1.0") {
            ours = ours.replace("\"version\": 1.0", "\"version\": 1");
        }
        assert_eq!(ours, expected);
    }
}

#[test]
fn rejects_exactly_what_orca_rejects() {
    let cases = [
        (
            "nope",
            OrcaParseError::InvalidJson(json::parse("nope").expect_err("bad")),
        ),
        ("[]", OrcaParseError::NotOrcaExport),
        ("null", OrcaParseError::NotOrcaExport),
        ("{\"format\":\"other\"}", OrcaParseError::NotOrcaExport),
        ("{\"version\":1}", OrcaParseError::NotOrcaExport),
        (
            "{\"format\":\"orca-settings\"}",
            OrcaParseError::UnsupportedVersion,
        ),
        (
            "{\"format\":\"orca-settings\",\"version\":2}",
            OrcaParseError::UnsupportedVersion,
        ),
        (
            "{\"format\":\"orca-settings\",\"version\":\"1\"}",
            OrcaParseError::UnsupportedVersion,
        ),
        (
            "{\"format\":\"orca-settings\",\"version\":null}",
            OrcaParseError::UnsupportedVersion,
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(OrcaExport::parse(text).expect_err(text), expected, "{text}");
    }
    assert_eq!(
        OrcaExport::parse("{").expect_err("bad").to_string(),
        "The file is not valid JSON."
    );
    assert_eq!(
        OrcaParseError::NotOrcaExport.to_string(),
        "The file is not an Orca settings export."
    );
    assert_eq!(
        OrcaParseError::UnsupportedVersion.to_string(),
        "The file comes from a newer Orca; update Orca and try again."
    );
    for accepted in [
        "{\"format\":\"orca-settings\",\"version\":1}",
        "{\"format\":\"orca-settings\",\"version\":1.0}",
        "{\"format\":\"orca-settings\",\"version\":1e0}",
    ] {
        assert!(OrcaExport::parse(accepted).is_ok(), "{accepted}");
    }
}

#[test]
fn numbers_are_never_coerced() {
    let export = full();
    let settings = export.settings().expect("settings");
    let literal = |key: &str| {
        settings
            .get(key)
            .and_then(Value::as_number)
            .map(|n| n.as_str().to_owned())
    };
    assert_eq!(literal("terminalFontSize").as_deref(), Some("13"));
    assert_eq!(literal("terminalLineHeight").as_deref(), Some("1.2"));
    assert_eq!(literal("tinyNumber").as_deref(), Some("1e-7"));
    assert_eq!(literal("hugeNumber").as_deref(), Some("1e+21"));
    let opacity = export.ui_value("workspaceBoardOpacity").expect("opacity");
    assert_eq!(opacity.to_compact_string(), "1");
}

#[test]
fn typed_statuses_keep_ids_and_unknown_keys() {
    let statuses = full()
        .workspace_statuses()
        .expect("shape")
        .expect("present");
    let ids: Vec<_> = statuses.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "todo",
            "in-progress",
            "status-1",
            "status-2",
            "status-3-2",
            "in-review",
            "completed",
            "status-9"
        ]
    );
    assert_eq!(statuses[2].label, "Konflikter ⚠");
    assert_eq!(
        statuses[7].extra.get("legacyHint"),
        Some(&Value::from("kept"))
    );
}

#[test]
fn typed_status_rules() {
    let rules = full().status_rules().expect("shape").expect("present");
    assert!(rules.is_enabled());
    assert_eq!(rules.hold_status, Presence::Value("status-9".to_owned()));
    assert_eq!(rules.hold_status(), Some("status-9"));
    assert_eq!(rules.new_workspace_status(), Some("todo"));
    assert_eq!(rules.pm_approval_team(), Some("example-org/pm-team"));
    assert_eq!(rules.merging_check_name(), "Reviews satisfied");
    assert!(rules.deletes_on_resolved() && rules.deletes_on_reviewed());
    assert_eq!(
        rules.status_for(WorkspaceStatusRuleCondition::MergeQueue),
        Some("status-3-2")
    );
    assert_eq!(rules.status_for(WorkspaceStatusRuleCondition::Merged), None);
    let order: Vec<_> = rules
        .status_by_condition
        .as_ref()
        .expect("mapping")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        order,
        [
            "reviewing",
            "merge-queue",
            "conflicts",
            "draft",
            "review",
            "pm-approval"
        ]
    );
    assert!(rules.extra.contains_key("futureRuleKey"));
    let prompts = rules.action_prompts.as_ref().expect("prompts");
    assert_eq!(
        prompts.review_pull_request.as_deref(),
        Some("Review {{prTitle}} carefully. 日本語もOK 🙂")
    );
    let conflict = rules.conflict_prompt_template.as_deref().expect("conflict");
    assert!(conflict.contains("sides’ intent\n\t- never \"force\" anything — ask first"));
    assert!(conflict.contains("C:\\work\\repo and a bell \u{7} survive."));
    let inbox = rules.review_inbox.as_ref().expect("inbox");
    assert!(inbox.is_enabled());
    assert_eq!(inbox.agent_or_default(), "claude");
    assert_eq!(inbox.view_or_default(), "chat");
}

#[test]
fn hold_status_absent_null_and_set_are_distinct() {
    let old = OrcaExport::parse(&fixture("old-build-export.json")).expect("valid");
    let rules = old.status_rules().expect("shape").expect("present");
    assert_eq!(rules.hold_status, Presence::Absent);
    assert_eq!(rules.new_workspace_status, Presence::Null);

    let null = json::parse(r#"{"holdStatus":null}"#).expect("json");
    let rules = WorkspaceStatusRuleConfig::from_value(&null).expect("shape");
    assert_eq!(rules.hold_status, Presence::Null);
    assert_eq!(rules.hold_status(), None);

    let blank = json::parse(r#"{"holdStatus":"  "}"#).expect("json");
    let rules = WorkspaceStatusRuleConfig::from_value(&blank).expect("shape");
    assert_eq!(rules.hold_status(), None);
}

#[test]
fn condition_order_matches_orca() {
    let ids: Vec<_> = WorkspaceStatusRuleCondition::ORDER
        .iter()
        .map(|c| c.id())
        .collect();
    assert_eq!(
        ids,
        [
            "merged",
            "closed",
            "reviewing",
            "merge-queue",
            "conflicts",
            "draft",
            "changes-requested",
            "merging",
            "pm-approval",
            "review"
        ]
    );
    assert_eq!(
        WorkspaceStatusRuleCondition::from_id("pm-approval"),
        Some(WorkspaceStatusRuleCondition::PmApproval)
    );
}

#[test]
fn typed_projects_and_quick_commands() {
    let export = full();
    let projects = export.projects().expect("shape");
    assert_eq!(projects.len(), 2);
    let widgets = &projects[0];
    assert_eq!(widgets.remote, "github.com/example-org/widgets");
    let hooks = widgets.settings.hook_settings.as_ref().expect("hooks");
    assert_eq!(hooks.setup_run_policy.as_deref(), Some("run-by-default"));
    assert_eq!(
        hooks.scripts.as_ref().and_then(|s| s.setup.as_deref()),
        Some("make worktree-setup")
    );
    let triage = widgets.settings.ci_triage.as_ref().expect("triage");
    assert!(triage.prompt.as_deref().expect("prompt").contains("—"));
    assert!(widgets.settings.extra.contains_key("staleRepoField"));
    assert!(widgets.extra.contains_key("note"));
    assert_eq!(projects[1].remote, "gitlab.example.com/group/sub/tools");

    let commands = export.quick_commands().expect("shape").expect("present");
    assert_eq!(commands[0].project_remote(), None);
    assert_eq!(
        commands[1].project_remote(),
        Some("github.com/example-org/widgets")
    );
}

#[test]
fn typed_keybindings_themes_cards_and_languages() {
    let export = full();
    let keybindings = export.keybindings().expect("shape").expect("present");
    let darwin = keybindings.platforms.get("darwin").expect("darwin");
    assert_eq!(
        darwin.get("editor.previousChange"),
        Some(&json::parse(r#"["ArrowUp"]"#).expect("json"))
    );
    assert_eq!(darwin.get("workspace.delete"), Some(&Value::Null));
    assert_eq!(keybindings.command_ids().count(), 3);

    let themes = export.editor_themes();
    let dark = themes.dark.expect("dark");
    assert_eq!(
        OrcaExport::editor_theme_name(dark).as_deref(),
        Some("Example Dark")
    );
    assert!(themes.light.is_some());

    assert_eq!(
        export
            .worktree_card_properties()
            .expect("shape")
            .expect("present"),
        ["status", "workspace-status", "pr", "linear-title", "ports"]
    );
    let servers = export.language_servers().expect("shape").expect("present");
    assert_eq!(
        servers.get("typescript").and_then(|s| s.server.as_deref()),
        Some("tsgo")
    );
    assert_eq!(servers.get("rust").and_then(|s| s.enabled), Some(false));
    assert!(export.formatters().is_some() && export.linters().is_some());
}

#[test]
fn absent_sections_read_as_orca_defaults() {
    let export = OrcaExport::parse(r#"{"format":"orca-settings","version":1,"keybindings":7}"#)
        .expect("valid");
    assert_eq!(export.exported_at(), "");
    assert!(export.workspace_statuses().expect("shape").is_none());
    assert!(export.projects().expect("shape").is_empty());
    assert!(export.keybindings_text().is_none());
    assert_eq!(export.editor_themes().dark, None);
}

#[test]
fn view_errors_name_the_key() {
    let export = OrcaExport::parse(
        r#"{"format":"orca-settings","version":1,"ui":{"workspaceStatuses":[{"id":5}]}}"#,
    )
    .expect("valid");
    let err = export.workspace_statuses().expect_err("bad shape");
    assert_eq!(err.key, "ui.workspaceStatuses");
    assert!(err.to_string().starts_with("`ui.workspaceStatuses`"));
}

#[test]
fn portable_filter_matches_orca() {
    let edited = r#"{
      "format": "orca-settings",
      "version": 1,
      "settings": {
        "theme": "light",
        "opencodeGoApiKey": "planted",
        "voice": {"enabled": true, "modelsDir": "/x", "microphoneDeviceId": "m"},
        "terminalQuickCommands": [
          {"id": "a", "label": "Planted", "scope": {"type": "repo", "repoId": "x"}, "command": "rm"},
          {"id": "b", "label": "Deploy", "scope": {"type": "project", "remote": "r"}, "command": "deploy"}
        ]
      },
      "ui": {"windowBounds": {"x": 9}, "workspaceStatusRules": {"repoIds": ["x"], "enabled": true}, "groupBy": "repo"},
      "keybindings": null,
      "editorThemes": {"editor-dark.json": "{}", "../../etc/passwd": "x", "editor-light.json": 4},
      "projects": [
        {"remote": "github.com/acme/app", "settings": {"ciTriage": {"prompt": "Triage"}, "worktreeBasePath": "/tmp", "forkSyncMode": "x", "symlinkPaths": [".env", "/etc/hosts", "../secrets", "a/../b"]}},
        {"remote": "github.com/acme/other", "settings": {"path": "/tmp"}},
        {"remote": "github.com/acme/legacy", "settings": {"ciTriage": {"hints": "  "}}}
      ]
    }"#;
    let portable = OrcaExport::parse(edited).expect("valid").portable();
    let expected = r#"{"format":"orca-settings","version":1,"exportedAt":"","settings":{"theme":"light","voice":{"enabled":true},"terminalQuickCommands":[{"id":"b","label":"Deploy","scope":{"type":"project","remote":"r"},"command":"deploy"}]},"ui":{"groupBy":"repo","workspaceStatusRules":{"enabled":true}},"projects":[{"remote":"github.com/acme/app","name":"","settings":{"symlinkPaths":[".env"],"ciTriage":{"prompt":"Triage"}}},{"remote":"github.com/acme/legacy","name":"","settings":{"ciTriage":{}}}],"keybindings":null,"editorThemes":{"editor-dark.json":"{}"}}"#;
    assert_eq!(
        Value::Object(portable.root().clone()).to_compact_string(),
        expected
    );
}

#[test]
fn private_setting_key_patterns() {
    for key in [
        "activeClaudeManagedAccountId",
        "agentStateRulesPath",
        "workspaceDir",
        "terminalDefaultShell",
        "agentDefaultEnv",
        "githubProjects",
        "microphoneDeviceId",
        "notificationsHTTP1Fallback",
    ] {
        assert!(!is_portable_setting_key(key), "{key}");
    }
    for key in [
        "theme",
        "jiraIssueKeyCasing",
        "prBotAuthorOverrides",
        "geminiCliOAuthEnabled",
        "dirtyMarker",
    ] {
        assert!(is_portable_setting_key(key), "{key}");
    }
}

#[test]
fn summary_matches_describe_settings_bundle() {
    assert_eq!(
        full().summary(),
        "31 settings, 23 layout preferences, automations, settings of 2 projects, keybindings, 2 editor themes"
    );
}
