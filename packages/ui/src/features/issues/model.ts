import type { DiscussionTotals, ListLabel } from "@/components/discussion/list";
import type { DiscussionAuthor } from "@/components/discussion/author";
import type { Comment } from "@/components/discussion/conversation/model";
import type { RepositoryLabel, RepositoryAssignee } from "@/lib/api";

export type IssueStateReason = "completed" | "not_planned" | "reopened";
export interface Issue extends Comment {
  title: string;
  state: "open" | "closed";
  state_reason: IssueStateReason | null;
  closed_at: number | null;
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
  can_label: boolean;
  can_assign: boolean;
  can_close: boolean;
}
export type IssueListItem = DiscussionTotals & {
  kind: "issue";
  labels: ListLabel[];
  author: DiscussionAuthor;
} & Pick<
    Issue,
    | "number"
    | "title"
    | "state"
    | "state_reason"
    | "closed_at"
    | "created_at"
    | "assignees"
  >;
export interface Page<T> {
  items: T[];
  next: number | null;
}
