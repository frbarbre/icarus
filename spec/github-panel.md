# GitHub panel: stacks, merge queues, and every GitHub status

The spec for the GitHub side of the Rust/GPUI rewrite. It covers the PR panel, the stack map, Group by Stack, merge queues, the status matrix, polling, rules-engine facts and the CLI.

Read with `features.md` §2.2, §2.11–2.13 and §3 (status rules), and `plan.md` §3 (rules engine). Those are not repeated here.

Sources:
- The fork at `~/Developer/orca` (frbarbre/orca): FORK.md §66–§81, and PRs 166, 190–197, 205, 207, 210, 212 and 214.
- GitHub docs, read 2026-11-15: stacked PRs are in **public preview**, REST API version `2026-03-10`.

Paths are relative to `src/` unless they say otherwise.

---

## Part A. Stacks and the merge queue, as the fork built them

### A.1 What a "stack" is here

A stack is **GitHub's native Stacked Pull Requests feature**. It is not a chain Orca works out from base branches.
- A stack is a server-side object with a `number` that is unique per repo.
- It holds an ordered list of PRs in the same repo. Position 1 is the bottom PR, which targets the trunk (`base.ref`, usually `main`). Each PR above it targets the head branch of the PR below.
- `PRInfo.stack` is "GitHub-registered stack metadata. **Absent for ordinary dependent PR chains.**" (`shared/github/pull-request-types.ts`).
- A chain of PRs whose bases point at each other, but which was never registered as a stack, gets no stack UI at all.

GitHub's rules that matter, from "About stacked PRs" and the stacks API reference:
- Every branch is in the same repo. Cross-fork stacks are not supported.
- PRs merge bottom-up. Merging PR *k* merges every open PR below it, atomically. The PRs above stay open and retarget to the stack base.
- Merge requirements (branch protection, rulesets, CODEOWNERS, required checks) come from the **bottom PR's base**, and apply to every PR in the stack.
- A stack **must** be merged with the async merge endpoint. The legacy synchronous merge endpoints and mutations are rejected.
- A repo where stacks are not enabled returns `404` from the stacks API and from `merge-async`.
- The GraphQL schema is read-only for stacks. All changes go through REST.

**Upstream versus fork.** Upstream Orca (stablyai) owns:
- stack detection, the stack-details query and `PRInfo.stack`;
- the basic stack map: a collapsible list that opens PRs;
- stacked PR creation;
- the merge-async merge path.

The fork added:
- the status badges (§69, PR 195);
- Merge stack (PR 196);
- Rebase stack (PR 197);
- merge queue badges and state (§76/§78, PRs 205 and 207);
- the stack position on cards and click-through to a worktree (PR 166);
- Group by Stack, the Reviewing lane and the Asc/Desc toggle (§67, PRs 190, 191, 194 and 210).

**False friend:** `right-sidebar/source-control/panel/stacked-sections.tsx` has nothing to do with PR stacks. It is §52, "Source Control as one scroll".

### A.2 Detection, data and caches

#### Data model (`shared/github/pull-request-types.ts`)

```
GitHubPRStack      = { number, position, size, baseRefName, baseSha?, baseTipSha?, entries?: GitHubPRStackEntry[] }
GitHubPRStackEntry = { position, number, title, url, updatedAt?, state: open|draft|closed|merged,
                       checksStatus: pending|success|failure|neutral, mergeable: MERGEABLE|CONFLICTING|UNKNOWN,
                       reviewDecision?: APPROVED|CHANGES_REQUESTED|REVIEW_REQUIRED|null,
                       mergeStateStatus?: string|null, inMergeQueue?: true, headRefName?, headSha? }
PRInfo += stack?, mergeQueueRequired?, inMergeQueue?, mergeStateStatus?, autoMergeEnabled?, autoMergeAllowed?, mergeMethodSettings?
```

#### Pipeline (main process, on every PR refresh of a workspace's PR)

1. **The base PR lookup.**
   - It runs `gh pr view N --json number,title,state,url,statusCheckRollup,updatedAt,isDraft,mergeable,reviewDecision,mergeStateStatus,autoMergeRequest,baseRefName,headRefName,baseRefOid,headRefOid,headRepositoryOwner,headRepository,additions,deletions`.
   - Branch discovery uses `GET /repos/o/r/pulls?head=o:branch&state=all&per_page=1` instead.
   - `gh pr view` has **no stack field and no merge queue field**.
2. **The stack summary** (`client/lookup/pr-stack-summary-cache.ts`).
   - This runs when the PR was found by number (linked or fallback number) and stack metadata has not been checked yet.
   - The call is `GET /repos/o/r/pulls/N`, sent through `github-rest-revalidation.ts`. That keeps the ETag and sends `If-None-Match`, so an unchanged PR costs nothing, because a 304 is free.
   - The REST PR carries `stack: {id, number, position, size, base: {ref, sha}}`. The REST list endpoint carries the same shape.
   - The cache key is `scope \0 repoKey # N`. Successes are cached for 60 s, failures for 5 s (a failure is never cached as "no stack"), up to 512 entries, and in-flight requests are deduplicated.
   - Stack metadata only adds information. A failed probe leaves the PR usable without it.
   - **Validation** (`rest-stack-metadata-validation.ts`):
     - `number`, `position` and `size` are positive safe integers, and `position <= size`;
     - `base.ref` is a non-empty string;
     - `base.sha` is absent or 40/64 hex.
     - Merge (A.6) *requires* valid metadata. Malformed non-null metadata, or a missing head SHA, aborts the merge with "Could not verify GitHub pull request stack metadata. Refresh and try again."
3. **Stack details** (`github/github-pr-stack.ts`, `hydrateGitHubPRStack`). One GraphQL call per stack:
   ```graphql
   query($owner:String!,$repo:String!,$pr:Int!,$baseRef:String!) {
     repository(owner:$owner,name:$repo) {
       baseTip: ref(qualifiedName:$baseRef) { target { oid } }      # fork: out-of-date detection
       pullRequest(number:$pr) { stack { number size baseRefName
         entries(first:100) { nodes { position pullRequest {
           number title url updatedAt state isDraft headRefName headRefOid
           mergeable reviewDecision mergeStateStatus isInMergeQueue      # isInMergeQueue: fork (§76)
           statusCheckRollup { state } } } } } }
     }
   }
   ```
   - `baseRef` is `refs/heads/<summary.baseRefName>`.
   - The cache key is `scope \0 repoKey : stackNumber`, cached for 30 s, up to 256 entries, with in-flight requests deduplicated. The cache is skipped when this PR's `updatedAt` differs from the cached entry's `updatedAt`.
   - A failure, or a stack number that doesn't match, gives the bare summary with no `entries`. The map then says "Stack details are temporarily unavailable."
   - The rate-limit guard is `graphql`, with 1 point noted per call.
   - Entry mapping:
     - `state`: MERGED → merged, CLOSED → closed, otherwise `isDraft ? draft : open`.
     - `checksStatus` from rollup `state`: SUCCESS → success; FAILURE/ERROR → failure; PENDING/EXPECTED → pending; else neutral.
     - `mergeable`: MERGEABLE/CONFLICTING, else UNKNOWN.
     - `reviewDecision`: an enum value or null.
     - `inMergeQueue` is only set when it is true.
     - Entries are sorted by `position` ascending.
   - `position` and `baseSha` always come from the per-PR summary, never from the details cache.
4. **Merge metadata for the stack base** (`client/detect/repository-merge-metadata.ts`):
   ```graphql
   repository { viewerDefaultMergeMethod mergeCommitAllowed rebaseMergeAllowed squashMergeAllowed autoMergeAllowed
     mergeQueue(branch:$branch){ id }  ref(qualifiedName:"refs/heads/$branch"){ rules(first:50){ nodes{ type } } } }
   ```
   - `mergeQueueRequired = mergeQueue != null || some rule.type == MERGE_QUEUE`.
   - Cached for 10 min, or 1 min while unknown. A failure gives `null`, which means unknown, not false.
   - For a stacked PR, `PRInfo.mergeQueueRequired` comes from the **stack base**, not from the PR's direct base.
