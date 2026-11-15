use std::fmt;

use crate::json::{Map, Value};

use super::Layer;
use super::path::KeyPath;

#[derive(Debug, Clone)]
pub enum Schema {
    Any,
    Bool,
    Number,
    Integer,
    String,
    Slug,
    Enum(&'static [&'static str]),
    Nullable(Box<Schema>),
    Array(Box<Schema>),
    Map(Box<Schema>),
    Object { fields: Vec<Field>, open: bool },
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Schema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub layer: Option<Layer>,
    pub key: String,
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(layer) = self.layer {
            write!(f, "{} settings: ", layer.name())?;
        }
        let key = if self.key.is_empty() {
            "(root)"
        } else {
            &self.key
        };
        write!(f, "`{key}`: {}", self.message)
    }
}

impl std::error::Error for ValidationError {}

fn field(name: &'static str, description: &'static str, schema: Schema) -> Field {
    Field {
        name,
        description,
        schema,
    }
}

fn closed(fields: Vec<Field>) -> Schema {
    Schema::Object {
        fields,
        open: false,
    }
}

fn open(fields: Vec<Field>) -> Schema {
    Schema::Object { fields, open: true }
}

fn array(item: Schema) -> Schema {
    Schema::Array(Box::new(item))
}

fn nullable(inner: Schema) -> Schema {
    Schema::Nullable(Box::new(inner))
}

fn map_of(value: Schema) -> Schema {
    Schema::Map(Box::new(value))
}

fn status_rules_schema() -> Schema {
    use Schema::{Bool, String as Str};
    open(vec![
        field("enabled", "Master switch for the PR status rules.", Bool),
        field(
            "statusByCondition",
            "Column id per PR condition (merged, closed, reviewing, merge-queue, conflicts, draft, changes-requested, merging, pm-approval, review). Unmapped conditions never move a card.",
            map_of(Str),
        ),
        field(
            "mergingCheckName",
            "Check whose success means the PR is merging.",
            Str,
        ),
        field(
            "newWorkspaceStatus",
            "Column new workspaces start in.",
            nullable(Str),
        ),
        field(
            "holdStatus",
            "Hold column: workspaces in it are skipped by every rule. Absent and null both mean no hold.",
            nullable(Str),
        ),
        field(
            "pmApprovalTeam",
            "GitHub team `org/team-slug` whose approval counts as PM approval.",
            nullable(Str),
        ),
        field(
            "onResolved",
            "`delete` removes your workspace once its PR is merged or closed.",
            Str,
        ),
        field(
            "onReviewed",
            "`delete` removes a review workspace once you have reviewed.",
            Str,
        ),
        field(
            "resolveConflictsWithAgent",
            "Launch an agent when your PR enters the conflicts column.",
            Bool,
        ),
        field(
            "conflictPromptTemplate",
            "Conflict agent prompt; blank means built-in.",
            Str,
        ),
        field(
            "reviewEnteredCommand",
            "Shell command run when a workspace enters the review column.",
            Str,
        ),
        field(
            "actionPrompts",
            "Prompts for the top-bar actions; blank means built-in.",
            open(vec![
                field("commitAndPush", "", Str),
                field("createPullRequest", "", Str),
                field("readyForReview", "", Str),
                field("reviewPullRequest", "", Str),
            ]),
        ),
        field(
            "reviewInbox",
            "Auto-create review workspaces for review requests.",
            open(vec![
                field("enabled", "", Bool),
                field(
                    "agent",
                    "Agent id for review, conflict and action prompts.",
                    Str,
                ),
                field("view", "chat | terminal | claude-web | default", Str),
                field("promptTemplate", "First-look review prompt.", Str),
                field("rereviewPromptTemplate", "Re-review prompt.", Str),
            ]),
        ),
    ])
}

