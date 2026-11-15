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
  - Checked live against codex-cli 0.169 with a ChatGPT login (ignored test `live_codex`): streaming, sleep and `thread/resume`, and replay through `icarus acp`. Tool calls, approvals and interrupt are checked only against the fake and Codex's own generated schema.

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

**Links, ports and previews in the client:** `icarus::OpenFrontend` (⌘⇧B), `icarus::OpenPreview` (⌘⌥P, Ctrl+Alt+P on Linux; ⌘⇧P stays the command palette), `OpenPullRequest`, `CopyPullRequestUrl`; title-bar frontend and preview buttons (picker for several previews, "behind head" warning); port chips on cards, kept live by `ports.changed`; a Previews section in the GitHub panel; real avatars on cards, the panel header, reviewers and stack rows. Icarus workspace folders are trusted automatically. `open_layout` focuses the workspace's agent and opens the PR in the system browser until the embedded tab lands.

**Layouts (server and CLI, D32):** named tab layouts from settings (`layouts.<name>`) plus a built-in `review` (agent left, browser on `{pr_url}` right), with types shared through `icarus-protocol::layout`. `icarus workspace create --layout`, `icarus workspace layout <ws> <name>`, `icarus layout list|show`, and the rules' create-from-PR and open-layout actions resolve placeholders, bind agent tabs to server agents (prompted tabs start their own agent), store the layout as pending and push it to the app; `workspace.take_layout` applies it once. The app side waits for the agent, browser, diff and explorer tab types.

**The app lives in `app/`:** a soft fork of Zed, kept as plain files in this repo (not a GitHub fork). It's based on upstream `zed-industries/zed` commit `596dfdcf32d85d4d8d17a8887b4d0979ec7c5e14` (2026-11-15). To merge upstream later, diff `app/` against that commit and apply the result to a newer one. It has its own Cargo workspace and toolchain, so the root workspace and CI's fast checks skip it. Build it with `cd app && cargo build -p zed` (on macOS with `DEVELOPER_DIR` set to Xcode).

**Agent tabs and the shared composer (D31):** each agent is a centre tab (`agent_ui::agent_tab`) showing only the conversation; one composer under the centre panes types into the target agent's own draft (the active agent tab, else the last one used), so drafts and attachment chips stay per agent. Closing the last agent tab opens a fresh one. ⌘⌥T opens a new agent tab; attachments, the review layout and the next-action button all target it. Checked in the live app.

**Live app capture:** `icarus app capture [--out DIR]` asks the running app to draw and save each window as a PNG (GPUI `frame-capture` feature), so the real app can be checked end to end without screen-recording permission or a visible GUI session.

**Statuses in the client:** Orca's 11 colours (with separate light and dark tones) and 16 icons. Color and Icon submenus save to settings, and only statuses that currently have workspaces show in the sidebar.

**Client feasibility:** the Zed fork builds on macOS without Xcode (runtime shaders). With Xcode, Metal shaders precompile too (`DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`).

**CI:** fail-fast phases plus sharded nextest (`.github/workflows/ci.yml`).

**Remote servers:**
- `icarus-server serve --listen <ip:port>` (repeatable, or the `server.listen` setting) accepts clients over TLS next to the local socket. The certificate is self-signed and stays in the server's state directory.
- It only binds loopback, private and Tailscale addresses unless `--allow-public` is given, and the error says why.
- `icarus server pair` prints a one-time code: 8 characters, single use, valid 10 minutes. Failed attempts are rate-limited per address. A client trades the code for its own token, which is stored hashed in `state.db`.
- `icarus server clients` lists paired clients, and `icarus server revoke <id>` drops one and closes its live connections. These, and `pair`, only work on the server's own machine.
- `icarus remote add <name> <addr> --code <code>` pairs, pins the certificate fingerprint and saves the server in `~/.config/icarus/servers.json`. `ssh://host` servers can be saved too. `remote list|remove` manage the list, and `--server <name>` points any command at a saved server.
- The local socket and its token work as before, and the protocol only gained optional fields.

