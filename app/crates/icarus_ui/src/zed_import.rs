//! Offers once to bring a normal Zed install's setup into Icarus; `icarus: import zed setup` runs
//! it any time. The import reads and writes this machine's folders, so the app runs it itself
//! instead of asking the (possibly remote) Icarus server.

use std::path::PathBuf;
use std::sync::Arc;

use db::kvp::KeyValueStore;
use extension_host::ExtensionStore;
use gpui::{Action as _, App, AppContext as _, Context, Global, TaskExt as _, actions};
use icarus_settings::zed_import::{self, FileStatus, ZedImport};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;

actions!(
    icarus,
    [
        /// Imports settings, keymap, snippets, themes and extensions from a normal Zed install.
        ImportZedSetup,
    ]
);

const PROMPTED_KEY: &str = "icarus_zed_import_prompted";

struct ImportPrompt;
struct ImportResult;

/// Where the Zed install is read from and the app's config directory it is imported into.
#[derive(Clone, Debug)]
pub struct ZedImportSources {
    pub config: PathBuf,
    pub data: Option<PathBuf>,
    pub target: PathBuf,
}

impl Global for ZedImportSources {}

#[derive(Default)]
struct PromptChecked;

impl Global for PromptChecked {}

fn sources(cx: &App) -> Option<ZedImportSources> {
    if let Some(sources) = cx.try_global::<ZedImportSources>() {
        return Some(sources.clone());
    }
    Some(ZedImportSources {
        config: zed_import::source_config_dir()?,
        data: zed_import::source_data_dir(),
        target: paths::config_dir().clone(),
    })
}

/// Whether the first-run prompt should be shown for this dry run.
pub fn should_prompt(already_prompted: bool, plan: &ZedImport) -> bool {
    !already_prompted && plan.found_anything() && plan.changes_anything()
}

fn remember_prompted(cx: &App) {
    let store = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        store
            .write_kvp(PROMPTED_KEY.to_string(), "1".to_string())
            .await
    });
}

fn run_import(
    sources: ZedImportSources,
    dry_run: bool,
    cx: &App,
) -> gpui::Task<anyhow::Result<ZedImport>> {
    cx.background_spawn(async move {
        Ok(zed_import::import(
            &sources.config,
            sources.data.as_deref(),
            &sources.target,
            dry_run,
        )?)
    })
}

fn prompt_if_needed(cx: &mut Context<Workspace>) {
    if cx.has_global::<PromptChecked>() {
        return;
    }
    cx.set_global(PromptChecked);
    let already_prompted = KeyValueStore::global(cx)
        .read_kvp(PROMPTED_KEY)
        .log_err()
        .flatten()
        .is_some();
    if already_prompted {
        return;
    }
    let Some(sources) = sources(cx) else {
        return;
    };
    let plan = run_import(sources, true, cx);
    cx.spawn(async move |workspace, cx| {
        let plan = plan.await?;
        if !should_prompt(already_prompted, &plan) {
            return anyhow::Ok(());
        }
        workspace.update(cx, |workspace, cx| {
            remember_prompted(cx);
            let message = format!("Import your Zed setup ({})?", plan.headline());
            workspace.show_notification(NotificationId::unique::<ImportPrompt>(), cx, |cx| {
                cx.new(|cx| {
                    MessageNotification::new(message, cx)
                        .primary_message("Import")
                        .primary_on_click(|window, cx| {
                            window.dispatch_action(ImportZedSetup.boxed_clone(), cx);
                        })
                        .secondary_message("Not now")
                })
            });
        })
    })
    .detach_and_log_err(cx);
}

fn result_message(report: &ZedImport) -> String {
    if !report.found_anything() {
        return format!("No Zed setup found in {}.", report.source.display());
    }
    if !report.changes_anything() {
        return "Your Zed setup is already imported.".to_string();
    }
    let counted = |count: usize, noun: &str| match count {
        1 => format!("1 {noun}"),
        count => format!("{count} {noun}s"),
    };
    let mut parts = Vec::new();
    if !report.added.is_empty() {
        parts.push(counted(report.added.len(), "setting"));
    }
    if !report.extensions_added.is_empty() {
        parts.push(format!(
            "{} (installing)",
            counted(report.extensions_added.len(), "extension")
        ));
    }
    let copied = report
        .files
        .iter()
        .filter(|file| file.status == FileStatus::Copied)
        .count();
    if copied > 0 {
        parts.push(counted(copied, "file"));
    }
    let mut message = format!("Imported from Zed: {}.", parts.join(", "));
    let mut skipped: Vec<&str> = report
        .files
        .iter()
        .filter(|file| file.status == FileStatus::Skipped)
        .map(|file| file.kind)
        .collect();
    skipped.dedup();
    if !skipped.is_empty() {
        message.push_str(&format!(
            "\nKept Icarus's own {} files.",
            skipped.join(", ")
        ));
    }
    if !report.kept.is_empty() {
        message.push_str(&format!(
            "\nKept {} settings Icarus already had.",
            report.kept.len()
        ));
    }
    if !report.dev_extensions.is_empty() {
        let ids: Vec<&str> = report
            .dev_extensions
            .iter()
            .map(|dev| dev.id.as_str())
            .collect();
        message.push_str(&format!(
            "\nDev extensions aren't imported: {}.",
            ids.join(", ")
        ));
    }
    message
}

