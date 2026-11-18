use std::path::PathBuf;

use super::*;
use crate::file_finder_tests::{init_test, open_file_picker};
use gpui::{TestAppContext, VisualTestContext};
use menu::Confirm;
use serde_json::json;
use util::path;
use workspace::{MultiWorkspace, OpenOptions, OpenVisible};

async fn open_finder(
    cx: &mut TestAppContext,
) -> (
    Entity<Picker<FileFinderDelegate>>,
    Entity<Workspace>,
    &mut VisualTestContext,
) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a.rs": "fn one() {}\n    let hello = 1;\nhello(); hello();\n",
                "b": {
                    "c.rs": "// HELLO there\n",
                },
                "d.rs": "nothing here\n",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let picker = open_file_picker(&workspace, cx);
    (picker, workspace, cx)
}

fn set_query(picker: &Entity<Picker<FileFinderDelegate>>, query: &str, cx: &mut VisualTestContext) {
    picker.update_in(cx, |picker, window, cx| picker.set_query(query, window, cx));
    cx.executor().advance_clock(TEXT_SEARCH_DEBOUNCE);
    cx.run_until_parked();
}

fn text_rows(
    picker: &Entity<Picker<FileFinderDelegate>>,
    cx: &mut VisualTestContext,
) -> Vec<String> {
    picker.read_with(cx, |picker, _| {
        picker
            .delegate
            .text_search
            .rows
            .iter()
            .map(|row| match row {
                TextRow::File { path, match_count } => {
                    format!("{} ({match_count})", path.path.as_unix_str())
                }
                TextRow::Match(text_match) => {
                    let (text, range) = text_match.preview.text_and_match_range();
                    format!(
                        "  {}: {}[{}]{}",
                        text_match.point_range.start.row + 1,
                        &text[..range.start],
                        &text[range.clone()],
                        &text[range.end..]
                    )
                }
            })
            .collect()
    })
}

fn sorted_groups(rows: Vec<String>) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    for row in rows {
        if row.starts_with("  ") {
            if let Some(group) = groups.last_mut() {
                group.push(row);
            }
        } else {
            groups.push(vec![row]);
        }
    }
    groups.sort();
    groups
}

fn no_matches_text(
    picker: &Entity<Picker<FileFinderDelegate>>,
    cx: &mut VisualTestContext,
) -> Option<SharedString> {
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.no_matches_text(window, cx)
    })
}

#[gpui::test]
async fn test_text_search_lists_each_file_followed_by_its_matches(cx: &mut TestAppContext) {
    let (picker, _workspace, cx) = open_finder(cx).await;
    set_query(&picker, "%hello", cx);

    assert_eq!(
        sorted_groups(text_rows(&picker, cx)),
        vec![
            vec![
                "a.rs (3)".to_string(),
                "  2: let [hello] = 1;".to_string(),
                "  3: [hello](); hello();".to_string(),
                "  3: hello(); [hello]();".to_string(),
            ],
            vec![
                "b/c.rs (1)".to_string(),
                "  1: // [HELLO] there".to_string(),
            ],
        ]
    );
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 6);
        assert!(!picker.delegate.text_search.searching);
        assert!(!picker.delegate.text_search.truncated);
    });
}

#[gpui::test]
async fn test_text_search_opens_the_match_at_its_line_and_column(cx: &mut TestAppContext) {
    let (picker, workspace, cx) = open_finder(cx).await;
    set_query(&picker, "%hello", cx);

    let target = picker.read_with(cx, |picker, _| {
        picker
            .delegate
            .text_search
            .rows
            .iter()
            .position(|row| {
                matches!(row, TextRow::Match(text_match)
                    if text_match.path.path.as_unix_str() == "a.rs"
                        && text_match.point_range.start == Point::new(2, 9))
            })
            .expect("second match on line 3 of a.rs")
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.set_selected_index(target, None, true, window, cx)
    });
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    let editor = cx.update(|_, cx| {
        workspace
            .read(cx)
            .active_item_as::<Editor>(cx)
            .expect("the match opens an editor")
    });
    editor.update(cx, |editor, cx| {
        assert_eq!(editor.title(cx), "a.rs");
        let selections = editor.selections.all_adjusted(&editor.display_snapshot(cx));
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].start, Point::new(2, 9));
        assert_eq!(selections[0].end, Point::new(2, 14));
    });
    cx.read(|cx| {
        assert!(
            workspace.read(cx).active_modal::<FileFinder>(cx).is_none(),
            "opening a match closes the finder"
        );
    });
}

