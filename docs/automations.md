# Automations

Automations move workspaces between status columns as their pull requests change, and can create, clean up and nudge workspaces for you. The server runs them, so they work with the app closed.

## Statuses

Statuses are the columns you sort workspaces into (and group the sidebar by). The defaults are Todo, In progress, In review and Done.

To change them, open Settings (`icarus: open settings`) → Statuses: add, rename, reorder and remove statuses, and pick each one's colour and icon. Or edit `workspace.statuses`:

```json
{
  "workspace": {
    "statuses": [
      { "id": "todo", "label": "Todo", "color": "neutral", "icon": "circle" },
      { "id": "in-progress", "label": "In progress", "color": "conductor-progress", "icon": "conductor-progress" },
      { "id": "blocked", "label": "Blocked", "color": "rose", "icon": "circle-pause" },
      { "id": "in-review", "label": "In review", "color": "conductor-review", "icon": "conductor-review" },
      { "id": "completed", "label": "Done", "color": "conductor-done", "icon": "conductor-done" }
    ]
  }
}
```

Ids are lowercase letters, digits, `-` and `_`, and must stay stable: rules refer to them.

| | Values |
|---|---|
| Colours | `neutral`, `blue`, `sky`, `violet`, `amber`, `emerald`, `rose`, `zinc`, `conductor-done`, `conductor-review`, `conductor-progress` |
| Icons | `circle`, `circle-dot`, `circle-progress`, `circle-dashed`, `circle-ellipsis`, `git-pull-request`, `timer`, `flag`, `circle-alert`, `circle-pause`, `circle-play`, `circle-check`, `ban`, `conductor-done`, `conductor-review`, `conductor-progress` |

## Turn automations on

Automations need three things: the loop on, the projects they act on, and permission to act.

```sh
icarus config set rules.enabled true
icarus config set rules.projects '["my-repo"]'   # project names or canonical remotes
icarus rules tick --dry-run                      # see what it would do
icarus config set rules.armed true               # let it act
```

Until `rules.armed` is true, every run is a dry run: it plans actions and reports them, but changes nothing. Notifications are still sent. The loop runs every minute (`rules.interval_ms`). The same switches are in Settings → Automation, except the project list.

```sh
icarus rules status   # on or off, armed or not, and what the last run planned and did
icarus rules tick     # run now
```

## Rules

The rules live under the `rules.pr` settings key. Settings → Automation and Pull request rules edit them.

### Move cards by pull request state

For each workspace with a pull request, the first matching condition decides its column:

| Condition | When |
|---|---|
| `merged` | The PR merged. |
| `closed` | The PR closed without merging. |
| `reviewing` | Someone else's PR (you are reviewing it). |
| `merge-queue` | The PR is in a merge queue (only checked when mapped). |
| `conflicts` | The PR has conflicts (only checked when mapped). |
| `draft` | The PR is a draft. |
| `changes-requested` | A reviewer requested changes and hasn't been re-requested. |
| `merging` | The check named `mergingCheckName` passed, or the PM team approved. |
| `pm-approval` | Every pending reviewer is in the PM team. |
| `review` | Anything else: an open PR waiting for review. |

Map conditions to status ids in Settings → Automation, or:

```json
{
  "rules": {
    "pr": {
      "enabled": true,
      "statusByCondition": {
        "draft": "in-progress",
        "changes-requested": "in-progress",
        "review": "in-review",
        "merged": "completed"
      }
    }
  }
}
```

A condition you don't map never moves a card.

### Other options

| Setting (under `rules.pr`) | What it does |
|---|---|
| `newWorkspaceStatus` | The column new workspaces start in. |
| `holdStatus` | A column the rules never touch. Park a workspace there to keep it as it is. |
| `mergingCheckName` | The check whose success means "merging". Default `Reviews satisfied`. |
| `pmApprovalTeam` | A GitHub team (`org/team-slug`) whose approval counts as PM approval. |
| `onResolved` | `delete` removes your workspace once its PR merged or closed (not forced: local changes keep it). |
| `onReviewed` | `delete` removes a review workspace once you approved or requested changes (skipped while you have unsent review comments). |
| `resolveConflictsWithAgent` | Start an agent when your PR gets conflicts. `conflictPromptTemplate` replaces the built-in prompt. |
| `reviewEnteredCommand` | A shell command run when a workspace enters the `review` column. |
| `reviewInbox.enabled` | Create a review workspace for every PR that requests your review, with an agent (`reviewInbox.agent`) and a review prompt (`promptTemplate`, `rereviewPromptTemplate`). |
| `actionPrompts` | Prompts for the next-action button's agent steps: `commitAndPush`, `createPullRequest`, `readyForReview`, `reviewPullRequest`. Blank uses the built-in prompt. |

Review workspaces follow the PR: when its author pushes, the workspace is fast-forwarded to the new head.

Automations also notify you when CI fails on your PR, someone requests your review, or a PR merges.

### Presets

Two shipped presets set up common flows. List them in your user settings:

