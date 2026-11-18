//! Latency budgets for the flows the user drives all day, measured through the real UI: clicks and
//! keys go in through the window, and a flow is done when its result is in a drawn frame.
//! Each prints a `PERF` line that `scripts/dev/perf.sh` collects into a table.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, DebugElement, Entity, TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::changes_tab::{Listing, NextChangedFile, SourceControlPanel};
use crate::driver::{Selector, click, tree, type_text};
use crate::review::{ComposerTarget, Destination};

struct Timing {
    /// On the executor's fake clock: what timers and debounces add.
    simulated: Duration,
    /// Real time spent rendering and computing, in a debug build.
    wall: Duration,
}

struct Started {
    simulated: std::time::Instant,
    wall: Instant,
}

fn start(cx: &mut VisualTestContext) -> Started {
    Started {
        simulated: cx.executor().now(),
        wall: Instant::now(),
    }
}

fn check(name: &str, timing: &Timing, simulated_budget: Duration, wall_budget: Duration) {
    println!(
        "PERF\tapp\t{name}\t{:.1}\t{}\t{}",
        timing.wall.as_secs_f64() * 1000.,
        wall_budget.as_millis(),
        timing.simulated.as_millis()
    );
    assert!(
        timing.simulated <= simulated_budget,
        "{name}: waited {:?} on timers, over its {simulated_budget:?} budget",
        timing.simulated
    );
    assert!(
        timing.wall <= wall_budget,
        "{name}: took {:?}, over its {wall_budget:?} budget",
        timing.wall
    );
}

fn drawn(cx: &mut VisualTestContext) -> Vec<DebugElement> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.debug_elements().to_vec()
    })
}

fn shows(elements: &[DebugElement], selector: &str) -> bool {
    Selector::parse(selector)
        .ok()
        .and_then(|selector| selector.find(elements))
        .is_some()
}

fn click_on(selector: &str, cx: &mut VisualTestContext) {
    let elements = drawn(cx);
    let bounds = Selector::parse(selector)
        .ok()
        .and_then(|parsed| parsed.find(&elements))
        .unwrap_or_else(|| panic!("{selector} is not on screen: {:#?}", tree(&elements)));
    cx.update(|window, cx| click(window, bounds.center(), cx));
}

/// Runs the app a fake millisecond at a time until `done` holds for a drawn frame.
fn run_until(
    started: Started,
    limit: Duration,
    cx: &mut VisualTestContext,
    mut done: impl FnMut(&[DebugElement], &mut VisualTestContext) -> bool,
) -> Timing {
    loop {
        cx.run_until_parked();
        let elements = drawn(cx);
        let simulated = cx
            .executor()
            .now()
            .saturating_duration_since(started.simulated);
        if done(&elements, cx) {
            return Timing {
                simulated,
                wall: started.wall.elapsed(),
            };
        }
        assert!(
            simulated < limit,
            "not done after {simulated:?}: {:#?}",
            tree(&elements)
        );
        cx.executor().advance_clock(Duration::from_millis(1));
    }
}

fn file(index: usize) -> String {
    format!("src/f{index:02}.rs")
}

/// A workspace on a repository whose `count` files all changed since the merge base, with the
/// Source Control panel open and listing them.
async fn changed_files(
    count: usize,
    cx: &mut TestAppContext,
) -> (
    Entity<Workspace>,
    Entity<SourceControlPanel>,
    VisualTestContext,
) {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        cx.set_global(db::AppDatabase::test_new());
        crate::changes_tab::register(cx);
        crate::sidebar::register(cx);
    });
    let fs = fs::FakeFs::new(cx.executor());
    let current = |index: usize| format!("fn f() {{}}\nnew line {index}\n");
    let files: serde_json::Map<String, Value> = (0..count)
        .map(|index| (format!("f{index:02}.rs"), json!(current(index))))
        .collect();
    fs.insert_tree(
        "/project",
        json!({ ".git": {}, "src": Value::Object(files) }),
    )
    .await;
    let paths: Vec<String> = (0..count).map(file).collect();
    let head: Vec<(&str, String)> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.as_str(), current(index)))
        .collect();
    let base: Vec<(&str, String)> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.as_str(), format!("fn f() {{}}\nold line {index}\n")))
        .collect();
    let dot_git = Path::new("/project/.git");
    fs.set_head_and_index_for_repo(dot_git, &head);
    fs.set_merge_base_content_for_repo(dot_git, &base);
    let project = project::Project::test(fs, [Path::new("/project")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let listing: Listing = serde_json::from_value(json!({
        "compare": { "status": "ready", "base": "origin/main" },
        "rows": paths.iter().map(|path| json!({
            "path": path, "status": "modified", "added": 1, "removed": 1, "unstaged": true,
        })).collect::<Vec<_>>(),
    }))
    .expect("listing");
    let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
        let panel = cx.new(|cx| SourceControlPanel::new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        workspace.open_panel::<SourceControlPanel>(window, cx);
        panel
    });
    panel.update(&mut cx, |panel, cx| {
        panel.show_listing_for_tests(listing, cx)
    });
    cx.run_until_parked();
    (workspace, panel, cx)
}