**Linear** (`crates/icarus-linear`, plan D12 and D3):
- A small read-only client for Linear's GraphQL API. Several issues come back in one query, rate limits pause requests until Linear's reset time, and a rejected key is reported clearly.
- The key comes from `LINEAR_API_KEY`, else from the `linear.api_key_command` user setting (for example a `bw get …` command). The command runs once and its output is kept in memory only; it runs again if Linear rejects the key. The key is never logged or written to disk.
- Workspace cards get the issue's title, state, labels, assignee and description from a cache, so listing never waits on Linear. The server refreshes linked issues every 5 minutes (`linear.poll_interval_ms`) and sends `workspace.changed` when a card changed.
- `icarus workspace create --from-linear ABC-12` names the workspace after the issue and uses Linear's branch name. `icarus issue show|open|refresh|status` look issues up.
- Without a key everything still works: cards keep the stored issue id and title.
- The read-only live test (`crates/icarus-linear/tests/live.rs`, key from `ICARUS_LINEAR_LIVE_KEY_COMMAND`) passes against the real API: viewer, team, recent issues by identifier, and not-found handling.

**Notifications (server, D7):**
- The server makes notifications and keeps them in `state.db` (the newest 500), so none are lost while no client is connected. Each has an id, kind, title, body, workspace, time and a read flag.
- Kinds: an agent finished its turn, waits for input or a permission, or failed (a failed turn, or a process that died on its own); CI failed, a review was requested, a PR merged (from the rules' GitHub poll, dry runs included); and the rules' `notify` action.
- Every kind can be switched off with `notifications.<kind>`, plus `notifications.enabled` and `notifications.sound`. All default to on.
- Repeats are dropped: agent news within 5 seconds (Orca's cooldown), a rule's message within 10 minutes, and GitHub news once per state (the same PR head, the same merge).
- Clients get a `notification` event (with `sound`) and `notifications.changed` with the unread count. RPCs: `notification.list|read|clear|kinds`. CLI: `icarus notifications [--unread] [--json]`, `icarus notifications read <id>|--all`, `icarus notifications clear`.

**Session history and sleep/wake (server, D8):**
- `agent.history` lists past sessions (sleeping, exited or killed, with at least one turn): title, provider, workspace, account, first prompt, last activity, turns and whether they can be resumed. It filters by workspace, pages with `before`, and searches what you asked and what the agent answered (every word must appear; a snippet shows where). The index is in memory and reads only the new part of each log.
- `agent.resume` brings a past session back as a live agent in its own directory, through the provider's own resume. Killed sessions no longer come back by themselves after a server restart; they stay in history until resumed.
- `workspace.sleep` puts the workspace's agents to sleep and stops its terminals' processes; screens and scrollback stay. `workspace.wake` restarts exactly what was parked. Cards show `sleeping`.
- CLI: `icarus agent history [--workspace] [--search]`, `icarus agent resume <id>`, `icarus workspace sleep|wake <id>`.

**Multiple accounts (server, D13, §7c):**
- Each managed account is its own config dir, as in Orca: `<state>/claude-profiles/<id>/home` (`CLAUDE_CONFIG_DIR`) or `<state>/codex-accounts/<id>/home` (`CODEX_HOME`).
- `account.add` makes the directory and returns the command you run to sign in through the provider's own login, e.g. `CLAUDE_CONFIG_DIR=… claude /login`. Icarus never reads, copies or stores credentials. It reads only the signed-in email from Claude Code's `.claude.json`.
- `account.select` sets the default per provider (`system` means the provider's own login). `agent.create` and `workspace.create` take an `account` override. Launches on a managed Claude account drop `ANTHROPIC_API_KEY` and the other auth variables, as Orca does.
- `account.list` shows each account with its newest usage meters, taken from the rate-limit events of agents that ran on it.
- CLI: `icarus account list|add|remove|use`.

**Review mode (server)** (github-review.md, D23):
- The server keeps a reviewer's unsent review per workspace and pull request in `state.db`: draft comments (path, line or range, side, markdown body, file comments), the summary, the verdict, and the chosen diff base. Drafts are keyed by PR, never by commit, so they survive server restarts and pushes. A file comment keeps its kind on disk (the fork lost it).
- A draft on lines outside the PR's diff becomes a file comment that quotes the code, as in the fork; a file the PR does not touch is refused with a reason. Up to 200 drafts are kept.
- `review.submit` sends one GitHub review with every draft (GraphQL variables, not a string-built query). On success the drafts and summary clear; on failure they stay and GitHub's message comes back word for word. Your own PR only offers "comment", and an empty "request changes" is refused (GitHub rejects it too).
- The diff base picker: since your last review ("Review N"), any PR commit (its own changes included), reviews lost in a rebase, or the target branch. After a rebase or a merge of the target, the reviewed version is replayed onto the new base (`refs/icarus/review-base/…`), so the Changes list shows only what changed since your review. The pick is remembered.
- Threads come back with side, file-or-line, resolved and outdated state, reactions with who reacted, and where each sits in the current file. Reply, resolve, unresolve and react go straight to GitHub.
- `review.upload` puts images (10 MB) and videos (100 MB) into GitHub's own attachments with the `gh` login and returns the markdown to insert.
- A workspace with unsent drafts is not removed, by hand (without force) or by the rules.
- RPCs: `review.get`, `review.set_base`, `review.comment.add|edit|remove|clear|list`, `review.set_summary`, `review.submit`, `review.threads`, `review.reply`, `review.resolve|unresolve`, `review.react`, `review.upload`, plus a `review.changed` event. CLI: `icarus review show|base|comment|summary|submit|threads|reply|resolve|unresolve|react|upload`, run from inside a workspace.
- Checked live on a private sandbox repo: two drafts with an uploaded PNG and MP4, one COMMENT review, reply, resolve, unresolve and reactions read back through `review.threads`; GitHub's refusals to approve or request changes on your own PR; a two-PR stack. Auto-merge could not be enabled on that private repo.

**Releases and updates (D9):**
- `.github/workflows/release.yml` runs on `v*` tags and by hand (`gh workflow run release.yml -f version=0.2.0`), never on normal pushes. It builds `icarus` + `icarus-server` for Linux x86_64 and aarch64 (glibc, on native Ubuntu 22.04 runners) and a universal macOS tarball, then `Icarus.app` for Apple silicon (`scripts/release/bundle-mac.sh`; Intel with the `intel_app` input) as a `.zip` and a `.dmg`. The app is ad-hoc signed (bundle id `dev.icarus.app`) and Zed's own updater is compiled off with `ZED_UPDATE_EXPLANATION`. It publishes a GitHub release with `SHA256SUMS` and generated notes, and the version is stamped into the binaries (`ICARUS_VERSION`).
- Homebrew: `brew install --cask frbarbre/tap/icarus` installs the app and links `icarus` and `icarus-server`. The cask and the tap's README live in `scripts/release/homebrew/`; each stable release writes them into `frbarbre/homebrew-tap` with the new version and sha256, pushing with the `TAP_TOKEN` secret (set) or a `TAP_DEPLOY_KEY`. The cask pins the version; pinning (not `version :latest` as in §72) lets `brew upgrade --cask icarus` see new versions and checks the download.
- `update.check` / `icarus update check` compare the running version with the newest non-prerelease on `frbarbre/icarus`, through `gh` when it is signed in, else unauthenticated HTTPS (which 404s while the repo is private). A newer version sends one `update_available` notification per version ("Update available — restart to update"). Release builds check in the background every `updates.check_interval_ms` (6 hours) unless `updates.channel` is `none`.
- `icarus update install` runs `brew upgrade --cask icarus` for Homebrew installs. Otherwise it downloads the release asset, checks it against `SHA256SUMS` and swaps in the app bundle (macOS) or the two binaries, keeping `*.previous`. Then it stops the server gracefully (agents sleep) and starts the new one through the login service if installed. The server now also treats SIGTERM as a graceful shutdown.
- Not yet: a Linux app tarball (Zed's `bundle-linux` needs cargo-about, a musl remote server and lld, and would double the build), and the app does not yet listen to `notification` events, so the update notice shows in `icarus notifications` only. TODO: show `update_available` in the app with a "Restart to update" action that runs `icarus update install`.

## Blocked on you

Parked until you're back; work continues on other items.
- **Private repo:** `brew install` and unauthenticated update checks need public release downloads, so they work only once frbarbre/icarus is public. Until then `icarus update` works through a signed-in `gh`.
- **Cost:** a release builds the Zed app on a macOS runner (roughly an hour, billed 10x while private). Cut releases deliberately.

## In progress

- App: the browser, explorer and diff tab types.
- Server: terminals that survive a server restart.

## Next

- App: the remaining tab types (D31): an embedded browser tab (wry), an explorer tab, and the diff tab as the Source Control "Changes vs base" view with filters and Orca's toggles.
- App: the review mode UI on top of the `review.*` RPCs (queued comments in the diff, verdict, base picker, threads, media in comments).
- App: notifications (sound, dock badge, the `notification` event), session history in the palette, account switching in the status bar.
- App: a missing agent behind a tab with no messages starts a fresh one quietly instead of showing an error.
- Server: terminals survive a server restart or update (hand the PTYs to the new server process), as agents already do.
- Remaining plan items: LSP config import, theme conversion on import, Remote Control toggle, migration scripts for existing Orca CLI users, a live Tailscale check of remote servers, multi-server client.
