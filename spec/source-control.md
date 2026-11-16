# Source Control / Changes panel: exact spec for the rewrite

This file specifies the Source Control panel as it is built in the Orca fork today (`~/Developer/orca`, fork `frbarbre/orca`, read 2026-11-15). It covers:
- the single Changes list and how it is built
- its filters and toggles
- file classification
- the sections around the list
- how the list drives the diff viewer
- the history of what was replaced
- a general, configurable filter model, plus a preset that reproduces today's behaviour exactly

It does not repeat [features.md](features.md) §2.7 / §2.9 / §5 or [plan.md](plan.md); it goes deeper on the same area. Paths are relative to `~/Developer/orca/src/` unless noted. "SC" means Source Control, `listing/` means `renderer/src/components/right-sidebar/source-control/listing/`, and `panel/` means the sibling `panel/` directory.

Where this spec mentions "the reference setup", the concrete values are in `private/reference-setup.md` (not in the repo), taken from the reference user's settings export.

---

## 0. One-paragraph summary

SC shows **one list, "Changes"**: every file whose content on disk differs from the **merge base of HEAD and the workspace's base ref** (normally `origin/main`, or the parent branch for a stacked PR). That covers committed, staged, unstaged and untracked changes, one row per file, updated live. There are no staged/unstaged sections and no commit box.

Three independent narrowing layers sit on top:
1. **A scope**, exactly one of: everything (default), *Uncommitted*, *an agent turn*, *a commit*. It is shown as a removable badge in the header.
2. **A type/area filter.** Hide-checkboxes for file types (Imports only, Implementation, Tests, Docs, Generated, Lint baselines, …) and areas (Backend, Frontend). When active it shows as a labelled pill.
3. **A path text filter.**

The header shows the file count and the summed +/− of the listed rows. Clicking a row opens that file's diff as the current scope defines it. Next/previous-file keys walk the rows in display order.

---

## 1. The Changes list model

### 1.1 What "the base" is

**Base ref resolution** (`shared/source-control-base-ref-resolution.ts`, `source-control/sync/use-base-refs.ts`). The first non-empty value wins:

1. **The worktree's `baseRef`** (worktree metadata).
   - Written at worktree creation as `args.compareBaseRef ?? remoteTrackingBase?.ref ?? baseBranch` (`main/ipc/worktree-remote.ts:1520`). That is the remote-tracking ref the worktree was branched from, e.g. `origin/main`, or `origin/<parent>` for a stacked branch. A review workspace gets the PR's target branch.
   - Special case: if `baseRef` is a full 40-hex commit sha **and** the workspace has a hosted review with a `baseRefName`, the effective base is the review's target branch, matched or rewritten against the repo/default candidates.
2. **The repo's `worktreeBaseRef`** (project setting).
3. **The detected default** (`shared/git-default-base-ref.ts`):
   - the target of `refs/remotes/origin/HEAD` if it is set
   - else the first of `origin/main`, `origin/master`, `main`, `master` that exists (one `git for-each-ref` call)
   - else `null`, and then **no compare runs at all**. The code never fabricates `origin/main`.

