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

The rules live under the `rules.orca_compat` settings key. Settings → Automation and Pull request rules edit them.

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
    "orca_compat": {
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

| Setting (under `rules.orca_compat`) | What it does |
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
