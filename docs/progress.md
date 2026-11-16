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
- Commands: `server start|stop|restart|status`, `terminal …`, `agent …`, `project …`, `workspace …`, all with `--json`.
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

**The app keeps its own user data:** `paths::APP_NAME` is `"Icarus"`, so the app never touches a normal Zed install. Config is `~/.config/icarus` (Zed's `settings.json`, `keymap.json`, `themes/` next to Icarus's own `icarus.json`), data `~/Library/Application Support/Icarus`, logs `~/Library/Logs/Icarus`, caches `~/Library/Caches/Icarus`, state `~/.local/state/Icarus` (XDG `icarus` dirs on Linux). Keychain items are prefixed `icarus:` and remote hosts get `~/.icarus_server`. The binary is still `zed` inside the bundle, because the `icarus` CLI owns that name and Zed's `cli` finds the app by it. Icarus user settings moved from `~/.config/icarus/settings.json` to `icarus.json`. While `icarus.json` is missing, an old `settings.json` is still read if it is valid Icarus settings (not Zed's), and the first settings write creates `icarus.json`.

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
- Checked live over Tailscale between two Macs: pairing with a pinned certificate, `--server` commands, a terminal run on the remote machine with its output read back, and `server pair` refused from the paired client.

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
- Tested with the fake agent only (the account's dir and the stripped API key reach the process); signing in to a real second account is untested until one exists.

**Review mode (server)** (github-review.md, D23):
- The server keeps a reviewer's unsent review per workspace and pull request in `state.db`: draft comments (path, line or range, side, markdown body, file comments), the summary, the verdict, and the chosen diff base. Drafts are keyed by PR, never by commit, so they survive server restarts and pushes. A file comment keeps its kind on disk (the fork lost it).
- A draft on lines outside the PR's diff becomes a file comment that quotes the code, as in the fork; a file the PR does not touch is refused with a reason. Up to 200 drafts are kept.
- `review.submit` sends one GitHub review with every draft (GraphQL variables, not a string-built query). On success the drafts and summary clear; on failure they stay and GitHub's message comes back word for word. Your own PR only offers "comment", and an empty "request changes" is refused (GitHub rejects it too).
- The diff base picker: since your last review ("Review N"), any PR commit (its own changes included), reviews lost in a rebase, or the target branch. After a rebase or a merge of the target, the reviewed version is replayed onto the new base (`refs/icarus/review-base/…`), so the Changes list shows only what changed since your review. The pick is remembered.
- Threads come back with side, file-or-line, resolved and outdated state, reactions with who reacted, and where each sits in the current file. Reply, resolve, unresolve and react go straight to GitHub.
- `review.upload` puts images (10 MB) and videos (100 MB) into GitHub's own attachments with the `gh` login and returns the markdown to insert.
- A workspace with unsent drafts is not removed, by hand (without force) or by the rules.
- RPCs: `review.get`, `review.set_base`, `review.comment.add|edit|remove|clear|list`, `review.set_summary`, `review.submit`, `review.threads`, `review.reply`, `review.resolve|unresolve`, `review.react`, `review.upload`, plus a `review.changed` event. CLI: `icarus review show|base|comment|summary|submit|threads|reply|resolve|unresolve|react|upload`, run from inside a workspace.
- Checked live on a private sandbox repo: two drafts with an uploaded PNG and MP4, one COMMENT review, reply, resolve, unresolve and reactions read back through `review.threads`; GitHub's refusals to approve or request changes on your own PR; a two-PR stack. With the sandbox public and a required check on `main`, auto-merge was enabled and disabled on a real PR.
- `review.request_reviewers` / `review.remove_reviewers` (`login` or `org/team`, comma-separated allowed).

**Diff navigation and Orca keybindings (2b):**
- In the diff tab, next/previous change (F7 / ⇧F7, `icarus::NextChange` / `PreviousChange`) follow Orca's §5.3 rules: no wrap at the ends, a change taller than the view pages by 85% until its end shows, the cursor lands on the first new line (a pure deletion's is the line after the gap). From outside any text field the keys focus the active diff (or the newest one while Source Control shows) and step there.
- Next/previous changed file (⌥F7 / ⌥⇧F7) start from the open diff tab's file; a single press wraps, a held key's repeats (under 150 ms apart) stop at the ends.
- The diff tab's key context carries `diff_mode` while it shows the diff, so remapped bare keys (↑/↓) never take over editing.
- `icarus settings import-keybindings <keybindings.json | orca settings export>` (and `icarus settings import`) translate Orca keybindings into the app's keymap (`~/.config/icarus/keymap.json`): a remap unbinds the action's default and binds the new keys in the right contexts, an empty list leaves no key, unknown or unsupported actions are reported, and importing again replaces only what the last import wrote (recorded in `.icarus-orca-keymap.json`). New actions it maps to: `icarus::ActivateNextItemOfSameType` / `ActivatePreviousItemOfSameType` and `icarus::FocusReviewSummary`.
- Not mapped yet: quick commands (`tab.runQuickCommand`, `tab.openQuickCommandsMenu`) and `tab.rename`, which have no app equivalent.

