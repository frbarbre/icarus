# Orca settings export/import: compatibility spec

Target: a from-scratch Rust rewrite must read the file `orca settings export` writes, apply it as the
Electron fork does, and write a file the Electron fork accepts.

Sources (fork at `~/Developer/orca`, HEAD `0d92720996`, 2026-11-13):

- `src/shared/settings-transfer.ts`: envelope, settings and UI filters, merge helpers
- `src/shared/settings-transfer-ui-fields.ts`: classification of every `PersistedUIState` key
- `src/shared/settings-transfer-projects.ts`: classification and parsers for every `Repo` field
- `src/shared/settings-transfer-cli.ts`, `src/cli/handlers/settings-transfer.ts`,
  `src/main/runtime/rpc/methods/settings-transfer.ts`, `src/main/window/settings-transfer-cli-relay.ts`
- `src/main/settings-transfer/settings-transfer.ts`: keybindings and editor-theme files, dialogs
- `src/renderer/src/lib/settings-transfer-actions.ts`: the export builder and the import applier
  that the GUI and the CLI share
- `src/renderer/src/components/settings/SettingsTransferSection.tsx`: the GUI
- `src/shared/workspace-status-rule-config.ts`, `workspace-status-rules.ts`,
  `workspace-status-rule-plan.ts`, `workspace-new-status-plan.ts`, `workspace-statuses.ts`,
  `workspace-action-prompts.ts`, `workspace-status-rule-prompt.ts`,
  `workspace-status-rule-conflict-prompt.ts`, `ci-triage.ts`
- Tests: `src/shared/settings-transfer.test.ts`, `src/main/settings-transfer/settings-transfer.test.ts`,
  `src/cli/handlers/settings-transfer.test.ts`
- FORK.md §3, §8, §13, §16, §64, §73, §83

Real data: a real export from the reference setup, `scratch/orca-settings.json` (gitignored, not
in the repo), is the **source of truth** for the checks below. It was written by Orca 1.4.485 with
`orca settings export` and contains every section of the envelope (settings, UI keys, projects,
keybindings and an editor theme). Which machine's export counts, and which older exports are stale,
is recorded in `private/reference-setup.md` (not in the repo).

Other files in `scratch/`. Note that `defaults.json` and `analysis.json` were built from an older
export, so regenerate `analysis.json` before relying on its list of changed values:

- `scratch/defaults.json`: `getDefaultSettings()`, the default UI state and
  `cloneDefaultWorkspaceStatusRuleConfig()`, evaluated from the fork's own TypeScript (macOS,
  home `/HOME`)
- `scratch/analysis.json`: each setting key's classification, plus the reference export's values
  that differ from the defaults
- `scratch/tools/*.mjs`: the scripts that produced these files. They run the fork's TS under
  Node 26 type stripping with npm imports stubbed.

> Checks run against the real file:
>
> - The fork's own `parseSettingsTransfer` returns `settings`, `ui` and `projects` identical to
>   the file, so the file is already in its filtered, portable form.
> - Re-serializing the file with an order-preserving JSON writer (2-space indent, non-ASCII left
>   unescaped, one trailing `\n`) reproduces it byte for byte.
> - No credential-like values were found. I scanned for token patterns (`sk-`, `ghp_`,
>   `github_pat_`, `xox*`, `AKIA`, JWTs, `lin_api_`, long base64) and for key names
>   (`key|token|secret|pass|auth|cred`). The only key-name hits are harmless:
>   `settings.jiraIssueKeyCasing`, `settings.prBotAuthorOverrides` (`[]`),
>   `settings.geminiCliOAuthEnabled` (`false`) and the top-level `keybindings`.

---

## 1. File envelope

```jsonc
{
  "format": "orca-settings",        // exact string; anything else is rejected
  "version": 1,                     // exact number 1; anything else is rejected ("newer Orca")
  "exportedAt": "2026-11-15T11:34:23.043Z", // JS Date.toISOString(); informational only
  "settings": { ... },              // portable GlobalSettings subset, an open map
  "ui": { ... },                    // portable PersistedUIState subset, an open map
  "projects": [ { "remote": "...", "name": "...", "settings": { ... } } ],
  "keybindings": "<string: full text of ~/.orca/keybindings.json>" | null,
  "editorThemes": { "editor-dark.json"?: "<string>", "editor-light.json"?: "<string>" }
}
```

Exact rules, from `buildSettingsTransferBundle` and `parseSettingsTransfer`:

| Field | Export | Import |
|---|---|---|
| `format` | always `"orca-settings"` | must be exactly `"orca-settings"` and the root must be a JSON object, or the error is "The file is not an Orca settings export." |
| `version` | always `1` (`SETTINGS_TRANSFER_VERSION`) | must be strictly `=== 1`, so a string `"1"` fails. Error: "The file comes from a newer Orca; update Orca and try again." No migration path exists yet. |
| `exportedAt` | `new Date().toISOString()`: millisecond precision, `Z` | a non-string becomes `""`. Never used for logic. |
| `settings` | `pickPortableSettings(settings, repos)` | a non-object becomes `{}`. Otherwise it is filtered again with `pickPortableSettings(raw.settings)`, with no repos. |
| `ui` | `pickPortableUi(ui)` | a non-object becomes `{}`. Otherwise it is filtered again with `pickPortableUi`. |
| `projects` | `pickPortableProjects(repos)` | a non-array becomes `[]`. Each entry needs a string `remote` and an object `settings`; every field is re-parsed, and an entry left with no fields is dropped. A missing `name` becomes `""`. |
| `keybindings` | text of `~/.orca/keybindings.json` if it exists and is valid JSON, else `null` | a non-string becomes `null` |
| `editorThemes` | `~/.orca/themes/editor-dark.json` and `editor-light.json`, each only if it exists and is valid JSON | only those two exact names, string values only. Any other key is dropped, so a name like `../../etc/passwd` cannot get through. |

Serialization:

- The GUI writes `JSON.stringify(bundle, null, 2)` with no trailing newline.
- `orca settings export --output F` writes the same text plus `"\n"`.
- `orca settings export` without `--output` prints it with `console.log`, which also ends in a newline.
- Key order in the envelope is exactly the order above.
- Inside `settings` and `ui`, key order is the stored object's insertion order. JS does not sort keys.

**`keybindings` and editor themes are JSON-encoded strings, not nested objects.** The writer
re-checks `JSON.parse` before writing each file. A typical keybindings string decodes to
`{"version":1,"keybindings":{},"platforms":{"darwin":{...overrides...},"linux":{},"win32":{}}}`.

Limits: the GUI's open dialog refuses files over 5 MiB (`MAX_IMPORT_BYTES`). The CLI path has no
size cap.

### 1.1 Transport (CLI)

- `orca settings export [--output FILE]` calls RPC `settingsTransfer.export`, which needs the
  `workspace` permission and takes no params, and returns `{ text }`.
- `orca settings import --file FILE` reads the file on the caller's side and calls RPC
  `settingsTransfer.import { text }`, which needs the `settings-write` permission and returns
  `{ summary }`. The CLI then prints `Imported <summary> from <file>. Orca is reloading.`
- Both RPCs are relayed to the renderer window: main sends IPC `settingsTransfer:cliRequest`
  `{kind, requestId, text?}` and waits up to 30 s for `settingsTransfer:cliReply`. The renderer
  runs the same `buildSettingsExportText()` and `applySettingsImport()` as the GUI.
- With no open window the error is "Orca has no open window to export or import settings in."
- A successful import reloads the window: `window.location.reload()`.
- The summary comes from `describeSettingsBundle`. Example:
  `156 settings, 35 layout preferences, automations, settings of 1 projects, keybindings, 1 editor themes`.
  "automations" appears when `ui.workspaceStatusRules` is present.

Backup scripts are built on these two commands: a backup runs
`orca settings export --output $TMP/orca-settings.json` and stores the file somewhere safe; a
restore fetches it, launches Orca if needed, waits for the window, then runs
`orca settings import --file …`. The reference setup has such a script (see
`private/reference-setup.md`). **A Rust Orca must provide the same two CLI commands, flags and file
format for such scripts to keep working.**

---

## 2. Import semantics: merge vs replace

All from `applySettingsImport`. The order matters:

1. **Parse and filter again** (`parseSettingsTransfer`). The file is untrusted: settings keys are
   re-checked against the private-name filter, UI keys against the allowlist, and project fields
   through each field's parser. A file someone edited by hand therefore cannot plant a token, a path
   or another machine's ids.
2. **Status rules** (`ui.workspaceStatusRules`, if present) go through
   `mergeImportedStatusRules(currentStoreConfig, imported, Date.now())`:
   ```
   { ...current, ...imported,
     repoIds: current.repoIds,
     handledPullRequests: current.handledPullRequests,
     ledgerVersion: current.ledgerVersion,
     newWorkspaceStatusSince: imported.newWorkspaceStatus ? now : current.newWorkspaceStatusSince }
   ```
   - This is a **shallow** merge at the rule-config level. A rule field missing from the file keeps
     the importer's current value. For example, a file from a build older than §83 has no
     `holdStatus`, so the importer's own value survives.
   - A field that is present is replaced whole. That includes the nested objects `statusByCondition`,
     `actionPrompts` and `reviewInbox`.
   - `repoIds`, the PR ledger and `ledgerVersion` always stay local.
   - A truthy imported `newWorkspaceStatus` restamps `newWorkspaceStatusSince` to the import time, so
     only workspaces created after the import move into that column.
3. **Settings** go through `mergeImportedSettings(currentSettings, bundle.settings)`, which returns
   **only the imported keys**. Each value is deep-merged onto the importer's current value: objects
   are merged recursively, while arrays and scalars replace. That keeps the importer's private
   nested fields, such as `voice.modelsDir`, `voice.microphoneDeviceId` and
   `notifications.customSoundPath`.
   - Then `terminalQuickCommands` is run through `resolveImportedQuickCommands`. A command scoped
     `{type:"project", remote}` becomes `{type:"repo", repoId}` for the **first** local repo whose
     `gitRemoteIdentity.canonicalKey === remote`, or is dropped when none matches.
   - The result goes to `settings:set`, which is a partial update: main does
     `{...stored, ...sanitized(update)}`, with notifications shallow-merged again. **Settings keys
     not in the file are left untouched.** Main's own sanitizers still apply.
4. **UI**: `ui:set(ui)` is a partial update, one top-level key at a time. A present key replaces the
   stored one (`workspaceStatuses` is replaced as a whole array). Keys not in the file are untouched.
   Main normalizes on write: `normalizeWorkspaceStatuses`, `normalizeWorkspaceStatusRuleConfig`,
   `normalizeGroupBy`, `normalizeSortBy` and the rest.
5. **Projects**: for each `projects[]` entry, **every** local repo, on every execution host, whose
   `gitRemoteIdentity.canonicalKey === entry.remote` gets
   `updateRepo(repo.id, entry.settings, {hostId})`.
   - This is a per-field replace: the fields present overwrite, and the absent ones stay.
   - An entry with no matching local repo is silently ignored.
   - Repo ids, paths and display names are never imported. `name` is informational only.
6. **Files**:
   - When `keybindings` is not `null` and is valid JSON, it **replaces** `~/.orca/keybindings.json`
     (creating `~/.orca/`).
   - `null` leaves the existing file alone.
   - Each editor theme that is present and valid JSON replaces its file in the theme directory. An
     absent theme is left alone.
   - Then `keybindings.reload()` runs.
7. **Reload** the window. Every stored value goes through the load-time hydration sanitizers again.

The GUI's confirm text says "This replaces your …". In practice this is an **overlay**:

- nothing is deleted;
- settings and UI keys absent from the file survive;
- status-rule fields absent from the file survive;
- local project bookkeeping survives.