5. **Merge queue membership** (`pr-merge-queue-membership.ts`, fork §78):
   - When the PR is open and `mergeQueueRequired === true`, Orca makes one more query: `repository{pullRequest(number){isInMergeQueue}}`. A failure gives undefined.
   - Every stack entry already carries `isInMergeQueue` from step 3.
6. **The status-rules snapshot** (`review-status-snapshot-query.ts`) also fetches `isInMergeQueue` for every linked PR, for the `merge-queue` condition.

#### Refresh cadence for `PRInfo`

This comes from `shared/review-refresh-policy.ts`:
- **Selected, open:** 60 s.
- **Visible, open:** 120 s.
- **Merged:** 60 s while checks are pending, then never.
- **Closed:** 15 min.
- **No PR:** 60 s when selected, 15 min otherwise.
- **Cooldown:** 10 s.
- **Post-push:** a 2.5 s delay.
- **Mergeability pending (`UNKNOWN`):** retried at 2.5 s after a manual refresh.
- **Refresh reasons:** `visible | active | post-push | manual | swr`.
- **After a mutation:** a successful merge or similar calls `invalidateReviewLookupsAfterPRMutation`.

### A.3 The stack map (Checks panel), screenshot in words

**Where it sits.** In the Checks panel, under the review header (`#123` and the state pill), above the editable title. It shows only when `activeReview.provider === 'github' && pr.stack` (`checks-panel/active-content.tsx` → `checks-panel/github-pr-stack-section.tsx` → `right-sidebar/GitHubPRStackMap.tsx`).

**Frame.** One rounded box with a 1px border, using `Collapsible`. It is **collapsed by default**, and the open state is local and not persisted.

```
┌───────────────────────────────────────────────────────────────┐
│ ⎇ Stack #42   2 of 3 · main                                 ▾ │   header button (always visible)
├───────────────────────────────────────────────────────────────┤
│ #812  Add billing export UI                       ( Review needed ) │   position 3 (top) first
│ #811  Billing export API                          ( Ready )         │   ← current PR: row bg = accent
│ #810  Billing schema migration                    ( Merged )        │   position 1
│ ⑂ main                                                            │   trunk row, muted
├───────────────────────────────────────────────────────────────┤
│ ⚠ This stack is out of date with main             [Rebase stack]  │   only when out of date (A.7)
├───────────────────────────────────────────────────────────────┤
│ [Merge stack · 2 PRs]                                             │   only when mergeable (A.6)
└───────────────────────────────────────────────────────────────┘
```

**Header row** (a button that toggles the collapse):
- a `GitPullRequest` icon at 14px, muted;
- **"Stack #N"** in semibold;
- muted "**{position} of {size} · {baseRefName}**", which is *this* PR's position;
- a chevron, right-aligned, rotated 180° when open;
- aria-label "Expand stack #N" or "Collapse stack #N".

**Entry rows** (expanded):
- Sorted by **position descending**, top of the stack first and the trunk last, like GitHub's merge box.
- Each row is a button at 11px: `#number` in a fixed 32px column, the title (truncated, flex), and the status badge (A.4).
- The current PR's row has an `accent` background (`data-current`).
- **Click** opens the stack entry (`github-pr-stack-entry-worktree.ts`):
  - with no modifier, it activates the worktree that has the PR: first the one with `worktree.linkedPR === number`, then the one whose branch name equals the entry's `headRefName`;
  - otherwise it opens `entry.url`;
  - Cmd, Ctrl or Shift always open the URL. The modifier routing (Orca browser or system browser) is the panel's usual hint.
- After the entries comes a trunk row: a `GitBranch` icon and `baseRefName`, muted.
- With no `entries`, the box shows one muted line: **"Stack details are temporarily unavailable."**

**Out-of-date bar** and **merge bar**: these sit below the collapsible content, so they are **visible even when collapsed**. See A.6 and A.7.

### A.4 Badge mapping (`right-sidebar/github-pr-stack-entry-status.ts`)

The rule is "Readiness follows GitHub's merge state, which only weighs required checks; the rollup counts every check."

The first rule that matches wins. `READY = {CLEAN, HAS_HOOKS, UNSTABLE}`.

| # | Condition on the entry | Label | Tone |
|---|---|---|---|
| 1 | `state == merged` | Merged | merged (violet) |
| 2 | `state == closed` | Closed | neutral (muted) |
| 3 | `state == draft` | Draft | neutral |
| 4 | `inMergeQueue` | Queued | queued (orange) |
| 5 | `mergeStateStatus ∈ READY` | Ready | ready (emerald) |
| 6 | `mergeable == CONFLICTING` or `mergeStateStatus == DIRTY` | Conflicts | blocked (rose) |
| 7 | `checksStatus == failure` (rollup) | Checks failed | blocked |
| 8 | `reviewDecision == CHANGES_REQUESTED` | Changes requested | blocked |
| 9 | `reviewDecision == REVIEW_REQUIRED` | Review needed | pending (amber) |
| 10 | `checksStatus == pending` | Checks pending | pending |
| 11 | `mergeStateStatus == BEHIND` | Behind | pending |
| 12 | `mergeStateStatus == BLOCKED` | Not ready | pending |
| 13 | `reviewDecision == APPROVED` | Approved | ready |
| 14 | otherwise (UNKNOWN, null) | Open | neutral |

**Badge style.** A rounded-full pill: 1px border, `px-1.5`, 10px medium text, 16px line height.

Tones (border / background / text):

| Tone | Light | Dark |
|---|---|---|
| ready | emerald-500/30 / emerald-500/15 / emerald-600 | emerald-400 |
| queued | orange | orange |
| pending | amber | amber |
| blocked | rose | rose |
| merged | violet | violet |
| neutral | `border-border bg-muted text-muted-foreground` | same |

`data-stack-pr-status=<tone>` is the test hook.

**Consequence.** A PR with failing *non-required* checks and `UNSTABLE` reads **Ready**. A PR that is `BLOCKED` only because a required check is pending reads **Checks pending**, through rule 10, before "Not ready".

### A.5 Stack position on the workspace card (PR 166)

- Card display has a "**Stack**" option (`stack`, off by default, listed after the issue properties in `fork-card-property-options.ts`).
- A card whose PR has `stack` shows, in the **title row**, a `Layers` icon at 12px plus **`{position}/{size}`** in 10px tabular muted text, for example `2/3`.
- The tooltip, on the right, reads "**Stack #N, P of S**".
- It sits before the PR-diff chip (§80) and the PR author avatar. Whichever of them comes first takes `ml-auto`.

### A.6 Merge stack

#### Button (`getGitHubPRStackMergeTarget`, fork PR 196)

The button shows from **any** PR in the stack when all of the following hold:
- `entries.length == stack.size`, so the details are complete;
- there is at least one entry with `state != merged`;
- **every** unmerged entry has:
  - `state == open` (a draft or closed entry hides the button),
  - `inMergeQueue != true` (while any PR is queued the button is hidden, so nothing is queued twice, §76),
  - `mergeStateStatus ∈ {CLEAN, HAS_HOOKS, UNSTABLE}`.

**Target:** the unmerged entry with the highest position. **Count:** the number of unmerged entries.

**Look:** a primary `xs` button in its own bordered bar, labelled **"Merge stack · 1 PR"** or **"Merge stack · N PRs"**. It is disabled while a merge is running (`mergingStack`).

#### Flow (`checks-panel/use-merge-github-pr-stack.ts`)

1. **Method:** `resolveGitHubPRMergeMethods(pr.mergeMethodSettings).defaultMethod`. That is the viewer's default, constrained to the allowed methods.
2. **Confirmation dialog** (`github-pr-stack-confirmation.ts`), built with the stack's position set to the target's position:
   - "Included: #a, #b, #c. " is added to the front only when entries exist for positions 1…target with none missing.
   - **Merge queue** (`pr.mergeQueueRequired === true`):
     - Title: "Queue through #T?"
     - Body: "{Included}GitHub will add N pull request(s) to the merge queue together. The queue chooses the merge method and may merge them in separate groups."
     - Confirm button: "Queue N PR(s)".
   - **Direct:**
     - Title: "Merge through #T?"
     - Body: "{Included}GitHub will merge N pull request(s) atomically using {method}. If any cannot merge, none will."
     - Confirm button: "Merge N PR(s)".
   - Here N is the *scope count*, which equals the target's position. It includes already-merged entries below, so it can differ from the button's count (Risk R3).
