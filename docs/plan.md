# Plan: a fast, general worktree-agent workspace

Draft for review, 2026-11-15. It combines three inputs:
- [features.md](features.md): what the Orca fork does.
- [settings-compat.md](settings-compat.md): the settings file we must import.
- [oss-research.md](oss-research.md): what we can reuse.

## 1. Goals

1. **Fast.** Starting an agent, Cmd+click to another file, and switching workspaces must all feel instant. The perf budgets are in §8.
2. **Work never stops.** Agents, terminals, dev servers and automations live in a server process, local or remote. Quitting, updating or crashing the client doesn't touch them.
3. **Agents in a composer, not a terminal.** A terminal is still there for dev servers and shells.
4. **A general product for anyone.** Statuses, rules, prompts, providers and per-language tools are all data. Nothing about one person's or one company's workflow is in the code or the repo.
5. **Run and tuned by agents.** Every setting, action and view is reachable from the CLI and from plain config files with a published schema, so an agent can operate and reconfigure the whole app without the UI (§2, §1b).
6. **Drop-in for Orca users.** It imports `orca-settings.json` (format `orca-settings`, version 1) and reproduces the imported status automations exactly. It can export the same format back.

## 1b. Configuration layers: everything personal lives outside the repo

All behaviour is configuration, resolved in layers, where later layers win:

1. **Built-in defaults:** generic and neutral.
2. **Shipped presets,** opt-in templates in the repo: e.g. "GitHub PR flow", "stacked PRs with merge queue", "review inbox", "CI triage". Generic wording, no company specifics.
3. **User settings** at `~/.config/icarus/settings.json`: personal prompts, statuses, rules, keybindings, card fields, tools.
4. **Project settings** at `icarus.json`, committed to the project's own repo: team-wide statuses, rules, filters, language tooling and prompts for that codebase.
5. **Project-local overrides** at `icarus.local.json`: gitignored, for personal tweaks to one project.

**Notes:**
- **Prompts are settings values,** never source code: agent prompts, review and re-review prompts, conflict and CI-triage prompts, top-bar action prompts, stack-rebase prompts.
- **An Orca import writes into layer 3 (or 4/5 per project)** and never into the repo.
- **`icarus config explain <key>`** shows which layer a value came from, so an agent can see why something behaves the way it does.
- **This repo dogfoods the split.** The reference setup used to validate compatibility is a private overlay in gitignored `private/` and `scratch/`, applied over the generic defaults. Specs in `docs/` describe behaviour generically and cite it only as "the reference setup".

## 2. Architecture: a server, and a client that connects to many

The product is two programs:
- **A server** that does all the work: repos, worktrees, agents, terminals, automations.
- **A client** that only shows and controls it.

The server runs anywhere:
- on the user's own machine, started automatically by the client
- headless on another machine (a Linux box, a Mac mini, a cloud VM) with `icarus serve`

One client can connect to several servers at once, the way Orca does with paired hosts.

```
            ┌──────────── client (Zed soft fork, GPUI) ────────────┐
            │ sidebar across all servers · board · agent composer  │
            │ editor · diff / source control · PR review           │
            └──────┬──────────────────┬──────────────────┬─────────┘
       local socket│        SSH tunnel│    Tailscale/TCP │ (token-paired)
            ┌──────┴──────┐    ┌──────┴──────┐    ┌──────┴──────┐
            │ server:     │    │ server:     │    │ server:     │
            │ this laptop │    │ dev box     │    │ desktop     │
            └─────────────┘    └─────────────┘    └─────────────┘

server = sessions (PTYs + agents) · agent relay · hook endpoint · turn snapshots
         rules engine · GitHub/Linear pollers · ports · git/worktrees · files
         language servers · state store (SQLite) · CLI endpoint
```

**Connecting to a server** (the Orca and Zed remote-development pattern):
- **Local:** a Unix socket with a token file, as Orca's daemon uses.
- **SSH:** the client runs `ssh host icarus serve --stdio`, installing or upgrading the server binary on the host first if needed. This works over Tailscale SSH or any `~/.ssh/config` host, with no ports to open.
- **Direct:** `icarus serve --listen` on a Tailscale address. The client pairs once with a one-time code and keeps a per-client token after that.
- **One protocol for all three:** versioned RPC plus event streams. A client and server on different versions still negotiate, and an older server keeps working until it's upgraded.
- **The client keeps a list of servers.** The sidebar groups workspaces by server and project, and a server that drops off shows as disconnected and reconnects on its own. Nothing on a server stops when a client disconnects.

**Files and code intelligence run on the server, next to the repo.** The client never needs a local checkout:
- language servers
- git and diffs
- file search, quick open, formatting

Zed already splits things this way for remote development: a headless `remote_server` reached over SSH, with the editor as a thin view. So the Zed fork's editor works against a remote project as-is. Our server either embeds or launches Zed's remote server for the editor side; spike 1 decides which.

**The CLI can do everything the app can (required).** Agents must be able to control the whole application from a shell.
- **One API behind both.** Every setting and every action is a typed command on the server's API, and the client UI calls the same commands. No feature exists only in the UI.
- **Settings are schema-driven.** Each setting is declared once, with key, type, default, scope (global, per project, per client) and description. The settings UI and the CLI are both generated from that declaration:
  - `icarus config list|get|set|unset|schema`, with `--project` or `--client` scope
  - `icarus rules list|add|edit|rm|test`, `icarus statuses …`, `icarus prompts …`
  - `icarus settings export|import` (Orca format v1 and native)
- **Actions are commands too:**
  - workspaces, agents, terminals, notes
  - reviews, comments, checks, merges
  - layout and focus in a connected client: open file, show diff, switch workspace
- **Output is built for agents.** Every command takes `--json`, uses stable exit codes, and has `--help` with examples.
- **Agents find it automatically.** It's on `PATH` in every agent and terminal session, with the server address and workspace id in the environment.
- **A CI test enforces it:** every setting in the schema and every UI action must have a CLI path.

**Every action can be bound to a key (required).**
- **One action registry.** The command palette, menus, keybindings and the CLI all draw from it, and each action has a stable id (e.g. `workspace.next_action`, `review.send_queued_comments`).
- **Defaults match Orca's,** with every shortcut from features.md §5.
- **Remap from the app or a file.** A Shortcuts screen with search and conflict warnings, a user keymap file, and `icarus keys list|bind|unbind`.
- **Context-aware bindings,** e.g. different behaviour in the diff, the composer or a terminal. Zed's keymap system already supports these contexts.
- **Imports Orca's keybindings.** `~/.orca/keybindings.json` and the export's `keybindings` map onto the new action ids, and any that can't be mapped are reported (§4).
- **Also checked in CI:** every action in the registry must be bindable and must appear on the Shortcuts screen.