### 2.1 Project matching (`remote`)

`remote` is `Repo.gitRemoteIdentity.canonicalKey`, computed by `deriveGitRemoteIdentity` from
`git remote -v` output:

1. Parse the `(fetch)` lines.
2. Normalize each URL:
   - scp-style `[user@]host:path` and URL-style both become `lowercase(host) + "/" + path`;
   - leading and trailing slashes are stripped from the path, and so is a trailing `.git`;
   - the path's case is **kept**;
   - for URL-style, `URL.hostname` drops the port;
   - Windows drive paths and empty URLs give `null`.
3. Pick one remote: `upstream` first, then `origin`, then the rest by name (`localeCompare 'en'`).

Example: `git@github.com:acme/widgets.git` gives `github.com/acme/widgets`. Matching
on import is **exact string equality**. No alias or host normalization is applied here, unlike
`splitGitRemoteKey`.

On export, projects are deduplicated by `remote`, first repo wins. Folder workspaces and repos with no
remote are skipped, and so is a repo with no portable field set. `name` is that repo's `displayName`.

**A Rust implementation must reproduce `normalizeGitRemoteUrl` and the remote priority exactly.
Otherwise project settings and project-scoped quick commands land nowhere.**

---

## 3. Classification: how exports cannot drift

There are three mechanisms, one per section.

### 3.1 `settings`: a denylist plus name patterns, applied at every nesting level

`isPortableSettingKey(key)` is false when the key is in `PRIVATE_SETTING_KEYS` or matches any of
`PRIVATE_SETTING_PATTERNS`, all case-insensitive:

```
PRIVATE_SETTING_KEYS = agentCmdOverrides, agentDefaultEnv, githubProjects, defaultRepoSelection,
  defaultLinearTeamSelection, openInApplications, devPluginPaths, disabledPlugins, minimaxUsageModels

PRIVATE_SETTING_PATTERNS = /account/ /token/ /cookie/ /apikey/ /secret/ /password/ /path/ /cwd/
  /machine/ /proxy/ /runtime/ /wsl/ /udid/ /telemetry/ /pairing/ /consent/ /endpoint/ /groupid/
  /workspaceid/ /environment/ /shell/ /migrated/ /migration/ /defaulted/ /seed/ /prompted/
  /dismissed/ /dir$/ /dirhistory/ /device/ /repoid/ /hostid/ /authority/ /deliverygate/
  /parking/ /retentionbudget/ /http1/
```

- At the top level, every key **not** private is exported. This is a denylist, so new settings
  travel by default.
- `scrubPrivateFields` applies the same test **recursively to every nested object key**, including
  objects inside arrays. Examples:
  - `voice.modelsDir` is dropped (`/dir$/`), as are `voice.openAiApiKeyConfigured` (`/apikey/`) and
    `voice.microphoneDeviceId` (`/device/`).
  - `notifications.customSoundPath` is dropped (`/path/`).
  - `linters.*.path` and `formatters.*.path` are dropped.
- Special case: top-level `terminalQuickCommands`. Before scrubbing, each command with
  `scope.repoId` is rewritten to `scope: {type:"project", remote: <canonicalKey>}`, or dropped when
  its repo has no remote.
  - On import the parser scrubs again. The nested key `repoId` matches `/repoid/`, so a planted
    `{type:"repo", repoId}` scope loses its `repoId`. Then `scope.type === "repo"` no longer
    resolves, and the test shows such a command is **dropped**.
- Quirk: `hostSettingOverrides` is exported, because no pattern matches it. Its inner keys are
  execution-host ids (`local`, `ssh:<id>`, `runtime:<id>`) and are not scrubbed. The reference
  export does not contain it.

### 3.2 `ui`: a typed allowlist (`UI_STATE_EXPORT: Record<keyof PersistedUIState, boolean>`)

Every `PersistedUIState` key must be listed, so a new key fails typecheck until it is classified.
Only `true` keys are exported. A key is copied only if it is present (`key in source`), so a key
whose value is `undefined` is absent from the JSON. `workspaceStatusRules` also has `repoIds`,
`handledPullRequests`, `ledgerVersion` and `newWorkspaceStatusSince` removed.

| UI key | Exported | Type / values | Default (fresh profile) |
|---|---|---|---|
| `groupBy` | yes | `'none' \| 'workspace-status' \| 'repo' \| 'pr-status' \| 'stack'` | `"repo"` |
| `sortBy` | yes | `'name' \| 'smart' \| 'recent' \| 'repo' \| 'manual'` | `"recent"` |
| `projectOrderBy` | yes | `'manual' \| 'recent'` | `"manual"` |
| `stackOrder` | yes | `'asc' \| 'desc'`; absent means `desc` | absent |
| `showActiveOnly` | yes | bool; deprecated, ignored on hydration | `false` |
| `hideSleepingWorkspaces` | yes | bool | `false` |
| `showSleepingWorkspaces` | yes | bool; deprecated legacy | `true` |
| `hideDefaultBranchWorkspace` | yes | bool | `false` |
| `hideAutomationGeneratedWorkspaces` | yes | bool | `false` |
| `hideCliCreatedWorkspaces` | yes | bool | `false` |
| `hideDetachedHeadWorkspaces` | yes | bool | `false` |
| `hideTemporaryCheckoutWorkspaces` | yes | bool (fork §9) | absent |
| `hideWorkspacesFromOtherDevices` | yes | bool | `false` |
| `alwaysShowDefaultBranchWorkspace` | yes | bool; absent means on | `true` |
| `agentsHideWorkspacesFromOtherDevices` | yes | bool | `false` |
| `agentsHideAutomationGeneratedWorkspaces` | yes | bool | `false` |
| `agentsHideCliCreatedWorkspaces` | yes | bool | `false` |
| `agentsShowChildAgents` | yes | bool | `false` |
| `agentsCompactMode` | yes | bool | `true` |
| `agentsShowSearch` | yes | bool | `true` |
| `agentsReadFilter` | yes | `'all' \| 'unread'` | `"all"` |
| `agentsGroupBy` | yes | `'none' \| 'status' \| 'project' \| 'worktree' \| 'agent'` | `"status"` |
| `uiZoomLevel` | yes | number | `0` |
| `editorFontZoomLevel` | yes | number | `0` |
| `worktreeCardProperties` | yes | `WorktreeCardProperty[]`: `status, unread, ci, branch, issue, linear-issue, linear-title, pr-author, stack, pr-diff, project, workspace-status, jira-issue, pr, automation, cli, comment, ports, inline-agents, host` | `["status","unread","issue","linear-issue","jira-issue","pr","automation","cli","comment","ports","inline-agents","host","project"]` |
| `agentActivityDisplayMode` | yes | `'compact' \| 'full'` | `"compact"` |
| `workspaceStatuses` | yes | `WorkspaceStatusDefinition[]` (§4.1) | Todo / In progress / In review / Done |
| `workspaceStatusRules` | yes, minus 4 local keys | `WorkspaceStatusRuleConfig` (§4) | disabled defaults |
| `workspaceBoardOpacity` | yes | number, clamped to 0.2–1 and rounded to 0.01 | `1` |
| `workspaceBoardColumnWidth` | yes | integer, clamped to 220–520 (UI step 20) | `308` |
| `syncTaskStatusFromWorkspaceBoard` | yes | bool | `false` |
| `statusBarItems` | yes | `StatusBarItem[]`: `claude, codex, gemini, antigravity, opencode-go, kimi, minimax, grok, cursor, zcode, ssh, resource-usage, ports` | all 13, in that order |
| `statusBarVisible` | yes | bool | `true` |
| `usagePercentageDisplay` | yes | `'used' \| 'remaining'` | `"used"` |
| `statusBarUsageMode` | yes | `'verbose' \| 'compact'` | `"compact"` |
| `browserDefaultUrl` | yes | string or null | absent |
| `browserDefaultSearchEngine` | yes | `'google' \| 'duckduckgo' \| 'bing' \| 'kagi' \| null` | absent |
| `browserDefaultZoomLevel` | yes | number | `0` |
| `browserDevToolsDock` | yes | `BrowserDevToolsDockLayout` | absent |
| `petVisible`, `petId`, `petSize` | yes | bool, string, number | absent |
| **all other keys** | **no** | window geometry, sidebar widths, last active repo or worktree, host scopes and filters (`filterRepoIds`, `agentsFilterRepoIds`, `visibleWorkspaceHostIds`, …), `collapsedGroups`, `manualRepoOrder`, Jira filter state, every `_…Migrated/_…Defaulted/_…Added` flag, update/notice/nag/tour/tip bookkeeping, `trustedOrcaHooks`, `setupScriptPromptDismissedRepoIds`, `customPets`, legacy `sidekick*`, `taskResumeState`, `workspaceCleanup`, `browserKagiSessionLink` (a secret), the per-pane read state | — |

The full list of 145 keys and their booleans is in `settings-transfer-ui-fields.ts`. A Rust port
should copy that table verbatim.

### 3.3 `projects[].settings`: a typed parser map (`REPO_FIELDS satisfies {[K in keyof Repo]-?: parser | null}`)

Each `Repo` field maps either to a parser (exported, and re-parsed on import) or to `null` (local).
A parser returning `undefined` drops the field.

