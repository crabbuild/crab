import type { ReactionSummary } from "@/components/discussion/reactions";
import type { RepositoryLabel, RepositoryAssignee } from "@/lib/api";

export interface Comment {
  reactions: ReactionSummary;
  number: number;
  author: string;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}
export interface Issue extends Comment {
  title: string;
  state: "open" | "closed";
  labels: RepositoryLabel[];
  assignees: RepositoryAssignee[];
  can_label: boolean;
  can_assign: boolean;
}
export type IssueListItem = Pick<
  Issue,
  | "number"
  | "title"
  | "state"
  | "author"
  | "created_at"
  | "labels"
  | "assignees"
>;
export interface Page<T> {
  items: T[];
  next: number | null;
}
