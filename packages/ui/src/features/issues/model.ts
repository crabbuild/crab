import type { DiscussionTotals, ListLabel } from "@/components/discussion/list";
import type { DiscussionAuthor } from "@/components/discussion/author";
import type { Comment } from "@/components/discussion/conversation/model";
import type { IssueType, RepositoryAssignee } from "@/lib/contracts";
import type { RepositoryLabel } from "@/lib/api";

export type IssueStateReason = "completed" | "not_planned" | "reopened";
export interface Issue extends Comment {
  title: string;
  state: "open" | "closed";
  blocked?: boolean;
  issue_type?: IssueType | null;
  issue_types?: IssueType[];
  issue_type_version?: number;
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
    | "version"
    | "title"
    | "state"
    | "blocked"
    | "issue_type"
    | "state_reason"
    | "closed_at"
    | "created_at"
    | "assignees"
  >;
export interface Page<T> {
  items: T[];
  next: number | null;
}