| Repo field | Transfer | Parser rule |
|---|---|---|
| `worktreeBaseRef` | exported | a string that is non-empty after trim; the value is kept untrimmed |
| `hookSettings` | exported | needs an object with an object `scripts`. `mode`: `'override'`, else `'auto'`. `setupRunPolicy`: `ask \| run-by-default \| skip-by-default`. `setupAgentStartupPolicy`: `start-immediately \| wait-for-setup`. `commandSourcePolicy`: `shared-only \| local-only \| run-both`. `scripts.setup` and `scripts.archive`: strings, default `""` |
| `issueSourcePreference` | exported | `upstream \| origin \| auto` |
| `defaultJiraProject` | exported | `{id,key,name,siteId}`; `null` becomes absent |
| `forkSyncMode` | exported | `ask \| safe-auto \| off` |
| `externalWorktreeVisibility` | exported | `hide \| show` |
| `agentWorktreeVisibility` | exported | `hide \| show` |
| `worktreeVisibilitySourcePreferences` | exported | only `{builtIn: {claude?, gsd?: hide\|show}}`; `custom` is dropped |
| `symlinkPaths` | exported | array of relative paths only. Entries starting with a drive letter, `/`, `\` or `~`, or containing a `..` segment, are dropped |
| `pythonVenvPath` | exported | the same relative-path rule |
| `portLabelsCommand` | exported | a non-empty string |
| `ciTriage` | exported | `parseCiTriageConfig`: `{}` when the instructions equal the built-in default, else `{prompt}`. A legacy `hints` is folded into `prompt` as `default + "\n\nAbout this repository:\n" + hints`. Any non-null object, even `{}`, **turns CI triage on** for the repo |
| `sourceControlAi` | exported | `normalizeRepoSourceControlAiOverrides`, which keeps `enabled`, `customAgentCommand`, `modelOverridesByOperation`, `instructionsByOperation`, `actionOverrides` and `prCreationDefaults`, keyed by known action ids |
| `id`, `path`, `displayName`, `badgeColor`, `repoIcon`, `upstream`, `addedAt`, `kind`, `folderUpgradeGitRootPath`, `gitUsername`, `worktreeBasePath`, `connectionId`, `executionHostId`, `ghAccount`, `gitRemoteIdentity`, `externalWorktreeVisibilityLegacy`, `externalWorktreeVisibilityPromptDismissedAt`, `externalWorktreeInboxBaselinePaths`, `importedExternalWorktreePaths`, `customWorktreeVisibilitySources`, `externalWorktreeDiscoverySuppressedAt`, `projectGroupId`, `projectGroupOrder`, `projectHostSetupMethod`, `orcadRetained` | local | machine paths, hosts, accounts, bookkeeping |

---

## 4. Status automations (`ui.workspaceStatusRules`): exact semantics

### 4.1 Columns: `ui.workspaceStatuses`

`WorkspaceStatusDefinition = { id: string, label: string, color?: string, icon?: string }`. The
array order is the board's column order. Normalization in `workspace-statuses.ts`:

- `id`: lowercased, trimmed, `[^a-z0-9_-]+` replaced by `-`, edge dashes stripped. When empty, it is
  a slug of the label.
- `label`: whitespace collapsed, at most 32 UTF-16 code units, fallback `Status N`.
- `color`: one of `neutral blue sky violet amber emerald rose zinc conductor-done conductor-review
  conductor-progress`. Otherwise it is the default visual for a built-in id, else a palette entry by
  index.
- `icon`: one of `circle circle-dot circle-progress circle-dashed circle-ellipsis git-pull-request
  timer flag circle-alert circle-pause circle-play circle-check ban conductor-done conductor-review
  conductor-progress`.
- The default column is `in-progress` when present, else the first column.
- New column ids are generated as `status-N` (and `status-N-2` on a clash), which is why real
  exports carry ids like `status-5` or `status-9-2` whose labels are unrelated.
- Rule targets refer to column **ids**, so the ids must survive verbatim. The rules do not check
  that an id exists among the columns.

### 4.2 `WorkspaceStatusRuleConfig` fields

Defaults come from `cloneDefaultWorkspaceStatusRuleConfig()`. Normalization comes from
`normalizeWorkspaceStatusRuleConfig`, which runs on every load, read and write.

| Key | Type | Default | Transfer | Normalization | Meaning / consumer |
|---|---|---|---|---|---|
| `enabled` | bool | `false` | exported | `=== true` | Master switch. `buildWorkspaceStatusRulePlan` returns an empty plan when false. |
| `repoIds` | string[] | `[]` | **local** | non-empty strings, deduped | Projects the rules act on. **Empty means the rules do nothing.** Not exported, so after a restore on a new machine the projects must be picked again. |
| `statusByCondition` | `{[condition]: columnId}` | `{}` | exported | only known conditions, non-empty trimmed strings, rebuilt in condition order | Column for each PR condition. An **unmapped condition never moves a card.** |
| `mergingCheckName` | string | `"Reviews satisfied"` | exported | trimmed, at most 120 characters; empty becomes the default | The CI check or commit-status name whose `success` means "merging" (the approval gate). Compared case-insensitively after trim. |
| `newWorkspaceStatus` | string \| null | `null` | exported | trimmed; empty becomes `null` | "New workspaces start in". A workspace Orca creates in a scoped project with **no column yet** goes here, but only if `createdAt >= newWorkspaceStatusSince` (`planNewWorkspaceStatusUpdates`). |
| `newWorkspaceStatusSince` | number (epoch ms) \| null | `null` | **local**, restamped to import time when the import sets a column | finite number, else `null`; forced to `null` when `newWorkspaceStatus` is null | Keeps the board from reshuffling when the column is chosen. |
| `holdStatus` | string \| null | `null` | exported | trimmed; empty becomes `null` | Hold column (§83). A workspace in it is skipped by **every** rule: no moves, no delete on merge, close or review, so no conflict agent, no review command and no head sync. It still counts as linked, so the inbox never re-clones it. |
| `pmApprovalTeam` | string \| null | `null` | exported | trimmed, at most 120 characters; empty becomes `null` | `org/team-slug`. Parsed by splitting on `/`, both halves required. The members' logins feed `pm-approval` and the second half of `merging`. |
| `onResolved` | `'delete' \| 'none'` | `'none'` | exported | anything except `'delete'` becomes `'none'` | Delete the workspace once **your** PR is merged or closed. Not forced, so git refuses dirty work or a live agent. Only when `target.headOid === pr.headRefOid`. |
| `onReviewed` | `'delete' \| 'none'` | `'none'` | exported | same as `onResolved` | Delete a **review** workspace for someone else's PR once you approved or requested changes (`readViewerReviewStanding.isFinished`), or once the request was withdrawn before you reviewed (`wasUnassigned`, needs `reviewInbox.enabled`). The delete is **forced**, but is skipped while unsent pending review comments exist. Forgets the ledger key when the PR is no longer in `review-requested:@me`. |
| `resolveConflictsWithAgent` | bool | `false` | exported | `=== true` | Launch an agent with the conflict prompt when your PR enters the `conflicts` column. |
| `conflictPromptTemplate` | string | `""` | exported | at most 8000 characters, **not trimmed** | Conflict agent prompt. When empty after trim, the built-in `buildConflictAgentPrompt` is used. Variables: §4.5. |
| `reviewEnteredCommand` | string | `""` | exported | trimmed, at most 1000 characters | Shell command run once, headless with `bash` and the app env, in a local workspace entering the column mapped from `review`. Empty means nothing runs. |
| `actionPrompts` | `{commitAndPush?, createPullRequest?, readyForReview?, reviewPullRequest?}` | `{}` | exported | known ids only, non-blank strings, each at most 8000 characters, **not trimmed** | Prompts for the top-bar buttons (§13). A missing or blank one falls back to `DEFAULT_WORKSPACE_ACTION_PROMPTS[id]`. Variables: §4.5. |
| `reviewInbox.enabled` | bool | `false` | exported | `=== true` | Auto-create a review workspace for each PR in `review-requested:@me`, at most 30 per tick, one at a time. Skipped when the PR is in the ledger, already linked to a workspace, authored by you, or when its head branch already has a workspace. |
| `reviewInbox.agent` | `TuiAgent` | `"claude"` | exported | must be a known agent id, else `"claude"` | Agent launched in the review workspace. It is also the agent for the top-bar action buttons. |
| `reviewInbox.view` | `'chat' \| 'terminal' \| 'claude-web' \| 'default'` | `"chat"` | exported | unknown becomes `"chat"` | How the review agent's tab opens. `default` follows the app default. |
| `reviewInbox.promptTemplate` | string | `DEFAULT_REVIEW_PROMPT_TEMPLATE` | exported | blank becomes the default; otherwise verbatim | First-look review prompt. |
| `reviewInbox.rereviewPromptTemplate` | string | `DEFAULT_REREVIEW_PROMPT_TEMPLATE` | exported | blank becomes the default | Used when a re-request follows a review you already left. The diff is pointed at `{{sinceCommit}}`. |
| `handledPullRequests` | string[] | `[]` | **local** | kept only when `ledgerVersion === 1`, else reset to `[]`. Deduped, newest 500 kept | Inbox ledger. Keys are `owner/repo#N` or `owner/repo#N@<reviewedCommitSha>`. |
| `ledgerVersion` | number | `1` | **local** | always written as `1` | Ledger schema version. |

### 4.3 Conditions: `WorkspaceStatusRuleCondition`, in exact evaluation order

`resolveWorkspaceStatusRuleCondition(pr, config, pmTeamLogins, viewerLogin)` returns the first
condition that matches:

1. `merged`: `pr.state === 'MERGED'`.
2. `closed`: `pr.state === 'CLOSED'`.
3. `reviewing`: a viewer login is known and the PR author differs from it, case-insensitive.
   Someone else's PR is always `reviewing` while open.
4. `merge-queue`: `pr.isInMergeQueue === true`, **and only if `statusByCondition['merge-queue']` is
   mapped**. Otherwise evaluation falls through.
5. `conflicts`: only if `statusByCondition.conflicts` is mapped.
   - `mergeable === 'CONFLICTING'` gives `conflicts`.
   - `mergeable === 'UNKNOWN'` returns **`null`**, so nothing changes this tick and the card does not
     bounce.
   - This step comes before `draft`, so a conflicting draft lands in Conflicts.
6. `draft`: `pr.isDraft`.
7. `changes-requested`: some review in `latestOpinionatedReviews` (falling back to `latestReviews`)
   has state `CHANGES_REQUESTED`, and its author is **not** currently a pending requested user
   reviewer. A re-request clears it.
8. `merging`: either of these holds:
   - a check named `mergingCheckName` (trim + lowercase equality) has state `success`;
   - PM approval is satisfied: the PM team is non-empty, no pending user reviewers remain, and every
     opinionated non-author verdict is `APPROVED`, with at least one, and at least one from a PM
     team member.
9. `pm-approval`: the PM team is non-empty, there is at least one pending user reviewer, and **all**
   pending user reviewers are PM team members.
10. `review`: everything else.

The outcome is `status = statusByCondition[condition] ?? null`. The planner then works through the
targets that are linked, have their PR resolved and are **not** held:

- `merged` or `closed`, with `onResolved='delete'` and a head OID match: **remove**, unforced.
- `reviewing`, with `onReviewed='delete'`, no pending review comments, and either a finished verdict
  or `wasUnassigned`: **remove**, forced, possibly forgetting the ledger key.
- Otherwise, when a status is mapped and differs from the current one: **status update**.

The inbox creations come after that (see `reviewInbox.enabled`). Side effects that run when a card
moves:

- **Conflict agent**: entering the `conflicts` column with `resolveConflictsWithAgent`.
- **Review command**: entering the `review` column with a non-empty `reviewEnteredCommand`.
- **Head sync**: a review workspace is fast-forwarded to the PR head. This is unconditional
  behaviour, not configurable.

PR data comes from the fork's single GraphQL snapshot query (FORK §3). Fields: `state`, `isDraft`,
`mergeable`, `isInMergeQueue`, `author`, `requestedReviewers` (users and teams), `latestReviews`,
`latestOpinionatedReviews` (both with `commit.oid`), `viewerLatestReview`, `checks` (check runs and
commit statuses, by name), `headRefOid`, `headRefName`, `baseRefName`, `url` and `title`, plus
`viewerLogin` and the PM team's member logins.

Dev builds do not poll unless `localStorage['orca.devStatusRules'] = '1'`. A Rust build should
likewise keep a second instance from acting on the same GitHub account.

### 4.4 CI triage (per project, `projects[].settings.ciTriage`)

- The presence of `ciTriage` on a repo turns triage on. For each failed run (`failure` or
  `timed_out`) of a red PR **you** wrote, a background agent starts with:
  `"CI failed on pull request #N at <sha>. Failed checks:\n- name: url\n…\n\n" + instructions`.
- `instructions` is `ciTriage.prompt`, else the built-in default, else the default with legacy hints
  appended.
- A combined status without a link never starts an agent on its own.
- Memory of what was triaged lives in localStorage for 7 days, with a cap of 3 per PR, so it is not
  part of the export.
- Decisions are logged to `<userData>/logs/ci-triage.jsonl` (§64).

### 4.5 Template variables

Rendering in `workspace-status-rule-prompt.ts`:

- The pattern is `{{\s*([a-zA-Z]+)\s*}}`.
- **An unknown or missing variable is left verbatim.**
- Variables for the review, re-review and conflict prompts: `prNumber`, `title`, `author` (default
  `unknown`), `branch` (head), `baseRef`, `url`, `repo` (`owner/repo`), `sinceCommit` (the reviewed
  commit, else `baseRef`) and `worktreePath`.
- For the conflict prompt, `baseRef` and `worktreePath` are required.

Action prompts are rendered by `renderWorkspaceActionPrompt`:

- The pattern is `{{(\w+)}}`, with no whitespace allowed, and unknown names are left verbatim.
- Afterwards `\n{3,}` is collapsed to `\n\n` and the result is trimmed.
- Variables: `branch`, `baseRef`, `worktreePath`, `prNumber`, `prUrl`, `prTitle`, `draftMode`,
  `prInstructions` and `templateRule`.

