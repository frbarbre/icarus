# GitHub and Linear

## GitHub

Icarus talks to GitHub through your `gh` login. Sign in once:

```sh
gh auth login
```

Settings → Integrations shows whether `gh` is signed in.

### Pull request status

A workspace's pull request is the open PR for its branch. Cards show it when the `pr` card field is on: the number, state, and a dot for its checks. The `pr-author`, `pr-diff` and `stack` fields add the author's avatar, +N −M and the stack position.

The GitHub panel (right dock; the action is `icarus::ToggleGitHubPanel`) shows the workspace's PR in full:

- The state (Draft, Open, Queued, Merged, Closed), refresh, and a link to GitHub.
- **Stack**: when the PR is part of a GitHub stack, every PR in it, with the current one highlighted, and a warning when the stack is out of date.
- **Merge queue**: the PR's position and estimated time when it's queued.
- **Needs attention**: conflicts, failing checks and other blockers.
- **Checks**: failing first, with durations.
- **Reviewers** and the **Review** section. See [Review a pull request](review.md#review-a-pull-request).
- **Previews**: the PR's preview deployments.

To open the PR: `icarus: open pull request`, or copy its URL with `icarus: copy pull request url`.

```sh
icarus pr status fix-login
icarus pr status fix-login --refresh   # skip the short cache
```

### Merge, merge queue and auto-merge

The title bar's next-action button picks the right merge step for the PR and the repository:

| Step | When |
|---|---|
| **Merge** | The PR can be merged now. |
| **Add to merge queue** | The repository uses a merge queue. |
| **Turn on auto-merge** / **Turn off auto-merge** | Checks or reviews are still pending. |
| **Merge stack** | The PR is part of a GitHub stack that can be merged together. |
| **Turn on stack auto-merge** / **Turn off stack auto-merge** | The PR is in a stack; with it on, the server's automation merges the stack once it's ready. |

Before merging, the button offers the earlier steps: **Commit & push**, **Create PR**, **Ready for review**, **Resolve conflicts**. Press ⌘⌥Enter (Ctrl+Alt+Enter) or click it. From the shell:

```sh
icarus workspace next-action fix-login --run
```

### Previews

Icarus finds a PR's preview deployments from GitHub Deployments, then commit statuses, then links in PR comments. ⌘⌥P (Ctrl+Alt+P) opens the primary preview, and the title bar has a button for it (with a picker when there are several). It warns when the preview is behind the PR's head.

```sh
icarus pr previews fix-login
icarus pr preview fix-login --app docs --browser
```

Tune how previews are found:

| Setting | What it does |
|---|---|
| `previews.environment_pattern` | Regex a deployment environment must match to count as a preview. |
| `previews.exclude_pattern` | Regex of environments that are never previews; empty turns it off. |
| `previews.comment_pattern` | Regex for preview links in PR comments, with named groups `url` (required), `app` and `sha`. |
| `previews.primary_app` | The app opened when none is named. |

## Linear

Icarus reads Linear issues to name workspaces and show issue details on cards. It never changes anything in Linear.

### Set up the API key

Create a personal API key in Linear's settings. Then give it to the server in one of two ways:

- Set `LINEAR_API_KEY` in the server's environment.
- Or set a command that prints the key, so it never sits in a file. This is the better choice when the server starts at login, since it doesn't see your shell's environment:

```sh
icarus config set linear.api_key_command '["security", "find-generic-password", "-s", "linear-api-key", "-w"]'
```

The command can be an argument list or a string run with `sh -c`. Its output is kept in memory only, and it runs again if Linear rejects the key. Set it in your user settings (it isn't read from project files). Settings → Integrations has the same field and a refresh button.

```sh
icarus issue status    # is Linear configured?
```

### Use issues

- In the New workspace dialog, search the **Linear issue** field by id or title (empty shows your open issues). Picking one links it and fills the name and Linear's branch name.
- ⌥⌘L (macOS) opens the window's linked issue in the browser.
- The `linear-title` card field shows the issue's title and coloured labels.

Linked issues refresh every 5 minutes (`linear.poll_interval_ms`). Without a key everything still works; cards keep the issue id and title they were created with.

```sh
icarus workspace create --project my-repo --from-linear ABC-123
icarus issue search "login redirect"
icarus issue show ABC-123
icarus issue open ABC-123
icarus issue refresh
```