**Review mode (app):**
- **Review section** in the GitHub panel: reviewers in GitHub's order (open requests, then everyone who reviewed, opinionated verdicts first), with remove on unanswered requests and re-request on people who reviewed (disabled while the workspace has uncommitted files), a "request a review" box (login or `org/team`, Enter), the diff base picker, the standing pill, the summary (saved as you type), and the verdicts the server allows (only "Only comment" on your own PR), with the queued-comment count. GitHub's error text is shown as is and drafts stay on failure.
- **Diff tabs:** ⌘⇧A (or the header's Comment button, or the palette) opens a comment box under the selected lines. It sends to the review as a draft, or to the agent as a composer chip; ⌘Enter sends, Escape cancels; "Image or video" uploads through `review.upload` and previews the image. Drafts show inline with a PENDING badge (edit, delete); PR threads show at their lines (outdated ones stack at the end), with reply, resolve/unresolve, 👍, "Add to agent", and resolved threads collapsed to one line.
- Everything refreshes when the server announces `review.changed`, so CLI and agent edits show at once.
- The built-in `review` layout now opens the first changed file's diff tab next to the PR (`{type: diff, path: "{first_changed_file}"}`), with the PR in front.
- Checked live on a public sandbox PR: the Review section, the comment box opened by ⌘⇧A, a queued draft with an uploaded PNG, the submitted thread inline, a reply and a resolve each showing within seconds.
- Not yet: "Comment now" (post without a review), pasting or dropping images into the box, @-mention and `:emoji:` autocomplete, a gutter "+" and drag-select, the assignable-users picker, editing or deleting published comments.

**Releases and updates (D9):**
- `.github/workflows/release.yml` runs on `v*` tags and by hand (`gh workflow run release.yml -f version=0.2.0`), never on normal pushes. It builds `icarus` + `icarus-server` for Linux x86_64 and aarch64 (glibc, on native Ubuntu 22.04 runners) and a universal macOS tarball, then `Icarus.app` for Apple silicon (`scripts/release/bundle-mac.sh`; Intel with the `intel_app` input) as a `.zip` and a `.dmg`. The app is ad-hoc signed (bundle id `dev.icarus.app`) and Zed's own updater is compiled off with `ZED_UPDATE_EXPLANATION`. It publishes a GitHub release with `SHA256SUMS` and generated notes, and the version is stamped into the binaries (`ICARUS_VERSION`).
- Homebrew: `brew install --cask frbarbre/tap/icarus` installs the app and links `icarus` and `icarus-server`. The cask and the tap's README live in `scripts/release/homebrew/`; each stable release writes them into `frbarbre/homebrew-tap` with the new version and sha256, pushing with the `TAP_TOKEN` secret (set) or a `TAP_DEPLOY_KEY`. The cask pins the version; pinning (not `version :latest` as in §72) lets `brew upgrade --cask icarus` see new versions and checks the download.
- `update.check` / `icarus update check` compare the running version with the newest non-prerelease on `frbarbre/icarus`, through `gh` when it is signed in, else unauthenticated HTTPS (which 404s while the repo is private). A newer version sends one `update_available` notification per version ("Update available — restart to update"). Release builds check in the background every `updates.check_interval_ms` (6 hours) unless `updates.channel` is `none`.
- `icarus update install` runs `brew upgrade --cask icarus` for Homebrew installs. Otherwise it downloads the release asset, checks it against `SHA256SUMS` and swaps in the app bundle (macOS) or the two binaries, keeping `*.previous`. Then it restarts the server gracefully (agents sleep, terminals keep running) and starts the new one through the login service if installed. The server now also treats SIGTERM as a graceful shutdown.
- Not yet: a Linux app tarball (Zed's `bundle-linux` needs cargo-about, a musl remote server and lld, and would double the build), and the app does not yet listen to `notification` events, so the update notice shows in `icarus notifications` only. TODO: show `update_available` in the app with a "Restart to update" action that runs `icarus update install`.

**Terminals survive restarts:**
- `icarus server restart`, `icarus update install` and a SIGTERM from launchd or systemd hand terminals to the next server. Processes, pids, background jobs, scrollback and sequence numbers stay the same, so attached clients resume by sequence. Agents still sleep and resume.
- How it works: the stopping server stops its readers and passes each PTY master over `SCM_RIGHTS` to a small `icarus-server hold` process, with the screen snapshot, recent output and the auth token. The next server takes them over on start. The master is never closed, and children run in their own sessions, so they get no SIGHUP. A holder that nobody claims gives up after 2 minutes (`ICARUS_HANDOFF_HOLD_SECS`).
- `icarus server stop` still ends terminals. After a crash or `kill -9`, the PTYs close with the server and their shells get SIGHUP; anything detached (`nohup`, `setsid`) is orphaned. The next server lists these terminals as `lost` (from `terminals.json`), and they can be respawned.
- Tested end to end (`crates/icarus-cli/tests/restart_e2e.rs`) on macOS and in a Linux container. The launchd and systemd restart paths have not been tested against a real service.

**App terminals run on the server (§3c, features §2.5/§4):**
- `icarus terminal attach <id>` (or `--create [--cwd] [--workspace] [--label] [-- cmd…]`) shows a server terminal in the current one, dtach-style: raw mode, the screen replayed, then output streamed by sequence; keys go to `terminal.write`, size changes to `terminal.resize`. It rides out a server restart (reconnects for up to 60 s and resumes by sequence, with no gaps or repeats) and exits with the shell's code (unknown, so 0, for a shell adopted across a restart). Closing the viewer leaves the shell running.
- In the app, every new local shell in an Icarus workspace (terminal panel, centre terminals, layout terminal tabs) is created on the server for that workspace and shown by running `icarus terminal attach` (the `icarus` next to the app in a release bundle, else on `PATH`). The tab keeps the server id, so a restored tab reattaches to the same shell; if that shell is gone a new one starts. Closing a tab on purpose kills the server shell; quitting or crashing the app leaves it running. Tabs are titled like Zed's own ("folder — zsh"). Upstream hooks: `project::terminals::ShellRouter` (picks the program for local interactive shells), `Terminal::external_id`, and `terminal_view::ExternalTerminalCloser`; Icarus registers both only in the real app. Shells outside Icarus workspaces, remote projects, `NewLocalTerminal` and task terminals stay as Zed runs them.
- Checked live: a counter started in an app terminal kept counting after the app was killed with `kill -9`, the relaunched app reattached the tab to the same shell with the output that continued meanwhile, and an `icarus server restart` with the app open kept the tab streaming without a gap.

**Language tooling and themes on import (§3b, D10, D28):**
- Language tooling is Zed's own (extensions plus Zed's `settings.json`); Icarus has no LSP settings or CLI. `icarus settings import` translates Orca's `languageServers`, `formatters` and `linters` (Orca's defaults when absent) into Zed keys: `languages.<Lang>.language_servers` (the chosen server first, its rivals turned off with `!name`, then `...`), `formatter` and `format_on_save`, `lsp.<server>.binary` for a custom command, and `auto_install_extensions` for tools that need one. Zed has pyrefly, pyright, basedpyright, ty, Ruff and typescript-language-server built in; tsgo comes from the `tsgo` extension (server `typescript-ls`) and oxlint/oxfmt from `oxc` (ids checked against zed-industries/extensions).
- Appearance becomes Zed settings too: `theme` (`mode` as in Orca, `system` by default, with a light and a dark theme), UI, buffer and terminal fonts, terminal line height, and inactive pane opacity. Orca's VS Code editor theme and custom terminal palettes become one Zed theme family, `themes/icarus-imported.json` ("Icarus Imported Dark/Light"); Orca's built-in terminal themes carry no colours and fall back to the Zed theme's.
- The result is merged into the app's Zed config directory (`~/.config/icarus`, or `--zed-dir` / `ICARUS_ZED_CONFIG_DIR`). Keys the user already set are kept and listed, comments and trailing commas are read, and the commented original is saved once as `settings.json.before-icarus-import`. `--dry-run` writes nothing, `--print-zed` prints the Zed settings, `--skip-zed` leaves Zed alone.