---

## 5. What a real configuration exercises

The reference setup's actual values (columns, rule mapping, prompts, project settings, quick
commands, keybindings, changed settings) are recorded in `private/reference-setup.md`, which is not
in the repo; the private overlay settings file is built from it. This section only lists which
parts of the format a real, heavily customized export uses, so an importer is tested against all
of them.

### 5.1 Status automations (`ui.workspaceStatusRules`): illustrative shape

```jsonc
{
  "enabled": true,
  "statusByCondition": {
    "reviewing":         "status-2",   // e.g. column "Their PRs"
    "merge-queue":       "status-3-2", // generated id, e.g. "In queue"
    "conflicts":         "status-1",
    "draft":             "todo",
    "changes-requested": "status-4",
    "merging":           "status-3",
    "pm-approval":       "completed",  // built-in id reused for an unrelated label
    "review":            "in-review"
    // merged, closed: unmapped (no move; handled by onResolved)
  },
  "holdStatus": "status-9",                      // hold column: every rule skips it
  "mergingCheckName": "Reviews satisfied",       // = default
  "newWorkspaceStatus": "todo",
  "pmApprovalTeam": "acme/pm-team",
  "onResolved": "delete",
  "onReviewed": "delete",
  "resolveConflictsWithAgent": true,
  "conflictPromptTemplate": "<custom, several KB, stack-aware>",
  "reviewEnteredCommand": "make dev-down",
  "actionPrompts": {                              // any of the four may be customized
    "commitAndPush":     "<custom>",
    "createPullRequest": "<custom>",
    "readyForReview":    "<custom>",
    "reviewPullRequest": "<custom>"
  },
  "reviewInbox": {
    "enabled": true,
    "agent": "claude",
    "view": "chat",
    "promptTemplate":         "<custom>",
    "rereviewPromptTemplate": "<custom>"
  }
  // An export from a build older than commit e5eed6a944 (§83) has no holdStatus key; treat absent as null.
  // repoIds, handledPullRequests, ledgerVersion, newWorkspaceStatusSince: stripped (local).
}
```

Prompt strings in real exports are long (up to several KB) and contain non-ASCII characters such as
`—` and `’`. Every prompt string must survive **byte for byte**.

`ui.workspaceStatuses` sets the board order. A real board has around ten custom columns, mixing
built-in ids (`todo`, `in-progress`, `in-review`, `completed`) relabelled to unrelated names with
generated ids (`status-N`, `status-N-2`). The default column (`in-progress` when present) is the
fallback for workspaces with no status, and usually no rule maps to it. One column may be the hold
column.

Column ids are generated and are not the same as the labels. An importer must keep the ids, because
rules point at them.

### 5.2 Project settings (`projects[].settings`): illustrative shape

A real work project sets most portable repo fields at once:

```jsonc
{
  "hookSettings": {
    "mode": "auto", "setupRunPolicy": "run-by-default", "setupAgentStartupPolicy": "wait-for-setup",
    "scripts": {
      "setup":   "make worktree-setup",
      "archive": "make worktree-archive"
    }
  },
  "externalWorktreeVisibility": "show",
  "portLabelsCommand": "make print-urls",
  "ciTriage": { "prompt": "<custom, a few KB>" },
  "sourceControlAi": {
    "actionOverrides": {
      "resolveConflicts": { "agentId": "claude", "agentArgs": "", "commandInputTemplate": "<custom guidance>\n\n{basePrompt}" },
      "resolveComments":  { "agentId": "claude", "agentArgs": "", "commandInputTemplate": "<custom guidance>\n\n{basePrompt}" }
    },
    "prCreationDefaults": { "draft": true, "generateDetailsOnOpen": true }
  }
}
```

A second, lightly configured project may set only `hookSettings` with empty scripts.

### 5.3 UI preferences a real setup changes

Typically changed: `groupBy` (e.g. `"workspace-status"`), `stackOrder`, `sortBy`,
`hideTemporaryCheckoutWorkspaces`, `worktreeCardProperties` (a reordered subset of §3.2's values),
`workspaceStatuses`, `workspaceStatusRules`, `statusBarItems` (reordered) and `statusBarUsageMode`.
Keys can be absent from a real export (for example `browserDefaultUrl`, `browserDevToolsDock` and
the `pet*` keys) and must then be left alone on import.

### 5.4 Settings a real setup changes

The reference export changes about 30 keys (marked in the **Differs** column of §6). Notable shapes:

- nested objects partly changed (`notifications`, `voice`, `commitMessageAi`, `sourceControlAi`),
  which exercise the deep merge;
- `terminalCustomThemes` holding an imported theme with 16 ANSI colours, referenced by id from
  `terminalThemeDark`;
- `agentDefaultArgs` with every agent set to `""`, which differs from `{}` only by having the keys;
- `sourceControlSectionLayout` (fork §36) and `languageServers` (fork §26);
- `experimentalWorktreeSymlinks`: a stale key no longer in the type, still carried along. This is
  the reason to preserve unknown keys.

`terminalQuickCommands` can hold many commands scoped to one project (`{type:"project", remote}`),
all `action: "terminal-command"` with `appendEnter: true`. They exercise
`resolveImportedQuickCommands`.

Keybindings hold platform overrides, some of which rebind chords the fork uses by default.
`editor-dark.json` can be a large VS Code theme (tens of KB, fork §5).

### 5.5 What a restore on a fresh machine does not bring back

- The status-rule `repoIds`: the rules are inert until projects are picked again.
- The PR ledger: the review inbox clones every outstanding request again on its first tick. That is
  by design.
- Project settings for a project that is not added yet, or whose canonical remote differs.
- Settings matched by the private patterns: accounts, `workspaceDir`, shell, proxy, and the like.

---

## 6. Full settings field table

Generated from `GlobalSettings` (`global-settings-types.ts`, `fork-source-control-settings.ts` and
the `GlobalSettingsBase` feature groups), the defaults from `getDefaultSettings('/HOME')` on macOS,
and `isPortableSettingKey`.

- **Transfer** gives the classification and the rule that decides it.
- **Differs** is set when the reference export's value is not the portable default (the values
  themselves are in `private/reference-setup.md`). It only shows which keys real users change.
- A `?` before the type marks an optional key.
- `_(absent)_` means the key is not in the default object, so readers apply the documented fallback.
- Defaults that depend on the platform: `terminalFontFamily`, `terminalRightClickToPaste`,
  `primarySelectionMiddleClickPaste`.
- Feature consumers follow the key prefix: `terminal*` is the terminal, `editor*` the editor,
  `nativeChat*` structured chat, `sourceControl*` the SC panel and AI, `orchestration*` agent
  orchestration, `experimental*` feature flags, `skip*Confirm` dialogs, `show*Button` the sidebar.