#[gpui::test]
async fn test_text_search_hints_and_replaces_stale_results(cx: &mut TestAppContext) {
    let (picker, _workspace, cx) = open_finder(cx).await;

    set_query(&picker, "%", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 0));
    assert_eq!(
        no_matches_text(&picker, cx).as_deref(),
        Some("Type text to search for in every file.")
    );

    set_query(&picker, "%hello", cx);
    assert_eq!(text_rows(&picker, cx).len(), 6);

    picker.update_in(cx, |picker, window, cx| {
        picker.set_query("%nothing", window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        text_rows(&picker, cx).len(),
        6,
        "previous rows stay while the new search is debounced"
    );
    cx.executor().advance_clock(TEXT_SEARCH_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(
        text_rows(&picker, cx),
        vec!["d.rs (1)".to_string(), "  1: [nothing] here".to_string()]
    );

    set_query(&picker, "%no such text", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 0));
    assert_eq!(
        no_matches_text(&picker, cx).as_deref(),
        Some("No matching text.")
    );

    set_query(&picker, "a.rs", cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.mode, QueryMode::Files);
        assert!(picker.delegate.text_search.rows.is_empty());
        assert!(picker.delegate.match_count() > 0);
    });
}

#[gpui::test]
async fn test_text_search_caps_the_number_of_matches(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    let many_lines = "needle\n".repeat(TEXT_SEARCH_MAX_MATCHES + 50);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "big.txt": many_lines }))
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let picker = open_file_picker(&workspace, cx);

    set_query(&picker, "%needle", cx);
    picker.read_with(cx, |picker, _| {
        let text_search = &picker.delegate.text_search;
        assert_eq!(text_search.match_rows, TEXT_SEARCH_MAX_MATCHES);
        assert_eq!(text_search.rows.len(), TEXT_SEARCH_MAX_MATCHES + 1);
        assert!(text_search.truncated);
        assert!(!text_search.searching);
    });
}

#[gpui::test]
async fn test_go_to_line_moves_the_caret_in_the_active_editor(cx: &mut TestAppContext) {
    let (picker, workspace, cx) = open_finder(cx).await;
    set_query(&picker, ":2", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 0));
    assert_eq!(
        no_matches_text(&picker, cx).as_deref(),
        Some("Open a file to go to a line.")
    );
    cx.dispatch_action(menu::Cancel);

    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                PathBuf::from(path!("/root/a.rs")),
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..OpenOptions::default()
                },
                window,
                cx,
            )
        })
        .await
        .expect("open a.rs");
    cx.run_until_parked();

    let picker = open_file_picker(&workspace, cx);
    set_query(&picker, ":", cx);
    assert_eq!(
        no_matches_text(&picker, cx).as_deref(),
        Some("Type a line number to go to in the current file.")
    );
    set_query(&picker, ":2:5", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 1));
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    let editor = cx.update(|_, cx| {
        workspace
            .read(cx)
            .active_item_as::<Editor>(cx)
            .expect("a.rs stays active")
    });
    editor.update(cx, |editor, cx| {
        let selections = editor.selections.all_adjusted(&editor.display_snapshot(cx));
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].start, Point::new(1, 4));
        assert_eq!(selections[0].end, Point::new(1, 4));
    });
    cx.read(|cx| {
        assert!(workspace.read(cx).active_modal::<FileFinder>(cx).is_none());
    });
}

