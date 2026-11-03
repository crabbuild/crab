import type { BoardFilter, TrackerSource } from "../kanban-types";

/**
 * Normalized representation of an issue fetched from an external tracker.
 */
export interface ExternalIssue {
  id: string;
  identifier?: string;
  title: string;
  description: string;
  status: string;
  assignee?: string;
  labels: string[];
  priority: string;
  priorityValue?: number | null;
  url: string;
  createdAt?: number;
  updatedAt: number;
  blockedBy?: Array<{
    id?: string | null;
    identifier?: string | null;
    state?: string | null;
  }>;
}

/**
 * Common interface that all tracker adapters (GitHub Issues, Jira, GitHub
 * Projects v2) must implement. The Board Sync layer programs against this
 * interface so the sync engine is tracker-agnostic.
 */
export interface TrackerAdapter {
  readonly source: TrackerSource;

  /** Authenticate with the external tracker (OAuth, PAT, or token). */
  authenticate(): Promise<void>;

  /** Fetch issues matching the given filter criteria. */
  fetchIssues(filter: BoardFilter): Promise<ExternalIssue[]>;

  /** Transition an issue to a new status on the external tracker. */
  transitionIssue(issueId: string, targetStatus: string): Promise<void>;

  /** Post a comment on an issue. */
  addComment(issueId: string, body: string): Promise<void>;

  /** Link a pull request URL to an issue. */
  linkPr(issueId: string, prUrl: string): Promise<void>;
}
