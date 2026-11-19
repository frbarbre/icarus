// The GitHub PR flow that Orca's workspace status rules ran, as an automation script.
// The options below are yours to change; `automations.scripts.<project>` points a project at this file.
import type { Action, Button, Decision, InboxPullRequest, PullRequest, Scope, Workspace } from "./.generated/icarus";

type Condition =
  | "merged" | "closed" | "reviewing" | "merge-queue" | "conflicts" | "draft"
  | "changes-requested" | "merging" | "pm-approval" | "review";

type Config = {
  /** The column each PR condition moves a card to; a missing condition never moves cards. */
  statusByCondition: Partial<Record<Condition, string>>;
  /** The column for a new workspace without one, for workspaces created at or after `newWorkspaceStatusSince` (epoch ms). */
  newWorkspaceStatus: string | null;
  newWorkspaceStatusSince: number;
  /** Delete the workspace when its PR merges or closes (only when its checkout is at the PR's head). */
  onResolved: "keep" | "delete";
  /** Delete a review workspace once you've approved or requested changes and sent your drafts. */
  onReviewed: "keep" | "delete";
  mergingCheckName: string;
  /** "org/team" whose approval moves a PR to merging, and whose pending review means pm-approval; listed in `automations.teams`. */
  pmApprovalTeam: string;
  resolveConflictsWithAgent: boolean;
  conflictPrompt: string;
  /** Run in a terminal when a card enters the review column. */
  reviewEnteredCommand: string;
  agent: string;
  reviewInbox: {
    enabled: boolean;
    prompt: string;
    rereviewPrompt: string;
    /** PRs already handled, as `owner/repo#number` or `owner/repo#number@commit`. */
    handled: string[];
  };
  /** Per project name or id: the instructions for an agent fixing a failed CI run on your PR. */
  ciTriage: Record<string, string>;
  notify: { ciFailed: boolean; merged: boolean; reviewRequested: boolean };
};

const config: Config = __ICARUS_CONFIG__;

const MAX_INBOX_CREATES = 30;
const MAX_CREATE_ATTEMPTS = 3;
const MAX_TRIAGES_PER_PR = 3;
const DAY = 86_400_000;

const scope: Scope = await Bun.stdin.json();
const now = scope.now;
const viewer = (scope.viewer ?? "").toLowerCase();
type Memory = {
  created: Record<string, number>;
  attempts: Record<string, number>;
  triaged: Record<string, number>;
  triageStarts: Record<string, { count: number; at: number }>;
};
const memory: Memory = { created: {}, attempts: {}, triaged: {}, triageStarts: {}, ...(scope.memory ?? {}) };
const actions: Action[] = [];
const buttons: Record<string, Button[]> = {};

const lower = (s: string | null | undefined) => (s ?? "").toLowerCase();
const fill = (template: string, vars: Record<string, unknown>) =>
  template.replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, key: string) => {
    const value = key.split(".").reduce<any>((at, part) => at?.[part], vars);
    return value == null ? "" : String(value);
  });
const prKey = (pr: PullRequest) => `${pr.repo.owner}/${pr.repo.name}#${pr.number}`;
// The server sends one notification per kind and subject, so a state is announced once.
const subject = (pr: PullRequest, head = false) => prKey(pr).toLowerCase() + (head ? `@${pr.head_ref_oid}` : "");
const team = new Set((scope.teams?.[config.pmApprovalTeam] ?? []).map(lower));

const pendingUsers = (pr: PullRequest) =>
  pr.requested_reviewers.flatMap((r) => (r.kind === "user" ? [r.login.toLowerCase()] : []));
