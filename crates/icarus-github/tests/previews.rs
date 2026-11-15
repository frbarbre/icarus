use icarus_github::previews::{PrPreviews, PreviewConfig, PreviewSource};
use icarus_github::{FakeGh, FakeResponse, GitHub};
use icarus_rules::facts::RepoRef;
use serde_json::{Value, json};

const HEAD: &str = "a8f65e7f2f33cee037a892fb93da01a56aeeed7a";
const OLD: &str = "1ce2fc9000000000000000000000000000000000";
const REF_ROUTE: &str = "GET repos/acme/widgets/deployments?ref=feat/preview&per_page=30";

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

fn pull(commit: Value, comments: Value) -> FakeResponse {
    FakeResponse::ok(
        json!({ "data": { "repository": { "pullRequest": {
            "headRefOid": HEAD,
            "headRefName": "feat/preview",
            "isCrossRepository": false,
            "comments": { "nodes": comments },
            "commits": { "nodes": [{ "commit": commit }] },
        } } } })
        .to_string(),
    )
}

fn deployment(environment: &str, sha: &str, state: &str, url: Option<&str>, at: &str) -> Value {
    json!({
        "environment": environment,
        "state": "ACTIVE",
        "createdAt": at,
        "commitOid": sha,
        "latestStatus": { "state": state, "environmentUrl": url, "logUrl": null, "createdAt": at },
    })
}

fn bot(body: &str, at: &str) -> Value {
    json!({ "body": body, "createdAt": at, "author": { "__typename": "Bot", "login": "github-actions" } })
}

async fn previews(fake: FakeGh, config: &PreviewConfig) -> (PrPreviews, GitHub<FakeGh>) {
    let github = GitHub::new(fake);
    let found = github
        .pr_previews(&repo(), 7, config)
        .await
        .expect("previews");
    (found, github)
}

fn app<'a>(found: &'a PrPreviews, name: &str) -> &'a icarus_github::previews::Preview {
    found
        .previews
        .iter()
        .find(|p| p.app == name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", found.previews))
}

#[tokio::test]
async fn deployments_group_by_environment_and_prefer_the_head() {
    let commit = json!({
        "deployments": { "nodes": [
            deployment("frontend-preview", HEAD, "SUCCESS", Some("https://feat.widgets.pages.dev"), "2026-11-15T13:17:00Z"),
            deployment("frontend-preview", HEAD, "INACTIVE", Some("https://old.widgets.pages.dev"), "2026-11-15T13:10:00Z"),
            deployment("insights-preview", HEAD, "IN_PROGRESS", None, "2026-11-15T13:18:00Z"),
            deployment("production", HEAD, "SUCCESS", Some("https://widgets.example"), "2026-11-15T13:19:00Z"),
            deployment("github-pages", HEAD, "SUCCESS", Some("https://acme.github.io"), "2026-11-15T13:19:00Z"),
        ] },
        "status": null,
        "checkSuites": { "nodes": [] },
    });
    let fake = FakeGh::new()
        .on("graphql PrPreviews", pull(commit, json!([])))
        .on(
            REF_ROUTE,
            FakeResponse::ok(
                json!([
                    { "id": 9, "sha": OLD, "environment": "docs-preview", "created_at": "2026-11-14T10:00:00Z" },
                    { "id": 8, "sha": OLD, "environment": "docs-preview", "created_at": "2026-11-13T10:00:00Z" },
                    { "id": 5, "sha": OLD, "environment": "frontend-preview", "created_at": "2026-11-14T09:00:00Z" },
                    { "id": 4, "sha": OLD, "environment": "Production", "created_at": "2026-11-14T09:00:00Z" },
                ])
                .to_string(),
            ),
        )
        .on(
            "GET repos/acme/widgets/deployments/9/statuses?per_page=1",
            FakeResponse::ok(
                json!([{ "state": "inactive", "environment_url": "https://docs-old.pages.dev",
                    "log_url": "https://ci/9", "created_at": "2026-11-14T10:01:00Z" }])
                .to_string(),
            ),
        );
    let (found, github) = previews(fake, &PreviewConfig::default()).await;

    let apps: Vec<&str> = found.previews.iter().map(|p| p.app.as_str()).collect();
    assert_eq!(apps, ["docs", "frontend", "insights"]);
    let frontend = app(&found, "frontend");
    assert_eq!(
        frontend.url.as_deref(),
        Some("https://feat.widgets.pages.dev")
    );
    assert_eq!(frontend.state, "success");
    assert!(frontend.is_head);
    assert_eq!(frontend.environment.as_deref(), Some("frontend-preview"));
    assert_eq!(frontend.source, PreviewSource::Deployment);
    let insights = app(&found, "insights");
    assert_eq!(insights.state, "in_progress");
    assert_eq!(insights.url, None);
    let docs = app(&found, "docs");
    assert!(!docs.is_head);
    assert_eq!(docs.sha.as_deref(), Some(OLD));
    assert_eq!(docs.state, "inactive");
    assert_eq!(docs.log_url.as_deref(), Some("https://ci/9"));
    assert_eq!(
        found.primary.as_ref().map(|p| p.app.as_str()),
        Some("frontend")
    );
    assert_eq!(
        github
            .runner()
            .count("GET repos/acme/widgets/deployments/5/statuses?per_page=1"),
        0
    );
    assert_eq!(
        github
            .runner()
            .count("GET repos/acme/widgets/deployments/8/statuses?per_page=1"),
        0
    );
}