fn hooks_schema() -> Schema {
    use Schema::String as Str;
    open(vec![
        field("mode", "auto | override", Str),
        field(
            "setupRunPolicy",
            "ask | run-by-default | skip-by-default",
            Str,
        ),
        field(
            "setupAgentStartupPolicy",
            "start-immediately | wait-for-setup",
            Str,
        ),
        field(
            "commandSourcePolicy",
            "shared-only | local-only | run-both",
            Str,
        ),
        field(
            "scripts",
            "Setup and archive scripts.",
            open(vec![field("setup", "", Str), field("archive", "", Str)]),
        ),
    ])
}

fn project_schema() -> Schema {
    use Schema::{Any, String as Str};
    closed(vec![
        field(
            "hooks",
            "Setup and archive scripts and their policies.",
            hooks_schema(),
        ),
        field(
            "ciTriage",
            "Present means CI triage is on; `prompt` overrides the built-in instructions.",
            open(vec![
                field("prompt", "", Str),
                field("hints", "Legacy.", Str),
            ]),
        ),
        field("worktreeBaseRef", "Base ref for new worktrees.", Str),
        field("issueSourcePreference", "upstream | origin | auto", Str),
        field("defaultJiraProject", "", Any),
        field("forkSyncMode", "ask | safe-auto | off", Str),
        field("externalWorktreeVisibility", "hide | show", Str),
        field("agentWorktreeVisibility", "hide | show", Str),
        field("worktreeVisibilitySourcePreferences", "", Any),
        field(
            "symlinkPaths",
            "Relative paths shared into worktrees.",
            array(Str),
        ),
        field("pythonVenvPath", "Relative path of the Python venv.", Str),
        field("portLabelsCommand", "Command that prints port labels.", Str),
        field(
            "sourceControlAi",
            "Per-project source-control AI overrides.",
            Any,
        ),
    ])
}

