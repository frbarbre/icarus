//! Orca's language servers, formatters and linters as plain Zed settings.

use crate::json::{Map, Value};

/// An Orca tool and how Zed runs it: the Zed server name, and the extension that provides it
/// (`None` when Zed has the server built in).
struct Tool {
    orca: &'static str,
    zed: &'static str,
    extension: Option<&'static str>,
    args: &'static [&'static str],
}

struct Language {
    orca: &'static str,
    zed: &'static [&'static str],
    servers: &'static [Tool],
    default_server: &'static str,
    /// Zed servers doing the same job, turned off so only the chosen one runs (as in Orca).
    rivals: &'static [&'static str],
    formatter: Option<&'static str>,
    linter: Option<&'static str>,
}

// Orca's registry (`shared/language-server.ts`, `formatter.ts`, `linter.ts`) against Zed's built-in
// adapters and the zed-industries/extensions registry (`tsgo`, `oxc`).
const LANGUAGES: [Language; 2] = [
    Language {
        orca: "python",
        zed: &["Python"],
        servers: &[
            Tool {
                orca: "pyrefly",
                zed: "pyrefly",
                extension: None,
                args: &["lsp"],
            },
            Tool {
                orca: "pyright",
                zed: "pyright",
                extension: None,
                args: &["--stdio"],
            },
            Tool {
                orca: "basedpyright",
                zed: "basedpyright",
                extension: None,
                args: &["--stdio"],
            },
            Tool {
                orca: "ty",
                zed: "ty",
                extension: None,
                args: &["server"],
            },
        ],
        default_server: "pyrefly",
        rivals: &["basedpyright", "pyrefly", "pylsp", "pyright", "ty"],
        formatter: Some("ruff"),
        linter: None,
    },
    Language {
        orca: "typescript",
        zed: &["TypeScript", "TSX", "JavaScript"],
        servers: &[
            Tool {
                orca: "tsgo",
                zed: "typescript-ls",
                extension: Some("tsgo"),
                args: &["--lsp", "--stdio"],
            },
            Tool {
                orca: "typescript-language-server",
                zed: "typescript-language-server",
                extension: None,
                args: &["--stdio"],
            },
        ],
        default_server: "tsgo",
        rivals: &[
            "eslint",
            "typescript-language-server",
            "typescript-ls",
            "vtsls",
        ],
        formatter: Some("oxfmt"),
        linter: Some("oxlint"),
    },
];

const FORMATTERS: [Tool; 2] = [
    Tool {
        orca: "oxfmt",
        zed: "oxfmt",
        extension: Some("oxc"),
        args: &["--lsp"],
    },
    Tool {
        orca: "ruff",
        zed: "ruff",
        extension: None,
        args: &["server"],
    },
];

const LINTERS: [Tool; 1] = [Tool {
    orca: "oxlint",
    zed: "oxlint",
    extension: Some("oxc"),
    args: &["--lsp"],
}];

#[derive(Debug, Default)]
pub(crate) struct Tooling {
    /// `auto_install_extensions`, `languages` and `lsp` in Zed's settings shape.
    pub zed: Map,
    /// `(Orca key, Zed key)` pairs for the import report.
    pub mapped: Vec<(String, String)>,
    pub notes: Vec<String>,
}

struct Choice {
    enabled: bool,
    command: Option<String>,
}

fn choice(value: Option<&Value>) -> Choice {
    let command = value
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    let enabled = match value {
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => other.get("enabled").and_then(Value::as_bool) != Some(false),
        None => true,
    };
    Choice { enabled, command }
}

fn strings(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|s| Value::from(*s)).collect())
}

fn section<'a>(settings: &'a Map, key: &str) -> Option<&'a Map> {
    settings.get(key).and_then(Value::as_object)
}

fn object_at<'a>(map: &'a mut Map, key: &str) -> &'a mut Map {
    let entry = map
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    match entry {
        Value::Object(map) => map,
        _ => unreachable!("replaced with an object above"),
    }
}

