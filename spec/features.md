# Orca fork: feature inventory (spec for the Rust/GPUI rewrite)

Source: `~/Developer/orca` (fork `frbarbre/orca` on upstream `stablyai/orca` 1.4.214, synced 2026-10-28), FORK.md "What this fork owns" §1–§84, 216 merged fork PRs, ~426 fork commits, and a read of `src/`. Paths are relative to `src/` unless noted.

Priority tags:
- **[MUST]** core to the daily workflow.
- **[SHOULD]** used and valued, but the rewrite can ship v1 without it.
- **[LATER]** polish, or a workaround for an Electron/Monaco quirk.
- **[CUT?]** probably unused by the reference user.

Generality tags (the rewrite should be a general product that still fits the reference setup exactly):
- `{GH}` assumes GitHub via `gh`.
- `{LIN}` assumes Linear.
- `{CC}` assumes Claude Code.
- `{USER}` specific to the reference setup: hardcodes one work repo, org or tool. The concrete values are in the reference setup (see `private/reference-setup.md`, not in the repo).

Write these as a pluggable provider or a config value, not as constants.

---

## 1. The workflow, as inferred

1. **Linear issue → workspace.** Create a worktree from a Linear issue, which takes Linear's branch name `{LIN}`. The card shows the issue title and labels; ⌥⌘L opens the issue. New workspaces land in a configured column (e.g. a draft column).
2. **Agent.** Claude Code runs in a PTY in the worktree with full permissions `{CC}`. It is usually viewed through the claude.ai Remote Control **Claude web view** or the Chat UI rather than the raw terminal. Sessions live in a terminal daemon, so they survive app restarts and updates.
3. **Watch the work.**
   - Agent turn snapshots give a diff per turn.
   - The single live Changes list against the branch base, with type filters, "Imports only" and Uncommitted-only.
   - Git blame, LSP navigation, format on save.
4. **Talk to the agent.**
   - Diff notes, sent with Mod+Enter, Enter, Enter, optionally with a prefix message.
   - The agent answers with `orca notes add|reply`, which renders as notes in the diff.
   - Queued PR comments are sent to the agent with ⌥⌘S.
5. **Next step from the top bar** (⌥⌘↩). It walks Commit & push → Create PR → Ready for review → Merge / auto-merge. Each step is an agent prompt the user can edit.
6. **PR lifecycle, automated** (status rules, polled every 60 s).
   - The card moves through the columns mapped to the conditions draft → review → pm-approval → merging (merge gate) → merge-queue.
   - It goes to the changes-requested or conflicts column when those happen. Entering the conflicts column launches a conflict agent, and entering the review column runs a configured shell command (the reference setup stops its local backend there) `{USER}`.
   - The workspace is deleted on merge or close.