pub fn document_schema() -> Schema {
    use Schema::{Any, Bool, Integer, Number, String as Str};
    closed(vec![
        field("$schema", "JSON Schema reference.", Str),
        field(
            "workspace",
            "Workspace board and cards.",
            closed(vec![
                field(
                    "statuses",
                    "Board columns in order. Ids are stable: rules refer to them.",
                    array(open(vec![
                        field("id", "Stable column id.", Schema::Slug),
                        field("label", "Column label.", Str),
                        field("color", "", Str),
                        field("icon", "", Str),
                    ])),
                ),
                field(
                    "cardFields",
                    "Fields shown on workspace cards, in order.",
                    array(Str),
                ),
                field("boardOpacity", "", Number),
                field("boardColumnWidth", "", Integer),
            ]),
        ),
        field(
            "sidebar",
            "Sidebar grouping and sorting.",
            closed(vec![
                field("groupBy", "", Str),
                field("sortBy", "", Str),
                field("projectOrderBy", "", Str),
                field("stackOrder", "", Str),
            ]),
        ),
        field(
            "statusBar",
            "Status bar.",
            closed(vec![
                field("items", "", array(Str)),
                field("visible", "", Bool),
                field("usageMode", "", Str),
                field("usagePercentageDisplay", "", Str),
            ]),
        ),
        field(
            "rules",
            "Automations.",
            closed(vec![
                field(
                    "orca_compat",
                    "Orca's PR status rule config, consumed by the rules engine as is.",
                    status_rules_schema(),
                ),
                field("enabled", "Run the automation loop.", Bool),
                field(
                    "armed",
                    "Execute planned actions. Until true, every tick is a dry run.",
                    Bool,
                ),
                field(
                    "projects",
                    "Projects (names or canonical remotes) the rules apply to.",
                    array(Str),
                ),
                field("interval_ms", "Polling interval in milliseconds.", Integer),
            ]),
        ),
        field(
            "agents",
            "Agent defaults.",
            open(vec![
                field(
                    "default",
                    "Provider for new agent sessions, e.g. claude or codex.",
                    Str,
                ),
                field(
                    "remote_control",
                    "Start new Claude Code sessions with Claude Code's own Remote Control, so they can be followed on claude.ai or the Claude app. Default false; each agent can be switched with `icarus agent remote-control`.",
                    Bool,
                ),
            ]),
        ),
        field(
            "actions",
            "Per next-action overrides keyed by action kind (agent, prompt).",
            Schema::Map(Box::new(open(vec![
                field("agent", "Provider for this step.", Str),
                field("prompt", "Prompt template for this step.", Str),
            ]))),
        ),
        field(
            "layouts",
            "Named tab layouts: a split tree ({split: row|column, children, sizes}) of panes ({tabs, active}). Tab types: agent, browser ({url} with {pr_url}, {preview_url}, {frontend_url}, {workspace_path}), diff, explorer, file, terminal.",
            Schema::Map(Box::new(Any)),
        ),
        field(
            "quickCommands",
            "Terminal quick commands. A `project` scope names a canonical remote.",
            array(open(vec![
                field("id", "", Str),
                field("label", "", Str),
                field(
                    "scope",
                    "",
                    open(vec![field("type", "", Str), field("remote", "", Str)]),
                ),
                field("action", "", Str),
                field("command", "", Str),
                field("appendEnter", "", Bool),
            ])),
        ),
        field(
            "keybindings",
            "Keybindings in Orca's keybindings.json shape.",
            open(vec![
                field("version", "", Number),
                field("keybindings", "", map_of(Any)),
                field("platforms", "", map_of(map_of(Any))),
            ]),
        ),
        field(
            "presets",
            "Shipped presets applied between the built-in defaults and your settings, e.g. `github-pr-flow`. Read from user settings only.",
            array(Str),
        ),
        field(
            "theme",
            "Appearance.",
            closed(vec![
                field("mode", "", Schema::Enum(&["system", "dark", "light"])),
                field(
                    "terminal",
                    "",
                    closed(vec![
                        field("dark", "", Str),
                        field("light", "", Str),
                        field("separateLight", "", Bool),
                        field("custom", "", array(Any)),
                    ]),
                ),
                field(
                    "editor",
                    "",
                    closed(vec![
                        field("dark", "", editor_theme_ref()),
                        field("light", "", editor_theme_ref()),
                    ]),
                ),
                field(
                    "fonts",
                    "",
                    closed(vec![
                        field("ui", "", Str),
                        field("editor", "", Str),
                        field("terminal", "", Str),
                        field("terminalSize", "", Number),
                        field("terminalLineHeight", "", Number),
                    ]),
                ),
                field(
                    "inactivePaneOpacity",
                    "Opacity of panes that don't have focus, 0 to 1.",
                    Number,
                ),
            ]),
        ),
        field(
            "changes",
            "Changes list filters. Facets extend the built-in `type` facet by id or add new ones (e.g. `area`); see source-control.md §7.",
            open(vec![
                field(
                    "facets",
                    "Facets in menu order. Each puts a file into at most one value by its first matching rule.",
                    array(open(vec![
                        field("id", "Facet id, e.g. type or area.", Str),
                        field("label", "Menu group label.", Str),
                        field(
                            "default",
                            "Value of files no rule matches; absent means no value (never hidden).",
                            Str,
                        ),
                        field(
                            "values",
                            "Values in menu order.",
                            array(open(vec![field("id", "", Str), field("label", "", Str)])),
                        ),
                        field(
                            "rules",
                            "Ordered rules `{value, when, id?, before?}`; overlay rules go ahead of the preset's unless `before` names a rule id.",
                            array(open(vec![
                                field("id", "", Str),
                                field("value", "", Str),
                                field("before", "", Str),
                                field(
                                    "when",
                                    "Predicate: path, regex, attr, change, content, builtin, all, any, not.",
                                    Any,
                                ),
                            ])),
                        ),
                        field(
                            "replace",
                            "Replace the built-in facet of this id instead of extending it.",
                            Bool,
                        ),
                    ])),
                ),
                field(
                    "analyzers",
                    "Content analyzers.",
                    open(vec![field(
                        "imports_only",
                        "The imports-only analyzer (TS/JS/Python), always merge base → disk.",
                        open(vec![
                            field(
                                "max_changed_lines",
                                "Candidates change at most this many lines.",
                                Integer,
                            ),
                            field(
                                "max_files",
                                "At most this many candidates, by path.",
                                Integer,
                            ),
                            field(
                                "max_bytes",
                                "Either side over this many characters → not imports-only.",
                                Integer,
                            ),
                            field(
                                "semantics",
                                "`orca-v1`: a pure reorder or reformat is not imports-only. `v2`: it is.",
                                Schema::Enum(&["orca-v1", "v2"]),
                            ),
                        ]),
                    )]),
                ),
            ]),
        ),
        field(
            "previews",
            "How a pull request's preview deployments are found.",
            closed(vec![
                field(
                    "environment_pattern",
                    "Regex a GitHub deployment environment must match to count as a preview.",
                    Str,
                ),
                field(
                    "exclude_pattern",
                    "Regex of deployment environments that are never previews; empty turns it off.",
                    Str,
                ),
                field(
                    "comment_pattern",
                    "Regex for preview links in PR comments, with named groups `url` (required), `app` and `sha`.",
                    Str,
                ),
                field(
                    "primary_app",
                    "App opened by `icarus pr preview` when none is named.",
                    Str,
                ),
            ]),
        ),
        field("project", "Per-project settings.", project_schema()),
        field(
            "ports",
            "Ports & services: how a workspace's listening ports are named and classified.",
            ports_schema(),
        ),
        field(
            "linear",
            "Linear issue titles and details on workspace cards. `LINEAR_API_KEY` in the server's environment wins over these.",
            closed(vec![
                field(
                    "api_key_command",
                    "Command the server runs to print the API key (argv array, or a string run with `sh -c`), e.g. `bw get notes <item-id>`. Output is trimmed and kept in memory only; re-run when Linear rejects the key. Read from user settings only.",
                    Any,
                ),
                field(
                    "poll_interval_ms",
                    "How often linked issues are refreshed. Default 300000 (5 minutes); at least 1000.",
                    Integer,
                ),
            ]),
        ),
        field(
            "server",
            "The server process.",
            closed(vec![field(
                "listen",
                "TCP addresses (`ip:port`) to accept paired clients on, besides the local socket, e.g. a Tailscale address. Used when `icarus-server serve` gets no `--listen`.",
                array(Str),
            )]),
        ),
        field(
            "notifications",
            "Server notifications. Every switch defaults to on.",
            closed(vec![
                field("enabled", "Master switch for all notifications.", Bool),
                field(
                    "sound",
                    "Clients play a sound with each notification.",
                    Bool,
                ),
                field("agent_turn_done", "An agent finished its turn.", Bool),
                field(
                    "agent_waiting",
                    "An agent waits for input or a permission.",
                    Bool,
                ),
                field(
                    "agent_failed",
                    "An agent's turn failed or its process died.",
                    Bool,
                ),
                field(
                    "ci_failed",
                    "Checks failed on a workspace's pull request.",
                    Bool,
                ),
                field("review_requested", "Someone asked for your review.", Bool),
                field("pr_merged", "A workspace's pull request merged.", Bool),
                field("rules", "A rule's `notify` action.", Bool),
                field(
                    "update_available",
                    "A newer Icarus release is out (once per version).",
                    Bool,
                ),
            ]),
        ),
        field(
            "updates",
            "Update checks against Icarus's GitHub releases. `icarus update check` works whatever these say.",
            closed(vec![
                field(
                    "channel",
                    "`stable` checks for new releases in the background (release builds only); `none` never does.",
                    Schema::Enum(&["stable", "none"]),
                ),
                field(
                    "check_interval_ms",
                    "How often the background check runs. Default 21600000 (6 hours); at least 60000.",
                    Integer,
                ),
            ]),
        ),
        field(
            "orca",
            "Orca compatibility data kept so an export reproduces the original file.",
            Any,
        ),
    ])
}