**The CLI talks to a server, never to the client.** It uses the local server by default, or `--server <name>`. `orca notes`, `orca worktree` and the rest work with no window open, including inside an agent on a remote server.

**What the server owns:** everything that has to outlive the window.
- **Processes:** PTY sessions with a headless emulator, so a snapshot can be replayed on reattach. Agent processes, spoken to over ACP through `claude-agent-acp`.
- **Agent lifecycle:** the hook endpoint and agent-turn snapshots (`refs/worktree/icarus-turns/...`), plus one warm, pre-spawned agent per active worktree.
- **Automation:** the rules engine and the providers it polls.
- **Data:** persistent state.
- **The fixes for two Orca gaps** (features.md §4.5). In Orca, turn events are lost while the app is closed, and the CLI doesn't work without the app. Here both live in the server.

**The app is a client:**
- It's a soft fork of Zed, with our features in new `orca_*` crates and only small patches to Zed itself. We rebase onto Zed regularly.
- Zed brings the editor, LSP, git diff and the agent thread UI (`acp_thread`, `agent_ui`).
- One patch: an "agent server" kind that connects to a server's agent relay instead of spawning a process. The thread view then attaches to a session the server keeps alive.

**Launching the local server** (as Orca does, features.md §4.1):
- a launchd job outside the app bundle (on Linux, a systemd user unit for `serve`)
- versioned socket, token and pid files
- the protocol version in the hello message
- an old server keeps serving its sessions after an upgrade

**Licence.** A Zed fork is GPL-3.0. That's fine for personal use or an open-source product, but it rules out a closed-source one. The server and CLI are our own code and can use any licence, unless the server embeds Zed's GPL `remote_server`. If closed source ever matters, the client would be rebuilt on GPUI and gpui-component (both Apache-2.0), with the server unchanged.

## 3. Automations: a general rules engine

**Managing statuses: a simple menu, not the board.** In Orca you have to open the Kanban board to create a status. Here:
- **A status menu** opens from the card's status chip, the sidebar, the palette or a hotkey. It sets the workspace's status, and right in the menu you can create, rename, recolour, change the icon of, reorder and delete statuses.
- **Deleting a status that rules point at** asks what to move its workspaces to and which rules to repoint.
- **The board is optional.** It's just one view of the same data.
- **Everything is on the CLI too,** for agents: `icarus statuses list|add|rename|edit|move|rm` and `icarus workspace status set <id|label>`.

**Model** (per project, editable in the UI and stored as data):
- **Statuses:** user-defined columns with stable ids (e.g. `todo`, `status-6`), label, colour and order. One column may be the *hold* column, which every rule skips.
- **Conditions:** named, ordered predicates over the workspace and its PR/issue. Examples: `pr.merged`, `pr.in_merge_queue`, `pr.mergeable == CONFLICTING`, `pr.approved_by_team("org/team")`, `pr.check_succeeded("name")`, `viewer.is_author`.
  - Today's 10 conditions ship as built-ins in today's order, first match wins.
  - A condition's outcome can be `move to column`, `skip` or `stop`, which reproduces today's behaviour for unmapped conditions (settings-compat §8.1).
- **Triggers:**
  - `tick`, `workspace.created`, `entered(column)`
  - `pr.head_moved`, `review.requested`, `review.withdrawn`, `viewer.reviewed`
  - `check.failed`, `user.action(name)`
- **Actions:**
  - `set_column`, `delete_workspace{force}`, `create_workspace_from_pr`
  - `launch_agent{agent, prompt_template, view, background}`
  - `run_command`, `sync_to_pr_head`
  - `open_layout`, `notify`, `log`