3. The renderer calls `mergeGitHubHostedReview({repo, prNumber: T, method})`, which reaches main `client/merge/merge-pr.ts` `mergePR`. The main side:
   1. `GET /repos/o/r/pulls/T` with `requireUsableStackMetadata`. If it fails, it returns `STACK_METADATA_UNAVAILABLE_ERROR`.
   2. If the PR has `stack`, it runs `detectRepositoryMergeMetadata` on the stack base.
   3. It releases the gh concurrency slot and calls `mergeGitHubPRStack` (`github-pr-stack-async-merge.ts`):
      ```
      PUT /repos/o/r/pulls/T/merge-async
          merge_action = merge_queue | direct_merge        (merge_queue iff mergeQueueRequired === true)
          merge_method = <method>                          (direct_merge only)
          sha          = <REST head.sha of T>              (merge is cancelled by GitHub if T is pushed meanwhile)
      env GH_PROMPT_DISABLED=1
      ```
   4. It handles the response. On a non-2xx, `gh api` exits non-zero but prints the body, which is parsed from the error's stdout. That covers 409 "already pending", which returns the pending request's uuid.
      - `status: merged | enqueued` → ok.
      - `status: pending` with `details.uuid` → poll.
      - anything else → fail with `details.message`, or "GitHub could not merge this stack."
   5. **Poll:** `GET …/merge-async/{uuid}` every **1 s**, at most **180** times (3 min). A transient poll error is ignored ("the merge was submitted; a transient poll failure cannot make it fail"). On timeout: "GitHub is still merging this stack. Refresh to check its status."
   6. On ok: `invalidateReviewLookupsAfterPRMutation`.
   7. A PR **without** a stack goes through the legacy path:
      - `getPRMergeBlocker` refuses REVIEW_REQUIRED, CHANGES_REQUESTED and mergeQueueRequired ("Use Merge when ready instead"), and reports conflicts, listing the files from local git;
      - then it runs `gh pr merge N --<method>`.
4. **In the renderer:** an error gives `toast.error(message)`. Success calls `onMerged` (`refreshHostedReviewAfterMutation`).

#### Other merge paths for a stacked PR

- **The top-bar next action** (`workspace-actions/workspace-action-context.ts`) returns **no merge action for a stacked PR** ("Stacks are left to GitHub"). The only merge path for a stacked PR in the fork is the stack map's Merge stack.
- **The PR panel** has no merge buttons in the fork (§13). Upstream's "Merge through #N · N PRs" label (`github-pr-stack-merge.ts`) and `resolveGitHubPRStackMergeDecision` (`shared/github/pull-request-stack-merge.ts`) are still in the tree, but no fork UI uses them. Their blocker reasons, checked bottom-up up to the target, are the precise preconditions for a partial "merge through #k". Port them for the CLI:
  - draft → "#N is still a draft.";
  - closed → "#N is closed.";
  - conflicts (CONFLICTING or DIRTY) → "#N has merge conflicts.";
  - CHANGES_REQUESTED → "#N has requested changes.";
  - REVIEW_REQUIRED → "#N still needs review approval.";
  - BEHIND → "#N must be updated.";
  - BLOCKED → "#N is blocked.".
- **Auto-merge** (`client/merge/pr-auto-merge.ts`) refuses stacked PRs: "GitHub does not support auto-merge for stacked pull requests." **Do not copy this.** Stack auto-merge is required in the rewrite; see R4.

### A.7 Rebase stack (fork PR 197)

- **Out of date** means `stack.baseSha && stack.baseTipSha && baseSha != baseTipSha`.
  - `baseSha` is the REST `stack.base.sha`, the commit the stack is based on.
  - `baseTipSha` is `ref(refs/heads/<base>).target.oid`, from the same details query.
- **Bar:** shown only when the panel's AI source-control actions are visible.
  - An `AlertTriangle` in amber-500, then "**This stack is out of date with {base}**" (truncated), then an outline `xs` button **"Rebase stack"**.
  - It sits below the collapsible, so it shows while collapsed.
- **Action** (`checks-panel/use-checks-panel-ai-queue.tsx`): `openOrStart({actionId: 'resolveConflicts', title: 'Rebase Stack With AI', description: 'Review and edit the full command input before starting an agent.', prompt, launchSource: 'conflict_resolution'})`.
  - The user sees the editable prompt in the agent-launch dialog first.
  - The agent is the `resolveConflicts` recipe agent.
- **Prompt** (`github-pr-stack-rebase-prompt.ts`, verbatim structure):
  ```
  Stack #N is out of date with `base`. Rebase the whole stack onto the latest `origin/base`.

  Follow the /<stack-skill> skill: invoke it now and use its current instructions for rebasing a stack.

  Open PRs in the stack, from the one closest to `base` up:
  - #810 `branch-a`            (merged and closed entries are omitted; ascending position; 'unknown branch' if no headRefName)
  ...

  - Fetch first, then start with the PR closest to `base` and work up: rebase it onto `origin/base`, then each PR above onto the one below it.
  - Resolve every conflict locally as you go.
  - Force-pushing is allowed. Push nothing until every branch is rebased, then push them all at once with `--force-with-lease`.
  - Do not run validation, tests, lint or typecheck; CI runs them after the push.
  ```
  `/<stack-skill>` stands for a personal stack-rebase skill that the fork hardcodes (specific to the reference setup; the name is in private/reference-setup.md, not in the repo). In the rewrite it becomes a template variable or preset content (plan.md §3).

### A.8 Merge queue UI (fork §76, §78; PRs 205 and 207)

| Surface | When | Shows |
|---|---|---|
| Review header state pill (`checks-panel/review-header.tsx`) | `pr.inMergeQueue` | uppercase 9px pill **"queued"**, orange-500 text on orange-500/15 with an orange-500/20 border, instead of OPEN |
| Merge state (`components/github-pr-merge-state.ts`) | `inMergeQueue` | **"In merge queue"**, orange tone, tooltip "GitHub's merge queue holds this pull request and merges it once its checks pass"; no direct merge, and no auto-merge control (`canShowGitHubPRAutoMergeControl` is false) |
| Merge state | base requires a queue, not queued | "Merge when ready" (amber, tooltip "This base branch uses GitHub merge queue"), or "Auto-merge on" if auto-merge is enabled; the auto-merge action is "Merge when ready" ("Add this pull request to the GitHub merge queue") |
| Stack map | entry `isInMergeQueue` | "Queued" orange badge; Merge stack hidden |
| Status rules | `isInMergeQueue` and `merge-queue` mapped | condition **merge-queue** ("In the merge queue"), checked right after *reviewing*, before *conflicts* |

**Enqueue path** (`pr-auto-merge.ts`, for non-stacked PRs):
- If the base requires a queue and `autoMergeAllowed === false`, Orca calls the GraphQL `enqueuePullRequest(pullRequestId, expectedHeadOid)`.
- Otherwise it runs `gh pr merge --auto --<method>`, which GitHub turns into enqueue-when-ready.
- "In clean status" errors become "This pull request can already be merged. Use Merge instead of auto-merge."

**Not handled:**
- `mergeQueueEntry` (state, position, ETA, jump, solo);
- dequeue, and removal reasons;
- the queue's configuration (Part B).

### A.9 Group by Stack (fork §67; PRs 190, 191, 194 and 210)

- **Mode:** Workspace options → Group by → **Stack**, the fifth mode after none, workspace-status, repo and pr-status (`WorktreeGroupBy`, `shared/sidebar-list/row-types.ts`).
- **Data:**
  - Stack mode loads PRs the way PR grouping does (`use-visible-review-refresh.ts`, `review-cache-inputs.ts`).
  - The lane reader `createSidebarStackReader` (`worktree-list/grouping/stack-key.ts`) reads `{number, position}` from the PR cache for the worktree.
  - Lineage (parent/child) nesting is **off** in this mode, so parent rows cannot reorder a stack.