const PORT_KINDS: &[&str] = &[
    "frontend",
    "api",
    "docs",
    "storybook",
    "database",
    "cache",
    "other",
];

fn ports_schema() -> Schema {
    use Schema::{Bool, Integer, String as Str};
    closed(vec![
        field(
            "labels_command",
            "Command run in the worktree that prints `label: url`, `label url`, or JSON `[{label,url,kind?}]` / `{label: url}` to name its services (e.g. `mise run print-urls`).",
            Str,
        ),
        field(
            "rules",
            "Overrides applied after detection, in order: `{match: {port?, command_regex?, label_regex?}, label?, kind?, primary?}`.",
            array(closed(vec![
                field(
                    "match",
                    "All given conditions must hold.",
                    closed(vec![
                        field("port", "Listening port.", Integer),
                        field(
                            "command_regex",
                            "Regex against the process command line.",
                            Str,
                        ),
                        field(
                            "label_regex",
                            "Regex against the detected or command label.",
                            Str,
                        ),
                    ]),
                ),
                field("label", "Label to show.", Str),
                field("kind", "Service kind.", Schema::Enum(PORT_KINDS)),
                field(
                    "primary",
                    "Make this the URL `ports.primary` returns for its kind.",
                    Bool,
                ),
            ])),
        ),
        field(
            "probe",
            "Send a short HTTP request to unclassified ports to detect the framework. Default true.",
            Bool,
        ),
    ])
}

