/**
 * Shared type definitions for the PR review workflow.
 */

/** A locally-accumulated draft comment that has not yet been published to the forge. */
export interface PendingComment {
  /** Client-generated UUID */
  id: string;
  /** Relative file path within the repo */
  path: string;
  /** Line number in the diff (new-side for additions, old-side for deletions) */
  line: number;
  /** End line for multi-line comments; null for single-line */
  endLine: number | null;
  /** Which side of the diff: "LEFT" (deletion) or "RIGHT" (addition/context) */
  side: "LEFT" | "RIGHT";
  /** Markdown body of the comment */
  body: string;
  /** When the comment was locally created (ISO string) */
  createdAt: string;
  /** If true, the anchored line content has changed since this comment was written */
  outdated: boolean;
  /** Original line content for suggestion pre-fill */
  originalContent?: string;
}

/** A conversation anchored to a specific file and line within a PR diff. */
export interface ReviewThread {
  /** Thread ID from the forge (GitHub discussion/thread ID) */
  threadId: string;
  /** File path the thread is anchored to */
  path: string;
  /** Line number */
  line: number;
  /** Whether the thread is resolved */
  resolved: boolean;
  /** Whether the thread's code context is outdated */
  outdated: boolean;
  /** Ordered list of comments in this thread */
  comments: ThreadComment[];
}

/** A single comment within a ReviewThread. */
export interface ThreadComment {
  id: string;
  author: string;
  body: string;
  createdAt: string;
  /** Whether this comment has been edited */
  edited: boolean;
  /** Whether the current user authored this comment */
  isMine: boolean;
  /** Suggestion metadata if this comment contains a code suggestion */
  suggestion?: {
    originalCode: string;
    suggestedCode: string;
    applied: boolean;
    outdated: boolean;
  };
}

/** A single file's progress entry recording when it was marked as viewed. */
export interface FileProgressEntry {
  path: string;
  /** The head SHA when the file was marked as viewed */
  viewedAtSha: string;
  /** Hash of the patch content at time of marking (for staleness detection) */
  patchHash: string;
}

/** Persisted state for file review progress on a PR. */
export interface FileProgressState {
  prNumber: number;
  headSha: string;
  files: FileProgressEntry[];
}

/** Identifies the currently focused element in the review navigator. */
export type FocusTarget =
  | { type: "line"; path: string; line: number }
  | { type: "thread"; path: string; threadId: string }
  | null;

/** The outcome chosen when submitting a review. */
export type Verdict = "comment" | "approve" | "request_changes";

/** Shape of an inline comment sent to the forge on review submission. */
export interface ForgeInlineComment {
  path: string;
  line: number;
  side: "LEFT" | "RIGHT";
  body: string;
  /** Start line for multi-line comments */
  start_line?: number;
}
