# Automations: user scripts over a scope

Status: phases 1–4 built (0.2.15); both engines run side by side in shadow mode. Removing the built-in rules engine (`crates/icarus-rules`, `rules.*` settings, `icarus rules`) waits for a week of agreeing shadow runs.

## Why

The rules engine compiles one workflow (Orca's status rules) into Icarus. Changing it means a release, and it is shaped around one user. Instead, Icarus becomes the engine (load facts, offer actions, run a script, record what happened) and the policy is a script the user owns and edits any minute.

## Model

```
trigger (60 s tick | event) ─▶ scope loader ─▶ user script ─▶ actions ─▶ Icarus applies them ─▶ run record
```

### Settings: statuses and prompts

- `statuses`: the list of status columns, as today (ids, labels, colours, order). Nothing else about them is built in.
- `prompts`: `{ "<id>": "<template>" }`. Templates only substitute `{{path}}` from the scope (`{{pr.number}}`, `{{workspace.status}}`, `{{linear.title}}`). No conditionals: logic belongs in the script.
- Both are layered like every setting (defaults < user < project < project-local), so a project can add or override them.

### Scripts: one per project, owned by the user

- Settings name a script per project: `"projects": { "<project>": { "automation": "automations/<project>.ts" } }`, resolved against `~/.config/icarus/`.
- Icarus ships no automation. A project without a script runs nothing. The docs carry a short example only.
- Any executable works: it gets the scope as JSON on stdin and prints actions as JSON on stdout. `.ts` files run with `bun`; anything else runs as is (shebang).
- The user's own scripts live in their dotfiles: `~/.config/icarus/automations` is a symlink into `~/.dotfiles` (user's setup, not Icarus's).

### Scope

What the script sees, per run. It is built from what Icarus already polls and caches, never from extra API calls per run (GitHub quota):

- `trigger`: `{ kind: "tick" }` or `{ kind: "event", name, workspace?, payload }`.
- `project`: id, name, repo, base.
- `workspaces[]`: id, name, path, branch, base, status, title, pinned, hidden, origin, created/activity times, and for each:
  - `git`: head, upstream, ahead/behind, uncommitted, conflicts, changed files (paths and +/−)
  - `pr`: what the PR snapshot holds (state, draft, mergeable, merge state, checks with each run, reviews and requested reviewers, the viewer's own reviews and the commit they reviewed, auto-merge, merge queue, stack)
  - `suggested_action`: what the built-in resolver would offer (see Buttons)
  - `linear`: the linked issue (id, title, state, labels, assignee, url)
  - `agents[]`: id, provider, title, state, turns, last activity
  - `tabs`: the panes and tabs open in its window, if any
  - `notes`, `review` (drafts pending)
- `inbox`: PRs awaiting the viewer's review that have no workspace yet.
- `settings`: the resolved `statuses` and `prompts`.
- `memory`: what the script stored in earlier runs (see State).
- `now`, `viewer` (GitHub login), `icarus_version`.

### Actions are the CLI

There is no separate action vocabulary. Every CLI command is a server method, and an action is exactly one call:

```json
[{ "method": "workspace.set", "params": { "id": "ws-…", "status": "reviewing" } }]
```

- The action types in the generated schema are built from the same definitions the CLI uses, so they can't drift, and a new command is a new action automatically.
- A script may also run `icarus …` itself. Icarus sets `ICARUS_RUN_ID`, so such calls are recorded against the run, and under a dry run or replay the CLI prints the call instead of making it.
- Icarus applies returned actions in order, stops a workspace's remaining actions after its first failure, and records each result.
- Guards applied by Icarus regardless of the script: a kill switch (`automations.enabled`), `automations.armed` (off = always a dry run), a limit on agents started per run, and no actions on a workspace in the hold column unless the script says `force: true`.

### State

Ticks re-run the same decision, so idempotent actions (set a status) are safe, but one-shot ones (start an agent when CI fails) would repeat. Two tools:

- `memory`: the script returns `{ actions, memory }`. Icarus stores `memory` per project and hands it back next run (e.g. `{ handled: { "4222@sha": true } }`). Size-capped.
- `previous`: the scope of the previous run that changed anything, so a script can diff ("checks went pending → failed").

Events (phase 2) cover most one-shot cases directly.

### Only while the app is open

The server keeps running so agents and terminals survive the app closing. Automations don't:
- Ticks and events run only while at least one app window is connected. CLI connections, including a script's own `icarus …` calls, don't count.
- Closing the last app window stops them within one tick. A run in progress finishes; no new ones start.
- When the app connects again, the next tick starts. Events that happened while it was closed are not replayed, but the first run's `previous` scope shows what changed since the last run.
- Exception: `automation emit` and `automation run` are explicit requests, so they run even with no app open. The Linear hook, for example, opens the app anyway.
- The PR poller already follows the same rule (no polling without a client); it changes from counting any client to counting app clients, so the scope stays fresh exactly while automations run.

### Triggers

- Phase 1: a tick every 60 s, plus `icarus automation run` on demand. One run at a time per project; a tick due while one runs is skipped.
- Phase 2: events from what the server already observes, debounced (one run per burst): `pr.opened`, `pr.state_changed` (draft/ready/merged/closed), `pr.head_changed`, `pr.base_changed`, `checks.changed`, `checks.failed`, `review.requested`, `review.submitted`, `agent.finished`, `agent.waiting`, `git.pushed`, `workspace.created`, `workspace.status_changed`, `linear.changed`.

### Custom events: outside tools hand work to the script

Anything outside Icarus can start a run: `icarus automation emit <name> [--project <id> | --path <dir>] [--payload <json>] [--env VAR…]`. The run gets `trigger: { kind: "event", name, payload }`. With `--path`, the project is the one owning that directory, a linked worktree included. With neither flag, it falls back to `automations.default_project`. Custom event names are free-form and typed as `string` in the schema.

Worked example: the Linear desktop app's "Open in coding tool" hook (`~/.linear/coding-tools.json`). Today it runs `orca-open-issue.sh`. That script resolves the repo, assigns the issue, creates a worktree named after the issue id and starts Claude in plan mode, with the prompt left on the clipboard. It becomes a one-line shim kept in the user's dotfiles:

```sh
exec icarus automation emit linear.open_issue --path "$LINEAR_WORK_DIR" \
  --env LINEAR_ISSUE_IDENTIFIER LINEAR_ISSUE_BRANCH_NAME LINEAR_PROMPT LINEAR_PROJECT_NAME
```

The project's script handles `linear.open_issue` with ordinary actions:
- `workspace.create { from_linear, name: id lowercased, no agent }`
- `issue.assign { id, me: true }`
- `tab.open { type: "agent", permission_mode: "plan", prompt: payload.LINEAR_PROMPT }`

The prompt goes into the composer directly, so the clipboard workaround goes away. The run is logged and replayable like any other.

### Buttons: the script decides the next action

The dynamic button on each card and in the top bar ("Ready for review", "Add to merge queue", "Fix checks"…) is a script decision like any other, not a separate built-in.

- **Deciding:** the script returns `buttons: { "<workspace id>": [{ id, label, icon?, style?, tooltip?, reason? }] }`. The first is the main button and the rest go in its menu. Icarus shows them as returned until the next run; `reason` is a free-text note for debugging.
- **Clicking:** a click is an event, `trigger: { kind: "event", name: "button", workspace, payload: { id } }`, run against a fresh scope. The script answers with actions like any run, so a button can be anything:
  - one CLI method (`pr.operate { operation: "ready" | "merge" | "enable_auto_merge" | "add_to_merge_queue" … }`)
  - a prompt (`agent.create { workspace, prompt_id: "fix-checks" }`: any action's `prompt_id` is rendered for its workspace into `prompt`)
  - several actions in a row
  - its own code: a `fetch` to some API, a shell command, anything the script language can do
- **Fresh enough:** buttons are re-decided on every run, and events (phase 2) make that happen within seconds of a PR change. A click re-reads the scope, so a stale button can't act on stale state: the script may answer "nothing to do" and the button updates.
- **Default:** Icarus's built-in resolver (today's, identical to Orca's `resolveWorkspaceAction`, with its ported tests) stays and is passed in as each workspace's `suggested_action`. A project without a script shows exactly that, and its click runs the matching method directly. A script that only wants to override a few cases returns `suggested_action` for the rest.
- **Debugging:** every button on screen points at the run that decided it. `icarus workspace button <id> --explain` prints that run's id, the scope fields it read (`pr.draft`, `pr.merge_state`…), the button returned and its `reason`; `automation replay` reruns it. A wrong button like "Add to merge queue" on a draft becomes: open the run, see whether the scope said `draft: false` (stale data) or the script decided wrong (logic).

### Generated schema and types

- `schema.json` (JSON Schema) and `icarus.d.ts` in `~/.config/icarus/automations/.generated/`.
- The base types (scope, actions, events) ship with the server (`automations.d.ts`, hand-written in phase 1; generating them from the Rust types is open). `Method` lists every server method; per-method params aren't typed yet.
- Status ids and prompt ids are written in as literal unions from the resolved settings. They are regenerated whenever `statuses` or `prompts` change, and on start.
- The folder is per machine and not meant for dotfiles.

### Run records and replay

- Each run stores: id, project, trigger, the scope (deduplicated: stored once when unchanged between runs), the script's stdout, stderr and exit code, the actions decided, each result, memory before and after, and the duration.
- Retention: 24 h of runs or the last 500 per project, whichever is more. Runs that failed or applied an action are kept 7 days. Stored in `<state dir>/automations.db` (SQLite, scopes deduplicated by hash, not compressed).
- CLI:
  - `icarus automation status`: on/armed, script, last runs
  - `icarus automation runs [--failed] [--acted]`
  - `icarus automation show <run> [--scope]`
  - `icarus automation replay <run> [--script <path>]`: re-run a script against a recorded scope as a dry run
  - `icarus automation run [--dry-run]`
  - `icarus automation schema`: regenerate the types now

## Settings UI: one Automations page in the app's settings

Once the script owns the policy, most of today's Icarus settings view has nothing left to configure. These go with the rules engine:
- the Automation section
- the Pull request rules
- the Top bar action prompts

The separate Icarus settings view also doesn't match the app's main settings window. So:
- A new **Automations** page in the main settings window, built like its other pages:
  - **Statuses:** the list, with add, rename, reorder, colour and remove
  - **Prompts:** id plus template, with a preview rendered against a chosen workspace (`prompt render`)
  - **Per project:** the script path, `enabled`/`armed`, the last runs with links to `automation show`, and buttons for "Run now (dry run)" and "Regenerate types"
- The Icarus settings view is retired, and what remains of it moves to pages in the main settings window. Everything stays reachable through `icarus config`, since agents will do most of the setup.

## Shipped skill: setting up automations

Icarus ships an agent skill, `icarus-automations`, installed for the user's agents like the CLI. People will mostly have an agent set this up, so the skill is the main entry point. It:

1. **Interviews the user.** Which statuses do you use? What should happen when a PR is opened, gets review requests, fails CI, merges? Do you review others' PRs from an inbox? Do you use Linear?
2. **Writes statuses and prompts** through `icarus config`, scaffolds `automations/<project>.ts` from the answers, and regenerates the types.
3. **Offers integrations:** for a Linear user, installs the "Open in coding tool" hook (the shim in Custom events plus `~/.linear/coding-tools.json`). Optionally it moves `automations/` into a dotfiles repo and symlinks it.
4. **Proves it before arming:** runs the script as a dry run, shows what it would do for each workspace, and arms it only when the user agrees.
5. **Debugs later:** it knows `automation runs/show/replay`, so "why did X happen" is a question the agent can answer from the run records.

## CLI gap audit: the review workspace

The acceptance test for "actions are complete": a script builds Orca's review workspace with actions only.

| Step | Today | Needed |
|---|---|---|
| Create the workspace from the PR, base = PR target | `workspace create --from-pr`, base set to `origin/<target>` | — |
| Choose the prompt: new review or re-requested | decided inside the rules engine | in the script, from `pr.viewer_reviews`; prompt ids from settings |
| Left pane: an agent running that prompt | `agent create --workspace --prompt` (lands as a new tab) | `tab open --type agent --prompt-id <id> --pane <ref>`, so it takes the pane's slot, reusing an unused agent (as 0.2.14 does) |
| Split, right pane: a browser on the PR | only via the built-in `review` layout | `pane split --workspace --direction right` and `tab open --type browser --url --pane <ref> --focus` |
| The first changed file behind it, in the background | only via the layout's `{first_changed_file}` | `tab open --type diff --path --pane <ref> --background` |
| Works with no window open | `workspace layout` queues for later | every `tab`/`pane` action queues the same way |

New commands (each also a server method, so each is an action):
- `icarus tab open|list|focus|move|close`: every tab type (agent, browser, diff, file, terminal, markdown) with its options, a target pane, `--focus` or `--background`, and an `--index`
- `icarus pane split|list|close`: panes addressed by id, or by a path such as `left`/`right`/`0.1` within the workspace's split tree
- `icarus prompt list|render <id> --workspace`: render a template against the scope, to check it
- `icarus app dispatch --workspace <id>`: target a workspace's window, not only the frontmost
- `icarus issue assign <id> --me|--user <login>` (and state/label changes as scripts need them): today `issue` only shows, opens and refreshes
- `tab open --type agent` takes `--permission-mode` and `--prompt`/`--prompt-id`, so a plan-mode agent with its prompt is one action
- `icarus automation emit`: custom events from outside tools (see Custom events)

Layouts stay as shorthand (`workspace layout`). The review layout becomes something the user's script composes or names in settings, not a built-in.

## Phases

1. **Engine.** Scope loader, actions = server methods (one registry the CLI also uses), the tick runner with guards, `memory`, run records, retention, `automation` CLI, schema and type generation. Plus the `prompt` commands and `pr operate`. Done, except the `tab` and `pane` commands from the audit (they need the app to apply them), `issue.assign`, and `inbox`/`tabs`/`notes` in the scope; those move to phase 3.
   - Buttons: script-returned buttons, click as a `button` event, `suggested_action` in the scope, `workspace button --explain`.
   - Accept: a sample script sets statuses and starts an agent; `replay` reproduces its decision from a recorded scope; schema ids follow a settings change; a script overrides one button and its click runs a shell command.
2. **Events.** The event list above, debounced, delivered as `trigger`. Done as a diff of each workspace's scope on every relevant change (1.5 s debounce); the trigger carries the first event plus `events` with all of them.
   - Includes `automation emit` for custom events.
   - Accept: `checks.failed` runs the script once per failure, within seconds; the Linear shim opens a workspace with a plan-mode agent holding the issue's prompt.
3. **Port and switch.** The user's Orca behaviour (statuses, review inbox, review head sync with target retarget, CI triage, next-action automation, top-bar rules) rewritten as `automations/flowbase.ts` in the user's dotfiles. Shadow mode: both engines evaluate each tick and their decisions are diffed until they agree. Then the built-in engine and the `rules.*` settings are removed, and `settings import` maps Orca's rules config to a generated starter script.
   - Accept: the script builds the review workspace exactly as today's (E2E), and a week of shadow runs shows no differing decisions.
   - Done: `inbox`, `linked_pr`, `review_pending`, `head_is_local`, `base_follows_target` in the scope; `review.workspace` (the review workspace as one action, reusing the built-in layout), `workspace.sync_to_pr_head`, `tab.open` (app applies it, queued without a window), `issue.assign`, `notification.send`; shadow mode (effects compared by key, recorded as `shadow` on the run, `automation runs --shadow`, counts in `automation status`); the port ships as `icarus automation template` rather than only in dotfiles.
   - Deferred: `pane split|list|close` and `tab list|focus|move|close` (`review.workspace` covers the review layout); removing the rules engine and `rules.*`, and `settings import` generating a starter script, after the shadow week.
4. **Settings and skill.** The Automations settings page, retiring the Icarus settings view, and the `icarus-automations` skill.
   - Accept: from a fresh setup, an agent using the skill produces a working, armed script for a project, Linear hook included.
   - Done: an Automations section in the Icarus settings view (settings, prompts with a rendered preview, per-project script, last run, shadow summary, Run now (dry run), Regenerate types); `icarus skill list|get|install [--force]` with the `icarus-automations` guide, installed as a stub in `~/.agents/skills` linked from `~/.claude/skills`.
   - Deferred: retiring the Icarus settings view and moving its pages into the main settings window; the rest of `spec/skills.md` (digest classification across releases, other agents' folders, `--local`, the Skills page).

## Open questions

- Linear and GitHub data beyond what is polled today: added to the poll when a script needs it, never fetched per run.
- Remote servers: the script runs on the server that owns the project, so its `bun` and dotfiles must exist there.
