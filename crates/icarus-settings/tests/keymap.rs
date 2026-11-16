use icarus_settings::keymap::{self, IMPORT_RECORD_FILE, Platform, keystroke, translate};
use serde_json::{Value, json};

/// Made-up remaps in Orca's `keybindings.json` shape.
fn orca() -> Value {
    json!({
        "version": 1,
        "keybindings": { "tab.close": ["Mod+Alt+W"] },
        "platforms": {
            "darwin": {
                "editor.nextChange": ["ArrowDown"],
                "editor.previousChange": ["ArrowUp"],
                "editor.nextFile": ["Mod+ArrowDown"],
                "tab.nextSameType": ["Mod+Shift+BracketRight"],
                "tab.nextAllTypes": ["Mod+Alt+BracketRight"],
                "tab.selectByIndex": ["Alt+1"],
                "tab.rename": [],
                "editor.showDiff": ["Ctrl+D"],
                "sourceControl.copyPullRequestUrl": ["Ctrl+G"],
                "made.up.action": ["Ctrl+Q"],
            },
            "linux": { "editor.nextChange": ["Mod+J"] },
        },
    })
}

fn section<'a>(translation: &'a keymap::Translation, context: &str) -> Option<&'a Value> {
    translation
        .sections
        .iter()
        .find(|section| section["context"] == context)
        .map(|section| &section["bindings"])
}

#[test]
fn chords_become_zed_keystrokes() {
    assert_eq!(
        keystroke("Mod+Shift+BracketRight", Platform::Mac).as_deref(),
        Some("cmd-}")
    );
    assert_eq!(
        keystroke("Mod+Shift+BracketLeft", Platform::Linux).as_deref(),
        Some("ctrl-{")
    );
    assert_eq!(
        keystroke("ArrowDown", Platform::Mac).as_deref(),
        Some("down")
    );
    assert_eq!(
        keystroke("Mod+Alt+Shift+T", Platform::Mac).as_deref(),
        Some("alt-shift-cmd-t")
    );
    assert_eq!(
        keystroke("Alt+Shift+F7", Platform::Mac).as_deref(),
        Some("alt-shift-f7")
    );
    assert_eq!(
        keystroke("Ctrl+Digit3", Platform::Linux).as_deref(),
        Some("ctrl-3")
    );
    assert_eq!(keystroke("Hyper+X", Platform::Mac), None);
}

#[test]
fn a_remap_replaces_the_default_and_bare_keys_stay_in_the_diff() {
    let translation = translate(&orca(), Platform::Mac);
    let unbind = &translation.sections[0]["unbind"];
    assert_eq!(
        unbind["f7"], "icarus::NextChange",
        "the default F7 gives way to ↓"
    );
    assert_eq!(unbind["shift-f7"], "icarus::PreviousChange");
    assert_eq!(unbind["alt-f7"], "icarus::NextChangedFile");
    assert_eq!(
        unbind["alt-cmd-]"], "icarus::ActivateNextItemOfSameType",
        "Orca's same-type default (⌥⌘]) is dropped"
    );
    assert_eq!(unbind["cmd-}"], "pane::ActivateNextItem");

    let diff = section(&translation, "IcarusDiffTab && diff_mode > Editor").expect("diff section");
    assert_eq!(diff["down"], "icarus::NextChange");
    assert_eq!(diff["up"], "icarus::PreviousChange");
    assert_eq!(diff["cmd-down"], "icarus::NextChangedFile");
    let outside = section(&translation, "Workspace && !Editor && !Terminal").expect("fallback");
    assert_eq!(
        outside["cmd-down"], "icarus::NextChangedFile",
        "keys with a modifier also work outside the diff editor"
    );
    assert!(
        outside.get("down").is_none(),
        "bare keys never leave the diff, so arrows keep moving the cursor elsewhere"
    );

    let workspace = section(&translation, "Workspace").expect("global section");
    assert_eq!(workspace["cmd-}"], "icarus::ActivateNextItemOfSameType");
    assert_eq!(workspace["alt-cmd-]"], "pane::ActivateNextItem");
    assert_eq!(workspace["ctrl-g"], "icarus::CopyPullRequestUrl");
    assert_eq!(
        section(&translation, "Editor").expect("editor")["alt-cmd-]"],
        "pane::ActivateNextItem",
        "global keys also win over the editor's own bindings"
    );
    assert_eq!(
        section(&translation, "Editor && mode == full").expect("show diff")["ctrl-d"],
        "icarus::ShowDiff"
    );
}

#[test]
fn tab_index_expands_to_nine_tabs_and_lists_report_what_happened() {
    let translation = translate(&orca(), Platform::Mac);
    let workspace = section(&translation, "Workspace").expect("global section");
    assert_eq!(workspace["alt-1"], json!(["pane::ActivateItem", 0]));
    assert_eq!(workspace["alt-9"], json!(["pane::ActivateItem", 8]));
    assert_eq!(
        translation.sections[0]["unbind"]["ctrl-5"],
        json!(["pane::ActivateItem", 4])
    );
    assert_eq!(
        workspace["alt-cmd-w"],
        json!(["pane::CloseActiveItem", { "close_pinned": false }]),
        "the cross-platform map applies too"
    );

    let skipped: Vec<&str> = translation
        .skipped
        .iter()
        .map(|s| s.orca.as_str())
        .collect();
    assert!(skipped.contains(&"tab.rename"), "{skipped:?}");
    assert!(
        skipped.contains(&"made.up.action"),
        "unknown actions are reported"
    );
    let next_change = translation
        .mapped
        .iter()
        .find(|m| m.orca == "editor.nextChange")
        .expect("mapped");
    assert_eq!(next_change.keystrokes, ["down"]);
    assert_eq!(next_change.unbound, ["f7"]);
}

