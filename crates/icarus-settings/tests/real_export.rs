use std::path::PathBuf;

use icarus_settings::import::{export_orca, import_orca};
use icarus_settings::json;
use icarus_settings::orca::OrcaExport;

// The reference export is private and gitignored; the test is a no-op wherever it is absent.
#[test]
fn reference_export_round_trips_when_present() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scratch/orca-settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let export = OrcaExport::parse(&text).expect("reference export parses");
    assert!(
        export.to_text() == text,
        "parse/serialize changed the bytes"
    );
    assert!(export.workspace_statuses().is_ok());
    assert!(export.status_rules().is_ok());
    assert!(export.projects().is_ok());
    assert!(export.keybindings().is_ok());
    assert!(export.quick_commands().is_ok());
    assert!(export.language_servers().is_ok());
    assert!(export.worktree_card_properties().is_ok());
    assert!(
        export.portable().to_text() == text,
        "the reference export is already in Orca's portable form"
    );

    let import = import_orca(&export);
    assert!(
        import.report.validation.is_empty(),
        "{} validation errors",
        import.report.validation.len()
    );
    let back = export_orca(&import.user, &import.projects).expect("valid export");
    assert!(back.to_text() == text, "import/export changed the bytes");

    let user = json::parse(&import.user.to_pretty_string()).expect("user doc reparses");
    let projects = import
        .projects
        .iter()
        .map(|(remote, doc)| {
            (
                remote.clone(),
                json::parse(&doc.to_pretty_string()).expect("project doc reparses"),
            )
        })
        .collect();
    let back = export_orca(&user, &projects).expect("valid export");
    assert!(
        back.to_text() == text,
        "import/export via files changed the bytes"
    );
}