| Key | Type | Default | Transfer | Differs | Meaning (source comment) |
|---|---|---|---|---|---|
| `activeClaudeManagedAccountId` | `string \| null` | null | excluded: pattern /account/i |  |  |
| `activeClaudeManagedAccountIdsByRuntime` | `?ClaudeManagedAccountRuntimeSelection` | _(absent)_ | excluded: pattern /account/i |  |  |
| `activeCodexManagedAccountId` | `string \| null` | null | excluded: pattern /account/i |  |  |
| `activeCodexManagedAccountIdsByRuntime` | `?CodexManagedAccountRuntimeSelection` | {"host":null,"wsl":{}} | excluded: pattern /account/i |  |  |
| `activeRuntimeEnvironmentId` | `?string \| null` | null | excluded: pattern /runtime/i |  | Active non-local runtime environment for client-routed RPC; null keeps local desktop behavior. |
| `agentCmdOverrides` | `Partial<Record<TuiAgent, string>>` | {} | excluded: key denylist |  | Per-agent CLI command overrides. A missing key means use the catalog default binary name. |
| `agentDefaultArgs` | `?Partial<Record<TuiAgent, string>>` | {} | exported | yes | Per-agent default CLI arguments appended after the binary/path and before prompts. |
| `agentDefaultEnv` | `?Partial<Record<TuiAgent, Record<string, string>>>` | {} | excluded: key denylist |  | Per-agent launch environment defaults used when yolo mode is exposed as env. |
| `agentHibernationIdleMs` | `?number` | 1800000 | exported |  | Milliseconds a completed agent must stay idle before hibernation can be considered. |
| `agentPermissionDefault` | `?AgentPermissionChoice` | "full-access" | exported |  | Every agent's permission in terminals and new chats, unless it has its own below. |
| `agentPermissionOverrides` | `?Partial<Record<TuiAgent, AgentPermissionChoice>>` | {} | exported | yes | Agents given their own permission instead of the all-agents one. |
| `agentSkillSharingEnabled` | `?boolean` | false | exported |  | Capability gate for agent/CLI skill publishing. |
| `agentStateRulesLiveUpdates` | `?boolean` | _(absent)_ | exported |  | Off ignores downloads/cache and uses bundled rules or local override; absent means on. |
| `agentStateRulesPath` | `?string \| null` | _(absent)_ | excluded: pattern /path/i |  | Local agent-state-rules.json replaces downloaded/bundled rules for testing. |
| `agentStatusHooksEnabled` | `boolean` | true | exported |  | Disabling must persist so startup doesn't reinstall global agent hook entries. |
| `agentWorkspaceTrustEnabled` | `boolean` | true | exported |  | Pre-trust the worktree or folder Orca starts an agent in. |
| `agentYoloDefaultsMigrated` | `?boolean` | true | excluded: pattern /migrated/i |  | One-shot guard. |
| `agentsSidebarIntroShown` | `?boolean` | _(absent)_ | exported |  | Set after the one-time legacy Agents tab introduction has been acknowledged. |
| `agentsSidebarMigratedFromExperimental` | `?boolean` | _(absent)_ | excluded: pattern /migrated/i |  | True when the profile previously opted into the legacy Agents view. |
| `aiVaultSearch` | `?AiVaultSearchSettings` | _(absent)_ | exported |  | Transcript full-text search consent + retention. Absent means off. |
| `alwaysForceDeletePreservedBranches` | `?boolean` | false | exported |  | Opt-in: delete preserved local branches after workspace removal. |
| `alwaysForceDeleteWorktrees` | `?boolean` | false | exported |  | Opt-in: workspace deletion discards changes. |
| `androidSdkPath` | `?string \| null` | null | excluded: pattern /path/i |  | Explicit Android SDK root. |
| `appFontFamily` | `string` | "Geist" | exported |  |  |
| `appIcon` | `AppIconId` | "classic" | exported |  |  |
| `artifactSharingEnabled` | `?boolean` | false | exported |  | Capability gate for agent-driven publishing. |
| `artifactsEnabled` | `?boolean` | true | exported |  | Deprecated: Artifacts are always available. |
| `autoRenameBranchFromWork` | `boolean` | true | exported |  | Rename generated creature branches from first-prompt work. |
| `autoRenameBranchFromWorkDefaultedOn` | `?boolean` | true | excluded: pattern /defaulted/i |  | One-shot migration guard. |
| `branchPrefix` | `BranchPrefixStrategy` | "git-username" | exported |  |  |
| `branchPrefixCustom` | `string` | "" | exported |  |  |
| `browserClientHostedRemoteEnabled` | `?boolean` | true | exported |  | Kill switch for paired client-hosted browser placement. |
| `browserGuestWorktreeRetentionBudget` | `?boolean` | true | excluded: pattern /retentionbudget/i |  | Kill switch. |
| `browserProfileScope` | `?BrowserProfileScope` | _(absent)_ | exported |  | Browser profile new tabs in a workspace use; missing = 'shared'. |
| `browserSshWorkspaceRoutingDisabledTargetIds` | `?string[]` | _(absent)_ | exported |  | Per-target opt-outs (SSH target ids; machine-ish but exported). |
| `browserSshWorkspaceRoutingEnabled` | `?boolean` | _(absent)_ | exported |  | Routes SSH-workspace browser pages through the SSH host. |
| `browserSshWorkspaceRoutingProbeSkippedTargetIds` | `?string[]` | _(absent)_ | exported |  | Targets whose forwarding preflight the user overrode. |
| `claudeAccountSwitchRunningSessions` | `?ClaudeAccountSwitchRunningSessions` | _(absent)_ | excluded: pattern /account/i |  | Absent means 'ask'. |
| `claudeAgentTeamsDefaultDisabledMigrated` | `?boolean` | true | excluded: pattern /migrated/i |  | One-shot guard. |
| `claudeAgentTeamsMode` | `?ClaudeAgentTeamsMode` | "off" | exported |  | Experimental Claude Agent Teams. |
| `claudeCopiedSystemDefaultNoticeDismissed` | `?boolean` | _(absent)_ | excluded: pattern /dismissed/i |  |  |
| `claudeManagedAccounts` | `ClaudeManagedAccount[]` | [] | excluded: pattern /account/i |  | Per-account auth. |
| `claudeSubagentNavigationEnabled` | `?boolean` | true | exported |  | Claude agent view subagent navigation. |
| `codexManagedAccounts` | `CodexManagedAccount[]` | [] | excluded: pattern /account/i |  |  |
| `codexSessionSourceHome` | `?{ host?: string; … }` | _(absent)_ | exported |  | Custom CODEX_HOME for Codex session-history discovery. Holds an absolute host path but the key name matches no pattern, so it is exported. |
| `codexSharedServerWarning` | `?boolean` | true | exported |  | Absent reads as on. |
| `codexTerminalServerIsolation` | `?boolean` | true | exported |  | Absent reads as on. |
| `combinedDiffFileTreeVisibleByDefault` | `boolean` | false | exported | yes |  |
| `commitMessageAi` | `?CommitMessageAiSettings` | {"enabled":true,"agentId":null,…} | exported | yes | AI commit-message config. |
| `compactWorktreeCards` | `boolean` | false | exported |  | Compact worktree cards. |
| `computerAwakeMode` | `?ComputerAwakeMode` | _(absent)_ | exported | yes | The legacy boolean maps true to Auto; main rewrites `keepComputerAwakeWhileAgentsRun` alongside it. |
| `confirmClosePinnedTab` | `boolean` | true | exported |  |  |
| `ctrlTabOrderMode` | `?CtrlTabOrderMode` | "mru" | exported |  |  |
| `defaultLinearTeamSelection` | `string[] \| null` | null | excluded: key denylist |  | Linear team ids. |
| `defaultRepoSelection` | `string[] \| null` | null | excluded: key denylist |  | Repo ids. |
| `defaultTaskSource` | `TaskProvider` | "github" | exported | yes |  |
| `defaultTaskViewPreset` | `TaskViewPresetId` | "all" | exported |  |  |
| `defaultTuiAgent` | `TuiAgent \| 'blank' \| null` | null | exported | yes | New-workspace agent. |
| `devPluginPaths` | `string[]` | [] | excluded: pattern /path/i |  |  |
| `diffCollapseUnchangedRegions` | `?boolean` | false | exported |  |  |
| `diffDefaultView` | `'inline' \| 'side-by-side'` | "inline" | exported |  |  |
| `diffShowWhitespace` | `boolean` | false | exported |  |  |
| `diffWordWrap` | `boolean` | false | exported |  |  |
| `disabledPlugins` | `string[]` | [] | excluded: key denylist |  |  |
| `disabledTuiAgents` | `TuiAgent[]` | [] | exported |  |  |
| `dismissedSkillFreshnessNudges` | `?string[]` | _(absent)_ | excluded: pattern /dismissed/i |  |  |
| `editorAutoSave` | `boolean` | false | exported |  |  |
| `editorAutoSaveDelayMs` | `number` | 1000 | exported |  |  |
| `editorFontFamily` | `?string` | "" | exported | yes | Empty follows `terminalFontFamily`. |
| `editorMinimapEnabled` | `boolean` | false | exported |  |  |
| `editorPreviewTabsEnabled` | `boolean` | true | exported |  |  |
| `editorWordWrap` | `?boolean` | true | exported |  |  |
| `electronHttp1CompatibilityMode` | `?boolean` | false | excluded: pattern /http1/i |  |  |
| `experimentalActivity` | `boolean` | false | exported |  | Agents view. |
| `experimentalActivityDefaultedOffForAllUsers` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `experimentalAgentDashboardMode` | `?AgentDashboardMode` | _(absent)_ | exported |  |  |
| `experimentalAgentDashboardPopout` | `?boolean` | _(absent)_ | exported |  |  |
| `experimentalAgentDashboardShowIdle` | `?boolean` | _(absent)_ | exported |  |  |
| `experimentalAgentHibernation` | `?boolean` | false | exported |  |  |
| `experimentalBrowserExtensions` | `?boolean` | false | exported |  |  |
| `experimentalCompactWorktreeCards` | `?boolean` | _(absent)_ | exported |  | Legacy key. |
| `experimentalConnectors` | `?boolean` | false | exported |  |  |
| `experimentalEphemeralVms` | `?boolean` | false | exported |  |  |
| `experimentalMobile` | `boolean` | false | exported |  |  |
| `experimentalNativeChat` | `?boolean` | false | exported |  |  |
| `experimentalNewWorktreeCardStyle` | `?boolean` | false | exported | yes |  |
| `experimentalPet` | `boolean` | false | exported |  |  |
| `experimentalSidekick` | `?boolean` | _(absent)_ | exported |  | Legacy key. |
| `experimentalTerminalAttention` | `boolean` | false | exported |  |  |
| `experimentalWorktreeSymlinks` | _(not in type; stale)_ | _(absent)_ | exported | yes | Stale persisted key. |
| `experimentalWslHosts` | `?boolean` | false | excluded: pattern /wsl/i |  |  |
| `floatingTerminalCwd` | `string` | "~" | excluded: pattern /cwd/i |  |  |
| `floatingTerminalCwdMigratedToAppWorkspace` | `?boolean` | true | excluded: pattern /cwd/i |  |  |
| `floatingTerminalDefaultedForAllUsers` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `floatingTerminalEnabled` | `boolean` | true | exported |  |  |
| `floatingTerminalTriggerLocation` | `'floating-button' \| 'status-bar'` | "floating-button" | exported |  |  |
| `floatingTerminalTrustedCwds` | `?string[]` | [] | excluded: pattern /cwd/i |  | main also refuses it from `settings:set` |
| `followSymlinkedDirectories` | `?boolean` | false | exported |  |  |
| `formatters` | `?FormatterSettingsMap` | _(absent)_ | exported |  | Fork. Nested `path` is scrubbed. |
| `geminiCliOAuthEnabled` | `boolean` | false | exported |  | Opt-in only; holds no credential itself. |
| `gitBlame` | `?{ enabled?: boolean; youForMyCommits?: boolean }` | _(absent)_ | exported |  | Fork §27. |
| `githubProjects` | `?GitHubProjectSettings` | {…} | excluded: key denylist |  |  |
| `gitlabProjects` | `?GitLabProjectSettings` | _(absent)_ | exported |  | Pinned + recent GitLab paths. |
| `hostSettingOverrides` | `?Partial<Record<ExecutionHostId, HostSettingOverrides>>` | _(absent)_ | exported |  | Keyed by host id; see the quirk in §3.1. |
| `httpProxyBypassRules` | `?string` | "" | excluded: pattern /proxy/i |  |  |
| `httpProxyUrl` | `?string` | "" | excluded: pattern /proxy/i |  |  |
| `jiraIssueKeyCasing` | `?JiraIssueKeyCasing` | "lowercase" | exported |  |  |
| `keepComputerAwakeWhileAgentsRun` | `boolean` | false | exported |  |  |
| `keybindings` | `?KeybindingOverrides` | _(absent)_ | exported |  | Legacy in-settings overrides. The real ones are in the `keybindings` file field. |
| `languageServers` | `?LanguageServerSettingsMap` | _(absent)_ | exported | yes | Fork §26. |
| `leftSidebarAppearanceMode` | `LeftSidebarAppearanceMode` | "default" | exported | yes |  |
| `leftSidebarTintColor` | `?string` | "#18181b" | exported |  |  |
| `leftSidebarTintOpacity` | `?number` | 0.08 | exported |  |  |
| `linters` | `?LinterSettingsMap` | _(absent)_ | exported |  | Fork. Nested `path` is scrubbed. |
| `localAccountRuntime` | `'auto' \| 'host' \| 'wsl'` | "auto" | excluded: pattern /account/i |  |  |
| `localAccountRuntimeDefaultedToAutoForAllUsers` | `?boolean` | true | excluded: pattern /account/i |  |  |
| `localAccountWslDistro` | `?string \| null` | null | excluded: pattern /account/i |  |  |
| `localAgentRuntime` | `?'host' \| 'wsl'` | _(absent)_ | excluded: pattern /runtime/i |  |  |
| `localAgentWslDistro` | `?string \| null` | _(absent)_ | excluded: pattern /wsl/i |  |  |
| `localBaseRefSuggestionDismissed` | `boolean` | false | excluded: pattern /dismissed/i |  |  |
| `localWindowsRuntimeDefault` | `GlobalWindowsRuntimeDefault` | {"kind":"windows-host"} | excluded: pattern /runtime/i |  |  |
| `localhostWorktreeLabelsEnabled` | `?boolean` | false | exported |  |  |
| `machineName` | `string` | "" | excluded: pattern /machine/i |  |  |
| `markdownReviewToolsEnabled` | `boolean` | true | exported |  |  |
| `minimaxEndpoint` | `'overseas' \| 'cn'` | "overseas" | excluded: pattern /endpoint/i |  |  |
| `minimaxGroupId` | `string` | "" | excluded: pattern /groupid/i |  |  |
| `minimaxUsageModels` | `string` | "general" | excluded: key denylist |  |  |
| `minimizeToTrayOnClose` | `?boolean` | false | exported |  | Windows only. |
| `mobileAutoRestoreFitMs` | `number \| null` | null | exported |  |  |
| `mobileEmulatorDefaultDeviceUdid` | `?string \| null` | null | excluded: pattern /udid/i |  |  |
| `mobileEmulatorEnabled` | `?boolean` | true | exported |  |  |
| `mobilePairingConnectionMode` | `?'automatic' \| 'local-only'` | "automatic" | excluded: pattern /pairing/i |  |  |
| `mobilePairingCustomAddress` | `?string \| null` | null | excluded: pattern /pairing/i |  |  |
| `mobilePairingCustomAddresses` | `?string[]` | [] | excluded: pattern /pairing/i |  |  |
| `nativeChatAppearance` | `?{fontFamily?, fontSize?, codeFontSize?, width?: 'comfortable'\|'wide'\|'full', contrast?, matchTerminalInterface?}` | _(absent)_ | exported |  | Chat appearance. |
| `nativeChatAutoName` | `?boolean` | _(absent)_ | exported |  | Absent means on. |
| `nativeChatInheritShellEnvironment` | `?boolean` | true | excluded: pattern /environment/i |  |  |
| `nativeChatInlineVisuals` | `?boolean` | true | exported |  |  |
| `nativeChatPermissionMode` | `?AgentChatPermissionMode` | "full-access" | exported |  |  |
| `nativeChatProviders` | `?Partial<Record<TuiAgent, boolean>>` | _(absent)_ | exported |  |  |
| `nativeChatQueueFollowUps` | `?boolean` | true | exported |  |  |
| `nativeChatResumeWorkOnRestart` | `?boolean` | false | exported |  |  |
| `nativeChatSessionOptions` | `?PersistedNativeChatSessionOptions` | {} | exported | yes |  |
| `nativeChatShellEnvironmentVariables` | `?string[]` | [] | excluded: pattern /environment/i |  |  |
| `nestWorkspaces` | `boolean` | true | exported |  |  |
| `nestedWorkerMaxDepth` | `?number` | 1 | exported |  |  |
| `newWorkspaceSetupOnly` | `?boolean` | _(absent)_ | exported |  | Deprecated. |
| `nodeDistMirrorUrl` | `?string` | "" | exported |  |  |
| `notifications` | `NotificationSettings` | {"enabled":true,"agentTaskComplete":true,"terminalBell":false,"suppressWhenFocused":true,"customSoundId":"system","customSoundVolume":100,"mutedNotificationSourceIds":[],"notifyHiddenWorkspaces":false} | exported | yes | `customSoundPath` is scrubbed. |
| `openClaudeTabsInWebView` | `?boolean` | false | exported | yes | Fork §18. |
| `openInApplications` | `?OpenInApplication[]` | [{"id":"vscode","label":"VS Code","command":"code"}] | excluded: key denylist |  |  |
| `openLinksInApp` | `boolean` | false | exported |  |  |
| `openLinksInAppInBackground` | `?boolean` | false | exported |  |  |
| `openLinksInAppModifierInverts` | `?boolean` | false | exported |  |  |
| `openLinksInAppPreferencePrompted` | `boolean` | false | excluded: pattern /prompted/i |  |  |
| `opencodeSessionCookie` | `string` | "" | excluded: pattern /cookie/i |  | Secret. |
| `opencodeWorkspaceId` | `string` | "" | excluded: pattern /workspaceid/i |  |  |
| `orchestrationChatErrorEscalationEnabled` | `?boolean` | true | exported |  |  |
| `orchestrationHeartbeatIntervalMinutes` | `?number` | 5 | exported |  |  |
| `orchestrationHeartbeatsEnabled` | `?boolean` | true | exported |  |  |
| `pluginConsents` | `Record<string, string>` | {} | excluded: pattern /consent/i |  |  |
| `pluginSystemEnabled` | `boolean` | false | exported |  |  |
| `prBotAuthorOverrides` | `string[]` | [] | exported |  | Bot-marked comment-author logins. |
| `primarySelectionMiddleClickPaste` | `?boolean` | true | exported |  | Platform default. |
| `primarySelectionMiddleClickPasteDefaultedForLinux` | `?boolean` | false | excluded: pattern /defaulted/i |  |  |
| `primarySelectionMiddleClickPasteDefaultedForTerminalDefaults` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `promptCacheTimerEnabled` | `boolean` | false | exported |  |  |
| `promptCacheTtlMs` | `number` | 300000 | exported |  | Only 300000 or 3600000. |
| `refreshLocalBaseRefOnWorktreeCreate` | `boolean` | false | exported | yes |  |
| `richMarkdownSpellcheckEnabled` | `?boolean` | true | exported |  |  |
| `rightSidebarOpenByDefault` | `boolean` | true | exported |  | Deprecated. |
| `showArtifactsButton` | `?boolean` | false | exported |  |  |
| `showAutomationsButton` | `?boolean` | true | exported |  |  |
| `showGitIgnoredFiles` | `?boolean` | true | exported |  |  |
| `showMenuBarIcon` | `?boolean` | true | exported |  |  |
| `showMobileButton` | `?boolean` | true | exported | yes |  |
| `showPinnedWorktreesInGroups` | `?boolean` | false | exported |  |  |
| `showSkillsButton` | `?boolean` | false | exported |  |  |
| `showTasksButton` | `boolean` | true | exported |  |  |
| `showTitlebarAppName` | `boolean` | true | exported |  |  |
| `skipCloseTerminalWithRunningProcessConfirm` | `boolean` | false | exported | yes |  |
| `skipCodexRateLimitResetConfirm` | `boolean` | false | exported |  |  |
| `skipDeleteArtifactConfirm` | `boolean` | false | exported |  |  |
| `skipDeleteAutomationConfirm` | `boolean` | false | exported |  |  |
| `skipDeleteWorktreeConfirm` | `boolean` | false | exported | yes |  |
| `sourceControlAi` | `?SourceControlAiSettings` | {"enabled":true,"actions":{…"{basePrompt}"…},…} | exported | yes | Global SC AI recipes. |
| `sourceControlCompareAgainstUpstream` | `boolean` | false | exported |  |  |
| `sourceControlGroupOrder` | `SourceControlGroupOrder` | "changes-first" | exported |  |  |
| `sourceControlGrouping` | `?'all' \| 'status'` | _(absent)_ | exported |  | Fork §35. Absent means 'all'. |
| `sourceControlSectionLayout` | `?{order?, hidden?, pr?: {order?, hidden?}}` | _(absent)_ | exported | yes | Fork §36. |
| `sourceControlViewMode` | `SourceControlViewMode` | "list" | exported | yes |  |
| `sshRemoteCliControlExistingHostsMigrated` | `?boolean` | true | excluded: pattern /migrated/i |  |  |
| `tabAutoGenerateTitle` | `boolean` | false | exported |  |  |
| `tabSwitchKeybindingSeed` | `?'pending' \| 'done'` | _(absent)_ | excluded: pattern /seed/i |  |  |
| `telemetry` | `?TelemetrySettings` | _(absent)_ | excluded: pattern /telemetry/i |  |  |
| `terminalActivePaneOpacity` | `number` | 1 | exported |  |  |
| `terminalAllowOsc52Clipboard` | `boolean` | true | exported |  |  |
| `terminalAllowOsc52ClipboardDefaultedOnForAllUsers` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `terminalBackgroundOpacity` | `?number` | _(absent)_ | exported |  |  |
| `terminalClipboardOnSelect` | `boolean` | false | exported |  |  |
| `terminalColorOverrides` | `?TerminalColorOverrides` | _(absent)_ | exported |  |  |
| `terminalCopyTrimsGutter` | `boolean` | true | exported |  |  |
| `terminalCursorBlink` | `boolean` | true | exported |  |  |
| `terminalCursorOpacity` | `?number` | _(absent)_ | exported |  |  |
| `terminalCursorStyle` | `'bar' \| 'block' \| 'underline'` | "block" | exported |  |  |
| `terminalCursorStyleDefaultedToBlock` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `terminalCustomThemes` | `?TerminalCustomTheme[]` | [] | exported | yes |  |
| `terminalDefaultShell` | `?string` | "" | excluded: pattern /shell/i |  |  |
| `terminalDefaultShellArgs` | `?string[]` | _(absent)_ | excluded: pattern /shell/i |  |  |
| `terminalDividerColorDark` | `string` | "#3f3f46" | exported |  |  |
| `terminalDividerColorLight` | `string` | "#d4d4d8" | exported | yes |  |
| `terminalDividerThicknessPx` | `number` | 3 | exported |  |  |
| `terminalFastScrollSensitivity` | `number` | 5 | exported |  |  |
| `terminalFocusFollowsMouse` | `boolean` | false | exported |  |  |
| `terminalFontFamily` | `string` | "SF Mono" | exported | yes | Platform default. |
| `terminalFontSize` | `number` | 14 | exported |  |  |
| `terminalFontWeight` | `number` | 500 | exported |  |  |
| `terminalFontWeightBold` | `number` | 700 | exported |  |  |
| `terminalGpuAcceleration` | `'auto' \| 'on' \| 'off'` | "auto" | exported |  |  |
| `terminalHiddenDeliveryGate` | `?boolean` | true | excluded: pattern /deliverygate/i |  |  |
| `terminalHiddenViewParking` | `?boolean` | true | excluded: pattern /parking/i |  |  |
| `terminalHiddenWorktreeRetentionBudget` | `?boolean` | true | excluded: pattern /retentionbudget/i |  |  |
| `terminalInactivePaneOpacity` | `number` | 0.9 | exported | yes |  |
| `terminalInlineImages` | `boolean` | true | exported |  |  |
| `terminalJISYenToBackslash` | `boolean` | false | exported |  |  |
| `terminalLigatures` | `'auto' \| 'on' \| 'off'` | "auto" | exported |  |  |
| `terminalLineHeight` | `number` | 1 | exported | yes |  |
| `terminalLinkActionPopoverEnabled` | `?boolean` | true | exported |  |  |
| `terminalLinkClickBehavior` | `?'actions' \| 'open' \| 'none'` | "actions" | exported |  |  |
| `terminalMacOptionAsAlt` | `'auto' \| 'true' \| 'false' \| 'left' \| 'right'` | "auto" | exported |  |  |
| `terminalMacOptionAsAltMigrated` | `boolean` | false | excluded: pattern /migrated/i |  |  |
| `terminalMainSideEffectAuthority` | `?boolean` | true | excluded: pattern /authority/i |  |  |
| `terminalMinimumContrastRatio` | `?number` | _(absent)_ | exported |  |  |
| `terminalModelQueryAuthority` | `?boolean` | true | excluded: pattern /authority/i |  |  |
| `terminalMouseHideWhileTyping` | `?boolean` | false | exported |  |  |
| `terminalPaddingX` | `?number` | _(absent)_ | exported |  |  |
| `terminalPaddingY` | `?number` | _(absent)_ | exported |  |  |
| `terminalPaneOpacityTransitionMs` | `number` | 140 | exported |  |  |
| `terminalQuickCommands` | `?TerminalQuickCommand[]` | [] | exported; repo scope rewritten to git remote |  | §3.1 |
| `terminalRightClickToPaste` | `boolean` | false | exported |  | Platform default. |
| `terminalRightClickToPasteDefaultedForPlatform` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `terminalScopeHistoryByWorktree` | `boolean` | true | exported |  |  |
| `terminalScrollSensitivity` | `number` | 1.15 | exported |  |  |
| `terminalScrollbackRows` | `number` | 5000 | exported |  |  |
| `terminalShortcutPolicy` | `?TerminalShortcutPolicy` | "orca-first" | exported |  |  |
| `terminalSshViewParking` | `?boolean` | true | excluded: pattern /parking/i |  |  |
| `terminalThemeDark` | `string` | "Ghostty Default Style Dark" | exported | yes |  |
| `terminalThemeLight` | `string` | "Builtin Tango Light" | exported | yes |  |
| `terminalTuiScrollSensitivity` | `number` | 1 | exported |  |  |
| `terminalTuiScrollSensitivityDefaultedToOne` | `?boolean` | true | excluded: pattern /defaulted/i |  |  |
| `terminalUrlMiddleClickBehavior` | `?'open' \| 'actions' \| 'none'` | "open" | exported |  |  |
| `terminalUseSeparateLightTheme` | `boolean` | true | exported |  |  |
| `terminalWindowsPowerShellImplementation` | `'auto' \| 'powershell.exe' \| 'pwsh.exe'` | "auto" | excluded: pattern /shell/i |  |  |
| `terminalWindowsShell` | `string` | "powershell.exe" | excluded: pattern /shell/i |  |  |
| `terminalWindowsWslDistro` | `?string \| null` | null | excluded: pattern /wsl/i |  |  |
| `terminalWordSeparator` | `?string` | _(absent)_ | exported |  |  |
| `theme` | `'system' \| 'dark' \| 'light'` | "system" | exported |  |  |
| `uiLanguage` | `UiLanguage` | "system" | exported |  |  |
| `uiLanguagePortugueseMigrationApplied` | `?boolean` | true | excluded: pattern /migration/i |  |  |
| `visibleTaskProviders` | `TaskProvider[]` | ["github","gitlab","linear","jira"] | exported |  |  |
| `visibleTaskProvidersDefaultedForJira` | `boolean` | true | excluded: pattern /defaulted/i |  |  |
| `voice` | `?VoiceSettings` | {"enabled":false,"sttModel":"","language":"en","dictationMode":"toggle","terminalConfirmBeforeInsert":false,"userModels":[]} (portable part) | exported | yes | `modelsDir`, `openAiApiKeyConfigured`, `microphoneDeviceId` and `microphoneDeviceLabel` are scrubbed. |
| `windowBackgroundBlur` | `?boolean` | false | exported |  |  |
| `workspaceDir` | `string` | "~/orca/workspaces" | excluded: pattern /dir$/i |  |  |
| `workspaceDirHistory` | `?OrcaWorkspaceLayout[]` | [] | excluded: pattern /dirhistory/i |  |  |
| `worktreeVisibilityDefaults` | `?WorktreeVisibilityDefaults` | {"external":"hide"} | exported |  | `customSources[].rootPath` is scrubbed. |
| `zcodePlanSite` | `?ZcodePlanSite` | "zai" | exported |  |  |

