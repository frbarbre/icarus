# Panes and tabs: exact spec of Orca's tab / group / split system, and how it maps onto Zed

Goal: the Rust client (soft fork of Zed, GPUI) must behave "straight up the same" as the Orca fork for everything in this document. The server owns terminals and agents; the client owns the layout.

Sources: `~/Developer/orca` at `0d92720996` (fork `frbarbre/orca`), FORK.md §6, §9, §18, §45, §47, §50, §58, §68; fork PRs 26, 56, 58, 59, 73, 76, 83, 89, 115, 120, 134, 138, 164, 169; the reference export (private, not in the repo; its values are summarized in `private/reference-setup.md`); Zed `main` at `596dfdcf` (2026-11-15). Orca paths are under `src/` unless noted; `renderer/` means `src/renderer/src/`.

Conventions:
- **Group** = Orca "tab group" = one tab strip plus its content area. Zed calls this a `Pane`.
- **Pane** = a split inside one terminal tab (Orca's terminal-pane). Zed has no such thing.
- "Mod" is ⌘ on macOS. Only macOS bindings are listed unless noted.
- Orca has a second code path behind the build switch `ORCA_LAYOUT_OWNER_IS_MAIN` (`shared/layout-owner-switch.ts`), where main owns the layout. It is off in shipped builds. This spec describes the shipped path (the renderer owns the model).
- **Upstream** = stablyai/orca; **[FORK]** marks behaviour the fork added.

---

## 1. Data model

### 1.1 Containment

```
window (one per app)
└─ active worktree (global selection; switching swaps the whole centre)
   └─ per worktree (key = worktreeId, or folderWorkspaceKey(id), or FLOATING_TERMINAL_WORKTREE_ID)
      ├─ layout: TabGroupLayoutNode           binary split tree, leaves = group ids
      ├─ groups: TabGroup[]                   flat list; array order is used for fallbacks
      │   └─ TabGroup { id, worktreeId, activeTabId|null, tabOrder: string[], recentTabIds?: string[] }
      ├─ tabs: Tab[]                          flat; tab.groupId points at its group
      └─ activeGroupId
         └─ terminal tab → TerminalLayoutSnapshot (inner pane tree, keyed by tab id)
```

Types (`shared/tab-types.ts`):

```ts
type TabGroupLayoutNode =
  | { type: 'leaf'; groupId: string }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; first; second; ratio?: number } // ratio of first, default 0.5
type TabContentType = 'terminal'|'editor'|'diff'|'conflict-review'|'check-details'|'agent-session'|'browser'|'simulator'
type Tab = { id; entityId; groupId; worktreeId; executionHostId?; contentType; label; generatedLabel?; aiVaultTitle?;
  quickCommandLabel?; customLabel: string|null; color: string|null; sortOrder; createdAt;
  isPreview?; isPinned?; agentSessionAgent?; viewMode?: 'terminal'|'chat'; lastFocusedAt? }
```

- **The group tree is strictly binary.** Every split has two children and one `ratio`, the share of `first`. There are no n-ary nodes.
- **Direction in the group tree:** `horizontal` means side by side (flex row); `vertical` means stacked.
- **Split side → node** (`tabs-group-actions.ts`, `tab-group-layout-tree.ts`):
  - left/right give `horizontal`; up/down give `vertical`.
  - left/up put the new group `first`; right/down put it `second`.
  - A new split node has ratio 0.5. It replaces the target leaf, so splitting one of three columns gives 50/25/25, not thirds.

### 1.2 Tab kinds and identity

- `Tab.id` is the strip identity. `Tab.entityId` is the backing content (`entityId` defaults to `id`).

| User-facing kind | contentType | id | entityId | Dedupe |
|---|---|---|---|---|
| Terminal (also hosts CLI agents such as Claude Code; see below) | terminal | terminal row UUID | same as id | ids globally unique; a duplicate hinted id throws |
| Structured agent chat | agent-session | `structured-agent-session-<sessionId>` | provider sessionId | one per session |
| File editor / markdown | editor | UUID | `editor:<wt>:<runtimeKey\|local>:<path>` | **per group**: opening a file whose tab is already in the target group activates it. The same file may be open in two groups. |
| Diff | diff | UUID | `<wt>::diff::<source>::<relPath>` | per group |
| Conflict review / check details | conflict-review / check-details | UUID | file/check id | per group |
| Browser (also the PR page; see §50) | browser | UUID | browser workspace id | **per worktree** on entityId |
| Mobile simulator | simulator | given id or UUID | same as id | one per worktree ("Go to Mobile Emulator") |

- **No settings tab and no dedicated PR tab exist.** The "PR page" is a browser tab on github.com.
- **"Same type" families** (`toVisibleTabType`), used by next/prev-same-type and by focus helpers: `terminal`, `editor` (= editor + diff + conflict-review + check-details), `agent-session`, `browser`, `simulator`.
- **Agent tabs in practice** (with `openClaudeTabsInWebView = true`):
  - A Claude agent tab is a **terminal tab** running the Claude CLI.
  - The tab can show one of three views: Terminal, Chat UI (`viewMode: 'chat'`), or Claude web (a claude.ai `<webview>` covering the pane). The choice is made from the tab's or pane's **View** submenu (FORK §18, PR 73).
  - Agent status (working / blocked / waiting / done) is tracked per pane key.
- **Editor and diff are one tab's two views [FORK §58]:**
  - ⌥⌘E shows the edit view and ⌥⌘D the diff, at the cursor line. Dirty files are saved first; a failed save aborts the switch.
  - Switching replaces the tab with the other view.
  - When file X becomes active in group G, the other view of X is closed if it is clean, has no draft, and all its tabs are in G.

### 1.3 Which editor group a file opens in

`resolveEditorOpenTargetGroupId` (`editor/tabs/editor-open-target-group.ts`):
1. An explicit target group wins.
2. Otherwise the active group, **unless its active tab is a terminal**. In that case:
   - prefer another group whose active tab is an editor;
   - then a group whose MRU contains an editor;
   - then the active group.

### 1.4 Terminal tab inner panes

`shared/terminal-tab-types.ts`:

```ts
type TerminalPaneLayoutNode = { type:'leaf'; leafId } | { type:'split'; direction:'vertical'|'horizontal'; first; second; ratio? }
type TerminalLayoutSnapshot = { root; activeLeafId; expandedLeafId; chatLeafId?;
  ptyIdsByLeafId?; buffersByLeafId?; scrollbackRefsByLeafId?; titlesByLeafId?; lastGridsByLeafId? }
```

- **The direction convention is inverted relative to groups.** Here `vertical` means side by side (split **right**) and `horizontal` means stacked (split **down**).
- `leafId` is a UUID and is the durable identity of a pane.
  - `paneKey = "<tabId>:<leafId>"`, so tab ids must not contain `:`.
  - The pane key indexes agent status, sleeping agent sessions and PTY incarnations.
- A new tab starts as one leaf: `{root: leaf, activeLeafId: leaf, expandedLeafId: null}`.
- Ratios are serialized only when `|r − 0.5| > 0.005`, rounded to 3 decimals.
- **PTY binding has three layers:**
  - `ptyIdsByLeafId[leafId]` per pane (the real binding);
  - `TerminalTab.ptyId`, a tab-level wake hint;
  - `ptyIdsByTabId`, the live set. It is session-only and reset on hydrate.

### 1.5 Per-worktree vs global

- **Per worktree:**
  - `layoutByWorktree`, `groupsByWorktree`, `unifiedTabsByWorktree`, `activeGroupIdByWorktree`;
  - terminal rows and their layouts;
  - active file, active browser tab and active tab type;
  - the recently-closed stacks (**in memory only**, not persisted);
  - `rightSidebarTabByWorktree` (PR 169; in memory, defaults to Source Control).
- **Per group:**
  - `activeTabId`;
  - `recentTabIds` (MRU, newest last, no cap beyond pruning to live ids);
  - `recentQuickCommandIdByGroup`.
- **Per terminal tab:** the pane layout, expanded pane, and pane titles.
- **Global:**
  - the active repo, worktree and workspace key;
  - derived "active surface" fields (active terminal, file or browser tab, and type), recomputed from the active group of the active worktree. An empty active group yields type `terminal`;
  - `worktreeNavHistory` (back/forward, cap 50);
  - `lastVisitedAtByWorktreeId`.
- **There is no cross-group or per-workspace tab MRU.** `Tab.lastFocusedAt` feeds only the ⌘J palette and auto-send ranking.

### 1.6 Persistence

**What is saved:** `WorkspaceSessionState` (`shared/workspace-session-state-types.ts`), with these layout-relevant fields:
- `unifiedTabs`, `tabGroups`, `tabGroupLayouts`, `activeGroupIdByWorktree`;
- `tabsByWorktree` (terminal rows), `terminalLayoutsByTabId`, `activeWorktreeIdsOnShutdown`, `remoteSessionIdsByTabId`;
- `sleepingAgentSessionsByPaneKey`, `terminalPtyIncarnationsByPaneKey`;
- `openFilesByWorktree` (edit mode only, with dirty draft content), `browserTabsByWorktree`, `activeTabTypeByWorktree`, `lastVisitedAtByWorktreeId`.

**Rules applied when saving** (`lib/workspace-session-unified-tabs.ts`):
- Each group's `tabOrder` is filtered to real tabs.
- **Empty groups are dropped**, and the layout is pruned to the surviving groups.
- A stale active tab is nulled.

**When it is saved:**
- The renderer subscriber debounces by 150 ms (trailing) and sends patches over IPC.
- Main debounces by 1 s with a 5 s max wait.
- Writing is enabled only after startup hydration finishes.
- The file is `<userData>/orca-data.json`, local host partition. Other hosts go in `workspaceSessionsByHostId`.

**Restore order** (`app-shell/use-app-startup-hydration.ts`): terminal rows → tabs → editors → browser → reconcile → SSH reconnect → terminal reconnect → structured chats.

**Validation on restore** (`slices/tabs-hydration.ts`, `tabs/tabs-reconciliation.ts`):
- Unknown worktrees and invalid ids are dropped.
- A tab's owner group is its own `groupId` if that group exists, otherwise the first group listing it. Duplicates are removed.
- The active tab is forced to the MRU tail.
- Groups emptied by validation are dropped.
- Missing layout leaves are added as `{split horizontal, first: existing, second: leaf}`.
- Duplicate or invalid leaves are pruned and their splits collapsed.
- Orphan tabs go to `groups[0]`.
- Non-renderable tabs are removed: a terminal not among the live rows, a browser not in `browserTabsByWorktree`, an editor not in the open files. Agent-session and simulator tabs are always kept.

**Re-binding terminal panes to daemon sessions:**
1. A reconnect plan collects the worktrees that were live at shutdown and, per tab, the saved PTY ids. The local daemon uses the saved `ptyId`; relay/SSH uses `remoteSessionIdsByTabId`. These are wake hints, not proof of liveness.
2. Saved scrollback is replayed into the pane first. Alt-screen tails are trimmed unless the TUI is still live.
3. When a pane mounts with real dimensions, it attaches using **the same session id**. The daemon returns one of:
   - **reattach**: the process is alive; a screen snapshot is replayed;
   - **coldRestore {scrollback, cwd}**: the process is gone but history exists; a new shell starts under the same id, seeded with the old scrollback;
   - **fresh spawn**.
4. If attach fails, the stale id is cleared and a fresh shell spawns.
5. Agents that were running in a pane resume through `sleepingAgentSessionsByPaneKey`: provider session id, prompt, launch config, origin `worktree-sleep|quit|live`.
6. `activeLeafId` and `expandedLeafId` are re-applied last.

---

## 2. Interactions

### 2.1 Creating tabs

- **New terminal (⌘T):**
  - Appended **at the end of the focused group** (no pin partitioning on this path).
  - Title is `Terminal N`, where N is the lowest free ordinal.
  - cwd is `startupCwd` or the worktree path.
  - Activated; that group becomes the active group.
- **Other kinds** (`createUnifiedTab`):
  - Target group: explicit target → the group of `afterTabId` → the active group → the first group or a new root group.
  - **Default insertion is the end of the tab's pin partition, not after the active tab.**
    - An unpinned tab goes at the very end; a pinned tab goes at the end of the pinned prefix.
    - With an `afterTabId` anchor (Duplicate Tab, some browser and agent launches): right after the anchor if it is in the same partition, else at the partition boundary.
  - A new **preview** tab replaces the existing preview tab in the same group, if compatible: editor kinds replace each other; any other kind replaces only its own kind.
  - Creating does not change the active group unless a split or root group was created.
- **"+" button (New tab)** opens a menu:
  - **Omnibox** "Search open tabs, history, files, URLs, agents…". It classifies input as URL, host, web search, new relative file, absolute file or HTML file URL, and also matches open tabs, history, agents and the static items.
  - **Static items:** New Terminal ⌘T, New Browser Tab (unbound), New Markdown ⌘⇧M, Open Markdown… ⌘⇧O, New/Go to Mobile Emulator ⌥⇧⌘E.
  - A separator, then one launch item per agent (default agent ⌥⌘T).
- **Quick-command Run button (per group strip) [FORK §9, PRs 115, 120]:**
  - Re-running a command whose labelled run is still live sends Ctrl-C to it (toggle).
  - Otherwise the command runs in an idle terminal, probed with a 1.5 s probe: own-label terminals first, then the group's active tab, then the same group, then other groups.
  - The tab is renamed to the command label, and the bell is marked.
  - If no terminal is idle, a new tab opens.
  - `tab.runQuickCommand` (unbound by default) answers only in the focused group.

### 2.2 Activating and switching

- **Click** activates on **pointer-up**, button 0 only. A press that moves 12 px starts a drag instead and never switches tabs.
- `activateTab`:
  - sets the group's active tab, moves it to the MRU tail and makes its group the active group;
  - stamps `lastFocusedAt`;
  - **ends preview** unless `preservePreview` (a re-click from the sidebar preserves it).
- **Next/prev (all types):** cycles the focused group's `tabOrder`, wrapping.
- **Next/prev (same type):** cycles tabs of the active tab's family in visual order, wrapping. It is a no-op with ≤1 tab of that type.
- **Next/prev terminal:** terminal tabs only, wrapping. From a non-terminal tab, "next" goes to the first terminal. Works with focus inside a terminal.
- **Go to tab N (1–9):**
  - Index N−1 of the focused group's `tabOrder`. **9 means the 9th tab, not the last.**
  - Ignored while the ⌘J palette is open, and only active in the terminal/workspace view.
- **MRU switcher (Ctrl+Tab):**
  - The overlay "Switch Tab" opens at the top centre (12vh) immediately. Each row shows icon, label and dirty dot.
  - Each Tab steps forward and Shift steps back. Releasing Ctrl commits; Esc or window blur cancels.
  - Order with `ctrlTabOrderMode = 'mru'`: the active tab first, then the focused group's `recentTabIds` newest first, then the remaining tabs in visual order. So one tap toggles to the previous tab.
  - `'sequential'` uses strip order. Needs ≥2 tabs. Scope is the focused group only.

### 2.3 Closing

**⌘W (`tab.close`):**
- If focus is in a terminal's text area **and** the active tab is a terminal, ⌘W closes the **inner pane** (§2.8). Closing the last pane closes the tab.
- Focus on chrome (for example the tab strip) closes the whole tab.
- A floating panel with focus handles ⌘W itself; an empty visible floating panel hides.

**Guards, in order:**
1. **Pinned:** the confirmation `confirmClosePinnedTab`, when on. Requests are queued 350 ms apart. Pinned tabs have no X and ignore middle-click; the menu's Close is disabled.
2. **Running process:** the terminal PTY is probed for 4 s. A live process prompts unless `skipCloseTerminalWithRunningProcessConfirm` is on. A busy agent-session chat gets the same dialog.
3. **Dirty editor:** if no other group shows the file, a save/discard prompt via the central queue.

**Which tab activates after a close** (only when the closed tab was the active one; `pickNextActiveTab`):
1. The newest live entry of the group's MRU.
2. Else the right neighbour.
3. Else the left neighbour.

**The group becomes empty:**
- If other groups exist, the group is removed and its parent split is replaced by the sibling subtree. The sibling keeps its internal ratios; the parent's ratio is lost.
- **The active group becomes the sibling** (the sibling leaf, or the first leaf of the sibling subtree, following `.first`).
- **Quirk:** this happens even if the removed group was not the active one.
- **The last group is kept, empty.**
- If the active worktree has no tabs, files or browser tabs left, the worktree is deselected (welcome screen).

**Close Others (context menu):**
- Closes the group's tabs except the clicked one and pinned tabs, after the dialog "Close other tabs? … Pinned tabs will stay open."
- Runs as a bulk close: **running terminals are killed without the process prompt**, while dirty editors still prompt.

**Keep only the active tab (`tab.closeOthers`; default ⌥⇧⌘W) [FORK PRs 83, 89, 134, 138]:**
- Every group in the active worktree handles the request; there is **no dialog**.
  - **Focused group:** keeps its active tab and closes every other tab.
  - **Every other group:** closes all its tabs, then collapses if empty.
- Always kept:
  - pinned tabs;
  - a **busy terminal tab**: any pane's PTY probe (4 s) returns `live` or **times out**, and that pane's agent status is not `done`. An agent in `working`, `blocked` or `waiting` with a live process keeps the tab. One busy pane keeps the whole split tab.
  - a dirty editor whose save prompt was cancelled.
- Idle shells, agents reporting `done`, editors and browsers close.
- Side effect of the collapse quirk above: after other groups collapse, focus ends on a collapsed group's sibling. When everything but the focused group goes, that is the focused group. If a busy group survives, focus can land on it.

**Close All Editor Tabs (⌥⌘W):** unpinned editor-family tabs of the group, with the dialog "Close all editor tabs?".

**Close to the Right / Left:** implemented (non-pinned tabs strictly after or before, with a dialog) but **not reachable from any menu** in the fork. Do not expose it.

**Close split pane (group menu or tab menu):** closes every tab of the group (with the guards), then removes the group if nothing survived.

### 2.4 Reopen closed tab (⇧⌘T)

- **Stack:** one cross-type stack per worktree, newest first, in memory only.
  - Up to 30 kind entries, 10 terminal snapshots, 10 editor and 10 browser.
  - Only explicit user closes are captured. Markdown-preview closes are excluded. Runtime-owned or ambiguous worktrees are skipped.
- **Terminal:** a **fresh shell** in the original `startupCwd`, keeping the shell override, custom title and colour. No old PTY, scrollback or agent session comes back.
- **Position:** the original group if it still exists (else the active group), at the original index within it.

### 2.5 Pinning, renaming, colour

- **Pin / Unpin** is in the context menu only; there is no keybinding.
  - Pinning moves the tab to the end of the pinned prefix and ends preview. Unpinning moves it to the first unpinned slot.
  - Pinned tabs show a pin icon and no X.
  - Drag-reordering **does not enforce** the partition: a pinned tab can be dragged among unpinned ones.
- **Rename a terminal or agent tab:**
  - Opened by double-click, "Change Title", or `tab.rename` (default ⌘R).
  - Inline input, min 72 px, text selected. Enter or blur commits; Esc cancels.
  - An empty value clears `customLabel` and reverts to the automatic title.
  - The displayed title is `customLabel ?? title`.
- **Rename an editor tab:** double-click on a preview tab makes it permanent. Double-click on the label of a permanent tab starts an inline rename of the **file on disk**. Browser tabs have no rename.
- **Tab colour swatches:** None, Blue #3b82f6, Purple #a855f7, Pink #ec4899, Red #ef4444, Orange #f97316, Yellow #eab308, Green #22c55e, Teal #14b8a6, Gray #9ca3af. Shown as a dot on the tab.

### 2.6 Tab strip rendering

**Strip and tab geometry:**
- One 32 px strip per group (31 px visible), with the Quick Command split button (24 px high) and a "Pane Actions" (…) button on the right. The … button appears only when splits exist and holds just "Close split pane".
- Tab width is fixed: 180 px, 220 px when the window is ≥1280 px wide, minimum 72 px.
- The active tab has a 2 px bottom bar and a lighter background.

**Terminal or agent tab, left to right:**
- an amber wash when there is unread activity;
- the leading icon: bell + agent icon, or agent state dot with progress ring + agent icon, or agent icon, or shell icon (inactive tabs at 70% opacity);
- pin icon, title, colour dot;
- a "collapse pane" button when an inner pane is expanded;
- X: always on the active tab, on hover for others, never on pinned tabs.

**Editor tab:**
- an icon: conflict, preview-eye, or file type;
- pin icon;
- the label, in **italic for a preview tab**;
- a line-through for a file deleted on disk;
- a git status letter;
- the dirty dot and X sharing one 16 px slot.

**Browser tab:** favicon (spinner while loading, cursor glyph while an agent drives it), pin icon, label, mute button, X.

**Hover card:** title plus program or URL.

**Overflow:**
- horizontal scroll, with the wheel mapped to horizontal;
- chevrons only when overflowing and the strip is ≥260 px. Each scrolls by `max(120, 0.75 × width)`;
- fading edges and a custom scroll indicator;
- **the active tab is sticky** at whichever edge it would scroll past;
- during a drag, hovering a chevron auto-scrolls every 180 ms.

**Mouse:** middle-click closes on `auxclick` (not on pinned tabs or while renaming).

**Group focus look:** with splits, unfocused groups are at 0.95 opacity and the focused group has an accent bottom border.

**Context menu, terminal/agent tab:**
1. Move Tab to Split ▸ Right / Left / Down / Up (only if the group has >1 tab).
2. Split terminal ▸ right ⌘D / down ⌘⇧D (inner panes).
3. View ▸ Terminal / Chat UI / Claude web, plus session items.
4. Pin/Unpin Tab, Copy Tab ID.
5. Close ⌘W (disabled when pinned), Close Others (disabled with one tab), Close split pane (only with splits).
6. Change Title ⌘R, Tab Color ▸.

**Context menu, editor tab:** Move Tab to Split, Rename (the file), Pin/Unpin, Copy Tab ID, Close, Close Others, Close All Editor Tabs ⌥⌘W, Close split pane, Open Markdown Preview (markdown only), Copy Path, Copy Relative Path, Reveal in Finder.

**Context menu, browser tab:** Move Tab to Split, Duplicate Tab, Pin/Unpin, Mute (when audible or muted), Copy Tab ID, Close, Close Others, Close split pane, Open In Browser.

### 2.7 Groups: split, resize, drag and drop

**Split by command:**
- The only command is **Move Tab to Split ▸ side**. It moves the tab into a new group on that side and needs ≥2 tabs in the source group.
- There is no "split and clone" and no keyboard shortcut for group splits.

**Resize:**
- Handle hit area 6 px; the divider is 3 px and strengthens on hover or drag.
- ratio = pointer position / container size, **clamped to [0.15, 0.85]**.
- Only styles change during the drag; the ratio is committed once on release and persisted.
- No double-click reset. **No equalize and no maximize/zoom for groups.**

**Drag and drop:**
- Uses pointer events; one drag context per worktree covers every strip and body.

**Drag targets and results:**

| Target | Rule | Result |
|---|---|---|
| A tab in any strip of the same worktree | insertion side = pointer x vs the tab's midpoint; 2 px blue bar on that edge | reorder (same-group index adjusted for removal) or move into that group at that index |
| Group edge | left/right bands = 20% of the **panel width** (including the strip); up/down bands = 20% of the **body height**, from the body's top and bottom; the strip itself never splits | a new group on that side (ratio 0.5), shown as a translucent blue half-panel overlay labelled "New split" |
| Group body centre | anything else inside the body | move the tab to the **end** of that group; no overlay, but the target group's active tab is previewed during the hover. Dropping on its own group does nothing. |

**No-op drops:**
- the only tab of a group dropped on its own group's edge;
- a one-tab group dropped on the facing side of its direct sibling, which would recreate the same layout.

**After any drop:**
- the tab becomes active in its destination, and the destination becomes the active group;
- the source group picks its next tab by MRU;
- an emptied source group collapses. The active group then becomes the sibling, falling back to the target.

**Cancel:** Esc restores the pre-drag activation snapshot.

**Not supported:** drags between windows, between worktrees, or between the floating panel and the main workspace.

**Other drag sources:**
- OS and explorer files dropped on a strip or editor body open as editor tabs in that group (no split).
- AI-vault sessions from the sidebar use a different zone rule: the centre is the rect inset 10% per side; outside it, the left third → left, the right third → right, otherwise up or down by y.

**Web views during drags [FORK §45/§47, PRs 56, 58, 59, 76]:**
- **Pointer passthrough:**
  - At drag start every registered `<webview>` (browser tabs, retained hosts and the Claude web cover) gets `pointer-events: none`, ref-counted, and webviews created mid-drag get it too.
  - Native drags toggle the same passthrough.
- **No focus into a guest mid-drag:**
  - While a drag is active, the Claude page is never focused.
  - When the drag ends, if the cover still owns focus, the page is focused then.
- **The drag sensor survives guest focus changes:**
  - At pointer-down it records whether focus is in a webview.
  - A window blur **caused by a webview taking focus** is ignored, and so is the matching window focus when focus returns.
  - Any other blur or focus (another app) cancels, and so does Esc.
- **⌥⌘arrows inside a guest page:** checked in the guest's input handler before window shortcuts, then replayed in the renderer, so group focus leaves the page.
- **Context menus** close on window blur, because clicks inside a webview are not "outside clicks".
- **Window focus** does not refocus a pane covered by Claude web; otherwise the page steals presses on the tab strip.

### 2.8 Splits inside a terminal tab

**Split right (⌘D) / split down (⌘⇧D):**
- From the keyboard, the tab or pane context menu, or the pane header button.
- If a pane is expanded, it is collapsed first.
- The active pane is wrapped in a 50/50 split with the new pane second.
- **The new pane becomes active and focused.**

**New pane cwd, in order:**
1. The source pane's confirmed OSC 7 cwd.
2. Its pending cwd.
3. The PTY's real cwd (local only, 1 s timeout).
4. The cached unconfirmed cwd.
5. `startupCwd ?? worktree path`.

**Close pane (⌘W in the terminal):**
- With >1 pane, a running process prompts, unless `skipCloseTerminalWithRunningProcessConfirm` is on.
- The last pane closes the tab.
- The sibling is promoted into the parent's slot, inheriting its size.
- **Focus successor:** the sibling-side leaf touching the freed edge.
  - Start at `children[1]` if the closed pane was first, else the previous sibling.
  - Then descend into the last child when the axis matches and the closed pane was not first; otherwise into the first child.
- **Process exit:** a non-zero exit shows an overlay and keeps the pane. In a split, an exited pane closes. A sole pane closes the tab, except an untouched fresh pane, which is kept for diagnostics.

**Focus next/prev pane (⌘] / ⌘[):**
- Needs ≥2 panes and un-expands first.
- Order is **pane creation order** (Map insertion order, which after a restore is replay order), not visual order. It wraps.
- **There is no directional pane focus inside a tab** in the fork. It was built and removed the same day in favour of group focus (FORK §6).
- Click focuses a pane. Focus-follows-mouse is opt-in.
- Inactive panes are dimmed (`terminalInactivePaneOpacity`, active 1, 140 ms transition).

**Resize:**
- Mouse only, min 50 px per pane (`min(50, total/2)`).
- **Double-click a divider resets that split to 50/50.**
- No keyboard resize.

**Equalize** (unbound; context menu):
- Each child's weight is 1 for a leaf or a cross-axis split, and the leaf count for a same-axis subtree. So three peers become thirds.
- No-op while a pane is expanded.

**Expand / collapse pane (⌘⇧Enter):**
- Needs ≥2 panes.
- Hides every off-path sibling up to the root, then makes the pane active and focused.
- Persists as `expandedLeafId`.
- Splitting or cycling focus collapses first. It auto-clears if the pane disappears.
- The tab shows a collapse button.

**Pane titles:**
- User-set only: Set Title… and Clear Pane Title (both unbound). Persisted per leaf.
- The header shows on the active visible tab, or when a title is set.
- The header X clears a title, or closes an untitled pane.

**Drag a pane:**
- **Within the tab:** the drop side is the nearest edge of the target pane (argmin of distances; fallback right). The source is detached, its sibling promoted, and the target wrapped.
- **Out to any tab strip of the same worktree** (only when the tab has >1 pane): creates a new terminal tab at that index in that group and **moves the live PTY**, title and scrollback. It is never killed.
- Dropping into another tab's pane tree is not supported.

### 2.9 Focus movement between groups [FORK §6]

**Chords:**
- ⌥⌘← → ↑ ↓ (`tabGroup.focusLeft/Right/Up/Down`; macOS only).
- They work from terminals, editable fields and the Claude page.
- They are skipped when the creation layout is active or the floating panel has focus.

**Candidates:**
- the `getBoundingClientRect` of visible `[data-tab-group-body-id]` elements of the active worktree (w > 0 and h > 0), in DOM order.
- Fewer than 2 candidates is the **single-group** case.

**Current group:**
1. The rect containing the centre of `document.activeElement`.
2. Else the stored active group.
3. Else the first rect.

**Selection, exactly:**

```
EDGE_TOLERANCE = 2px
for each other rect P:
  gap = left: A.left−P.right | right: P.left−A.right | up: A.top−P.bottom | down: P.top−A.bottom
  skip if gap < −2
  shared = overlap on the perpendicular axis (vertical span for left/right, horizontal for up/down)
  skip if shared ≤ 0
  best = P if no best, or gap < best.gap − 2, or (|gap − best.gap| ≤ 2 and shared > best.shared)
```

- No diagonal moves and no wrap.
- At an edge the chord is **consumed and nothing happens**.

**Single-group case:**
- ⌥⌘← and ⌥⌘→ fall back to worktree history back/forward (they share that default binding). The fallback runs only if the chord also matches the history action's binding.
- ⌥⌘↑ and ⌥⌘↓ fall through.

**Landing:**
1. The target's active tab is activated.
2. For a terminal, its `activeLeafId` pane is focused.
3. An empty group just becomes active.
4. Two frames later, if focus is not already inside the target rect, the first visible `textarea, input, [contenteditable], webview, iframe, [tabindex≥0]` whose centre is inside it gets focus. Else the group body itself gets focus (`tabIndex=-1`), which an empty group needs so it is not reclaimed.

**Other focus paths:**
- Pointer-down or focus-capture anywhere in a group makes it the active group.
- There is **no "focus next/prev group"** command.

### 2.10 Programmatic layouts [FORK §50]

When a review workspace launches its agent:
1. The agent's group is split to the right.
2. The first changed file's diff opens in the right group.
3. The PR opens as a browser tab in the same right group and is selected.
4. The agent tab is selected in its group, without stealing focus.

With no files, only the browser tab goes right. The rewrite's layout API must support these steps.

### 2.11 Floating terminal / workspace ([CUT?] in features.md)

- A draggable, resizable overlay at z-index 45: min 420×280, default 920×560, bounds kept in localStorage.
- It has its own scratch worktree (`FLOATING_TERMINAL_WORKTREE_ID`), with full tab groups and splits.
- The titlebar strip shows only the focused group's tabs; split groups have no strips.
- ⌥⌘A toggles it. ⌥⇧⌘A maximizes (opening pre-maximized if closed). Double-clicking the titlebar toggles maximize; minimize hides it.
- No drags to or from the main workspace.
- Its trigger can be a floating button or in the status bar.

---

## 3. Keybindings (tabs, groups, panes)

Overrides live in `~/.orca/keybindings.json` (`{version, keybindings, platforms:{darwin,linux,win32}}`).
- An entry replaces that action's defaults; `[]`, `null` or `false` unbinds it.
- The platform section wins over the common section.
- With `terminalShortcutPolicy = "orca-first"`, every listed chord is captured even with focus in a terminal. For example, a Ctrl+letter override never reaches the shell, and Orca shows a capture toast.

The reference setup overrides several of these defaults (see `private/reference-setup.md`, not in the repo); the rewrite must apply such overrides on import.

In the table, a blank Zed cell means Zed has no equivalent action.

| Action id | Orca default (macOS) | Behaviour | Zed action / default |
|---|---|---|---|
| `tab.newTerminal` | ⌘T | new terminal at the end of the focused group | `workspace::NewCenterTerminal` (unbound; cmd-t is the symbol finder) |
| `tab.newAgent` | ⌥⌘T | new default-agent tab | – (Zed: alt-cmd-t = CloseOtherItems) |
| `tab.newBrowser` | unbound | new browser tab | – |
| `tab.newMarkdown` / `tab.openMarkdown` | ⌘⇧M / ⌘⇧O | new / open markdown | – |
| `tab.close` | ⌘W | close tab; in a terminal, close the pane first | `pane::CloseActiveItem` cmd-w |
| `tab.closeAll` | ⌥⌘W | close unpinned editor tabs of the group (with a dialog) | `pane::CloseAllItems` cmd-k w (all kinds) |
| `tab.closeOthers` [FORK] | ⌥⇧⌘W | keep only the active tab; clear other groups; keep pinned and busy terminals; no dialog | ≈ `workspace::CloseInactiveTabsAndPanes` ctrl-alt-cmd-w (rules differ, §4) |
| `tab.reopenClosed` | ⇧⌘T | cross-type reopen, original group and index | `pane::ReopenClosedItem` cmd-shift-t (files only) |
| `tab.rename` | ⌘R | inline rename of a terminal or agent tab | Rename via double-click / item menu |
| `tab.runQuickCommand` [FORK] | unbound | run or toggle the group's last quick command | – |
| `tab.openQuickCommandsMenu` | ⌥⌘P [FORK default] | quick commands menu | – |
| `tab.nextSameType` / `previousSameType` | ⌥⌘] / ⌥⌘[ | cycle the same family, wrapping | – |
| `tab.nextAllTypes` / `previousAllTypes` | ⇧⌘] / ⇧⌘[ | cycle all tabs of the group, wrapping | `pane::ActivateNextItem` / `ActivatePreviousItem`: cmd-} / cmd-{ and alt-cmd-→ / ← |
| `tab.nextTerminal` / `previousTerminal` | Ctrl+PgDn / Ctrl+PgUp | cycle terminal tabs | – |
| `tab.previousRecent` | Ctrl+Tab (hold; ⇧ reverses) | MRU switcher overlay | `tab_switcher::Toggle` ctrl-tab / ctrl-shift-tab |
| `tab.selectByIndex` | Ctrl+1…9 | Nth tab of the focused group; 9 = 9th | `pane::ActivateItem(n)` ctrl-1…9 (clamps to the last); ctrl-0 = last |
| `workspace.selectByIndex` | ⌘1…9 | jump to worktree N | conflicts with `workspace::ActivatePane` cmd-1…9 |
| `tabGroup.focusLeft/Right/Up/Down` [FORK] | ⌥⌘← → ↑ ↓ | §2.9 | `workspace::ActivatePaneLeft…` cmd-k cmd-arrows |
| `worktree.history.back/forward` | ⌥⌘← / ⌥⌘→ | the single-group fallback of the chord above | – |
| `terminal.splitRight` / `splitDown` | ⌘D / ⌘⇧D | inner pane split | Terminal: cmd-d → `pane::SplitRight` (a group split) |
| `terminal.closePane` | ⌘W | close the inner pane; the last pane closes the tab | – |
| `terminal.focusNextPane` / `focusPreviousPane` | ⌘] / ⌘[ | inner panes, creation order, wrapping | – (`workspace::ActivateNextPane` is unbound on macOS) |
| `terminal.expandPane` | ⌘⇧Enter | expand/collapse an inner pane | ≈ `workspace::ToggleZoom` shift-esc (group level) |
| `terminal.equalizePaneSizes` | unbound | weighted equalize of inner panes | ≈ `workspace::ResetPaneSizes` (unbound) |
| `terminal.setTitle` / `clearPaneTitle` | unbound | pane title | – |
| `terminal.clear` / `terminal.search` | ⌘K / ⌘F | – | `terminal::Clear` cmd-k / search |
| `terminal.toggleNativeChat` | ⌘⇧J | terminal ↔ chat view of an agent tab | – |
| `editor.showEdit` / `editor.showDiff` [FORK §58] | ⌥⌘E / ⌥⌘D | swap a file tab's view | – |
| `floatingTerminal.toggle` | ⌥⌘A | floating workspace | ≈ `terminal_panel::Toggle` ctrl-` |
| `floatingWorkspace.maximize` / `minimize` | ⌥⇧⌘A / unbound | floating panel | – |
| Pin / Unpin tab | (menu only) | §2.5 | `pane::TogglePinTab` cmd-k shift-enter |
| Move tab to split ▸ side | (menu only) | §2.7 | `pane::SplitAndMove*`, `workspace::MoveItemToPaneInDirection` (unbound) |

**Unmapped chords:**
- Orca has no binding for group split, group resize, group zoom, next/prev group, swap groups or move tab to group.
- Zed's defaults for those (cmd-k arrows, cmd-k shift-arrows, shift-esc, ctrl-shift-pgup/pgdn) would be **additions**. For "the same" behaviour, remove them from the Orca keymap or at least make sure they don't shadow Orca chords.

---

## 4. Mapping to Zed

The Zed concepts map like this:

| Orca | Zed |
|---|---|
| worktree centre | `Workspace.center: PaneGroup` |
| group | `Pane` |
| layout tree | `Member` / `PaneAxis` (n-ary, with flexes) |
| tab | `Box<dyn ItemHandle>` |
| per-group MRU | `Pane.activation_history` |
| pinned prefix | `pinned_tab_count` |
| preview tab | `preview_item_id` |
| persistence | `SerializedPaneGroup`, plus `SerializableItem` per kind |

Verdicts: **Same** = Zed already does it; **Differs** = Zed has it but behaves differently (patch); **Missing** = build it.

### 4.1 Model and persistence

| Behaviour | Verdict | Notes |
|---|---|---|
| Many worktrees in one window, each with its own centre layout, swapped on worktree switch | Missing | A Zed `Workspace` has one `center`. Either keep one `Workspace`/centre per worktree and swap the window's root view, or patch `Workspace` to hold `center_by_worktree`. Check whether current Zed's multi-project window work covers this before patching. |
| Binary tree, new split = 50/50 of the target (3 columns = 50/25/25) | Differs | Zed inserts a same-axis sibling and **resets the whole axis to equal**. Patch `PaneAxis::split` to always wrap the target in a new 2-member axis with flexes [1,1]. |
| Removing a group promotes its sibling, keeping its size | Differs | Zed removes the member and **resets the flexes** of the axis. Patch `PaneAxis::remove` so the surviving member takes the removed 2-axis's slot and flex. |
| Ratio clamp [0.15, 0.85], committed on release, no double-click reset for groups | Differs | Zed clamps by px (80/100 min) and double-click resets the axis. Patch `compute_resize`; drop the double-click handler for the centre. |
| Per-group MRU, active tab on restore = MRU tail | Same | `activation_history` per pane. |
| Pinned prefix; pin → end of prefix; unpin → first unpinned | Same | `change_tab_pin_state`. |
| Preview tabs | Differs | Zed promotes on edit, double-click, pin or drop. Orca also promotes on **any activation** (except a sidebar re-click), and replaces a preview only with a compatible kind. Patch `activate_item` and `close_current_preview_item`. |
| Persistence of tree, flexes, active and pinned per pane, items by kind | Same (mechanism) | Zed's SQLite `pane_groups/panes/items` plus each item's own table. Orca's empty-group drop on save matches Zed dropping empty panes on deserialize. |
| Re-binding terminals to live sessions on restore | Missing | Zed's terminal `deserialize` **spawns a fresh shell** in the saved cwd. Our terminal item serializes `{tab id, pane tree, leaf→server session id, titles, expanded leaf}` and asks the server to attach by session id. The server answers reattach (snapshot) / cold restore (scrollback + new shell, same id) / fresh, as Orca's daemon does (plan §3c). |
| Stable ids (tab id, leafId, paneKey) usable by the CLI and agents | Missing | Zed `item_id` is an entity id. Store Orca-style UUIDs in each item and expose them over the client↔server layout API. |

### 4.2 Interactions

| Behaviour | Verdict | Notes |
|---|---|---|
| New tab inserted at the **end** (of its pin partition) | Differs | Zed inserts at `max(active+1, pinned_count)`. Patch `add_item_inner`'s default index, or pass an explicit index from our open paths. |
| Next/prev tab, all types, wrapping | Same | `ActivateNextItem{wrap_around:true}`. |
| Next/prev **same type**; next/prev terminal | Missing | New pane actions filtering by the item's kind. |
| Ctrl+Tab MRU switcher, hold-to-cycle, release commits, scoped to the focused group | Same (nearly) | `tab_switcher::Toggle` (current pane, MRU, preselects the previous, confirms on release). Turn off `show_file_preview` and match the row look. Orca's `sequential` mode needs a small patch. |
| Go to tab N, where 9 = 9th tab (no clamp) | Differs | Zed clamps to the last tab. Patch the handler, or add an Orca action. |
| Activate tab on pointer-up; a 12 px move starts a drag | Differs | Zed activates on mouse-down. Small patch in `render_tab`. |
| After close: MRU, then right, then left | Same (nearly) | `activate_on_close: "history"`. Zed's fallback when history is empty is the **left** neighbour; Orca's is **right then left**. Patch the fallback (one line). |
| Last group kept empty; empty worktree → deselected | Differs | Zed keeps the last pane (same). Set `when_closing_with_no_tabs` so the window never closes. Worktree deselection is ours to build. |
| Focus after a group is removed = first leaf of the sibling subtree (even when not focused) | Differs | Zed focuses `self.panes.last()` (most recently created). Patch `force_remove_pane` to use the tree sibling. Decide whether to keep the unfocused-removal quirk; it matters only for Close Others with surviving busy groups. |
| Pinned close: confirmation dialog | Differs | Zed `CloseActiveItem{close_pinned:false}` refuses and jumps to an unpinned tab. Ours prompts (`confirmClosePinnedTab`). |
| Close Others (menu): dialog, skip pinned, kill running terminals | Differs | Zed `CloseOtherItems` skips pinned, but a running terminal is `is_dirty` and prompts. Replace with our own close-scope command. |
| Keep only active with busy-terminal and agent-done rules, no dialog | Missing / Differs | Zed `CloseInactiveTabsAndPanes` is the closest shape. Implement our own: probe the server for each terminal tab's sessions (4 s, timeout = busy, agent `done` = idle), close the rest, collapse empty panes. |
| Close to left/right | Same in Zed, but **hidden in Orca** | Don't show them in our menu. |
| Reopen closed tab, cross-type, original group and index, terminal = fresh shell in cwd | Missing | Zed restores only file items into the **active** pane, without the index. Build a per-worktree closed stack (kind, state, pane id, index; caps 30/10/10/10). |
| Tab context menu contents (§2.6) | Differs | Replace Zed's pane tab menu with Orca's list. Item-specific entries come from `tab_extra_context_menu_actions`. |
| Inline rename; empty reverts; tab colours; status dot, bell, unread wash; fixed tab width; sticky active tab; chevrons | Missing / Differs | `tab_content` can draw the decorations. Fixed width, chevrons and sticky active tab are patches to `render_tab_bar`. Inline rename is an editor in the tab element. |
| Middle-click close, not on pinned tabs | Same | |
| Unfocused groups dimmed 0.95, accent border on the focused group | Same | `active_pane_modifiers { inactive_opacity: 0.95, border_size }`. |
| "+" menu with omnibox | Missing | Replace the pane's new-item menu. |
| Quick-command button per group | Missing | Tab bar end slot (`Pane::set_render_tab_bar_buttons`). |

### 4.3 Splits, drag and drop

| Behaviour | Verdict | Notes |
|---|---|---|
| Drag a tab to a group edge to split | Same mechanism, differs in detail | Zed: zone = `drop_target_size` (0.2) × **min(w, h)** from every edge, nearest edge wins, highlights half. Orca: left/right = 20% of **panel width**, up/down = 20% of **body height**, **tab strip excluded**. Patch `handle_drag_move`. |
| Drop on the body centre = append to that group | Differs | Check Zed's body-drop index (it drops via the active index); patch to append at the end. |
| Reorder within a strip, move into another strip at an index | Same | `DraggedTab` plus a per-tab drop target. |
| Alt-drag clones | Differs | Orca never clones. Disable the clone modifier (our items return `can_split() = false`, or patch `handle_tab_drop`). |
| No-op split drops (only tab on own edge; one-tab group onto the facing side of its sibling) | Differs | Add these checks in `handle_tab_drop`. |
| Previewing the target group's active tab while hovering, snapshot restore on Esc | Missing | Low priority but part of the feel. |
| Pinned partition not enforced by drag | Differs | Zed makes the pinned state follow the drop position. Decide; to be identical, don't change pinned state on drop. |
| Drag between windows or worktrees | Same (unsupported in both) | |
| "Move Tab to Split ▸ side" (needs ≥2 tabs) | Same | `SplitMode::MovePane`, i.e. `pane::SplitAndMove*`. Bind nothing by default. |
| Split command that clones | Remove | Zed's default `SplitRight` is `ClonePane`; Orca has none. |
| Group zoom / maximize / equalize | Zed has, Orca lacks | `ToggleZoom` (shift-esc), `ResetPaneSizes`. Unbind them to stay identical, or keep them as harmless extras. |
| Web views during drags | Missing | Zed has no webview. With a native child view (wry/WKWebView), hide it or swap it for a snapshot while a GPUI drag is active. Never focus it mid-drag. Keep ⌥⌘arrows working while the page has focus (route them back to GPUI). |

### 4.4 Directional focus

| Behaviour | Verdict | Notes |
|---|---|---|
| ⌥⌘arrows between groups, no wrap | Differs | Zed `find_pane_in_direction` probes **one point** 4 px past the edge at the cursor's y (or the centre's). Orca picks the **nearest gap with perpendicular overlap, ties to the larger overlap**. These differ when a neighbour column is split, e.g. the active group is tall and the left column holds two groups: Zed picks the one at the cursor or centre height, Orca the one sharing more edge (DOM order on a full tie). Patch in our algorithm (§2.9), using `bounding_boxes`. |
| At the edge: consume, do nothing | Differs | Zed falls through to the **docks** (left, right or bottom). Patch: no dock fallback in the centre. |
| Single group: ⌥⌘← / → = worktree history back / forward | Missing | Our action handler. |
| Landing: activate the active tab, focus the terminal's active inner pane, empty group takes focus | Same (nearly) | Zed focuses the pane's active item. Our terminal item focuses its `activeLeafId` leaf. |
| Next/prev group | Neither in Orca's keymap | Zed's `ActivateNextPane` is unbound on macOS; leave it unbound. |

### 4.5 Terminal tabs

| Behaviour | Verdict | Notes |
|---|---|---|
| Terminals as centre tabs | Same | `NewCenterTerminal` puts a `TerminalView` in the centre pane. Orca has **no bottom terminal panel**; don't use `TerminalPanel` for normal terminals. |
| **Nested splits inside one terminal tab** | Missing (verified) | `TerminalView` wraps exactly one `Terminal`. Zed "splits" terminals only by adding `Pane`s (in the terminal panel or the centre). Build a `TerminalTabItem` that owns a binary pane tree of leaf `TerminalView`s (§4.7). |
| ⌘D / ⌘⇧D = inner split | Differs | Zed's Terminal-context cmd-d is `pane::SplitRight` (a new group). Rebind to our inner split. |
| ⌘W closes the inner pane first | Missing | Terminal-context `cmd-w` → our close-pane action. When the last pane closes, it closes the item. |
| ⌘] / ⌘[ next/prev inner pane, creation order, wrapping | Missing | Terminal context. |
| ⌘⇧Enter expand pane, persisted | Missing | Zed's zoom is pane-group-level and not persisted per pane. |
| Weighted equalize, double-click divider = 50/50, min 50 px | Missing | Port `PaneAxis` resize code into the inner tree. |
| Drag an inner pane within the tab; drag an inner pane out to a strip = new tab with the live session | Missing | The session moves; it is not respawned. Server RPC `move_session_to_tab` (Orca: `pty.moveLeafToNewTab`). |
| Drop files on a terminal = paste paths | Same | `TerminalView::handle_drop`. |
| PTY on the server, client streams | Missing | Zed's PTY is local (remote = local ssh wrapper). Patch the `Terminal` byte source (plan §3c). `Terminal::new_display_only` + `write_output` is a usable starting point. |
| Floating terminal | Missing | Closest is `TerminalPanel` (a dock with its own PaneGroup), but Orca's is an overlay window with groups of any tab kind. It is [CUT?]; if kept, build an overlay hosting a second centre `PaneGroup` for a scratch worktree. |

### 4.6 Tab kinds as Zed `Item`s

| Kind | Zed today | Plan |
|---|---|---|
| Terminal tab | `TerminalView` (one terminal) | **`TerminalTabItem`** (new): owns the inner tree. Leaves are server-backed `TerminalView`s. `tab_content` shows agent icon/dot, bell, colour, pin and collapse button. `is_dirty = false` (Orca never treats terminals as dirty; the running check is a separate probe). `can_split = false`. Serialized with ids, tree and session ids. The agent views (Terminal / Chat / Claude web) are a per-leaf view switch inside this item. |
| Agent chat | `AgentPanel` dock only; the test-only `ThreadViewItem` proves a `ConversationView` can live in an item | **`AgentThreadItem`** (new) wrapping `ConversationView`, attached to a server-held ACP session (plan §2). Serialized as the session id. Close guard = "agent busy" dialog. |
| Browser / PR page | none | **`BrowserItem`** (new) around the gpui-component wry `WebView` (plan §3d). Dedupe per worktree; mute; favicon; Duplicate; Open In Browser; drag and focus gating as in §4.3. |
| Editor / markdown | `Editor` item | Reuse as is. Dedupe per pane already matches. Add the inline file-rename menu entry. |
| Diff | `ProjectDiff`, `TextDiffView`, … | A per-file diff item, read-only. A **`FileViewSwitch`** behaviour: ⌥⌘E/⌥⌘D (or their overrides) replace the tab with the other view at the cursor line, plus the "one view per file per group" closing rule. |
| Conflict review, check details, simulator | – | New items as those features land (the simulator is probably cut). |

### 4.7 The inner terminal tree

- Do **not** nest Zed `Pane`s inside an item. That would bring a tab bar, item semantics and focus rules that don't apply.
- Build a small `SplitTree<Leaf>` widget instead:
  - binary nodes with ratio;
  - a divider with min 50 px, double-click → 0.5, and coalesced resize. Send the PTY only the final size, as Orca does.
  - expand: hide off-path siblings;
  - weighted equalize;
  - the focus-successor rule;
  - creation-order cycling;
  - nearest-edge drop zones for pane drags.
- About 600–900 lines of GPUI; the divider logic can be copied from `pane_group.rs` `compute_resize`.

---

## 5. Recommendation (ranked by effort, behaviour identical)

### Reuse as-is

1. `Pane` item list, activation history (MRU), pinned prefix rendering, middle-click, per-tab drop targets for reorder and cross-pane moves.
2. `tab_switcher` for Ctrl+Tab, with preview off.
3. `active_pane_modifiers` (inactive opacity 0.95, focused border).
4. Zed's persistence plumbing (`SerializedPaneGroup`, `SerializableItem` registry).
5. `Editor` as the file tab, and the git diff views as the base of the diff item.
6. Keymap contexts, to express "terminal context ⌘W/⌘D/⌘]" vs "workspace context".

### Patch (small, in `workspace` — keep each patch behind an `orca_layout` setting to ease rebases)

7. **Small patches:**
   - Activate-on-close fallback: right then left.
   - New-item insertion at the end.
   - `ActivateItem(n)` without clamping.
   - Activate on mouse-up.
   - Preview promotion on activation, with compatibility-based replacement.
   - Pinned close: ask instead of skipping.
   - No pin-state change on drop.
   - Disable alt-drag clone.
   - Body drop appends at the end.
8. **Edge zones:** width-based for left/right, body-height-based for up/down, tab strip excluded. Add the no-op checks.
9. **Directional focus:** replace the probe-point search with Orca's gap/overlap algorithm, and drop the dock fallback in the centre. Add the single-group history fallback in our action.
10. **Binary tree semantics in `PaneAxis`:** split always wraps; remove promotes the sibling with its flex; ratio clamp [0.15, 0.85]; no double-click reset; focus after removal goes to the tree sibling.
11. **Tab bar look:** fixed widths, chevrons, sticky active tab, Orca's context menu list, Pane Actions button, quick-command slot, new-tab menu hook.
12. **Default keymap:** a dedicated Orca base keymap mirroring §3. Remove the Zed chords that collide or add behaviour Orca lacks: cmd-1…9 ActivatePane, alt-cmd-arrows next/prev item, alt-cmd-t close others, cmd-k chord splits, shift-esc zoom. Import `~/.orca/keybindings.json`, so a user's overrides land (including Ctrl+letter chords, captured even in the terminal under `orca-first`).

### Build (new `orca_*` crates)

13. **Close rules:**
    - Keep-only-active with server-side busy probes and agent-done.
    - Close Others with a dialog.
    - Close split pane.
    - The cross-type reopen-closed stack with original pane and index.
    - Worktree deselection when it empties.

    Medium effort.
14. **Per-worktree centres in one window,** with swap on worktree switch and layout restore per worktree. Medium; the main structural decision. Check Zed's current multi-project window support first.
15. **`TerminalTabItem` with the inner split tree:**
    - split ⌘D/⌘⇧D, close ⌘W, cycle ⌘]/⌘[, expand ⌘⇧Enter, equalize, titles;
    - pane drag inside the tab and out to a strip (moving the server session);
    - restore by session id with reattach / cold / fresh.

    Medium-large; depends on the server terminal protocol.
16. **`AgentThreadItem`** in the centre on top of `agent_ui`'s `ConversationView`, attached to server sessions. Medium.
17. **`BrowserItem`** (wry) plus drag/focus gating and arrow routing. Medium-large; see plan §3d.
18. **Editor/diff one-tab view switch** (§58) and the one-view-per-file rule. Small-medium.
19. **Same-type and terminal-only cycling, tab colours, inline rename, status decorations, "+" omnibox, quick-command run/toggle.** Small each; the omnibox is medium.
20. **Drag hover preview of the target group's active tab, plus snapshot restore on cancel.** Small, last.
21. **Floating workspace.** Only if not cut: an overlay hosting a second centre for a scratch worktree. Medium.

### Decisions to confirm with the reference user

- **Keep or drop the quirks:**
  - focus jumps to a removed group's sibling even when that group was unfocused;
  - inner pane cycling follows creation order rather than visual order;
  - pinned tabs can be dragged out of the pinned prefix.
- **Zed extras:** whether group zoom, equalize and keyboard group splits should exist even though Orca has none. "The same" says no default bindings.
- **features.md §5 vs overrides:** features.md §5 lists Orca's defaults. A user's `keybindings.json` can swap or rebind any of them (the reference setup does; see `private/reference-setup.md`), so the preset must come from the imported file, not from features.md.
