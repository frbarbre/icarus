# Progress

Status of the build against [plan.md](plan.md). Updated as milestones land.

## Done

**Server** (`crates/icarus-server`, binary `icarus-server`):
- Local socket server: length-prefixed JSON frames, token auth, one server per state directory, a protocol version in the hello.
- Terminals:
  - PTYs owned by the server, with a headless `alacritty_terminal` emulator per session.
  - Rendered-screen and scrollback reads; snapshots replay as VT sequences.
  - Reattach resumes by sequence number, or falls back to a snapshot.
  - Terminal queries (DSR, DA) are answered by the server.
- Agents:
  - The user's own unmodified `claude` binary runs in stream-JSON mode (D19). Its output is normalized into events, kept in memory and in `threads/<id>.jsonl`.
  - Clients attach, detach and resume by sequence number while the agent keeps working.
  - Usage meters come from Claude Code's own `rate_limit_event`; no credentials are read.
  - Sessions survive a server restart: they come back asleep and resume the provider session (`--resume`) on the next message. Shutting the server down puts agents to sleep instead of killing them.
  - Interrupt, sleep and kill are supported.
- Projects and workspaces (SQLite `state.db`):
  - Adding a project detects its root, canonical remote and default base.
  - Creating a workspace makes a git worktree under the Icarus workspace root with a DNS-safe slug (D3), and starts its agent at once ("always an agent").
  - A workspace can be set (status, title, base, issue, PR), shows the Changes list, and can be removed (refused when dirty unless forced; its agents and terminals end with it).
- A fake stream-JSON agent (`icarus-fake-agent`) lets tests run without Claude or the network.
- **Codex provider** (`--provider codex`, or `agents.default: "codex"`):
  - Drives the user's own `codex app-server` over JSON-RPC on stdio, as Orca's native chat does: `initialize`, `thread/start` or `thread/resume`, `turn/start`, `turn/interrupt`.
  - Notifications become the same events as Claude's (text and reasoning deltas, `Bash`/`apply_patch`/MCP tool calls and results, plans, turn end, token usage, rate limits), so `icarus acp` and transcripts render both alike.
  - Bypass maps to Orca's Full access (`never` + `danger-full-access`); otherwise Orca's Ask (`on-request` + read-only), with approval requests declined.
  - Sleep, wake and restart restore resume the Codex thread id. Auth is the user's own `codex login`; Icarus never reads its tokens.
  - The binary comes from `agents.commands.codex`, else `ICARUS_CODEX_BIN`, else `codex` on `PATH`. A fake app-server (`icarus-fake-codex`) backs the end-to-end tests.
  - Not yet checked against a live Codex: that needs `codex` installed and `codex login`.

**Git** (`crates/icarus-git`):
- Worktree list, create and remove; repo info; canonical remotes.
- Orca's base-ref resolution, which never guesses a base.
- The single Changes list: merge base vs working tree, committed + staged + unstaged + untracked merged into one row per file, exact line counts, and an uncommitted-only scope.

**Settings** (`crates/icarus-settings`):
- Lossless `orca-settings` v1 parse and byte-exact export.
- Layered Icarus settings with `explain` and a schema.
- The Orca importer (D4, D18).

**CLI** (`crates/icarus-cli`, binary `icarus`):
- Commands: `server start|stop|status`, `terminal …`, `agent …`, `project …`, `workspace …`, all with `--json`.
- **`icarus acp`:** an Agent Client Protocol agent on stdio, backed by server-owned sessions. It gives Zed's agent UI on top of Icarus with no Zed patch.
  - On `session/new` it reuses the workspace's warm agent.
  - `session/load` replays history, and `session/list` enables Zed's thread import.
  - Every update is validated against the official ACP schema in tests.

