// The built-in GitHub PR flow (Orca's workspace status rules), as an automation script.
// Edit the config below, then point `automations.scripts.<project>` at this file.
import type { Action, Button, Decision, Scope, Workspace, PullRequest } from "./.generated/icarus";

const config = {
  // Which column each PR condition moves a card to; leave one out to never move cards for it.
  statusByCondition: {
    merged: "completed",
    closed: "completed",
    reviewing: "in-review",
    "merge-queue": null,
    conflicts: null,
    draft: "in-progress",
    "changes-requested": "in-progress",
    merging: "in-review",
    "pm-approval": null,
    review: "in-review",
  } as Record<string, string | null>,
  newWorkspaceStatus: "in-progress" as string | null,
  onResolved: "keep" as "keep" | "delete",
  onReviewed: "keep" as "keep" | "delete",
  mergingCheckName: "Reviews satisfied",
  // Logins of the PM approval team (lowercase); empty turns the pm-approval and team-approval conditions off.
  pmTeam: [] as string[],
  resolveConflictsWithAgent: false,
  reviewEnteredCommand: "",
  reviewInbox: {
    enabled: false,
    agent: "claude",
    prompt: "Review PR #{{pr.number}}: {{pr.title}} ({{pr.url}}). Leave draft comments with `icarus review comment add`.",
    rereviewPrompt: "PR #{{pr.number}} changed since your last review. Review what changed since {{since}}.",
  },
  ciTriage: { enabled: false, maxPerPr: 3, prompt: "CI failed on #{{pr.number}}: {{failed}}. Find out why and fix it." },
  notify: { ciFailed: true, merged: true, reviewRequested: true },
};

const scope: Scope = await Bun.stdin.json();
const viewer = (scope.viewer ?? "").toLowerCase();
const memory: { handled: Record<string, number>; triaged: Record<string, number>; notified: Record<string, number> } = {
  handled: {},
  triaged: {},
  notified: {},
  ...(scope.memory ?? {}),
};
const actions: Action[] = [];
const buttons: Record<string, Button[]> = {};
const lower = (s: string | null | undefined) => (s ?? "").toLowerCase();
const fill = (template: string, vars: Record<string, unknown>) =>
  template.replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, path: string) => {
    const value = path.split(".").reduce<any>((at, key) => at?.[key], vars);
    return value == null ? "" : String(value);
  });

