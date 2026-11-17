use super::imports::{Semantics, is_import_only_change};
use super::*;
use crate::tests::{Repo, repo, run, write};

const BASE: &str = "main";

fn overlay() -> Value {
    serde_json::json!({
        "facets": [
            { "id": "type", "rules": [
                { "id": "lint-baseline", "value": "lint-baseline", "before": "generated-unset",
                  "when": { "regex": r"(?i)(?:^|/)(?:\.ratchet|\.[^/]*baseline[^/]*)(?:/|$)" } }
            ] },
            { "id": "area", "label": "Areas",
              "values": [ { "id": "backend", "label": "Backend" }, { "id": "frontend", "label": "Frontend" } ],
              "rules": [
                { "value": "backend", "when": { "path": ["server/**"] } },
                { "value": "frontend", "when": { "path": "web/**" } }
              ] }
        ]
    })
}

fn generic() -> Classifier {
    Classifier::new(FilterConfig::preset()).expect("preset compiles")
}

fn project() -> Classifier {
    Classifier::new(
        FilterConfig::preset()
            .with_overlay(&overlay())
            .expect("overlay"),
    )
    .expect("overlay compiles")
}

const ATTRIBUTES: &str = "\
attr/imports.ts review-imports
attr/generated.txt review-generated
attr/test.txt review-test
attr/docs.txt review-documentation
AGENTS.md review-agent-guidance
locales/** review-localization
images/** review-assets
tests/helper.rs review-implementation
api/types.ts linguist-generated
guide/** linguist-documentation
dist/keep.js -linguist-generated
ui/__snapshots__/a.snap -linguist-generated
src/legacy.test.ts -review-test
src/forced.ts -review-imports
";

fn fixture() -> Repo {
    let repo = repo();
    let root = repo.root.as_path();
    write(
        root,
        "web/only-imports.ts",
        "import a from 'a'\n\nexport const x = a\n",
    );
    write(
        root,
        "src/forced.ts",
        "import a from 'a'\nexport const y = a\n",
    );
    write(
        root,
        "web/reordered.ts",
        "import a from 'a'\nimport b from 'b'\nexport const z = [a, b]\n",
    );
    write(
        root,
        "svc/models.py",
        "import os\n\n\ndef f():\n    return os.sep\n",
    );
    write(root, "server/main.go", "package main\n");
    run(root, &["add", "."]);
    run(root, &["commit", "-q", "-m", "fixture base"]);
    run(root, &["checkout", "-q", "-b", "feature"]);

    write(root, ".gitattributes", ATTRIBUTES);
    write(root, "attr/imports.ts", "export const a = 1\n");
    write(root, "attr/generated.txt", "g\n");
    write(root, "attr/test.txt", "t\n");
    write(root, "attr/docs.txt", "d\n");
    write(root, "AGENTS.md", "# agents\n");
    write(root, "locales/de.json", "{}\n");
    write(root, "tests/helper.rs", "fn helper() {}\n");
    write(root, "api/types.ts", "export type T = 1\n");
    write(root, "guide/intro.md", "# intro\n");
    write(root, "dist/keep.js", "keep\n");
    write(root, "ui/__snapshots__/a.snap", "snap\n");
    write(root, "Cargo.lock", "# lock\n");
    write(root, "dist/bundle.min.js", "min\n");
    write(root, "src/legacy.test.ts", "legacy\n");
    write(root, "src/widget.test.ts", "widget\n");
    write(root, "server/handler_test.go", "package main\n");
    write(root, "server/main.go", "package main\n\nfunc main() {}\n");
    write(root, "README.md", "readme\n");
    write(root, ".ratchet/state.json", "{}\n");
    write(root, "dist/.lint-baseline.json", "{}\n");
    run(root, &["add", "."]);
    run(root, &["commit", "-q", "-m", "feature work"]);

    write(
        root,
        "web/only-imports.ts",
        "import a from 'a'\nimport c from 'c'\n\nexport const x = a\n",
    );
    write(
        root,
        "src/forced.ts",
        "import a from 'a'\nimport b from 'b'\nexport const y = a\n",
    );
    write(
        root,
        "web/reordered.ts",
        "import b from 'b'\nimport a from 'a'\nexport const z = [a, b]\n",
    );
    write(
        root,
        "svc/models.py",
        "import os\nfrom typing import (\n    Any,\n)\n\n\ndef f():\n    return os.sep\n",
    );
    write(root, "web/app.tsx", "export const App = 1\n");
    std::fs::create_dir_all(root.join("images")).expect("mkdir");
    std::fs::write(
        root.join("images/logo.png"),
        [0x89, b'P', b'N', b'G', 0, 1, 2],
    )
    .expect("png");
    repo
}

fn listing(repo: &Repo, classifier: &Classifier, query: ListQuery) -> Listing {
    list(
        &repo.root,
        classifier,
        &ListQuery {
            base: Some(BASE.into()),
            ..query
        },
    )
    .expect("list")
}

// `-` as rule id means the facet's default applied.
const GOLDEN: &[(&str, &str, Option<&str>, &str)] = &[
    (".gitattributes", "implementation", None, "-"),
    (
        ".ratchet/state.json",
        "lint-baseline",
        None,
        "lint-baseline",
    ),
    ("AGENTS.md", "agent-guidance", None, "review-agent-guidance"),
    ("Cargo.lock", "generated", None, "generated-heuristic"),
    ("README.md", "implementation", None, "-"),
    ("api/types.ts", "generated", None, "linguist-generated"),
    (
        "attr/docs.txt",
        "documentation",
        None,
        "review-documentation",
    ),
    ("attr/generated.txt", "generated", None, "review-generated"),
    ("attr/imports.ts", "imports", None, "imports"),
    ("attr/test.txt", "test", None, "review-test"),
    (
        "dist/.lint-baseline.json",
        "lint-baseline",
        None,
        "lint-baseline",
    ),
    (
        "dist/bundle.min.js",
        "generated",
        None,
        "generated-heuristic",
    ),
    ("dist/keep.js", "implementation", None, "generated-unset"),
    (
        "guide/intro.md",
        "documentation",
        None,
        "linguist-documentation",
    ),
    ("images/logo.png", "assets", None, "review-assets"),
    (
        "locales/de.json",
        "localization",
        None,
        "review-localization",
    ),
    (
        "server/handler_test.go",
        "test",
        Some("backend"),
        "test-heuristic",
    ),
    ("server/main.go", "implementation", Some("backend"), "-"),
    ("src/forced.ts", "imports", None, "imports"),
    ("src/legacy.test.ts", "implementation", None, "test-unset"),
    ("src/widget.test.ts", "test", None, "test-heuristic"),
    ("svc/models.py", "imports", None, "imports"),
    (
        "tests/helper.rs",
        "implementation",
        None,
        "review-implementation",
    ),
    (
        "ui/__snapshots__/a.snap",
        "implementation",
        None,
        "generated-unset",
    ),
    ("web/app.tsx", "implementation", Some("frontend"), "-"),
    (
        "web/only-imports.ts",
        "imports",
        Some("frontend"),
        "imports",
    ),
    ("web/reordered.ts", "implementation", Some("frontend"), "-"),
];

#[test]
fn golden_classification_covers_every_preset_rule_and_the_project_overlay() {
    let repo = fixture();
    let classifier = project();
    let listed = listing(&repo, &classifier, ListQuery::default());
    let paths: Vec<&str> = listed.rows.iter().map(|r| r.row.path.as_str()).collect();
    let golden_paths: Vec<&str> = GOLDEN.iter().map(|g| g.0).collect();
    assert_eq!(paths, golden_paths);
    let mut hit = BTreeSet::new();
    for (path, kind, area, rule) in GOLDEN {
        let explained = explain(&repo.root, &classifier, Some(BASE.into()), path).expect("explain");
        let row = listed
            .rows
            .iter()
            .find(|r| r.row.path == *path)
            .expect("row");
        assert_eq!(row.facets["type"].as_deref(), Some(*kind), "{path} type");
        assert_eq!(row.facets["area"].as_deref(), *area, "{path} area");
        let rule_id = explained.facets[0].rule_id.as_deref().unwrap_or("-");
        assert_eq!(rule_id, *rule, "{path} rule: {explained:?}");
        hit.insert(rule_id.to_string());
    }
    for rule in &FilterConfig::preset().facets[0].rules {
        let id = rule.id.clone().expect("preset rules have ids");
        assert!(hit.contains(&id), "no golden case for preset rule {id}");
    }
}

#[test]
fn without_the_overlay_lint_baselines_and_areas_fall_back_to_the_generic_preset() {
    let repo = fixture();
    let listed = listing(&repo, &generic(), ListQuery::default());
    let facet = |path: &str| {
        listed
            .rows
            .iter()
            .find(|r| r.row.path == path)
            .map(|r| r.facets.clone())
            .expect("row")
    };
    assert_eq!(
        facet(".ratchet/state.json")["type"].as_deref(),
        Some("implementation")
    );
    assert_eq!(
        facet("dist/.lint-baseline.json")["type"].as_deref(),
        Some("generated")
    );
    assert!(!facet("web/app.tsx").contains_key("area"));
    let types = &listed.facets[0];
    assert!(
        !types
            .values
            .iter()
            .any(|v| v.id == "lint-baseline" && v.present)
    );
    assert_eq!(listed.facets.len(), 1);
}

#[test]
fn explain_reports_attributes_heuristics_and_the_deciding_rule() {
    let repo = fixture();
    let explained =
        explain(&repo.root, &project(), Some(BASE.into()), "src/forced.ts").expect("explain");
    assert_eq!(
        explained
            .attributes
            .get("review-imports")
            .map(String::as_str),
        Some("unset")
    );
    assert!(
        explained.imports_only,
        "the analyzer overrides -review-imports"
    );
    assert_eq!(explained.facets[0].rule, Some(0));
    let unchanged =
        explain(&repo.root, &project(), Some(BASE.into()), "server/other.go").expect("explain");
    assert!(unchanged.row.is_none());
    assert_eq!(unchanged.facets[1].value.as_deref(), Some("backend"));
}

fn hide(pairs: &[(&str, &str)]) -> Hidden {
    let mut hidden = Hidden::new();
    for (facet, value) in pairs {
        hidden
            .entry(facet.to_string())
            .or_default()
            .insert(value.to_string());
    }
    hidden
}

#[test]
fn hidden_values_text_and_totals_narrow_the_base_scope() {
    let repo = fixture();
    let classifier = project();
    let all = listing(&repo, &classifier, ListQuery::default());
    let narrowed = listing(
        &repo,
        &classifier,
        ListQuery {
            hidden: hide(&[
                ("type", "test"),
                ("type", "generated"),
                ("area", "frontend"),
            ]),
            ..ListQuery::default()
        },
    );
    assert!(narrowed.rows.iter().all(|r| {
        !matches!(r.facets["type"].as_deref(), Some("test" | "generated"))
            && r.facets["area"].as_deref() != Some("frontend")
    }));
    assert!(
        narrowed.rows.iter().any(|r| r.row.path == "README.md"),
        "no area is never hidden by area"
    );
    assert_eq!(narrowed.unfiltered, all.totals);
    assert_eq!(narrowed.totals.files, narrowed.rows.len());
    assert_eq!(
        narrowed.totals.added,
        narrowed
            .rows
            .iter()
            .filter_map(|r| r.row.added)
            .sum::<u64>()
    );
    assert!(narrowed.facets.iter().all(|f| f.active));
    assert_eq!(
        narrowed.all_rows, None,
        "only asked-for listings carry every row"
    );

    let everything = listing(
        &repo,
        &classifier,
        ListQuery {
            hidden: hide(&[("type", "test")]),
            all_rows: true,
            ..ListQuery::default()
        },
    );
    assert_eq!(everything.all_rows.as_ref(), Some(&all.rows));
    assert!(
        everything
            .all_rows
            .iter()
            .flatten()
            .any(|r| r.facets["type"].as_deref() == Some("test")),
        "hidden rows come with their facet values"
    );

    let text = listing(
        &repo,
        &classifier,
        ListQuery {
            text: Some("  SERVER/ ".into()),
            ..ListQuery::default()
        },
    );
    assert_eq!(text.rows.len(), 2);
    let too_long = listing(
        &repo,
        &classifier,
        ListQuery {
            text: Some("x".repeat(MAX_TEXT_QUERY + 1)),
            ..ListQuery::default()
        },
    );
    assert!(too_long.rows.is_empty());

    let absent = listing(
        &repo,
        &classifier,
        ListQuery {
            hidden: hide(&[("type", "localization-nope")]),
            ..ListQuery::default()
        },
    );
    assert!(
        !absent.facets[0].active,
        "a hidden value that is not present is not active"
    );
}

#[test]
fn facets_apply_in_every_scope() {
    let repo = fixture();
    let root = repo.root.as_path();
    let classifier = project();
    let hidden = hide(&[("type", "test")]);
    let paths = |listed: &Listing| -> Vec<String> {
        listed.rows.iter().map(|r| r.row.path.clone()).collect()
    };

    write(root, "web/new.test.ts", "t\n");
    let uncommitted = listing(
        &repo,
        &classifier,
        ListQuery {
            scope: Scope::Uncommitted,
            hidden: hidden.clone(),
            ..ListQuery::default()
        },
    );
    assert!(paths(&uncommitted).contains(&"web/app.tsx".to_string()));
    assert!(!paths(&uncommitted).contains(&"web/new.test.ts".to_string()));
    assert!(uncommitted.rows.iter().all(|r| r.discardable));

    let feature = crate::git(root, &["rev-parse", "HEAD"]).expect("head");
    let commit = listing(
        &repo,
        &classifier,
        ListQuery {
            scope: Scope::Commit(feature.trim()[..10].to_string()),
            hidden: hidden.clone(),
            text: Some("src/".into()),
            ..ListQuery::default()
        },
    );
    assert_eq!(paths(&commit), ["src/legacy.test.ts"]);
    assert_eq!(
        commit.commit.as_ref().map(|c| c.subject.as_str()),
        Some("feature work")
    );
    assert!(
        commit
            .rows
            .iter()
            .all(|r| !r.discardable && r.row.committed)
    );

    let start = turns::start_turn(root).expect("start");
    write(root, "server/turn.go", "package main\n");
    write(root, "server/turn_test.go", "package main\n");
    let meta = turns::TurnMeta {
        prompt: "add a turn".into(),
        agent_id: "agent-1".into(),
        provider: None,
        started_at: 1,
        completed_at: 2,
    };
    turns::finish_turn(root, &start, &meta)
        .expect("finish")
        .expect("not empty");
    let turn = listing(
        &repo,
        &classifier,
        ListQuery {
            scope: Scope::Turn("latest".into()),
            hidden,
            ..ListQuery::default()
        },
    );
    assert_eq!(paths(&turn), ["server/turn.go"]);
    assert_eq!(turn.unfiltered.files, 2);
    assert_eq!(turn.rows[0].facets["area"].as_deref(), Some("backend"));
    assert_eq!(
        turn.turn.as_ref().map(|t| t.meta.prompt.as_str()),
        Some("add a turn")
    );
}

#[test]
fn scopes_parse_and_print() {
    for text in ["base", "uncommitted", "turn:latest", "commit:abc1234"] {
        assert_eq!(Scope::parse(text).expect("parse").to_string(), text);
    }
    assert_eq!(Scope::parse("").expect("empty"), Scope::Base);
    assert_eq!(
        Scope::parse("turn").expect("turn"),
        Scope::Turn("latest".into())
    );
    assert!(Scope::parse("range:a..b").is_err());
}

#[test]
fn overlays_validate_rules_values_and_anchors() {
    let bad_value = serde_json::json!({ "facets": [ { "id": "type", "rules": [
        { "value": "nope", "when": { "path": "x" } } ] } ] });
    let config = FilterConfig::preset()
        .with_overlay(&bad_value)
        .expect("merges");
    assert!(Classifier::new(config).is_err());
    let bad_anchor = serde_json::json!({ "facets": [ { "id": "type", "rules": [
        { "value": "test", "before": "missing", "when": { "path": "x" } } ] } ] });
    assert!(FilterConfig::preset().with_overlay(&bad_anchor).is_err());
    let bad_regex = serde_json::json!({ "facets": [ { "id": "type", "rules": [
        { "value": "test", "when": { "regex": "(" } } ] } ] });
    let config = FilterConfig::preset()
        .with_overlay(&bad_regex)
        .expect("merges");
    assert!(Classifier::new(config).is_err());
    let replaced = serde_json::json!({ "facets": [ { "id": "type", "replace": true, "default": "x",
        "values": [ { "id": "x" } ] } ],
        "analyzers": { "imports_only": { "semantics": "v2", "max_files": 3 } } });
    let config = FilterConfig::preset()
        .with_overlay(&replaced)
        .expect("replace");
    assert!(config.facets[0].rules.is_empty());
    assert_eq!(config.analyzers.imports_only.semantics, Semantics::V2);
    assert_eq!(config.analyzers.imports_only.max_files, 3);
    assert_eq!(config.analyzers.imports_only.max_changed_lines, 60);
}

#[test]
fn change_predicates_test_kind_lines_binary_and_sources() {
    let row = ChangeRow {
        path: "a.rs".into(),
        status: ChangeStatus::Added,
        old_path: None,
        added: Some(10),
        removed: Some(0),
        committed: false,
        staged: false,
        unstaged: false,
        untracked: true,
        conflicted: false,
        binary: false,
    };
    let config = serde_json::json!({ "facets": [ { "id": "size", "values": [ { "id": "small" }, { "id": "new" } ],
        "rules": [
            { "value": "small", "when": { "change": { "lines": { "max": 5 } } } },
            { "value": "new", "when": { "all": [
                { "change": { "kind": ["added"], "sources": ["untracked"], "binary": false } },
                { "not": { "regex": "^b" } } ] } }
        ] } ] });
    let classifier = Classifier::new(
        FilterConfig::default()
            .with_overlay(&config)
            .expect("config"),
    )
    .expect("compile");
    let facts = Facts {
        path: "a.rs",
        row: Some(&row),
        attrs: None,
        imports_only: false,
    };
    assert_eq!(classifier.classify(&facts)["size"].as_deref(), Some("new"));
    let without_row = Facts { row: None, ..facts };
    assert_eq!(classifier.classify(&without_row)["size"], None);
}

#[test]
fn the_imports_only_analyzer_respects_its_limits() {
    let repo = fixture();
    let base = crate::changes(
        &repo.root,
        &ChangesQuery {
            base: Some(BASE.into()),
            uncommitted_only: false,
        },
    )
    .expect("changes");
    let CompareStatus::Ready { merge_base, .. } = &base.compare else {
        panic!("compare not ready");
    };
    let run =
        |config: ImportsOnlyConfig| imports_only_paths(&repo.root, merge_base, &base.rows, &config);
    let all = run(ImportsOnlyConfig::default());
    assert_eq!(
        all.iter().map(String::as_str).collect::<Vec<_>>(),
        ["src/forced.ts", "svc/models.py", "web/only-imports.ts"]
    );
    let first = run(ImportsOnlyConfig {
        max_files: 1,
        ..ImportsOnlyConfig::default()
    });
    assert!(
        first.is_empty(),
        "the first candidate by path is api/types.ts, an added file"
    );
    let small = run(ImportsOnlyConfig {
        max_changed_lines: 1,
        ..ImportsOnlyConfig::default()
    });
    assert_eq!(small.len(), 2, "svc/models.py changes 3 lines");
    let tiny = run(ImportsOnlyConfig {
        max_bytes: 10,
        ..ImportsOnlyConfig::default()
    });
    assert!(tiny.is_empty());
    let v2 = run(ImportsOnlyConfig {
        semantics: Semantics::V2,
        ..ImportsOnlyConfig::default()
    });
    assert!(v2.contains("web/reordered.ts"));
}

fn v1(path: &str, before: &str, after: &str) -> bool {
    is_import_only_change(path, before, after, Semantics::OrcaV1)
}

fn v2(path: &str, before: &str, after: &str) -> bool {
    is_import_only_change(path, before, after, Semantics::V2)
}

#[test]
fn imports_only_for_typescript_and_javascript() {
    let base = "import a from 'a'\nimport { b } from \"b\";\n\nconst x = a + b\n";
    for path in [
        "x.ts", "x.tsx", "x.mts", "x.cts", "x.js", "x.jsx", "x.mjs", "x.cjs",
    ] {
        assert!(
            v1(path, base, &format!("import c from 'c'\n{base}")),
            "{path}"
        );
    }
    assert!(!v1("x.rs", base, &format!("import c from 'c'\n{base}")));
    assert!(v1(
        "x.ts",
        base,
        "import a from 'a'\nimport * as all from 'all'\nimport 'side-effect'\nimport type { T } from 't'\nimport { b } from \"b\";\nconst x = a + b\n"
    ));
    assert!(v1(
        "x.ts",
        base,
        "import a from 'a'\nimport {\n  b,\n  c,\n} from 'b'\nconst x = a + b\n"
    ));
    assert!(v1(
        "x.ts",
        base,
        "import a from 'a'\nimport { b } from \"b\";\nexport * from './all'\nexport { d } from './d'\nconst x = a + b\n"
    ));
    assert!(
        !v1(
            "x.ts",
            base,
            "import a from 'a'\nimport { b } from \"b\";\nexport { a }\nconst x = a + b\n"
        ),
        "a local export is code"
    );
    assert!(
        !v1(
            "x.ts",
            base,
            "import a from 'a'\nimport { b } from \"b\";\nconst m = import('m')\nconst x = a + b\n"
        ),
        "dynamic import is code"
    );
    assert!(!v1(
        "x.ts",
        base,
        "import a from 'a'\nimport { b } from \"b\";\nconst x = a - b\n"
    ));
    assert!(
        !v1(
            "x.ts",
            base,
            "import a from 'a'\nimport { b } from \"b\";\n// note\nconst x = a + b\n"
        ),
        "comments are code"
    );
    assert!(
        !v1("x.ts", base, &base.replace("\n\n", "\n\n\n\n")),
        "blank lines only"
    );
    assert!(
        !v2("x.ts", base, &base.replace("\n\n", "\n\n\n\n")),
        "blank lines only"
    );
    assert!(v1(
        "x.ts",
        base,
        &base
            .replace('\n', "\r\n")
            .replace("const", "import z from 'z'\r\nconst")
    ));
}

#[test]
fn reordered_or_reformatted_imports_follow_the_chosen_semantics() {
    let base = "import a from 'a'\nimport { b, c } from 'b'\nconst x = a\n";
    let reordered = "import { b, c } from 'b'\nimport a from 'a'\nconst x = a\n";
    let reformatted = "import a from 'a'\nimport {\n  b,\n  c\n} from 'b'\nconst x = a\n";
    assert!(
        !v1("x.ts", base, reordered),
        "orca-v1: a pure reorder stays implementation"
    );
    assert!(
        !v1("x.ts", base, reformatted),
        "orca-v1: a pure reformat stays implementation"
    );
    assert!(v2("x.ts", base, reordered));
    assert!(v2("x.ts", base, reformatted));
    let py = "import os\nimport sys\n\nprint(os, sys)\n";
    let py_reordered = "import sys\nimport os\n\nprint(os, sys)\n";
    assert!(!v1("x.py", py, py_reordered));
    assert!(v2("x.py", py, py_reordered));
    assert!(
        !v2("x.py", py, "import sys\nimport os\n\nprint(sys, os)\n"),
        "code changed"
    );
}

#[test]
fn imports_only_for_python() {
    let base = "import os\n\ndef f():\n    return os.sep\n";
    assert!(v1("m.py", base, &format!("import sys\n{base}")));
    assert!(v1(
        "m.pyi",
        base,
        &format!("from typing import Any\n{base}")
    ));
    assert!(v1(
        "m.py",
        base,
        &format!("from typing import (\n    Any,\n    Dict,\n)\n{base}")
    ));
    assert!(v1(
        "m.py",
        base,
        &format!("from a import b, \\\n    c\n{base}")
    ));
    assert!(v1(
        "m.py",
        "import os\nif TYPE_CHECKING:\n    import a\n",
        "import os\nif TYPE_CHECKING:\n    import a\n    import b\n"
    ));
    assert!(!v1(
        "m.py",
        base,
        "import os\n\ndef f():\n    return os.name\n"
    ));
    assert!(!v1("m.py", base, &format!("{base}# trailing\n")));
}

#[test]
fn generated_and_test_heuristics_match_the_spec_verbatim() {
    for path in [
        "a/__generated__/x.ts",
        "pkg/node_modules/x/y.js",
        "Vendor/x.go",
        "bun.lockb",
        "Gemfile.lock",
        "x.Designer.cs",
        "svc.pb.gw.go",
        "a_pb2_grpc.py",
        "schema.gen.ts",
        "x_generated.go",
        "model.freezed.dart",
        "app.min.mjs",
        "app.css.map",
        "Chip.snap",
    ] {
        assert!(is_generated_path(path), "{path}");
    }
    for path in [
        "build/x.js",
        "target/x.rs",
        "out/x",
        "bin/x",
        "src/generator.ts",
    ] {
        assert!(!is_generated_path(path), "{path}");
    }
    for path in [
        "a/__mocks__/x.ts",
        "cypress/x.ts",
        "Tests/x.cs",
        "spec/x.rb",
        "x.spec.js",
        "handler_test.go",
        "user-specs.rb",
        "test_client.py",
        "pkg/conftest.py",
        "FooTest.java",
        "BarSpec.kt",
    ] {
        assert!(is_test_path(path), "{path}");
    }
    for path in [
        "specs/x.md",
        "test_page.ts",
        "Contest.java",
        "footest.java",
        "latest.kt",
    ] {
        assert!(!is_test_path(path), "{path}");
    }
}
