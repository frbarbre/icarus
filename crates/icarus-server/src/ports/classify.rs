use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Frontend,
    Api,
    Docs,
    Storybook,
    Database,
    Cache,
    Other,
}

impl Kind {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "frontend" => Some(Self::Frontend),
            "api" => Some(Self::Api),
            "docs" => Some(Self::Docs),
            "storybook" => Some(Self::Storybook),
            "database" => Some(Self::Database),
            "cache" => Some(Self::Cache),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Frontend => 0,
            Self::Api => 1,
            Self::Docs => 2,
            Self::Storybook => 3,
            Self::Other => 4,
            Self::Database => 5,
            Self::Cache => 6,
        }
    }

    pub fn is_http(self) -> bool {
        !matches!(self, Self::Database | Self::Cache)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Classification {
    pub kind: Kind,
    pub framework: Option<String>,
    pub confidence: Confidence,
}

impl Classification {
    fn new(kind: Kind, framework: &str, confidence: Confidence) -> Self {
        Self {
            kind,
            framework: (!framework.is_empty()).then(|| framework.to_string()),
            confidence,
        }
    }

    pub fn unknown() -> Self {
        Self::new(Kind::Other, "", Confidence::Low)
    }
}

struct Pattern {
    regex: &'static str,
    kind: Kind,
    framework: &'static str,
    confidence: Confidence,
}

const fn pattern(
    regex: &'static str,
    kind: Kind,
    framework: &'static str,
    confidence: Confidence,
) -> Pattern {
    Pattern {
        regex,
        kind,
        framework,
        confidence,
    }
}

// First match wins, so wrappers (storybook runs webpack or vite) come before what they wrap.
const COMMAND_PATTERNS: &[Pattern] = {
    use Confidence::{High, Low, Medium};
    use Kind::*;
    &[
        pattern(
            r"\bstorybook\b|start-storybook",
            Storybook,
            "storybook",
            High,
        ),
        pattern(r"\bspotlight\b", Other, "spotlight", High),
        pattern(r"\bvitepress\b", Docs, "vitepress", High),
        pattern(r"\bdocusaurus\b", Docs, "docusaurus", High),
        pattern(r"\bmkdocs\b", Docs, "mkdocs", High),
        pattern(r"\bmintlify\b|\bmint dev\b", Docs, "mintlify", High),
        pattern(r"\bvitest\b", Other, "vitest", Medium),
        pattern(r"\bremix(-serve)?\b|@remix-run", Frontend, "remix", High),
        pattern(r"svelte-kit|sveltekit", Frontend, "sveltekit", High),
        pattern(r"\bnux(t|i)\b", Frontend, "nuxt", High),
        pattern(r"\bastro\b", Frontend, "astro", High),
        pattern(r"\bvite(\.js)?\b", Frontend, "vite", High),
        pattern(
            r"\bnext(\.js)? (dev|start)\b|next-server|next/dist",
            Frontend,
            "next",
            High,
        ),
        pattern(r"\bng serve\b|@angular/cli", Frontend, "angular", High),
        pattern(r"react-scripts", Frontend, "create-react-app", High),
        pattern(
            r"webpack[- ](dev-server|serve)\b|webpack-dev-server",
            Frontend,
            "webpack",
            High,
        ),
        pattern(r"\bgatsby\b", Frontend, "gatsby", High),
        pattern(r"\bparcel\b", Frontend, "parcel", High),
        pattern(r"\brsbuild\b|\brspack\b", Frontend, "rspack", High),
        pattern(r"\bexpo\b", Frontend, "expo", Medium),
        pattern(r"\bmetro\b|react-native start", Frontend, "metro", Medium),
        pattern(r"\bember serve\b", Frontend, "ember", High),
        pattern(r"manage\.py\s+runserver|django", Api, "django", Medium),
        pattern(r"\bfastapi\b", Api, "fastapi", High),
        pattern(r"\buvicorn\b", Api, "uvicorn", Medium),
        pattern(r"\bgunicorn\b", Api, "gunicorn", Medium),
        pattern(r"\bhypercorn\b", Api, "hypercorn", Medium),
        pattern(r"\bflask\b", Api, "flask", Medium),
        pattern(
            r"\brails (s|server)\b|\bpuma\b|\bunicorn\b",
            Api,
            "rails",
            Medium,
        ),
        pattern(r"\bphp artisan serve\b", Api, "laravel", Medium),
        pattern(r"\bpostgres\b|\bpostmaster\b", Database, "postgres", High),
        pattern(r"\bmysqld\b|\bmariadbd\b", Database, "mysql", High),
        pattern(r"\bmongod\b", Database, "mongodb", High),
        pattern(r"\bclickhouse\b", Database, "clickhouse", High),
        pattern(
            r"\belasticsearch\b|\bopensearch\b",
            Database,
            "elasticsearch",
            High,
        ),
        pattern(r"\bredis-server\b|\bvalkey-server\b", Cache, "redis", High),
        pattern(r"\bmemcached\b", Cache, "memcached", High),
        pattern(
            r"docker-proxy|com\.docker|vpnkit|\borbstack\b",
            Other,
            "docker",
            Medium,
        ),
        pattern(
            r"\bhttp\.server\b|\bSimpleHTTPServer\b",
            Other,
            "python-http-server",
            Low,
        ),
        pattern(r"\bgo run\b|/go-build\d*/", Api, "go", Low),
        pattern(r"\bcargo run\b|/target/(debug|release)/", Api, "rust", Low),
        pattern(
            r"\bnodemon\b|\btsx\b.*server|\bts-node\b.*server",
            Api,
            "node",
            Low,
        ),
        pattern(
            r"\b(node|bun|deno)\b.*\b(server|app|index|main)\.(c|m)?(j|t)s\b",
            Api,
            "node",
            Low,
        ),
    ]
};

static COMPILED: LazyLock<Vec<(Regex, &'static Pattern)>> = LazyLock::new(|| {
    COMMAND_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(&format!("(?i){}", p.regex)).ok().map(|r| (r, p)))
        .collect()
});