function pendingUsers(pr: PullRequest): string[] {
  return pr.requested_reviewers.flatMap((r) => (r.kind === "user" ? [r.login.toLowerCase()] : []));
}
function verdicts(pr: PullRequest) {
  return pr.latest_opinionated_reviews ?? pr.latest_reviews;
}
function checkState(check: PullRequest["checks"][number]): string {
  const raw = check.kind.type === "run" ? check.kind.conclusion ?? "" : check.kind.state;
  const value = raw.toUpperCase();
  if (value === "SUCCESS") return "success";
  if (["FAILURE", "ERROR", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"].includes(value)) return "failure";
  return check.kind.type === "run" && check.kind.status !== "COMPLETED" ? "pending" : "neutral";
}
const authoredBySomeoneElse = (pr: PullRequest) => !!viewer && !!pr.author && lower(pr.author) !== viewer;
function unansweredChangeRequest(pr: PullRequest) {
  const pending = new Set(pendingUsers(pr));
  return verdicts(pr).some((r) => r.state === "CHANGES_REQUESTED" && !pending.has(lower(r.login)));
}
function viewerFinished(pr: PullRequest) {
  if (!viewer || pendingUsers(pr).includes(viewer)) return false;
  const mine =
    pr.latest_reviews.find((r) => lower(r.login) === viewer) ??
    (pr.latest_opinionated_reviews ?? []).find((r) => lower(r.login) === viewer);
  return mine?.state === "APPROVED" || mine?.state === "CHANGES_REQUESTED";
}
function approvedByTeamAndEveryone(pr: PullRequest) {
  if (!config.pmTeam.length || pendingUsers(pr).length) return false;
  const others = verdicts(pr).filter((r) => lower(r.login) !== lower(pr.author));
  return (
    others.length > 0 &&
    others.every((r) => r.state === "APPROVED") &&
    others.some((r) => config.pmTeam.includes(lower(r.login)))
  );
}
function awaitsOnlyTeam(pr: PullRequest) {
  const pending = pendingUsers(pr);
  return config.pmTeam.length > 0 && pending.length > 0 && pending.every((l) => config.pmTeam.includes(l));
}

/** The first matching condition, in the built-in rules' order; a null name leaves the status alone. */
function condition(ws: Workspace, pr: PullRequest): { name: string | null; remove?: boolean } {
  for (const [name, state] of [["merged", "MERGED"], ["closed", "CLOSED"]] as const) {
    if (pr.state !== state) continue;
    if (config.onResolved === "delete" && ws.head_is_local) return { name, remove: true };
    return { name };
  }
  if (authoredBySomeoneElse(pr)) {
    if (config.onReviewed === "delete" && ws.review_pending === 0 && viewerFinished(pr)) return { name: "reviewing", remove: true };
    return { name: "reviewing" };
  }
  const mapped = (name: string) => config.statusByCondition[name] != null;
  if (pr.is_in_merge_queue && mapped("merge-queue")) return { name: "merge-queue" };
  if (pr.mergeable === "CONFLICTING" && mapped("conflicts")) return { name: "conflicts" };
  if (pr.mergeable === "UNKNOWN" && mapped("conflicts")) return { name: null };
  if (pr.is_draft) return { name: "draft" };
  if (unansweredChangeRequest(pr)) return { name: "changes-requested" };
  const wanted = lower(config.mergingCheckName.trim());
  const merging =
    (!!wanted && pr.checks.some((c) => lower(c.name.trim()) === wanted && checkState(c) === "success")) ||
    approvedByTeamAndEveryone(pr);
  if (merging) return { name: "merging" };
  if (awaitsOnlyTeam(pr)) return { name: "pm-approval" };
  return { name: "review" };
}

const trigger = scope.trigger;

for (const ws of scope.workspaces) {
  const pr = ws.pr;
  if (!pr) {
    if (config.newWorkspaceStatus && !ws.status && ws.created_ms >= scope.now - 5 * 60_000) {
      actions.push({ method: "workspace.set", params: { id: ws.id, status: config.newWorkspaceStatus } });
    }
    continue;
  }
  const { name, remove } = condition(ws, pr);
  if (remove) {
    actions.push({ method: "workspace.remove", params: { id: ws.id, force: name === "reviewing" } });
    continue;
  }
  const status = name ? config.statusByCondition[name] : null;
  if (status && status !== ws.status) {
    actions.push({ method: "workspace.set", params: { id: ws.id, status } });
    if (name === "review" && config.reviewEnteredCommand) {
      actions.push({ method: "terminal.create", params: { workspace: ws.id, command: ["sh", "-c", config.reviewEnteredCommand] } });
    }
    if (name === "conflicts" && config.resolveConflictsWithAgent) {
      actions.push({
        method: "agent.create",
        params: { workspace: ws.id, provider: config.reviewInbox.agent, title: `Resolve conflicts #${pr.number}`, prompt: `PR #${pr.number} has conflicts with ${pr.base_ref_name}. Merge it in and resolve them.` },
      });
    }
  }
  // Review head sync: a review workspace follows the PR's head and target branch.
  if (
    authoredBySomeoneElse(pr) && pr.state === "OPEN" && pr.head_ref_oid && ws.branch === pr.head_ref_name &&
    (!ws.head_is_local || !ws.base_follows_target)
  ) {
    actions.push({ method: "workspace.sync_to_pr_head", params: { id: ws.id } });
  }
}

if (config.reviewInbox.enabled && Array.isArray(scope.inbox)) {
  for (const pr of scope.inbox) {
    if (pr.workspace || lower(pr.author) === viewer) continue;
    const since = pr.viewer_latest_review?.commit_oid ?? null;
    const key = `#${pr.number}@${since ?? ""}`;
    if (memory.handled[key]) continue;
    memory.handled[key] = scope.now;
    const vars = { pr, since };
    actions.push({
      method: "review.workspace",
      params: {
        project: scope.project.id,
        number: pr.number,
        status: config.statusByCondition.reviewing,
        since,
        layout: "review",
        agent: {
          provider: config.reviewInbox.agent,
          prompt: fill(since ? config.reviewInbox.rereviewPrompt : config.reviewInbox.prompt, vars),
          title: `Review #${pr.number}`,
        },
      },
    });
  }
}

if (trigger.kind === "event") {
  for (const event of "events" in trigger ? trigger.events : []) {
    const ws = scope.workspaces.find((w) => w.id === event.workspace);
    const pr = ws?.pr;
    if (!ws || !pr) continue;
    const once = (key: string) => (memory.notified[key] ? false : (memory.notified[key] = scope.now, true));
    if (event.name === "checks.failed" && !authoredBySomeoneElse(pr)) {
      const failed = (event.payload.failed as string[]).join(", ");
      if (config.notify.ciFailed && once(`ci:${pr.number}@${pr.head_ref_oid}`)) {
        actions.push({ method: "notification.send", params: { workspace: ws.id, title: `CI failed on #${pr.number}`, body: failed } });
      }
      const triaged = `#${pr.number}`;
      if (config.ciTriage.enabled && (memory.triaged[triaged] ?? 0) < config.ciTriage.maxPerPr) {
        memory.triaged[triaged] = (memory.triaged[triaged] ?? 0) + 1;
        actions.push({
          method: "agent.create",
          params: { workspace: ws.id, provider: config.reviewInbox.agent, title: "Fix checks", prompt: fill(config.ciTriage.prompt, { pr, failed }) },
        });
      }
    }
    if (event.name === "pr.state_changed" && event.payload.to === "MERGED" && config.notify.merged && once(`merged:${pr.number}`)) {
      actions.push({ method: "notification.send", params: { workspace: ws.id, title: `#${pr.number} merged`, body: pr.title } });
    }
  }
}

// Ledgers stay small: forget anything older than two weeks.
for (const ledger of [memory.handled, memory.triaged, memory.notified]) {
  for (const [key, at] of Object.entries(ledger)) if (scope.now - (at as number) > 14 * 86_400_000) delete ledger[key];
}

const decision: Decision = { actions, memory, buttons };
console.log(JSON.stringify(decision));
