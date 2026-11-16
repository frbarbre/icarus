# Icarus: open-source building blocks research

Date: 2026-11-15. Scope: a native (macOS first) Rust + GPUI git-worktree agent workspace. Each worktree runs a coding agent (mainly Claude Code) in a chat/composer UI, with a code viewer/editor with LSP go-to-definition, a diff/source-control view, and GitHub PR review. It replaces Orca (Electron). The complaints about Orca are slow Claude session start and slow Cmd+click go-to-definition in Monaco.

How this was checked: crates.io API metadata (versions and dates below are as of today), GitHub API repo metadata, a sparse clone of `zed-industries/zed` at `596dfdc` (2026-11-15) to read crate manifests, licenses and agent UI source, and Zed and Claude Code docs.

---

## 0. TL;DR

1. **Zed already ships most of this product.** "Parallel Agents" ([blog, 2026-04-22](https://zed.dev/blog/parallel-agents), [docs](https://zed.dev/docs/ai/parallel-agents)) adds a Threads Sidebar of many concurrent agent threads. You can pick the agent per thread (Claude Code via ACP), create a git worktree per thread ("New Git Worktree" in the Agent Panel toolbar), and archive a worktree (`agent_ui/src/thread_worktree_archive.rs`). Zed also has the editor/LSP and the git panel and diff. **Before building anything, use stock Zed (Agentic layout) for a week** and write down what is actually missing. The gap is probably GitHub PR review, an Orca-style worktree list and workflow, and Orca-specific automations.
2. **For the composer, the recommendation is option (a): a soft fork of Zed** (a branch rebased on upstream that adds and hides features). It does not extract `agent_ui`, and it does not reimplement. `agent_ui` pulls in 156 of Zed's ~253 internal crates, 119 of them GPL-3.0. Extracting it means carrying most of Zed anyway, without Zed's integration. Reimplementing gives up ~100k lines of mature thread UI.
3. If you want your own codebase and a small binary instead, use option (c). Build on **GPUI (Apache-2.0) + gpui-component/gpui-kit (Apache-2.0)** and talk **ACP** (`agent-client-protocol` 5.3.0, Apache-2.0) to **`@agentclientprotocol/claude-agent-acp`** (v0.107.0, Apache-2.0). Use Zed's `acp_thread` and `thread_view.rs` as reference only. This stays permissively licensed. Expect months of work to reach Zed's level of polish.
4. **Licensing:** for personal use, GPL-3.0 doesn't matter. If you distribute a Zed fork, the whole app must be GPL-3.0 with source available. GPUI, gpui-component, ACP, claude-agent-acp, gix, alacritty_terminal and octocrab are all permissive.

---

## 1. Interpreting the original brief

| Phrase | Most plausible meaning | Notes |
|---|---|---|
| "Zed has a really fast editor written in Rust" | Zed's `editor` + `multi_buffer` + `language` + `lsp` + `project` crates (GPL-3.0) | Go-to-definition in Zed is native: rope/sum_tree buffers, tree-sitter and LSP, with no JS bridge. |
| "GPUI for the agent UI" | `gpui` (Apache-2.0), Zed's UI framework | Use it directly, with gpui-component for widgets if you don't take Zed's `ui` crate. |
| "something like Open Web UI for their agent, a chat agent and agent view" | **Zed's Agent Panel / thread view** (`agent_ui` + `acp_thread`) | This is the GPUI-native "chat with an agent" view, rendering tool calls, diffs, plans, thinking, permission prompts, images, subagents and a message queue. Open WebUI ([open-webui/open-webui](https://github.com/open-webui/open-webui), ★158k, Python + Svelte, custom "Open WebUI License" since v0.6.6: BSD-3 plus a branding clause) is a chat front end for LLM APIs, not coding agents. Nothing in it is reusable for a Rust app beyond UX ideas. OpenCode ([anomalyco/opencode](https://github.com/anomalyco/opencode), MIT, TypeScript client/server) is a coding *agent* with its own TUI/desktop UI. It speaks ACP (`opencode acp`), so it is a second agent you could plug in, not a UI to reuse. |

---

## 2. GPUI

| Aspect | Finding |
|---|---|
| License | Apache-2.0 (`crates/gpui`, `gpui_platform`, `gpui_macos` link `LICENSE-APACHE`). 37 Zed crates are Apache-2.0 and 208 are GPL-3.0-or-later. |
| Maturity | Pre-1.0. The README says "There will often be breaking changes between versions." It is production-proven by Zed (120 fps) and Longbridge Pro. |
| crates.io (official) | `gpui` **0.2.2, published 2025-10-22**. It is stale: upstream has since split it into `gpui` + `gpui_platform` + `gpui_macos`/`gpui_linux`/`gpui_windows`/`gpui_web`, and those split crates are not published by Zed. |
| crates.io (snapshots) | **`gpui-pre` / `gpui-pre-platform` 0.3.12** (2026-11-09, Apache-2.0, owner `huacnlee` from Longbridge). These are periodic snapshots of Zed's GPUI crates, used by gpui-kit 0.9.1. There is also **`gpui-fast` 0.1.15** (longbridge/gpui-fast, Apache-2.0), an optional performance fork. |
| Git dep (what serious apps do) | `gpui = { git = "https://github.com/zed-industries/zed", rev = "<sha>" }` plus `gpui_platform` (features `font-kit` on macOS) and `gpui_macos`. diri pins it this way and even vendors a patched `gpui_macos`. The trade-off: the git dep fetches the whole Zed repo, but you get current APIs. |
| Platforms | macOS (Metal), Linux (wgpu on git main; X11/Wayland), Windows (DirectX), and experimental web (`gpui_web`/wasm). macOS is the most mature. |
| Docs | Thin. Use the `crates/gpui/README.md` and `crates/gpui/examples/*`, [gpui.rs](https://www.gpui.rs), the gpui-kit docs ([gpui-kit.com](https://gpui-kit.com)) and their story/gallery app. Reading Zed source is the real documentation. |

**Recommendation:** for your own app (option c), depend on **`gpui-kit` 0.9.1**, which pins `gpui-pre =0.3.12` and re-exports GPUI. Then bump in lockstep with gpui-kit's weekly releases. For a Zed fork (option a), the fork's in-tree GPUI is used automatically.

### Component libraries

| Library | License | State | Notes |
|---|---|---|---|
| **longbridge/gpui-kit** (formerly `gpui-component`. The GitHub repo was renamed, and the crates `gpui-kit`/`gpui-component`/`gpui-base` are 0.9.1, 2026-11-09) | Apache-2.0 | ★20.7k, weekly releases, used in production (Longbridge Pro) | 75+ components: inputs, lists, **virtual lists (variable height)**, **tables (virtualized, 100k+ rows)**, **dock layout (serializable)**, tabs, **Markdown + HTML rendering**, **code editor (tree-sitter highlighting, LSP diagnostics/completion/hover, 200k lines)**, charts, themes, AccessKit, headless UI tests, a webview crate. **The best fit for option (c).** |
| Zed `ui` crate | GPL-3.0 | in-tree | Zed's own design system. It pulls in 20 crates (7 GPL), including `theme` and `settings`. |
| gpuikit, guise-ui, gpui-ce, glassy-ui | various | small or one-person | Not recommended. |

### Real GPUI apps outside Zed
- **Longbridge Pro**: a commercial trading app, built with gpui-kit.
- **[penso/arbor](https://github.com/penso/arbor)** (MIT, ★846, pushed 2026-10-27): a native **git worktree manager with terminals, diffs and agent status (Claude Code/Codex)**. It uses `gpui 0.2.2` from crates.io, `gix 0.80`, `git2 0.20`, `gix-diff`, and Zed's `alacritty_terminal` fork. **This is the closest permissive codebase to fork for the worktree and diff shell**, but it is terminal-centric with no chat composer.
- **[cristicretu/diri](https://github.com/cristicretu/diri)** (**AGPL-3.0**, ★494, pushed 2026-11-12): Rust + GPUI (git dep on Zed at a pinned rev) plus a Swift sidecar. It runs many agents in parallel across worktrees and SSH hosts. The UI is terminals, not chat. Use it as a reference only, because AGPL.
- Herdr GPUI (Apache-2.0, a client for the Herdr runtime), and OpenSquirrel (abandoned 2026-03).

---

## 3. Zed crates: reuse, footprint, license

Computed from the workspace manifests at `596dfdc` (internal crates only, non-dev deps, transitive):

| Crate | Internal crates pulled in | of which GPL-3.0 | Role |
|---|---|---|---|
| `gpui` | 13 | 0 | UI framework |
| `gpui_platform` | 21 | 0 | platform backends |
| `ui` | 20 | 7 | Zed design system |
| `lsp` | 18 | 2 | LSP client (JSON-RPC over stdio, uses Zed's `lsp-types` fork) |
| `git` | 23 | 7 | git: **shells out to the `git` CLI** (no git2/gix dependency) |
| `language` | 49 | 29 | buffers, tree-sitter, language registry |
| `markdown` | 57 | 37 | markdown rendering in GPUI |
| `project` | 82 | 55 | worktrees, LSP store, git store, buffer store |
| `acp_thread` | 91 | 64 | ACP thread model (entries, tool calls, diffs, plans, terminals) |
| `workspace` | 92 | 65 | panes, docks, items |
| `agent_servers` | 93 | 65 | spawns ACP agents (Claude/Codex/Gemini/custom, registry) |
| `editor` | 100 | 73 | the editor (incl. `multi_buffer`) |
| `terminal_view` | 101 | 74 | terminal UI |
| **`agent_ui`** | **156** | **119** | Agent Panel, thread view, message editor, threads sidebar/archive |
| `zed` (whole app) | 235 | 195 | |

Key takeaways:
- **None of these crates are published to crates.io** (they are `publish = false` at the workspace level, version 0.1.0). You either depend on them by git (`zed = { git, rev }` per crate, all at the same rev) or vendor them.
- **The smallest viable set** for "fast editor + LSP go-to-def" is `editor` + `project` + `language` + `lsp` (+ `workspace` in practice). That is about 100 internal crates plus Zed's pinned forks of `tree-sitter`, `lsp-types` and `alacritty_terminal`. That is most of Zed's build anyway, which is why option (a) wins.
- **The editor is not separable cheaply.** `editor` requires `project` (and through it `settings`, `theme`, `fs`, `client` traits, etc.).
- **License:** everything above `gpui` and `gpui_platform` is GPL-3.0-or-later (no AGPL in this closure; Zed's AGPL is limited to collab server crates). Linking any of these crates makes the combined binary GPL-3.0.

---

## 4. Deep dive: Zed's agent chat UI (primary composer candidate)

### 4.1 Crates that make up the Agent Panel and thread view
- **`acp_thread`** (~35k LOC): the protocol-agnostic thread model. `AgentThreadEntry` = `UserMessage | AssistantMessage | SessionMessage | ToolCall | Elicitation | ContextCompaction`. `ToolCallContent` = `ContentBlock | Diff(Entity<Diff>) | LegacyDiff | Terminal`. `ToolCallStatus` includes `WaitingForConfirmation`, `Pending`, `InProgress`, `Paused`, `Completed`, `Failed`, `Cancelled`. It also covers `mention.rs` (@-mentions), `subagents.rs`, `compaction.rs`, `elicitation.rs` and `terminal.rs`, and it speaks ACP v1 and the draft v2.
- **`agent_servers`**: launches external ACP agents (`custom.rs`: `CLAUDE_AGENT_ID = "claude-acp"`, `CODEX_ID`, `GEMINI_ID`), runs them as registry agents (npx/bun-packaged `@agentclientprotocol/claude-agent-acp`), and handles auth, proxies and version updates.
- **`agent_ui`** (~103k LOC): `agent_panel.rs` (15k), `conversation_view.rs` (21k), `conversation_view/thread_view.rs` (16k), `message_editor.rs` (6k), `completion_provider.rs` (@-mention and slash completion), `agent_diff.rs` (review the agent's edits across buffers), `threads_archive_view.rs`, `thread_worktree_archive.rs` (persists WIP to a git ref and then runs `git worktree remove`), `thread_metadata_store.rs`, plus model/mode/profile selectors.
- **Supporting crates:** `agent_settings`, `action_log` (tracks agent edits for accept/reject), `buffer_diff`, `streaming_diff`, `markdown` (assistant text), `editor` (**the message input is a full Zed `Editor`**), `ui`, `theme`, `workspace`, `project`, `terminal_view` (live terminal tool calls).

### 4.2 How it talks to Claude Code
Zed (ACP client, `agent-client-protocol =5.3.0` with `unstable` + `unstable_protocol_v2`) spawns `claude-agent-acp`, a Node/Bun process. That process embeds `@anthropic-ai/claude-agent-sdk` 0.3.323, which runs the `claude` CLI. JSON-RPC runs over stdio. Zed sets `ANTHROPIC_API_KEY=""` so that the subscription login is used. Permission requests arrive as ACP `session/request_permission` and render inline as permission cards with allow-once/always/pattern dropdowns. File edits arrive as ACP diffs, and the adapter offers compact diff patches and "edit review".

### 4.3 What it renders (from `thread_view.rs`, 92 `render_*` fns)
Tool calls (`render_any_tool_call`, collapsible commands, numbered read-file output), **diffs as live mini editors** (`render_diff_editor`), **plans/TODOs** (`render_plan_entries`/`render_plan_summary`), **thinking blocks** (`render_thinking_block`, effort and thinking controls), **permission prompts** (`render_permission_buttons*`, granularity dropdowns, sandbox authorization), **images** (`render_image_output`), **subagents** (`render_subagent_card`/`expanded_content`), **terminal tool calls** (embedded live terminal), elicitations (MCP forms), a **message queue and steering** (`render_message_queue_entries`, `render_queue_steer_button`), context compaction, an edited-files summary with accept/reject, and error callouts (refusal, prompt too large, auth). This is far richer than what any web chat UI in this space offers.

### 4.4 Options

| | (a) Zed fork, stripped down | (b) Extract `agent_ui` + deps into own GPUI app | (c) Own composer on gpui-kit, Zed as reference |
|---|---|---|---|
| Effort to first usable version | **Low**: days to change layout and defaults and hide features. Parallel agents and worktrees already exist. | **High**: 156 crates, global singletons (`Settings`, `ThemeSettings`, `AppState`, `Workspace`, `Project`, `client`), plus feature flags. In practice it is "Zed minus some crates", with your own glue code that breaks on every rebase. | **Medium–high**: ACP client and thread model are about 2–4k LOC. Matching Zed's tool-call, diff, plan and permission UX is months. |
| Perf | Best available (Zed's own). The slow-start problem lives in the agent process, not in the UI. | Same as (a) | Can be as fast or faster. You control everything, with a smaller binary and faster cold start. |
| License | GPL-3.0 for the whole app | GPL-3.0 for the whole app | **Permissive** (Apache/MIT) if you read but don't copy Zed code |
| Update churn | Rebase onto Zed main weekly. Keep changes additive (new crates plus small hooks) to minimize conflicts. You get new ACP features for free. | **Worst**: internal APIs change daily, and you'd also be reimplementing Workspace integration. | Only GPUI/gpui-kit churn and ACP spec churn. Moderate. |
| Editor/LSP go-to-def | Free (Zed's) | Must also pull in `editor`/`project` (+100 crates) | gpui-kit's code editor has LSP diagnostics, completion and hover. Go-to-def, multi-file navigation and the project model are **your own work**. This is the big hidden cost. |

**Recommendation: (a), a soft fork of Zed** (Icarus = Zed + `icarus_*` crates + a small patch set). The reasons:
- The product's two hard parts, a fast editor with LSP navigation and a rich agent thread UI, are exactly the two things Zed has spent years on. Both are GPL and both are entangled with `project`/`workspace`.
- Zed upstream is moving in the same direction (Parallel Agents, worktree threads, threads sidebar), so a fork keeps gaining features instead of drifting.
- Keep the patch set additive. New crates (`orca_worktrees`, `orca_pr_review`, `orca_layout`) register panels and actions through `workspace` hooks, and the changes to Zed files are a small list (default layout, branding, hidden UI). Rebase weekly. Don't patch `editor` or `acp_thread` internals.
- License: fine for personal use. If you ever distribute, the app is GPL-3.0 (publish the source). That's acceptable for a free tool and blocks a closed-source product.

Pick (c) only if a permissive license or a tiny app is a hard requirement. In that case: gpui-kit + `agent-client-protocol` + your own `Thread` model modeled on `acp_thread`, with gpui-kit's markdown, virtual list and code editor. Read Zed's `thread_view.rs` for UX but don't copy from it. Don't pick (b).

---

## 5. Agent integration (Claude Code)

| Path | Session start | UI richness | Notes |
|---|---|---|---|
| **ACP via `claude-agent-acp`** (v0.107.0, 2026-11-13, Apache-2.0, [repo](https://github.com/agentclientprotocol/claude-agent-acp); renamed from `zed-industries/claude-code-acp`) | Node/Bun adapter boot plus SDK plus `claude` CLI. Standalone executables per release, with no Node needed, which avoids `npx` resolution: use those. | **Richest and standardized**: tool calls, diffs, plans/TODOs, thinking, permissions with presentation extensions, images, @-mentions, slash commands, subagent transcripts, terminals, MCP, resume, and the ACP v2 draft | Any ACP client UI gets Codex, Gemini and OpenCode for free. Rust crate `agent-client-protocol` 5.3.0 ([rust-sdk](https://github.com/agentclientprotocol/rust-sdk), Apache-2.0, 2026-11-14). |
| **Direct `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages`** | Fastest: one process, no adapter. Add `--bare` to skip discovery of hooks, plugins, MCP and CLAUDE.md (this needs `ANTHROPIC_API_KEY`, not the subscription login). | You parse Anthropic message events yourself: `stream_event` deltas, `tool_use`/`tool_result`, `system/init`, `system/api_retry`, and `permission_denied`. Interactive permission approval requires the SDK control protocol (`canUseTool`) or `--permission-prompt-tool` (an MCP tool you host). Resume works with `--resume <id or .jsonl path>`. | The control protocol is undocumented for third parties (it's what the TS/Python SDKs speak), so you'd be tracking an internal API. |
| Claude Agent SDK (TS [claude-agent-sdk-typescript](https://github.com/anthropics/claude-agent-sdk-typescript) / Python [claude-agent-sdk-python](https://github.com/anthropics/claude-agent-sdk-python), MIT) | Same as the CLI plus the host runtime | Full: `canUseTool`, hooks, `interrupt()`, streaming input, images | There is no official Rust SDK (`claude-agent-sdk` 0.1.1 on crates.io is from 2025-09 and its repo is gone). claude-agent-acp *is* the SDK wrapped in a standard protocol. |

**Session start speed:** most of the "slow Claude session start" comes from the agent process, not the UI. Ways to reduce it:
1. Use the claude-agent-acp **standalone binary** (not `npx`).
2. **Pre-spawn and keep a warm agent process per worktree** (start it when the worktree is opened, before the first prompt). ACP allows several sessions per process (`session/new`, `session/load`).
3. Avoid MCP servers blocking the first turn (`MCP_TIMEOUT`, cached tool lists).
4. Consider `--bare`-like settings when no CLAUDE.md or hooks are needed.

Measure the time from `initialize` to `session/new` and the time to first token.

**Other Claude chat UIs (what's reusable):**
- opcode/Claudia (Tauri + React, **AGPL-3.0**): UX ideas only.
- Crystal, now Nimbalyst (Electron, MIT): worktree-per-session flow ideas.
- Sculptor (MIT, Python/TS, containers): ideas only.
- Conductor and Superset (closed, macOS): UX benchmarks.
- ParadevAI/Paradev (MIT, ★16k): an orchestration UX reference.

None of them provide Rust code worth reusing. Zed's agent UI is the only native one that is relevant.

---

## 6. Per-component recommendation table

| Component | Pick | Alternatives | License | Maturity / version | Reuse mode |
|---|---|---|---|---|---|
| UI framework | **GPUI** (in-tree via Zed fork; or `gpui-pre` 0.3.12 via gpui-kit) | gpui git dep at a pinned rev; crates.io `gpui` 0.2.2 (stale) | Apache-2.0 | pre-1.0, heavy churn | depend |
| Widgets | Zed `ui` (fork) / **gpui-kit 0.9.1** (option c) | gpuikit, guise-ui | GPL-3.0 / Apache-2.0 | gpui-kit: weekly, production use | depend |
| Agent composer | **Zed `agent_ui` + `acp_thread`** (fork) | own composer on gpui-kit with Zed as reference | GPL-3.0 | mature, very active | fork (a) / reference-only (c) |
| Agent protocol | **ACP**, `agent-client-protocol` 5.3.0 | raw stream-json | Apache-2.0 | stable v1 + v2 draft | depend |
| Claude adapter | **`claude-agent-acp`** standalone binary (v0.107.0) | Agent SDK directly; `claude -p` stream-json | Apache-2.0 (SDK: Anthropic terms) | ships several releases a week | depend (bundle binary) |
| Editor + go-to-def | **Zed `editor`/`project`/`language`/`lsp`** (fork) | gpui-kit code editor + own LSP client | GPL-3.0 / Apache-2.0 | Zed: mature; gpui-kit: decent single-file | fork / depend |
| LSP client (option c) | **`async-lsp` 0.2.4** + `lsp-types` 0.97 (or `ls-types`/`gen-lsp-types` for newer spec) | Zed `lsp` crate (GPL); hand-rolled JSON-RPC | MIT/Apache | `lsp-types` last released 2024-06; Zed uses a fork | depend |
| Syntax highlighting | **tree-sitter 0.27.2** + `tree-sitter-highlight` | syntect | MIT | mature | depend |
| Git: worktrees and mutations | **shell out to `git`** (like Zed's `git` crate: `git worktree add/remove/list --porcelain`) | git2 0.21 (worktree API exists, libgit2 lags); gix 0.90 (worktree add not supported) | (git CLI) | most robust and matches user config/hooks | depend on CLI |
| Git: fast reads (status, diff, blame, log) | **gix 0.90** (+ `gix-diff` 0.70) | git2 0.21; CLI | MIT/Apache | very active, monthly | depend |
| Syntax-aware diff | **difftastic 0.72** as a CLI (`DFT_DISPLAY=json`) | Zed `buffer_diff` (line diff with word highlight); `similar` crate | MIT | mature (its library API is not stable, use the CLI) | depend (CLI) |
| Terminal fallback | **`alacritty_terminal` 0.26** (or Zed's fork at a pinned rev) + Zed `terminal_view` in a fork | wezterm `termwiz` 0.23 (2025-03, slower releases) | Apache-2.0 / MIT | mature | depend |
| GitHub API | **octocrab 0.54.9** (REST) + `graphql_client` 0.16 for PR review threads (GraphQL only) | `gh` CLI shell-out (fastest to build, reuses auth) | MIT/Apache | active | depend |
| Fuzzy finder | **nucleo 0.5** / `nucleo-matcher` (Zed itself now has `fuzzy_nucleo`) | frizbee 0.13 (SIMD), Zed `fuzzy` | MPL-2.0 (file-level copyleft only, fine) | stable, slow releases | depend |
| File watching | **notify 9.0.0-rc.9** (or 8.x stable) + `notify-debouncer-full` | Zed `fs` crate (FSEvents) | CC0 / MIT/Apache | mature | depend |
| Persistence | SQLite (`rusqlite`) / Zed `db` (fork) | sled, redb | MIT | mature | depend |
| Shell to fork from (option c) | **penso/arbor** (MIT): worktrees, terminals, diffs in GPUI | diri (AGPL, reference only) | MIT | active | fork or reference |

---

## 7. Concrete recommended stack

**Primary (option a, "Zed soft fork"):**
- Base: a `zed-industries/zed` fork, tracking `main`, rebased weekly, with an `orca/*` patch-set branch.
- Agent: built-in ACP plus the bundled `claude-agent-acp` standalone binary. Add a warm-spawn change: start the agent server when a worktree is opened, and call `session/new` ahead of time.
- Worktrees: build on Zed's existing parallel-agents worktree support. Add an `orca_worktrees` crate for an Orca-style sidebar (worktree list, status, create/delete/archive, `git worktree` via CLI).
- Editor, LSP, git panel and diff: stock Zed.
- PR review: a new `orca_pr` crate using octocrab plus GraphQL (review threads, comments, suggestions), rendered with Zed's `editor` diff and multibuffer (like the project diff view), with inline comment blocks.
- Defaults: "Agentic" layout, Claude as the default agent, and telemetry and collab features turned off.

**Alternative (option c, permissive, from scratch):** `gpui-kit` 0.9.1 (pins `gpui-pre` 0.3.12) + `agent-client-protocol` 5.3.0 + `claude-agent-acp` binary + gix/CLI git + `async-lsp`/`lsp-types` + tree-sitter + octocrab + nucleo + notify + alacritty_terminal. Optionally start from **arbor** (MIT) for the worktree, terminal and diff shell.

---

## 8. Licensing implications

| Scenario | Option (a)/(b) (Zed GPL crates) | Option (c) (GPUI + permissive crates) |
|---|---|---|
| Personal use, never distributed | No obligations. GPL only applies on distribution. | None |
| Distribute binaries (even free) | The **whole app must be GPL-3.0-or-later** and you must provide the complete corresponding source. Keep Zed's notices. Don't use Zed trademarks/branding. | Apache-2.0/MIT notices (NOTICE files). MPL-2.0 (nucleo): modifications to its files stay MPL. |
| Closed-source/commercial product | Not possible | Possible |
| Copying Zed code into a permissive app | Makes the app GPL. Read for ideas, but don't copy code. | — |
| Claude Agent SDK / Claude Code | Governed by Anthropic's terms, not OSS terms. Bundling the `claude` CLI inside the claude-agent-acp binary is what the adapter already does. Check the terms before redistributing. Users authenticate with their own subscription or key. | same |
| AGPL references (diri, opcode) | Read only. Don't copy code. | same |

---

## 9. Risks

1. **GPUI API churn** (high): it is pre-1.0 with weekly breaking changes, the official crates.io release is a year stale, and `gpui-pre` is a third-party snapshot pin. For (a) the churn arrives through rebases. For (c), pin exact versions and upgrade in lockstep with gpui-kit.
2. **Fork maintenance** (option a, medium): Zed merges about 100+ PRs a week. Keep patches additive and rebase often, or conflicts in `agent_ui`/`workspace` will pile up. Build time is long: a cold release build of Zed takes tens of minutes and needs about 20+ GB of disk.
3. **Upstream overlap** (strategic): Zed is actively building the same product (Parallel Agents, worktree threads). That's good for a fork, but it makes a separate app less valuable. Re-evaluate whether stock Zed is enough.
4. **Agent start latency is not a UI problem**: the Node/Bun adapter, the SDK, the `claude` CLI and MCP startup dominate. A Rust UI alone won't fix "slow Claude session start". You also need warm processes and the standalone binary, and you should measure.
5. **Adapter and protocol churn**: claude-agent-acp releases several times a week, and ACP v2 is a draft. Pin adapter versions, and smoke-test the handshake the way Zed's `claude_adapter_initialize_smoke` test does.
6. **Raw stream-json is an internal-ish contract**: if you bypass ACP, permission handling depends on the SDK control protocol, which isn't a documented public API.
7. **macOS-only bits**: `gpui_macos` (Metal, CoreText via font-kit), FSEvents, keychain, notarization and signing. Linux and Windows backends exist but are less polished. diri needed a vendored `gpui_macos` patch for a release-profile linker issue.
8. **Licensing lock-in**: choosing (a) makes the app GPL forever, and going back to permissive means a rewrite.
9. **LSP go-to-def in option (c)**: gpui-kit's editor is single-buffer oriented. Cross-file navigation, a project-wide symbol index and multi-root LSP management are your own work, and they are exactly where Monaco/Orca is slow today.

---

## 10. Links

- Zed: https://github.com/zed-industries/zed (checked at `596dfdc`, 2026-11-15) · Parallel Agents blog https://zed.dev/blog/parallel-agents · docs https://zed.dev/docs/ai/parallel-agents
- GPUI README: https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md · crates.io `gpui` https://crates.io/crates/gpui · `gpui-pre` https://crates.io/crates/gpui-pre · `gpui-fast` https://github.com/longbridge/gpui-fast
- gpui-kit (gpui-component): https://github.com/longbridge/gpui-kit · https://gpui-kit.com
- ACP: https://agentclientprotocol.com · https://github.com/agentclientprotocol/agent-client-protocol · Rust SDK https://github.com/agentclientprotocol/rust-sdk
- Claude ACP adapter: https://github.com/agentclientprotocol/claude-agent-acp (npm `@agentclientprotocol/claude-agent-acp`)
- Claude Code headless/stream-json: https://code.claude.com/docs/en/headless · Agent SDK: https://code.claude.com/docs/en/agent-sdk/overview
- GPUI apps: https://github.com/penso/arbor · https://github.com/cristicretu/diri
- Others: https://github.com/anomalyco/opencode · https://github.com/open-webui/open-webui · https://github.com/winfunc/opcode · https://github.com/stravu/crystal · https://github.com/imbue-ai/sculptor · https://github.com/ParadevAI/Paradev
- Rust crates: gix https://github.com/GitoxideLabs/gitoxide · git2 https://github.com/rust-lang/git2-rs · async-lsp https://github.com/oxalica/async-lsp · lsp-types https://github.com/gluon-lang/lsp-types · tree-sitter https://github.com/tree-sitter/tree-sitter · alacritty_terminal https://github.com/alacritty/alacritty · octocrab https://github.com/XAMPPRocky/octocrab · nucleo https://github.com/helix-editor/nucleo · notify https://github.com/notify-rs/notify · difftastic https://github.com/Wilfred/difftastic