7. **CI triage.** A red PR you wrote gets one background agent per failed run. It reruns noise once, fixes clear failures and pushes, and leaves the rest red.
8. **Stacks.** Group by Stack in the sidebar. The stack map shows merge readiness and merge-queue state; there are "Merge stack" and "Rebase stack" actions (the latter names the reference setup's stack-rebase skill `{USER}`).
9. **Review inbox.**
   - Others' PRs where you are a requested reviewer are cloned into a review workspace automatically (in the column mapped to `reviewing`). The review agent runs with a prompt. The layout puts the agent on the left and the first file plus the PR on the right.
   - The workspace head follows force-pushes.
   - You queue comments locally and submit one review with a verdict.
   - The workspace is deleted after you review. A re-request clones it again with an interdiff since your last review.
10. **Hold column.** Freezes all automation for a workspace.
11. **Settings travel.** Export and import JSON through the UI or the `orca settings` CLI. The fork has its own update channel and Homebrew cask.

---

## 2. Feature areas

### 2.1 Projects and workspaces (upstream base)
- [MUST] **Add a project** (local repo or clone) with a per-repo base ref (e.g. `origin/main`). `store/slices/repos.ts`, Settings → Repository.
- [MUST] **Create a workspace:**
  - The dialog takes a name (emoji shortcodes), project, start point (base, branch, SHA, PR) and an agent picker.
  - It can link a GitHub PR/issue or a Linear issue.
  - `git fetch` and `git worktree add` run in the background with progress, cancel and retry. `components/new-workspace`, `main/ipc/worktree-remote.ts`.
- [MUST] **`orca.yaml`:**
  - `scripts.setup` / `scripts.archive`, `setupAgentStartupPolicy`, `defaultTabs`.
  - `worktree.sharedDirectories`, `.worktreeinclude`, a trust prompt.
  - The setup runner (`set -e`, `ORCA_*` env) runs in a "Setup" tab. `main/hooks.ts`.
- [MUST] **Delete or archive a workspace.** The archive hook runs, then the folder and branch are removed. It confirms when the checkout is dirty and offers force. `DeleteWorktreeDialog*`.
- [MUST] **Rename / edit details**, including linking an issue (GitHub or Linear).
- [SHOULD] **Shared paths** (APFS clone) copied into each new worktree.
- [SHOULD] **Sleep / wake** a workspace. It kills the terminals and keeps the sessions resumable.
- [CUT?] Auto-hibernation of idle agents.
- [SHOULD] **Worktree back/forward history** (Mod+Alt+←/→).
- [SHOULD] **Open in external editor.** [FORK] Mod+O opens the selected file or worktree in the first "Open in" app (commits 73f7f0e3ca, 4a3c18d599).
- [LATER] Parent/child workspace nesting.
- [CUT?] Sparse checkout presets, folder (non-git) workspaces, project groups, Resource Manager cleanup.
- [MUST] Workspace status columns and the **Workspace Board** (kanban). The columns are user-defined (§3).
- [SHOULD] Hidden/external worktrees: Show/Hide for `git worktree add` worktrees that Orca did not create.
- [MUST] **Hide temporary checkouts.** Hides worktrees under `/tmp`, `/var/folders` etc. (agent scratch). FORK §9.
- [SHOULD] A workspace whose branch only tracks trunk is not matched to trunk's PR, so it is not deleted (18e3814f6f).

### 2.2 Sidebar and workspace cards
- [MUST] **Workspace list:**
  - Group by none | workspace-status | repo | pr-status | **stack** [FORK §67].
  - Sort by name | smart | recent | repo | manual (drag).
  - Text filter, unread bold, pin, multi-select, context menu.
  - Filters: hide sleeping, default-branch, detached-HEAD, temporary checkouts.
- [MUST] **Group by Stack.** One section per GitHub stack ("Stack #N"), ordered from the PR furthest from main down; newest stack first, then "Not in a stack". Workspaces in the column mapped to `reviewing` go first. FORK §67, PRs 166, 190–194, 210. `{GH}`
- [MUST] **Card contents:**
  - Title, branch, agent rows with state glyphs.
  - PR status icon: amber spin while checks run, red only once nothing is running (PRs 110, 111).
  - CI chip, ports, host badge.
- [MUST] **Card display options** (each toggleable):
  - Workspace status chip [§67].
  - PR author avatar.
  - Linear title [§7] `{LIN}`, Linear labels as colored chips [§70] `{LIN}`.
  - PR diff +N/−N [§80, PRs 212, 214].
  - Project chip [§82].
- [SHOULD] Mod+1…9 selects a workspace by index; Mod+Shift+↑/↓ navigate.
- [SHOULD] Mod+J worktree jump palette: recent sessions, worktrees, PR `#123`, tabs.
- [CUT?] Sidebar entries for Tasks, Activity feed, Agent Dashboard, Automations, Artifacts, Skills, Setup Guide.

### 2.3 Workspace statuses and automations (detail in §3)
- [MUST] **Status rules** ("Move workspaces by pull request state"). They map 10 PR conditions to user columns, polled every 60 s. FORK §3, PRs 79, 87, 95, 105, 221. `{GH}`
- [MUST] **Delete on merge/close** (`onResolved`) and **close a review workspace after review** (`onReviewed`). §3, §4.
- [MUST] **Review inbox.** Auto-clone others' PRs that request your review, with the review agent, prompt templates and a view choice. §3, §8, §50.
- [MUST] **Review head sync** (follows force-pushes) and **re-review interdiff** base. §8, §4.
- [MUST] **Conflict agent** on entering Conflicts; **review-entered command**. §3, §8.
- [MUST] **Hold column.** §83, PR 219.
- [MUST] **New workspaces start in** a column. Commit 0743817901.
- [MUST] **Merge-queue condition.** §78, PR 207.
- [MUST] **CI triage** (per repo). §64, PRs 128, 130–133, 135, 187.
- [MUST] **Top-bar next action and Review button** with editable prompts. §13, PRs 143–146, 153, 155.

### 2.4 Tabs, panes, layout (upstream base)
- [MUST] **Tab groups and splits.**
  - Tabs split by dragging to a pane edge, nested; the layout is saved per worktree.
  - Tab types: terminal, agent, editor, diff, markdown, browser, PR page.
  - `store/slices/tabs.ts`, `tab-group-state.ts`.
- [MUST] **Tab operations:**
  - Pinned tabs, rename, reopen closed (Mod+Shift+T), MRU Ctrl+Tab.
  - Next/prev all-types and same-type.
  - Mod+W closes (a terminal pane first).
- [MUST] **Move focus between tab groups by direction** (⌥⌘←/→/↑/↓). Picks the nearest edge-sharing group, no wrap. FORK §6.
- [SHOULD] **⌥⇧⌘W: keep only the active tab** and clear the other splits. A terminal with a running process is kept unless its agent reports done. PRs 83, 89, 134, 138.
- [SHOULD] New-tab omnibox (tabs/files/URLs/agents).
- [CUT?] Floating terminal / floating workspace.
- [LATER] Modal blur via a body attribute (§59) and popover outside-press close (§79). These are Electron/Radix workarounds and do not port.

### 2.5 Terminals and the daemon (detail in §4)
- [MUST] **PTYs owned by a detached terminal daemon.** Sessions survive quit, update and crash. Warm reattach and cold scrollback restore. `main/daemon/*`.
- [MUST] **Terminal emulation:**
  - xterm.js features: WebGL, search (Mod+F), web links, OSC 8, unicode11, images.
  - Login shell with OSC 133 integration, OSC 7 cwd, OSC 52 clipboard.
  - Kitty keyboard protocol, Option-as-Alt.
- [MUST] **Splits inside a terminal tab** (Mod+D / Mod+Shift+D), focus next/prev pane, expand pane, Mod+K clear.
- [MUST] **macOS line-editing keys** (Cmd/Opt + arrows/backspace) translated to readline sequences. `terminal-shortcut-policy.ts`.
- [SHOULD] Link click popover (Orca browser / system / copy); Cmd-click opens directly.
- [SHOULD] Terminal themes, Ghostty config import.
- [SHOULD] Drag files onto a terminal to paste their paths.
- [SHOULD] Copy Context.
- [SHOULD] Manage sessions: kill all, restart daemon, Quit Completely.

### 2.6 Agents, Claude web view, chat
- [MUST] **Launch Claude Code in a PTY.**
  - Default flags give full autonomy (`--dangerously-skip-permissions`) and are editable.
  - The prompt is injected via argv or paste (`--prefill` for drafts).
  - A folder-trust preflight writes `~/.claude.json`.
  - `shared/agent-launch-*.ts`, `main/agent-launch`. `{CC}`
- [MUST] **Agent status detection:**
  - Orca-managed hooks in `~/.claude/settings.json` POST to a loopback hook server.
  - Fallbacks: OSC 7501, then the terminal title.
  - States: working / waiting / done / blocked / idle.
  - `main/claude/hook-*`, `main/agent-hooks/README.md`. `{CC}`
- [MUST] **Background agent sessions** (`launchAgentBackgroundSession`): an unfocused PTY tab with a title. Used by review, conflict, CI triage and top-bar actions.
- [MUST] **Claude web view.**
  - The View menu picks Terminal | Chat UI | Claude web. Claude web covers the pane with `https://claude.ai/code/<bridgeSessionId>`, read from `~/.claude/sessions/<pid>.json`.
  - Default view setting `openClaudeTabsInWebView`; it auto-opens for new sessions.
  - FORK §18, PRs 14, 16, 73, 81, 147, 162. `{CC}`
  - Related: §45 tab drags over it (PRs 56, 76); §47 group-focus arrows forwarded from the guest; §48 reload when a delivered prompt is not reflected (PR 60); §74 paired hosts (PR 203).
- [SHOULD] **Chat UI** (native structured chat via `@anthropic-ai/claude-agent-sdk`). Mod+Shift+J toggles chat/terminal. It is one of the review inbox's view options (default `chat`). Arrow keys step through images in its preview (PR 176). Large surface; decide whether the web view alone is enough.
- [SHOULD] Notifications: an agent finished or needs you (system notification, sound, Dock badge, header bell).
- [SHOULD] Agent session history ("AI Vault"): search `~/.claude` transcripts and resume with `claude --resume`.
- [SHOULD] Keep awake (caffeinate) while agents work.
- [CUT?] The 43 other agents (codex, gemini, opencode, aider, cursor, …) and their hook listeners. Keep the agent abstraction pluggable, but ship Claude only.
- [CUT?] Agent Dashboard, Activity feed, Agent Teams, orchestration/runs, scheduled automations, account hot-swap, usage/rate-limit bar, pet overlay, skills marketplace, voice dictation.

### 2.7 Agent turns and snapshots
- [MUST] **Agent turn snapshots** (FORK §31, PRs 37, 43).
  - Hook `turnStartedAt` snapshots the worktree; `done` snapshots it again.
  - The snapshot is `git add -A` into a temp index (`GIT_INDEX_FILE`), so the real index is never touched.
  - The end snapshot is a commit with the start as parent and the turn JSON (prompt, agent, pane, times, stats) as its message, under `refs/worktree/orca-turns/<completedAt>-<oid>`.
  - Empty turns are skipped; the newest 50 are kept per worktree.
  - A wake-up after a stop resumes and replaces the turn.
  - `main/agent-turns/*`.
- [MUST] **Agent turns section** in Source Control. Clicking a turn filters the Changes list to it. §46, PRs 57, 71, 82.
- [SHOULD] **⌥⇧⌘T** opens/toggles the latest turn. §33.
- [LATER] Horizontal trackpad scroll in combined diffs. §33.

### 2.8 Notes (diff notes ↔ agent)
- [MUST] **Diff notes:**
  - Gutter "+" or Mod+Shift+A adds a note; Mod+Enter saves.
  - "Send notes to an agent" menu with an auto-picked agent. Mod+Enter → Enter → Enter sends in three presses (§19).
  - Notes are kept after send, marked `sentAt`, and carry "Note id:" lines (§21).
- [MUST] **Agent notes:**
  - `orca notes add --file --line [--end-line] --body [--agent]`, plus `list`, `rm` and `reply --id`.
  - Stored in `WorktreeMeta.agentNotes`, rendered as cards with the agent icon, with replies nested.
  - Agent notes are markdown; a literal `\n` in the CLI body becomes a newline (§49).
  - An agent-notes shelf and per-file badges in Source Control.
  - FORK §21, §49, PRs 8, 10, 12, 13, 17, 61, 66, 85, 86, 96.
- [MUST] **A follow-up reply under a note goes straight to an agent.** §53.
- [SHOULD] **Message prefix field** at the top of every "Send to an agent" menu. §84, PR 222.
- [SHOULD] **Notes UX:**
  - "<agent> is thinking…" spinner (PR 98); a question counts as answered after the agent's next turn (PR 113).
  - Collapse a note thread (PR 170).
  - @-mention workspace files in notes, ranked like quick open, gitignored excluded (PRs 102, 107, 116).
  - Drafts persist in localStorage for 30 days, capped at 300 (PR 99).
- [SHOULD] **One shared Composer** for every box that writes to an agent or GitHub. GitHub boxes can @-tag files, inserted as a link to the PR branch. PRs 117, 129, 148.
- [LATER] The new-comment box opens a view-zone gap instead of covering code (§41). One line copied from the editor pastes as text (§62).
- [CUT?] Open PR 118: a copy button per note.

### 2.9 Source control and diffs
- [MUST] **One Changes list.**
  - Every file that differs from the branch base, committed or not, listed once and live (§35, PR 45). The `sourceControlGrouping='all'` default replaces staged/unstaged/untracked sections.
  - The header totals +/− for the listed files (§55).
- [MUST] **Filters on Changes:**
  - File type: implementation, tests, docs, generated, agent guidance, localization, assets (§17).
  - Lint baselines, plus Backend / Frontend areas from path prefixes hardcoded for the reference work repo (§65) `{USER}`. These should become config-driven path globs.
  - Imports only (§66).
  - ⌥⌘U uncommitted only (§38).
  - Turn/commit filter (§46).
- [MUST] **Discard** from a Changes row (§63). Stage, unstage and discard; commit with Mod+Enter; amend; push / force-with-lease / pull / sync. Upstream `right-sidebar/source-control`.
- [MUST] **Merge from the top bar.** The top bar's next-action button merges the open PR or toggles auto-merge. This moved out of Source Control in commit 058840d (2026-10-22); see source-control.md. Nothing merges with uncommitted changes. PRs 137, 139, 140, 143. `{GH}`
- [MUST] **Diff viewer.**
  - Monaco diff; combined "all changes" diff with a file tree.
  - Scope: base / commit / branch.
  - Next/prev change with F7 / Shift+F7. A change taller than the viewport is paged through first (PR 165).
  - Next/prev changed file with ⌥F7 / ⌥⇧F7, and review-comment links (§2, PR 1).
- [MUST] **Editor and diff as one tab's two views.** ⌥⌘E edit, ⌥⌘D diff, opened at the cursor's line. §58, PRs 180, 181.
- [SHOULD] **Source Control sections:**
  - Review, Pending review, Agent notes, Agent turns, Changes, Commits.
  - Drag to reorder, hide, collapse; one scroll.
  - §32 (superseded), §36, §43, §52, PRs 39, 46, 54, 65.
- [SHOULD] Centre a diff's first change after layout (§20).
- [SHOULD] Keyboard ↑/↓ scroll 40 px in the combined diff (§44).
- [SHOULD] Open branch diffs retarget when the diff base changes (§23).
- [SHOULD] A worktree opens on Source Control the first time, then on the last-used right-sidebar tab (PR 169).
- [MUST] **The right sidebar follows the active file's kind:** source control for a diff, the explorer for a file (§29).
- [SHOULD] Conflict review, abort merge/rebase (upstream).
- [LATER] Upstream AI actions: generate commit message / PR details.
- [MUST] **Explorer** (live tree, create/rename/delete/move with undo, git-status colors, copy path) and **Search** (ripgrep: case, regex, include/exclude). Upstream.
- [SHOULD] **Material Icon Theme** icons everywhere (§28).
- [CUT?] No Create PR in the Source Control header (§51): the agent or CLI creates PRs. AI-vs-human line attribution.

### 2.10 Code editor and language servers
- [MUST] **Monaco editor** (to be replaced):
  - Syntax highlighting, autosave on blur/idle, Mod+S save, Alt+Z word wrap.
  - Find/replace, large-paste guard.
  - Image, PDF, CSV, Mermaid and HTML viewers (only the image viewer is a MUST).
- [MUST] **Language servers** (FORK §26, PRs 19–25, 28, 29, 31, 32, 44).
  - The `LANGUAGE_SERVERS` registry (`shared/language-server.ts`) lists each language's Monaco ids, extensions → LSP language id, candidate servers and default.
  - Python: pyrefly | Pyright | basedpyright | ty. TypeScript: tsgo | typescript-language-server.
  - A server is found in project tool dirs (venv `bin/`, `node_modules/.bin`), else on `PATH`, else a custom path.
  - Settings per language: `{enabled, server, command}`.
  - Provides definition, hover, references and Peek.
  - One server per project root, at most 4, 15-minute idle shutdown.
  - `{USER}` server list; it is already a registry, so keep it data-driven.
- [MUST] **Cmd+click go to definition**, including from inside Peek (§34). It is slow today; see §6.
- [SHOULD] **Diagnostics and completion.**
  - Diagnostics: oxlint (`oxlint --lsp`, PR 177); type errors from tsgo/pyrefly via pull diagnostics (PR 178).
  - Completion (PR 179), auto-imports via `completionItem/resolve`, quick fixes, Shift+Alt+O organize imports (§61, PR 184).
- [SHOULD] **Mod+S formats** with the project's oxfmt (`oxfmt --lsp`) or Ruff (`ruff server`) as one undo step; autosave never formats. §60, PR 183. `{USER}` tools; make formatters config-driven.
- [SHOULD] **Restart Language Server** (context menu, F1, settings) (PRs 103, 163).
- [SHOULD] `didChangeWatchedFiles`, so symbols an agent just wrote resolve (PRs 112, 159).
- [SHOULD] Deleting a workspace stops its servers (PR 122).
- [SHOULD] **Git Blame for the cursor's line.** Inline "Author, 2 days ago • summary"; the hover shows the PR and a "show commit" link that filters Changes. §27, PR 27.
- [SHOULD] **Editor theme from a VS Code theme file**: `~/.orca/themes/editor-dark.json` / `editor-light.json`. §5.
- [LATER] Markdown: raw editor plus preview. The rich TipTap editor is [CUT?].

### 2.11 PR review and comments (GitHub) `{GH}`
- [MUST] **Review mode.** Inline comments are queued locally in `<userData>/pending-review-drafts.json` (atomic writes) and submitted as one review with Approve / Request changes / Comment through GraphQL `addPullRequestReview`. FORK §4, PRs 3, 93, 208.
- [MUST] **Diff base picker.** "Since your last review" (default, `viewerLatestReview.commit.oid`), the whole PR, or any PR commit tagged with your reviews. §4, §14, PR 92.
- [MUST] **Reviewers list** in the Review section: standing, re-request button, request/unrequest people or `org/team` (§4, §77). Re-request is disabled while there are uncommitted changes (PR 136).
- [MUST] **PR panel** (Checks tab).
  - Needs attention (failing checks, conflicts), Checks, Pending review, Comments, Resolved: draggable and hideable (§37, §40).
  - Comment tabs: Humans (the default) / Bots / Unresolved (§81, PRs 84, 213, 216, 217).
- [MUST] **Inline PR threads in the diff:**
  - Reply, resolve.
  - Outdated threads stay, placed at the end (§22).
  - Context lines are commentable (PR 173).
- [MUST] **Queue PR comments for the agent**, from the diff and the panel (§10). The status bar shows "N queued", and ⌥⌘S sends them (§39). Sending leaves the threads open, with no "Fixing" replies (§12).
- [SHOULD] **Comment composer extras:**
  - @-mentions (§11, PR 94); emoji `:shortcodes:` (§24, PRs 9, 11).
  - Paste or drop images and videos (§15). Uploads go to GitHub's own user-attachments endpoint; the earlier separate bucket was removed in 02caec50b9. See github-review.md.
  - An @login hover card (PR 119); who reacted, on hover (§42).
- [SHOULD] **Edit your own comments:** inline comments (§4), review summaries (PR 104). Delete inline comments via GraphQL (PR 171).
- [SHOULD] **Review summaries** show their verdict badge (§56). ⇧⌥⌘R focuses the summary box (§54).
- [MUST] **Review workspace layout:** the agent on the left; the first changed file plus a PR browser tab on the right (§50, PRs 62, 64, 211).
- [SHOULD] Upstream PR page (conversation, files, timeline), Create PR composer, issues drawer. Mostly replaced by agents in this workflow.

### 2.12 Checks, CI triage `{GH}`
- [MUST] **Checks list.**
  - Elapsed/running duration (PR 124).
  - Rerun failed/all from Needs attention (PR 121).
  - Job logs.
  - A pending commit status counts as queued (PR 111).
- [MUST] **CI triage.** One background agent per failed run, with an editable per-repo prompt. Details in §3.7.
- [SHOULD] Mod+Shift+K opens the Checks tab (PR 158).
- [SHOULD] Fix: the PR comment fetch no longer loops while the Checks tab is open (0350d3e4c6).

### 2.13 Stacks and merge queue `{GH}`
- [MUST] **Stack map** in the Checks panel.
  - Badges follow `mergeStateStatus`: CLEAN / HAS_HOOKS / UNSTABLE → Ready; BLOCKED → Not ready (§69, PRs 195–197).
  - "Queued" when `isInMergeQueue`, and "Merge stack" is hidden then (§76).
- [MUST] **Merge queue state per PR.** An extra `isInMergeQueue` query; the header badge reads "queued", with no "Merge when ready" (§78).
- [SHOULD] **Rebase stack.** Launches a conflict-resolution agent whose prompt names the reference setup's stack-rebase skill and allows force-push (`right-sidebar/github-pr-stack-rebase-prompt.ts`) `{USER}`. Make it a template.
- [MUST] Group by Stack (2.2).

### 2.14 Linear `{LIN}`
- [MUST] **Create a workspace from a Linear issue** (Linear's branch name). Link/unlink an issue on a workspace. `main/linear`, `@linear/sdk` with a personal API token.
- [MUST] **Card shows the Linear title and labels** (§7, §70). **⌥⌘L opens the issue** (§71).
- [SHOULD] Tasks drawer: list/board, issue detail editing (status, assignee, priority, labels, estimate), new issue.
- [SHOULD] Opt-in: move the issue to In Progress when its worktree is created.
- [SHOULD] `orca linear …` CLI (issue/search/list/create/status/assignee/label/comment …). Agents use it.
- [CUT?] Jira, GitLab, Bitbucket, Azure DevOps, Gitea, GitHub Projects.

### 2.15 Quick Open, palettes, quick commands
- [MUST] **Quick Open (Mod+P).**
  - Ranks filename first: exact, prefix, substring, fuzzy, then path.
  - Dependency folders go last; gitignored files rank second.
  - `%` text search, `:` go to line (§30, PRs 35, 36).
  - Changed files open as their diff, with line counts; text hits open at the match (PRs 149, 150).
- [MUST] Mod+J worktree palette (upstream).
- [SHOULD] **Quick commands.**
  - Saved commands/prompts, global or per project.
  - The tab-bar split button runs one in any idle terminal and toggles while running (PRs 115, 120).
  - ⌥⌘P opens the menu (PR 126).
  - Host filter (§75); button sizing (§68, cosmetic).

### 2.16 Ports, preview, browser
- [SHOULD] **Ports panel.** Listening ports per worktree (PID attribution), advertised dev URLs.
- [SHOULD] **Port names** from a per-repo URL command (e.g. `make print-urls`) that labels the ports (PR 106) `{USER}` example.
- [SHOULD] A top-bar button and Mod+Shift+B open the running frontend dev server (PRs 108, 151, 152).
- [SHOULD] **Preview deployment button.** Parses a deploy bot's PR comments (a fixed text pattern naming the commit SHA and the URL, one comment per app) and opens the newest. Its tooltip gives the commit, age and whether it is behind the head (§25, PRs 18, 220) `{USER}`; make the pattern configurable. Mod+Shift+P opens it (PR 158; open PR 154 overlaps).
- [SHOULD] Embedded browser tab, used by the review layout to show the PR page.
- [CUT?] Design Mode, CDP automation, cookie import, extensions, profiles, and the `orca` browser CLI (~65 commands).

### 2.17 Settings, export/import
- [MUST] Settings panes: General, Appearance, Git, Terminal, Agents, Integrations (GitHub, Linear token), Notifications, Repository, Shortcuts (full remap; `~/.orca/keybindings.json`), Automations (status rules, top-bar prompts), Language Servers, Experimental (Chat UI default view).
- [MUST] **Persistence compatibility.** State is in `~/Library/Application Support/orca/profiles/<profileId>/profile-state.db` (SQLite `profile_state_documents`, domains `ui`, `repos`, `worktreeMeta`…). The legacy file is `orca-data.json`. To carry over an existing setup, the rewrite needs an importer, or must read these directly.
- [SHOULD] **Share settings.** One JSON (`format:"orca-settings", version:1`) with a secrets denylist; the import merges (§16, PRs 160, 161, 202). `orca settings export|import` (§73).
- [CUT?] Settings panes for SSH, remote servers, voice, plugins, artifacts, mobile, cloud.

### 2.18 CLI (`orca …`)
- [MUST] `orca notes add|list|reply|rm`: the agent → diff channel [FORK].
- [MUST] `orca worktree list|show|current|create|rm|ps|set`; `orca terminal list|show|read|wait|stop|create|switch|rename|split|close|send|set-pane-title`; `orca agent launch`; `orca open`, `orca status`, `orca agent-context`. Agent skills (e.g. an `orca-cli` skill) drive Orca through them.
- [SHOULD] `orca repo …`, `orca file open|diff|open-changed`, `orca reference …`, `orca ports`, `orca search`, `orca linear …`, `orca settings export|import`.
- [CUT?] Browser, jira, computer, emulator, environment/serve/host, orchestration, automations, connector, artifacts, skills, speech, vm, account.
- §57 `orca file open --line/--column` is retired; upstream has it.

### 2.19 Release and update
- [MUST] **Own update channel.**
  - The feed is the fork's GitHub releases (`latest-mac.yml`); release URLs come from env/build flags (`VITE_ORCA_RELEASES_REPO`).
  - **Self-install on macOS:** download the zip in-process (avoids quarantine), check the manifest sha512, back up the settings JSON, replace `/Applications/Orca.app`, keep `Orca.app.previous`, relaunch. `ORCA_UPDATE_NO_SELF_INSTALL=1` opts out.
  - FORK §1, `fork-install/`.
- [SHOULD] Version line: fork patch +1 per release; the title carries the upstream version; two-part release notes. FORK "Releasing".
- [SHOULD] **Homebrew cask** `frbarbre/orca` with `version :latest` (§72).
- [LATER] **PR preview builds** as a separate app (`com.stablyai.orca.preview`, own userData), tagged `preview-pr<N>-<sha>`.
- [CUT?] Linux release (§74), Windows, RC/perf channels.

### 2.20 Upstream areas: likely cut
- **Platforms and remote:**
  - Mobile companion (`mobile/`, `components/mobile`).
  - Cloud (`cloud/`, connector-cloud, ephemeral VMs).
  - SSH worktrees, paired hosts, remote Orca server (`main/ssh`, `orcad`, `main/host`). Note that several fork features (§4 drafts file, §74) carry remote-host compatibility code that the rewrite can drop.
  - Windows/WSL (`main/wsl`, `main/windows`, pwsh).
  - Linux AppImage updater.
- **Integrations and extensibility:** other agents, other task providers, plugins and marketplaces.
- **Instrumentation and onboarding:** telemetry (PostHog), crash reporting, onboarding/tours/feature wall, i18n (11 locales).
- **Misc:** emulator/simulator tabs, computer use, voice, app icon variants.

---

## 3. Workspace status automations: implementation spec (absolute requirement)

### 3.0 Where it lives
Pure logic (port it line by line):
- `shared/workspace-status-rules.ts`: condition resolver.
- `shared/workspace-status-rule-plan.ts`: planner.
- `shared/workspace-status-rule-config.ts`: config, defaults, normalization.
- `shared/workspace-status-rule-prompt.ts`: template rendering.
- `shared/workspace-status-rule-conflict-prompt.ts`.
- `shared/workspace-new-status-plan.ts`.
- `shared/workspace-action.ts` and `workspace-action-prompts.ts`: top bar.
- `shared/ci-triage.ts`.

Side effects and fetch:
- Side effects: `renderer/src/components/workspace-status-rules/*` (poller `use-workspace-status-rule-poller.ts`, `apply-workspace-status-rule-plan.ts`, `create-review-workspace.ts`, `launch-conflict-agents.ts`, `run-review-entered-commands.ts`, `apply-review-agent-view.ts`, `open-review-workspace-layout.ts`).
- GitHub fetch: `main/github/review-status-snapshot*.ts`.
- Head sync: `shared/github/review-head-sync.ts` and `main/github/review-head-sync.ts`.
- Interdiff: `main/github/review-base.ts`.
- Command runner: `main/github/workspace-command.ts`.
- CI triage: `renderer/src/lib/ci-triage-{trigger,watch}.ts`, `app-shell/use-ci-triage-watch.ts`, `main/ci-triage-log.ts`.
- Settings UI: `renderer/src/components/settings/PullRequestStatusRulesSection.tsx` and `PullRequestStatusRuleTabs.tsx`, copy in `pull-request-status-rule-copy.ts`.

**Compatibility suite.** Port these tests as fixtures:
- Rules core: `shared/workspace-status-rules{,-conflicts}.test.ts`, `workspace-status-rule-{plan,config,prompt,conflict-prompt}.test.ts`, `workspace-new-status-plan.test.ts`, `workspace-statuses.test.ts`.
- Snapshot: `main/github/review-status-snapshot-{mapping,merge-gate,mergeable}.test.ts`, `pr-merge-queue-membership.test.ts`.
- Side effects: `renderer/.../workspace-status-rules/*.test.ts`.
- Head sync and interdiff: `shared/github/review-head-sync.test.ts`, `main/github/review-head-sync.test.ts` and `review-base.test.ts` (real git), `shared/github/pull-request-reviewers.test.ts`.
- CI triage: `shared/ci-triage.test.ts`, `renderer/src/lib/ci-triage-{trigger,watch}.test.ts`, `main/ci-triage-log.test.ts`.
- Settings and actions: `shared/workspace-action-prompts.test.ts`, `shared/settings-transfer.test.ts`.

### 3.1 Statuses (columns)
- **Model.** `WorkspaceStatusDefinition = {id, label, color?, icon?}`. The columns are fully user-defined, and array order is board order. The per-workspace value is worktree meta `workspaceStatus?: string`.
- **Upstream defaults:** `todo` Todo · `in-progress` In progress · `in-review` In review · `completed` Done. `DEFAULT_WORKSPACE_STATUS_ID='in-progress'` is used for display only. The rules read the raw stored value (`?? null`).
- **A customized board** (illustrative; the reference setup's real columns are in `private/reference-setup.md`, not in the repo). **Importers must keep the ids; the labels are cosmetic.** Ids created in the UI look like `status-N` and need not match the labels.

  | id | label | color | icon | mapped condition |
  |---|---|---|---|---|
  | `status-1` | Conflicts | rose | circle-alert | conflicts |
  | `status-2` | Their PRs | sky | git-pull-request | reviewing |
  | `status-3` | Ready to merge | emerald | circle-check | merging |
  | `in-review` | In review | conductor-review | conductor-review | review |
  | `todo` | Draft | neutral | circle | draft (+ new workspaces) |
  | `in-progress` | In progress | neutral | circle-ellipsis | — |

  Conditions left unmapped (here `merged`, `closed`, `merge-queue`, `pm-approval`, `changes-requested`) "do nothing" except deletion.
- **Colors:** `neutral blue sky violet amber emerald rose zinc conductor-done conductor-review conductor-progress`.
- **Icons:** `circle circle-dot circle-progress circle-dashed circle-ellipsis git-pull-request timer flag circle-alert circle-pause circle-play circle-check ban conductor-done conductor-review conductor-progress`.
- **Normalization:**
  - Label: trimmed, whitespace collapsed, at most 32 chars, else `Status N`.
  - Id: lowercased, `[^a-z0-9_-]+`→`-`, with `-2…-99`, then `status-<base36>` on collisions.
  - An empty array becomes the defaults.
  - Migration flags: `_workspaceStatuses{DefaultOrderMigrated,ReorderedDefaultRepaired,DefaultWorkflowMigrated,DefaultVisualsMigrated}`.
- **Board UI:** `workspaceBoardOpacity` 0.2–1; `workspaceBoardColumnWidth` 308 (220–520, step 20).

### 3.2 Persisted config: `ui.workspaceStatusRules` (`WorkspaceStatusRuleConfig`)

The reference setup's values for these keys are in `private/reference-setup.md` (not in the repo).

| key | default | notes |
|---|---|---|
| `enabled` | false | master switch |
| `repoIds` | [] | **empty means the poller does nothing** |
| `statusByCondition` | {} | condition → column id; an absent key means "Do nothing" |
| `mergingCheckName` | `'Reviews satisfied'` | ≤120 chars |
| `newWorkspaceStatus` / `newWorkspaceStatusSince` | null / null | column id / ms timestamp set when chosen |
| `holdStatus` | null | column id of the hold column |
| `pmApprovalTeam` | null | `org/slug`, e.g. `acme/pm-team` `{USER}` |
| `onResolved` | `'none'` | `'none' \| 'delete'` |
| `onReviewed` | `'none'` | `'none' \| 'delete'` |
| `resolveConflictsWithAgent` | false | |
| `conflictPromptTemplate` | `''` (built-in) | ≤8000 |
| `reviewEnteredCommand` | `''` | shell command, e.g. `make dev-down` `{USER}`, ≤1000 |
| `actionPrompts.{commitAndPush,createPullRequest,readyForReview,reviewPullRequest}` | {} | each optional; blank means the built-in prompt |
| `reviewInbox.{enabled,agent,view,promptTemplate,rereviewPromptTemplate}` | false, `claude`, `chat`, defaults | |
| `handledPullRequests` | [] | ledger, last 500, discarded unless `ledgerVersion===1` |
| `ledgerVersion` | 1 | |

- **Ledger key:** `owner/repo#N` or `owner/repo#N@<reviewedCommitOid>`. An entry matches a PR key if it is equal or starts with `key+'@'`.
- **Per-repo CI triage:** `Repo.ciTriage?: {prompt?, hints?}` (repos domain). Absent means off; `{}` means the default prompt.
- **Other stores:**
  - localStorage `orca.ciTriage.memory` and `orca.devStatusRules` (dev gate).
  - `<userData>/pending-review-drafts.json`.
  - `<userData>/logs/ci-triage.jsonl`.
- **Settings export/import:**
  - The export drops `repoIds`, `handledPullRequests`, `ledgerVersion`, `newWorkspaceStatusSince`.
  - The import keeps the local values of those and sets `since=now` if the import carries a `newWorkspaceStatus`.
- A live profile DB can disagree with the logs about `repoIds` (for example empty in one document while the CI log names real repo ids). Verify which profile/document is authoritative before migrating.

### 3.3 Condition resolver (first match wins)
`resolveWorkspaceStatusRuleCondition(pr, config, pmTeamLogins, viewerLogin)`:
1. `state==='MERGED'` → **merged**.
2. `state==='CLOSED'` → **closed**.
3. The viewer is known and the author ≠ viewer (case-insensitive) → **reviewing**: someone else's PR, any open state.
4. `isInMergeQueue` **and** `merge-queue` is mapped → **merge-queue**.
5. Only if `conflicts` is mapped:
   - `mergeable==='CONFLICTING'` → **conflicts** (even a draft).
   - `'UNKNOWN'` → **null**: no move this tick.
6. `isDraft` → **draft**.
7. Unanswered change request → **changes-requested**. That is a `CHANGES_REQUESTED` review in `latestOpinionatedReviews ?? latestReviews` whose author is not currently a pending *user* reviewer. A re-request clears it; a later plain comment does not.
8. **merging** if either:
   - a check named `mergingCheckName` (case-insensitive, trimmed) is `success` (a CheckRun or a StatusContext), or
   - the PM team is non-empty, there are no pending user reviewers, and the opinionated verdicts (minus the author) are ≥1 and all `APPROVED`, with at least one from a PM-team member.
9. **pm-approval**: the PM team is non-empty, pending user reviewers ≥1, and all of them are PM-team members. Team reviewers never count.
10. Otherwise **review**.

The column is `statusByCondition[condition] ?? null`. **An unmapped condition never moves a card.**

**Viewer standing** (`readViewerReviewStanding`):
- `verdict = latestReviews(viewer)?.state ?? latestOpinionatedReviews(viewer)?.state`.
- `isOwed` = the viewer is a pending user reviewer.
- `isFinished = !isOwed && verdict ∈ {APPROVED, CHANGES_REQUESTED}`.
- `commitOid = latestReviews → latestOpinionatedReviews → viewerLatestReview` commit.

### 3.4 Poller and tick
- **Scheduling:** runs while `enabled`. `POLL_INTERVAL_MS=60_000`, run immediately, then setTimeout 60 s after each finish. Paused while the window is hidden; re-runs on focus/visible; no overlap.
- **Tick** (`runWorkspaceStatusRuleTick`). It returns early if disabled or `repoIds` is empty.
  1. Apply new-workspace column updates (`planNewWorkspaceStatusUpdates`). These apply to workspaces in scoped repos with no column, `createdAt >= since`, not archived/bare/main.
  2. Load the pending review drafts.
  3. **Scope** (`collectWorkspaceStatusRuleScope`).
     - Targets are non-archived, non-bare, non-main worktrees in scoped repos with a branch whose PR-cache entry exists (upstream's per-branch PR lookup).
     - `existingBranches` holds all branches, including archived and main worktrees.
     - Target fields: `currentStatus`, `headOid`, `hasPendingReviewComments` (drafts file or meta), `executionHostId`.
  4. **Snapshot.** One GraphQL call via `gh api graphql`, cwd = the first local scoped repo.
     - `viewer{login}`, then up to 60 aliased `repository{pullRequest(number)}`.
     - If the inbox is on: `search("is:pr is:open review-requested:@me archived:false repo:o/r …", first:30)`.
     - Fragment fields: number title url isDraft isInMergeQueue state mergeable headRefName baseRefName headRefOid author repository; reviewRequests(20) User login / Team slug; latestReviews(20) and latestOpinionatedReviews(20) {state author commit.oid}; viewerLatestReview; last commit statusCheckRollup contexts(100) (CheckRun name/status/conclusion, StatusContext context/state).
     - The PM roster comes from `gh api --paginate /orgs/<org>/teams/<slug>/members`, cached 1 h; on error the stale list is used, else [].
     - Rate-limit guard blocked, or any error, gives an **empty snapshot**, which means no actions.
     - **Mapping:**
       - state: MERGED/CLOSED, else OPEN.
       - mergeable: MERGEABLE/CONFLICTING, else UNKNOWN.
       - CheckRun: not COMPLETED → pending; SUCCESS → success; NEUTRAL/SKIPPED → neutral; other → failure.
       - StatusContext: SUCCESS → success; FAILURE/ERROR → failure; else pending.
       - Reviews: kept only if APPROVED/CHANGES_REQUESTED/COMMENTED/DISMISSED/PENDING with an author.
  5. Set PR authors (card avatars).
  6. **Plan** (`buildWorkspaceStatusRulePlan`). For each target not in the hold column whose PR is in the snapshot:
     1. **Resolved removal.** Condition merged/closed, `onResolved==='delete'`, and `target.headOid === pr.headRefOid` (guards against an unrelated old PR). Unforced.
     2. **Reviewed removal.** Condition reviewing, `onReviewed==='delete'`, no pending review comments, and (`standing.isFinished` or `wasUnassigned`). **Forced.** `forgetHandledKey=owner/repo#n` only if the PR is not in this tick's inbox.
        - `wasUnassigned` requires all of: the viewer is known, the inbox is on, the PR is not in the inbox results, no team requested, the viewer is not owed, the viewer has no review, and the ledger has the PR (the inbox created it).
     3. **Status move.** The mapped status differs from `currentStatus` (null counts as different).
     4. **Inbox creations** (if the inbox is on). For each inbox PR, key = `makeHandledPullRequestKey(repo, n, standing.commitOid)`. It is skipped if:
        - the ledger has the exact key;
        - any target links the PR (held included);
        - the author is the viewer;
        - the branch already exists.

        At most 30 per tick. There is no seeding: every outstanding request is cloned on the first tick.
  7. **Apply.** Status updates in parallel → removals sequentially (toast `Kept <name>: <reason>` on failure) → forget keys after successful removals → creations sequentially.
  8. Conflict agents → review-entered commands → head sync of released targets.

### 3.5 Actions in detail
- **Delete workspace.** `removeWorktree`.
  - Resolved removals are unforced: git refuses a dirty checkout or a live agent.
  - Reviewed removals are forced.
  - The orca.yaml archive script runs first.
- **Create review workspace** (`create-review-workspace.ts`):
  1. Repo pick: a scoped repo whose path ends with `owner/repo` or whose upstream matches, else the first scoped repo.
  2. `resolveGitHubPrStartPointForRepo` gives baseBranch, pushTarget, branch override and compareBaseRef.
  3. Setup-hook trust check.
  4. Name `review-<n>-<slug(title)≤40>`, display title = PR title, linked PR, initial status = `statusByCondition.reviewing`, base ref = `sinceReviewCommit ?? compareBaseRef`.
  5. Setup and default tabs run in the background.
  6. **Re-review interdiff** (`main/github/review-base.ts`).
     - If the reviewed commit's merge-base with the target equals HEAD's and it is an ancestor of HEAD, the base is the reviewed commit.
     - Otherwise `git merge-tree --write-tree --merge-base <reviewedBase> <currentBase> <reviewed>` + `commit-tree -p currentBase` (author Orca <orca@localhost>), giving the ref `refs/orca/review-base/<sha1(path)[0:16]>-<commit[0:12]>`. Older refs with the same prefix are deleted. A note is appended to the prompt (plus a conflict note).
     - Refs under `refs/orca/review-base/` are exact compare bases (no merge-base).
  7. Launch the agent: `launchAgentBackgroundSession({agent: reviewInbox.agent, prompt, title: 'Review #N'})`. The prompt is the re-review template if `sinceReviewCommit`, else the first-review template.
  8. **View**, by `reviewInbox.view`:
     - `default`: no-op.
     - `claude-web`: prefer the web view.
     - `terminal` / `chat`: disable the web view and set the mode. Chat waits until the agent is detected on the pane.
  9. **Layout:** the agent tab on the left; the first changed file (alphabetical) as a branch diff vs base in a right split plus a browser tab with the PR URL (selected); refocus the agent without stealing focus.
  10. Success: ledger `markPullRequestHandled`, toast `Reviewing #N — title`. Failure: up to 3 attempts per key, counted in memory only, so a restart retries.
- **Head sync** (follow force-pushes). Candidates: released, viewer known, someone else's PR, local, OPEN, head ≠ local HEAD, `headRefName === branch`. Steps:
  1. The OID is 40 hex.
  2. HEAD ≠ the OID.
  3. On the branch, not detached.
  4. `status --porcelain --untracked-files=no` is clean.
  5. `fetch origin +refs/heads/b:refs/remotes/origin/b`.
  6. `cat-file -e oid^{commit}`.
  7. No local-only commits by `user.email` (`log --format=%ae oid..HEAD`).
  8. `reset --hard oid`.

  Results: current / other-branch / dirty / unreachable / own-commits / updated, logged to the console only.
- **Conflict agent.** Runs only for a `statusUpdate` with `condition==='conflicts'`, i.e. when entering the column, and only if `resolveConflictsWithAgent`. The workspace must be yours (reviewing takes precedence), not held, and actually moving.
  - Agent `reviewInbox.agent`, title `Resolve conflicts #N`, launchSource `conflict_resolution`.
  - The prompt is the custom template, else the built-in `buildConflictAgentPrompt`: upstream's PR-conflict prompt with a no-force-push rule.
  - The default template text: fetch base, merge `--no-ff`, resolve, `git merge --continue`, `diff --check`, plain push, never force (the full text is in `shared/workspace-status-rule-conflict-prompt.ts`).
  - A failure gives a toast. No retry or dedupe.
- **Review-entered command.** Runs on a `statusUpdate` with `condition==='review'` on the local host if the command is set.
  - `/bin/bash -c <cmd>`, cwd = the worktree, `process.env`, 5-minute timeout, its own process group, output tail ≤4000.
  - A failure gives a toast with the last 6 lines.

### 3.6 Templates
- **Rules engine:** `/\{\{\s*([a-zA-Z]+)\s*\}\}/g`; an unknown variable is left verbatim.
  - Variables: `prNumber title author branch baseRef url repo sinceCommit` (`sinceCommit` falls back to baseRef); `worktreePath` in the conflict prompt only.
- **Default first-review prompt:** "You are reviewing pull request #{{prNumber}} — "{{title}}" by {{author}} ({{url}}). The branch {{branch}} is checked out and the diff is pointed at {{baseRef}}. What UI changes should I check for? Analyse the changes… architecture decisions…" + `REVIEW_AGENT_NOTE_INSTRUCTION`.
  - The note instruction tells the agent to leave findings via `orca notes add --file --line [--end-line] --body '<markdown>'`.
  - Verbatim in `shared/workspace-status-rule-config.ts` and `shared/agent-note-prompt.ts`.
- **Default re-review prompt:** "You reviewed … before, and {{author}} has asked you to look again. … diff is pointed at {{sinceCommit}} … for each of my review comments say done as asked / done differently / not addressed … flag anything new."
- **Top-bar action prompts:**
  - Engine `/\{\{(\w+)\}\}/g`, no padding; 3+ newlines collapse to 2; trimmed.
  - Variables: `branch baseRef worktreePath prNumber prUrl prTitle draftMode templateRule prInstructions`.
  - Defaults for commitAndPush, createPullRequest (`gh pr create`, draft mode, template rule), readyForReview (`gh pr ready`) and reviewPullRequest are in `shared/workspace-action-prompts.ts`.
  - Resolution: `custom[id]?.trim() || default`.
- **Next-action priority** (`shared/workspace-action.ts`):
  1. A merge stopped on conflicts → Resolve conflicts (upstream `resolveConflicts` recipe).
  2. No PR → Create PR (waits for the prCache to answer).
  3. Uncommitted or unpushed work → Commit & push.
  4. PR conflicts.
  5. Draft → Ready for review.
  6. Ready, clean PR → Merge / toggle auto-merge (PR 143).

  Nothing shows on the default branch. Review is a separate button while a PR is open or draft. All of these launch `reviewInbox.agent`.

### 3.7 CI triage (independent of the rules config)
- **Enable:** per repo (project settings → "Triage failed CI with an agent"); the prompt is editable. Legacy `hints` are appended as "About this repository:".
- **Triggers:**
  1. **watch.** Sidebar-visible workspaces whose prCache entry has `checksStatus==='failure'` and state open/draft. Checks are fetched at most every 30 s per `[repoId, pr, headSha]`; it re-evaluates on prCache/repos changes and at startup.
  2. **checks.** Any `fetchPRChecks` for a triage-enabled repo.
- **Decision** (`maybeStartCiTriage`):
  1. Failed = completed with conclusion failure or timed_out, sorted by name.
  2. None linked (no URL) → `no-linked-failure`.
  3. A failed name still running → `rerunning`.
  4. No workspace on the branch → `no-workspace`.
  5. Memory is `localStorage orca.ciTriage.memory {runs:{runKey:ts}, prStarts:{prKey:{count,at}}}`, with a 7-day TTL. `runKey=[repoId,pr,checkUrl]`, keyed by run URL, not head.
  6. All runs already in memory → `already-triaged`.
  7. `prStarts>=3` → `cap-reached`.
  8. **Record memory now**, before the author check.
  9. Viewer not the author → `not-author`. The memory is still consumed (a quirk, kept for compatibility).
  10. `started`.
  11. `startFixChecksAgent` with the Source Control "fixChecks" recipe agent, else the default agent, `commandInputTemplate` default `{basePrompt}`, in a new background tab with `submit-after-ready` → `launched` or `launch-failed`.
- **Prompt:** "CI failed on pull request #N at <sha>. Failed checks:\n- name: url\n\n" + instructions.
  - The default instructions are a 5-case decision: noise → rerun once if `run_attempt==1` (it mentions a specific error-tracker MCP `{USER}`); behind main → merge `origin/main` `{USER}` (hardcoded `main`); clear fix; a test the change broke; otherwise leave red.
  - Never force-push. The final line is "Reran: / Fixed: / Left red:".
  - Verbatim in `shared/ci-triage.ts`.
- **Log:** `<userData>/logs/ci-triage.jsonl`, rotated at 2 MiB to `.1`.
  - Fields: trigger, repoId, prNumber, branch, headSha, failed[], decision, runs, worktreeId, prStarts, error, at, window, pid, version.
  - Non-start decisions are deduped per window per `[decision, repo, pr, sha]`.

### 3.8 Hold column
A raw `workspaceStatus === holdStatus` filters the target out before planning. It gets no moves, no removals (resolved or reviewed), no conflict agent, no review command and no head sync. It **still counts** toward linked PRs and existing branches, so the inbox never re-clones it, and its author avatar still updates. Moving it to any other column releases it on the next tick.

### 3.9 Hardcoded vs configurable

**Configurable today:**
- The column set and visuals.
- The condition → column mapping per condition, including "do nothing", which also disables the merge-queue and conflicts conditions entirely.
- Project scope.
- The merge-gate check name and the PM team.
- Delete on resolve; close after review.
- New-workspace column; hold column.
- Conflict agent on/off and its prompt.
- The review-entered command.
- Inbox on/off, agent, view and both prompts.
- The four top-bar prompts.
- CI triage per repo and its prompt.

**Hardcoded** (the rewrite should lift these into config while keeping the same defaults):
- **The condition vocabulary and precedence** (10 fixed conditions, first match wins). "PM approval" and "merge gate" are the reference user's org process `{USER}`, built into the engine.
- **Provider:** GitHub only `{GH}` (GraphQL shape, `gh` CLI, `review-requested:@me`, team-members REST). There is no Linear-state trigger; Linear is display only.
- **Timing and limits:** 60 s poll; 60 linked PRs per query; 30 inbox creations per tick; 3 create attempts; ledger of 500; 1 h roster TTL; CI throttle 30 s; CI cap 3 per PR per 7 days; command timeout 5 min.
- **Triggers:** the conflict agent fires only on entering via `conflicts`; the command fires only on entering via `review`; both are bound to their conditions, not to arbitrary columns.
- **Agent choice:** review, conflict and top-bar actions share `reviewInbox.agent`; CI triage uses the fixChecks recipe agent.
- **Review workspaces:** the name format, the layout (agent left; first file plus PR browser right) and the head-sync policy (`reset --hard`; skip on own commits or dirty) are fixed.
- **CI triage:** fixed to PRs you authored; the default text names a specific error-tracker MCP and `origin/main` `{USER}`.
- **Removal force semantics** (resolved unforced, reviewed forced) are fixed.
- **Stack rebase prompt** names the reference setup's stack-rebase skill `{USER}`.

### 3.10 Primitives the behaviour decomposes into

**Triggers:**
- Poll tick (interval, on focus).
- Workspace created.
- PR snapshot changed. Derived events: entered condition X, head moved, review requested, review request withdrawn, viewer reviewed, re-requested after review.
- Failed check observed (watch or checks fetch).
- User button or shortcut (top-bar action).

**Conditions** (predicates over the PR snapshot, workspace and settings):
- PR fields: state; author == viewer; in merge queue; mergeable; draft; unanswered change request.
- Named check succeeded.
- Every verdict approved with ≥1 from team T; pending reviewers ⊆ team T.
- Viewer owed / finished.
- Workspace: is in column C (hold); local HEAD == PR head; has unsent drafts; ledger contains key; branch exists; created after T; has no column.
- Checks: still running with the same name; authored by viewer; per-run memory; per-PR cap.

**Actions:**
- Set column.
- Delete workspace (force flag, archive hook).
- Create workspace from PR (base ref, interdiff base, initial column).
- Launch an agent with a templated prompt (agent, view, title, background).
- Arrange layout (open diff, open URL tab).
- Run a shell command in the worktree.
- Sync the workspace to the PR head.
- Ledger mark/forget; toast; append to log.

**Suggested general model:** an ordered list of `{when: trigger, if: condition-expr, then: [actions]}` per project, plus named "PR condition" columns. Ship a built-in preset that reproduces §3.3–§3.8 exactly (the reference setup is then a parameterization of it), validated against the ported test suite.

---

## 4. Terminal host / daemon (sessions outlive the app) [MUST]

This is the reference user's favourite feature. Agents, terminals and dev servers keep running when Orca updates, restarts, quits or crashes. Paths are under `main/daemon/`; read `main/daemon/AGENTS.md` for the invariants. Generality: this is platform-neutral; only Claude resume is `{CC}`.

### 4.1 The owning process
- **Daemon.** `out/main/daemon-entry.js` runs as plain Node (Electron with `ELECTRON_RUN_AS_NODE=1`).
  - Chain: `daemon-entry.ts` → `startDaemon()` (`daemon-main.ts`) → `DaemonServer` (`daemon-server.ts`) → `TerminalHost` (`terminal-host.ts`).
  - PTYs are node-pty children spawned in-process (`pty-subprocess.ts`).
  - Each session runs an `@xterm/headless` emulator with SerializeAddon (`headless-emulator.ts`, `session.ts`, `session-output-plane.ts`).
  - Dev servers are ordinary PTY sessions; there is no separate process type.
- **Launch** (`daemon-out-of-process-launcher.ts`, `daemon-launched-child-spawn.ts`).
  - Spawn options: `detached:true`, stdio `['ignore','ignore','pipe','ipc']`, `cwd=userData`, `ORCA_USER_DATA_PATH`.
  - Arguments: `--socket --token --pid-record --launch-nonce --entry-path --app-version …`.
  - Readiness: an IPC message `{type:'ready', pid, startedAtMs}`.
  - **macOS, packaged** (`macos-daemon-launchd.ts`):
    - A signed helper bundle (`Contents/Helpers/<TerminalHost>.app`) is copied to `userData/daemon-host/macos/…`.
    - It is submitted as a launchd job in `gui/<uid>`: label `com.stablyai.orca.terminal.<sha256(userData)[:16]>.v<PROTOCOL>`, `KeepAlive:false`, `AbandonProcessGroup:true`.
    - It lives outside the app bundle, so an update cannot kill or replace it. Falls back to a fork launch.
  - Linux/Windows relocation variants are [CUT?].
- **Files** (`userData/daemon/`, mode 0700, versioned by protocol):
  - `daemon-v<P>.sock`, or `/tmp/.orca-daemon-<uid>/<hash>/` when the path is too long.
  - `daemon-v<P>.token`: shared secret.
  - `daemon-v<P>.pid`: JSON `{pid, startedAtMs, launchNonce, entryPath, appVersion, bootId…}`, written with `wx`.
- **Socket publish** (`daemon-endpoint-ownership.ts`): bind a private name, then `link` exclusively. On EEXIST, prove the incumbent dead, then `rename`. A daemon never unlinks the socket on shutdown.
- **Protocol** (`types.ts`, `daemon-hello-protocol.ts`, `ndjson.ts`, `daemon-stream-binary-framing.ts`). `PROTOCOL_VERSION = 53`.
  - Each client opens two sockets, `role: control | stream`. Hello is `{type:'hello', version, token, clientId, role}`.
  - Control: NDJSON RPC `{id, type, payload}` → `{id, ok, payload|error}`. Requests: `createOrAttach, write, resize, pausePty/resumePty, setSessionBackground, kill, signal, listSessions, getCwd, getForegroundProcess, getSnapshot, takePendingOutput, clearScrollback, shutdownIfIdle, shutdown, ping…`.
  - Stream events: `data{data, seq}`, `exit{code, incarnationId, cause}`, `dataGap`, `transientFact` (bell, command-finished, pr-link), `sessionBackgroundMarker`.
  - Optional binary framing: `[kind u8][len u32 BE][body]`.
- **Found again on launch** (`daemon-provider-init.ts` `initDaemonPtyProvider`):
  - `DaemonSpawner.ensureRunning()` adopts a live daemon on `daemon-v<P>.sock`, or launches one.
  - **Legacy adapters** probe every previous protocol version and keep those daemons alive. `DaemonPtyRouter` routes old sessions to their old daemon and new terminals to the current one; an old daemon retires when idle.
  - Sessions are **never migrated** between daemons.
  - A same-protocol daemon from another app version is recycled only after it drains.
  - Fallback: `DegradedDaemonPtyProvider` runs fresh terminals in-process, without persistence.
- **Daemon exit:** when idle (no sessions and no clients). SIGTERM kills the PTYs. On macOS, logout retires it.

### 4.2 Identity and persisted state
- Session ID `<worktreeId>@@<uuid8>` (`pty-session-id.ts`). Pane key `<tabId>:<leafUUID>` (`shared/stable-pane-id.ts`).
- The PTY env gets `ORCA_PANE_KEY`, `ORCA_TAB_ID`, `ORCA_WORKTREE_ID`. `incarnationId` distinguishes respawns.
- Layout and session map: `WorkspaceSessionState` (`shared/workspace-session-state-types.ts`), stored in the profile DB domain `workspaceSession`. Fields: `tabsByWorktree[].ptyId`, `terminalLayoutsByTabId[].ptyIdsByLeafId`, `sleepingAgentSessionsByPaneKey`, `activeWorktreeIdsOnShutdown`, …
- Repair code: `main/persistence/restoring-sessions/*`.

### 4.3 Reattach
- **Warm** (daemon alive): `createOrAttach(sessionId)` returns `{isNew:false, snapshot}`. The `TerminalSnapshot` holds serialized ANSI, alt-screen scrollback, mode rehydrate sequences, cwd, title and `outputSequence`. Renderer: `terminal-pane/ipc-pty-connect.ts`, `pty-connection*`.
- **History on disk:** `userData/terminal-history/<sessionId>/{meta.json, output.log, checkpoint.json, scrollback.bin}`.
  - Written by main, which pulls `takePendingOutput` every 5 s and checkpoints with a 45 s cooldown.
  - The daemon buffers 5,000 rows plus ≤2 MiB of pending output while the app is closed.
  - Limits: log 5 MB, checkpoint 200 MB.
- **Cold** (daemon gone, e.g. reboot): the session is missing from the daemon and `meta.endedAt === null`.
  - Spawn a fresh shell with the saved cwd and size; seed and paint the saved scrollback.
  - **Resume the agent** with the provider session id captured by hooks (`claude --resume <id>`).

### 4.4 What survives
| Event | Result |
|---|---|
| Cmd+Q, update relaunch, app crash | Everything survives; quit only disconnects (`startup/main-process-quit.ts`) |
| Update with a protocol bump | Old daemon keeps its sessions (legacy routing) |
| Quit Completely / Manage Sessions → Kill all | All PTYs killed |
| Restart daemon (`daemon-provider-restart.ts`) | Ends terminals, relaunches |
| Daemon crash, reboot, logout | Processes lost; layout plus scrollback cold-restored, agents resumed; dead daemon auto-respawned while the app runs |

### 4.5 CLI, remote, status
- **CLI:** the `orca` CLI never talks to the daemon. It reads `userData/orca-runtime.json` and does RPC to Electron main (or orcad), which goes through the PTY provider. With the app closed: `runtime_unavailable` ("Run 'orca open' first").
  - Rewrite option: make the daemon/host itself serve the CLI, so `orca terminal …` works without the UI.
- **Remote:** `orca serve` / orcad (`main/orcad/`) runs the same provider headless. SSH hosts install orcad. [CUT?]
- **Agent status:**
  - The hook HTTP server lives in **main**, not the daemon (`main/agent-hooks/server/*`). Hooks re-read `userData/agent-hooks/endpoint.env` per call, so long-lived agents find a restarted app.
  - Last status is persisted in `last-status.json`.
  - Events fired while the app is closed are lost (inferred); status is re-derived from OSC 7501 and titles.
  - OSC/title parsing runs in main's output pipeline (`main/runtime/terminal/output/terminal-derived-agent-facts.ts`). The daemon scans only backgrounded sessions (`daemon-background-transient-facts.ts`).
  - Agent turn snapshots are fed only by the main-side hook listener (`startup/main-window-agent-status.ts`), so **turns ending while the app is closed are not recorded**.
  - Rewrite option: put the hook endpoint in the daemon so status and turns are never lost.
- **Ports** (`main/ports/*`): lsof/netstat scan in a worker; attributed by the listener's cwd inside the worktree; printed URLs parsed from PTY output.

---

## 5. Keyboard shortcuts (macOS defaults; all rebindable via Settings → Shortcuts / `~/.orca/keybindings.json`)

The upstream registry is `shared/keybindings/definitions-core-{1..4}.ts`; global dispatch is `renderer/src/app-shell/use-global-keybindings.ts`.

**These tables list Orca's defaults.** User overrides live in `keybindings.platforms.<os>` of `~/.orca/keybindings.json` and win over them; the reference setup's overrides are in `private/reference-setup.md` (not in the repo). Overrides can bind bare keys (e.g. ↑ / ↓ for next/previous change), leave an action unbound, and take Ctrl chords that would otherwise reach the terminal.

**Global and navigation:**

| Binding | Action |
|---|---|
| Mod+P | Quick Open (go to file; `%` text, `:` line) |
| Mod+J | Switch worktree palette (Cmd+1–6 jumps rows; Tab filters host/repo) |
| Mod+N / Mod+Shift+N | Create worktree |
| Mod+Alt+R | Rename workspace |
| Mod+Shift+Backspace | Delete workspace (confirms) |
| Mod+1…9 | Select workspace by index |
| Mod+Shift+↑ / ↓ | Previous / next worktree |
| Mod+Alt+← / → | Worktree history back / forward |
| Mod+B / Mod+L | Toggle left / right sidebar |
| Mod+Shift+E / F / G / I | Explorer / Search / Source Control / Ports |
| Mod+Shift+K | Checks (PR) panel [FORK] |
| Mod+Shift+0 | Focus worktree list |
| Mod+Shift+R | Force reload |
| Mod+= / Mod+- / Mod+0 | Zoom in / out / reset |
| Mod+E | Voice dictation [CUT?] |
| Mod+Alt+A, Mod+Alt+Shift+A | Floating terminal / workspace [CUT?] |
| Mod+O | Open selection/worktree in first "Open in" app [FORK] |

**Tabs and groups:**

| Binding | Action |
|---|---|
| Mod+T | New terminal tab |
| Mod+Alt+T | New default-agent tab |
| Mod+Shift+M / Mod+Shift+O | New markdown / open markdown |
| Mod+W | Close tab (terminal: pane first) |
| Mod+Alt+W | Close all editor tabs |
| Mod+Alt+Shift+W | Close other tabs and splits [FORK] |
| Mod+R | Rename tab |
| Mod+Shift+T | Reopen closed tab |
| Mod+Shift+] / [ | Next / previous tab (all types) |
| Mod+Alt+] / [ | Next / previous tab (same type) |
| Ctrl+Tab | MRU tab (hold to cycle) |
| Ctrl+PageDown / PageUp | Next / previous terminal |
| Ctrl+1…9 | Select tab by index |
| Mod+Alt+← → ↑ ↓ | Focus tab group in that direction [FORK §6] |
| Mod+Alt+P | Quick commands menu [FORK] |

**Terminal:**

| Binding | Action |
|---|---|
| Mod+D / Mod+Shift+D | Split right / down |
| Mod+] / Mod+[ | Focus next / previous pane |
| Mod+Shift+Enter | Expand pane |
| Mod+K | Clear |
| Mod+F | Search scrollback |
| Mod+C / Mod+V / Mod+A | Copy / paste / select all |
| Cmd+← / → | `^A` / `^E` |
| Cmd+↑ / ↓ | Scroll to top / bottom |
| Cmd+Backspace / Delete | `^U` / `^K` |
| Opt+← / → | `ESC b` / `ESC f` |
| Opt+Backspace / Delete | `ESC DEL` / `ESC d` |
| Shift+Enter | CSI-u `ESC[13;2u` (kitty) or fallback |
| Ctrl+Enter | `ESC[13;5u` or CR |
| Mod+Shift+J | Toggle Chat UI ↔ terminal |

**Editor and diff:**

| Binding | Action |
|---|---|
| Mod+S | Save; formats with oxfmt/Ruff [FORK §60] |
| Mod+F / Mod+Alt+F | Find / replace |
| Alt+Z | Word wrap |
| Mod+Alt+C | Copy context |
| Mod+Shift+V | Markdown preview |
| F7 / Shift+F7 | Next / previous change |
| Alt+F7 / Alt+Shift+F7 | Next / previous changed file [FORK §2] |
| Mod+Alt+E / Mod+Alt+D | Show editor / show diff of the file [FORK §58] |
| Mod+Shift+A | Add review note |
| Mod+Enter | Save note (then Enter, Enter sends) [§19]; commit in SC; submit dialogs |
| Esc | Cancel note / close popover |
| Shift+Alt+O | Organize imports [FORK §61] |
| Cmd+click | Go to definition (also inside Peek) [FORK §34] |
| ↑ / ↓ (combined diff) | Scroll 40 px [FORK §44] |
| F1 → Restart Language Server | [FORK] |

**Source control, PR and workspace actions** (all [FORK]; ⌥⌘ chords are macOS only):

| Binding | Action |
|---|---|
| Mod+Alt+U | Uncommitted-only toggle |
| Mod+Alt+S | Send queued PR comments |
| Mod+Alt+Shift+R | Focus review summary |
| Mod+Alt+Shift+T | Open latest agent turn |
| Mod+Alt+G / Mod+Alt+Shift+G | Open PR on GitHub / copy PR URL |
| Mod+Shift+P | Open PR preview deployment |
| Mod+Shift+B | Open frontend dev server (replaces upstream's new-browser-tab) |
| Mod+Alt+L | Open Linear issue |
| Mod+Alt+Enter | Run the workspace's next action |
| Mod+Alt+Shift+Enter | Review |

**File Explorer:**

| Binding | Action |
|---|---|
| Mod+Z / Mod+Shift+Z | Undo / redo file operations |
| Mod+Alt+C / Mod+Alt+Shift+C | Copy path / relative path |
| Mod+Backspace | Delete |

**Unbound actions** (available to bind): open settings, Workspace Board, Agent Dashboard, Tasks, Checks toggle (upstream), send review notes, equalize panes, set pane title, run quick command, one "new <agent> tab" per agent.

**App menu:** Cmd+Q, Cmd+H / ⌥⌘H, Cmd+M, Ctrl+Cmd+F, ⌥⌘I DevTools, standard edit roles. Shift-, Cmd- or Opt-click on Check for Updates selects RC / perf / local builds.

---

## 6. Performance pain points (code paths to avoid)

### 6.1 Starting a Claude session is slow
**Path:**
1. `create-worktree.ts` → IPC `worktrees:create` → `main/ipc/worktree-remote.ts` `performLocalWorktreeCreate`, all through one shared git executor. It runs in order:
   - base ref resolve;
   - branch-name probes **plus GitHub PR lookups** (`:1190-1320`);
   - an **awaited `git fetch` of the base** (`:1357`);
   - `git worktree add`;
   - shared dirs and `.worktreeinclude` copies;
   - the setup script (with `wait-for-setup`, Claude waits for the whole setup, e.g. `pnpm install`).
2. The renderer `activateAndRevealWorktree` mounts the pane → IPC → main `ipc/pty/ipc/spawn.ts` waits for the daemon. Then `spawn-run.ts` runs these one after another:
   - launch plan;
   - preflight;
   - a **`~/.claude.json` trust read-modify-write** (≤1.5 s, `agent-workspace-trust.ts`);
   - plugin readiness and `ClaudeAgentViewService.readyForLaunch` (≤200 ms);
   - env assembly.
3. Unix socket → daemon:
   - 3 health checks and a cold-restore probe;
   - node-pty spawns an **interactive login shell** that runs the user's rc;
   - `session-shell-ready-barrier.ts` waits for the OSC 133 marker (15 s fallback, plus a 30/200 ms settle) before **typing** `claude …` into the shell.
4. Claude's hooks each fork `/bin/sh` + `ps` + `curl` to the loopback server. The session id is learned from the SessionStart hook.
5. Web view:
   - waits for the hook session id;
   - polls IPC every 1.5 s for up to 30 s, and main **reads every `~/.claude/sessions/*.json`** each poll;
   - a new `<webview>` process loads claude.ai;
   - a prompt-focus poll every 100 ms for up to 10 s;
   - a possible full reload 4 s after a delivered prompt.

**For the rewrite:**
- Create the worktree from the local ref at once, fetch in the background, and optionally keep a pool of pre-made worktrees.
- Exec `claude` directly with an environment captured once from the login shell; don't type into an interactive shell.
- Run the preflights in parallel, or skip them.
- Own the PTY over one socket to the host; no renderer → main → daemon hops.
- Make hook delivery a long-lived socket, not `sh` + `curl` per event.
- Watch the one `sessions/<pid>.json` with FSEvents.
- Keep a warm WKWebView; no reload heuristics.

### 6.2 Cmd+click go-to-definition into another file is slow
**Path:**
1. Monaco calls `provideDefinition` on Cmd+hover **and again on click**: two round-trips (`renderer/src/lib/monaco-language-server-providers.ts:110`).
2. Each request sends the **whole file text** over IPC (`model.getValue()`) → `main/ipc/language-server.ts`.
3. `language-server-service.ts` `withSession` runs `resolveLaunch` on every request, which walks up with sync `existsSync` (`typescript-project-resolution.ts`).
4. The pool: one cold tsgo/pyrefly per project root, so per worktree; a cap of 4; eviction means a cold restart; lazy start on the first request with `initialize` and a project load.
5. `didOpen` / full `didChange`, then `definition`. The preview **`readFileSync`s the whole target file on the main thread** to get 10 lines, on hover too.
6. Opening the target: `code-navigation.ts` `jumpToCode`.
   - `activateAndRevealWorktree` makes about 10 store writes and renders.
   - `openFile({forceContentReload:true})` **re-reads the file over IPC** and closes and replaces the tab.
   - `<Editor key={modelUri}>` **destroys and recreates the Monaco instance** per file.
   - The reveal waits 2+ animation frames.
7. After open, a 300 ms diagnostics debounce, then **re-pull diagnostics for every other open file** on the same server queue.

**For the rewrite:**
- Reuse the hover result for the click, and send incremental edits only.
- Cache project roots, and start servers when the workspace opens.
- Share one server per repo where possible.
- Read the preview lazily or off-thread.
- Keep one editor per pane and swap buffers; reuse loaded text; reveal synchronously.
- Short-circuit activation for the same worktree.
- Prioritize definition/hover over diagnostics, and cancel stale requests.

---

## 7. Notes on sources
- **FORK.md drift:**
  - §4 says inline comments cannot be deleted; PR 171 added delete.
  - §15's separate media bucket is gone (02caec50b9); uploads now use GitHub's own attachments endpoint.
  - §32 is superseded by §52.
  - §57 is retired.
  - The F4 binding in PR 180 was replaced by ⌥⌘E/⌥⌘D.
  - PR 164 (DevTools) and PR 123 (macOS delete fix) were superseded by upstream.
- **Reverted:** a remote-server board column (025446b502 reverted by 28777a474e).
- **Open PRs:**
  - 58: Claude page focus during a tab drag; §45 may already cover it.
  - 118: a copy button on notes.
  - 154: Mod+Shift+P opens the preview in Orca's browser; it overlaps PR 158.
- **Not in FORK.md but merged:** see the PR refs throughout §2. Covered here: LSP extras, notes and composer, checks, merge button, ports, quick commands and tabs.
