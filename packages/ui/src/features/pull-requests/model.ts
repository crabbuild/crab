import type { DiscussionTotals, ListLabel } from "@/components/discussion/list";
import type { DiscussionAuthor } from "@/components/discussion/author";
import { type RepositoryAssignee, type RepositoryLabel } from "@/lib/api";
import { type ReactionSummary } from "@/components/discussion/reactions";
import { type InlineDiffThread } from "@/features/repository/diff/model";

export interface PullComment {
  reactions: ReactionSummary;
  number: number;
  author: DiscussionAuthor;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

export interface PullSummary {
  draft: boolean;
  number: number;
  title: string;
  state: "open" | "closed" | "merged";
  author: DiscussionAuthor;
  base_ref: string;
  head_ref: string;
  created_at: number;
  updated_at: number;
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
}

export type PullListItem = Omit<PullSummary, "labels"> &
  DiscussionTotals & {
    kind: "pull";
    labels: ListLabel[];
  };

export interface PullRequest extends PullSummary {
  reactions: ReactionSummary;
  body: string;
  version: number;
  can_edit: boolean;
  can_label: boolean;
  can_assign: boolean;
  base_oid: string;
  head_oid: string;
  original_base_oid: string | null;
  original_head_oid: string | null;
  can_manage: boolean;
  can_decide: boolean;
  can_merge: boolean;
  branches_available: boolean;
  merge: {
    author: DiscussionAuthor;
    method: "fast_forward" | "merge_commit";
    commit_oid: string;
    message: string;
    created_at: number;
  } | null;
  merge_pending: {
    request_id: string;
    author: DiscussionAuthor;
    method: "fast_forward" | "merge_commit";
    pull_version: number;
    base_oid: string;
    head_oid: string;
    message: string;
    created_at: number;
  } | null;
  merge_requirements: {
    protected: boolean;
    required_approvals: number;
    approvals: number;
    changes_requested: number;
    checks_satisfied: boolean;
    checks: Array<{
      context: string;
      state: "error" | "failure" | "pending" | "success" | null;
      description: string | null;
      target_url: string | null;
      author: string | null;
      updated_at: number | null;
      run_id: number | null;
    }>;
    satisfied: boolean;
  };
}

export type ReviewState = "commented" | "approved" | "changes_requested";

export interface PullReview {
  reactions: ReactionSummary;
  number: number;
  author: DiscussionAuthor;
  body: string;
  state: ReviewState;
  commit_oid: string;
  current: boolean;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}

export interface PullReviewThread extends InlineDiffThread {
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
