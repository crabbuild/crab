import type { DiscussionAuthor } from "../author";
import type { ReactionSummary } from "../reactions";

export interface Comment {
  reactions: ReactionSummary;
  number: number;
  author: DiscussionAuthor;
  body: string;
  version: number;
  created_at: number;
  updated_at: number;
  can_edit: boolean;
}
export type ReviewState = "commented" | "approved" | "changes_requested";
export interface Review extends Comment {
  state: ReviewState;
  commit_oid: string;
  current: boolean | null;
}
export type Target =
  | { kind: "issue"; number: number }
  | { kind: "pull"; number: number }
  | { kind: "issue_comment"; issue: number; number: number }
  | {
      kind: "pull_comment" | "pull_review" | "pull_thread";
      pull: number;
      number: number;
    }
  | { kind: "pull_reply"; pull: number; thread: number; number: number };
export type Action =
  | { kind: "issue_closed"; reason: "completed" | "not_planned" }
  | {
      kind: "issue_close_reason_changed";
      from: "completed" | "not_planned" | null;
      to: "completed" | "not_planned";
    }
  | {
      kind:
        | "drafted"
        | "ready_for_review"
        | "created"
        | "updated"
        | "closed"
        | "reopened"
        | "pinned"
        | "unpinned"
        | "approved"
        | "changes_requested"
        | "commented"
        | "merged"
        | "resolved"
        | "unresolved";
    }
  | { kind: "review_requested" | "review_request_cancelled"; reviewer: string }
  | {
      kind:
        | "milestone_assigned"
        | "milestone_removed"
        | "issue_linked"
        | "issue_unlinked";
      number: number;
      title: string;
    };
export interface ActivityEvent {
  number: number;
  target: Target;
  title: string;
  actor: DiscussionAuthor;
  action: Action;
  created_at: number;
}
export type Entry =
  | { kind: "comment"; value: Comment }
  | { kind: "review"; value: Review }
  | { kind: "activity"; value: ActivityEvent };
export interface Page {
  items: Entry[];
  start: string | null;
  end: string | null;
  has_earlier: boolean;
  has_later: boolean;
  head_oid: string | null;
}
export function entryKey(entry: Entry) {
  return `${entry.kind}:${entry.value.number}`;
}
export function entryId(entry: Entry) {
  const prefix =
    entry.kind === "comment"
      ? "issuecomment"
      : entry.kind === "review"
        ? "pullrequestreview"
        : "event";
  return `${prefix}-${entry.value.number}`;
}
export function entryVersion(entry: Entry) {
  return entry.kind === "activity" ? 1 : entry.value.version;
}
const order = { comment: 0, review: 1, activity: 2 };
export function entryOrder(left: Entry, right: Entry) {
  return (
    left.value.created_at - right.value.created_at ||
    order[left.kind] - order[right.kind] ||
    left.value.number - right.value.number
  );
}