const PORT_HINTS: &[(u16, Kind, &str)] = &[
    (5432, Kind::Database, "postgres"),
    (3306, Kind::Database, "mysql"),
    (27017, Kind::Database, "mongodb"),
    (6379, Kind::Cache, "redis"),
    (11211, Kind::Cache, "memcached"),
    (6006, Kind::Storybook, "storybook"),
    (5173, Kind::Frontend, "vite"),
    (5174, Kind::Frontend, "vite"),
];

pub fn classify_command(command: &str, process: &str, port: u16) -> Classification {
    let haystack = format!("{process} {command}");
    let matched = COMPILED
        .iter()
        .find(|(regex, _)| regex.is_match(&haystack))
        .map(|(_, p)| Classification::new(p.kind, p.framework, p.confidence));
    match matched {
        Some(found) if found.framework.as_deref() == Some("docker") => {
            port_hint(port).unwrap_or(found)
        }
        Some(found) => found,
        None => port_hint(port).unwrap_or_else(Classification::unknown),
    }
}

fn port_hint(port: u16) -> Option<Classification> {
    PORT_HINTS
        .iter()
        .find(|(p, _, _)| *p == port)
        .map(|(_, kind, framework)| Classification::new(*kind, framework, Confidence::Low))
}

#[derive(Debug, Clone, Default)]
pub struct ProbeResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