---

## 7. Rust type sketch (serde, lossless)

Design rules:

1. **Keep unknown keys at every level.** Orca adds settings keys every few days, real exports
   already carry a stale key (`experimentalWorktreeSymlinks`), and exports from older builds lack
   newer keys (`holdStatus` before §83). Each typed struct gets
   `#[serde(flatten)] extra: Map<String, Value>`.
2. **`settings` and `ui` stay ordered maps at the envelope level.** Use `serde_json` with the
   `preserve_order` feature (`Map` = `IndexMap`). Typed views are decoded from them on demand, so a
   round trip does not reorder or reformat keys the Rust side does not model.
3. **Never coerce numbers.** `workspaceBoardOpacity` is `1`, an integer; an `f64` would write `1.0`.
   Keep `serde_json::Number` or `Value` for anything not consumed, and use `f64` only in typed views.
4. **Lenient enums.** Orca never rejects a bad value: it falls back to the default. Deserialize
   unknown enum strings into an `Other(String)` variant, keep them on write, and resolve to the
   default when the value is used.
5. Writing `serde_json::to_string_pretty` (2-space indent) plus `"\n"` reproduces the CLI output
   byte for byte. This was verified with an equivalent order-preserving writer on the real file.

```rust
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const FORMAT: &str = "orca-settings";
pub const VERSION: u64 = 1;

/// Top-level file. Field order = JS output order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsTransferBundle {
    pub format: String,                 // must == FORMAT, checked after parse
    pub version: Value,                 // must be the JSON number 1 exactly ("1" is rejected)
    #[serde(default)]
    pub exported_at: String,            // RFC 3339 with millis + 'Z'
    #[serde(default)]
    pub settings: Map<String, Value>,   // ordered; filter with is_portable_setting_key on read+write
    #[serde(default)]
    pub ui: Map<String, Value>,         // ordered; allowlist UI_STATE_EXPORT
    #[serde(default)]
    pub projects: Vec<ProjectSettingsTransfer>,
    #[serde(default)]
    pub keybindings: Option<String>,    // JSON *text* of ~/.orca/keybindings.json
    #[serde(default)]
    pub editor_themes: EditorThemes,
    #[serde(flatten)]
    pub extra: Map<String, Value>,      // Orca drops these; keep or drop by policy
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditorThemes {
    #[serde(rename = "editor-dark.json", default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<String>,           // JSON text
    #[serde(rename = "editor-light.json", default, skip_serializing_if = "Option::is_none")]
    pub light: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,      // Orca ignores other names (path-traversal guard): never write them to disk
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSettingsTransfer {
    pub remote: String,                 // GitRemoteIdentity.canonicalKey, exact-match
    #[serde(default)]
    pub name: String,                   // informational
    pub settings: PortableRepoSettings,
}

/// Field order = REPO_FIELDS order (the order the exporter emits).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableRepoSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_settings: Option<RepoHookSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_source_preference: Option<String>,      // upstream|origin|auto
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_jira_project: Option<JiraProjectReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_sync_mode: Option<String>,               // ask|safe-auto|off
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_worktree_visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_worktree_visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_visibility_source_preferences: Option<Value>, // { builtIn: { claude?, gsd? } }
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_paths: Option<Vec<String>>,           // relative only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_venv_path: Option<String>,             // relative only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_labels_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_triage: Option<CiTriageConfig>,            // Some({}) == enabled with default prompt
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_control_ai: Option<Value>,             // RepoSourceControlAiOverrides; keep as Value
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoHookSettings {
    pub mode: String,                                  // "auto" | "override" (anything else -> auto)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_run_policy: Option<String>,              // ask|run-by-default|skip-by-default
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_agent_startup_policy: Option<String>,    // start-immediately|wait-for-setup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_source_policy: Option<String>,         // shared-only|local-only|run-both
    pub scripts: HookScripts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookScripts {
    #[serde(default)] pub setup: String,
    #[serde(default)] pub archive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraProjectReference { pub id: String, pub key: String, pub name: String, pub site_id: String }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CiTriageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,          // legacy; fold into prompt on import
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility { Hide, Show }

// ---------- UI typed views (decode from bundle.ui on demand) ----------

pub type WorkspaceStatusId = String;   // e.g. "todo", "in-progress", "status-6"

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceStatusDefinition {
    pub id: WorkspaceStatusId,
    pub label: String,                  // <= 32 UTF-16 units
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,          // WORKSPACE_STATUS_COLOR_IDS
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,           // WORKSPACE_STATUS_ICON_IDS
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceStatusRuleCondition {
    Merged, Closed, Reviewing, MergeQueue, Conflicts, Draft,
    ChangesRequested, Merging, PmApproval, Review,
}
/// Evaluation order; first match wins (see §4.3).
pub const CONDITION_ORDER: [WorkspaceStatusRuleCondition; 10] = {
    use WorkspaceStatusRuleCondition::*;
    [Merged, Closed, Reviewing, MergeQueue, Conflicts, Draft, ChangesRequested, Merging, PmApproval, Review]
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolveAction {
    Delete,
    #[default]
    #[serde(other)]
    None,                               // Orca: anything but "delete" means none
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewAgentView {
    #[default] Chat, Terminal, ClaudeWeb, Default,
    // unknown -> Chat (Orca's fallback); use a custom Deserialize if the raw value must survive
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceActionPrompts {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub commit_and_push: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub create_pull_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ready_for_review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub review_pull_request: Option<String>,
    #[serde(flatten)] pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInbox {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "default_agent")] pub agent: String,        // TuiAgent id; unknown -> "claude"
    #[serde(default)] pub view: ReviewAgentView,
    #[serde(default)] pub prompt_template: String,               // blank -> DEFAULT_REVIEW_PROMPT_TEMPLATE
    #[serde(default)] pub rereview_prompt_template: String,      // blank -> DEFAULT_REREVIEW_PROMPT_TEMPLATE
    #[serde(flatten)] pub extra: Map<String, Value>,
}
fn default_agent() -> String { "claude".into() }

/// Exported shape (local keys absent). Field order = JS object order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusRuleConfig {
    #[serde(default)] pub enabled: bool,
    /// LOCAL: never export; keep importer's value.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub repo_ids: Option<Vec<String>>,
    #[serde(default)] pub status_by_condition: IndexMap<String, WorkspaceStatusId>, // String key keeps unknown conditions
    #[serde(default = "default_check")] pub merging_check_name: String,
    #[serde(default)] pub new_workspace_status: Option<WorkspaceStatusId>,
    /// LOCAL: epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub new_workspace_status_since: Option<Value>,
    /// Absent in files from builds before 2026-11-11; absent == null.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub hold_status: Option<Option<WorkspaceStatusId>>,
    #[serde(default)] pub pm_approval_team: Option<String>,   // "org/team-slug"
    #[serde(default)] pub on_resolved: ResolveAction,
    #[serde(default)] pub on_reviewed: ResolveAction,
    #[serde(default)] pub resolve_conflicts_with_agent: bool,
    #[serde(default)] pub conflict_prompt_template: String,
    #[serde(default)] pub review_entered_command: String,
    #[serde(default)] pub action_prompts: WorkspaceActionPrompts,
    pub review_inbox: Option<ReviewInbox>,
    /// LOCAL.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub handled_pull_requests: Option<Vec<String>>,
    /// LOCAL.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ledger_version: Option<u32>,
    #[serde(flatten)] pub extra: Map<String, Value>,
}
fn default_check() -> String { "Reviews satisfied".into() }
```