#[gpui::test]
async fn test_file_names_rank_in_orca_tiers(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                ".gitignore": "lib/button",
                "button": { "index.ts": "" },
                "lib": { "b_u_t_t_o_n.rs": "", "button": "" },
                "node_modules": { "ui": { "button": "" } },
                "src": {
                    "button": "",
                    "button_group.rs": "",
                    "my_button_test.rs": "",
                },
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let picker = open_file_picker(&workspace, cx);
    picker.update(cx, |picker, _| picker.delegate.include_ignored = Some(true));

    picker
        .update_in(cx, |picker, window, cx| {
            picker
                .delegate
                .spawn_search(parse_file_search_query("button"), window, cx)
        })
        .await;
    let paths = picker.read_with(cx, |picker, _| {
        picker
            .delegate
            .matches
            .matches
            .iter()
            .filter_map(|entry| {
                entry
                    .relative_path()
                    .map(|path| path.as_unix_str().to_owned())
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(
        paths,
        [
            "src/button",
            "lib/button",
            "node_modules/ui/button",
            "src/button_group.rs",
            "src/my_button_test.rs",
            "lib/b_u_t_t_o_n.rs",
            "button/index.ts",
        ]
    );
}

struct FakeDiffs {
    opened: std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>>,
}

impl ChangedFilesSource for FakeDiffs {
    fn changed_files(&self, _: &Workspace, _: &App) -> HashMap<PathBuf, ChangedFile> {
        HashMap::from_iter([(
            PathBuf::from(path!("/root/a.rs")),
            ChangedFile {
                status: "M".into(),
                status_color: Color::Modified,
                added: Some(3),
                removed: Some(1),
            },
        )])
    }

    fn open_diff(
        &self,
        abs_path: PathBuf,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<Task<anyhow::Result<Entity<Editor>>>> {
        self.opened.borrow_mut().push(abs_path.clone());
        let open = workspace.open_abs_path(abs_path, OpenOptions::default(), window, cx);
        Some(cx.spawn(async move |_, _| {
            open.await?
                .downcast::<Editor>()
                .context("the fake diff is a plain editor")
        }))
    }
}

#[gpui::test]
async fn test_changed_files_open_as_the_diff_at_the_match(cx: &mut TestAppContext) {
    let (picker, workspace, cx) = open_finder(cx).await;
    cx.dispatch_action(menu::Cancel);
    drop(picker);
    let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        set_changed_files_source(
            FakeDiffs {
                opened: opened.clone(),
            },
            cx,
        )
    });

    let picker = open_file_picker(&workspace, cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.changed_files.len(), 1);
    });
    set_query(&picker, "%hello", cx);
    let target = picker.read_with(cx, |picker, _| {
        picker
            .delegate
            .text_search
            .rows
            .iter()
            .position(|row| {
                matches!(row, TextRow::Match(text_match)
                    if text_match.point_range.start == Point::new(1, 8))
            })
            .expect("the match on line 2 of a.rs")
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.set_selected_index(target, None, true, window, cx)
    });
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(*opened.borrow(), [PathBuf::from(path!("/root/a.rs"))]);
    let editor = cx.update(|_, cx| {
        workspace
            .read(cx)
            .active_item_as::<Editor>(cx)
            .expect("the diff opens")
    });
    editor.update(cx, |editor, cx| {
        let selections = editor.selections.all_adjusted(&editor.display_snapshot(cx));
        assert_eq!(selections[0].start, Point::new(1, 8));
    });

    let picker = open_file_picker(&workspace, cx);
    set_query(&picker, "d.rs", cx);
    cx.dispatch_action(Confirm);
    cx.run_until_parked();
    assert_eq!(
        opened.borrow().len(),
        1,
        "an unchanged file opens as a plain file"
    );
}
