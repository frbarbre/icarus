# CLI reference

`icarus` talks to the Icarus server. Anything the app does, the CLI can do, so scripts and agents can drive Icarus too. `icarus <command> --help` shows every option.

## Global options

| Option | What it does |
|---|---|
| `--json` | Print machine-readable JSON instead of text. Works on every command. |
| `--server <name>` | Talk to a saved server (see `icarus remote list`) instead of this machine's. |
| `--home <dir>` | Use another server state directory (default `~/.icarus`, or `ICARUS_HOME`). |

Commands that work on a workspace take its id, name or path. `git`, `notes` and `review` default to the workspace containing the current directory (or `ICARUS_WORKSPACE_ID`, which agents' shells have set).

## Commands

### Server and connections

| Command | What it does |
|---|---|
| `server start [--listen ADDR] [--allow-public]` | Start the server in the background. |
| `server stop` | Stop the server and everything it runs. |
| `server restart` | Restart without stopping anything: terminals keep running, agents sleep and resume. |
| `server status` | Whether the server is running. |
| `server install` / `uninstall` | Start the server at login (launchd or a systemd user unit), or stop doing so. |
| `server pair [--ttl SECS]` | Print a one-time pairing code. |
| `server clients` | List paired clients. |
| `server revoke <id>` | Revoke a paired client. |
| `remote add <name> <address> [--code] [--fingerprint] [--ssh HOST] [--command]` | Save a server: a paired address, or `ssh://host`. |
| `remote list` / `remove <name>` | List or forget saved servers. |
| `update check` | Compare the running version with the newest release. |
| `update install [--force]` | Install the newest release and restart the server. |

### Projects and workspaces

| Command | What it does |
|---|---|
| `project add <path> [--name] [--base]` | Add a git repository. |
| `project list` / `remove <id>` | List or remove projects. |
| `project scripts <id>` | Show the repository's committed scripts and whether they're trusted. |
| `project trust <id> [--always]` / `untrust <id>` | Trust or untrust them. |
| `workspace create --project <p> [name] …` | Create a workspace. See [Workspaces](workspaces.md#from-the-shell). |
| `workspace adopt <path> [--project <p>] [--name <n>]` | Register an existing worktree as a workspace. See [Workspaces](workspaces.md#adopt-an-existing-worktree). |
| `workspace list [--project]` / `show <ws>` | List or show workspaces. |
| `workspace set <ws> [--status] [--title] [--base] [--pin\|--unpin]` | Change a workspace. |
| `workspace reorder <ws>...` | Put workspaces at the top of the Manual sort. |
| `workspace mark-read` / `mark-unread <ws>` | Clear or set the unread mark. |
| `workspace setup <ws> [--trust]` | Run the setup script again. |
| `workspace layout <ws> <layout>` | Rearrange the workspace's tabs. |
| `workspace changes <ws> [--uncommitted]` | Changed files against the base. |
| `workspace turns <ws>` | Recorded agent turns. |
| `workspace next-action <ws> [--run]` | Show or run the next step. |
| `workspace sleep` / `wake <ws>` | Park and restart a workspace. |
| `workspace remove <ws> [--force] [--keep-branch] [--skip-archive] [--allow-failed-archive]` | Remove a workspace. |
| `layout list [--project]` / `show <name>` | Tab layouts. |

### Agents

| Command | What it does |
|---|---|
| `agent create [--workspace] [--cwd] [--provider] [--prompt] [--model] [--account] [--title] [--ask-permissions] [--resume] [--remote-control on\|off]` | Start an agent. |
| `agent list` | List agents. |
| `agent send <id> <text>` | Send a message. |
| `agent read <id>` | Print the transcript. |
| `agent wait <id> [--timeout-ms]` | Wait for the current turn to end. |
| `agent interrupt <id>` | Stop the current turn. |
| `agent kill <id> [--remove]` | End the session. |
| `agent history [--workspace] [--search] [--limit] [--before]` | Past sessions. |
| `agent resume <id>` | Bring a past session back. |
| `agent remote-control on\|off [<id>] [--workspace]` | Claude Code's Remote Control. |
| `account list` / `add <provider> <label>` / `use <id>` / `remove <id>` | Agent accounts. |
| `attach comments` / `check` / `file` / `text` / `turn <ws> …` | Attach context to a workspace's composer. |
| `composer show <ws> [--all]` / `clear <ws>` | Pending attachments. |
| `notes add <path:line[-end]> <body>` | Add a note. |
| `notes list [--path] [--all]` / `show <id>` | List or show notes. |
| `notes reply <id> <body> [--wait]` | Reply; a short-lived agent answers. |
| `notes edit` / `rm <id> [--reply]` / `resolve` / `unresolve` / `attach <id>` | Manage notes. |
| `acp [--provider]` | Speak the Agent Client Protocol on stdio (the app's agent tabs use this; other ACP editors can too). |

### Terminals

| Command | What it does |
|---|---|
| `terminal create [--workspace] [--cwd] [--label] [-- cmd…]` | Start a terminal (a login shell without a command). |
| `terminal list` | List terminals. |
| `terminal read <id> [--scrollback]` | Print the screen or all output. |
| `terminal send <id> <text> [--enter]` | Type into it. |
| `terminal wait <id> [--contains TEXT] [--exit] [--timeout-ms]` | Wait for output or exit. |
| `terminal resize <id> <cols> <rows>` | Resize. |
| `terminal rename <id> [title]` | Set or clear the tab title. |
| `terminal kill <id> [--remove]` | Kill its process. |
| `terminal attach [<id>] [--create …]` | Show a server terminal in this terminal. |
| `ports list <ws>` | Services the workspace is running. |
| `ports open <ws> [--kind] [--browser]` | Print or open a service URL. |

### Git, review and GitHub

| Command | What it does |
|---|---|
| `git status` | Branch, upstream, ahead/behind. |
| `git stage` / `unstage` / `discard <paths>…` | Stage, unstage or throw away changes. |
| `git commit [-m] [--amend] [-a]` | Commit. |
| `git push [--force-with-lease]` / `pull` / `sync` | Push, pull, or both. |
| `git commits [--base] [--limit]` / `commit-files <sha>` | Commits on the branch, and a commit's files. |
| `changes list <ws> [--scope] [--hide] [--text]` | The Changes list with file types. |
| `changes explain <ws> <path>` | Why a file got its type. |
| `changes filter <ws> [--hide] [--show] [--reset]` | Saved hidden file types. |
| `review show` / `threads` / `base` / `summary` / `submit` / `people` | The pending review. See [Review](review.md#from-the-shell). |
| `review comment add` / `list` / `edit` / `rm` / `clear` / `edit-published` / `delete-published` | Review comments. |
| `review reply` / `resolve` / `unresolve` / `react` / `upload` | Threads, reactions, media. |
| `pr status <ws> [--refresh]` | The workspace's pull request. |
| `pr previews <ws>` / `preview <ws> [--app] [--browser]` | Preview deployments. |
| `issue show` / `open` / `search` / `refresh` / `status` | Linear issues. |
| `rules status` / `tick [--dry-run]` | Automations. |
| `notifications [--unread] [--limit]` / `read <id>\|--all` / `clear` | Notifications. |

### Settings and the app

| Command | What it does |
|---|---|
| `config get [key]` / `set <key> <value>` / `unset <key>` `[--layer] [--project]` | Read and change settings. |
| `config explain <key>` | Which layer a value comes from. |
| `config schema` | The settings JSON Schema. |
| `settings import-zed` | A Zed install's setup into the app. |
| `theme import vscode\|warp <file> [--select]` | Convert a colour theme. |
| `file open <path[:line[:col]]> [--focus]` | Open a file in the window that owns its worktree. |
| `app dispatch <action> [--args JSON]` | Run an app action in the frontmost window. |
| `app config get` / `set` / `unset` | The app's own `client.json`. |
| `app capture [--out DIR]` | Save each app window as a PNG. |

## Examples

Start a task from a Linear issue and wait for the agent:

```sh
icarus workspace create --project my-repo --from-linear ABC-123 --prompt "Implement ABC-123" --json \
  | jq -r '.agents[0].id'
icarus agent wait <agent>
icarus workspace changes abc-123
```

Feed failing CI back to the agent:

```sh
icarus attach check fix-login "ci / test"
icarus agent send <agent> "CI is failing; the log is attached. Fix it."
```

Run a dev server in a workspace and wait until it's up:

```sh
id=$(icarus terminal create --workspace fix-login --cwd ~/icarus/workspaces/my-repo/fix-login --label dev -- pnpm dev)
icarus terminal wait "$id" --contains "ready" --timeout-ms 60000
icarus ports open fix-login --browser
```

List workspaces that need you, as JSON:

```sh
icarus workspace list --json | jq '.[] | select(.unread.kind == "needs_input") | .name'
```

Use `open` from any tool that supports a `LAUNCH_EDITOR`-style command:

```sh
icarus file open src/auth.ts:42:7 --focus
```
