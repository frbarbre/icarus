# Keyboard shortcuts

Icarus keeps Zed's default shortcuts and adds its own. Where an Icarus shortcut takes a key Zed used, the Zed action stays in its menu and the command palette.

The Linux column applies to the app built from source; releases ship a macOS app only. "None" means no default key: the action is in the command palette, and you can bind one yourself.

## Workspaces and agents

| Action | macOS | Linux | Action name |
|---|---|---|---|
| New workspace | ⌘⇧N | Ctrl+Shift+N | `icarus::NewWorkspace` |
| Workspace switcher | ⌘J | Ctrl+J | `icarus::OpenWorkspaceSwitcher` |
| Filter the Workspaces panel (panel focused) | ⌘F | Ctrl+F | `icarus::FocusWorkspaceFilter` |
| New agent tab | ⌘⌥T | none | `icarus::NewAgent` |
| Run the next action | ⌘⌥Enter | Ctrl+Alt+Enter | `icarus::RunNextAction` |
| Open the running frontend | ⌘⇧B | Ctrl+Shift+B | `icarus::OpenFrontend` |
| Open the PR's preview | ⌘⌥P | Ctrl+Alt+P | `icarus::OpenPreview` |
| Open the PR on GitHub | ⌥⌘G | none | `icarus::OpenPullRequest` |
| Copy the PR's URL | ⌥⌘⇧G | none | `icarus::CopyPullRequestUrl` |
| Open the linked Linear issue | ⌥⌘L | none | `icarus::OpenLinearIssue` |
| Toggle a card field | ⌘K C 1…9, 0 | Ctrl+K C 1…9, 0 | `icarus::ToggleCardField` |

The card field chords, in order: 1 unread, 2 status chip, 3 agents, 4 pull request, 5 PR author, 6 PR diff, 7 stack, 8 Linear title, 9 ports, 0 project.

## Panels

Each key focuses its panel, opening the dock; pressed again while the panel has focus, it hides the panel.

| Action | macOS | Linux | Action name |
|---|---|---|---|
| Explorer | ⌘⇧E | Ctrl+Shift+E | `project_panel::ToggleFocus` |
| Source Control panel | ⌘⇧G | Ctrl+Shift+G | `icarus::ToggleSourceControlPanel` |
| Checks panel | ⌘⇧K | Ctrl+Shift+K | `icarus::ToggleGitHubPanel` |
| Search | ⌘⇧F | Ctrl+Shift+F | `pane::DeploySearch` |
| Quick Open: go to a file, `%text` searches file contents, `:line` or `:line:column` jumps in the active file | ⌘P | Ctrl+P | `file_finder::Toggle` |
| Left dock | ⌘B | Ctrl+B | `workspace::ToggleLeftDock` |
| Right dock | ⌘L (outside text editors), ⌘R, ⌥⌘B | Ctrl+Alt+B | `workspace::ToggleRightDock` |

The right sidebar's tabs follow Orca's order: Explorer, Agents, Source Control, Checks. Their keys are Orca's and work from editors, terminals and the agent composer too, so Zed's bindings that took them are gone: delete line (⌘⇧K) and reveal in project panel (⌘⇧E) stay in the command palette, and previous search match (⌘⇧G) is Shift+Enter in the search bar. Inside a text editor ⌘L still selects the line.

## Diffs, review and Source Control

| Action | macOS | Linux | Action name |
|---|---|---|---|
| Show the diff of the active file | ⌥⌘D | none | `icarus::ShowDiff` |
| Show a diff tab as the editable file | ⌥⌘E | none | `icarus::ShowEdit` |
| Next / previous change | F7 / ⇧F7 | F7 / Shift+F7 | `icarus::NextChange` / `PreviousChange` |
| Next / previous changed file | ⌥F7 / ⌥⇧F7 | Alt+F7 / Alt+Shift+F7 | `icarus::NextChangedFile` / `PreviousChangedFile` |
| Comment or note on the selected lines | ⌘⇧A | Ctrl+Shift+A | `icarus::AddReviewComment` |
| Send the comment box | ⌘Enter | Ctrl+Enter | `icarus::SubmitReviewComment` |
| Send a reply (Shift+Enter for a new line) | Enter | Enter | `icarus::SubmitReviewComment` |
| Close the comment box | Escape | Escape | `icarus::CancelReviewComment` |
| Uncommitted changes only, and back | ⌥⌘U | none | `icarus::ToggleUncommittedChanges` |
| Latest agent turn, and back | ⌥⇧⌘T | none | `icarus::OpenLatestAgentTurn` |
| Commit (in the commit dialog) | ⌘Enter | Ctrl+Enter | `icarus::ConfirmCommit` |