#[tokio::test]
async fn a_vercel_status_is_a_preview_when_there_are_no_deployments() {
    let commit = json!({
        "deployments": { "nodes": [] },
        "status": { "contexts": [
            { "context": "Vercel – marketing", "state": "SUCCESS",
              "targetUrl": "https://marketing-git-feat-acme.vercel.app", "createdAt": "2026-11-15T12:00:00Z" },
            { "context": "ci/build", "state": "SUCCESS", "targetUrl": "https://ci.example/1", "createdAt": "2026-11-15T12:00:00Z" },
        ] },
        "checkSuites": { "nodes": [{ "app": { "name": "Netlify", "slug": "netlify" }, "checkRuns": { "nodes": [
            { "name": "Deploy Preview", "status": "COMPLETED", "conclusion": "SUCCESS",
              "detailsUrl": "https://app.netlify.com/sites/docs/deploys/1", "completedAt": "2026-11-15T12:01:00Z" },
        ] } }] },
    });
    let fake = FakeGh::new()
        .on("graphql PrPreviews", pull(commit, json!([])))
        .on(REF_ROUTE, FakeResponse::ok("[]"));
    let (found, _) = previews(fake, &PreviewConfig::default()).await;

    assert_eq!(found.previews.len(), 2, "{:?}", found.previews);
    let marketing = app(&found, "marketing");
    assert_eq!(marketing.source, PreviewSource::Status);
    assert_eq!(
        marketing.url.as_deref(),
        Some("https://marketing-git-feat-acme.vercel.app")
    );
    assert_eq!(marketing.sha.as_deref(), Some(HEAD));
    assert!(marketing.is_head);
    let netlify = app(&found, "netlify");
    assert_eq!(netlify.url, None);
    assert_eq!(
        netlify.log_url.as_deref(),
        Some("https://app.netlify.com/sites/docs/deploys/1")
    );
    assert_eq!(
        found.primary.as_ref().map(|p| p.app.as_str()),
        Some("marketing")
    );
}

#[tokio::test]
async fn bot_comments_fill_in_and_newest_head_comment_wins() {
    let commit = json!({ "deployments": { "nodes": [
        deployment("frontend-preview", HEAD, "SUCCESS", Some("https://feat.widgets.pages.dev"), "2026-11-15T13:17:00Z"),
    ] }, "status": null, "checkSuites": { "nodes": [] } });
    let comments = json!([
        bot(
            "🔎 **Insights preview deployment** for `1ce2fc9`: https://old.insights.pages.dev",
            "2026-11-14T10:00:00Z"
        ),
        bot(
            "🔎 **Frontend preview deployment** for `a8f65e7`: https://feat.widgets.pages.dev",
            "2026-11-15T13:18:00Z"
        ),
        bot(
            "🔎 **Insights preview deployment** for `a8f65e7`: https://feat.insights.pages.dev",
            "2026-11-15T13:18:00Z"
        ),
        bot(
            "| **storybook** | Ready ([Inspect](https://vercel.com/acme/sb/x)) | [Visit Preview](https://sb-git-feat.vercel.app) |",
            "2026-11-15T13:00:00Z"
        ),
        json!({ "body": "my preview: https://mine.example.dev", "createdAt": "2026-11-15T13:20:00Z",
            "author": { "__typename": "User", "login": "dev" } }),
        bot(
            "Linear: https://linear.app/acme/issue/E-1",
            "2026-11-15T13:20:00Z"
        ),
    ]);
    let fake = FakeGh::new()
        .on("graphql PrPreviews", pull(commit, comments))
        .on(REF_ROUTE, FakeResponse::ok("[]"));
    let (found, _) = previews(fake, &PreviewConfig::default()).await;

    let apps: Vec<&str> = found.previews.iter().map(|p| p.app.as_str()).collect();
    assert_eq!(apps, ["frontend", "insights", "storybook"]);
    assert_eq!(app(&found, "frontend").source, PreviewSource::Deployment);
    let insights = app(&found, "insights");
    assert_eq!(insights.source, PreviewSource::Comment);
    assert_eq!(
        insights.url.as_deref(),
        Some("https://feat.insights.pages.dev")
    );
    assert_eq!(insights.sha.as_deref(), Some("a8f65e7"));
    assert!(insights.is_head);
    let storybook = app(&found, "storybook");
    assert_eq!(
        storybook.url.as_deref(),
        Some("https://sb-git-feat.vercel.app")
    );
    assert!(!storybook.is_head);
}

