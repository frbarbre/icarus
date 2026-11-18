# Agent skills: how Icarus gets set up

Status: agreed direction (2026-11-18), not built.

Most people will have an agent set Icarus up rather than click through settings. So setup is agent-first: Icarus ships skills, installs them for the user's agents, and every "set this up" spot in the app has a button that runs the matching skill in an embedded terminal. Everything a skill does goes through the CLI, and everything in the CLI also has a control in the app, so doing it by hand always remains possible.

## Shipped skills

| Skill | Purpose |
|---|---|
| `icarus-setup` | First run: check `gh` and agent logins, import Orca/Zed/VS Code settings and keymaps, add the first projects, pick a theme, then hand off to `icarus-automations`. |
| `icarus-cli` | Reference for every command: targeting workspaces, tabs, panes and agents, and driving the app (`app tree/click/wait/capture`). The other skills build on it. |
| `icarus-automations` | Interview, then statuses, prompts and `automations/<project>.ts`; integrations (Linear hook, dotfiles); a dry run before arming; debugging from run records (see automations.md). |
| `icarus-project` | Add a repo; set its base branch; write setup/archive/run scripts by reading the repo (package manager, env files, codegen, docker); trust, environment recipes, ports and previews, named layouts. |
| `icarus-linear` | Connect Linear through a key command (never a raw key); link issues; install the "Open in coding tool" hook; issue commands. |
| `icarus-review` | Review a PR inside Icarus: diff base, draft comments, notes and threads, summary and verdict, attaching PR comments to the composer. |
| `icarus-workspace-env` | Per-workspace environments: ports, `.localhost` URLs, env files and databases per worktree. |
| `icarus-orchestration` | Spread work across workspaces and agents: child workspaces, agents with prompts, wait, read transcripts, collect results. |
| `icarus-settings` | Change settings safely: which layer, `config explain`, keybindings, themes, and the Zed editor settings underneath. |
| `icarus-debug` | When something misbehaves: server logs, `workspace next-action --json`, automation runs and replay, `app capture`, a CPU sample, and a bug report with all of it attached. |

## Installing and updating (as Orca does)

- **Stubs, not copies:** the installed `SKILL.md` is a short stub (name, description of when to use it) whose body says to run `icarus skill get <name>`. The full guide always matches the running Icarus, and an update needs no reinstall.
- **One shared copy:** skills go to `~/.agents/skills` and each detected agent's folder (`~/.claude/skills`, Codex's, following `CLAUDE_CONFIG_DIR`) links to it. `--local` installs into a project instead.
- **Never clobber edits:** each copy is classified by its digest (current, an older release, edited, other). Only unedited Icarus copies are updated; `--force` backs an edited one up first.
- **CLI:** `icarus skill list|get|install [--all|<name>…] [--agent] [--local] [--force] [--dry-run]|update|installed`.
- Unlike Orca, Icarus installs the core skills (`icarus-cli`, `icarus-setup`) on first start without asking, and updates installed unedited copies on each update. The others install on first use (see buttons).
- On launch, a stale or missing core skill shows a one-line notice with an Update button, like Orca's freshness nudge.

## Skills page in settings

A **Skills** page in the main settings window, built like its other pages. Everything on it also works through `icarus skill …`.

- **One row per shipped skill:** name, what it's for, and its state for each detected agent (Claude Code, Codex): installed and current, out of date, edited by you, or not installed.
- **Actions per row:**
  - Install / Update / Reinstall (an edited copy asks first, then backs it up as `--force` does)
  - Uninstall
  - Run, which opens the skill in a terminal like the "Set up with agent" buttons
  - View, which shows the full guide (`skill get`)
  - Open in editor: opens the installed skill folder in the user's chosen editor. The editor follows the existing `sourceControl.openIn` choice and can be Icarus itself, which opens it as a file tab. `icarus skill open <name> [--editor <cmd>]` does the same from the CLI. Editing a shipped skill marks it "edited", so updates leave it alone until reinstalled.
- **Header:** where skills live (`~/.agents/skills` and each agent's link), Install all / Update all, and the scope switch (user, or this project with `--local`).
- **Custom skills:** the user's own skills found in those folders are listed read-only under "Other skills", so the page shows everything the agents can see.

## "Set up with agent" buttons: an embedded terminal runs the skill

Wherever something isn't configured, the app offers a button that:
1. installs the skill if it is missing (`icarus skill install <name>`), so the command can't fail on a missing skill;
2. opens a terminal tab in the right folder (the workspace's, the project root, or home);
3. runs the user's default agent CLI with the skill, e.g. `claude "/icarus-project"` or `codex "/icarus-project"`, with context as arguments (`/icarus-linear connect`, `/icarus-project <path>`).

The command is visible, answerable in the terminal and copyable to run again later. There is always a "do it by hand" link next to it.

Where the buttons appear:
- **First run:** the welcome screen's main action is "Set up with agent" (`icarus-setup`). "Set up by hand" opens the settings.
- **No projects:** the empty sidebar runs `icarus-project`.
- **Project without a setup script or recipe:** in its settings, `icarus-project` / `icarus-workspace-env`.
- **No automations:** on the Automations settings page, `icarus-automations`.
- **Linear not connected**, or a workspace with a linked issue but no Linear skill: `icarus-linear` (Orca shows a dismissible prompt in the same spot).
- **Something wrong:** the bug-report entry in the help menu runs `icarus-debug`.

## Phases

1. `icarus skill` CLI and server methods, stub install with digest classification, auto-install of the core skills, the launch notice, the Skills settings page. Write `icarus-cli` and `icarus-setup`.
2. The terminal-button component and its placements; `icarus-project`, `icarus-linear`, `icarus-settings`, `icarus-debug`.
3. `icarus-automations` (with automations phase 4), `icarus-review`, `icarus-workspace-env`, `icarus-orchestration`.

Accept: on a clean machine, the welcome button gets a user from nothing to a project with a working setup script and an armed automation through one agent conversation, with no settings opened by hand.