pub(crate) fn translate(settings: &Map) -> Tooling {
    let mut out = Tooling::default();
    let servers = section(settings, "languageServers");
    let formatters = section(settings, "formatters");
    let linters = section(settings, "linters");

    let sections: [(&str, Option<&Map>, Vec<&str>); 3] = [
        (
            "languageServers",
            servers,
            LANGUAGES.iter().map(|l| l.orca).collect(),
        ),
        (
            "formatters",
            formatters,
            FORMATTERS.iter().map(|t| t.orca).collect(),
        ),
        ("linters", linters, LINTERS.iter().map(|t| t.orca).collect()),
    ];
    for (key, map, known) in sections {
        if settings.contains_key(key) && map.is_none() {
            out.notes.push(format!(
                "settings.{key} is not an object; Orca's defaults apply"
            ));
        }
        for name in map.into_iter().flat_map(|m| m.keys()) {
            if !known.contains(&name.as_str()) {
                out.notes.push(format!(
                    "settings.{key}.{name}: Orca has no such entry; skipped (kept for export)"
                ));
            }
        }
    }
    if servers.is_none() && formatters.is_none() && linters.is_none() {
        out.notes.push(
            "settings.languageServers/formatters/linters are absent: Orca's defaults (pyrefly + Ruff, tsgo + oxlint + oxfmt) are imported".into(),
        );
    }

    for language in &LANGUAGES {
        let server_value = servers.and_then(|s| s.get(language.orca));
        let server = choice(server_value);
        let requested = server_value
            .and_then(|v| v.get("server"))
            .and_then(Value::as_str);
        let Some(tool) = language
            .servers
            .iter()
            .find(|t| Some(t.orca) == requested)
            .or_else(|| {
                language
                    .servers
                    .iter()
                    .find(|t| t.orca == language.default_server)
            })
        else {
            continue;
        };
        if let Some(requested) = requested.filter(|r| *r != tool.orca) {
            out.notes.push(format!(
                "settings.languageServers.{}.server: `{requested}` is not one of Orca's servers; {} is used, as in Orca",
                language.orca, tool.orca
            ));
        }
        let from = format!("settings.languageServers.{}", language.orca);
        let formatter = language.formatter.and_then(|id| {
            let tool = FORMATTERS.iter().find(|t| t.orca == id)?;
            Some((
                tool,
                choice(formatters.and_then(|f| f.get(id))),
                format!("settings.formatters.{id}"),
            ))
        });
        let linter = language.linter.and_then(|id| {
            let tool = LINTERS.iter().find(|t| t.orca == id)?;
            Some((
                tool,
                choice(linters.and_then(|f| f.get(id))),
                format!("settings.linters.{id}"),
            ))
        });

        let mut used: Vec<(&Tool, &Choice, String)> = Vec::new();
        let mut off: Vec<&str> = Vec::new();
        if server.enabled {
            used.push((tool, &server, from.clone()));
        }
        for (extra, extra_choice, extra_from) in [&linter, &formatter].into_iter().flatten() {
            if extra_choice.enabled {
                used.push((extra, extra_choice, extra_from.clone()));
            } else {
                off.push(extra.zed);
            }
        }
        off.extend(
            language
                .rivals
                .iter()
                .filter(|r| !used.iter().any(|(t, _, _)| t.zed == **r)),
        );
        off.sort_unstable();
        off.dedup();
        let mut list: Vec<Value> = used.iter().map(|(t, _, _)| Value::from(t.zed)).collect();
        list.extend(off.iter().map(|r| Value::from(format!("!{r}"))));
        list.push("...".into());

        let mut entry = Map::new();
        entry.insert("language_servers".into(), Value::Array(list));
        if let Some((fmt, fmt_choice, fmt_from)) = &formatter
            && fmt_choice.enabled
        {
            let mut name = Map::new();
            name.insert("name".into(), fmt.zed.into());
            let mut server = Map::new();
            server.insert("language_server".into(), Value::Object(name));
            entry.insert("formatter".into(), Value::Object(server));
            entry.insert("format_on_save".into(), "on".into());
            out.notes.push(format!(
                "{fmt_from}: Zed formats on every save with {}, while Orca formatted only on Mod+S in projects with its config file",
                fmt.orca
            ));
        }
        for zed in language.zed {
            object_at(&mut out.zed, "languages")
                .insert((*zed).into(), Value::Object(entry.clone()));
            out.mapped.push((from.clone(), format!("languages.{zed}")));
        }

        for (tool, choice, tool_from) in used {
            if let Some(extension) = tool.extension {
                object_at(&mut out.zed, "auto_install_extensions")
                    .insert(extension.into(), true.into());
                out.mapped.push((
                    tool_from.clone(),
                    format!("auto_install_extensions.{extension}"),
                ));
            }
            if let Some(command) = &choice.command {
                let mut binary = Map::new();
                binary.insert("path".into(), command.as_str().into());
                binary.insert("arguments".into(), strings(tool.args));
                let mut server = Map::new();
                server.insert("binary".into(), Value::Object(binary));
                object_at(&mut out.zed, "lsp").insert(tool.zed.into(), Value::Object(server));
                out.mapped
                    .push((tool_from, format!("lsp.{}.binary", tool.zed)));
            }
        }
    }
    out
}