`hold_status: Option<Option<_>>` tells "key absent" (keep the importer's value, per
`mergeImportedStatusRules`) apart from "explicit null". This is the one place where the difference
between absent and null changes import behaviour. The same holds in principle for every rule field,
so the safest import path merges the raw JSON objects (`{...current, ...imported}` on `Map`) and
then normalizes, instead of merging typed structs.

Functions to port verbatim, with their tests:

- `is_portable_setting_key` and `scrub_private_fields`, recursive and including arrays
- `pick_portable_ui` with the `UI_STATE_EXPORT` table, plus `pick_portable_status_rules`
- the `REPO_FIELDS` parsers
- `normalize_git_remote_url` and `derive_git_remote_identity`
- `merge_nested`: objects deep, arrays and scalars replace
- `merge_imported_status_rules`
- `resolve_imported_quick_commands`
- `normalize_workspace_status_rule_config` and `normalize_workspace_statuses`
- `resolve_workspace_status_rule_condition` and `build_workspace_status_rule_plan`
- `plan_new_workspace_status_updates`
- the two template renderers

Port the fork's test fixtures in `src/shared/settings-transfer.test.ts`,
`workspace-status-rule-config.test.ts`, `workspace-status-rules*.test.ts` and
`workspace-status-rule-plan.test.ts` as golden tests. Also use a real export (the private reference
export locally; a scrubbed copy in CI) as a round-trip fixture: parse, then write, must give
identical bytes.

---

## 8. Mapping onto a generic trigger/condition/action rules model

The rewrite plans user-defined statuses and generic rules. Today's config is not a rule list. It is
a set of **fixed slots**: a hardcoded classifier (§4.3) picks exactly one condition per PR, and each
slot either names a target column or switches on a hardcoded side effect. Many behaviours live only
in code. To import the current file with **no change in behaviour**, the importer must produce a
preset that reproduces both the fields in the file and the implicit behaviour listed here.

### 8.1 Engine primitives the generic model must provide