pub fn classify_probe(response: &ProbeResponse) -> Option<Classification> {
    use Confidence::{High, Low, Medium};
    if response.status == 0 {
        return None;
    }
    let body = response.body.to_ascii_lowercase();
    let content_type = response.content_type.to_ascii_lowercase();
    let html = content_type.contains("html")
        || body.trim_start().starts_with("<!doctype html")
        || body.contains("<html");
    let found = if body.contains("/@vite/client") {
        Classification::new(Kind::Frontend, "vite", High)
    } else if html && (body.contains("storybook") || body.contains("sb-show-main")) {
        Classification::new(Kind::Storybook, "storybook", High)
    } else if body.contains("id=\"__next\"") || body.contains("/_next/") {
        Classification::new(Kind::Frontend, "next", High)
    } else if body.contains("id=\"__nuxt\"") || body.contains("/_nuxt/") {
        Classification::new(Kind::Frontend, "nuxt", High)
    } else if html && (body.contains("swagger-ui") || body.contains("redoc")) {
        Classification::new(Kind::Docs, "openapi", High)
    } else if html && body.contains("__sveltekit") {
        Classification::new(Kind::Frontend, "sveltekit", High)
    } else if html && body.contains("/static/js/bundle.js") {
        Classification::new(Kind::Frontend, "create-react-app", Medium)
    } else if content_type.contains("json") {
        Classification::new(Kind::Api, "", Medium)
    } else if html
        && ["id=\"root\"", "id=\"app\"", "id='root'", "id='app'"]
            .iter()
            .any(|marker| body.contains(marker))
    {
        Classification::new(Kind::Frontend, "", Medium)
    } else if html {
        Classification::new(Kind::Frontend, "", Low)
    } else {
        return None;
    };
    Some(found)
}

pub fn classify_openapi(response: &ProbeResponse) -> Option<Classification> {
    let body = response.body.trim_start();
    let spec = (200..300).contains(&response.status)
        && body.starts_with('{')
        && (body.contains("\"openapi\"") || body.contains("\"swagger\""));
    spec.then(|| Classification::new(Kind::Api, "openapi", Confidence::High))
}

// On a confidence tie the command line wins: a page's generic HTML says less than the binary.
pub fn merge(command: Classification, probe: Option<Classification>) -> Classification {
    match probe {
        Some(probe) if probe.confidence > command.confidence => {
            let framework = probe.framework.or(command
                .framework
                .filter(|_| command.kind == probe.kind && command.confidence > Confidence::Low));
            Classification { framework, ..probe }
        }
        _ => command,
    }
}