fn diff_tabs(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<crate::DiffTab>(cx)
            .map(|tab| tab.read(cx).path().to_path_buf())
            .collect()
    })
}

#[gpui::test]
async fn clicking_a_changed_file_draws_its_diff(cx: &mut TestAppContext) {
    let (_workspace, _panel, mut cx) = changed_files(3, cx).await;
    let cx = &mut cx;
    let started = start(cx);
    click_on(&format!("scm-row:{}", file(0)), cx);
    let timing = run_until(started, Duration::from_secs(2), cx, |elements, _| {
        shows(elements, &format!("diff-tab:{}", file(0))) && shows(elements, "text:new line 0")
    });
    check(
        "click a changed file → diff drawn",
        &timing,
        Duration::from_millis(50),
        Duration::from_secs(3),
    );

    let started = start(cx);
    click_on(&format!("scm-row:{}", file(1)), cx);
    let timing = run_until(started, Duration::from_secs(2), cx, |elements, _| {
        shows(elements, &format!("diff-tab:{}", file(1))) && shows(elements, "text:new line 1")
    });
    check(
        "click the next file → its diff drawn",
        &timing,
        Duration::from_millis(50),
        Duration::from_secs(2),
    );
}

#[gpui::test]
async fn a_held_file_key_steps_through_twenty_files_without_waiting_for_each_diff(
    cx: &mut TestAppContext,
) {
    let (workspace, panel, mut cx) = changed_files(20, cx).await;
    let cx = &mut cx;
    let started = start(cx);
    let mut slowest_press = Duration::ZERO;
    for repeat in 0..20 {
        let pressed = Instant::now();
        cx.update(|window, cx| window.dispatch_action(Box::new(NextChangedFile), cx));
        slowest_press = slowest_press.max(pressed.elapsed());
        // Key repeats come faster than a diff loads, so the loads are still in flight here.
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selected_for_tests()),
            Some(repeat),
            "repeat {repeat} moved the selection at once"
        );
    }
    let last = PathBuf::from("/project").join(file(19));
    let timing = run_until(started, Duration::from_secs(2), cx, |elements, cx| {
        shows(elements, &format!("diff-tab:{}", file(19)))
            && shows(elements, "text:new line 19")
            && diff_tabs(&workspace, cx) == std::slice::from_ref(&last)
    });
    check(
        "20 held-key steps → last diff drawn",
        &timing,
        Duration::from_millis(50),
        Duration::from_secs(4),
    );
    check(
        "slowest held-key step",
        &Timing {
            simulated: Duration::ZERO,
            wall: slowest_press,
        },
        Duration::ZERO,
        Duration::from_millis(500),
    );
}