| Primitive | Why it is needed (current behaviour) |
|---|---|
| **Poll tick** every 60 s while enabled. A window-visibility-aware timer drives it; it does not run in dev builds without opt-in. | All rules run per tick. Nothing is driven by webhooks. |
| **Scope** = set of projects (`repoIds`). | Rules, the new-workspace column and the review inbox act only on these projects. Empty scope means everything is off. Scope is **not in the export** (§5.5). |
| **Targets** = workspaces in scope with a linked GitHub PR resolved (repo + number). | Status rules act only on workspaces with a linked PR. |
| **PR snapshot facts** (one GraphQL call per tick): `state`, `isDraft`, `mergeable`, `isInMergeQueue`, `author`, `requestedReviewers` (user/team), `latestReviews`, `latestOpinionatedReviews` (with commit oid), `viewerLatestReview`, named check/status states, `headRefOid`, `headRefName`, `baseRefName`, `title`, `url`, viewer login, PM team members, and the `review-requested:@me` list limited to the scoped repos. | Inputs of every condition below. |
| **Exclusive, ordered classification** ("first match wins"), returning **one** condition or **no-op**. | Today's conditions are mutually exclusive by order, not by their predicates. Several predicates overlap: a conflicting draft is `conflicts`, never `draft`. A generic engine that evaluates rules independently would move one card twice in a tick. The preset must be **one ordered rule group with first-match semantics**. |
| **"Unmapped means skip, and fall through" vs "unmapped means stop"** | `merge-queue` and `conflicts` are *conditional* in the order: they take part only when mapped, otherwise evaluation falls through to later conditions. Every other condition still **matches and stops** when unmapped, and then nothing moves. Example: `merged` unmapped stops evaluation, and the card stays put. The engine needs both kinds of rule. |
| **No-op result**: "hold still this tick" | `conflicts` mapped plus `mergeable == UNKNOWN` returns null. The card is not moved anywhere, even if `draft` or `review` would match. |
| **Transition detection** ("on enter column") | The conflict agent and the review command fire only when a **status update is planned**, that is when the computed column differs from the current one. They key on the **condition** (`conflicts`, `review`), not on the column id. If two conditions map to the same column, only the named condition fires the action. They never re-fire while the card stays. |
| **Exclusion column** (`holdStatus`) | A pre-filter that removes targets from moves, deletes and head sync. Held targets still count as "linked" for inbox de-duplication. |
| **Ledger** (`handledPullRequests`, at most 500, key `owner/repo#N[@sha]`) | Keeps a review workspace from being created twice. It is forgotten after certain deletes. Local state, not configuration. |
| **Actions**: set column, delete workspace (with force flag and guards), launch agent (agent + view + prompt template), run shell command, create review workspace, head sync. | See §8.3. |

### 8.2 Each setting translated into a preset

Notation: `C(x)` means classifier condition `x` from §4.3. "Implicit" marks behaviour the file does
not encode and the importer must add itself.

| Setting in file | Equivalent generic rule(s) | Fixed today (enum or hardcoded) | Implicit behaviour to add |
|---|---|---|---|
| `enabled` | Master switch of the "PR rules" preset group. | bool | Also gates the new-workspace column, the inbox, head sync and PR-author avatars. |
| `repoIds` (local) | The scope of the group. | — | Empty means inert. Not exported. On import, keep the local value or ask the user. |
| `statusByCondition[c] = col` | One rule per condition, **in the fixed order of §4.3**: trigger `tick`; condition `C(c)`; action `set column col`, only if it differs from the current one. | The 10 condition ids and their predicates are hardcoded. Only the target column is configurable. | Unmapped conditions: add a "stop, no-op" rule (except `merge-queue` and `conflicts`, which are skipped). Predicates use `mergingCheckName` and `pmApprovalTeam` (below). Viewer and author comparisons ignore case. |
| `statusByCondition.conflicts` present | Also adds the rule `mergeable == UNKNOWN → no-op` just before `draft`. | — | The mere presence of a column turns on the UNKNOWN hold. |
| `statusByCondition['merge-queue']` present | Inserts the rule `isInMergeQueue → col` after `reviewing`. | — | — |
| `mergingCheckName` | Parameter of `C(merging)`: a check whose name equals the setting (trimmed, case-insensitive) has state `success`. | default `"Reviews satisfied"`, at most 120 characters | `C(merging)` also matches the PM-approved-by-everyone path (§4.3 step 8), even when the check is missing. |
| `pmApprovalTeam` | Parameter of `C(merging)` (second path) and `C(pm-approval)`: the team's member logins. | `org/slug` | null disables both PM paths. Pending reviewers count users only; team requests are ignored. |
| `newWorkspaceStatus` + `…Since` | A separate rule: trigger `tick`; condition: workspace in scope, `status == null`, `createdAt != null && createdAt >= since`; action `set column`. | — | Runs before the PR rules in the same tick, and also for workspaces with no PR. Workspaces found on disk (`createdAt == null`) are never moved. On import, `since` = import time. |
| `holdStatus` | A group-level exclusion: `status == holdStatus` skips every PR rule and head sync. | — | Absent in older exports, which means null and no hold. |
| `onResolved: delete` | Rule placed *before* the column mapping: `C(merged) or C(closed)` **and** `target.headOid == pr.headRefOid`, then `delete workspace (force=false)` and **stop**. | `'delete' \| 'none'` | Unforced: git refuses uncommitted work or a live agent, and the deletion is retried next tick. The head-OID guard protects branches that resolve to an old PR. When not deleted (guard fails, or `none`), it falls through to the column mapping for `merged`/`closed`. With those unmapped (the usual setup), nothing happens. |
| `onReviewed: delete` | Rule before the column mapping: `C(reviewing)` and no unsent pending review comments and (viewer verdict APPROVED or CHANGES_REQUESTED and the viewer is not re-requested, **or** "unassigned"), then `delete workspace (force=true)`, forget the ledger key if the PR is not currently in the inbox, and **stop**. | `'delete' \| 'none'` | "Unassigned" needs `reviewInbox.enabled`, a ledger hit, no team still requested, the viewer not pending, and no review by the viewer. Pending review comments come from a device file, `pending-review-drafts.json`. |
| `resolveConflictsWithAgent` | Rule: trigger `on transition` with condition `conflicts` (a status update planned), then `launch agent` with agent `reviewInbox.agent`, prompt `conflictPromptTemplate` (empty means built-in), background session, title `Resolve conflicts #N`. | bool | **The agent is the review-inbox agent**; there is no separate setting. Not local-only. It fires only if `statusByCondition.conflicts` is mapped and the column changes. |
| `conflictPromptTemplate` | Prompt parameter of the action above. | — | The empty-string fallback is upstream's built-in prompt. Variables: §4.5. |
| `reviewEnteredCommand` | Rule: trigger `on transition` with condition `review`, **local host only**, then `run shell command` (bash, app env, cwd = worktree), once. | at most 1000 characters, trimmed | A failure toasts the last 6 output lines. No retry. |
| `actionPrompts.*` | Not automations: templates for the 4 top-bar buttons (manual triggers). Map them to "manual actions" with the agent `reviewInbox.agent`. | The 4 ids are fixed. | Which button shows is chosen by hardcoded priority (FORK §13), not by config. A blank value means the built-in prompt. Renderer: §4.5. |
| `reviewInbox.enabled` | Rule: trigger `tick`; for each PR in `review-requested:@me` within the scoped repos (at most 30 per tick, one at a time): skip when the ledger has key `owner/repo#N[@viewerReviewedSha]`, when a scoped workspace is already linked to it (held ones included), when the viewer is the author, or when a workspace already exists for the head branch. Otherwise **create the review workspace** and record the key. | bool | Created workspace: name `review-<N>-<slug(title)[:40]>`, in the scoped project whose path ends in `owner/repo` (or whose upstream matches), else the first scoped project. Initial column = `statusByCondition.reviewing`. Setup hooks pass the trust gate. On a re-request, the diff base is pinned to the last reviewed commit (an interdiff ref). |
| `reviewInbox.agent` | Agent of the review, conflict and top-bar actions. | `TuiAgent` enum (45 ids), default `claude` | Shared by three features. Map it to three actions with the same agent. |
| `reviewInbox.view` | How the agent tab opens. | `chat \| terminal \| claude-web \| default` | — |
| `reviewInbox.promptTemplate` / `rereviewPromptTemplate` | The prompt of the create action. The re-review one is used when the viewer has reviewed before (a reviewed commit exists). | blank means built-in | Variables: §4.5. `sinceCommit` falls back to `baseRef`. |
| _(no setting)_ head sync | An implicit rule: every tick, each released local review workspace (someone else's open PR) is fast-forwarded or reset to the PR head. It refuses on a wrong branch, detached HEAD, a dirty tree, a failed fetch, or a local-only commit by the reviewer's `user.email`. | always on while enabled | Must be in the preset as an always-on rule. |
| _(no setting)_ PR author avatars | Card display data for workspaces in scope. | always on | Not a rule. Keep it as a side effect of the snapshot. |
| _(project)_ `ciTriage` | A separate per-project rule: trigger "a PR **you** authored has a failed run (`failure`/`timed_out`) that has a link", then launch one background agent per run with the triage prompt. | Presence means on. | Not part of `workspaceStatusRules`. The `enabled`/`repoIds` gates do not apply; it runs for any repo with `ciTriage`. Its memory (7 days, at most 3 per PR) lives in localStorage. |

### 8.3 A full file as a preset (worked translation)

Translation of the illustrative config of §5.1. The same translation of the reference setup's real
file, with its column ids and values, is in `private/reference-setup.md` (not in the repo).

Group "PR rules", scope = the importer's chosen projects, first-match, ordered:

```
pre:   hold column            = "status-9" (skipped by every rule and by head sync)
pre:   new workspace          -> "todo" if status==null && createdAt>=importTime
1  C(merged)|C(closed) && headOid==prHead  -> delete (unforced); stop        [onResolved=delete]
   C(merged)|C(closed) otherwise           -> no-op; stop                    [unmapped]
2  C(reviewing) && finished-or-unassigned && !pendingComments -> delete (forced), forget key; stop [onReviewed=delete]
   C(reviewing)                            -> "status-2"
3  isInMergeQueue                          -> "status-3-2"
4  mergeable==CONFLICTING (own PR)         -> "status-1"; on enter: launch agent claude/chat with custom conflict prompt
   mergeable==UNKNOWN                      -> no-op; stop
5  isDraft                                 -> "todo"
6  unanswered CHANGES_REQUESTED            -> "status-4"
7  check "Reviews satisfied"==success || PM(acme/pm-team)+everyone approved -> "status-3"
8  all pending user reviewers ∈ PM team    -> "completed"
9  otherwise                               -> "in-review"; on enter (local): run `make dev-down`
post:  review inbox ON: create review workspaces (agent claude, view chat, custom prompts), initial column "status-2"
post:  head sync of review workspaces
manual: 4 top-bar actions with the custom prompts, agent claude
per project with ciTriage: CI triage ON with its prompt
```

A Rust importer that builds this from the file and runs it on a generic engine with the §8.1
primitives behaves like the Electron fork. Exporting back to `orca-settings` v1 is possible only
while a preset stays inside this fixed shape. A rule set edited outside the shape needs a new
format version or an extension key; the fork drops unknown `workspaceStatusRules` keys when it
normalizes.

## 9. Compatibility checklist for the Rust rewrite

- [ ] Accept `format == "orca-settings"` and `version === 1` only. Return the same three error
      messages.
- [ ] Filter on import exactly as on export. Never trust the file.
- [ ] Settings: deep-merge objects and replace arrays, write only the keys present, and leave
      absent keys alone.
- [ ] UI: replace per key and leave absent keys alone. Status rules: shallow-merge onto the current
      ones, keep `repoIds`, the ledger and `ledgerVersion` local, and restamp
      `newWorkspaceStatusSince`.
- [ ] Projects: match by exact `canonicalKey` and apply to **all** matching repos on all hosts,
      replacing each field present.
- [ ] Quick commands: resolve `{type:"project", remote}` to the first matching repo id, else drop.
- [ ] Keybindings and themes: replace only when the value is non-null and valid JSON. Accept only
      the two theme file names.
- [ ] Normalize everything after the import, as the hydration sanitizers do.
- [ ] Export: the same key order, `JSON.stringify(_, null, 2)`, a trailing `\n` for `--output`,
      unknown keys preserved, the same private-key filter, and the 4 local status-rule keys removed.
- [ ] CLI: `orca settings export [--output FILE]` and `orca settings import --file FILE`. The
      import reports a summary in the format `describeSettingsBundle` uses.
- [ ] Status rules: the exact condition order (§4.3), "unmapped never moves", the `UNKNOWN`
      mergeable hold, the hold column, the head-OID guard on delete, the forced review delete with
      the pending-comment guard, a 30-creation cap per tick, and the ledger key format
      `owner/repo#N[@sha]`.