**Compare base vs merge target.**
- `compareBaseRef` is what the list compares against.
- `effectiveBaseRef` is the PR/rebase target.
- They differ only when the `sourceControlCompareAgainstUpstream` setting is on. Then compare uses the pinned worktree/repo base, else the branch's upstream (`upstreamName`), else `effectiveBaseRef`.
- With it **off** (the default), compare = effective base.
- If the worktree `baseRef` is a *reviewed commit* (the Review section's "diff starts at" picker, features §2.11 / FORK §4, §14), `compareBaseRef` is that ref verbatim.

**Changing the base.** Overflow menu → "Change Base Ref…" opens a ref picker.
- If the worktree owns a base, it writes worktree `baseRef`. Otherwise it writes repo `worktreeBaseRef`.
- "Use primary" clears it.
- Open diff tabs of that worktree are retargeted to the new compare (FORK §23, `git/retarget-branch-diff-files.ts`).

**Ref qualification** (`shared/worktree/base-ref.ts`). A name containing `/` is tried as `refs/remotes/<name>` and then `refs/heads/<name>`. A bare name is tried as `refs/heads/<name>`. `refs/…` is used as given. This is so tags never shadow branches.

**Merge base** (`main/git/source-control/branch-compare.ts`, `getBranchCompare`):
- Resolve `HEAD` and the base to oids.
- `mergeBase = git merge-base <baseOid> <headOid>`.
- Exception: a review base (`refs/orca/review-base/*`, an interdiff base) is used exactly, with no merge-base.
- Ahead/behind come from `git rev-list --left-right --count base...HEAD`.

Summary status values:

| status | when | effect on list |
|---|---|---|
| `loading` | compare in flight | previous summary kept if same base (`preserveExistingSummary`) |
| `ready` | ok | rows are base-relative |
| `invalid-base` | base ref doesn't resolve | list falls back to uncommitted-only rows (see 1.3) |
| `no-merge-base` | unrelated histories | same |
| `unborn-head` | no commit yet | same |
| `error` | git failed | same |

**Fetch behaviour.** The panel **never fetches on its own.**
- `origin/<base>` moves only when something fetches:
  - worktree creation (`worktrees:prefetchCreateBase`, when `refreshLocalBaseRefOnWorktreeCreate` is on)
  - a user's or an agent's `git fetch` / pull / sync
  - upstream's Fetch/Sync actions
- After a remote action, status, branch compare and history all refresh (`sync/remote-refresh.ts`).
- Upstream (ahead/behind of the branch's own upstream) is read when the panel becomes visible (`use-upstream-status-fetch.ts`). That is a status read, not a network fetch.

> Rewrite: same chain. Keep "no base → no compare" (never guess). The server owns it. Expose `base_ref` per workspace and per project, and a `compare_against_upstream` flag. An optional background fetch of the base ref is a new feature; it would make the list move under the user, so default it off.

### 1.2 The three git reads that feed the list

All three run on the host that owns the repo; in the rewrite that is the server.

1. **Committed branch changes**: `git diff --raw --numstat -z -M -C <mergeBase> <headOid> --`, parsed by `shared/git-change-list.ts`. Each entry is `{path, status: added|modified|deleted|renamed|copied, oldPath?, added?, removed?}`. A binary file has no counts.
2. **Working-tree status**: upstream's `git status --porcelain=v2 -z --untracked-files=all` with per-file numstat (`main/git/source-control/status-read.ts`). Each entry is `{path, status, area: staged|unstaged|untracked, oldPath?, added?, removed?, conflictKind?, conflictStatus?, submoduleRoot?}`. A path can appear twice (staged and unstaged).
3. **Branch line total**: one ranged diff, **merge base → working tree**, `git -c core.quotePath=false -c diff.autoRefreshIndex=false diff -z --numstat -M <mergeBase> --` (`shared/git-branch-line-total.ts`).
   - Untracked files are added by reading them (`collectUntrackedAdditions`: every line counts as added).
   - It yields `files: {path → {added, removed}}` (FORK §55, PR 70) plus totals split into test and generated by the path heuristics (upstream's line-total chip).
   - If an untracked path also appears in the tracked numstat or as a rename source, the whole total is `null`, since summing would double count.
   - Soft deadline 500 ms: status publishes without it and the total backfills when it arrives. Hard timeout 15 s.
   - Cool-down after a slow run is `min(duration, 30 s)`. One in-flight run is shared per (host, path, mergeBase).
   - The renderer only uses `files` when `total.mergeBase === compareSummary.mergeBase` (`selectBranchLineCounts`, `listing/scoped-change-entries.ts`).

### 1.3 Merging into one row per file: `buildAllChangeEntries` (`listing/all-changes.ts`)

Inputs are the status entries and the branch entries (after the type and text filters, see §2.4), plus `{uncommittedOnly, lineCounts}`.

```
byPath = branch entries keyed by path, then every status entry appended to its path's `local` list
         (status entries inside a submodule — `submoduleRoot` set — are skipped)
for each path:
  if uncommittedOnly and local is empty → drop
  deletedLocally = any local.status == deleted and none untracked
  fromBase = (branch && !uncommittedOnly) ? {branch.status, branch.oldPath} : localStatus(local)
     localStatus: any untracked|added → added;
                  else first renamed|copied → that status + its oldPath;
                  else modified
  if deletedLocally and fromBase.status == added → drop      (added on branch/index, then deleted: nets to nothing)
  status  = deletedLocally ? deleted : fromBase.status
  oldPath = (!deletedLocally && fromBase.oldPath) ? fromBase.oldPath : none
  counts  = uncommittedOnly ? (local has exactly 1 entry ? that entry's counts : none)
          : lineCounts[path] ?? (exactly one source among [branch?, ...local] ? its counts : none)
rows sorted by compareFileNames(path)    (shared/file-name-sort)
```

Worked cases (from `all-changes.test.ts`):

| situation | row |
|---|---|
| added on branch, edited since | `A` |
| untracked new file | `A`, every line counted as added |
| committed modify, then deleted on disk | `D` |
| added on branch, then deleted on disk | no row |
| staged rename, nothing on branch | `R old → new` |
| renamed on branch, edited since | `R before → path` |
| staged + unstaged, nothing on branch | `M`; the count comes from the branch total if measured, else none |
| inside a submodule | no row |

- **Binary files**: listed, with no counts (numstat `-`).
- **Conflicted files**: listed. Opening one goes to the conflict resolver (§5).
- **Uncommitted scope** (PR 100): each row describes the uncommitted change only. Its status comes from the local entries and its counts from the single local entry. A file staged *and* unstaged therefore shows no count.

> Rewrite: compute all of this on the server and stream one row per path with `{path, status, oldPath, added, removed, binary, sources: {committed, staged, unstaged, untracked}, conflict?, discardable}`. Keep the per-row "exact count from merge base → disk" as the default count. It is what makes the header total honest.

### 1.4 Live updates

- **Git status** (`right-sidebar/useGitStatusPolling.ts`):
  - Triggered by file-watcher, terminal and push signals.
  - The trigger is debounced by 125 ms, with at least 3 s between runs (duration-aware).
  - A safety poll runs every 60 s.
  - Opening the panel refreshes at once.
- **Branch compare** (`sync/use-branch-compare.ts`):
  - Runs at once when the HEAD oid, the base ref, or the remote ahead/behind/upstream changes.
  - Otherwise polls every 30 s while visible, with idle time growing with the last run's duration up to 5 min.
  - Runs are serialized, and skipped ticks collapse into one trailing run.
  - It also recovers when the window becomes visible.
- **Line total**: published with the status (see 1.2). A change to only a file's count republishes it (`shared/git-branch-line-total-files.ts`).
- **Review attributes**: re-read when the set of changed paths changes, and on every status refresh while a `.gitattributes` is among the changed paths. Imports-only is re-read when any candidate's counts change (see 3.5).
- **Agent turns**: the list reloads on `agentTurns.onChanged` for that worktree path.
- **Open diffs**: a `branch-worktree` tab reloads on file-watcher events and on every status change, like an unstaged diff.

The server should push row diffs instead of having the client poll. Keep the coalescing: status ≥ 3 s apart under load, compare after a HEAD move.

### 1.5 The Changes section header

`listing/all-changes-section.tsx`, left to right:

- **"CHANGES"**, the shared 28 px section header: chevron, 11 px uppercase label, 10 px count.
- **Count** = rows listed.
  - Tooltip "N files changed vs <base>" only when nothing narrows the list (count == total) and the scope is not a turn or commit.
- **Line total** "+A −R": the sum of `added`/`removed` over the **listed** rows, so every filter narrows it (PR 70).
  - Hidden when both are 0.
  - Tooltip: "A lines added, R lines deleted in the listed files".
  - Rows with unknown counts add 0.
- **Scope badge**, when a scope is set: a pill with the label and an × button ("Show all changes").
  - Uncommitted → **"Uncommitted"**.
  - Turn → **"Latest turn"** while it is the newest turn, otherwise **"Historical changes"**. The tooltip is the turn's prompt.
  - Commit → **"Commit <compareRef>"**, a short sha. The tooltip is the subject.
- **Type/area filter trigger**:
  - an icon button (ListFilter) while nothing is hidden
  - a **pill naming what is still shown** while a filter is active, e.g. `Implementation · Frontend` (PR 78), see 2.2
  - only rendered when more than one filter key is present or a filter is active

The section stays rendered when a filter or scope empties it, so the controls stay reachable. With nothing changed and no filter it disappears, and the upstream empty state shows.

Above the sections, the panel header (upstream) shows:
- the HEAD/branch line, with the branch line-total chip (+/− with test/generated split in its hover)
- "vs <base>" with ahead/behind
- compare state (spinner, error, Retry)
- the overflow menu: View as list/tree, Group by status, Change Base Ref…, Refresh branch compare
- the text-filter toggle

The header has no Create PR button (FORK §51) and the panel has no commit box (§35).

### 1.6 Rows (`listing/branch-entry-row.tsx`)

- Material file icon, tinted by status colour.
- File name; in list view, the dimmed parent dir after it.
- Badges:
  - your diff-note count
  - **agent-note count** (bot icon, `agent-note-file-badge.tsx`)
  - unresolved PR review threads
  - pending (unsent) review comments
- `+A −R` and a status letter (`ChangeEntryStat`).
- **Discard** hover button (PR 186). Shown only when the file has a discardable uncommitted change (unstaged or untracked, not conflicted, not a submodule: `canDiscardStatusEntry`).
  - It never appears under a turn or commit scope.
  - It goes through the usual confirmation and throws away **only** the uncommitted change; committed branch changes stay.
  - The label is "Discard changes", "Delete untracked file" or "Restore file", by kind.
- Right-click menu: open the diff, reveal in explorer, copy paths, open in external app, and the same Discard item.
- Draggable as a workspace file path.
- Highlight (`bg-accent`): any open tab of this path in this worktree, whatever its diff source (`edit`, `unstaged`, `staged`, `branch`, `commit`, `branch-worktree`), highlights the row (`buildAllChangesOpenRowKeys`). The list scrolls that row into view (`scrollToRowKey`).
- Click, double-click and modifier-click: see §5.1.

**Tree vs list.** The `sourceControlViewMode` setting (`"list"` or `"tree"`) is global. It is toggled from the overflow menu.
- Tree: `buildSourceControlTree` → `compactSourceControlTree` (single-child directory chains fold into one row) → flatten, honouring collapsed directories.
- Collapsed directories are panel state that resets on worktree switch.
- The **review order** (what next/previous-file walks) is the visible file rows of the tree in tree mode, and the sorted paths in list mode. A collapsed directory's files are skipped.
- Rows are virtualized against the panel's single scroller.

---

## 2. Modes, toggles and filters

### 2.1 The scope ("Changes filter"; `renderer/src/lib/changes-filter.ts`)

There is one value per worktree:

```
ChangesFilter = null
              | {kind:'uncommitted'}
              | {kind:'turn',   compare:{commitOid,parentOid,compareRef,baseRef}, entries[], prompt, latest}
              | {kind:'commit', compare:{…}, entries[], subject?, message?}
```

- **Mutually exclusive.** Setting one replaces the other.
- **Per worktree.**
- **In memory only**, a module-level map. It is lost on reload or restart, and not saved per workspace.
- **Kept outside the panel**, so the shortcuts work with SC closed.

| Scope | Rows | Row counts | Opening a row | Discard |
|---|---|---|---|---|
| none (default) | §1.3, base-relative | exact merge base→disk | `branch-worktree` diff (base → disk) | yes |
| Uncommitted | files with any staged, unstaged or untracked change; status from local only | the single local entry's counts, else none | that file's uncommitted diff, preferring **unstaged → untracked → staged** (`pickUncommittedRowEntry`) | yes |
| Turn | the turn's frozen file list (`commitCompare(turn.oid)` = start snapshot → end snapshot), sorted | from the turn's numstat | `commit` diff of that snapshot pair. The **latest turn too** (PR 71): it shows what the turn changed, never the live disk | no |
| Commit | the commit's files (parent → commit, `-M -C`) | from its numstat | `commit` diff | no |

Entry points:

| Action | Effect |
|---|---|
| **⌥⌘U** `sourceControl.toggleUncommittedOnly` | Uncommitted on; or off if it is already on. Opens the right sidebar on SC at once (PR 77). macOS only by default. |
| **⌥⇧⌘T** `sourceControl.openLatestAgentTurn` | If the current scope is a turn with `latest`, clear it (PR 69). Otherwise list the turns, take the newest, set the turn scope, and open its first file. With no turns, a toast: "No agent turns in this workspace yet." Opens SC at once. |
| Click a row in Agent turns | That turn's scope (`latest` = it is `turns[0]`). Opens its first file. It never toggles off. |
| Click a commit in Commits, or "Show in Changes" under an expanded commit | Commit scope; opens its first file. |
| Git blame hover → "Show commit" | Commit scope for that sha (`lib/monaco-git-blame.ts`). |
| × on the badge | Scope cleared. |

- "Opens its first file" means the alphabetically first path (`compareFileNames`), as a **preview** tab (`lib/show-in-changes.ts`). It also opens the right sidebar on SC.
- **Latest stays honest.** When a newer turn lands, a previewed turn's badge flips to "Historical changes" (`syncLatestAgentTurn`, run from `panel-ready.tsx` when `turns[0].oid` changes). It flips back if it becomes the newest again.
- **The open diff follows the scope** (PR 82, `listing/open-diff-follows-filter.ts`). When the scope key changes (`all` | `uncommitted` | `turn:<oid>` | `commit:<oid>`), the active tab is reopened the way clicking its row in the new list would, and the old tab is closed. This only happens if:
  - the tab is a diff
  - it is not dirty
  - it belongs to this worktree
  - its path is in the new list

### 2.2 File type and area filter (`listing/file-category*.ts`, `file-scope-store.ts`)

**What it hides.** A set of hidden keys, `SourceControlFileFilterKey` = category | area.
- A row is hidden if its **category** is hidden, or it lies in a hidden **area**.
- The two groups are combined with AND: show = type shown ∧ (no area ∨ area shown).

**Persistence.** Per worktree (PR 15), in a module-level store (`file-scope-store.ts`), so quick open reads the same filter. It is **not** saved across restarts.

**Which keys are offered.** Only keys present in the current changes: the union of status entries and branch entries, classified (`collectSourceControlFileCategories`). The order is fixed:

1. Types: `implementation, imports, test, documentation, generated, lint-baseline, agent-guidance, localization, assets`
2. Areas: `backend, frontend` (fork values; see §3.6)

**"Active"** means a hidden key is among the present keys. A hidden key that is absent narrows nothing and does not count as active.

**Menu** (dropdown, `align=end`, 192 px wide):
- "SHOW FILE TYPES", then one checkbox per present type. Checked = shown. The menu stays open on toggle.
- Separator.
- "SHOW AREAS", then the present areas.

Labels: Implementation, Imports only, Tests, Documentation, Generated, Lint baselines, Agent guidance, Localization, Assets, Backend, Frontend.

**Trigger label** while active: per group, nothing if that group hides nothing. Otherwise the names still shown, joined with ", ", or "No file types" / "No areas" when all are hidden. The two group labels are joined with " · ".

**Where it applies.** The rows, the count, the header total, the review order (stepping), quick open's changed-file rows (PR 149/150), and the "Group by status" sections.

> **Quirk to decide:** under a **turn or commit scope the type/area filter and the text filter are not applied.** `buildScopedChangeEntries` returns `changesFilter.entries` unfiltered, but the pill still reads as active. Under **Uncommitted** both apply. See §8.

### 2.3 Text filter

The header search toggle filters paths by case-insensitive substring of the trimmed query. Over 2 KiB it is treated as "too large" and matches nothing. It resets on worktree switch. It narrows the Changes list in the default and Uncommitted scopes (same caveat as above).

### 2.4 How the layers compose (exact order)

```
status entries ─┐  (type/area filter → text filter)          ┐
branch entries ─┘  (type/area filter → text filter)          ├─ buildAllChangeEntries({uncommittedOnly, lineCounts}) → rows
                                                             ┘
scope = turn|commit → rows = scope.entries sorted (no type/area/text filtering)
rows → tree/list → review order;   rows → header count and total
```

### 2.5 Grouping mode

`sourceControlGrouping`: `'all'` (unset = all) or `'status'`.
- `'status'` restores upstream's Staged / Changes / Untracked / Committed-on-branch sections, with staging, a commit box path and View all (combined diff).
- It is toggled from the overflow menu ("Group by status" / "Show all changes in one list") and saved globally.
- `sourceControlGroupOrder: 'changes-first'` only matters there.

The reference setup does not use `'status'`. The rewrite can drop it, or ship it as a later option.

### 2.6 Shortcuts in this area

| Action id | Default (macOS) | Scope |
|---|---|---|
| `sourceControl.toggleUncommittedOnly` | ⌥⌘U | global |
| `sourceControl.openLatestAgentTurn` | ⌥⇧⌘T | global |
| `editor.nextChange` / `previousChange` | F7 / ⇧F7 | editor + outside-editor fallback |
| `editor.nextFile` / `previousFile` | ⌥F7 / ⌥⇧F7 | editor + fallback |
| `editor.showEdit` / `showDiff` | ⌥⌘E / ⌥⌘D | editor |
| `editor.addReviewNote` | ⇧⌘A | editor |
| `sourceControl.focusReviewSummary` | ⇧⌥⌘R | global |
| `sourceControl.sendQueuedComments` | ⌥⌘S | global |

All of these are rebindable; the reference setup overrides several (see `private/reference-setup.md`, not in the repo). Bindings without a modifier (for example bare arrows for next/previous change) are allowed and must work (§5.3).

- The ⌥⌘/Ctrl+Alt defaults are macOS-only, because Ctrl+Alt is AltGr on Windows and Linux.
- There is **no shortcut** for the type/area filter or the text filter today.
- Upstream's Mod+Shift+G reveals Source Control.

---

## 3. File classification

### 3.1 Categories

Each file gets exactly **one** type and at most **one** area.

Types, in display order: `implementation`, `imports` (Imports only), `test`, `documentation`, `generated`, `lint-baseline`, `agent-guidance`, `localization`, `assets`.

### 3.2 Inputs

**Git attributes.** `git -c core.quotePath=false check-attr -z --stdin <names…>` with all changed paths on stdin, NUL-separated (`main/git/check-review-attributes.ts`, `shared/git-review-attributes.ts`).
- Limits: at most 2000 paths, 15 s timeout.
- Attributes asked for:
  - `review-implementation`, `review-test`, `review-documentation`, `review-generated`, `review-agent-guidance`, `review-localization`, `review-assets`, `review-imports` (the `review-*` set follows [Linear's convention](https://linear.app/enablement/guide/diffs#organize-files-with-.gitattributes))
  - `linguist-generated`, `linguist-documentation` (GitHub's)
- State mapping: `set`/`true` → set; `unset`/`false` → unset; anything else (`unspecified`, a value) → ignored.
- Because git answers, nested `.gitattributes`, `.git/info/attributes`, `core.attributesFile` and uncommitted edits all count.
- On failure (an older host) the result is no attributes, so path heuristics only.
- `linguist-vendored` is **not** read.

**Imports-only marks.** §3.5 sets `review-imports: set` on qualifying paths, overriding any value from `.gitattributes`, including `-review-imports`.

### 3.3 Algorithm (`listing/file-category.ts`, `classifySourceControlFilePath`)

```
1. if attributes: first attribute in this list whose state is `set` decides:
     review-imports→imports, review-generated→generated, review-test→test,
     review-documentation→documentation, review-agent-guidance→agent-guidance,
     review-localization→localization, review-assets→assets, review-implementation→implementation,
     linguist-generated→generated, linguist-documentation→documentation
2. h = path heuristic: LINT_BASELINE → lint-baseline; else isGeneratedCodePath → generated;
                       else isTestCodePath → test; else implementation
3. if any attribute that maps to category h is `unset` → implementation, else h
     (generated is removed by -review-generated or -linguist-generated; test by -review-test;
      lint-baseline has no attribute and can't be removed; implementation stays implementation)
```

Note that unsetting a heuristic's attribute yields `implementation`. It does **not** fall through to the next heuristic: a `.snap` file with `-linguist-generated` becomes implementation, not test.

`agent-guidance`, `localization` and `assets` come **only** from attributes. No path heuristic produces them.

**Areas.** The first prefix match from a hardcoded list of two path prefixes (`AREA_PREFIXES`), one → backend, the other → frontend, else none. The prefixes are specific to the reference setup's work repo (exact values in `private/reference-setup.md`); think `server/` → backend, `web/` → frontend.

### 3.4 Path heuristics (verbatim)

All are case-insensitive unless noted, and accept `/` and `\` (`shared/generated-code-path.ts`, `shared/test-code-path.ts`, `listing/file-category.ts`).

**Lint baseline (fork):**

```
(?:^|/)(?:\.<linttool>|\.[^/]*baseline[^/]*)(?:/|$)       flags i
```

It matches a path segment named after one specific lint tool's state directory (`.<linttool>`; the exact name is in `private/reference-setup.md`), or any dot-named segment containing "baseline", e.g. `.eslint-baseline/` or `.lint-baseline.json`. The generic part is `(?:^|/)\.[^/]*baseline[^/]*(?:/|$)`.

**Generated** (upstream). The path is generated if any of these match:
- **Directory segment** `(?:^|[/\\])(?:__generated__|__pycache__|\.next|\.nuxt|coverage|dist|generated|node_modules|vendor)[/\\]`. `build/`, `target/`, `out/` and `bin/` are deliberately excluded.
- **Lockfile basename:** `bun.lock(b)`, `Cargo.lock`, `composer.lock`, `flake.lock`, `Gemfile.lock`, `go.sum`, `gradle.lockfile`, `mix.lock`, `npm-shrinkwrap.json`, `package-lock.json`, `packages.lock.json`, `Pipfile.lock`, `pnpm-lock.yaml`, `poetry.lock`, `pubspec.lock`, `uv.lock`, `yarn.lock`.
- **Basename suffix (`…$`)** — any of:
  - `.generated.<ext>`, `.designer.<ext>`
  - `[._](pb|pb2|pb2_grpc).<…>.<ext>`
  - `[._]gen.<ext>`, `_generated.<ext>`
  - `.g.dart`, `.freezed.dart`
  - `.min.(js|css|mjs)`
  - `.(js|css|mjs).map`
  - `.snap`

**Test** (upstream). The path is a test if any of these match:
- **Directory segment** `(?:^|[/\\])(?:__mocks__|__snapshots__|__tests__|cypress|e2e|spec|tests?|testdata)[/\\]`. `specs/` is excluded.
- **Basename suffix:**
  - `.test.<ext>`, `.spec.<ext>`
  - `[-_](test|spec)s?.<ext>`
  - `test_*.py`, `conftest.py`
  - `.snap`
- **Case-sensitive type suffix:** `(Test|Tests|Spec)\.(java|kt|kts|scala|groovy|cs|swift|php|rb)$`.

### 3.5 "Imports only" (fork, PR 189)

**Shared rule** (`shared/import-only-change.ts`, `isImportOnlyChange(path, before, after)`):

1. Language by extension: `ts tsx mts cts js jsx mjs cjs` → script; `py pyi` → python; anything else → **false**.
2. Split each version into lines (`\r?\n`) and walk them, partitioning into *import statements* and *code lines*:
   - **Script import start:**
     - `^\s*import(?:\s|\{|\*|['"])`: `import x`, `import {`, `import *`, `import 'side-effect'`, `import type …`. Dynamic `import(` is code.
     - Re-export start: `^\s*export\s+(?:type\s+)?(?:\*|\{)`.
   - **Script statement end:** the first line from the start on that matches `(?:from\s*|^\s*import\s*)['"][^'"]+['"]\s*;?\s*$`.
     - For a re-export, if a line contains `}` without `\bfrom\b` before any end is found, it is **not** an import: `export { a }` exports locals and counts as code.
     - No end found → code.
   - **Python start:** `^\s*(?:import\s+\S|from\s+\S+\s+import\b)`. Leading indent is allowed, so imports inside `if TYPE_CHECKING:` count.
     - If the first line has `(` and no `)`, the statement runs to the first line containing `)`.
     - Otherwise it continues while a line ends with `\`.
   - An import statement's lines are joined with spaces, then whitespace is collapsed and trimmed.
   - Every non-import line that is not blank goes into `code`. **Blank lines are ignored.**
3. Sort the import statements.
4. Result: **code lines identical, in order** ∧ **sorted import lists differ**.

Consequences to reproduce:
- Pure reordering, or a pure reformat of imports (multi-line ↔ one line), gives equal sorted lists → **false**. That stays implementation.
- A blank-line-only change → false.
- Comments are code lines, so a changed comment → false.
- A trailing `// comment` on an import's last line defeats the end regex, so the statement may run on and swallow later lines. That is a known weakness of the regex approach.

**Candidates and cost** (`listing/import-only-attributes.ts`, `main/git/import-only-paths.ts`):
- A candidate is a path in the branch line total's `files` with `0 < added+removed ≤ 60`, i.e. merge base → disk, so committed and uncommitted together.
- Candidates are sorted, and the first 200 are taken.
- For each: `before = git show <mergeBase>:<path>`, `after = file on disk`.
  - Either missing (an added or deleted file) → not imports-only.
  - Either over 256,000 chars → not imports-only.
- 8 at a time, 10 s per `git show`.
- **Local worktrees only.** An SSH or remote host returns none.
- Re-asked only when a candidate's counts change: the key is a JSON of `[path, added, removed]`.
- Requires the branch compare to be `ready` (it needs `mergeBase`).
- Qualifying paths get `review-imports: set`, so they classify as `imports` ahead of every other attribute (§3.3 step 1).

### 3.6 Generic vs specific to the reference setup

| Piece | Kind | Notes |
|---|---|---|
| `review-*` / `linguist-*` attributes via `git check-attr` | generic | Linear/GitHub conventions |
| generated/test path heuristics | generic, upstream | also used by the branch line-total chip's split |
| precedence table | generic, hardcoded | |
| imports-only for TS/JS/Python | generic, but regex-based with fixed limits (60 lines, 200 files, 256 kB) | |
| **Lint baselines** regex (one tool's dot-dir, `*baseline*` dot-segments) | **specific to the reference setup's work repo** (its lint ratchet files) | FORK §65, PR 188 |
| **Areas** (two hardcoded path prefixes) | **specific to the reference setup's work repo** | `AREA_PREFIXES` in `file-category.ts` |
| Labels and the fixed category list | hardcoded | no user-defined categories |

The reference work repo's own `.gitattributes` sets only `linguist-generated=true`, on a generated API-types directory and on lockfiles (details in `private/reference-setup.md`). So in practice its categories come from:
- the path heuristics
- that one attribute
- lint baselines
- imports-only
- the areas

---

## 4. Sections of the panel

**Section ids and default order** (`shared/source-control-sections.ts`): Review `pending-review`, Pending review `pending-comments`, Agent notes `agent-notes`, Agent turns `agent-turns`, Changes `changes`, Commits `commits`.

**Layout: one scroller** (FORK §52, PR 65; `panel/stacked-sections.tsx`).
- Sections stack at natural height in the configured order. They are not resizable and their headers are not sticky.
- Commits is always last as the footer (it can be hidden, not moved).
- The Changes list virtualizes against that one scroller.

**Section visibility** — a section renders only when:
| Section | Shown when |
|---|---|
| Review | the workspace has a PR (`prNumber !== null`). It holds the reviewers, "what the diff compares against" picker, standing and verdict form. Upstream-review features; see features §2.11. |
| Pending review | ≥1 unsent review draft |
| Agent notes | ≥1 agent note |
| Agent turns | a local worktree (not SSH or remote) with ≥1 turn |
| Changes | rows exist, or a filter or scope is active |
| Commits | git history is visible and not hidden |

**Header behaviour.** Every section uses the shared `SectionHeader`: a 28 px row, chevron, uppercase label, count, actions.
- Click toggles collapse.
- Drag a header onto another to move it before or after, depending on which half it is dropped on.
- Right-click → "Hide section".
- View ▸ Source Control Sections has one checkbox per section.
- Order and hidden sections persist globally in `sourceControlSectionLayout {order, hidden, pr:{…}}`. Unknown ids are dropped; missing ids are appended in default order.

**Collapse state.** Per worktree, in memory for the session (`BoundedMap` of 512). The default collapsed set is `{history}`, i.e. Commits. The keys are `all-changes` (Changes), `pending-review`, `pending-comments`, `agent-turns`, `history`. Agent notes keeps its own expanded state, starting collapsed on each mount.

**Layout shape** (example values; the reference setup's layout is in `private/reference-setup.md`):
```json
"sourceControlSectionLayout": {
  "order":  ["changes", "pending-review", "pending-comments", "agent-notes", "agent-turns"],
  "hidden": ["commits"],
  "pr": { "order": ["pr-attention","pr-checks","pr-pending-review","pr-comments","pr-resolved"], "hidden": [] }
}
```
With this example the panel shows **Changes**, then Review (if a PR), Pending review, Agent notes and Agent turns (each when non-empty), with Commits hidden.

**Agent turns section** (`agent-turns/agent-turns-section.tsx`):
- Rows are newest first.
- Each row shows a bot icon, the first line of the prompt (or "Agent turn"), and `agent · 3m ago · N files +I −D`. The time refreshes every 60 s. The tooltip is the full prompt.
- Clicking a row sets the turn scope (§2.1).
- Turn recording itself is in features §2.7 and FORK §31. Key facts for this spec:
  - the snapshot is a temp-index `git add -A` + `write-tree`
  - the end commit's parent is the start snapshot
  - refs `refs/worktree/orca-turns/<completedAt padded 15>-<oid8>`
  - 50 kept
  - empty turns dropped
  - a resumed turn replaces its earlier ref

**Commits section** (upstream git history graph), collapsed by default and hideable. Clicking a commit or "Show in Changes" sets the commit scope. Clicking a file under an expanded commit opens that file's commit diff directly, without a scope.

**Removed from the panel** (do not rebuild):
- the commit box and Stage All (§35)
- the "New pull request" composer (§52)
- the header Create PR button (§51)
- your own Notes shelf (§32; notes live in the diff)
- View all on Changes (§46)
- the merge button in SC (see §6, PR 137 → commit 058840d)

---

## 5. Diff viewer behaviour tied to the list

### 5.1 Opening a row (`listing/open-change-entry.ts`, `use-row-opening.ts`)

**Click modes:**
- Single click → **preview** tab. It replaces the group's current preview tab.
- Double click → permanent tab.
- ⌘-click, ⇧-click or ⌥-click (Ctrl on other platforms) → opens in a **new split to the right**, not as a preview.

**Routing**, in order:
1. A turn or commit scope → a `commit` diff for that snapshot pair (`open-changes-filter-entry.ts`).
2. A conflicted file → the conflict resolver.
3. Uncommitted scope → that file's uncommitted diff, preferring unstaged → untracked → staged.
   - A staged diff is the index side.
   - An added or untracked **markdown** file opens in edit mode instead.
4. Compare not ready (no base, error) → the first local entry's uncommitted diff, if any.
5. Otherwise → a **`branch-worktree` diff**:
   - The original side is the file at the merge base, from the branch diff's base side.
   - The modified side is the file on disk, from the working-tree diff (`editor/branch-worktree-diff.ts`, `combineBaseAndWorkingTreeDiff`; binary and image flags are merged).
   - There is one tab per (worktree, path), id `branch-worktree::<path>`. Reopening refreshes its compare snapshot and requests a reload.

**Single file only.** Every route opens one file. The combined "all changes" view (upstream `CombinedDiffViewer`, with file tree, ↑/↓ 40 px scroll, and next/previous-file snapping headers, FORK §44) is reachable only in "Group by status". Turns and commits no longer open it (§46).

### 5.2 Editor and diff are one tab's two views (FORK §58, PR 181)

- The header toggle Edit | Changes (Source | Rich | Changes for markdown) is on both views.
- `editor.showEdit` and `editor.showDiff` switch views. Switching replaces the tab with the other view **at the cursor's line**, and saves unsaved edits first.
- "Changes" opens exactly what the SC row would (`openDiffAtLocation` area `branch-worktree`). It is unavailable while a turn or commit is previewed, or when the scope doesn't list the file:
  - Uncommitted scope: the file needs a local change.
  - No scope: the file needs a local change, or a ready compare. With a ready compare, even an unchanged file passes and gets an identical diff, a small leak (§8).
- **Diffs are read-only.** Editing, completions and diagnostics happen in the editor.
- Opening one view in a group closes the other view of that file in that group, unless the other is dirty or also open elsewhere.

### 5.3 Navigation

**Next/previous change** (`editor.nextChange` / `previousChange`; may be bound to bare keys such as ↓ / ↑):
- **No wrap.** At the last change it does nothing (`goToDiffWithoutWrap`, `diff-change-step.ts`). It does not cross into the next file.
- A **change taller than the viewport is paged first** (PR 165): it scrolls 85 % of the viewport per press until the far edge is visible, then jumps.
- A pure deletion's start counts as the line after the gap.

**Next/previous changed file** (`editor.nextFile` / `previousFile`):
- It steps through the **published review order**: the rows SC shows, honouring scope, filters, tree collapse and view mode (`step-to-changed-file.ts`, `use-published-review-order.ts`).
- The current position is the open tab's `<diffSource>::<path>` key. If that key isn't in the order, the first row with the same path (same side preferred) is used. A tab that isn't in the list enters at the first row (next) or the last (previous).
- A single press **wraps**. A **held** chord does not (`changed-file-hold-navigation.ts`), and keeps stepping across diff remounts.
- Opens as a preview through `openDiffAtLocation`, which uses the same routing as §5.1 (PR 101: under Uncommitted, stepping opens the uncommitted diff).
- If the panel never mounted, the fallback order is status rows and then branch rows.

**Shortcuts outside the editor** (`diff-shortcuts-outside-editor.ts`). Change, file, add-note and show-edit keys pressed while focus is outside the diff editor (for example in the SC list) focus the open diff and re-send the key to it. They act only if:
- the SC tab is showing, or the active tab is a diff; and
- focus is not in a text field; and
- for a binding **without** ⌘/Ctrl/⌥ (e.g. bare arrows), nothing holds focus (`document.body`).

With no diff open, the file keys open the first or last changed file.

**Scrolling and reveal:**
- **Centre the first change** on open (FORK §20). If the editor is under 50 px tall at first compute, it waits for `onDidLayoutChange`.
- A pending reveal (from quick open, a note, a PR comment link, Cmd+click from another diff) takes precedence, and the auto-scroll stands down.
- Cmd+click from a diff into another changed file opens **that file's diff** at the definition line (PRs 21, 29, 31).

**Retargeting** (FORK §23):
- A new base or compare snapshot moves open `branch`, `combined-branch` and `branch-worktree` tabs of that worktree onto it, keeping their tab id.
- A file that is no longer changed is left alone.

**Related surfaces that read the same scope:**
- **Quick open** (PRs 149, 150; `buildScopedChangeEntries` is shared). Changed files show the row's `+/−` and status letter and open the SC diff; `%` text matches open the diff at the match.
- **The right sidebar follows the file kind** (FORK §29): SC for diffs, Explorer for files. A worktree first opens on SC, then on the tab last used in it (PR 169, session memory).
- Diff settings that apply here: `diffDefaultView`, `diffWordWrap`, `diffShowWhitespace`, `diffCollapseUnchangedRegions`, `combinedDiffFileTreeVisibleByDefault`.

---

## 6. Changelog: how this panel got here

Read this before reusing anything older. Dates are merge dates in 2026. "→" means superseded by.

| Date | PR / commit | Change | Status now |
|---|---|---|---|
| 09-25 | #1, §2 | Inline PR comments in the diff; file-step shortcuts; comment links open the diff at the line | live |
| 10-06 | 7d1e38a, §17 | File type filter: a full-width dropdown under the commit button, over staged/unstaged/untracked/branch sections | → compact icon in the Changes header (#46), then a labelled pill (#78) |
| 10-09 | #15 | Type filter kept per worktree (`hiddenFileCategoriesByWorktree` in panel state) | → per-worktree store `file-scope-store.ts` (#149); still not persisted |
| 10-10 | #37, §31 | Agent turn snapshots; Agent turns section opens a turn in the combined commit diff | → turn opens as a Changes scope (#57) |
| 10-10 | #38, §32 | Resizable sections (react-resizable-panels), height memory in localStorage | → one scroller (#65) |
| 10-10 | #39 | Own Notes shelf removed from SC | live |
| 10-10 | #40 | Sticky section headers | → not sticky (#65) |
| 10-10 | #41, §33 | ⌥⇧⌘T opens the newest turn (in the combined diff) | → sets the turn scope (#57); toggles off (#69) |
| 10-10 | #43 | One turn per prompt across steering and wake-ups | live |
| 10-11 | #45, §35 | **One Changes list** vs base; `branch-worktree` diff (then *editable*, autosaved); Agent turns docked at the bottom with Commits | list live; editable → read-only (#181); docking → one scroller (#65) |
| 10-11 | #46, §36 | Reorder/hide sections, `sourceControlSectionLayout`; commit box removed; Review as one section | live (Commits no longer docked) |
| 10-12 | #48 | Review opens at full height | moot after #65 |
| 10-12 | #49, §38 | ⌥⌘U Uncommitted only, "rows still read against base" | rows → uncommitted-relative (#100) |
| 10-12 | #54, §43 | Pending review section split out of Review | live |
| 10-12 | #55, §44 | Combined diff: ↑/↓ scroll, file step snaps headers | live, but only in status grouping |
| 10-12 | #57, §46 | Turns/commits/blame as a **Changes scope**; View all removed; latest turn shown *live* (start → disk) | live except: latest is frozen (#71) |
| 10-12 | #65, §52 | One scroller; Commits last section; PR composer removed | live |
| 10-12 | #67/#68, §54 | ⇧⌥⌘R focuses the review summary; Esc releases it | live |
| 10-12 | #69 | Latest-turn shortcut toggles off | live |
| 10-12 | #70, §55 | Header line total; exact per-file counts from the ranged diff | live |
| 10-12 | #71 | Latest turn = frozen snapshot diff | live |
| 10-13 | #75, #77, #78 | Spacing; shortcuts reveal SC at once; active type filter as a labelled pill | live |
| 10-13 | #82 | Open diff follows the scope; Uncommitted rows open the uncommitted diff | live |
| 10-14 | #100 | Uncommitted rows show the uncommitted status and counts | live |
| 10-15 | #101 | File stepping under Uncommitted opens the uncommitted diff | live |
| 10-21 | #137, #140 | SC primary button merges the PR or toggles auto-merge; no merge with uncommitted changes | **→ moved to the top-bar next-action button (058840d, 10-22)**; SC's commit area isn't rendered. features.md §2.9 "Primary merge button" is stale on location. |
| 10-23 | #149, #150 | Quick open shares the scoped rows and opens the SC diff | live |
| 10-27 | #165 | Change step pages through tall changes | live |
| 10-28 | #180 | F4 opens the diffed file's tab | → ⌥⌘E/⌥⌘D views (#181) |
| 10-28 | #181, §58 | Editor/diff as one tab's two views; diffs read-only | live |
| 10-29 | #186, §63 | Discard from Changes rows | live |
| 10-29 | #188, §65 | Lint baselines type; Backend/Frontend areas | live, specific to the reference setup |
| 10-29 | #189, §66 | Imports only | live |

---

## 7. Generalization: a configurable filter model

Goal: anyone can define their own file types, areas and views. Today's fork behaviour ships as a preset that is identical.

### 7.1 Concepts

- **Scope**: where rows come from and what they are measured against. One at a time:
  - `base` — the default: merge base → disk
  - `uncommitted`
  - `turn(<id>|latest)`
  - `commit(<sha>)`
  - optionally `range(<a>..<b>)`, `since_review`
- **Facet**: a named dimension that puts each file into **at most one value**, by an ordered list of rules (first match wins), with an optional default.
  - Today there are two: `type` (exclusive, default `implementation`) and `area` (no default, so a file can have none).
  - Users can add more, e.g. `package`, `layer`, `owner`.
- **Rule**: a `when` predicate and a `value`. Predicates (all evaluated on the server):
  - `path`: gitignore-style globs, or `regex`; any-of.
  - `attr`: `{name, state: set|unset|value=<v>}` via `git check-attr`. The attribute names to query are the union of attributes referenced by rules.
  - `change`: kind (`added|modified|deleted|renamed|copied`), `binary`, `lines` range (`added+removed` against the scope's base), `sources` (committed/staged/unstaged/untracked).
  - `content`: a named content analyzer.
    - `imports_only` is built in, implemented with **tree-sitter queries per language**. Each language declares its import and re-export node kinds, and analyzers can be added by config.
    - Analyzer limits are explicit config: `max_changed_lines`, `max_files`, `max_bytes`.
  - Combinators: `all`, `any`, `not`.
  - `builtin`: named predicate libraries such as `generated_heuristic`, `test_heuristic`, `linguist_generated`, so presets don't paste regexes.
- **Visibility**: per workspace, per facet, a set of **hidden values**, the same semantics as today:
  - a row is shown iff, for every facet, its value is not hidden
  - a file with no value in a facet is never hidden by that facet
- **View**: a named, saved combination of `scope`, hidden values per facet and `text`. A view can be applied, toggled (apply, or revert to the previous state) or used as a CLI `--filter`.

Where config lives (plan.md's schema-driven settings):
- global `changes.facets` / `changes.views`
- project-level overrides in `<repo>/.icarus/changes.toml`, which can be committed so a team shares them
- per-workspace runtime state (scope, hidden set, text) on the server, so CLI and UI agree
- *persist it across restarts*: that is new, but the server owns it now

### 7.2 Exact preset: "orca-fork" (reproduces today)

```toml
# Facet order = menu group order; value order = menu order (also the "present" order).
[[changes.facets]]
id = "type"
label = "File types"
exclusive = true
default = "implementation"
values = [
  { id = "implementation", label = "Implementation" },
  { id = "imports",        label = "Imports only" },
  { id = "test",           label = "Tests" },
  { id = "documentation",  label = "Documentation" },
  { id = "generated",      label = "Generated" },
  { id = "lint-baseline",  label = "Lint baselines" },   # a project overlay may own this one
  { id = "agent-guidance", label = "Agent guidance" },
  { id = "localization",   label = "Localization" },
  { id = "assets",         label = "Assets" },
]
# First match wins. Rules 1–10 are §3.3 step 1; 11–15 reproduce steps 2–3 exactly.
rules = [
  { value = "imports",        when = { any = [ { attr = { name = "review-imports", state = "set" } },
                                               { content = "imports_only" } ] } },
  { value = "generated",      when = { attr = { name = "review-generated",       state = "set" } } },
  { value = "test",           when = { attr = { name = "review-test",            state = "set" } } },
  { value = "documentation",  when = { attr = { name = "review-documentation",   state = "set" } } },
  { value = "agent-guidance", when = { attr = { name = "review-agent-guidance",  state = "set" } } },
  { value = "localization",   when = { attr = { name = "review-localization",    state = "set" } } },
  { value = "assets",         when = { attr = { name = "review-assets",          state = "set" } } },
  { value = "implementation", when = { attr = { name = "review-implementation",  state = "set" } } },
  { value = "generated",      when = { attr = { name = "linguist-generated",     state = "set" } } },
  { value = "documentation",  when = { attr = { name = "linguist-documentation", state = "set" } } },
  { value = "lint-baseline",  when = { regex = '(?i)(?:^|/)(?:\.<linttool>|\.[^/]*baseline[^/]*)(?:/|$)' } },
  { value = "implementation", when = { all = [ { builtin = "generated_heuristic" },
                                               { any = [ { attr = { name = "review-generated",   state = "unset" } },
                                                         { attr = { name = "linguist-generated", state = "unset" } } ] } ] } },
  { value = "generated",      when = { builtin = "generated_heuristic" } },
  { value = "implementation", when = { all = [ { builtin = "test_heuristic" },
                                               { attr = { name = "review-test", state = "unset" } } ] } },
  { value = "test",           when = { builtin = "test_heuristic" } },
]

[changes.analyzers.imports_only]
languages = { script = ["ts","tsx","mts","cts","js","jsx","mjs","cjs"], python = ["py","pyi"] }
compare = "merge_base_to_disk"      # always vs the branch base, whatever the scope
max_changed_lines = 60              # candidates: 0 < added+removed <= 60
max_files = 200                     # first 200 by path
max_bytes = 256000                  # either side over → not imports-only (chars today)
remote_hosts = false                # today: local worktrees only
semantics = "orca-v1"               # see §3.5: code lines equal in order, sorted import statements differ,
                                    # blank lines ignored, reorder/reformat-only is NOT imports-only

[[changes.facets]]
id = "area"
label = "Areas"
exclusive = true
# no default: files outside every area have no value and are never hidden by this facet
values = [ { id = "backend", label = "Backend" }, { id = "frontend", label = "Frontend" } ]
rules  = [ { value = "backend",  when = { path = ["server/**"] } },   # example prefixes; the reference
           { value = "frontend", when = { path = ["web/**"] } } ]      # setup's are in private/reference-setup.md
```

`generated_heuristic` and `test_heuristic` must be the §3.4 regexes verbatim.

Placement: the `area` facet and the `lint-baseline` rule are specific to one work repo. They belong in a **project overlay keyed by the remote canonical key** (e.g. `github.com/acme/widgets`, as settings export already keys projects), not in the global preset. The reference setup's overlay values are in `private/reference-setup.md`. Every other project then gets the generic set, matching what it sees today minus two inert entries.

Built-in views to reproduce today's toggles:

```toml
[[changes.views]]  id = "uncommitted"   scope = "uncommitted"   toggle = true   badge = "Uncommitted"
[[changes.views]]  id = "latest-turn"   scope = "turn:latest"   toggle = true   badge = "Latest turn"  # "Historical changes" once superseded
```

The keymap maps `sourceControl.toggleUncommittedOnly` → `changes.view.toggle:uncommitted` and `sourceControl.openLatestAgentTurn` → `changes.view.toggle:latest-turn`. Views carry only a scope, so hidden types survive scope changes, as today.

### 7.3 UI composition

**Header chips, left to right:**
1. **Scope chip.** Today's badge (Uncommitted / Latest turn / Historical changes / Commit abc1234) with ×.
2. **One chip per facet that hides something,** with today's "what is still shown" text (`Implementation`, `No areas`). Clicking it opens that facet's checkbox menu, listing only present values. Present values are computed over the default scope's rows, as today.
3. **Text chip** when a query is set.

Then the count and the `+A −R` total of the listed rows.

**The filter icon** opens one menu with a group per facet ("Show file types", "Show areas", …), plus "Views" (apply/save current…). "Edit filters…" opens the config file or a settings page.

**Keybindable actions** (all on the CLI too):
- `changes.scope.set:<scope>`, `changes.scope.clear`
- `changes.view.apply:<id>`, `changes.view.toggle:<id>`
- `changes.facet.toggle:<facet>.<value>`, `changes.facet.show_only:<facet>.<value>`, `changes.facet.reset[:<facet>]`
- `changes.text.focus`

Every one reveals the SC panel when it fires, as today.

### 7.4 CLI

```
icarus changes list [--workspace W] [--scope base|uncommitted|turn:<id>|turn:latest|commit:<sha>]
                   [--view <id>] [--hide type=generated,test] [--hide area=backend] [--only type=implementation]
                   [--text <q>] [--json]
  → rows: {path, oldPath?, status, added?, removed?, binary, sources:{committed,staged,unstaged,untracked},
           facets:{type:"test", area:"frontend"|null}, discardable}
    plus {scope, base:{ref, mergeBase, aheadBy, behindBy, status}, totals:{added, removed, files}}
icarus changes classify <path>... [--explain] [--json]   # the facet values and the rule that matched, for debugging configs
icarus changes facets list|show <id>        icarus changes views list|apply|toggle|save|rm
icarus changes scope get|set|clear          icarus changes filter get|set|reset   # drives the connected client's panel
icarus turns list|show <id> [--json]        icarus changes open <path> [--line N]  # opens the row's diff in the client
```

These commands do not exist in Orca today; they are a proposal. The same server queries back the UI, quick open and the CLI, so all three agree (today quick open and the panel share `buildScopedChangeEntries` for this reason).

### 7.5 Rewrite notes

- The server computes rows, classification and analyzers. The client renders and keeps only view state.
- Implement `imports_only` with tree-sitter (Zed already ships TS/JS/Python grammars). Keep the `orca-v1` semantics switch, so the preset matches the regex version on reorder-only, reformat-only and blank-only changes. That includes not counting pure reorders as imports-only, which is arguably a bug; see §8.
- Make the git-attribute names to query data-driven: the union of `attr` predicates.
- Keep the "present values only" menus and the "hidden-but-absent is not active" rule.

---

## 8. Quirks and decisions to confirm

1. **The type/area and text filters are ignored under a turn or commit scope**, although the pill still shows them as active (`scoped-change-entries.ts`). The proposal applies facets in every scope. Confirm that this departure is wanted.
   - **Decided (2026-11-15): facets and text apply in every scope** (`icarus_git::filters::list`). "Present" values are computed over the base rows plus the scope's rows.
2. Scope, hidden types and the text query are **lost on restart**. They live in memory per worktree. The proposal persists scope and hidden values per workspace on the server.
   - **Decided (2026-11-15): hidden values persist** per workspace in the server's SQLite `kv` table (`changes.hidden.<workspace id>`), set with `workspace.changes.filter` / `icarus changes filter`. Scope and text stay per request; a `hide` on `workspace.changes` overrides the saved set for that request only.
3. **Imports-only** treats a pure reorder or reformat of imports as *not* imports-only, and is fooled by trailing comments on import lines. Possible fixes: (a) keep `orca-v1` for exactness; (b) a `v2` that also counts reorder/reformat-only changes.
   - **Decided (2026-11-15): both ship, `orca-v1` is the default** so the preset reproduces Orca exactly; `changes.analyzers.imports_only.semantics: "v2"` opts in to counting reorder/reformat-only changes. The analyzer is the §3.5 regex port, not tree-sitter yet, so the trailing-comment weakness remains.
4. `showDiff` (⌥⌘D) on an unchanged file opens an empty base→disk diff when the compare is ready and no scope is set.
5. The latest-turn shortcut lists turns on the client; turn events are lost while the app is closed (features §4.5). The server-owned recorder fixes this.
6. "Group by status", staging and the combined View all are reachable only in `'status'` grouping, which the reference setup doesn't use. Suggest cutting them from v1, but keep the combined-diff keyboard rules (§5.3) if it ships.
7. The SC merge button (PRs 137/140) **no longer exists** in SC. Merging lives on the top-bar next-action button. features.md §2.9 should point there.