#[gpui::test]
async fn every_held_key_repeat_draws_its_files_diff(cx: &mut TestAppContext) {
    let (workspace, panel, mut cx) = changed_files(20, cx).await;
    let cx = &mut cx;
    cx.run_until_parked();
    assert!(
        panel.read_with(cx, |panel, _| panel.ready_diff_count_for_tests()) > 0,
        "the files a first step lands on are loaded before it"
    );
    let key_repeat = Duration::from_millis(33);
    let mut slowest = Duration::ZERO;
    for repeat in 0..20 {
        let started = start(cx);
        cx.update(|window, cx| window.dispatch_action(Box::new(NextChangedFile), cx));
        let elements = drawn(cx);
        let wall = started.wall.elapsed();
        assert!(
            shows(&elements, &format!("diff-tab:{}", file(repeat)))
                && shows(&elements, &format!("text:new line {repeat}"))
                && shows(&elements, &format!("text:old line {repeat}")),
            "repeat {repeat} drew {}'s diff in the frame after the key: {:#?}",
            file(repeat),
            tree(&elements)
        );
        slowest = slowest.max(wall);
        // The next repeat arrives a key-repeat later; meanwhile the files ahead finish loading.
        cx.executor().advance_clock(key_repeat);
        cx.run_until_parked();
    }
    let last = PathBuf::from("/project").join(file(19));
    assert_eq!(
        diff_tabs(&workspace, cx),
        std::slice::from_ref(&last),
        "each step replaced the preview"
    );
    check(
        "slowest held-key repeat → its diff drawn",
        &Timing {
            simulated: Duration::ZERO,
            wall: slowest,
        },
        Duration::ZERO,
        Duration::from_millis(250),
    );
}

#[gpui::test]
async fn a_filter_toggle_redraws_the_list_in_the_same_frame(cx: &mut TestAppContext) {
    let (_workspace, panel, mut cx) = changed_files(20, cx).await;
    let cx = &mut cx;
    let kind = |index: usize| {
        if index.is_multiple_of(2) {
            "implementation"
        } else {
            "test"
        }
    };
    let rows: Vec<Value> = (0..20)
        .map(|index| {
            json!({
                "path": file(index), "status": "modified", "added": 1, "removed": 1,
                "unstaged": true, "facets": { "type": kind(index) },
            })
        })
        .collect();
    let listing: Listing = serde_json::from_value(json!({
        "compare": { "status": "ready", "base": "origin/main" },
        "rows": rows,
        "all_rows": rows,
        "totals": { "files": 20, "added": 20, "removed": 20 },
        "unfiltered": { "files": 20, "added": 20, "removed": 20 },
        "facets": [{
            "id": "type", "label": "Types",
            "values": [
                { "id": "implementation", "label": "Implementation", "present": true },
                { "id": "test", "label": "Tests", "present": true },
            ],
        }],
    }))
    .expect("listing");
    panel.update(cx, |panel, cx| panel.show_listing_for_tests(listing, cx));
    cx.run_until_parked();
    let elements = drawn(cx);
    assert!(shows(&elements, &format!("scm-row:{}", file(1))) && shows(&elements, "text:+20"));

    let started = start(cx);
    panel.update(cx, |panel, cx| {
        panel.toggle_facet_value("type".into(), "test".into(), cx)
    });
    let elements = drawn(cx);
    let wall = started.wall.elapsed();
    assert!(
        !shows(&elements, &format!("scm-row:{}", file(1)))
            && shows(&elements, &format!("scm-row:{}", file(0)))
            && shows(&elements, "text:+10"),
        "the next frame lists only implementation files: {:#?}",
        tree(&elements)
    );
    check(
        "filter toggle → list redrawn",
        &Timing {
            simulated: Duration::ZERO,
            wall,
        },
        Duration::ZERO,
        Duration::from_millis(100),
    );

    panel.update(cx, |panel, cx| {
        panel.toggle_facet_value("type".into(), "test".into(), cx)
    });
    let elements = drawn(cx);
    assert!(
        shows(&elements, &format!("scm-row:{}", file(1))) && shows(&elements, "text:+20"),
        "showing tests again brings them back at once"
    );
}