**More server and CLI work:**
- `icarus-server proxy` bridges stdio to the machine's server and starts it if needed. Remote clients run it over SSH (`Client::connect_ssh`); SSH does the auth, and the proxy presents the local token.
- Settings over RPC and the CLI:
  - `icarus config get|set|unset|explain|schema` (layer: user, project or local)
  - `icarus settings import|export`: Orca v1. Project settings go to the untracked `icarus.local.json` of the project whose remote matches.

**Rules** (`crates/icarus-rules`, 153 tests):
- A pure engine with user-defined statuses, first-match groups, triggers, actions and guards.
- `orca_preset` builds the engine config from Orca's `workspaceStatusRules`.
- `next_action` implements the next-action button (§7d), plus head sync and CI-triage decisions.
- 19 of Orca's test files are ported.

**Client** (Zed fork, crate `icarus_ui`):
- A workspaces panel grouped by the user's status columns, with cards (issue id, title, branch, agent state).
- A status menu on each card that sets the status through the server.
- Clicking a card opens its worktree.
- Icarus is registered as a default external agent (`agent_servers.Icarus` → `icarus acp`).
- An offscreen visual test (`ICARUS_VISUAL_ONLY=1 cargo run -p zed --bin zed-visual-test-runner --features visual-tests`) renders the panel.

**GitHub** (`crates/icarus-github`, 57 tests):
- All calls go through the user's `gh` login.
- Queries: batched PR snapshot, review inbox, team roster, merge metadata, native stacks (API version `2026-03-10`).
- Mutations: merge, auto-merge, merge queue, stack merge, reviews, threads, reactions, check reruns, attachments.
- Rate-limit budget and ETag caching.
- Checked read-only against a real repository: 20 PRs across every state, the review inbox, merge-queue detection and two real stacks.

**Open in editor:**
- `icarus file open <path>[:line[:col]]` and `scripts/icarus-open` (a `LAUNCH_EDITOR` target, D27).
- Verified with the real Zed build: the file opens in the window that owns the worktree, at the position.

**Client additions:**
- The title-bar next-action button (`icarus::RunNextAction`, ⌘⌥Enter, Ctrl+Alt+Enter on Linux), driven by `workspace.next_action` / `workspace.run_next_action`.
- The agent panel is docked at the bottom, with Icarus as the default agent.
- Light- and dark-mode visual baselines.

**Automation, wired end to end** (301 tests):
- `pr.status`, `workspace.next_action`, `workspace.run_next_action`, `rules.status` and `rules.tick`, plus the matching CLI commands.
- The automation loop builds the engine config from imported Orca rules and executes the planned actions:
  - status moves and deletes
  - review workspaces with an agent
  - commands, head sync, notifications
  - PR operations and Icarus-managed stack auto-merge
- It stays in dry-run until `rules.armed=true`.
- Checked against live GitHub with the reference setup (read-only, dry run):
  - The tick planned exactly the open review requests, each with the user's own review prompt.
  - The next action came back as "Add to merge queue" for an approved PR in a merge-queue repo, "Ready for review" for a draft, and "Resolve conflicts" for a conflicting PR.

**Source control** (319 tests):
- `workspace.changes` accepts the scopes base, uncommitted, `turn:<…>` and `commit:<sha>`. Configurable facet filters apply in every scope.
- A per-workspace hidden-filter state survives server restarts.
- `changes.explain` reports why a file got its type.
- The generic `type` preset; project areas and lint baselines come from project settings (`changes.facets`).
- "Imports only" uses Orca's semantics by default.
- Agent turn snapshots: `refs/worktree/icarus-turns/*`. The real index is untouched, empty turns are skipped, and the newest 50 are kept.
- CLI: `icarus changes list|explain|filter` and `icarus workspace turns`.

**GitHub panel in the client:**
- PR header with state or Queued pill, refresh, open on GitHub.
- Stack map with Orca's 14-rule badges, the current PR highlighted, and the out-of-date warning.
- Merge queue position and ETA, needs attention, checks (failing first, with durations), reviewers.
- Empty states. Visual tests in dark and light.

