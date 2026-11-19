# Icarus automations

An automation is one script per project that Icarus runs on a timer, when something changes (a PR, checks, an agent, a status), and when a button on a card is clicked. The script reads a JSON scope on stdin and prints a decision on stdout: actions to run, memory to keep, and buttons to show. `icarus automation --help` and `docs/automations.md` in the Icarus repo are the reference.

## Setting one up with the user

1. **Interview first.** Ask what should happen and when, in their words: which columns cards move through, what an agent should do on a review request or a red CI run, what they want to be told about, what must never happen automatically (deletes, pushes). Don't write code until you can say the flow back in a few lines and they agree.
2. **Statuses and prompts are settings, not code.** List them with `icarus config get workspace.statuses` and `icarus prompt list`; change them with `icarus config set workspace.statuses '<json>'` and `icarus config set prompts.<id> '<text>'`, and check a prompt with `icarus prompt render`. Scripts refer to them by id, so a renamed column needs no script change.
3. **Start from the template.** Scripts live in `~/.config/icarus/automations/` (`$ICARUS_CONFIG_DIR/automations/` when that is set). `icarus automation template > <that folder>/<project>.ts` writes the built-in GitHub PR flow with its options at the top. Change the options before changing the logic. `icarus automation schema` writes `.generated/icarus.d.ts` beside it, so the editor checks the script's types.
4. **Point the project at it:** `icarus config set automations.scripts.<project> automations/<project>.ts` (a path relative to `~/.config/icarus/`, or absolute; the key is the project's name or id).
5. **Dry run before arming.** Until `automations.armed` is true, every run only records what it would do. Run `icarus automation run --project <project> --dry-run`, show the user the planned actions in `icarus automation show <run>`, and get a yes before `icarus config set automations.armed true`.

## Settings from the old built-in rules

Icarus no longer has built-in rules. When it finds `rules` in the user settings (from an older Icarus or an Orca import), it writes them into `automations/orca-flow.ts`, points their projects at it, and saves the old settings in `automations/rules-backup.json`. If the user asks where their rules went, show them that script's options.

## Actions a script can return

Any server method the user could run through the CLI, e.g. `workspace.set` (status), `workspace.remove`, `agent.create` (with `prompt_id` to use a saved prompt), `terminal.create`, `review.workspace` (check out a PR to review, with an agent and the review layout), `workspace.sync_to_pr_head`, `tab.open`, `issue.assign`, `notification.send`.

## Events from outside Icarus

`icarus automation emit <name> --path <dir> [--env KEY…]` runs the script with a custom event. For example, a Linear hook that opens a planning agent on an issue:

```sh
icarus automation emit linear.open_issue --path "$LINEAR_WORK_DIR" --env LINEAR_ISSUE_ID
```

and in the script, on `trigger.name === "linear.open_issue"`, return `tab.open` with `type: "agent"`, `mode: "plan"` and a prompt, plus `issue.assign` for the issue.

## Debugging

- `icarus automation runs [--failed] [--acted]` lists recent runs; `icarus automation show <run>` shows the trigger, actions, results and errors; `--scope` prints the exact input.
- `icarus automation replay <run> [--script <file>]` re-runs a recorded scope as a dry run, so a fix can be tested on the input that went wrong.
- `icarus workspace button <id> --explain` shows why a card shows the button it does.
