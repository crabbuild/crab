import { type CommentMinimization } from "@/components/discussion/conversation/comment-visibility";
import type { DiscussionAuthor } from "@/components/discussion/author";
import type { SelectedLineRange } from "@pierre/diffs";
import { type ReactNode } from "react";
import type { Change } from "@/lib/api";

export function changePanelId(pathHex: string) {
  return `changed-file-${pathHex}`;
}

export interface InlineDiffThread {
  minimization?: CommentMinimization | null;
  number: number;
  path_hex: string;
  side: "old" | "new";
  start_line: number;
  end_line: number;
  author: DiscussionAuthor;
  body: string;
  suggested_text: string | null;
  resolved: boolean;
  outdated: boolean;
  vanished: boolean;
}

export interface DiffReviewFileSummary {
  total: number;
  open: number;
  resolved: number;
  outdated: number;
}

export interface DiffTarget {
  pathHex: string;
  thread: number;
}

export interface DiffReviewState {
  preferenceScope: string;
  threads: InlineDiffThread[];
  target?: DiffTarget;
  isViewed?: (file: Change) => boolean;
  onLineSelected: (file: Change, range: SelectedLineRange | null) => void;
  onThreadSelected: (thread: InlineDiffThread) => void;
  fileSummary?: (pathHex: string) => DiffReviewFileSummary | null;
  fileSummaryPartial?: boolean;
  fileSummaryVersion?: string;
  renderAnnotation?: (thread: InlineDiffThread) => ReactNode;
  renderFileActions?: (file: Change) => ReactNode;
}
