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
| `workspace adopt <path> [--project <p>] [--name <n>]` | Register a worktree of another clone as a workspace. See [Workspaces](workspaces.md#adopt-a-worktree-from-another-clone). |
| `workspace list [--project] [--all]` / `show <ws>` | List workspaces, rescanning worktrees first (`--all` adds hidden ones), or show one. |
| `workspace hide <ws>` / `unhide <ws> [--follow-settings]` | Hide or show a workspace in the sidebar. See [Worktrees made elsewhere](workspaces.md#worktrees-made-elsewhere). |
| `workspace inbox [--project]` / `show-source <source> --project <p>` | Newly found worktrees, and showing every worktree from a source. |
| `workspace set <ws> [--status] [--title] [--base\|--use-primary] [--pin\|--unpin]` | Change a workspace. `--use-primary` drops its own base so it follows the repository's primary branch. |
| `workspace refs <ws> [query] [--limit]` | Branches the base can be changed to (Source Control's Change Base Ref). |
| `workspace reorder <ws>...` | Put workspaces at the top of the Manual sort. |
| `workspace mark-read` / `mark-unread <ws>` | Clear or set the unread mark. |
| `workspace setup <ws> [--trust]` | Run the setup script again. |
| `workspace layout <ws> <layout>` | Rearrange the workspace's tabs. |
| `workspace changes <ws> [--uncommitted]` | Changed files against the base. |
| `workspace turns <ws>` | Recorded agent turns. |
| `workspace next-action <ws> [--run] [--button <id>]` | Show or run the next step. With an automation script deciding the buttons, `--run` clicks one. |
| `workspace button <ws> [--explain]` | The workspace's buttons and whether the built-in resolver or a script run decided them; `--explain` shows what that run saw (PR state, draft, checks). |
| `workspace sleep` / `wake <ws>` | Park and restart a workspace. |
| `workspace remove <ws> [--force] [--keep-branch] [--delete-worktree] [--delete-branch] [--skip-archive] [--allow-failed-archive]` | Remove a workspace. A worktree Icarus didn't create is only hidden unless `--delete-worktree`. |
| `workspace review --project <p> <number> [--name] [--status] [--since <sha>] [--prompt] [--provider] [--layout]` | Check out someone else's PR to review, with an agent and a layout. |
| `workspace sync-pr-head <ws>` | Move a review workspace to its PR's head and target branch. |
| `layout list [--project]` / `show <name>` | Tab layouts. |
| `tab open <ws> --type agent\|browser\|diff\|explorer\|file\|terminal […] [--split right\|down] [--background]` | Open a tab in the workspace's window (queued until one is open). |

### Agents

| Command | What it does |
|---|---|
| `agent create [--workspace] [--cwd] [--provider] [--prompt] [--model] [--mode] [--account] [--title] [--ask-permissions] [--resume] [--remote-control on\|off]` | Start an agent. |
| `agent mode <id> [<mode>]` | List the agent's permission modes, or switch to one; it survives restarts. |
| `agent model <id> [<model>]` | List the agent's models, or switch to one for the following turns. |
| `agent commands <id>` | The agent's slash commands. |
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
| `pr operate <ws> <operation>` | Act on it directly: `ready`, `merge`, `enable_auto_merge`, `disable_auto_merge`, `add_to_merge_queue`, `dequeue`, `merge_stack`, `enable_stack_auto_merge`, `disable_stack_auto_merge`. |
| `pr previews <ws>` / `preview <ws> [--app] [--browser]` | Preview deployments. |
| `issue show` / `open` / `search` / `refresh` / `status` | Linear issues. |
| `issue assign <id> [--assignee <user>]` | Assign an issue (to you by default). |
| `linear login` / `logout` / `status` | Save (from a hidden prompt or stdin), remove or check the Linear API key. See [Linear](github-and-linear.md#set-up-the-api-key). |
| `rules status` / `tick [--dry-run]` | The built-in rules engine (replaced by automation scripts). |
| `automation status` | Whether automations are on and armed, each project's script and last run. |
| `automation run [--project] [--dry-run]` | Run the scripts now. |
| `automation emit <name> [--project\|--path] [--workspace] [--payload JSON] [--env VAR…] [--dry-run]` | Send a custom event to a project's script, e.g. from Linear's "Open in coding tool" hook. |
| `automation runs [--project] [--failed] [--acted] [--shadow] [--limit]` / `show <run> [--scope]` | Run records: trigger, script output, actions and results, and what the script saw. |
| `automation replay <run> [--script <path>]` | Re-run a script on a recorded scope as a dry run. |
| `automation schema` | Regenerate `icarus.d.ts` and `schema.json`. |
| `automation template` | Print a starter script: the built-in GitHub PR flow with its options at the top. |
| `skill list` / `get <name>` / `install [name…] [--force]` | Agent skills shipped with Icarus, e.g. `icarus-automations`. Install writes a stub to `~/.agents/skills` and links it from `~/.claude/skills`. |
| `prompt list [--project]` / `render <id> --workspace <ws>` | Prompt templates, and one rendered against a workspace. |
| `notifications [--unread] [--limit]` / `read <id>\|--all` / `clear` | Notifications. |
| `notifications send <title> [--body] [--workspace]` | Post one. |

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