#[gpui::test]
async fn a_card_click_shows_its_workspace(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        AppState::set_global(app_state.clone(), cx);
        <project::DisableAiSettings as settings::Settings>::register(cx);
        editor::init(cx);
        project::trusted_worktrees::init(Default::default(), cx);
        crate::worktree_trust::register(cx);
        crate::worktree_trust::add_icarus_workspace_path(Path::new("/project"), cx);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/project", json!({ "a.txt": "" })).await;
    let mut big = serde_json::Map::new();
    for directory in 0..40 {
        let files: serde_json::Map<String, Value> = (0..20)
            .map(|file| (format!("f{file}.rs"), json!("")))
            .collect();
        big.insert(format!("d{directory}"), Value::Object(files));
    }
    fs.insert_tree("/w/feature", Value::Object(big)).await;
    let project = project::Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let cx = &mut cx;
    let add_cards = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
        workspace.update_in(cx, |workspace, window, cx| {
            crate::register_panel_actions(workspace);
            let panel = cx.new(|cx| crate::WorkspacesPanel::preview(workspace.weak_handle(), cx));
            panel.update(cx, |panel, cx| {
                panel.set_workspaces(
                    vec![
                        crate::tests::summary("main", "/project", None),
                        crate::tests::summary("feature", "/w/feature", None),
                    ],
                    cx,
                )
            });
            workspace.add_panel(panel, window, cx);
            workspace.open_panel::<crate::WorkspacesPanel>(window, cx);
        });
        cx.run_until_parked();
    };
    add_cards(&workspace, cx);
    let shown = move |path: &'static str| {
        move |_: &[DebugElement], cx: &mut VisualTestContext| {
            window
                .read_with(cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspace()
                        .read(cx)
                        .root_paths(cx)
                        .iter()
                        .any(|root| root.as_ref() == Path::new(path))
                })
                .unwrap_or(false)
        }
    };

    let started = start(cx);
    click_on("card:ws-feature", cx);
    let timing = run_until(started, Duration::from_secs(2), cx, shown("/w/feature"));
    check(
        "card click → new workspace shown (800 files)",
        &timing,
        Duration::from_millis(50),
        Duration::from_secs(3),
    );

    let feature = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("window");
    add_cards(&feature, cx);
    let started = start(cx);
    click_on("card:ws-main", cx);
    let timing = run_until(started, Duration::from_secs(2), cx, shown("/project"));
    check(
        "card click → open workspace switched",
        &timing,
        Duration::from_millis(50),
        Duration::from_secs(2),
    );
}