fn editor_theme_ref() -> Schema {
    use Schema::String as Str;
    open(vec![
        field("name", "", nullable(Str)),
        field("orcaFile", "", Str),
    ])
}

pub fn validate(doc: &Value, layer: Option<Layer>) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    check(
        &document_schema(),
        doc,
        &KeyPath::default(),
        layer,
        &mut errors,
    );
    errors
}

fn check(
    schema: &Schema,
    value: &Value,
    path: &KeyPath,
    layer: Option<Layer>,
    errors: &mut Vec<ValidationError>,
) {
    let mut fail = |message: String| {
        errors.push(ValidationError {
            layer,
            key: path.to_string(),
            message,
        });
    };
    match (schema, value) {
        (Schema::Any, _)
        | (Schema::Bool, Value::Bool(_))
        | (Schema::Number, Value::Number(_))
        | (Schema::String, Value::String(_)) => {}
        (Schema::Integer, Value::Number(n)) => {
            if n.as_i64().is_none() {
                fail(format!("expected an integer, found {n}"));
            }
        }
        (Schema::Slug, Value::String(text)) => {
            let ok = !text.is_empty()
                && text.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                });
            if !ok {
                fail(format!(
                    "`{text}` is not a valid id (lowercase letters, digits, `-`, `_`)"
                ));
            }
        }
        (Schema::Enum(allowed), Value::String(text)) => {
            if !allowed.contains(&text.as_str()) {
                fail(format!("`{text}` is not one of {}", allowed.join(", ")));
            }
        }
        (Schema::Nullable(_), Value::Null) => {}
        (Schema::Nullable(inner), _) => check(inner, value, path, layer, errors),
        (Schema::Array(item), Value::Array(items)) => {
            for (i, entry) in items.iter().enumerate() {
                check(item, entry, &path.child(i.to_string()), layer, errors);
            }
        }
        (Schema::Map(item), Value::Object(map)) => {
            for (key, entry) in map {
                check(item, entry, &path.child(key.clone()), layer, errors);
            }
        }
        (Schema::Object { fields, open }, Value::Object(map)) => {
            for (key, entry) in map {
                match fields.iter().find(|f| f.name == key) {
                    Some(f) => check(&f.schema, entry, &path.child(key.clone()), layer, errors),
                    None if !open => errors.push(ValidationError {
                        layer,
                        key: path.child(key.clone()).to_string(),
                        message: "unknown setting".to_owned(),
                    }),
                    None => {}
                }
            }
        }
        (schema, value) => fail(format!(
            "expected {}, found {}",
            expected(schema),
            value.kind()
        )),
    }
}