**Import from Zed:**
- `icarus settings import-zed [--from DIR] [--from-data DIR] [--zed-dir DIR] [--dry-run] [--json]` brings a normal Zed install (`~/.config/zed`; extensions from Zed's data directory, `~/Library/Application Support/Zed` or `~/.local/share/zed`) into the app's config directory (`~/.config/icarus`). `ICARUS_ZED_SOURCE_DIR` / `ICARUS_ZED_SOURCE_DATA_DIR` override the source.
- Zed's `settings.json` (comments and trailing commas read) is merged in; keys the app's file already sets win, and `agent.dock`, `agent.enabled`, `disable_ai` and `agent_servers.Icarus` are never imported. Theme, icon theme and fonts carry over. Every installed extension id goes into `auto_install_extensions`, so the app downloads it itself; dev extensions (symlinks to local checkouts, or `dev` in Zed's `index.json`) are reported, not imported. `keymap.json`, snippets and custom themes are copied only when the app has none of its own (Zed's empty keymap template and the Orca import's theme don't count); otherwise they're reported as skipped. Running it again changes nothing.
- It runs where the app runs, never over RPC: both folders are on the app's machine, and a remote server would read and write its own host's. The app calls the same library: on first launch, if a Zed install has something new, a one-time notification offers "Import your Zed setup (settings, N extensions, theme)?" with Import / Not now (remembered in the app's key-value store), and `icarus: import zed setup` runs it any time. Settings, keymap, snippets and themes reload on their own; the app installs the new extensions at once.

**Remote Control (D21):**
- `agents.remote_control` (default off; Orca's `openClaudeTabsInWebView` imports into it) starts new Claude Code sessions with Claude Code's own `--remote-control` flag, appended last to the user's unmodified `claude` command line. `agent.create` / `workspace.create` take `remote_control`, and `agent.remote_control {id | workspace, enabled}` switches it per agent or for every Claude agent of a workspace: an idle agent restarts on its session right away, a busy one on its next start. The choice survives server restarts.
- Agent info carries `remote_control` and `remote_control_url` (`https://claude.ai/code/<bridgeSessionId>`, read from Claude Code's `sessions/<pid>.json` records only; the `.key` files beside them are never opened). CLI: `icarus agent remote-control on|off <id> | --workspace <id>`, `--remote-control on|off` on `agent create` and `workspace create`; `icarus agent list` shows the link.
- Tested with the fake agent; Remote Control with the headless stream-JSON mode against a real account is still spike 0.

**Release prep (v0.1.0):**
- `scripts/release/bundle-mac.sh` compiles the app on the `stable` channel (`ZED_RELEASE_CHANNEL=stable`, as Zed's own bundle script does); `app/crates/zed/RELEASE_CHANNEL` stays `dev` for local builds. Stable turns on: saved API keys in the Keychain (`icarus:<url>`), the single-instance check (on Icarus's own port, 47737+, so a running Zed is not mistaken for Icarus), the app DB under `db/0-stable` (dev builds keep `0-dev`), Zed's stable extension API range (WIT up to 0.7.0: catppuccin, oxc 0.4.x and tsgo 1.x all fit), no GPUI inspector, and the stable app icon in About.
- Stays off on every channel: Zed's updater (`ZED_UPDATE_EXPLANATION`; "Check for Updates" points to `icarus update`), crash uploads (no `ZED_MINIDUMP_ENDPOINT` is compiled in), and, through Icarus's default settings, telemetry (`telemetry.metrics` and `telemetry.diagnostics` false), the title bar's "Sign In" and Zed's onboarding banners. "What's new" only follows an update Zed's updater installed, so it never shows. Kept on purpose: the extension registry at api.zed.dev, Zed's docs link, and sign-in from the user menu or agent panel for those who want Zed's hosted models.
- Branding: windows and the release-channel name read "Icarus" ("Icarus Dev" locally), the app menu says About/Hide/Quit Icarus, Help links to frbarbre/icarus (Zed's email, Twitter, jobs and release-notes items are gone), Install CLI is gone from the menu (it would replace Zed's `/usr/local/bin/zed`), and `scripts/release/brand-plist.sh` renames Zed in the bundle's permission prompts and document type.
- Versions: the app keeps Zed's version (1.30.0) for the extension registry; About shows the Icarus version (`ICARUS_VERSION` from the workflow, else the root workspace version) with Zed's beside it, and `icarus --version` / `icarus-server --version` report the stamped version like `server.info` and `update.check` already did.

**Notes in the app:** notes render inline in the diff tab under their lines, as cards with an accent edge and a NOTE or AGENT NOTE badge so they read differently from PR threads. ⌘⇧A's box writes a note on its "Agent" side (`note.add`). Replying shows the throwaway session as "Thinking…", then its answer and the files it changed (each opens its diff tab); a failed answer shows the error and Retry, which asks the same question again. Notes can be edited, deleted, resolved (collapsing to one row) and added to the agent's composer as one chip. Open diff tabs refresh from `note.changed`, and Source Control rows show a badge with the file's open-note count. Checked live with the real `claude`: a reply was answered in about 4 s and appeared in the open tab by itself.

**Review and notes, finished:**
- **Comment now:** a comment box posts one comment on GitHub straight away instead of queueing it (`review.comment.post`, `icarus review comment add --now`), with the same out-of-diff handling as queued drafts.
- **Uploads everywhere:** images and videos can be picked, pasted or dropped into every comment box (review comments, replies, notes and the review summary). An "Uploading …" placeholder sits at the cursor until GitHub answers; a failure takes it back out and shows GitHub's words. Pasted data goes up through `review.upload`'s base64 route.
- **@-mentions and `:emoji:`:** comment boxes complete the PR's author and the repository's assignable users (`review.mentionables`, cached for ten minutes; `icarus review people`), and GitHub emoji shortcodes from a bundled list. The reviewer box completes logins as you type.
- **Gutter:** hovering a diff tab's gutter shows "+"; clicking or dragging over rows opens the comment box on them. Rows with a PR thread, a pending draft or a note get a coloured strip (blue, amber, green) and, on hover, an icon that scrolls to the card and expands it if collapsed. One small upstream hook (`Editor::set_gutter_comment_hook`) lets Icarus own Zed's diff-review gutter button.
- **Published comments:** your own comments get edit and delete (with a confirmation) (`review.comment.edit_published` / `delete_published`; `icarus review comment edit-published|delete-published`), and all eight GitHub reactions from a picker, with each reaction pill toggling.
- **Notes:** an answer from a reply session links to its transcript (it wakes the session from history into an agent tab). Notes also show, with the same gutter "+", in plain file editors of an Icarus workspace.
- Checked live on the public sandbox: comment now, edit, two reactions and delete round-tripped through GitHub, and a note appeared in an open file editor by itself.

**Notes (server):**
- Notes live in state.db: a note sits on a file and line range of a workspace (worktree-relative path, optional `side: old` for the base side of a diff, optional `base`), with its author (`user` or `agent` plus the agent's id), markdown body, the lines it was written on, times and a resolved flag. Each note has a thread of replies by `user`, `agent` or `reply-agent`; a reply-agent entry also carries its session id, status (pending, running, done, failed), error and the files it changed.
- RPCs: `note.add {workspace | cwd, path, line, end_line?, body, author?, agent_id?, side?, base?}`, `note.list {workspace | cwd, path?, include_resolved?}`, `note.get`, `note.thread` (the conversation as text), `note.reply {id, body}`, `note.edit {id, body, reply_id?}`, `note.remove {id, reply_id?}`, `note.resolve` / `note.unresolve`, and `attach.note {id}`. Every change broadcasts `note.changed {workspace, id, path, removed}`.
- Replying never messages the workspace's running agent. Each user reply starts its own throwaway session in the worktree (default provider and account, like other new sessions), given the whole thread: the note, file and lines with the current code, and every finished reply. Its last message is posted back as the reply; then the session is killed and dropped from the live agents, and its transcript stays in session history under the workspace. It may edit files when asked: the worktree is snapshotted before and after (the turn-snapshot code, which leaves the user's index alone), and the changed files are stored on the reply and listed under its answer. Several replies can run at once, each seeing the thread up to its own message. Failures keep the agent's error; `notes.reply_timeout_ms` (default 10 minutes) stops a session that takes too long; a server restart marks unfinished replies failed and kills their sessions. Reply sessions are not workspace agents (so layouts, rules and the agent list ignore them) and send no "agent finished" notifications.
- `attach.note` puts the whole conversation, with file, lines and code, in the composer as one `note` attachment, like PR review threads.
- CLI: `icarus notes add <path>:<line>[-<end>] <body…>`, `list [--path] [--all]`, `show`, `reply <id> <body…> [--wait]`, `edit`, `rm`, `resolve`, `unresolve`, `attach`, with `--json`. The workspace comes from `--workspace`, else `ICARUS_WORKSPACE_ID`, else the current directory; paths are relative to the current directory or the worktree; a literal `\n` in a body is a newline. Run by an agent (`ICARUS_AGENT_ID` set), the note or reply is authored by that agent, and an agent's reply starts no session.
- Tested with the fake agent (which now follows the last `@fake …` line of a prompt): add, list, edit, remove, resolve, agent-authored notes, answers landing in the thread from a session that is not the workspace's agent and then ends, two replies at once, a reply that writes a file, failure, timeout, the composer attachment, events and a restart. One live run with the real `claude` answered "What does line 1 say? One sentence." in about 6 seconds.

**Source Control actions and agent turns** (source-control.md §1.6, §4; features §2.9):
- Server RPCs, so the app, the CLI and agents share them (they run where the worktree is): `git.status`, `git.discard`, `git.discard_kind`, `git.stage`, `git.unstage`, `git.commit {message, amend?, all?}`, `git.push {force_with_lease?}`, `git.pull`, `git.sync`. Every change broadcasts `git.changed {workspace, op}`. Errors are git's own words; git never waits on a prompt or an editor.
- Discard throws away only the uncommitted change (committed branch changes stay): edits go back to HEAD, untracked or newly added files are deleted, a file deleted on disk is restored, and a staged rename brings its old path back. Paths outside the worktree are refused.
- Commit takes what's staged, or every change when nothing is (the upstream commit box's rule); `--amend` without a message keeps the old one; commit hooks run and their output is the error.
- Push publishes a branch whose upstream isn't a same-named branch (a workspace branch starts out tracking its base, e.g. `origin/main`, which `published: false` reports); pull and sync refuse or skip until then. `--force-with-lease` refuses when the remote moved since the last fetch.
- CLI: `icarus git status|discard|stage|unstage|commit|push|pull|sync`, workspace from `--workspace`, `ICARUS_WORKSPACE_ID` or the current directory.
- App, Source Control panel: a hover Discard button per row ("Discard changes", "Delete untracked file", "Restore file"), only for an unstaged or untracked, non-conflicted change and never under a turn or commit scope, after a confirmation; an Agent turns section (prompt, `agent · 3m ago · N files +I −D`, click to scope the list to that turn, "Latest turn" only while no newer turn exists); a slim git bar with the branch, ↑ahead ↓behind or "not published", Commit…, Pull, Push and a menu with Sync and Force push (with lease), git errors shown verbatim. The commit box itself stays out of the panel as Orca removed it (§4): Commit… opens a small dialog where ⌘Enter commits, with an amend toggle. Actions: `icarus::CommitChanges`, `PushBranch`, `ForcePushWithLease`, `PullBranch`, `SyncBranch`, `DiscardSelectedChange`. Panels refresh at once on `git.changed`.
- Checked live: a tiny real-Claude turn appeared in Agent turns, a CLI commit and a CLI discard updated the open panel, and discarding a committed-only file was refused.

**Sidebar, cards and quick commands** (features.md §2.2, §2.14, §2.15; plan §3a):
- Group by stack: one section per GitHub stack ("Stack #N"), newest stack first, each listed from the PR furthest from main down (or up with `sidebar.stackOrder: "asc"`), workspaces in the rules' `reviewing` status in their own section first, and "Not in a stack" last in status order, exactly as Orca builds it. `workspace.list` now carries a cached `pr` summary per workspace (number, state, +/−, checks, merge queue, stack place) and never calls GitHub for it.
- Card fields follow `workspace.cardFields` (Orca's `worktreeCardProperties` ids, defaults matching Orca's): status chip, agents, PR chip with a checks dot, PR author, PR diff, stack position, Linear title with coloured label pills, ports and the project chip. The status chip stays while grouped by status, because it is the status menu. A View (gear) menu in the panel header sets the grouping, stack order and fields; the same values change with `icarus config set`, the Orca import, and the actions `icarus::ToggleCardField {field}`, `SetWorkspaceGrouping {group_by}` and `ToggleGroupByStack`.
- ⌥⌘L (`icarus::OpenLinearIssue`, macOS) opens the window's linked Linear issue from the server's cache, or says why it can't. It replaces upstream's ⌥⌘L "search within selection" in editors and panes; that keeps its key inside the search bar.
- ⌘J (`icarus::OpenWorkspaceSwitcher`, Ctrl+J on Linux) is a fuzzy palette over every workspace (name, issue id and title, branch, status, PR number); Enter opens it. It takes the key from upstream's toggle-bottom-dock.
- Quick commands from the `quickCommands` setting (the project's own first, then global ones): `icarus::RunQuickCommand` repeats the workspace's last one (or the first), and `icarus::OpenQuickCommandsMenu` picks one. As in Orca, a command still running in its terminal gets Ctrl-C; otherwise it's typed into an idle terminal shown in the window (its own first); otherwise a new server terminal tab runs it. The server reports a terminal's `busy` state (a foreground job other than the shell) and gains `terminal.set_label`. Neither action has a default key (Orca's ⌥⌘P is Open preview here); the user's ⌘R and ⌃P remaps import to them.
- Checked live: grouping and a field toggle saved through dispatched actions, the switcher and the menu, and all three quick-command paths (new terminal, reuse of the idle one, Ctrl-C to the running one). The first live run caught a crash (reading the workspace during its own update) before it shipped.

**Server features in the app (gap #6):**
- **Notifications:** the app follows the server's `notification` and `notifications.changed` events. While an Icarus window is in front a notification is a toast ("Open" jumps to its workspace window and, for agent news, its agent tab; `update_available` offers "Restart to update", which asks first and then runs `icarus update install`). Otherwise it's a macOS banner through GPUI's system notifications, plus the "Glass" sound when the server says sound is on; clicking the banner does the same as "Open". The Dock badge shows the unread count. A status-bar bell lists the recent 50 (newest first, unread dotted) with Mark all read and Clear, also as actions (`icarus::ShowNotifications`, `MarkAllNotificationsRead`, `ClearNotifications`). Banners need the bundled app: macOS refuses them to an unbundled dev build, which GPUI already guards.
- **Session history:** `icarus::OpenAgentHistory` opens a palette over `agent.history`; typing searches on the server (full text, after a short pause). Rows show the title or first prompt, provider, workspace, age and turns, and a search snippet. Enter resumes the session (`agent.resume`) and opens it as an agent tab in its workspace's window. No default key: ⌘⇧H is project-wide replace in Zed.
- **Accounts:** a status-bar item with the default account's usage meter (the fuller of Claude Code's 5-hour and 7-day windows, e.g. "Claude 61%"; tooltip with both). Its menu lists every account per provider with its meters, switches the default (`account.select`) and adds an account, showing the provider's own login command with a copy button; Icarus never handles credentials. With only the system login and no usage yet it stays hidden.
- **Remote Control:** with an agent tab active, the status bar shows its Remote Control state; the menu turns it on or off (`agent.remote_control`, `icarus::ToggleRemoteControl`) and, once Claude Code reports it, offers the claude.ai link (open or copy). The app asks for the link every 2 s until it appears. The app crate tells `icarus_ui` which server agent a tab shows.
- **Remote servers:** the Workspaces panel follows the servers saved with `icarus remote add` (re-reading the file, so new ones appear within seconds), keeps one connection per server and lists each server's workspaces in its own section with the server's name, address and state. A Servers menu in the panel header lists this machine and every saved server and explains how to pair another.

**Working in a saved server's workspaces from the app:**
- **Routing:** every Icarus call from a window goes to the server that owns its workspace (`icarus_ui::routing`): a project opened over SSH belongs to the saved server on that host; a local folder to the server whose workspace holds it (the app's own server wins when both list it). The next-action button, links and ports, the GitHub and Review panels, Source Control, notes, quick commands and Remote Control all follow it; the panels no longer keep their own connections, they use the app's or the saved server's.
- **Opening:** a saved server with an SSH destination (`ssh://host`, or `icarus remote add … --ssh <host>` for a paired address) opens its workspaces as Zed remote projects over SSH; inside them Zed starts `icarus acp` and terminals on that host, so they reach that host's server. Without one, a folder that exists here (same machine, shared disk) opens locally; otherwise the app explains how to add an SSH destination.
- **Agents and terminals for local folders of a saved server:** agent tabs run `icarus --server <name> acp` (a hook in the agent panel picks the agent server per project), and terminals attach with `icarus --server <name> terminal attach`; a tab's terminal id carries its server (`term-1@desk`), so restored tabs reattach to the right one.
- **Cards:** a remote card's status chip moves it on its own server. Refresh events (review, notes, git, ports) from every saved server reach the open views.
- **The app's own server:** the Servers menu picks which server the app's own connection uses (this machine or a saved one); it reconnects at once and is remembered.
- **SSH remoting with releases is unverified:** a stable build downloads Zed's official remote-server binary for the app's Zed version (1.30.0) from zed.dev. It's built from upstream, not this fork, so its protocol may not match; dev builds need `ZED_BUILD_REMOTE_SERVER` to build and upload their own. Shipping Icarus's own remote-server binary with releases is the fix if they don't match.
- Checked live: the second server's section with its workspace (a temporary server paired over TLS on localhost), a real agent turn's notification in the bell list, mark-all-read reaching the server, the history palette resuming a sleeping session into its tab, the account meter, and the Remote Control item on an agent tab.

**Icarus settings in the app (plan §4, features.md §2.17):** `icarus::OpenSettings` opens an "Icarus Settings" tab. Everything reads and writes through the server's `config.*` RPCs, so the layers stay authoritative: a You / Project / Local switch picks where edits go (user `icarus.json`, the project's `icarus.json`, or `icarus.local.json`), each row shows the layer its value comes from (via `config.explain`) with a reset when it's set in the chosen layer, and "Edit in JSON" opens that layer's file (new `config.paths` RPC). Sections:
- Statuses: add, rename, reorder and remove, with Orca's colour and icon pickers.
- Automation: the loop, armed, interval, and per PR condition the column it moves cards to or "Don't move".
- Agents (default provider, Remote Control default), integrations (GitHub login check through `gh auth status`, the Linear key command and refresh), notifications (master switch, sound, each kind), workspace card fields, ports and previews, named layouts (listed, edited in JSON), update channel.
- Checked live: the tab opens and shows the dev server's real settings. A GPUI test covers loading, sources, writes to the user and project layers, adding a status and the card-field toggle; baselines `icarus_settings(_light)`.

**Orca workspace state (plan §4):** `icarus settings import-orca-state [--from <profile-state.db>] [--dry-run]` reads Orca's `profile-state.db` read-only (default: the single profile under Orca's data folder) and sends its `worktreeMeta` and `repos` documents to the server's `settings.import_orca_state`. Each Orca worktree is matched to an Icarus workspace by path, else by the repository's canonical remote plus the worktree's folder name (= workspace name or branch). It brings over the status, a pinned display name as the title, the linked Linear issue (each only where the workspace has none or a different status), and diff notes and agent notes with their replies as Icarus notes; imported note ids are remembered, so running it again adds nothing. Checked against a read-only copy of the real database: 15 Orca worktrees; a throwaway workspace named like one of them matched by repo and name with its status and one note planned.

**Themes (D10):** `icarus theme import vscode|warp <file> [--name] [--select] [--dry-run]` converts a VS Code colour theme (JSON with comments, through the Orca importer's converter) or a Warp theme (YAML; gradients become their first colour) into a Zed theme family in the app's `themes/` folder (`imported-<name>.json`, so re-importing replaces it). `--select` makes it the theme for its appearance, keeping the mode and the other slot, and backs up a commented settings file first.

**Keyboard details (Orca's pane and terminal keys):**
- ⌥⌘←/→/↑/↓ (`icarus::FocusGroupLeft/Right/Up/Down`, macOS) move focus to the nearest tab group sharing that edge, Orca's rule exactly (2 px tolerance, larger shared edge breaks ties), with no wrap and nothing at the edge; they work from editors and terminals too. Zed's own ⌥⌘←/→ (previous/next tab, still on ⌘{ / ⌘}) and the editor's ⌥⌘↑/↓ (insert cursor above/below, still on ⌘⌃P / ⌘⌃N) give way.
- Terminal splits use Zed panes rather than Orca's panes-inside-a-tab, so a split terminal can be dragged, resized, zoomed and restored like any tab: ⌘D / ⌘⇧D split right/down (Linux Ctrl+Shift+D / Alt+Shift+D, as in Orca), ⌘] / ⌘[ cycle the terminal panes in creation order and wrap (`icarus::FocusNextTerminalPane` / `FocusPreviousTerminalPane`), ⌘⇧Enter expands one (`workspace::ToggleZoom`), ⌘K clears. A split of a server terminal starts its own server shell in the source's folder (before, Zed's clone re-ran `icarus terminal attach` and mirrored the same shell); split layouts reattach to the same shells after a restart. ⌘⇧D displaces Zed's debug-panel focus only inside a terminal.
- Line editing follows Orca's `terminal-shortcut-policy.ts`: ⌘←/→ ^A/^E, ⌘⌫ ^U, ⌘⌦ ^K, ⌥←/→ ESC b / ESC f, ⌥⌫ ESC DEL, ⌥⌦ ESC d except in full-screen apps (vim keeps xterm's own `CSI 3;3~`), ⌘↑/↓ jump to the top/bottom of the scrollback; on Linux Ctrl+←/→ send ESC b / ESC f.
- The Orca keybindings importer also maps `tabGroup.focus*` and `terminal.splitRight/splitDown/focusNextPane/focusPreviousPane/expandPane`.

**Setup and archive scripts (Orca's `orca.yaml` hooks, features.md §2.1):**
- Scripts come from the shared source (the repo's `orca.yaml` `scripts.setup` / `scripts.archive`, else `project.hooks.scripts` in the committed `icarus.json`) and the local one (`project.hooks.scripts` in `icarus.local.json` or user settings), combined by `project.hooks.commandSourcePolicy` as Orca does: an unset policy means local-only when a local script exists; `run-both` joins them. `setupRunPolicy` (`run-by-default`, `skip-by-default`, `ask`) and `setupAgentStartupPolicy` (`start-immediately`, `wait-for-setup`, also read from `orca.yaml`) work as in Orca. The Orca import already maps `hookSettings` into `project.hooks`.
- Setup runs when a workspace is created, after the worktree exists: a bash runner with `set -e` (a shebang's flags replayed) in a server terminal labelled **Setup**, with Orca's variables (`ORCA_ROOT_PATH`, `ORCA_WORKTREE_PATH`, `ORCA_WORKSPACE_NAME`, the legacy `CONDUCTOR_ROOT_PATH` / `GHOSTX_ROOT_PATH`), `ICARUS_*` equivalents plus `ICARUS_WORKSPACE_ID`, and Git prompts disabled. The tab stays a usable shell afterwards and survives app and server restarts. Its status (running, succeeded, failed with exit code or reason, skipped) is on `workspace.list` / `workspace.info` as `setup`, refreshed with `workspace.changed`; an optional `project.hooks.setupTimeoutMs` interrupts a slow script. With `wait-for-setup` the workspace's agent exists at once but its first prompt (and a rules-created review agent) waits until setup ends. An `ask` project refuses a create without a decision (nothing is created); the rules' review workspaces answer it with run.
- The layout a new workspace opens with shows the Setup terminal (in its first plain terminal tab, else after the first pane's tabs); layouts' terminal tabs accept a `terminal_id`. Cards show "Setting up" with a spinner, or "Setup failed" with the reason in a tooltip. Server terminals with a label (Setup, quick commands) are titled by it.
- Archive runs before a workspace is removed (also by the rules' delete action), in the worktree with the same variables, through bash with a 120 s timeout (`project.hooks.archiveTimeoutMs`) that stops its whole process group, output captured. A failure keeps the workspace unless the removal is forced or `allow_failed_archive` is set; `skip_archive` skips it. An unforced rules delete whose archive fails is refused and retried next tick.
- CLI: `icarus workspace create --setup / --no-setup`, `icarus workspace setup <ws>` (re-run in a new Setup terminal), `icarus workspace remove --skip-archive / --allow-failed-archive`; `workspace list/show` say "setting up" or "setup failed (code)".
- Checked live in the app: the new workspace opened with its agent and the Setup tab beside it; the card showed "Setting up" during a re-run; removal ran the archive script first. That run caught a real bug the tests had missed (they used `/bin/sh`): zsh reserves `$status`, so the exit-code line failed in a zsh login shell; a test now runs the line under every installed shell.

**Visual tests pin their theme:** the runner sets the theme mode to dark at start, so dark baselines no longer follow the machine's appearance; `ICARUS_VISUAL_ASSUME_APPEARANCE=light` proves it (all baselines pass assuming a light system). CI no longer forces dark mode.

## CI

`.github/workflows/app.yml` tests the app on every push to main and every PR that touches `app/**`, the three crates it pulls from the root workspace (`icarus-client`, `icarus-protocol`, `icarus-settings`) or the root `Cargo.toml`, and on demand via workflow_dispatch. Four parallel jobs on free hosted runners: `cargo test` for icarus_ui, title_bar, agent_ui, git_ui, project_panel and `workspace --lib` on Linux; the whole `zed` crate under nextest with two retries for upstream's timing-sensitive session-restore tests, also on Linux; clippy with `-D warnings` on icarus_ui, agent_ui, workspace, title_bar and zed (all targets) on macOS so the `cfg(target_os = "macos")` code is linted; and the Icarus visual tests (`ICARUS_VISUAL_ONLY=1`) on macOS, which compare against the tracked `icarus_*.png` baselines and upload the screenshots and diffs when they fail. The macos-15 image's Xcode ships the Metal compiler, so no extra toolchain or `runtime_shaders` is needed. Builds use `CARGO_PROFILE_DEV_DEBUG=0` and no incremental, caches are per job and saved only from main, and no disk cleanup is done because a build peaks under 20 GB on runners with 45+ GB free. The runner pins the theme mode to dark, so the baselines don't depend on the runner's system appearance.

## Blocked on you

Parked until you're back; work continues on other items.
- **macOS notification permission:** accept the prompt the first time the app shows a notification.
- **Homebrew tap token:** the `TAP_TOKEN` secret can't push to frbarbre/homebrew-tap ("Permission denied", 403). Edit the token at github.com/settings/personal-access-tokens: Repository access → only frbarbre/homebrew-tap, Permissions → Contents: **Read and write**. Then rerun: `gh run rerun <release publish run id> --failed`, or `gh workflow run release-publish.yml -f version=0.1.0 -f run_id=43780249933`.

**v0.1.0 is released** (github.com/frbarbre/icarus/releases/tag/v0.1.0): the macOS app (zip + dmg, Apple silicon, unsigned), the CLI and server for macOS (universal) and Linux (x86_64, arm64), and SHA256SUMS. The tag run built everything, but GitHub refused its release creation with a 403 that a probe couldn't reproduce from a dispatched run; `release-publish.yml` published the same built files.

## In progress

Nothing right now; see Next.

## Next

Gaps against the [MUST] list in features.md, in the order they're being built:
1. **Terminal details, leftovers:** task terminals on the server; Orca's pane-only extras (equalize, pane titles, the single-group ⌥⌘←/→ fallback to worktree history); kitty-keyboard-aware line editing (Orca lets a TUI that negotiated the kitty protocol keep ⌥⌫ / ⌥←→; Zed's terminal has no such signal, so those keys always translate).
2. **Review and notes, leftovers:** gutter markers are coloured strips plus an icon on hover (not a permanent icon per row); a row with several kinds shows one marker (threads, then drafts, then notes); the reviewer box completes logins rather than offering a separate picker list; plain file editors show notes but not PR threads or drafts.
4. **Source Control leftovers:** the right-click row menu (open, reveal, copy path, Discard), stage/unstage in the app (RPCs and CLI exist; the single Changes list has no staging UI, as in Orca), the Commits section, and turn rows opening their first file in a diff tab rather than the commit view.
5. **Sidebar and cards, leftovers:** grouping by repo and by PR status (they fall back to status grouping), sort orders, the text filter, pin and multi-select (features.md §2.2), a per-client override of the card fields, and hotkeys bound per card field by default.
5b. **Setup and archive, leftovers:** Orca's `defaultTabs` and `worktree.sharedDirectories` / `.worktreeinclude` from `orca.yaml` (layouts cover tabs today), Orca's trust prompt for a repo's committed scripts, and an "ask" prompt in the app (an `ask` project needs `--setup` / `--no-setup` from the CLI).
6. **Server features in the app, leftovers:** an SSH-opened workspace checked end to end on a real second machine (and Icarus's own remote-server binary in releases if Zed's official one doesn't match the fork); terminals in SSH-opened projects are Zed's remote shells, not server terminals (Zed's terminal routing hook only covers local shells); notifications, open-file requests and layouts from saved servers other than the app's own; "this machine" listed alongside the saved servers when the app's own connection points elsewhere; per-kind notification toggles in the app (they're settings today); Remote Control's link checked against a real account.
7. **Settings, leftovers:** the settings tab doesn't refresh on its own when settings change elsewhere (CLI or agent) — reopen or switch scope; Orca's pending PR-review drafts (a separate `pending-review-drafts.json`) aren't imported; the other rule options (on merge/close, on reviewed, conflict agent, review inbox) are edited in JSON.
7c. **SSH remote projects in releases:** build and ship the fork's own `remote_server` with each release (Zed's SSH remoting would otherwise download Zed's official 1.30.0 remote server, which may not speak this fork's protocol), and test opening a workspace over SSH against a real host.

Dropped on purpose: the Claude web view (plan §5 open question; the composer replaces it); the Workspace Board (kanban), since statuses are a simple menu; permission prompts routed to the app (agents run with bypass, the chosen default); measuring the plan §8 speed targets against Orca (user's call, 2026-11-16).