**Attachments** (§3-0):
- The server stores attachments per workspace and builds them from PR threads (with unresolved and humans-only filters), failed checks (with a CI log tail), file ranges, text and turn diffs.
- `composer.attach` reaches only the app window. The Zed fork renders each attachment as a composer chip (`icarus://attachment/<id>`) and expands it into the prompt on send.
- Checked live against the real build; the first run caught and fixed a double-lease crash.

**Ports and previews** (365 tests):
- `ports.list` / `ports.primary`: listeners attributed to workspaces, ~40 command patterns plus an HTTP probe, the labels command and rules.
- `pr.previews`: GitHub Deployments first, then provider statuses, then comments. Live check: both previews on a real PR, with frontend as primary.

**Avatars (server):** PR, review, requested-reviewer and stack-entry facts carry real GitHub avatar URLs (`avatarUrl(size: 64)`). `workspace.list` / `workspace.info` add `pr_author {login, avatar_url}` from the PR cache only, so listing never calls GitHub. Live read-only check returned real URLs for users and bots.

**Links, ports and previews in the client:** `icarus::OpenFrontend` (⌘⇧B), `icarus::OpenPreview` (⌘⇧P), `OpenPullRequest`, `CopyPullRequestUrl`; title-bar frontend and preview buttons (picker for several previews, "behind head" warning); port chips on cards, kept live by `ports.changed`; a Previews section in the GitHub panel; real avatars on cards, the panel header, reviewers and stack rows. Icarus workspace folders are trusted automatically. `open_layout` focuses the workspace's agent and opens the PR in the system browser until the embedded tab lands.

**Layouts (server and CLI, D32):** named tab layouts from settings (`layouts.<name>`) plus a built-in `review` (agent left, browser on `{pr_url}` right), with types shared through `icarus-protocol::layout`. `icarus workspace create --layout`, `icarus workspace layout <ws> <name>`, `icarus layout list|show`, and the rules' create-from-PR and open-layout actions resolve placeholders, bind agent tabs to server agents (prompted tabs start their own agent), store the layout as pending and push it to the app; `workspace.take_layout` applies it once. The app side waits for the agent, browser, diff and explorer tab types.

**Live app capture:** `icarus app capture [--out DIR]` asks the running app to draw and save each window as a PNG (GPUI `frame-capture` feature), so the real app can be checked end to end without screen-recording permission or a visible GUI session.

**Statuses in the client:** Orca's 11 colours (with separate light and dark tones) and 16 icons. Color and Icon submenus save to settings, and only statuses that currently have workspaces show in the sidebar.

**Client feasibility:** the Zed fork builds on macOS without Xcode (runtime shaders). With Xcode, Metal shaders precompile too (`DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`).

**CI:** fail-fast phases plus sharded nextest (`.github/workflows/ci.yml`).

## Blocked on you

Parked until you're back; work continues on other items.
- **Codex:** deferred by you. The provider is nearly complete but untested; a live check needs Codex installed and `codex login` on the build machine.
- **Linear titles and hover cards:** need a Linear API key (or the Linear MCP authorized in this session).
- **Zed fork's GitHub home:** fork repo or submodule; it lives only on the build machine for now.
- **⌘⇧P:** taken by "Open preview"; the command palette moved to F1. Say if you want it back.

## In progress

- Rules engine (`crates/icarus-rules`): Orca's status automations as a general engine, with Orca's tests ported.
- Client: Icarus crates inside the Zed fork (workspace sidebar, status menu, docked composer), verified with offscreen visual tests.

## Next

- Server: GitHub provider (PR facts, reviews, checks, stacks, merge queue), the rules engine wired to it, CI triage, the review inbox.
- Server: `launchd`/`systemd` service install, and `serve --stdio` / `--listen` for remote servers.
- Client: composer with attachments (§3-0), next-action button (§7d), GitHub panel and review, the source control panel with filters, the browser tab.