## Panes and terminals

| Action | macOS | Linux | Action name |
|---|---|---|---|
| Focus the tab group left / right / up / down | ⌥⌘← / → / ↑ / ↓ | none | `icarus::FocusGroupLeft` / `Right` / `Up` / `Down` |
| Split a terminal right | ⌘D | Ctrl+Shift+D | `pane::SplitRight` |
| Split a terminal down | ⌘⇧D | Alt+Shift+D | `pane::SplitDown` |
| Next / previous terminal pane | ⌘] / ⌘[ | Ctrl+] / Ctrl+[ | `icarus::FocusNextTerminalPane` / `FocusPreviousTerminalPane` |
| Expand a terminal pane | ⌘⇧Enter | Ctrl+Shift+Enter | `workspace::ToggleZoom` |
| Clear a terminal | ⌘K | Zed's default | `terminal::Clear` |

With one tab group, ⌥⌘← / ⌥⌘→ go back and forward in history. Zed's previous/next tab stays on ⌘{ / ⌘}, and the editor's add-cursor-above/below on ⌘⌃P / ⌘⌃N. Terminal line-editing keys are listed in [Terminals](terminals.md#line-editing).

## Actions without a default key

| Action | What it does |
|---|---|
| `icarus::OpenSettings` | Open the Icarus Settings tab. |
| `icarus::OpenAgentHistory` | Search and resume past agent sessions. |
| `icarus::FocusComposer` | Put the cursor in the composer. |
| `icarus::RunQuickCommand` / `OpenQuickCommandsMenu` | Run a quick command. |
| `icarus::SetPaneTitle` / `ClearPaneTitle` | Name a terminal pane. |
| `icarus::ToggleRemoteControl` | Remote Control for the active agent. |
| `icarus::ShowNotifications` / `MarkAllNotificationsRead` / `ClearNotifications` | Notifications. |
| `icarus::AddProject` | Add a project. |
| `icarus::ToggleWorkspacesPanel` | Show or hide the Workspaces panel. |
| `icarus::CommitChanges`, `PushBranch`, `PullBranch`, `SyncBranch`, `ForcePushWithLease` | Git actions on the workspace. |
| `icarus::ActivateNextItemOfSameType` / `ActivatePreviousItemOfSameType` | Cycle through tabs of the active tab's kind (agents, terminals, files, diffs). |
| `icarus::FocusReviewSummary` | Jump to the review summary. |
| `icarus::SetWorkspaceGrouping` / `SetWorkspaceSort` | Group or sort the sidebar (`{"group_by": "stack"}`, `{"sort_by": "smart"}`). |
| `icarus::ImportZedSetup` | Import a Zed install's setup. |

## Rebinding

Shortcuts live in the app's Zed keymap, `~/.config/icarus/keymap.json` (`zed: open keymap`, or the keymap editor). It works exactly like [Zed's keymap](https://zed.dev/docs/key-bindings): bind an action in a context, or unbind a key with `null`.

```json
[
  {
    "context": "Workspace",
    "bindings": {
      "cmd-r": "icarus::RunQuickCommand",
      "cmd-shift-h": "icarus::OpenAgentHistory",
      "cmd-alt-t": null
    }
  },
  {
    "context": "Workspace",
    "bindings": {
      "cmd-k c p": ["icarus::ToggleCardField", { "field": "pr" }]
    }
  }
]
```

Note that ⌘⇧H is Zed's project-wide replace; binding it as above replaces that.

You can also run any action from the shell, which is handy for scripts and agents:

```sh
icarus app dispatch icarus::NewAgent
icarus app dispatch icarus::SetWorkspaceSort --args '{"sort_by": "name"}'
```