const hasTeamReviewer = (pr: PullRequest) => pr.requested_reviewers.some((r) => r.kind === "team");
const verdicts = (pr: PullRequest) => pr.latest_opinionated_reviews ?? pr.latest_reviews;
const authoredBySomeoneElse = (pr: PullRequest) => !!viewer && !!pr.author && lower(pr.author) !== viewer;
const checkState = (check: PullRequest["checks"][number]) => {
  const value = (check.kind.type === "run" ? check.kind.conclusion ?? "" : check.kind.state).toUpperCase();
  if (value === "SUCCESS") return "success";
  if (["FAILURE", "ERROR", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"].includes(value)) return "failure";
  return check.kind.type === "run" && check.kind.status !== "COMPLETED" ? "pending" : "neutral";
};

function unansweredChangeRequest(pr: PullRequest) {
  const pending = new Set(pendingUsers(pr));
  return verdicts(pr).some((r) => r.state === "CHANGES_REQUESTED" && !pending.has(lower(r.login)));
}
function viewerOwed(pr: PullRequest) {
  return !!viewer && pendingUsers(pr).includes(viewer);
}
function viewerFinished(pr: PullRequest) {
  if (!viewer || viewerOwed(pr)) return false;
  const mine =
    pr.latest_reviews.find((r) => lower(r.login) === viewer) ??
    (pr.latest_opinionated_reviews ?? []).find((r) => lower(r.login) === viewer);
  return mine?.state === "APPROVED" || mine?.state === "CHANGES_REQUESTED";
}
function approvedByTeamAndEveryone(pr: PullRequest) {
  if (!team.size || pendingUsers(pr).length) return false;
  const others = verdicts(pr).filter((r) => lower(r.login) !== lower(pr.author));
  return others.length > 0 && others.every((r) => r.state === "APPROVED") && others.some((r) => team.has(lower(r.login)));
}
function awaitsOnlyTeam(pr: PullRequest) {
  const pending = pendingUsers(pr);
  return team.size > 0 && pending.length > 0 && pending.every((login) => team.has(login));
}

const inbox: InboxPullRequest[] = Array.isArray(scope.inbox) ? scope.inbox : [];
const inInbox = new Set(inbox.map(prKey));
const handledBefore = (pr: PullRequest) =>
  config.reviewInbox.handled.some((k) => k === prKey(pr) || k.startsWith(`${prKey(pr)}@`)) ||
  Object.keys(memory.created).some((k) => k === prKey(pr) || k.startsWith(`${prKey(pr)}@`));

/** Done reviewing: finished, or (with the inbox on) no longer asked and once handled. */
function reviewDone(ws: Workspace, pr: PullRequest) {
  if (ws.review_pending > 0) return false;
  if (viewerFinished(pr)) return true;
  return (
    config.reviewInbox.enabled && !!viewer && !inInbox.has(prKey(pr)) && !hasTeamReviewer(pr) &&
    !viewerOwed(pr) && handledBefore(pr)
  );
}

/** The first matching condition, in Orca's order; `hold` keeps the current column. */
function condition(ws: Workspace, pr: PullRequest): { name: Condition | "hold"; remove?: boolean } {
  const mapped = (name: Condition) => config.statusByCondition[name] != null;
  for (const [name, state] of [["merged", "MERGED"], ["closed", "CLOSED"]] as const) {
    if (pr.state !== state) continue;
    return { name, remove: config.onResolved === "delete" && ws.head_is_local === true };
  }
  if (authoredBySomeoneElse(pr)) {
    return { name: "reviewing", remove: config.onReviewed === "delete" && reviewDone(ws, pr) };
  }
  if (pr.is_in_merge_queue && mapped("merge-queue")) return { name: "merge-queue" };
  if (pr.mergeable === "CONFLICTING" && mapped("conflicts")) return { name: "conflicts" };
  if (pr.mergeable === "UNKNOWN" && mapped("conflicts")) return { name: "hold" };
  if (pr.is_draft) return { name: "draft" };
  if (unansweredChangeRequest(pr)) return { name: "changes-requested" };
  const wanted = lower(config.mergingCheckName.trim());
  if (
    (wanted && pr.checks.some((c) => lower(c.name.trim()) === wanted && checkState(c) === "success")) ||
    approvedByTeamAndEveryone(pr)
  ) {
    return { name: "merging" };
  }
  if (awaitsOnlyTeam(pr)) return { name: "pm-approval" };
  return { name: "review" };
}

for (const ws of scope.workspaces) {
  const pr = ws.pr;
  if (!pr) {
    if (config.newWorkspaceStatus && !ws.status && ws.created_ms >= config.newWorkspaceStatusSince) {
      actions.push({ method: "workspace.set", params: { id: ws.id, status: config.newWorkspaceStatus } });
    }
    continue;
  }
  const { name, remove } = condition(ws, pr);
  if (remove) {
    actions.push({ method: "workspace.remove", params: { id: ws.id, force: name === "reviewing" } });
    continue;
  }
  const status = name === "hold" ? undefined : config.statusByCondition[name];
  if (status && status !== ws.status) {
    actions.push({ method: "workspace.set", params: { id: ws.id, status } });
    if (name === "review" && config.reviewEnteredCommand) {
      actions.push({ method: "terminal.create", params: { workspace: ws.id, command: ["sh", "-c", config.reviewEnteredCommand] } });
    }
    if (name === "conflicts" && config.resolveConflictsWithAgent) {
      actions.push({
        method: "agent.create",
        params: {
          workspace: ws.id,
          provider: config.agent,
          title: `Resolve conflicts #${pr.number}`,
          prompt: fill(config.conflictPrompt, { prNumber: pr.number, baseRef: pr.base_ref_name, branch: pr.head_ref_name, worktreePath: ws.path }),
        },
      });
    }
  }
  if (
    authoredBySomeoneElse(pr) && pr.state === "OPEN" && pr.head_ref_oid && ws.branch === pr.head_ref_name &&
    (ws.head_is_local === false || ws.base_follows_target === false)
  ) {
    actions.push({ method: "workspace.sync_to_pr_head", params: { id: ws.id } });
  }
}

if (config.reviewInbox.enabled) {
  let creates = 0;
  for (const pr of inbox) {
    const since = pr.viewer_latest_review?.commit_oid ?? null;
    const key = since ? `${prKey(pr)}@${since}` : prKey(pr);
    if (pr.workspace) {
      memory.created[key] ??= now;
      continue;
    }
    if (lower(pr.author) === viewer) continue;
    if (config.notify.reviewRequested) {
      actions.push({
        method: "notification.send",
        params: {
          kind: "review_requested",
          subject: subject(pr, true),
          title: `Review requested: #${pr.number}`,
          body: `${pr.title} by ${pr.author ?? "someone"}`,
        },
      });
    }
    if (config.reviewInbox.handled.includes(key) || memory.created[key]) continue;
    if ((memory.attempts[key] ?? 0) >= MAX_CREATE_ATTEMPTS || creates >= MAX_INBOX_CREATES) continue;
    memory.attempts[key] = (memory.attempts[key] ?? 0) + 1;
    creates += 1;
    const vars = {
      prNumber: pr.number, title: pr.title, prTitle: pr.title, author: pr.author, url: pr.url, prUrl: pr.url,
      branch: pr.head_ref_name, baseRef: since ?? `origin/${pr.base_ref_name}`, sinceCommit: since,
    };
    actions.push({
      method: "review.workspace",
      params: {
        project: scope.project.id,
        number: pr.number,
        status: config.statusByCondition.reviewing,
        since,
        layout: "review",
        agent: {
          provider: config.agent,
          prompt: fill(since ? config.reviewInbox.rereviewPrompt : config.reviewInbox.prompt, vars),
          title: `Review #${pr.number}`,
        },
      },
    });
  }
}

const triage = config.ciTriage[scope.project.name] ?? config.ciTriage[scope.project.id];
const trigger = scope.trigger;
if (trigger.kind === "event") {
  for (const event of "events" in trigger ? trigger.events : []) {
    const ws = scope.workspaces.find((w) => w.id === event.workspace);
    const pr = ws?.pr;
    if (!ws || !pr) continue;
    if (event.name === "checks.failed") {
      const failed = pr.checks.filter((c) => checkState(c) === "failure");
      const mine = !authoredBySomeoneElse(pr);
      if (mine && config.notify.ciFailed) {
        actions.push({
          method: "notification.send",
          params: {
            kind: "ci_failed",
            subject: subject(pr, true),
            workspace: ws.id,
            title: `CI failed on #${pr.number}`,
            body: `${pr.title}: ${failed.map((c) => c.name).join(", ")} failing`,
          },
        });
      }
      const linked = failed.filter((c) => c.url);
      const rerunning = pr.checks.some((c) => checkState(c) === "pending" && failed.some((f) => f.name === c.name));
      const untriaged = linked.filter((c) => !memory.triaged[`${prKey(pr)} ${c.url}`]);
      const starts = memory.triageStarts[prKey(pr)]?.count ?? 0;
      if (triage && untriaged.length && !rerunning && starts < MAX_TRIAGES_PER_PR) {
        for (const c of untriaged) memory.triaged[`${prKey(pr)} ${c.url}`] = now;
        memory.triageStarts[prKey(pr)] = { count: starts + 1, at: now };
        if (mine) {
          const list = [...untriaged, ...failed.filter((c) => !c.url)]
            .map((c) => (c.url ? `- ${c.name}: ${c.url}` : `- ${c.name}`))
            .join("\n");
          actions.push({
            method: "agent.create",
            params: {
              workspace: ws.id,
              provider: config.agent,
              title: "Fix CI",
              prompt: `CI failed on pull request #${pr.number} at ${pr.head_ref_oid}. Failed checks:\n${list}\n\n${triage}`,
            },
          });
        }
      }
    }
    if (event.name === "pr.state_changed" && event.payload.to === "MERGED" && config.notify.merged) {
      actions.push({
        method: "notification.send",
        params: { kind: "pr_merged", subject: subject(pr), workspace: ws.id, title: `#${pr.number} merged`, body: pr.title },
      });
    }
  }
}

// Linear's "Open in coding tool" hook: `icarus automation emit linear.open_issue --path "$LINEAR_WORK_DIR" --env …`.
if (scope.trigger?.kind === "event" && scope.trigger.name === "linear.open_issue") {
  const env = (scope.trigger.payload ?? {}) as Record<string, string | undefined>;
  const issue = env.LINEAR_ISSUE_IDENTIFIER;
  // The name doubles as the dev server's <name>.localhost label, so it stays the short issue id.
  const name = lower(issue ?? env.LINEAR_ISSUE_BRANCH_NAME ?? "linear-task");
  const branch = env.LINEAR_ISSUE_BRANCH_NAME;
  const existing = scope.workspaces.find((ws) => lower(ws.name) === name);
  if (!existing) {
    actions.push({
      method: "workspace.create",
      params: {
        project: scope.project.id,
        name,
        ...(branch ? { branch } : {}),
        ...(issue ? { issue } : {}),
        agent: false,
      },
    });
  }
  // A draft, not a prompt: the app brings the workspace up with the text in the composer, unsent.
  actions.push({
    method: "agent.create",
    params: {
      workspace: existing?.id ?? name,
      provider: config.agent,
      mode: "plan",
      draft: env.LINEAR_PROMPT || `Work on ${issue ?? "this issue"}.`,
    },
  });
  if (issue) actions.push({ method: "issue.assign", params: { id: issue } });
}

// Inbox entries stay while the PR still asks for a review, so a deleted review workspace isn't recreated.
for (const ledger of [memory.created, memory.attempts]) {
  for (const key of Object.keys(ledger)) if (!inInbox.has(key.split("@")[0]) && now - (memory.created[key] ?? 0) > 14 * DAY) delete ledger[key];
}
for (const [key, at] of Object.entries(memory.triaged)) if (now - at > 7 * DAY) delete memory.triaged[key];
for (const [key, value] of Object.entries(memory.triageStarts)) if (now - value.at > 7 * DAY) delete memory.triageStarts[key];

const decision: Decision = { actions, memory, buttons };
console.log(JSON.stringify(decision));
