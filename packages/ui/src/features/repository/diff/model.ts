import type { SelectedLineRange } from "@pierre/diffs";
import { type ReactNode } from "react";

export function changePanelId(pathHex: string) {
  return `changed-file-${pathHex}`;
}

export interface InlineDiffThread {
  number: number;
  path_hex: string;
  side: "old" | "new";
  start_line: number;
  end_line: number;
  author: string;
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

export interface DiffReviewState {
  threads: InlineDiffThread[];
  onLineSelected: (pathHex: string, range: SelectedLineRange | null) => void;
  onThreadSelected: (thread: InlineDiffThread) => void;
  onDiffSelected?: (thread: InlineDiffThread) => void;
  fileSummary?: (pathHex: string) => DiffReviewFileSummary | null;
  fileSummaryVersion?: string;
  renderAnnotation?: (thread: InlineDiffThread) => ReactNode;
}