fn import_zed_setup(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    remember_prompted(cx);
    workspace.dismiss_notification(&NotificationId::unique::<ImportPrompt>(), cx);
    let Some(sources) = sources(cx) else {
        show_result(
            workspace,
            "No home directory to find Zed's settings in.".into(),
            cx,
        );
        return;
    };
    let import = run_import(sources, false, cx);
    cx.spawn(async move |workspace, cx| {
        let result = import.await;
        workspace.update(cx, |workspace, cx| {
            let message = match result {
                Ok(report) => {
                    install_extensions(&report.extensions_added, cx);
                    result_message(&report)
                }
                Err(error) => format!("Importing your Zed setup failed: {error:#}"),
            };
            show_result(workspace, message, cx);
        })
    })
    .detach_and_log_err(cx);
}

// Settings and keymaps reload when their files change, but auto-installs only run at startup.
fn install_extensions(ids: &[String], cx: &mut App) {
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    store.update(cx, |store, cx| {
        for id in ids {
            if !store.installed_extensions().contains_key(id.as_str()) {
                store.install_latest_extension(Arc::from(id.as_str()), cx);
            }
        }
    });
}

fn show_result(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_notification(NotificationId::unique::<ImportResult>(), cx, |cx| {
        cx.new(|cx| MessageNotification::new(message, cx))
    });
}

pub(crate) fn register(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ImportZedSetup, _window, cx| {
            import_zed_setup(workspace, cx);
        });
        if window.is_some() {
            prompt_if_needed(cx);
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use workspace::MultiWorkspace;

    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dir");
        }
        std::fs::write(path, text).expect("write");
    }

    fn zed_install(root: &Path) -> ZedImportSources {
        write(
            &root.join("zed/settings.json"),
            "// mine\n{\"theme\": \"Gruvbox Dark\", \"vim_mode\": true}\n",
        );
        write(
            &root.join("zed-data/extensions/installed/gruvbox/extension.toml"),
            "id = \"gruvbox\"\n",
        );
        ZedImportSources {
            config: root.join("zed"),
            data: Some(root.join("zed-data")),
            target: root.join("icarus"),
        }
    }

    async fn open_workspace(
        sources: ZedImportSources,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.set_global(db::AppDatabase::test_new());
            cx.set_global(sources);
            register(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "a.rs": "" })).await;
        let project = project::Project::test(fs, [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (workspace, cx)
    }

    fn prompted(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, _| {
            workspace.has_notification(&NotificationId::unique::<ImportPrompt>())
        })
    }

    #[gpui::test]
    async fn a_zed_install_is_offered_once_and_imports_on_request(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let sources = zed_install(root.path());
        let (workspace, mut cx) = open_workspace(sources.clone(), cx).await;
        assert!(prompted(&workspace, &mut cx));
        assert!(!sources.target.exists(), "the prompt itself writes nothing");
        let remembered = cx.update(|_, cx| KeyValueStore::global(cx).read_kvp(PROMPTED_KEY));
        assert_eq!(remembered.expect("kvp").as_deref(), Some("1"));

        cx.dispatch_action(ImportZedSetup);
        cx.run_until_parked();
        assert!(!prompted(&workspace, &mut cx));
        let settings =
            std::fs::read_to_string(sources.target.join("settings.json")).expect("settings");
        assert!(settings.contains("\"vim_mode\": true"), "{settings}");
        assert!(settings.contains("\"gruvbox\": true"), "{settings}");
        assert!(workspace.read_with(&cx, |workspace, _| {
            workspace.has_notification(&NotificationId::unique::<ImportResult>())
        }));
    }

    #[gpui::test]
    async fn no_prompt_without_a_zed_install(cx: &mut TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let sources = ZedImportSources {
            config: root.path().join("zed"),
            data: None,
            target: root.path().join("icarus"),
        };
        let (workspace, mut cx) = open_workspace(sources, cx).await;
        assert!(!prompted(&workspace, &mut cx));
    }

    #[test]
    fn the_prompt_shows_only_when_there_is_something_new() {
        let root = tempfile::tempdir().expect("tempdir");
        let sources = zed_install(root.path());
        let plan = |dry_run| {
            zed_import::import(
                &sources.config,
                sources.data.as_deref(),
                &sources.target,
                dry_run,
            )
            .expect("import")
        };
        assert!(should_prompt(false, &plan(true)));
        assert!(!should_prompt(true, &plan(true)));
        assert_eq!(
            result_message(&plan(false)),
            "Imported from Zed: 2 settings, 1 extension (installing)."
        );
        assert!(!should_prompt(false, &plan(true)));
        assert_eq!(
            result_message(&plan(false)),
            "Your Zed setup is already imported."
        );
    }
}