#[tokio::test]
async fn a_custom_comment_pattern_reads_any_author() {
    let commit =
        json!({ "deployments": { "nodes": [] }, "status": null, "checkSuites": { "nodes": [] } });
    let comments = json!([
        json!({ "body": "Deployed api@a8f65e7 to <https://api-7.review.acme.dev>", "createdAt": "2026-11-15T13:20:00Z",
            "author": { "__typename": "User", "login": "deploybot" } }),
        bot(
            "🔎 **Frontend preview deployment**: https://ignored.pages.dev",
            "2026-11-15T13:18:00Z"
        ),
    ]);
    let fake = FakeGh::new()
        .on("graphql PrPreviews", pull(commit, comments))
        .on(REF_ROUTE, FakeResponse::ok("[]"));
    let config = PreviewConfig::from_settings(&json!({
        "comment_pattern": r"Deployed (?P<app>\w+)@(?P<sha>[0-9a-f]+) to <(?P<url>[^>]+)>",
    }))
    .expect("config");
    let (found, _) = previews(fake, &config).await;

    assert_eq!(found.previews.len(), 1, "{:?}", found.previews);
    let api = app(&found, "api");
    assert_eq!(api.url.as_deref(), Some("https://api-7.review.acme.dev"));
    assert!(api.is_head);
    assert_eq!(found.primary.as_ref().map(|p| p.app.as_str()), Some("api"));
}

#[tokio::test]
async fn settings_choose_environments_and_the_primary_app() {
    let commit = json!({ "deployments": { "nodes": [
        deployment("web-preview", HEAD, "SUCCESS", Some("https://web.example"), "2026-11-15T13:17:00Z"),
        deployment("api-preview", HEAD, "SUCCESS", Some("https://api.example"), "2026-11-15T13:17:00Z"),
        deployment("qa", HEAD, "SUCCESS", Some("https://qa.example"), "2026-11-15T13:17:00Z"),
    ] }, "status": null, "checkSuites": { "nodes": [] } });
    let fake = || {
        FakeGh::new()
            .on("graphql PrPreviews", pull(commit.clone(), json!([])))
            .on(REF_ROUTE, FakeResponse::ok("[]"))
    };

    let (found, _) = previews(fake(), &PreviewConfig::default()).await;
    assert_eq!(found.previews.len(), 2);
    assert_eq!(found.primary.as_ref().map(|p| p.app.as_str()), Some("web"));

    let config = PreviewConfig::from_settings(&json!({
        "environment_pattern": "(?i)preview|^qa$",
        "exclude_pattern": "^web-",
        "primary_app": "API",
    }))
    .expect("config");
    let (found, _) = previews(fake(), &config).await;
    let apps: Vec<&str> = found.previews.iter().map(|p| p.app.as_str()).collect();
    assert_eq!(apps, ["api", "qa"]);
    assert_eq!(found.primary.as_ref().map(|p| p.app.as_str()), Some("api"));
}

#[test]
fn invalid_patterns_are_rejected() {
    assert!(PreviewConfig::from_settings(&json!({ "environment_pattern": "(" })).is_err());
    assert!(PreviewConfig::from_settings(&json!({ "comment_pattern": "https://\\S+" })).is_err());
}

#[tokio::test]
async fn a_failed_ref_lookup_still_returns_head_previews() {
    let commit = json!({ "deployments": { "nodes": [
        deployment("frontend-preview", HEAD, "SUCCESS", Some("https://feat.widgets.pages.dev"), "2026-11-15T13:17:00Z"),
    ] }, "status": null, "checkSuites": { "nodes": [] } });
    let fake = FakeGh::new()
        .on("graphql PrPreviews", pull(commit, json!([])))
        .on(REF_ROUTE, FakeResponse::status(404));
    let (found, _) = previews(fake, &PreviewConfig::default()).await;
    assert_eq!(found.previews.len(), 1);
}