- **Guards** (all of today's are kept): the ledger of handled PRs, per-PR caps (CI triage: 3 per 7 days), "only if local HEAD equals PR head", and "never touch a workspace with uncommitted changes".
- **Providers:** GitHub is first (`gh` auth, GraphQL), Linear second. The code-review and issue sources are traits, so GitLab or Jira can be added later.

**An imported Orca setup becomes a preset in the user's settings layer.** The importer turns `ui.workspaceStatusRules`, the review inbox, CI triage and top-bar prompts into this model (settings-compat §8.3). The implicit behaviour in today's code becomes explicit rules. Examples: the review agent shared with the conflict agent, CI triage running outside the rules' on/off switch, head sync always on.

**Correctness check:** port the ~35 existing Orca test files for status rules and CI triage as fixtures (features.md §3) and run them against the preset.

**Team- and company-specific details are settings, never code.** Orca hardcodes several things that belong in settings:
- an approval team for a "PM approval"-style condition
- a merge-gate check name
- tool and branch names inside the CI-triage prompt
- a skill name in the stack-rebase prompt
- path-based "areas" for change filters
- the formatter and linter choice
- the pattern of the preview-deployment bot comment

All of these become ordinary values in the settings layers (§1b).

## 3-0. Context goes into the composer as attachments (required; borrowed from Conductor)

**One system replaces Orca's separate send paths:** diff notes sent to an agent, the queued PR comments with ⌥⌘S, follow-ups on notes, and "send unresolved comments".

**Layout: the composer is always at the bottom of the workspace** (as in Conductor; reference: `scratch/screens/conductor-hero-light.png`).

```
┌ sidebar ─┬ center: tab groups / splits (agents, terminals, browser, editor, diff) ┬ right ┐
│          │                                                                         │       │
│          │   everything sits on top of the composer, in Zed's pane tree            │       │
│          ├─────────────────────────────────────────────────────────────────────────┤       │
│          │ → Agent: "Fix CI on #123"  ▾        [chip] [chip] [chip]    ⌘L to focus │       │
│          │ prompt…                                                                  │       │
│          │ model · effort · permissions                              ＋  ↑ send    │       │
└──────────┴─────────────────────────────────────────────────────────────────────────┴───────┘
```

- **The composer is part of the workspace, not of a tab.** It spans the bottom of the center column, below the pane tree, so splits, terminals, the browser and the editor all sit above it and never cover it. It resizes with its content up to a cap, and the divider can be dragged.
- **The header row names the target agent** ("→ Agent: <thread title>") with a picker to switch. The attachment chips sit in the same row, and a hint shows the focus key (⌘L by default, bindable).
- **Agent threads are still tabs in the pane tree,** which show their transcripts. Sending from the bottom composer goes to the target agent, and focus moves to its tab if that tab is visible. A thread tab has no composer of its own, to avoid two inputs.
- **Hiding:** the composer can collapse to a one-line bar with a toggle, but never disappears. Attachments still land in it and show a count.

**There is always an agent:**
- Every workspace has at least one agent thread from creation on. The server pre-spawns it, warm (§2), so the composer is never targetless and the first send is instant.
- Closing the last agent thread immediately starts a fresh, clean one in the same workspace and makes it the target. Any composer draft and attachments carry over.
- Closing a non-last thread retargets the composer to the most recently used remaining thread.
- Starting a fresh thread on purpose ("New agent", bindable) also retargets the composer.
- Idle warm agents cost little. The server can park idle ones after a configurable time and wake them on focus or send. That's invisible to the user apart from a first-send delay.

**Behaviour:**
- **"Add to agent" never sends.** It appends the item to the composer of the workspace's **target agent**, as a small attachment chip above the input.
- **The target agent** is the agent thread you used most recently in that workspace. You can change the target from the composer or the "Add to agent" menu, and a thread can be pinned as the target.
- **You then write one prompt about all of them,** e.g. "these three are the same bug; fix in queue.py", and send once. The attachments are serialized into the message as structured context blocks (as Orca's note and comment prompts do today); the chips show what's included.
- **Each chip:**
  - shows its kind, source and a one-line preview (e.g. "PR comment · @octocat · queue.py:126", "Note · slide.py:40–52", "Check failure · e2e")
  - can be removed, and expands to show its full content
  - clicking it jumps to the source: the line in the diff, the thread, the check log
- **Attachments survive** switching tabs, restarts and pushes. They live on the server as part of the composer draft, keyed by PR and thread ids rather than commit shas. That fixes the "a push empties the queue" bug.
- **Sending marks the sources.**
  - Notes get `sent_at` and the agent's reply threads under them.
  - PR threads stay open and are marked "sent to agent" locally; nothing is posted on GitHub.
  - Sources edited after attaching show a "changed" dot on the chip.

**Attachable kinds** (one registry, so more can be added):
- diff notes and note threads
- PR review threads and comments, human or bot
- whole review summaries
- failed checks with their log excerpt
- files, and line ranges from the editor or a diff
- a turn's diff
- terminal output selections
- images and screenshots

**Shortcuts and bulk actions** (all are actions, so bindable):
- `agent.attach` on the focused item
- "Attach all unresolved" from the Comments panel, which follows the active audience tab, e.g. humans only
- `agent.focus_composer` jumps to the target agent's composer with the chips visible
- the old ⌥⌘S maps to "attach all queued and focus the composer"

**The CLI** for agents and scripts:
- `icarus attach <kind> <id> [--workspace] [--thread]`
- `icarus composer show|clear|send [--message "…"]`

**Automations use the same pipe.** For example, CI triage attaches the failed check and sends with its prompt.

**Tests:** attach, remove, reorder, persist across a server restart, serialization golden files for each kind, and source marking after send.

## 3a. Workspace cards: choose what they show (required)

As in Orca (features.md §2.2, `ui.worktreeCardProperties`), each part of a workspace card can be switched on or off.

**Built-in card fields:**
- status, unread
- linked issue, Linear title, Linear labels
- PR state, PR author avatar, PR diff `+N −M`
- stack position
- workspace status chip, project chip
- CI state, inline agents, ports, host or server

An Orca import carries the selection over from `worktreeCardProperties`.

**Ways to toggle them:**
- a "Card" menu in the sidebar header with checkboxes, plus reordering where it makes sense
- a hotkey per toggle
- `icarus config set ui.card.fields '[...]'`, or `icarus card show|hide <field>`

**General:**
- Fields come from a registry, so integrations add their own: GitHub adds PR fields, Linear adds issue fields, a future GitLab adds MR fields.
- A field whose provider isn't configured is hidden automatically.
- Settings are per client, so a laptop and a big screen can differ. A project can override them.

## 3b. Code editor and language tooling

**Keep everything the fork built** (features.md §2.10, FORK §26, §58, §60, §61, PRs 19–32, 42, 44, 103, 112, 159, 163, 177–184):
- Cmd+click go-to-definition (also from Peek, and into the target's diff when coming from a diff), hover, find references in Peek, back/forward history.
- Completion, auto-imports, quick fixes, organize imports.
- Diagnostics from linters and type checkers.
- Format on save.
- Restart language server.
- Language servers pick up files an agent just wrote.
- Git blame for the cursor's line.
- **Edit mode and diff mode as two views of one tab.** Switch with ⌥⌘E / ⌥⌘D, landing on the same line.

**General, file-based configuration instead of Orca's per-tool settings pane.** Use Zed's model, which the fork already gives us:
- **Settings files.** Language tooling lives in the user settings file and in a per-project file (`icarus.json`, committed to the repo, the way `.zed/settings.json` works).
  - `languages.<Lang>` picks the language servers, formatter, linters and format-on-save for that language.
  - `lsp.<server>` sets each server's binary, arguments, initialization options and settings.
- **Nothing is built in for one person's stack.** Tools like pyrefly, tsgo, oxlint, oxfmt and Ruff are entries in a server registry, not special cases. Adding a new tool is config, not code:
  - name, command, languages
  - how to find the binary: project `node_modules/.bin`, venv `bin/`, `PATH`, or an explicit path
  - capabilities
- **Sensible per-language presets** ship generic (e.g. Python with a type checker plus Ruff; TypeScript with tsgo, oxlint and oxfmt) and are opt-in. The importer translates Orca's `languageServers` and formatter settings into these files.
- **Agents configure it easily.** They can edit the JSON file, which hot-reloads, or use `icarus config set languages.Python.language_servers '["pyrefly","ruff"]'`.
  - Both are validated against the published schema, and errors show up in the app and in the CLI output.
  - `icarus lsp list|status|restart|logs` shows what's running per worktree.
- **The settings UI is a thin layer over the same files:** a searchable list of languages and servers, an "edit JSON" button, and live status per server.
- **Performance** (the slow Cmd+click in Orca, features.md §6.2): servers run on the server machine next to the repo and stay warm per project. Opening the target file is a buffer open in Zed's editor, with no IPC file copies and no editor rebuild.

## 3c. Terminals

**Engine: `alacritty_terminal`, the same as Zed.** It's Alacritty's VT parser and screen grid as a library (Apache-2.0). Zed's `terminal` crate wraps it, and `terminal_view` draws it on the GPU through GPUI. In Orca the same jobs are done by node-pty, xterm-headless on the daemon and xterm.js with WebGL in Electron.

**The split.** PTYs live on the server, so terminals survive the client:
- **Server, per session:**
  - Opens the PTY and spawns the login shell or command, with `TERM`, cwd, env and the shell integration for OSC 133/7.
  - Reads output on a dedicated thread and never blocks on clients.
  - Feeds every byte to a **headless `alacritty_terminal::Term`**. That keeps the exact screen, scrollback, modes and title, so the server can answer "what does the terminal look like right now" without a client.
  - Appends the raw bytes, numbered, to a bounded in-memory ring.
  - Writes scrollback checkpoints to disk for a cold restore after a reboot (Orca's model, features.md §4.3).
  - Derives facts from the stream: command finished, exit code, cwd, bell, URLs, ports printed. Automations and the CLI can use these with no client attached.
- **Client:** Zed's `terminal` and `terminal_view`, with a small patch so the byte source is a server stream instead of a local PTY. Everything Zed's terminal does comes for free:
  - GPU rendering, selection, search, links, hyperlinks (OSC 8), Kitty keyboard protocol, Option-as-Alt, splits
  - Zed's key mapping, plus Orca's macOS line-editing keys (features.md §2.5)

**Attach and reattach:**
1. The client asks for session X. The server replies with a snapshot of the current screen and scrollback, serialized from its `Term`, plus the next sequence number.
2. The client loads the snapshot into its own `Term`, then applies live bytes from that number on. There's no gap and no full replay.
3. After a network blip, the client resumes from its last sequence number if it is still in the ring; otherwise it takes a fresh snapshot.
4. Several clients can watch one session; input from any of them goes to the PTY. Resize follows the most recently active client.

**Why it's fast:**
- No Electron IPC hops and no JS terminal emulator.
- Rendering is a GPU text atlas, the same path as Zed's editor.
- Bytes stream over the socket in binary frames and are coalesced per frame on the client.
- The server's emulator runs off the client's critical path.
- Opening a terminal tab takes one RPC; the shell is often already warm.

**Dev servers** are ordinary sessions with a label. **Ports** are detected on the server by matching listening sockets to the session's process tree, plus URLs printed in the output (Orca's approach, features.md §4.5).

**Agents are not terminals.** Claude Code runs over ACP in the composer (§2). A raw-terminal view of an agent is optional and runs as an ordinary session.

**The CLI controls sessions too:**
- `icarus terminal list|create|read|send|wait|kill|split|rename`
- `read` returns the server's rendered screen or the scrollback as text, which is exactly what agents need.

**Spike 0 measures:**
- keystroke-to-glyph latency, locally and over SSH
- throughput (`cat` of a large file)
- reattach time with 20 sessions open

**Decision (2026-11-15): Alacritty, final.** Ghostty was preferred only if it were a plain swap. It isn't: Zed's `terminal_view` renders alacritty's grid directly, so Ghostty would mean writing our own GPUI terminal renderer. No abstraction layer for swapping emulators.

## 3d. Built-in browser (review layout opens the PR on GitHub)

**Verdict: feasible, moderate work, with known edges.** No Electron is needed. We embed the operating system's own web engine as a native child view inside the GPUI window:
- **macOS:** WKWebView, which is Safari's engine.
- **Linux:** WebKitGTK.

`wry`, the webview library behind Tauri, does exactly this, and **gpui-component already ships a `WebView` element built on wry** (feature `webview`). There's also a standalone `gpui-wry` crate. So this uses existing pieces; we don't have to write a browser.

**What we build:**
- **A "browser" tab type** in Zed's pane system, wrapping the gpui-component WebView. It provides URL, back/forward, reload, open-in-default-browser, and a context menu (Orca §18's right-click, links opening externally).
- **A persistent login.** It uses one persistent web data store per app profile, so you log into GitHub once and stay logged in.
- **Actions, hotkeys and CLI:** `browser.open <url>`, `icarus browser open|reload|close`. The review rule's `open_layout` action can say "agent left; first file and the PR page right", as today.

**Known edges, to handle in the spike:**
- **Drawing order.** A native view always draws above GPUI's own content, so menus, popovers, the palette and tab drags can't be drawn on top of the page. Orca hit the same thing with Electron's webview (fork §45, §47). The fix is the same: when an overlay opens or a drag starts over the page, hide the webview or swap in a snapshot of it, and route focus arrows back to GPUI.
- **Linux/Wayland.** Embedding a child webview is solid on X11 but limited on Wayland in wry. Check in the spike. The fallback on Wayland is a separate borderless window docked to the pane, or opening in the default browser.
- **Visual tests** can't capture the page. Test the tab with a stub page and test layout separately.
- **Not a full browser.** No extensions, devtools docking, cookie import or automation. Those were already marked [CUT?].

**Estimate:** a basic macOS tab that loads, stays logged in and fits the review layout takes a few days. Overlay and drag handling, a context menu and Linux/X11 add about a week. Wayland is an open question.

**Fallback if it disappoints:** open the PR in the default browser and tile it beside the app's window. That's a worse experience, so it's only the fallback.

## 3e. Explorer: Zed's project panel, as-is

**Every view is a tab type** (D31). Agent, browser, diff and explorer are items in Zed's pane tree, so they drag, split and stack exactly like files (for example one file on the left and two stacked on the right). The side panels stay available, but they're not the only home for these views.

The file explorer is Zed's `project_panel`, unchanged, plus an explorer tab type wrapping it.
- **Zed already covers Orca's explorer:** live tree, create/rename/delete/move, git-status colours, copy path, reveal the active file.
- **Icons:** Zed's icon themes cover the Material Icon Theme (fork §28) through the existing Material Icon Theme extension for Zed; no code of ours.
- **Remote servers:** Zed's remote project support covers repos on a remote server.
- **Our only additions are glue:**
  - it shows the active workspace's worktree
  - the right sidebar switches between Explorer and Source Control based on the active file's kind (fork §29)
  - its actions are in the action registry, so they can be bound to keys and reached from the CLI

## 4. Settings compatibility

- **Import:** reads `orca-settings.json` v1 losslessly, keeping unknown keys and exact number formatting (settings-compat §7).
  - Merge semantics match Orca: settings deep-merge, UI keys are replaced one by one, projects match by canonical remote `github.com/owner/repo`.
- **Export:** writes the same format byte-for-byte, so `~/.dotfiles/orca-settings.sh` keeps working against either app.
  - The native format is a superset (version 2). It adds what v1 lacks, notably the rules' project list. Without it, a restore on a new machine leaves the rules doing nothing.
- **Keybindings:** `~/.orca/keybindings.json` command ids map to the new commands, and unknown ones are reported.
- **Workspace state:** an optional one-time importer reads Orca's `profile-state.db` (statuses per workspace, notes, pending review drafts). Then switching apps mid-flight doesn't lose card positions.

## 5. v1 scope

Every [MUST] in features.md. In short:

**Workspaces:**
- Projects; create a workspace from a branch, PR or Linear issue.
- `orca.yaml` setup and archive scripts; delete or archive.
- Sidebar: grouping, including by stack, and card options; the board.

**Agents and notes:**
- Claude Code over ACP in the composer, background sessions, status, notifications.
- Agent turn snapshots, and the turn filter in Changes.
- Diff notes to the agent; `orca notes` for agent → diff.

**Source control and editor:**
- One live Changes list with filters, discard, the merge button, the diff viewer, and editor and diff as one tab.
- Editor with LSP go-to-definition, references, hover and Peek. Formatters and linters are configured per language.

**GitHub:**
- PR review mode: queued comments, verdict, diff base picker, reviewers, threads in the diff.
- Comments queued for the agent.
- Checks, CI triage, the stack map, merge-queue state.

**Automations and settings:**
- Rules engine with the generic presets and the Orca import, hold column, review inbox, conflict agent, top-bar next action.
- Settings import and export; keybindings.

**Platform:**
- The server, terminals for dev servers, ports.
- Remote servers over SSH or Tailscale, and one client connected to several servers. This replaces Orca's SSH worktrees, paired hosts and `orca serve`, which features.md had marked as cut.
- **macOS and Linux only, for both client and server.** Zed and GPUI already support both, and the server relies on Unix sockets, PTYs, launchd/systemd and SSH. No Windows support, ever, so no Windows abstractions in the code.
- CLI: `notes`, `worktree`, `terminal`, `agent`, `settings`.

**Cut from v1** (features.md §2.20):
- other platforms: mobile, cloud VMs managed by the app, and Windows/WSL permanently
- agents other than Claude Code and Codex (D6), though the agent layer stays pluggable through ACP
- browser automation, plugins, telemetry, i18n, voice
- other task providers

**Open question:** is the claude.ai web view still needed once a good composer exists? The recommendation is to drop it.

## 5b. Testing

**How Zed tests** (checked in zed-industries/zed, 2026-11-15). We adopt the same approach.
- **Runner:** `cargo nextest run --workspace`. CI runs only the changed crates and their reverse dependencies. The config is `.config/nextest.toml`: slow-test timeouts, slow tests run first, and DB tests serialized.
- **Unit and integration tests:** `#[gpui::test]` gives a `TestAppContext`:
  - a fake platform with no real window, but the real UI tree
  - a deterministic executor, so async work runs in a seeded, reproducible order
  - simulated time (`advance_clock`), so timers and debounces need no sleeping
  - `#[gpui::test(iterations = N)]` runs randomized tests, and `SEED=…` replays a failing order exactly
- **UI interaction tests:** `VisualTestContext` simulates keystrokes, clicks and actions against real views, then asserts on state. This replaces Playwright-style tests: no browser, no DOM, and everything runs in-process in milliseconds.
- **Fakes, all in-process:**
  - `FakeFs`
  - a fake git repository
  - `FakeLanguageServer`, which scripts LSP responses
- **Visual regression:** a separate macOS-only binary (`zed-visual-test-runner`, feature `visual-tests`).
  - It renders real windows off-screen through Metal, with deterministic scheduling.
  - It compares screenshots against baseline PNG images; `UPDATE_BASELINE=1` re-records them.
  - It needs no screen-recording permission.
- **Static checks:** clippy, rustfmt, typos, a keymap checker, and licence checks.

**Our test layers:**
1. **Server unit tests** (plain `cargo test`), with no UI:
   - **Rules engine:** today's Orca status-rule and CI-triage tests become table-driven fixtures. Input: a PR snapshot plus workspace state. Output: the expected actions. Every imported preset must pass all of them.
   - **Settings:** `orca-settings.json` import and export round-trip byte for byte. The public fixtures are synthetic exports that cover every field, enum value and edge case (absent `holdStatus`, stale keys, non-ASCII prompts).
   - **The reference setup as a private test:** an extra test reads `private/` or `scratch/` when present and checks that the real export round-trips and the rules fixtures pass. It skips itself when those files are absent, as in public CI.
   - **Filters:** a golden test classifies a fixture repo, checking every rule in the preset.
   - **GitHub:** recorded GraphQL/REST responses (fixtures from the real API, with secrets stripped) for every status in github-panel.md.
2. **Server integration tests,** running real processes in temporary directories:
   - real `git` worktrees
   - real PTYs: spawn, write, read the rendered screen, kill and reattach
   - a **fake ACP agent**, a scripted binary that emits tool calls, diffs, permission prompts and long turns, so agent flows test without Claude or the network
   - multiple clients on one server; client crash mid-turn, then resume from a sequence number
2b. **The `orca` CLI as a test harness:** most end-to-end flows are CLI scripts against a real server (create workspace, launch fake agent, add note, change status, assert JSON). This doubles as the check that everything is reachable from the CLI.
3. **Client tests:** `#[gpui::test]` with a `VisualTestContext`, connected to an in-process server over an in-memory transport. They test keybindings, the status menu, the composer, the diff panel, the filters and the stack map, with deterministic time and randomized iterations for the tricky ones (reconnect, live updates).
4. **Visual regression:** Zed's visual test runner pattern for key screens (sidebar, composer, diff with filters, stack map, review), on macOS in CI.
5. **Perf benchmarks** (`criterion`, plus timing harnesses run in CI and compared against a stored baseline) for the §8 budgets: agent start, Cmd+click, workspace switch, terminal throughput and latency, reattach with 20 sessions.
6. **Remote:** the same integration suite runs with the server in a Linux container reached over SSH. That covers Linux and the remote path in one go.
7. **Coverage checks** (§2): every setting and action reachable from the CLI and bindable to a key, and every keybinding id valid.

**CI: fail fast, then shard** (files: `mise.toml`, `rust-toolchain.toml`, `moon.yml`, `.moon/`, `.config/nextest.toml`, `lefthook.yml`, `_typos.toml`, `.github/workflows/ci.yml`). Modelled on a production mise + moon monorepo setup:
- **Tooling:** mise pins every tool; moon defines the check phases; lefthook runs `fix-phase-1` on commit, so most phase-1 failures never reach CI.
- **Phase 1, "rapid checks"** (`moon ci root:check-phase-1`, about a minute, no compilation): rustfmt, taplo, typos, actionlint. This is its own job, and everything else waits on it, so a format or typo error fails CI almost immediately.
- **Phase 2** (`check-phase-2`): clippy with `-D warnings`, which also catches type errors, plus cargo-deny.
  - It runs in parallel with the test build, not before it.
  - **If it fails, it cancels the whole workflow run** (`gh run cancel`), so a lint error doesn't wait for the test shards.
- **Tests:** build once per OS with `cargo nextest archive`, then shard the run with `--partition slice:k/N`: 4 Linux shards and 2 macOS shards to start, matching nextest's official reuse-build example. Shard counts are env vars in the workflow. Raise them as the suite grows; public repos get free standard runners on both OSes.
- **One required status check, `ci ok`,** for branch protection and the merge queue.
- **Pull requests:** in-flight runs are cancelled on a new push. Caches are saved only from `main` and restored on PRs.
- **Before there's Rust code,** the Rust jobs skip themselves, so CI is green from the first commit.
- **Later, as the suite grows:**
  - run only affected crates on PRs (nextest `rdeps()` filters, as Zed does)
  - a macOS visual-test job and a Linux SSH-remote job
  - a nightly coverage and mutation job

**Public repo hygiene:**
- `scratch/` and `private/` are gitignored. They hold the reference setup: the real settings export, screenshots and notes.
- Docs and fixtures in the repo use neutral example values only.
- **A phase-1 check fails CI** if a denylist of private terms appears in tracked files. The denylist itself lives in `private/` and is skipped in public CI, so it's a pre-commit guard via lefthook.

**Coverage policy: no feature without tests, no fix without a regression test.**
- **Every feature has an id in features.md** (e.g. `SC-UNCOMMITTED`, `RULES-HOLD`, `STACK-AUTOMERGE`). Its tests carry that id in their name or an attribute.
- **CI checks the mapping both ways:** a `[MUST]`/`[SHOULD]` feature marked as built but with no tests fails the build, and so does a test pointing at an unknown id. The features.md table doubles as the coverage report.
- **Definition of done for a feature:**
  - tests at the lowest layer that can catch the bug: engine logic in unit tests, process behaviour in integration tests, UI wiring in client tests
  - at least one CLI end-to-end flow
  - a visual baseline if the feature is a screen
- **Every bug fix starts with a failing test** that reproduces it. That test stays.
- **Line coverage** (`cargo llvm-cov`) is reported per PR. New and changed lines in the server and rules engine must be ≥ 90% covered; elsewhere coverage is a signal, not a gate.
- **Mutation testing** (`cargo-mutants`) runs nightly on the rules engine and settings importer. Those two are where a silent regression hurts most.
- **No flaky tests:** a test that fails nondeterministically is fixed or quarantined with an issue within a day. Deterministic executors and fake time make this realistic.
- **Every PR runs the full fast suite.** Visual, remote/Linux and benchmark suites run on PRs that touch their area and nightly on `main`.

## 6. Milestones

Risky spikes come first, so we find out early if the architecture holds.

0. **Spike: agents survive the app, locally and remotely.**
   - The server runs the user's `claude` binary in stream-JSON mode behind the `icarus_claude_bridge` (D19). A minimal GPUI client chats through the relay over ACP.
   - The bridge maps:
     - text, thinking, tool calls and results
     - Edit/Write as diffs, TodoWrite as a plan
     - images
     - permission prompts, via `--permission-prompt-tool` against an MCP tool the server hosts
     - resume and fork
   - Checks: the same map for Codex, the `statusLine` rate-limit feed for the meters, and that nothing touches credentials.
   - Kill the client mid-turn, reopen it, and the thread replays and continues.
   - Repeat with the server on another machine, reached over SSH or Tailscale.
   - Also measure cold and warm agent start times against Orca.
1. **Spike: the Zed fork.** The fork builds; the agent panel's thread view runs against the relay; we confirm the patch to Zed is small.
2. **Server core.**
   - PTY sessions with reattach, the state store, the hook endpoint, turn snapshots, the CLI.
   - `serve` mode (`--stdio`, `--listen`), pairing, and a multi-server client.
3. **Workspaces.** Projects, worktree create/delete, `orca.yaml`, sidebar, board, Changes list, diff, notes.
4. **Rules engine.** GitHub provider, the Orca importer (synthetic fixtures public, the reference setup checked privately), Orca's tests ported and green, review inbox, CI triage.
5. **PR review.** Queued comments, verdict, threads, checks, stacks, merge queue.
6. **Polish to daily-driver.** Linear, quick open and commands, ports and preview, keybindings import, settings export, updater.

At each milestone, use the new app for the part it covers while Orca handles the rest. They don't conflict: separate state, same repos.

## 7. Open decisions

1. **Licence.** A GPL Zed fork, or permissive-only (much more work)? The repo will be public.
2. **GitHub access.** Keep using the `gh` CLI's auth, as Orca does, or add an app token?
3. **State import.** Import Orca's workspace state (`profile-state.db`), or settings only?

## 7b. Decisions log

| # | Decision |
|---|---|
| D1 | **Name: Icarus.** The repo is `frbarbre/icarus` (scaffolded from a Backstage minimal template with TechDocs; these docs are its TechDocs pages). The CLI is `icarus`, the URL scheme is `icarus://`, and the app id is TBD. |
| D2 | **A new CLI, not Orca-compatible.** The plan includes writing new versions of scripts and agent skills that call `orca …` (worktree create/set, terminal, notes, open, settings export/import) and migrating them to `icarus …`, with a checklist per script and skill. |
| D3 | **Opening from an issue tracker:** <ul><li>an `icarus://open?issue=…` URL scheme</li><li>`icarus workspace create --from-linear <id> --prompt …`, which drops the prompt into the new workspace's composer with no clipboard step</li><li>worktree-name rules (max length, DNS-safe for `<slug>.localhost` dev URLs) as settings</li></ul> |
| D4 | **Project config files are Icarus's own.** <ul><li>`icarus.json` is committed to the project.</li><li>`icarus.local.json` is untracked and needed for nothing, so a project works without committing anything.</li><li>Both support the same scripts as Orca's `orca.yaml`: setup and archive scripts with their run policies, shared directories, include files.</li><li>The importer converts an existing `orca.yaml` once; Icarus never reads `orca.yaml` at runtime, to avoid confusing the two apps.</li></ul> |
| D5 | **Agents run on the user's subscription login, done exactly as Orca does it,** with the usage meters. API-key auth may exist but isn't the path. Details: §7c, once researched. |
| D6 | **Agents in v1: Claude Code and Codex.** Both over ACP. Other ACP agents later, through the same slot. |
| D7 | **Notifications come from the server** (turn done, waiting for permission, CI failed, review requested, PR merged, …), so none are lost while the client is closed. The client shows them with sound, dock badge and per-event toggles. They're also available to rules (`notify`) and the CLI. |
| D8 | **Session history and sleep/wake.** Past agent threads are stored by the server, searchable from the palette and the CLI, and resumable. Workspaces can sleep (park agents and terminals) and wake. |
| D9 | **Updates work one-to-one like the fork's update channel (FORK §1), with no Apple Developer account.** <ul><li>Releases are GitHub releases with a manifest and sha512.</li><li>The update downloads in-process, which avoids the quarantine attribute.</li><li>Before installing it backs up settings, swaps the app bundle, keeps `Icarus.app.previous` and relaunches.</li><li>The first install needs the usual one-time "allow" in macOS settings.</li><li>The server binaries, local and remote, are updated to match by the client (over SSH for remote ones).</li><li>A Homebrew cask and a Linux tarball/AppImage ship too.</li></ul> |
| D10 | **Themes convert on import.** The VS Code editor theme and the Warp terminal theme become Zed theme files, and fonts, line height and inactive-pane opacity carry over. Also available as `icarus theme import vscode|warp <file>`. |
| D11 | **Voice dictation is cut from v1.** |
| D13 | **Multiple accounts per provider, like Orca:** switchable from the status bar and the CLI, with meters per account. Within the limits in §7c. |
| D14 | **Where a worktree lives decides who manages it.** Worktrees under Orca's workspace root belong to Orca, and Icarus never runs automations on them. Worktrees under Icarus's root belong to Icarus. Both apps can show both, but only the owner acts. |
| D15 | **No telemetry.** Crashes and server panics are written to local log files and offered as a pre-filled GitHub issue. `icarus doctor` collects diagnostics. |
| D16 | **Licence: GPL-3.0.** It's a Zed soft fork, and the repo is public. |
| D17 | **GitHub auth through `gh`'s login,** as in Orca, with an optional token setting for servers without `gh`. |
| D18 | **The Orca import is settings only.** Orca's workspace state isn't imported; existing worktrees finish their life in Orca (D14). |
| D19 | **Subscription users: Icarus drives the user's own, unmodified `claude` binary** in headless stream-JSON mode, not the Agent SDK (§7c). An `icarus_claude_bridge` crate in the server translates stream-JSON into ACP for Zed's agent UI, which stays unchanged. API-key users may also use the ACP adapter. |
| D20 | **No rewind or fork in v1.** Threads are linear. |
| D21 | **Claude Code's own Remote Control is exposed per thread.** A toggle starts the session with Remote Control and shows the claude.ai link, so a session can be followed from a phone. There's no Icarus mobile app. Spike 0 checks that Remote Control works with the headless stream-JSON bridge. If it doesn't, the toggle runs that thread as an interactive `claude` session instead. |
| D22 | **Permissions are bypassed by default** (`--dangerously-skip-permissions` or the agent's equivalent) for everyone. It's configurable per user, project and thread, and the permission cards still work for anyone who turns bypass off. |
| D23 | **Media in GitHub comments is a MUST** (fork §15, github-review.md): <ul><li>drag-and-drop, paste and file-pick of images and videos into every GitHub comment box</li><li>uploaded through GitHub's own user-attachments endpoint with the `gh` login</li><li>rendered as previews in the box and in threads</li></ul> It also covers the composer: images attach to agent prompts the same way. |
| D24 | **The next-action button** (FORK §13; `src/shared/workspace-action.ts`) sits top right and shows the one next step for the workspace. Its hotkey is `workspace.run_next_action`, default **Mod+Alt+Enter** as in Orca. See §7d. |
| D25 | **The composer reaches Zed's agent UI through `icarus acp`.** It's an ACP agent on stdio, which Zed starts as a custom agent server (`agent_servers.Icarus`), backed by sessions the Icarus server owns. That needs no patch to Zed's agent crates: closing Zed ends only the bridge, `session/load` replays history, and `session/list` powers thread import. Icarus's own UI lives in `icarus_*` crates inside the Zed fork, starting with `icarus_ui`. |
| D26 | **Release builds use Xcode's Metal compiler** via `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`. Dev builds may use `gpui_platform/runtime_shaders`, which needs no Xcode. |
| D27 | **"Open in editor" from dev tools.** `icarus file open <path>[:line[:col]] [line] [col]`, and `scripts/icarus-open` as a `LAUNCH_EDITOR` target. The server forwards the request only to the app's own connection (`icarus-app`). The app opens the file in the window whose worktree contains it, at the position, and brings it forward. Any tool that uses `launch-editor` (TanStack DevTools, Vite, React DevTools) works. |
| D28 | **Appearance follows the system** by default (`theme.mode: "system"`, a light and a dark theme). All Icarus UI uses theme colours only, and visual tests cover both modes. |
| D29 | **Ports and services, detected for everyone.** The server lists each workspace's listening ports and attributes them to the worktree by process cwd, by ancestry from its terminals and agents, and by URLs printed in terminal output. Each port is classified from the command line (Vite, Next, Storybook, Django, uvicorn, Rails, Postgres, Redis…) and a short HTTP probe (`/@vite/client`, `__next`, JSON or OpenAPI) as frontend, API, docs, Storybook, database, cache or other. Optional project settings refine this: `ports.labels_command` (Orca's `portLabelsCommand`, e.g. a print-URLs script, imported as is) and `ports.rules`. "Open frontend" (⌘⇧B), card port chips, and `icarus ports list\|open --kind frontend` all use `ports.primary`. Preview deployments are covered separately (D30). |
| D30 | **Preview URLs come from GitHub Deployments first.** It's the standard that GitHub Actions environments, Vercel, Netlify, Cloudflare Pages and others publish to.<ul><li>Deployments for the PR head (or its branch) are grouped by preview-like environment (`previews.environment_pattern`, production excluded). Each one's latest status gives its `environment_url` and state.</li><li>Fallbacks: preview-provider commit statuses and check runs, then PR bot comments matched by a heuristic or a configurable `previews.comment_pattern`.</li><li>Each preview has an app label (`frontend-preview` → frontend) and is flagged when it's behind the PR head.</li><li>The primary preview (`previews.primary_app`, else frontend/web/app) powers ⌘⇧P and `icarus pr preview`.</li></ul> |
| D32 | **Layouts are named tab arrangements** (`layouts.<name>` in settings): a split tree (`split: row\|column`, `children`, optional `sizes`) whose leaves are panes of tabs (`agent`, `browser`, `diff`, `explorer`, `file`, `terminal`). Browser URLs take placeholders (`{pr_url}`, `{preview_url}`, `{frontend_url}`, `{workspace_path}`). Built-ins (overridable by name): `default`, a single agent tab, which every new workspace opens with unless another layout is named; and `review`, an agent tab on the left and a browser on the PR on the right. A terminal is a tab like any other. `icarus workspace create --layout <name>`, `icarus workspace layout <ws> <name>` and the rules' create-from-PR/open-layout actions store the layout on the workspace and push it to the app; an app that opens later applies a pending layout once. Agent tabs bind to server-owned agents (an agent tab without a prompt binds to the workspace's newest agent, so the automation's review agent sits in the left tab). |
| D31 | **Agent, browser, diff and explorer are tab types** in Zed's pane tree, draggable and splittable like files. Each agent is a tab showing only its conversation; one composer, always visible at the bottom, targets the active (or last used) agent tab and keeps a separate draft and attachments per agent. |
| D12 | **Linear stays minimal:** <ul><li>the issue title on workspace cards</li><li>issue details in the card hover</li><li>a shortcut to open the issue</li><li>create a workspace from an issue (D3)</li></ul> No Tasks drawer and no Linear UI re-implementation. Everything else (status changes, comments, search) is left to agents through the Linear MCP. The `orca linear …` CLI is not carried over. |

## 7c. Agent accounts, auth and usage: mirror Orca (D5)

How Orca does it today (paths under the fork's `src/`):
- **Claude auth is the Claude Code CLI's own subscription login** (keychain or `.credentials.json`). Orca never runs its own OAuth.
  - Sign-in runs `claude auth login --claudeai`, or the pasted-code variant.
  - Multiple managed accounts each get their own profile home, `<userData>/claude-profiles/<id>/home`. Launches select one with `CLAUDE_CONFIG_DIR` plus pointer env vars, and only the auth is swapped, so chat history stays shared. (`main/claude-accounts/*`)
  - Before launch, the API-key and token env vars (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, …) are stripped, unless the user accepted a key in the CLI.
- **Usage meters** show the session window and weekly window. Orca reads them from three places:
  - `GET https://api.anthropic.com/api/oauth/usage` with the CLI's OAuth token, polled every 15 minutes with backoff
  - a managed Claude `statusLine` script that POSTs the CLI's `rate_limits` payload to the hook server at most every 15 s
  - SDK `rate_limit_event` messages

  (`main/rate-limits/*`)
- **Codex:**
  - Login runs `codex login` or `codex login --device-auth`. Both ChatGPT login and API-key modes are accepted.
  - Managed accounts each get their own `CODEX_HOME`, plus a runtime home that holds the active account's `auth.json` for hot-swapping.
  - Status comes from hooks in `<home>/hooks.json`.
  - Native chat drives a `codex app-server` JSON-RPC subprocess and resumes with `thread/resume`.
  - Usage comes from a `codex app-server` probe, with a fallback to `chatgpt.com/backend-api/wham/usage`.
- **Native Claude chat** uses `@anthropic-ai/claude-agent-sdk` in Electron's main process. The SDK spawns the real `claude` CLI, and Orca owns the spawn and the env. Resume, rewind and fork use the SDK's `resume`, `resumeSessionAt` and `forkSession`.

**Constraint: Anthropic's terms for subscription use** (code.claude.com/docs/en/legal-and-compliance, read 2026-11-15). This overrides "one-to-one with Orca" wherever the two conflict.
- **Subscription (OAuth) login is for ordinary use of Claude Code** and Anthropic's own apps.
- **Developers building products,** "including those using the Agent SDK", should use API keys.
- **Not permitted:**
  - offering Claude.ai login inside a third-party app
  - routing requests through a user's Free/Pro/Max/Ultra credentials on their behalf
  - collecting, storing or intermediating Claude.ai credentials or session tokens. Sign-in must complete through Anthropic's own flow.
- **Explicitly allowed:** an end user signing in to the **unmodified Claude Code binary** with their own subscription, including where a product runs Claude Code. The binary must be unmodified, and its built-in auth methods must not be removed or restricted.
- **Name use:** a product may say in plain text that it "runs Claude Code", but may not use Anthropic's or Claude Code's names or logos in its own branding.

**What that means for Icarus:**
- **Subscription users run the user's own, unmodified `claude` binary,** driven over its headless stream-JSON mode, never the Agent SDK. They sign in through Claude Code's own login flow, run by the user. Icarus never reads, copies, stores or forwards OAuth tokens or credential files.
- **Orca does a few things Icarus must not copy:**
  - reading the OAuth token to call the usage endpoint
  - copying `auth.json` and credentials between account homes
  - a "pasted code" login inside the app
- **Usage meters come only from what Claude Code itself hands out:** the `rate_limits` payload Claude Code passes to a user-configured `statusLine` command, and rate-limit events in its stream output.
- **Multiple accounts** are separate `CLAUDE_CONFIG_DIR`s. The user signs in to each one through Claude Code's own `/login`. Icarus only chooses which directory a launch uses.
- **API-key users** (Claude Console or a cloud provider) can use any transport, including an ACP adapter built on the Agent SDK.
- **Codex:** OpenAI's terms for ChatGPT-login use in third-party tools still need the same check before relying on `codex app-server` with a ChatGPT login.
- **This is a reading of the published docs, not legal advice.** Before publishing Icarus, confirm the intended subscription path with Anthropic through the contact on that page.

## 7d. The next-action button (D24)

**One button at the top right, plus a hotkey (Mod+Alt+Enter),** always shows the single next step for the active workspace. It's hidden on the default branch and when nothing is left to do.

**Resolution order, as in the fork, first match wins:**
1. **A local merge or rebase stopped on conflicts:** *Resolve conflicts.*
2. **No PR yet,** once the PR lookup has actually answered: *Create PR*. Its prompt commits and pushes too.
3. **Uncommitted changes or unpushed commits:** *Commit & push.*
4. **The open or draft PR conflicts:** *Resolve conflicts.*
5. **The PR is a draft:** *Ready for review.*
6. **The PR is open and its head is the local commit:** what GitHub allows. That's *Merge*, *Turn on auto-merge* or *Turn off auto-merge*; *Add to merge queue* for queue-protected bases; and *Merge stack* or *Auto-merge stack* for a stacked PR (github-panel.md R4).

**What a click or the hotkey does:**
- **Agent steps** (Create PR, Commit & push, Ready for review, Resolve conflicts) **start a new agent session every time,** as in Orca. It's never the current thread. The session uses whichever agent provider is configured for that step: the step's own setting, else the project's or user's default agent. That can be Claude Code, Codex, or any ACP agent added later.
  - The new thread opens as its own tab, titled after the action (e.g. "Ready for review").
  - It gets the action's prompt from settings, filled with the PR and branch variables. Create PR also gets the PR template and draft defaults.
  - It becomes the composer's target agent (§3-0), so follow-ups go to it.
- **GitHub steps** (merge, auto-merge on/off, enqueue, stack merge) **call GitHub directly,** with no agent. They show a confirmation when the step can't be undone, and a toast with the result.

**Settings and CLI:**
- Each step's prompt, agent (Claude Code or Codex), model and permission mode is a settings value. That includes the four imported from Orca's `actionPrompts`.
- Steps and the resolution order are data. A project can add its own step, e.g. "Deploy preview", with a condition and an action from the rules engine's vocabulary, or hide one.
- `icarus workspace next-action [--json]` prints the current step and why. `icarus workspace next-action --run` runs it.
- The Review button for PRs, with its own hotkey, sits beside it as in Orca.

**Tests:** the fork's `workspace-action.test.ts` and `workspace-action-prompts.test.ts` become table-driven fixtures, plus a client test that the hotkey starts a new thread and retargets the composer.

## 8. Perf budgets (measured on the same machine against Orca)

| Interaction | Target | Today's slow path (features.md §6) |
|---|---|---|
| New agent ready for input | < 300 ms warm, < 2 s cold | fetch + gh lookups → login shell → `~/.claude.json` rewrite → typed launch |
| Cmd+click to another file | < 50 ms (server warm) | whole file over IPC twice, LSP cold start per worktree, Monaco model rebuild |
| Switch workspace | < 16 ms frame | |
| Reopen app with 20 live sessions | < 500 ms to interactive | |

## 9. Importing a live Orca setup

- **Export from the machine where the automations actually run.** Profiles on different machines can differ. One may have the rules enabled with no projects selected, which makes them inert. Where the reference setup came from is recorded in `private/`.
- **The export doesn't carry the project scope** (`repoIds`). The importer must ask which projects the rules apply to, or the native format must carry it (§4).