```json
{ "presets": ["github-pr-flow", "review-inbox"] }
```

- `github-pr-flow`: drafts and changes-requested go to In progress, open PRs and merging ones to In review, and new workspaces start in In progress.
- `review-inbox`: review requests become workspaces in In review.

Presets sit between the built-in defaults and your own settings, so anything you set wins. You still need `rules.projects` and `rules.armed`.

## Automation scripts (preview)

A script decides instead of the rules: you write the policy, Icarus loads the facts, applies what the script returns and records every run. They will replace the rules above.

The quickest start is the built-in flow as a script: `icarus automation template > ~/.config/icarus/automations/my-app.ts` writes the GitHub PR rules above (statuses by PR state, review inbox, review head sync, conflict and CI agents, notifications) with their options at the top. Or let an agent set it up: `icarus skill install` gives Claude Code the `icarus-automations` skill, which interviews you, writes the script and dry-runs it before arming.

Point a project at a script, relative to `~/.config/icarus` (an absolute path works too):

```json
{
  "automations": {
    "armed": true,
    "scripts": { "my-app": "automations/my-app.ts" }
  },
  "prompts": { "fix-checks": "CI failed on #{{pr.number}} ({{pr.title}}). Find out why and fix it." }
}
```

Every 60 s while an app window is open (`automations.interval_ms`), and on `icarus automation run` or `emit`, the script gets the scope as JSON on stdin and prints its decision as JSON on stdout. `.ts` and `.js` files run with `bun`; anything else runs as an executable. Print logs to stderr.

```ts
import type { Scope, Decision } from "./.generated/icarus";

const scope: Scope = await Bun.stdin.json();
const out: Required<Decision> = { actions: [], memory: scope.memory, buttons: {} };

for (const ws of scope.workspaces) {
  const pr = ws.pr;
  if (pr?.state === "MERGED" && ws.status !== "completed") {
    out.actions.push({ method: "workspace.set", params: { id: ws.id, status: "completed" } });
  }
  if (pr?.is_draft && pr.checks_rollup === "success") {
    out.buttons[ws.id] = [{ id: "ready", label: "Ready for review", reason: "draft with green checks" }];
  }
}

const t = scope.trigger;
if (t.kind === "event" && t.name === "button" && t.payload.id === "ready") {
  out.actions.push({ method: "pr.operate", params: { workspace: t.workspace, operation: "ready" } });
}

console.log(JSON.stringify(out));
```

- **Events:** within about two seconds of a change, the script runs with `trigger.kind: "event"`: `workspace.created`, `workspace.status_changed`, `pr.opened`, `pr.state_changed` (`DRAFT`/`OPEN`/`MERGED`/`CLOSED`), `pr.head_changed`, `pr.base_changed`, `checks.changed`, `checks.failed` (with the failed check names), `review.requested`, `review.submitted`, `git.pushed`, `agent.finished`, `agent.waiting` and `linear.changed`. One burst of changes is one run; `trigger.events` lists all of them. Changes your script makes come back as events too, so act only when something still needs doing. Nothing is detected while no app window is open.
- **Actions** are server methods, the same ones the CLI calls (`Method` in the generated types lists them). `prompt_id` in an action's params renders that prompt for its workspace into `prompt`. A script may also run `icarus …` itself; those calls are recorded with the run.
- **Buttons:** a workspace listed in `buttons` shows the script's buttons instead of the built-in next step; a click is a `button` event run on a fresh scope. Leave a workspace out to keep the built-in step, which every workspace also carries as `suggested_action`.
- **Memory:** return `memory` and you get it back as `scope.memory` next run. `scope.previous` is the scope of the last run that changed something.
- **Safety:** with `automations.armed` off every run is a dry run. `automations.enabled: false` stops everything. A workspace in `automations.hold_status` gets no actions unless the action says `"force": true`, and a run starts at most `automations.max_agents_per_run` agents (3).
- **Scope extras:** `scope.inbox` lists open PRs requesting your review (with the `workspace` already on each, if any). Each workspace carries `linked_pr`, `review_pending` (unsent draft comments), `head_is_local` and `base_follows_target`.
- **Actions beyond the CLI's usual ones:** `review.workspace` checks out a PR to review with an agent and a layout, `workspace.sync_to_pr_head` moves a review workspace to the PR's head and target, `tab.open` opens a tab in the workspace's window (queued until one is open), `issue.assign` and `notification.send`.
- **Shadow mode:** while the rules above are on, each timed run is compared with what the rules did. `icarus automation status` counts the runs that differed and `icarus automation runs --shadow` lists them; once they agree for your projects, turn the rules off.
- **Types:** `~/.config/icarus/automations/.generated/icarus.d.ts` and `schema.json` list your status and prompt ids; they are rewritten when those settings change.

Debugging: `icarus automation runs --failed`, `icarus automation show <run> --scope`, `icarus automation replay <run> [--script fixed.ts]`, and `icarus workspace button <ws> --explain` for why a button shows.
