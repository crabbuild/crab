export type RulesetId = string | { organization: string; ruleset: string };

import { type CommentMinimization } from "@/components/discussion/conversation/comment-visibility";
import type { DiscussionTotals, ListLabel } from "@/components/discussion/list";
import type { DiscussionAuthor } from "@/components/discussion/author";
import type { RepositoryAssignee } from "@/lib/contracts";
import { type RepositoryLabel } from "@/lib/api";
import { type ReactionSummary } from "@/components/discussion/reactions";
import { type InlineDiffThread } from "@/features/repository/diff/model";

export interface PullSummary {
  draft: boolean;
  number: number;
  title: string;
  state: "open" | "closed" | "merged";
  author: DiscussionAuthor;
  base_ref: string;
  head_ref: string;
  created_at: number;
  closed_at: number | null;
  merged_at: number | null;
  updated_at: number;
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
}

export type PullListItem = Omit<PullSummary, "labels"> &
  DiscussionTotals & {
    version: number;
    kind: "pull";
    rollup: import("./rollups").PullRollup;
    labels: ListLabel[];
  };

export type MergeMethod = "merge_commit" | "squash" | "rebase" | "fast_forward";

export interface RequiredCheckReport {
  state: "error" | "failure" | "pending" | "success" | null;
  description: string | null;
  target_url: string | null;
  author: string | null;
  updated_at: number | null;
  run_id: number | null;
  run_status?: "queued" | "in_progress" | "completed" | null;
  conclusion?: string | null;
  provider?:
    | "github_actions"
    | "crab_check_run"
    | "crab_status"
    | "crab_status_or_check_run";
}

export interface PullRequest extends PullSummary {
  has_edit_history?: boolean;
  reactions: ReactionSummary;
  body: string;
  version: number;
  can_edit: boolean;
  can_label: boolean;
  can_assign: boolean;
  can_apply_suggestions: boolean;
  can_update_branch?: boolean;
  base_oid: string;
  base_revision?: number | null;
  can_change_base?: boolean;
  head_oid: string;
  head_repository?: { id: string; owner: string; name: string } | null;
  github_actions_ref?: string | null;
  original_base_oid: string | null;
  original_head_oid: string | null;
  can_manage: boolean;
  can_decide: boolean;
  review_counts_for_merge?: boolean;
  can_merge: boolean;
  auto_merge_configured?: boolean;
  merge_queued?: boolean;
  merge_queue_requested?: boolean;
  merge_queue_required?: boolean;
  branches_available: boolean;
  merge: {
    author: DiscussionAuthor;
    method: MergeMethod;
    commit_oid: string;
    message: string;
    created_at: number;
    bypass_rulesets?: RulesetId[];
    tested_candidate_oid?: string | null;
  } | null;
  merge_pending: {
    request_id: string;
    author: DiscussionAuthor;
    method: MergeMethod;
    pull_version: number;
    base_oid: string;
    head_oid: string;
    message: string;
    created_at: number;
    bypass_rulesets?: RulesetId[];
    tested_candidate_oid?: string | null;
  } | null;
  merge_methods: MergeMethod[];
  bypass_merge_methods?: MergeMethod[];
  merge_requirements: {
    rulesets?: string[];
    bypass_options?: { id: RulesetId; name: string }[];
    protected: boolean;
    updates_restricted?: boolean;
    linear_history_required?: boolean;
    required_approvals: number;
    approvals: number;
    changes_requested: number;
    code_owners?: CodeOwnersReport | null;
    unresolved_conversations?: number | null;
    head_up_to_date?: boolean | null;
    checks_satisfied: boolean;
    checks: Array<
      RequiredCheckReport & {
        context: string;
        integration_id?: string | null;
        head_sha?: string;
        additional_report?: RequiredCheckReport;
      }
    >;
    satisfied: boolean;
  };
}

export interface CodeOwnersReport {
  path: string | null;
  base_oid: string | null;
  head_oid: string | null;
  invalid_lines: number;
  affected_paths: number;
  missing_paths?: number;
  problem: string | null;
  groups: {
    line: number;
    owners: string[];
    paths: string[];
    affected: number;
    approved?: boolean;
    has_eligible_owner?: boolean;
  }[];
}

export interface PullReviewThread extends InlineDiffThread {
  has_edit_history?: boolean;
  reactions: ReactionSummary;
  path: string;
  base_oid: string;
  head_oid: string;
  old_blob_oid: string | null;
  new_blob_oid: string | null;
  resolved_by: DiscussionAuthor | null;
  resolved_at: number | null;
  current: boolean;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
  can_reply: boolean;
  can_resolve: boolean;
}

export interface PullReviewReply {
  has_edit_history?: boolean;
  minimization?: CommentMinimization | null;
  reactions: ReactionSummary;
  number: number;
  author: DiscussionAuthor;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

export interface Page<T, Cursor = number> {
  items: T[];
  next: Cursor | null;
}
