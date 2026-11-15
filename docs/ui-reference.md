# UI reference: how workspaces look today

Observed in a live Orca (1.4.x) through Playwright over CDP. The private reference screenshots and the detailed capture (with real PR content) are kept out of the repo (`private/ui-reference.md`). This file describes only the structure.

The goal is the **same general UI**, not pixel parity.

## Window layout (every workspace)

```
┌ left sidebar ─────┬ center: tab groups / splits ─────────────┬ right sidebar ───────┐
│ Search            │ tab strip: [agent] [terminal] [+]  ▶ Run │ icon tabs:           │
│ Tasks             │ ┌──────────────────────────────────────┐ │ explorer · search ·  │
│ Automations       │ │ active tab (agent chat / terminal /  │ │ source control ·     │
│ Workspaces  🔔 ⚙ ＋│ │ browser / editor / diff)             │ │ checks (PR) · ports  │
│  ▸ status group   │ │                                      │ │ … next-action button │
│    workspace card │ └──────────────────────────────────────┘ │ panel content        │
├───────────────────┴──────────────────────────────────────────┴──────────────────────┤
│ status bar: agent usage meters · account · hosts · ports                             │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

**Left sidebar:** grouped by workspace status, in the board's column order. Some columns (for example merge queue, approved, changes requested) only appear when non-empty.
- A group header is the status icon, name and a collapse chevron.
- **Card:**
  - Row 1: PR icon (coloured by PR state; an orange dot means unread), title (the linked issue id, or the PR title for review workspaces), then a link icon, stack position `⧉ n/m`, diff `+A −D` and the author avatar.
  - Row 2: chips: the workspace status chip (icon and colour of the status), then the issue labels as coloured dots.
  - Row 3: the issue title.
  - Row 4, in a tinted row: CI state icon, agent icons with a `+N` overflow, and a `›` chevron that expands the agents.
- Expanded agent rows show the agent icon, the tab title and the age (e.g. "1h").
- **Hovering a card** shows a popover: name, branch, a REVIEWS list (the PR with its checks state and repo) and an ISSUES & TASKS list (the linked issue with its state chip), each with a `+` to link more.

**Top bar of the center:** the tabs of the active group (agent tabs show the agent's icon), `+`, and a **Run** quick-command split button.

**Right edge:**
- Icon tabs for the right sidebar.
- The **next-action button**, whose label depends on the PR state: "Merge when ready" on your own approved PR, "Resolve conflicts" on a conflicted one, none on a review workspace.

**Agent tab** (chat UI):
- **Header:** a sidebar toggle, the session title with a chevron menu, outline, export, files, more, Share.
- **Thread:** markdown messages, collapsible "Ran N commands ›" tool groups, an "Edited N files" card with per-file `+/−` and Undo / See all.
- **Composer at the bottom:** `+` attach, the input ("Type / for commands", or "Ask anything, @ to mention files, / for commands"), send and mic. Below it: model, effort and permission mode pickers, plus a working spinner.

## Reviewing workspace (someone else's PR)

Different from every other status:
- **Layout:** two tab groups side by side.
  - **Left:** the review agent ("Review #N"), whose thread ends in the review prompt's summary (for example Standards / Spec / Questions) and a count of agent notes it left.
  - **Right:** a **browser tab with the PR on github.com**. It shows the "requested your review" banner, Add your review, the stack pill, and the Conversation / Commits / Checks / Files changed tabs. The browser has back, forward, reload, a URL field, and tools (download, inspect, comment, annotate, devtools, open external, more).
- **Card:**
  - The title is the PR title, not an issue id.
  - The avatar is the PR author.
  - The status chip is the column mapped to `reviewing`.
  - The agent row shows "Review #N".
- **No next-action button** in the top bar.
- **The right sidebar opens on Source Control,** in review mode:
  - Header: `PR #N`, the branch, `→ origin/<base>`, ahead/behind, open-on-GitHub.
  - **REVIEW section:**
    - Reviewers with their state dot (amber = pending) and remove `×`. A bot reviewer has re-request ↻ and comment icons.
    - The target-branch picker "<base> · target branch", i.e. the diff base.
    - "Your review was requested".
    - The summary box "Add review summary…" with **Only comment / Request changes / Approve**.
  - **CHANGES `N` `+A −D`** with a filter icon. A tree view by folder with counts. Each file row shows the file icon, an agent-note badge (`💬 n`), `+N −M` and the A/M letter.

## Your own PR in review

- **Tabs** are per top-bar action: "Run", "Commit & push", "Ready for review", each its own agent chat.
- **Top bar:** "Merge when ready" (it becomes Merge or Merge queue depending on the repo).
- **Source Control:**
  - The REVIEW section lists reviewers, with a remove `×` on humans and re-request on bots.
  - The summary box has **only "Only comment"**, because you can't approve your own PR.
  - CHANGES as a tree; test files get a flask icon.

## Conflicts workspace

- **Top bar:** "Resolve conflicts". The tabs include "Resolve conflicts #N", the conflict agent that the rule started.
- **The Checks tab** of the right sidebar, top to bottom:
  - **Header:** `#N OPEN` with refresh and more.
  - **Stack map, collapsed:** "Stack #N · k of m · <base>" with a chevron, plus a warning row "This stack is out of date with <base>" and a **Rebase stack** button.
  - **Expanded:** one row per PR in the stack (`#N title [state pill]`, pills coloured by state such as Conflicts or Changes requested), then the base branch at the bottom.
  - **The PR title,** then "PR updated …".
  - **NEEDS ATTENTION:** e.g. "Conflicts block this PR" with a **Resolve** button.
  - **An explainer:** "The hosting provider reports conflicts, but local Git did not reproduce them…", with copyable commands (`git fetch origin` / empty commit / `git push`).
  - **CHECKS:** a summary ("n passing · n unresolved"). Rows have a state icon, name, duration, conclusion text and an external link. Neutral shows as a dashed circle; Skipped as a circle with a minus.
  - **COMMENTS:** audience tabs (all / humans / bots / unresolved) with counts and icons, and an empty state such as "No human comments."
- **Pending checks:** a pending check row (name · Queued) with an amber spinner, and "n pending" in the CHECKS header.

## Draft, default and hold columns

- **Draft:** an empty-circle chip, with labels and its stack position.
- **Default:** the primary-branch workspace with a `primary` badge and its own agent row.
- **Hold column:** a dashed-circle header.

## Interactions observed

- Clicking a card switches the whole center and right panes to that workspace's saved layout. The right sidebar remembers its tab per workspace: Source Control for review workspaces, Checks for a conflict one.
- The stack map toggles open and closed.
- Clicking the status chip on a card does not open a menu today. Statuses are changed elsewhere (the board). The new app adds the status menu here (plan §3).
- The sidebar scrolls independently; group headers aren't sticky.

## For the new app

Keep:
- the three-column structure
- the card anatomy (four rows, chips, agent row)
- per-status grouping
- the review layout: agent left, GitHub browser right, Source Control in review mode
- the per-action agent tabs
- the context-dependent next-action button
- the Checks panel order: stack map, needs attention, checks, comments

Restyle it in Zed's visual language, but the information and where it sits should feel the same.