pub fn kind_from_label(label: &str) -> Option<Kind> {
    let label = label.to_ascii_lowercase();
    let has = |words: &[&str]| {
        label
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| words.contains(&word))
    };
    if has(&["storybook"]) {
        Some(Kind::Storybook)
    } else if has(&["docs", "doc", "swagger", "openapi", "redoc"]) {
        Some(Kind::Docs)
    } else if has(&[
        "frontend",
        "web",
        "app",
        "ui",
        "site",
        "client",
        "dashboard",
    ]) {
        Some(Kind::Frontend)
    } else if has(&["api", "backend", "server", "graphql", "gateway"]) {
        Some(Kind::Api)
    } else if has(&[
        "db",
        "database",
        "postgres",
        "postgresql",
        "mysql",
        "mongo",
        "mongodb",
    ]) {
        Some(Kind::Database)
    } else if has(&["redis", "cache", "valkey", "memcached"]) {
        Some(Kind::Cache)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_classify_by_table() {
        let cases: &[(&str, &str, u16, Kind, Option<&str>)] = &[
            (
                "node /repo/node_modules/.bin/vite",
                "node",
                5173,
                Kind::Frontend,
                Some("vite"),
            ),
            (
                "node /repo/node_modules/vite/bin/vite.js --port 4000",
                "node",
                4000,
                Kind::Frontend,
                Some("vite"),
            ),
            (
                "node /repo/node_modules/.bin/next dev",
                "node",
                3000,
                Kind::Frontend,
                Some("next"),
            ),
            (
                "next-server (v15.1.0)",
                "node",
                3000,
                Kind::Frontend,
                Some("next"),
            ),
            (
                "node /repo/node_modules/nuxi/bin/nuxi.mjs dev",
                "node",
                3000,
                Kind::Frontend,
                Some("nuxt"),
            ),
            (
                "node /repo/node_modules/.bin/astro dev",
                "node",
                4321,
                Kind::Frontend,
                Some("astro"),
            ),
            (
                "node /repo/node_modules/.bin/remix vite:dev",
                "node",
                5173,
                Kind::Frontend,
                Some("remix"),
            ),
            (
                "node /repo/node_modules/.bin/webpack serve --mode development",
                "node",
                8080,
                Kind::Frontend,
                Some("webpack"),
            ),
            (
                "node /repo/node_modules/.bin/webpack-dev-server",
                "node",
                8080,
                Kind::Frontend,
                Some("webpack"),
            ),
            (
                "node /repo/node_modules/.bin/storybook dev -p 6006",
                "node",
                6006,
                Kind::Storybook,
                Some("storybook"),
            ),
            (
                "node /repo/node_modules/react-scripts/scripts/start.js",
                "node",
                3000,
                Kind::Frontend,
                Some("create-react-app"),
            ),
            (
                "node /repo/node_modules/.bin/expo start",
                "node",
                8081,
                Kind::Frontend,
                Some("expo"),
            ),
            (
                "node /repo/node_modules/.bin/react-native start",
                "node",
                8081,
                Kind::Frontend,
                Some("metro"),
            ),
            (
                "python manage.py runserver 0.0.0.0:8000",
                "Python",
                8000,
                Kind::Api,
                Some("django"),
            ),
            (
                "/repo/.venv/bin/python /repo/.venv/bin/uvicorn app.main:app --reload",
                "Python",
                8000,
                Kind::Api,
                Some("uvicorn"),
            ),
            (
                "/repo/.venv/bin/fastapi dev main.py",
                "Python",
                8000,
                Kind::Api,
                Some("fastapi"),
            ),
            (
                "gunicorn app:app -b 0.0.0.0:8000",
                "gunicorn",
                8000,
                Kind::Api,
                Some("gunicorn"),
            ),
            (
                "hypercorn app:app",
                "Python",
                8000,
                Kind::Api,
                Some("hypercorn"),
            ),
            (
                "python -m flask run",
                "Python",
                5000,
                Kind::Api,
                Some("flask"),
            ),
            (
                "puma 6.4.0 (tcp://localhost:3000) [app]",
                "ruby",
                3000,
                Kind::Api,
                Some("rails"),
            ),
            ("bin/rails server", "ruby", 3000, Kind::Api, Some("rails")),
            (
                "/var/folders/x/T/go-build123/b001/exe/main",
                "main",
                8080,
                Kind::Api,
                Some("go"),
            ),
            (
                "/repo/target/debug/api-server",
                "api-server",
                8080,
                Kind::Api,
                Some("rust"),
            ),
            ("node server.js", "node", 4000, Kind::Api, Some("node")),
            (
                "/opt/homebrew/opt/postgresql@16/bin/postgres -D /data",
                "postgres",
                5432,
                Kind::Database,
                Some("postgres"),
            ),
            (
                "/usr/local/bin/mysqld",
                "mysqld",
                3306,
                Kind::Database,
                Some("mysql"),
            ),
            (
                "redis-server *:6379",
                "redis-server",
                6379,
                Kind::Cache,
                Some("redis"),
            ),
            (
                "mongod --dbpath /data",
                "mongod",
                27017,
                Kind::Database,
                Some("mongodb"),
            ),
            (
                "/usr/bin/docker-proxy -proto tcp -host-port 5432",
                "docker-proxy",
                5432,
                Kind::Database,
                Some("postgres"),
            ),
            (
                "/Applications/Docker.app/Contents/MacOS/com.docker.backend",
                "com.docker",
                9999,
                Kind::Other,
                Some("docker"),
            ),
            (
                "node /repo/node_modules/.bin/spotlight-sidecar",
                "node",
                8969,
                Kind::Other,
                Some("spotlight"),
            ),
            (
                "node /repo/node_modules/.bin/vitest --ui",
                "node",
                51204,
                Kind::Other,
                Some("vitest"),
            ),
            (
                "/usr/bin/python3 -m http.server 8000",
                "Python",
                8000,
                Kind::Other,
                Some("python-http-server"),
            ),
            (
                "some-binary --serve",
                "some-binary",
                9123,
                Kind::Other,
                None,
            ),
            (
                "some-binary",
                "some-binary",
                5432,
                Kind::Database,
                Some("postgres"),
            ),
        ];
        for (command, process, port, kind, framework) in cases {
            let found = classify_command(command, process, *port);
            assert_eq!(found.kind, *kind, "{command}");
            assert_eq!(found.framework.as_deref(), *framework, "{command}");
        }
    }

    fn html(body: &str) -> ProbeResponse {
        ProbeResponse {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            body: body.into(),
        }
    }

    type Expected = Option<(Kind, Option<&'static str>)>;

    #[test]
    fn probes_classify_by_markers() {
        let cases: &[(ProbeResponse, Expected)] = &[
            (
                html(r#"<script type="module" src="/@vite/client"></script>"#),
                Some((Kind::Frontend, Some("vite"))),
            ),
            (
                html(r#"<div id="__next"></div><script src="/_next/static/x.js">"#),
                Some((Kind::Frontend, Some("next"))),
            ),
            (
                html(r#"<div id="__nuxt"></div>"#),
                Some((Kind::Frontend, Some("nuxt"))),
            ),
            (
                html("<html><title>Storybook</title><body class=\"sb-show-main\">"),
                Some((Kind::Storybook, Some("storybook"))),
            ),
            (
                html("<html><div id=\"swagger-ui\"></div>"),
                Some((Kind::Docs, Some("openapi"))),
            ),
            (
                html("<!doctype html><html><div id=\"root\"></div>"),
                Some((Kind::Frontend, None)),
            ),
            (
                html("<html><body>hello</body></html>"),
                Some((Kind::Frontend, None)),
            ),
            (
                ProbeResponse {
                    status: 404,
                    content_type: "application/json".into(),
                    body: "{\"detail\":\"Not Found\"}".into(),
                },
                Some((Kind::Api, None)),
            ),
            (
                ProbeResponse {
                    status: 200,
                    content_type: "text/plain".into(),
                    body: "ok".into(),
                },
                None,
            ),
            (ProbeResponse::default(), None),
        ];
        for (response, expected) in cases {
            let found = classify_probe(response).map(|c| (c.kind, c.framework));
            let expected = expected.map(|(k, f)| (k, f.map(str::to_string)));
            assert_eq!(found, expected, "{}", response.body);
        }
    }

    #[test]
    fn openapi_spec_means_api() {
        let spec = ProbeResponse {
            status: 200,
            content_type: "application/json".into(),
            body: r#"{"openapi":"3.1.0","info":{}}"#.into(),
        };
        assert_eq!(classify_openapi(&spec).map(|c| c.kind), Some(Kind::Api));
        assert!(
            classify_openapi(&ProbeResponse {
                status: 404,
                ..spec
            })
            .is_none()
        );
    }

    #[test]
    fn confident_probe_overrides_a_weak_command() {
        let command = classify_command("python3 -m http.server 8000", "Python", 8000);
        let probe = classify_probe(&html(r#"<script src="/@vite/client">"#));
        let merged = merge(command, probe);
        assert_eq!(merged.kind, Kind::Frontend);
        assert_eq!(merged.framework.as_deref(), Some("vite"));
        assert_eq!(merged.confidence, Confidence::High);

        let command = classify_command("node node_modules/.bin/vite", "node", 5173);
        let generic = classify_probe(&html("<html></html>"));
        assert_eq!(merge(command.clone(), generic), command);

        let django = classify_command("python manage.py runserver", "Python", 8000);
        let page = classify_probe(&html("<html><div id=\"root\">"));
        assert_eq!(merge(django.clone(), page).kind, Kind::Api);
    }

    #[test]
    fn labels_suggest_kinds() {
        assert_eq!(kind_from_label("Frontend"), Some(Kind::Frontend));
        assert_eq!(kind_from_label("Web app"), Some(Kind::Frontend));
        assert_eq!(kind_from_label("API"), Some(Kind::Api));
        assert_eq!(kind_from_label("API docs"), Some(Kind::Docs));
        assert_eq!(kind_from_label("Storybook"), Some(Kind::Storybook));
        assert_eq!(kind_from_label("Postgres"), Some(Kind::Database));
        assert_eq!(kind_from_label("Redis"), Some(Kind::Cache));
        assert_eq!(kind_from_label("Spotlight"), None);
    }
}
