export type Trigger =
  | { kind: "tick" }
  | { kind: "manual" }
  | { kind: "event"; name: "button"; workspace: string; payload: { id: string } }
  | {
      kind: "event";
      name: EventName;
      workspace: string;
      payload: any;
      /** Everything that changed in this burst; the trigger's own fields are the first. */
      events: { name: EventName; workspace: string; payload: any }[];
    }
  | { kind: "event"; name: string; workspace?: string | null; payload: any };
/** Changes Icarus notices, debounced into one run per burst. Custom `emit` names are any string. */
export type EventName =
  | "workspace.created"
  | "workspace.status_changed"
  | "linear.changed"
  | "pr.opened"
  | "pr.state_changed"
  | "pr.head_changed"
  | "pr.base_changed"
  | "checks.changed"
  | "checks.failed"
  | "review.requested"
  | "review.submitted"
  | "git.pushed"
  | "agent.finished"
  | "agent.waiting";

export interface Scope {
  trigger: Trigger;
  run_id: string;
  dry_run: boolean;
  /** Epoch ms. */
  now: number;
  project: { id: string; name: string; root: string; remote: string | null; base: string | null };
  workspaces: Workspace[];
  /** Open PRs in this project requesting the viewer's review. */
  inbox: InboxPullRequest[] | { error: string };
  /** Members (logins) of each team listed in `automations.teams`, keyed "org/team". */
  teams: Record<string, string[]>;
  settings: {
    statuses: { id: StatusId; label?: string; color?: string; icon?: string }[];
    prompts: Record<PromptId, string>;
  };
  /** What the script returned as `memory` last time, or `{}`. */
  memory: any;
  /** The scope of the last run that applied an action, or null. */
  previous: Omit<Scope, "trigger" | "run_id" | "dry_run" | "now" | "memory" | "previous"> | null;
  /** GitHub login, once known. */
  viewer: string | null;
  icarus_version: string;
}

export interface Workspace {
  id: string;
  name: string;
  title: string | null;
  path: string;
  branch: string | null;
  base: string | null;
  status: StatusId | null;
  pinned: boolean;
  hidden: boolean;
  origin: string;
  created_ms: number;
  activity_ms: number | null;
  issue: { provider: string; id: string; title?: string; url?: string; [key: string]: any } | null;
  git: Git | null;
  git_error?: string;
  pr: PullRequest | null;
  pr_known?: boolean;
  pr_error?: string;
  /** The built-in next step (the same as Orca's); return it as a button to keep the default. */
  suggested_action: { kind: string; label: string; agent_step: boolean } | null;
  agents: Agent[];
  /** The PR the workspace was created from, if any. */
  linked_pr: { number: number; [key: string]: any } | null;
  /** Draft review comments not yet sent. */
  review_pending: number;
  /** Whether the local HEAD is the PR's head commit. */
  head_is_local?: boolean;
  /** Whether the workspace base follows the PR's target branch. */
  base_follows_target?: boolean;
}

export interface InboxPullRequest extends PullRequest {
  /** The workspace already on this PR, if any. */
  workspace: string | null;
  is_cross_repository: boolean;
}

export interface Git {
  branch: string | null;
  head: string | null;
  upstream: string | null;
  ahead: number;
  behind: number;
  uncommitted: boolean;
  conflicts: boolean;
  default_branch: boolean;
}

export interface Agent {
  id: string;
  provider: string;
  title: string | null;
  state: "starting" | "idle" | "working" | "waiting" | "sleeping" | "exited";
  mode: string;
  turns: number;
  created_ms: number;
  last_activity_ms: number;
}

export type CheckState = "success" | "failure" | "pending" | "neutral";

export interface Check {
  name: string;
  kind:
    | { type: "run"; status: string; conclusion: string | null }
    | { type: "status"; state: string };
  url: string | null;
  required: boolean | null;
  started_at?: string;
  completed_at?: string;
}

export interface Review {
  login: string;
  state: "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED" | "PENDING";
  commit_oid: string | null;
}

export interface PullRequest {
  repo: { owner: string; name: string };
  number: number;
  title: string;
  url: string;
  author: string | null;
  state: "OPEN" | "CLOSED" | "MERGED";
  is_draft: boolean;
  head_ref_name: string;
  base_ref_name: string;
  head_ref_oid: string;
  base_ref_oid: string | null;
  mergeable: "MERGEABLE" | "CONFLICTING" | "UNKNOWN" | null;
  merge_state_status:
    | "CLEAN" | "HAS_HOOKS" | "UNSTABLE" | "BEHIND" | "BLOCKED" | "DIRTY" | "DRAFT" | "UNKNOWN" | null;
  review_decision: "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;
  requested_reviewers: ({ kind: "user"; login: string } | { kind: "team"; slug: string })[];
  latest_reviews: Review[];
  latest_opinionated_reviews: Review[] | null;
  viewer_latest_review: { state: string; commit_oid: string | null } | null;
  checks: Check[];
  checks_rollup: CheckState | null;
  is_in_merge_queue: boolean;
  merge_queue_entry: { state: string; position: number | null; eta_seconds: number | null } | null;
  merge_queue_required: boolean | null;
  auto_merge: { method: "MERGE" | "SQUASH" | "REBASE" | null; enabled_by: string | null } | null;
  auto_merge_allowed: boolean | null;
  stack: { number: number; position: number; size: number; base_ref_name: string; entries: any[] } | null;
  updated_at?: string;
  additions?: number;
  deletions?: number;
  changed_files?: number;
}

/** One server method call; `icarus <command> --help` documents each method's params. */
export interface Action {
  method: Method;
  params?: Record<string, any> & { prompt_id?: PromptId };
}

export interface Button {
  /** Sent back as `trigger.payload.id` when clicked. */
  id: string;
  label: string;
  icon?: string;
  style?: "primary" | "outlined" | "subtle" | "danger";
  tooltip?: string;
  /** Why the script chose it; shown by `icarus workspace button --explain`. */
  reason?: string;
}

export interface Decision {
  actions?: Action[];
  memory?: any;
  /** Per workspace id; the first is the main button. `[]` shows none; leaving a workspace out keeps the built-in. */
  buttons?: Record<string, Button[]>;
}