fn expected(schema: &Schema) -> &'static str {
    match schema {
        Schema::Any => "any value",
        Schema::Bool => "a boolean",
        Schema::Number => "a number",
        Schema::Integer => "an integer",
        Schema::String | Schema::Slug | Schema::Enum(_) => "a string",
        Schema::Nullable(_) => "a value or null",
        Schema::Array(_) => "an array",
        Schema::Map(_) | Schema::Object { .. } => "an object",
    }
}

pub fn json_schema() -> Value {
    let mut root = Map::new();
    root.insert(
        "$schema".into(),
        "https://json-schema.org/draft/2020-12/schema".into(),
    );
    root.insert(
        "$id".into(),
        "https://icarus.invalid/schemas/settings.json".into(),
    );
    root.insert("title".into(), "Icarus settings".into());
    root.insert(
        "description".into(),
        "One layer of Icarus settings: built-in defaults < presets < ~/.config/icarus/icarus.json < icarus.json < icarus.local.json. Objects merge deeply; arrays and scalars replace.".into(),
    );
    if let Value::Object(body) = to_json_schema(&document_schema()) {
        root.extend(body);
    }
    Value::Object(root)
}

fn type_only(name: &str) -> Value {
    let mut map = Map::new();
    map.insert("type".into(), name.into());
    Value::Object(map)
}

fn to_json_schema(schema: &Schema) -> Value {
    match schema {
        Schema::Any => Value::object(),
        Schema::Bool => type_only("boolean"),
        Schema::Number => type_only("number"),
        Schema::Integer => type_only("integer"),
        Schema::String => type_only("string"),
        Schema::Slug => {
            let mut map = Map::new();
            map.insert("type".into(), "string".into());
            map.insert("pattern".into(), "^[a-z0-9_-]+$".into());
            Value::Object(map)
        }
        Schema::Enum(allowed) => {
            let mut map = Map::new();
            map.insert("type".into(), "string".into());
            map.insert(
                "enum".into(),
                Value::Array(allowed.iter().map(|a| Value::from(*a)).collect()),
            );
            Value::Object(map)
        }
        Schema::Nullable(inner) => {
            let mut map = Map::new();
            map.insert(
                "anyOf".into(),
                Value::Array(vec![to_json_schema(inner), type_only("null")]),
            );
            Value::Object(map)
        }
        Schema::Array(item) => {
            let mut map = Map::new();
            map.insert("type".into(), "array".into());
            map.insert("items".into(), to_json_schema(item));
            Value::Object(map)
        }
        Schema::Map(item) => {
            let mut map = Map::new();
            map.insert("type".into(), "object".into());
            map.insert("additionalProperties".into(), to_json_schema(item));
            Value::Object(map)
        }
        Schema::Object { fields, open } => {
            let mut map = Map::new();
            map.insert("type".into(), "object".into());
            let mut properties = Map::new();
            for f in fields {
                let mut prop = to_json_schema(&f.schema);
                if let (Value::Object(p), false) = (&mut prop, f.description.is_empty()) {
                    p.insert("description".into(), f.description.into());
                }
                properties.insert(f.name.to_owned(), prop);
            }
            map.insert("properties".into(), Value::Object(properties));
            map.insert("additionalProperties".into(), Value::Bool(*open));
            Value::Object(map)
        }
    }
}