#[test]
fn linux_uses_its_own_map_and_ctrl_for_mod() {
    let translation = translate(&orca(), Platform::Linux);
    let diff = section(&translation, "IcarusDiffTab && diff_mode > Editor").expect("diff");
    assert_eq!(diff["ctrl-j"], "icarus::NextChange");
    assert!(
        translation
            .mapped
            .iter()
            .all(|m| m.orca != "editor.nextFile")
    );
}

#[test]
fn quick_commands_and_the_switcher_map_to_their_icarus_actions() {
    let orca = json!({ "version": 1, "platforms": { "darwin": {
        "tab.runQuickCommand": ["Mod+R"],
        "tab.openQuickCommandsMenu": ["Ctrl+P"],
        "worktree.palette": ["Mod+Alt+J"],
    } } });
    let translation = translate(&orca, Platform::Mac);
    let workspace = section(&translation, "Workspace").expect("global section");
    assert_eq!(workspace["cmd-r"], "icarus::RunQuickCommand");
    assert_eq!(workspace["ctrl-p"], "icarus::OpenQuickCommandsMenu");
    assert_eq!(workspace["alt-cmd-j"], "icarus::OpenWorkspaceSwitcher");
    assert_eq!(
        translation.sections[0]["unbind"]["cmd-j"], "icarus::OpenWorkspaceSwitcher",
        "the switcher's default gives way to the remap"
    );
    assert!(translation.skipped.is_empty(), "{:?}", translation.skipped);
}

#[test]
fn terminal_panes_and_group_focus_map_to_their_actions() {
    let orca = json!({ "version": 1, "platforms": { "darwin": {
        "terminal.splitRight": ["Mod+Alt+D"],
        "terminal.focusNextPane": ["Ctrl+N"],
        "terminal.expandPane": ["Mod+Alt+Enter"],
        "tabGroup.focusLeft": ["Ctrl+H"],
    } } });
    let translation = translate(&orca, Platform::Mac);
    let terminal = section(&translation, "Terminal").expect("terminal section");
    assert_eq!(terminal["alt-cmd-d"], "pane::SplitRight");
    assert_eq!(terminal["ctrl-n"], "icarus::FocusNextTerminalPane");
    assert_eq!(terminal["alt-cmd-enter"], "workspace::ToggleZoom");
    assert_eq!(
        section(&translation, "Workspace").expect("global section")["ctrl-h"],
        "icarus::FocusGroupLeft"
    );
    let unbind = &translation.sections[0]["unbind"];
    assert_eq!(unbind["cmd-d"], "pane::SplitRight", "Orca's ⌘D gives way");
    assert_eq!(unbind["cmd-]"], "icarus::FocusNextTerminalPane");
    assert_eq!(unbind["alt-cmd-left"], "icarus::FocusGroupLeft");
    assert!(translation.skipped.is_empty(), "{:?}", translation.skipped);
}

#[test]
fn an_empty_list_unbinds_the_default_without_a_new_key() {
    let orca = json!({ "version": 1, "platforms": { "darwin": { "editor.showEdit": [] } } });
    let translation = translate(&orca, Platform::Mac);
    assert_eq!(translation.sections.len(), 1, "only the unbind");
    assert_eq!(
        translation.sections[0]["unbind"]["alt-cmd-e"],
        "icarus::ShowEdit"
    );
}

#[test]
fn applying_merges_into_the_keymap_and_importing_again_replaces_its_own_sections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let keymap = dir.path().join("keymap.json");
    let mine = json!({ "context": "Editor", "bindings": { "ctrl-x ctrl-s": "workspace::Save" } });
    std::fs::write(&keymap, format!("// my keymap\n[\n  {mine},\n]\n")).expect("write");

    let first =
        keymap::apply(dir.path(), &translate(&orca(), Platform::Mac), false).expect("apply");
    assert_eq!(first.replaced, 0);
    assert!(
        first.backup.is_some(),
        "the commented original is saved before rewriting"
    );
    let written: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&keymap).expect("read")).expect("json");
    assert_eq!(
        written[0], mine,
        "the user's own sections stay first and untouched"
    );
    assert_eq!(written.len(), 1 + first.added);
    assert!(dir.path().join(IMPORT_RECORD_FILE).exists());

    let changed =
        json!({ "version": 1, "platforms": { "darwin": { "editor.nextChange": ["ArrowRight"] } } });
    let second =
        keymap::apply(dir.path(), &translate(&changed, Platform::Mac), false).expect("again");
    assert_eq!(
        second.replaced, first.added,
        "the last import's sections are replaced"
    );
    let written: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&keymap).expect("read")).expect("json");
    assert_eq!(written.len(), 1 + second.added);
    assert_eq!(written[0], mine);
    let text = std::fs::read_to_string(&keymap).expect("read");
    assert!(
        text.contains("\"right\"") && !text.contains("\"down\""),
        "{text}"
    );

    let third =
        keymap::apply(dir.path(), &translate(&changed, Platform::Mac), false).expect("same");
    let again: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&keymap).expect("read")).expect("json");
    assert_eq!(third.replaced, third.added);
    assert_eq!(
        again, written,
        "importing the same file twice changes nothing"
    );
}

#[test]
fn a_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let applied =
        keymap::apply(dir.path(), &translate(&orca(), Platform::Mac), true).expect("dry run");
    assert!(applied.added > 0);
    assert!(!dir.path().join("keymap.json").exists());
    assert!(!dir.path().join(IMPORT_RECORD_FILE).exists());
}