#[gpui::test]
async fn a_submitted_note_goes_to_the_agent_composer_at_once(cx: &mut TestAppContext) {
    cx.update(crate::enable_for_tests);
    let calls = cx.update(|cx| {
        crate::review::tests::stub_rpc(cx, |method, _| {
            Ok(match method {
                "note.add" => json!({
                    "id": "note-1", "path": file(0), "line": 2, "end_line": 2,
                    "author": "user", "body": "Rename this",
                }),
                "note.list" => json!([]),
                _ => json!({}),
            })
        })
    });
    let (workspace, _panel, mut cx) = changed_files(1, cx).await;
    let cx = &mut cx;
    click_on(&format!("scm-row:{}", file(0)), cx);
    cx.run_until_parked();
    let review = workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<crate::DiffTab>(cx)
                .next()
                .and_then(|tab| tab.read(cx).review().cloned())
        })
        .expect("the diff tab has its review layer");
    review.update_in(cx, |review, window, cx| {
        review.open_composer(
            ComposerTarget::Lines { start: 2, end: 2 },
            String::new(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    cx.update(|window, cx| type_text(window, "Rename this", cx));
    let typed = review.read_with(cx, |review, cx| {
        review
            .composer()
            .map(|composer| composer.read(cx).editor().read(cx).text(cx))
    });
    assert_eq!(
        typed.as_deref(),
        Some("Rename this"),
        "typing reaches the note box"
    );

    let started = start(cx);
    review.update_in(cx, |review, window, cx| {
        review.submit_composer(Destination::Agent, window, cx)
    });
    let timing = run_until(started, Duration::from_secs(1), cx, |_, _| {
        !crate::review::tests::calls_to(&calls, "attach.note").is_empty()
    });
    assert_eq!(
        crate::review::tests::calls_to(&calls, "attach.note"),
        [json!({ "id": "note-1" })]
    );
    check(
        "note submit → sent to the agent composer",
        &timing,
        Duration::from_millis(20),
        Duration::from_secs(1),
    );
}

/// Prints a `REDRAWS` row: the most notifies any watched view got, and how many files were written.
fn check_redraws(flow: &str, notifies: usize, budget: usize, writes: usize) {
    println!("REDRAWS\tapp\t{flow}\t{notifies}\t{budget}\t{writes}");
    assert!(
        notifies <= budget,
        "{flow}: {notifies} redraws, over its {budget} budget"
    );
    assert_eq!(writes, 0, "{flow}: wrote {writes} files");
}

fn agent(id: &str, state: &str, last_activity_ms: u64) -> crate::model::AgentSummary {
    crate::model::AgentSummary {
        id: id.into(),
        title: Some("Create PR".into()),
        state: state.into(),
        provider: "claude".into(),
        turns: 1,
        last_activity_ms,
        revision: 7,
        ..Default::default()
    }
}

#[gpui::test]
async fn redraw_budget_of_the_workspaces_sidebar(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        AppState::set_global(app_state.clone(), cx);
        <project::DisableAiSettings as settings::Settings>::register(cx);
        editor::init(cx);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/project", json!({ "a.txt": "" })).await;
    let project = project::Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let cx = &mut cx;
    let workspaces = |agents: Vec<crate::model::AgentSummary>| {
        let mut feature = crate::tests::summary("feature", "/w/feature", None);
        feature.agents = agents;
        vec![
            crate::tests::summary("main", "/project", None),
            feature,
            crate::tests::summary("fix", "/w/fix", None),
        ]
    };
    let panel = workspace.update_in(cx, |workspace, window, cx| {
        let panel = cx.new(|cx| crate::WorkspacesPanel::preview(workspace.weak_handle(), cx));
        workspace.add_panel(panel.clone(), window, cx);
        workspace.open_panel::<crate::WorkspacesPanel>(window, cx);
        panel
    });
    // A second window's panel, holding its own copy of the same agents.
    let other_panel = workspace.update(cx, |workspace, cx| {
        cx.new(|cx| crate::WorkspacesPanel::preview(workspace.weak_handle(), cx))
    });
    let notifies = std::rc::Rc::new(std::cell::Cell::new([0usize; 3]));
    let live_merges = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let _counters = cx.update(|_, cx| {
        let count = |slot: usize| {
            let notifies = notifies.clone();
            move || {
                let mut counts = notifies.get();
                counts[slot] += 1;
                notifies.set(counts);
            }
        };
        let (first, second, third) = (count(0), count(1), count(2));
        let live_merges = live_merges.clone();
        vec![
            cx.observe(&panel, move |_, _| first()),
            cx.observe(&other_panel, move |_, _| second()),
            cx.observe(&workspace, move |_, _| third()),
            cx.observe_global::<crate::model::LiveAgents>(move |_| {
                live_merges.set(live_merges.get() + 1);
                assert!(
                    live_merges.get() < 1000,
                    "the panels overwrite each other's agents forever"
                );
            }),
        ]
    });
    let writes = || {
        fs.files()
            .iter()
            .map(|path| fs.write_count_for_path(path))
            .sum::<usize>()
    };
    let measure = |flow: &str, budget: usize, cx: &mut VisualTestContext| {
        cx.run_until_parked();
        notifies.set([0; 3]);
        let writes_before = writes();
        for _ in 0..300 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
            drawn(cx);
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
        }
        let most = notifies.get().into_iter().max().unwrap_or(0);
        check_redraws(flow, most, budget, writes() - writes_before);
    };

    panel.update(cx, |panel, cx| {
        panel.set_workspaces(workspaces(Vec::new()), cx)
    });
    measure("sidebar idle, 5 s", 2, cx);

    let animated = vec![
        agent("a1", "working", 100),
        agent("a2", "working", 100),
        agent("a3", "waiting", 100),
        agent("a4", "idle", 100),
    ];
    panel.update(cx, |panel, cx| {
        panel.set_workspaces(workspaces(animated), cx)
    });
    measure("sidebar with animated agent states, 5 s", 2, cx);

    panel.update(cx, |panel, cx| {
        panel.set_workspaces(workspaces(vec![agent("pr", "working", 100)]), cx)
    });
    other_panel.update(cx, |panel, cx| {
        panel.set_workspaces(workspaces(vec![agent("pr", "working", 200)]), cx)
    });
    cx.update(|_, cx| {
        let pushed = agent("pr", "working", 150);
        crate::model::agent_state_pushed(
            json!({
                "id": pushed.id, "title": pushed.title, "state": pushed.state,
                "provider": pushed.provider, "turns": pushed.turns,
                "last_activity_ms": pushed.last_activity_ms, "revision": pushed.revision,
            }),
            cx,
        )
    });
    measure("two windows' panels after an agent.state push, 5 s", 2, cx);
    assert!(live_merges.get() < 20, "the panels settle on one copy");
}