- **Lane keys** (`shared/sidebar-list/stack-groups.ts`): `stack:<repoId>:<number>`, `stack:reviewing` and `stack:none`.
- **Lane order** (`orderStackGroups`):
  1. **Reviewing** (PR 191). These are worktrees whose raw `workspaceStatus` equals the status the rules map to *reviewing* (someone else's PR), stacked or not. The lane exists only when the rules are enabled and *reviewing* is mapped.
  2. **One lane per stack**, ordered by **stack number descending** (newest first). Inside a lane, worktrees are ordered by stack position:
     - **Desc** (default): the PR furthest from main first;
     - **Asc**: the PR on main first.
  3. **Not in a stack.** These are ordered by the rank of their workspace status in the configured status list (PR 194), so a move to *Approved* reorders them.
- **Headers** (`worktree-list/rows/header-visuals.ts`):
  - Reviewing: an `Eye` icon and "Reviewing";
  - a stack: a `Layers` icon and **"Stack #N"** in foreground tone;
  - Not in a stack: a `Layers2` icon and **"Not in a stack"** in muted tone.
- **Asc/Desc toggle** (`sidebar/SidebarStackOrderToggle.tsx`, PR 210). It shows only under Group by = Stack, in the Workspace options menu.
  - It is a full-width 24px outline toggle group of two items:
    - `ArrowDownWideNarrow` **Desc**, tooltip "List each stack from the PR furthest from main down";
    - `ArrowUpNarrowWide` **Asc**, tooltip "List each stack from the PR on main up".
  - The selected item is bold on `foreground/10`.
  - The choice persists as the UI field `stackOrder: 'asc'|'desc'` (default `desc`).
- **Paired phone:** its feed has no stack facts, so everything shows under "Not in a stack".
- **Workspace status chip** (§67, PRs 190, 192 and 193; `sidebar/worktree-card-workspace-status.tsx`).
  - Card display → "Workspace status" puts a chip on its own row **above the Linear title**: 16px tall, a 4px radius, a sidebar border, a 55% accent fill, the status icon in its colour, and the label in 3xs muted text.
  - The tooltip reads "Status: X".
  - Linear labels follow as matching chips with coloured dots (§70).
  - It exists so the status column stays visible under Stack grouping.

### A.10 Creating a stacked PR (upstream; still in the fork's Checks-panel composer)

- `useHostedReviewStackParent` looks up an open PR for the composer's **base** branch. It runs only when the base is neither the repo default nor the head, with a 300 ms debounce.
- When a parent is found, the composer shows a checkbox, **"Stack this PR above #P"**, with the hint "Creates a GitHub Stack or extends the parent's existing stack." The hint is hidden once ticked.
- The submit label becomes "Create PR in stack", "Create draft PR in stack" or "Push & Create PR in stack".
- Main `stacked-hosted-review-creation.ts` with `github/stacked-pr-creation.ts`:
  1. **Preflight.** Only github.com is allowed. The base and head must be non-empty and different. Orca reads `GET pulls?head=o:base&state=open&per_page=2`, which must return exactly one parent, and `GET pulls?head=o:head&base=base&state=open`, which must return at most one. It then reads `GET /repos/o/r/stacks?pull_request=N` for both.
     - The pair may already be registered as adjacent entries in the same stack. Then nothing is done.
     - If the head is already in another stack: "The pull request already belongs to a different GitHub stack."
     - If the parent is in more than one stack: an error.
     - If the parent's stack is closed: "The selected parent belongs to a closed stack."
     - If the parent is not the top of its stack: "Choose the top pull request in the stack as the base branch before adding another layer."
  2. Create the PR if it is missing. An existing PR that targets another base gives an error and links it.
  3. **Register:**
     - if the parent has a stack: `POST /repos/o/r/stacks/{n}/add` with `pull_requests[]=child`;
     - otherwise: `POST /repos/o/r/stacks` with `pull_requests[]=parent&pull_requests[]=child`.
     - The returned `number` must be a positive integer.
  4. **Errors.** A 404 or "feature not supported" means "GitHub stacked pull requests are not available for this repository." Other errors return `createdReview`, so the UI links the PR before it shows the stack error.

---

## Part B. The complete GitHub status matrix

Notes on the tables:
- **Orca today:** H = handled (shown or used), P = partly, I = ignored.
- **Cond:** whether the value should drive a rules-engine fact (Part C).
- **Fields:** the minimal GraphQL fields to fetch.

Colour vocabulary, the same as the fork's tones:
- **green** = emerald (ready, passed);
- **amber** = pending or attention;
- **orange** = queued;
- **red** = rose (blocked, failed);
- **violet** = merged;
- **grey** = neutral or muted;
- **blue** = informational.

### B.1 Pull request lifecycle

| Value | Meaning | Orca | UI | Cond | Fields |
|---|---|---|---|---|---|
| `state OPEN` | open | H | "Open" grey pill; status icon by checks | yes | `state` |
| `state CLOSED` | closed unmerged | H | "Closed" red; panel offers Reopen | yes (`closed`) | `state closedAt` |
| `state MERGED` | merged | H | "Merged" violet; panel offers Delete worktree | yes (`merged`) | `state merged mergedAt mergeCommit{oid} mergedBy{login}` |
| `isDraft true` | draft | H | "Draft" grey pill; next action Ready for review | yes | `isDraft` |
| `locked` / `activeLockReason` | conversation locked | I | lock icon and tooltip with the reason | yes | `locked activeLockReason` |
| `baseRef == null` | base branch deleted | I | red "Base branch deleted" | yes | `baseRef{name}` |
| `headRef == null` (open PR) | head branch deleted | P (head-oid guards only) | red "Head branch deleted"; blocks push actions | yes | `headRef{name}` |
| base changed | base retargeted (manual or automatic after a stack merge) | I (the next fetch just shows a new `baseRefName`) | blue info "Base changed to X"; triggers diff-base repair | yes (event) | `baseRefName baseRefOid`; timeline `BASE_REF_CHANGED_EVENT`, `AUTOMATIC_BASE_CHANGE_SUCCEEDED_EVENT/FAILED_EVENT` |
| head moved / force-pushed | new commits | P (head sync for review workspaces; triage keyed by sha) | "Updated N min ago"; "force-pushed" note | yes (`pr.head_moved`) | `headRefOid`; timeline `HEAD_REF_FORCE_PUSHED_EVENT` |
| `isCrossRepository`, `maintainerCanModify` | fork PR, push rights | P (headRepo kept) | "from fork" chip; disable push actions when false | yes | `isCrossRepository maintainerCanModify headRepositoryOwner{login}` |
| `additions` / `deletions` / `changedFiles` | size | H (+/− card chip, §80) | green +N, red −N | yes (size rules) | `additions deletions changedFiles` |
| `closingIssuesReferences` | linked issues | I | issue chips | yes | `closingIssuesReferences(first:5){nodes{number title state}}` |
| `viewerCanUpdateBranch`, `viewerCanMergeAsAdmin`, `viewerCanEnableAutoMerge`, `viewerCanDisableAutoMerge` | permissions | I | enable or disable buttons accordingly | gate | same names |

### B.2 Mergeability and merge state

`mergeable` (MergeableState):

| Value | Orca | UI | Cond | Notes |
|---|---|---|---|---|
| MERGEABLE | H | — | yes | |
| CONFLICTING | H (conflicts condition, panel, next action, conflict agent) | red "Conflicts" plus a list of conflicting files from local git | yes | |
| UNKNOWN | H (rules: *no move this tick*; 2.5 s retry after a manual refresh) | grey "Checking…" spinner | yes, as "unknown" (rules must be able to wait) | GitHub computes this lazily, and the first read after a push is often UNKNOWN |

`mergeStateStatus` (MergeStateStatus). The REST `mergeable_state` has the same values in lowercase; Orca uppercases them.

| Value | Meaning | Orca | Recommended UI | Cond |
|---|---|---|---|---|
| CLEAN | mergeable, commit status passing | H | green "Ready to merge" | yes |
| HAS_HOOKS | mergeable with passing status, pre-receive hooks exist | H (= ready) | green "Ready to merge" | yes |
| UNSTABLE | mergeable, but non-required status failing | H (= ready; "Checks failed" when the rollup failed) | green "Ready" plus an amber dot "non-required checks failing" | yes |
| BEHIND | head out of date; strict required checks | H ("Behind" amber; stack blocker "must be updated") | amber "Behind {base}" plus an "Update branch" action (`updatePullRequestBranch`) | yes |
| BLOCKED | blocked by a requirement (reviews, required checks, rules, conversation resolution, deployments) | H ("Blocked" red in the panel, "Not ready" amber in the stack map) | amber "Blocked: {reason}", where the reason comes from B.6 | yes |
| DIRTY | conflicts, merge commit can't be created | H (= conflicts) | red "Conflicts" | yes |
| DRAFT | deprecated; use isDraft | I (state draft wins) | — | no |
| UNKNOWN | not computed | P (merge state "Checking") | grey "Checking…" | yes ("unknown") |

Orca's resolved kinds today (`shared/github/pull-request-merge-state.ts`), first match wins:
1. merged
2. closed
3. draft
4. in_merge_queue
5. review_required
6. changes_requested
7. conflicts
8. merge_queue (or merge_queue_auto_merge_on)
9. merge_status_unavailable or checks_passed (no metadata)
10. behind
11. blocked
12. mergeable_checks_{failed, pending, passed}, or mergeable
13. checks_passed or checking

Keep this as the PR panel's "merge state" chip.

### B.3 Auto-merge and the merge queue

| Value | Orca | UI | Cond | Fields |
|---|---|---|---|---|
| `autoMergeRequest != null` | H (`autoMergeEnabled`; Disable auto-merge) | amber "Auto-merge on ({method}) by @x" | yes | `autoMergeRequest{enabledAt enabledBy{login} mergeMethod}`; `autoMergeRequest(includeStack:true)` for stacks |
| auto-merge disabled + reason | I | toast or timeline "Auto-merge disabled: {reason}" | yes (event) | timeline `AUTO_MERGE_DISABLED_EVENT{reason reasonCode}` |
| repo `autoMergeAllowed` | H | hides Enable auto-merge | gate | `repository.autoMergeAllowed` |
| base requires a merge queue | H (`mergeQueueRequired`, 10-min cache) | "Merge when ready" | yes | `repository.mergeQueue(branch:){id}`, `ref.rules{type}` (MERGE_QUEUE) |
| `isInMergeQueue` | H | orange "queued" pill / "In merge queue" | yes | `isInMergeQueue` |
| `mergeQueueEntry.state = QUEUED` | I | orange "Queued #pos" | yes | `mergeQueueEntry{state position}` |
| `AWAITING_CHECKS` | I | orange spinner "Queue checks running" | yes | `mergeQueueEntry{state}` |
| `MERGEABLE` | I | orange→green "Merging next" | yes | |
| `UNMERGEABLE` | I | red "Will be removed from queue" | yes | |
| `LOCKED` | I | orange "Locked (merging)" | yes | |
| `position`, `estimatedTimeToMerge` (seconds), `enqueuedAt`, `enqueuer`, `jump`, `solo`, `baseCommit`/`headCommit` | I | "#3 · ~12 min" tooltip; "jumped" and "solo" chips | position/ETA as numeric facts | `mergeQueueEntry{position estimatedTimeToMerge enqueuedAt enqueuer{login} jump solo}` |
| queue config (`mergeMethod`, `mergingStrategy` ALLGREEN/HEADGREEN, `groupingStrategy`, `maximumEntriesToBuild/ToMerge`, `minimumEntriesToMerge`, `checkResponseTimeout`) | I | queue info popover | rarely | `mergeQueue(branch:){configuration{…} nextEntryEstimatedTimeToMerge entries{totalCount}}` |
| **removed from queue** + reason | I | red toast "Removed from merge queue: {reason}"; status falls back | **yes** (`pr.dequeued(reason)`) | timeline `REMOVED_FROM_MERGE_QUEUE_EVENT{createdAt reason enqueuer{login} beforeCommit{oid}}`, `ADDED_TO_MERGE_QUEUE_EVENT` |

**Removal reasons.**
- GraphQL `RemovedFromMergeQueueEvent.reason` is a free **String**, and the webhook `pull_request.dequeued.reason` is a string too. GitHub does not publish the set.
- The values commonly seen are: `MANUAL`, `MERGE`, `MERGE_CONFLICT`, `CI_FAILURE`, `CI_TIMEOUT`, `ALREADY_MERGED`, `QUEUE_CLEARED`, `ROLL_BACK`, `BRANCH_PROTECTIONS`, `GIT_TREE_INVALID`, `INVALID_MERGE_COMMIT`, `UNKNOWN_REMOVAL_REASON`. **These are unverified; treat them as open strings**, map the known ones to labels, and show the rest verbatim.
- The webhook `merge_group.destroyed` reasons are `merged`, `dequeued` and `invalidated`.

**Stack-specific merge-async results** (A.6):
- request `status`: `pending | merged | enqueued | failed`;
- HTTP: 200 (already merged or queued), 202 (accepted), 400 (draft or closed), 403, 404 (stacks off), 409 (already pending; reuse its uuid), 422.
- `enqueued` is final for the request but does **not** mean merged.
- Results are kept for **24 h**.

UI while running: a spinner on the button, "Merging stack…" / "Queuing stack…". The result is persisted (see R6).

### B.4 Reviews and review requests

| Value | Orca | UI | Cond | Fields |
|---|---|---|---|---|
| `reviewDecision APPROVED` | H (merge state, stack badge "Approved") | green check "Approved" | yes | `reviewDecision` |
| `CHANGES_REQUESTED` | H | red "Changes requested" | yes | |
| `REVIEW_REQUIRED` | H | amber "Review required" | yes | |
| `reviewDecision null` (no review requirement) | H (falls through) | — | yes ("none") | |
| review `APPROVED` / `CHANGES_REQUESTED` / `COMMENTED` / `DISMISSED` / `PENDING` (PullRequestReviewState) | H (snapshot keeps all 5 with an author; rules use opinionated ones; reviewer list shows standing; verdict badges §56) | avatar stack with ✓ (green), ± (red), 💬 (grey), struck-through (dismissed), draft (viewer's own PENDING) | yes (`pr.reviews.by(user).state`, `pr.approvals.count`) | `latestOpinionatedReviews(first:20){nodes{state author{login} commit{oid} submittedAt}}`, `latestReviews`, `viewerLatestReview{state commit{oid}}` |
| review dismissed event | I | "Approval dismissed by @x" | yes (event) | timeline `REVIEW_DISMISSED_EVENT` |
| review stale after a push (a dismiss-stale setting) | I | the approval greyed out "on older commit" | yes | compare `review.commit.oid` to `headRefOid` |
| review requests — users | H | "Awaiting @x" amber avatars; re-request; remove | yes | `reviewRequests(first:20){nodes{asCodeOwner requestedReviewer{__typename ...on User{login} ...on Team{combinedSlug slug} ...on Bot{login} ...on Mannequin{login}}}}` |
| review requests — teams | P (fetched; ignored for PM approval; picker can add/remove `org/team`) | team chip | yes (`pr.requested_team("org/t")`) | as above |
| **CODEOWNERS** (`asCodeOwner`) | I | "Code owner" tag on the request; "Code owner review required" blocker | yes | `reviewRequests{nodes{asCodeOwner}}`; requirement from rules (B.6) |
| viewer standing (owed / finished / re-requested) | H (rules) | "Your review: requested / approved / re-requested" | yes | as above |
| unresolved review threads | H (Unresolved tab, §81) | "N unresolved" chip | yes (count; required by conversation-resolution rules) | `reviewThreads(first:100){totalCount nodes{isResolved isOutdated}}` |

### B.5 Checks and commit statuses

**CheckRun.status** (CheckStatusState):

| Value | Meaning | Orca | UI | Cond |
|---|---|---|---|---|
| QUEUED | waiting to start | H ("Queued") | grey hollow circle | yes |
| IN_PROGRESS | running | H (card "running" amber spinner) | amber spinner and elapsed time | yes |
| COMPLETED | done; see conclusion | H | by conclusion | yes |
| WAITING | waiting on an environment protection rule (deployment approval) | **P/bug**: `mapCheckRunRESTStatus` maps it to "completed", and the rollup normalizer counts it **neutral** (R1) | amber pause icon "Waiting for approval" plus an "Approve deployment" link | **yes** |
| REQUESTED | requested, not yet queued (rerequested / app) | **P/bug**: same as WAITING | grey "Requested" | yes |
| PENDING | queued awaiting concurrency (Actions) | **P/bug**: same as WAITING | grey "Pending" | yes |

**CheckRun.conclusion** (CheckConclusionState):

| Value | Orca | UI | Cond |
|---|---|---|---|
| SUCCESS | H, passed | green check | yes |
| FAILURE | H, failed (CI triage) | red ✕ | yes |
| NEUTRAL | H, neutral | grey dash | yes |
| CANCELLED | H, failed | grey ⊘, counted failed | yes |
| SKIPPED | H, passed | grey skip arrow, counted passed | yes |
| TIMED_OUT | H, failed (triage) | red clock | yes |
| ACTION_REQUIRED | H, failed; pending-approval suites synthesised as rows | amber "Action required" with a link | yes |
| STALE | **P: inconsistent**. The checks list maps it to failure (`mappers.ts`); the rollup and card classify it neutral (not in `FAILED_CONCLUSIONS`) | grey "Stale" (GitHub marks runs incomplete for 14 days as stale) | yes |
| STARTUP_FAILURE | H, failed | red "Startup failure" | yes |

**StatusContext.state** (StatusState):

| Value | Orca | UI | Cond |
|---|---|---|---|
| SUCCESS | H | green | yes |
| FAILURE | H | red | yes |
| ERROR | H (= failure) | red "Error" | yes |
| PENDING | H (= queued; the card ignores it, PR 111, because "Reviews satisfied" stays pending for hours) | grey hollow | yes |
| EXPECTED | **P**: the stack map treats it as pending; the rollup and card count it neutral | amber "Expected — not reported" (a required context that hasn't posted) | yes |

**Rollup and requiredness:**

| Value | Orca | UI | Cond | Fields |
|---|---|---|---|---|
| `statusCheckRollup.state` | P (stack entries only; elsewhere recomputed from contexts) | PR status icon | yes | `commits(last:1){nodes{commit{statusCheckRollup{state}}}}` |
| per-state counts | I | "12 passed · 1 failed · 3 running" | yes (counts) | `contexts(first:1){checkRunCount checkRunCountsByState{state count} statusContextCount statusContextCountsByState{state count}}` |
| **`isRequired`** per check | **I** | a "Required" tag; required failures sorted first; "required checks" sub-rollup | **yes** (`pr.required_checks.state`) | `...on CheckRun{isRequired(pullRequestNumber:$n)}`, `...on StatusContext{isRequired(pullRequestNumber:$n)}` |
| check suites (`status`, `conclusion`, app) | P (only used to synthesise pending-approval rows) | group by app or workflow | rarely | `checkSuites(first:50){nodes{status conclusion app{slug} workflowRun{event runNumber}}}` |
| a named check succeeded | H (the "merging" gate check) | — | yes (`pr.check("name").conclusion`) | contexts |

### B.6 Branch protection and ruleset requirements

What makes a PR `BLOCKED`. Orca reads only `rules{type}` for MERGE_QUEUE.

| Requirement | Orca | UI ("Blocked: …" reason) | Cond | Fields |
|---|---|---|---|---|
| required approving review count | I | "Needs 2 approvals (1/2)" | yes | `baseRef{branchProtectionRule{requiredApprovingReviewCount requiresCodeOwnerReviews dismissesStaleReviews requireLastPushApproval}}`; rulesets via `baseRef{rules(first:50){nodes{type parameters{...on PullRequestParameters{requiredApprovingReviewCount requireCodeOwnerReview dismissStaleReviewsOnPush requireLastPushApproval requiredReviewThreadResolution}}}}}` |
| code owner review | I | "Code owner review required" | yes | as above |
| required status checks (+ strict = up to date) | I (implicit via BEHIND) | "Required: lint, test" | yes | `...on RequiredStatusChecksParameters{requiredStatusChecks{context integrationId} strictRequiredStatusChecksPolicy}`; or `isRequired` (B.5) |
| conversation resolution | I | "N unresolved conversations" | yes | `requiredReviewThreadResolution`, plus B.4 threads |
| required deployments | I | "Deploy to staging required" | yes | `RequiredDeploymentsParameters{requiredDeploymentEnvironments}` |
| linear history, signed commits | I | "Requires signed commits" | rarely | rule types `REQUIRED_LINEAR_HISTORY`, `REQUIRED_SIGNATURES` |
| merge queue | H | B.3 | yes | rule type `MERGE_QUEUE` |
| allowed merge methods | H (`mergeMethodSettings`) | method picker | gate | repo `*Allowed`, `viewerDefaultMergeMethod`; ruleset `allowedMergeMethods` |
| bypass ability | I | "Merge (bypass rules)" for admins | gate | `viewerCanMergeAsAdmin`; merge-async `bypass_rules` |

`branchProtectionRule` and ruleset parameters can be hidden from users without admin or push rights. Treat **null as unknown**, never as "no requirement".

### B.7 Deployments and environments

Orca today ignores deployments. The only related feature is §25, which parses "Preview deployment for `sha`" bot comments.

| Value | UI | Cond | Fields |
|---|---|---|---|
| DeploymentState `QUEUED`, `PENDING`, `IN_PROGRESS`, `WAITING` | amber rocket "Deploying to {env}" | yes | `commits(last:1){nodes{commit{deployments(first:10,orderBy:{field:CREATED_AT,direction:DESC}){nodes{environment state latestStatus{state environmentUrl logUrl createdAt}}}}}}` |
| `ACTIVE`, `SUCCESS` | green rocket; env URL button (generalises §25) | yes (`pr.deployment(env).state`, `.url`) | |
| `FAILURE`, `ERROR` | red rocket | yes | |
| `INACTIVE`, `DESTROYED`, `ABANDONED` | grey | yes | |
| DeploymentStatusState: the same set minus `ABANDONED` and `DESTROYED` | — | — | `latestStatus.state` |
| pending environment approval | amber "Approve deploy to prod" | yes | check run `WAITING`, plus REST `GET /repos/o/r/actions/runs/{run_id}/pending_deployments` |

### B.8 Stack-specific statuses

| Value | Orca | UI | Cond | Fields |
|---|---|---|---|---|
| in a stack (`stack != null`) | H | stack map; card `P/S` | yes | REST `pull.stack`; GraphQL `stackEntry{position stack{number size baseRefName}}` |
| position, size, is top, is bottom | H | `2/3` | yes | as above |
| stack base out of date (`base.sha != tip`) | H | amber bar, Rebase stack | yes | REST `stack.base.sha` + `baseRef: ref(...)`; or GraphQL `baseRefOid` of the bottom PR vs tip |
| all unmerged entries ready | H (Merge stack button) | button | yes (`stack.ready`) | entries B.2 fields |
| first blocker below the target (draft, closed, conflicts, changes_requested, review_required, behind, blocked) | P (shared code, no UI) | "Blocked by #810: needs approval" | yes | entries |
| any entry queued | H | "Queued" badge; button hidden | yes | `isInMergeQueue` per entry |
| stack closed or dissolved (`open:false`, 204 on unstack) | P (only at creation) | grey "Stack closed" | yes | REST `GET /stacks/{n}` (`open`) |
| added to a stack (webhook `pull_request.stacked`) | I | event | yes (event) | poll diff of `stack` |
| async merge pending, merged, enqueued or failed | H, in memory, 3 min | B.3 | yes | REST merge-async |
| stacks feature unavailable (404) | H at creation | hide stack actions | gate | 404 |

### B.9 One efficient polling design

Constraints:
- GraphQL has 5,000 points per hour and no conditional requests.
- REST has 5,000 per hour, and an authenticated **304 is free**.
- Search is limited to 30 per minute.

The goal is that nothing is fetched twice unless it changed.

**Tier 0 — free change detectors (REST plus ETag).** A 304 means "skip tier 2 for this key".
- Per repo: `GET /repos/o/r/pulls?state=all&sort=updated&direction=desc&per_page=50`. Its `updated_at` moves on push, review, comment, label, state change and retarget. It also carries `stack`.
- Per tracked head SHA with non-terminal checks: `GET /repos/o/r/commits/{sha}/check-suites` and `GET /repos/o/r/commits/{sha}/status`. These catch check transitions, which **don't** bump PR `updated_at`.
- Per active stack: `GET /repos/o/r/stacks/{n}`, for membership and `open`.
- Reuse Orca's `revalidateGitHubRestJson`: a bounded representation map (512 entries, 16 MiB), keyed by execution scope and endpoint.

**Tier 1 — one batched GraphQL "fingerprint" query for every tracked PR.** It uses `nodes(ids:)` with up to 100 PR node IDs per document, so there is no alias explosion. It asks only for scalars and counts:

```graphql
query($ids:[ID!]!) {
  rateLimit { cost remaining resetAt }
  viewer { login }
  nodes(ids:$ids) { ... on PullRequest {
    id number updatedAt state isDraft locked headRefOid baseRefName baseRefOid
    headRef { name } baseRef { name }
    mergeable mergeStateStatus reviewDecision isInMergeQueue
    mergeQueueEntry { state position estimatedTimeToMerge }
    autoMergeRequest(includeStack:true) { enabledAt mergeMethod }
    stackEntry { position stack { number size baseRefName } }
    reviewRequests { totalCount } reviewThreads { totalCount }
    commits(last:1) { nodes { commit { oid statusCheckRollup { state
      contexts(first:1) { checkRunCountsByState { state count } statusContextCountsByState { state count } } } } } }
  } }
}
```

- The expected cost is about 1–3 points per 100 PRs.
- `fingerprint = hash(all scalars)`. A PR whose fingerprint is unchanged is done for this tick.
- `UNKNOWN` mergeability is re-asked at +2.5 s, then +10 s, then left to the next tick.

**Tier 2 — detail per changed PR or stack.** These are batched into one document per tick, at most 20 PRs:
- reviews and requests (B.4);
- contexts with `isRequired(pullRequestNumber:)` (B.5);
- timeline items since the last seen cursor, of these types: `BASE_REF_CHANGED_EVENT`, `HEAD_REF_FORCE_PUSHED_EVENT`, `HEAD_REF_DELETED_EVENT`, `ADDED_TO_MERGE_QUEUE_EVENT`, `REMOVED_FROM_MERGE_QUEUE_EVENT`, `AUTO_MERGE_ENABLED_EVENT`, `AUTO_MERGE_DISABLED_EVENT`, `REVIEW_DISMISSED_EVENT`, `READY_FOR_REVIEW_EVENT`, `CONVERT_TO_DRAFT_EVENT`, `DEPLOYED_EVENT`;
- deployments (B.7);
- the stack's entries via `pullRequest.stack{entries{…B.2 fields + isInMergeQueue}}`, plus `baseTip` — once per stack, not once per member.

**Tier 3 — repo metadata.** Merge methods, `autoMergeAllowed`, `mergeQueue(branch){configuration}`, and `ref.rules` with parameters plus `branchProtectionRule`. Refreshed every 10 min, and also on a base change or a merge error.

**Inbox.** The `review-requested:@me` search stays separate, on a 60 s cadence. It counts against the search budget and is bounded to 30 results.

**Cadence per PR.** This keeps Orca's policy and adds:
- **15 s:** the active workspace's PR while checks are running, while it is in the queue (`AWAITING_CHECKS` or `LOCKED`), or while a merge-async is pending (poll its uuid at 1 s, then 5 s after 30 s, until terminal);
- **60 s:** selected or visible;
- **120 s:** other open PRs;
- **15 min:** closed;
- **stop:** merged with settled checks.
- The window being hidden does not pause the **server** poller, which serves the rules engine (plan.md §2). Only the client's visibility-based extras pause.

**Budget.**
- Tier 1 for 100 PRs at 60 s ≈ 180 points per hour.
- Tier 2 at no more than 20 PRs per minute × about 5 points ≈ 6,000 points per hour worst case. That is too much, so cap tier 2 at **1,500 points per hour** and favour visible PRs.
- Read `rateLimit.remaining`. Below 500, drop to tier 1 only. Below 50, stop, keeping Orca's breaker floors of 50 for core and graphql and 2 for search.
- Treat secondary-limit 403/429 responses with `retry-after` as a global backoff.

**Refresh triggers without webhooks.** Each enqueues an immediate refresh for the affected PRs:
1. **A local `git push`** of the head branch, seen through the git watcher or the server's git runner: +2.5 s, then a follow-up at +15 s for checks to appear.
2. **A `git fetch`** that moves `origin/<base>` or `origin/<head>`. That means a stack may be out of date, or the head moved.
3. **An agent turn ending** in a workspace with a PR (agents push and run `gh pr …`).
4. **An app mutation** (merge, enqueue, auto-merge, ready, review submit, re-request, comment): immediate, then +3 s, +10 s and +30 s until the expected state is seen.
5. **Window focus or visibility**, and selecting a workspace or opening the Checks tab.
6. **A merge-async uuid** reaching a terminal status: refresh the whole stack.
7. **`icarus pr refresh`** from the CLI, so agents can force it.
8. **A Tier 0 304→200 flip.**

---

## Part C. Generalisation

### C.1 Fact model for the rules engine

Every value in Part B becomes a typed fact on `pr.*`, with the enum values as listed. Facts are recomputed on each snapshot. **Transition events** are derived by diffing the previous and current snapshot, giving `changed(fact)` and `entered(fact == v)`. Conditions are expressions over facts (plan.md §3).

| Fact | Type |
|---|---|
| `pr.exists`, `pr.number`, `pr.url`, `pr.title`, `pr.author`, `pr.is_mine` | bool / int / str |
| `pr.state` | `open \| draft \| closed \| merged` |
| `pr.locked`, `pr.cross_repo`, `pr.maintainer_can_modify` | bool |
| `pr.base.name`, `pr.base.deleted`, `pr.base.changed` (event), `pr.head.oid`, `pr.head.deleted`, `pr.head.moved` (event), `pr.head.force_pushed` (event), `pr.head.is_local` | |
| `pr.mergeable` | `MERGEABLE \| CONFLICTING \| UNKNOWN` |
| `pr.merge_state` | `CLEAN \| HAS_HOOKS \| UNSTABLE \| BEHIND \| BLOCKED \| DIRTY \| UNKNOWN` |
| `pr.merge_state_kind` | Orca's 18 resolved kinds (B.2) |
| `pr.blockers` | set of `review_required \| code_owner_review \| approvals_short(n) \| required_checks \| conversations \| deployments \| signatures \| behind \| conflicts \| draft` |
| `pr.review_decision` | `APPROVED \| CHANGES_REQUESTED \| REVIEW_REQUIRED \| none` |
| `pr.reviews.approvals`, `pr.reviews.by(login).state`, `pr.reviews.unanswered_change_request`, `pr.reviews.dismissed` (event), `pr.approved_by_team("org/t")` | |
| `pr.requests.users`, `pr.requests.teams`, `pr.requests.code_owners`, `pr.requests.only_team("org/t")`, `viewer.owed`, `viewer.finished`, `viewer.rerequested` (event) | |
| `pr.threads.unresolved` | int |
| `pr.checks.state` (all) / `pr.checks.required_state` | `success \| failure \| pending \| neutral \| none` |
| `pr.checks.counts.<STATE>` | int, per CheckRunState and StatusState |
| `pr.check("name").status / .conclusion / .required / .url` | B.5 enums |
| `pr.checks.waiting_approval` | bool |
| `pr.auto_merge.enabled`, `.method`, `.disabled_reason` (event) | |
| `pr.merge_queue.required`, `.in_queue`, `.state` (`QUEUED \| AWAITING_CHECKS \| MERGEABLE \| UNMERGEABLE \| LOCKED`), `.position`, `.eta_seconds`, `.dequeued(reason)` (event) | |
| `pr.deployment("env").state`, `.url` | DeploymentState |
| `pr.stack.in_stack`, `.number`, `.position`, `.size`, `.is_top`, `.is_bottom`, `.out_of_date`, `.ready`, `.blocker` (`{reason, pr}`), `.any_queued`, `.merge.status` (`pending \| merged \| enqueued \| failed`) | |
| `pr.size.additions`, `.deletions`, `.files` | int |

**Today's ten conditions as a preset.** Order matters, and the first match wins:
1. `merged: pr.state == merged`
2. `closed: pr.state == closed`
3. `reviewing: !pr.is_mine`
4. `merge-queue: pr.merge_queue.in_queue`
5. `conflicts: pr.mergeable == CONFLICTING` (UNKNOWN means `stop`)
6. `draft: pr.state == draft`
7. `changes-requested: pr.reviews.unanswered_change_request`
8. `merging: pr.check($mergingCheckName).conclusion == SUCCESS || (pr.requests.users == 0 && pr.approved_by_team($pm))`
9. `pm-approval: pr.requests.only_team($pm)`
10. `review: true`

**New examples the matrix enables:**
- `when entered(pr.merge_queue.state == UNMERGEABLE) or pr.dequeued(reason in [CI_FAILURE, MERGE_CONFLICT])` → `launch_agent(ci-triage)` and `set_column(Conflicts)`.
- `when pr.stack.out_of_date && pr.stack.is_top` → `launch_agent(rebase-stack template)`. This uses one agent per stack. Dedupe by `stack.number@base_tip`.
- `when pr.stack.ready && pr.stack.is_top && column == "Approved"` → `notify("Stack #N ready to merge")`. Never merge automatically by default.
- `when pr.checks.waiting_approval` → `notify` with the approve link.
- `when pr.base.changed` → repair the compare base (today's PR-created worktree repair, as an explicit rule).
- `when pr.head.deleted && pr.state == open` → `set_column(Hold)`.

**Guards.**
- Facts with an `UNKNOWN` or `null` value **must not** satisfy `==` against concrete values. The rules engine needs a three-valued `unknown` that makes the condition `stop` (no move), the way Orca treats UNKNOWN mergeability.
- An empty snapshot (rate limit or error) gives no actions, as today.

### C.2 CLI

Everything goes through the server API (plan.md §2). Every command takes `--workspace <id|path>` (default: the cwd's workspace) or `--pr owner/repo#N`, and `--json`.

| Command | Does |
|---|---|
| `icarus pr status [--json] [--fields a,b]` | all `pr.*` facts (C.1) for the PR, including the stack and the merge queue; exit code 0 open, 3 merged, 4 closed, 2 no PR |
| `icarus pr checks [--required] [--failed] [--json] [--watch]` | check rows: name, status, conclusion, required, url, startedAt, completedAt; `--watch` streams changes |
| `icarus pr wait --until '<expr>' [--timeout 30m]` | blocks until a fact expression holds, for example `pr.checks.required_state == success` or `pr.merge_queue.state == MERGEABLE`. Agents use this instead of sleeping |
| `icarus pr refresh` | forces tiers 1 and 2 now |
| `icarus pr merge [--method squash] [--queue\|--direct] [--yes]` | non-stacked uses the legacy path with Orca's blockers; stacked behaves like `stack merge --through <this>` |
| `icarus pr auto-merge on\|off [--method]`, `icarus pr enqueue [--jump]`, `icarus pr dequeue` | auto-merge, or the queue mutations `enqueuePullRequest` / `dequeuePullRequest` |
| `icarus pr update-branch [--rebase]` | `updatePullRequestBranch` (the BEHIND fix) |
| `icarus pr ready`, `icarus pr reviewers add\|rm <login\|org/team>` | as in the UI |
| `icarus stack show [--json]` | the stack map: number, base, base_sha, base_tip, out_of_date, and entries (position, number, title, state, badge {label, tone}, merge_state, review_decision, checks, in_queue, worktree id if any) top-down; plain output mimics A.3 |
| `icarus stack merge [--through N] [--method] [--queue\|--direct] [--yes] [--no-wait]` | the A.6 preconditions (full-stack button rule, or the shared blocker rule with `--through`); prints the confirmation text unless `--yes`; submits merge-async; polls until terminal unless `--no-wait`; persists the uuid so `stack merge --resume` works within 24 h |
| `icarus stack rebase [--agent X] [--template T]` | launches the rebase-stack agent with the A.7 prompt template |
| `icarus stack create <pr…>`, `icarus stack add <n> <pr…>`, `icarus stack unstack <n>` | the REST stacks API, with Orca's preflight messages |
| `icarus rules facts [--json]` | the full fact set the engine sees for a workspace |
| `icarus rules test <rule> [--workspace]` | dry-run: which condition matches, and what actions would run |

**JSON shape** for `pr status`. Its keys are the fact names, so automations, the CLI and the UI share one vocabulary:

```json
{ "pr": { "number": 811, "state": "open", "mergeable": "MERGEABLE", "merge_state": "BLOCKED",
  "merge_state_kind": "blocked", "blockers": ["review_required"], "review_decision": "REVIEW_REQUIRED",
  "checks": { "state": "pending", "required_state": "success", "counts": { "SUCCESS": 12, "IN_PROGRESS": 1 } },
  "merge_queue": { "required": true, "in_queue": false, "state": null, "position": null, "eta_seconds": null },
  "auto_merge": { "enabled": false },
  "stack": { "number": 42, "position": 2, "size": 3, "base": "main", "out_of_date": false, "ready": false,
             "blocker": { "reason": "review_required", "pr": 811 } } },
  "fetched_at": "…", "unknown": ["deployments"] }
```

---

## Risks and open questions

- **R1 — WAITING, REQUESTED and PENDING check runs are misread today.**
  - `main/github/mappers.ts` `mapCheckRunRESTStatus` maps anything that isn't `queued` or `in_progress` to `completed`.
  - The rollup normalizer (`shared/pr-check-status.ts`) treats a `waiting` run with no conclusion as **neutral**.
  - So a PR held behind an environment-approval gate can show a green card.
  - `STALE` is "failure" in the checks list but neutral on the card. `EXPECTED` is pending in the stack map but neutral elsewhere.
  - The rewrite needs one classifier.
- **R2 — Stacks are in public preview** (REST `2026-03-10`).
  - Orca's `gh api` calls send **no `X-GitHub-Api-Version`** header.
  - Repos without the feature return 404.
  - Pin the version header, and gate every stack action on a capability probe.
- **R3 — The counts disagree.** The Merge stack button counts *unmerged* entries, but the dialog's N is the target's *position*, which includes merged entries below.
- **R4 — Auto-merge for stacks is a requirement, not a refusal (product decision, 2026-11-15).** The rewrite must support stack auto-merge. Today Orca refuses it.
  - **Status on GitHub's side:** stacked PRs went GA on 2026-10-06, and the changelog says stack auto-merge is "rolling out over the following weeks" (github.blog/changelog/2026-10-06-stacked-pull-requests-generally-available). Some docs still say it is unsupported.
  - **What to build:**
    1. Use GitHub's native stack auto-merge (`autoMergeRequest(includeStack:)` / enable on the stack) wherever the repo has it.
    2. Detect support per repo at runtime, not by a hardcoded refusal: probe it, and on a "not supported" error, fall back.
    3. **Fallback:** the server turns on our own "merge stack when ready". It watches the stack's facts and calls the existing `merge-async` stack merge (direct or queue) on the top PR once every unmerged PR is Ready, pinned to the head shas it saw. It also works with the app closed, because the server owns it.
  - **UI:** "Auto-merge stack" on the stack map and in the top-bar next action. It shows who enabled it, the method (direct or merge queue) and whether it is native or app-managed. It turns off automatically, with a reason, if a head moves or someone pushes.
  - **Rules engine:** the action `enable_stack_auto_merge`; the facts `stack.auto_merge.enabled` and `stack.auto_merge.disabled_reason`.
  - **CLI:** `icarus stack auto-merge on|off [--method direct|queue]`.
  - **Verify against a real stack in a repo with stacks enabled before settling the native path.**
- **R5 — The out-of-date signal compares REST `stack.base.sha` with the base tip.** GitHub doesn't document exactly when `base.sha` moves. For example, does a server-side cascade rebase update it? A false positive offers a force-pushing agent.
- **R6 — The async merge is held only in memory for 3 minutes.** GitHub says stack merges can take minutes, and keeps results for 24 h. Persist the uuid server-side and resume.
- **R7 — Merge queue removal reasons are untyped strings**, and the reason list in B.3 is unverified. Orca has no dequeue detection at all.
- **R8 — Branch protection and ruleset parameters may be invisible without admin rights.** Blocker reasons must degrade to "Blocked" when unknown, never to "no requirement".
- **R9 — Duplicate lane headers.** Stack lanes are keyed by `repoId:number`, but the header reads only "Stack #N". Two projects with the same stack number give two identical headers. Add the project name in the rewrite.
- **R10 — Rebase stack names a hardcoded personal skill (A.7), and it force-pushes.** Ship it as a template with an explicit confirm.
