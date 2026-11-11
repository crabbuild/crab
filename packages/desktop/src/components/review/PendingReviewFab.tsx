import { FileEdit } from "lucide-react";
import { usePendingReview } from "../../review/PendingReviewContext";

// ── Types ────────────────────────────────────────────────────────────

export interface PendingReviewFabProps {
  onClick(): void;
}

// ── Component ────────────────────────────────────────────────────────

/**
 * Floating action button anchored to the bottom-right of the PR view.
 *
 * Displays the count of pending review comments and opens the
 * ReviewSubmissionDialog on click. Hidden when there are no pending comments.
 */
export function PendingReviewFab({ onClick }: PendingReviewFabProps) {
  const { count } = usePendingReview();

  if (count === 0) return null;

  return (
    <button
      type="button"
      className="fixed bottom-4 right-4 z-50 inline-flex items-center gap-2 rounded-lg border border-border bg-card px-3 py-2 text-xs font-medium text-foreground shadow-lg hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
      onClick={onClick}
      aria-label={`${count} pending review comment${count === 1 ? "" : "s"}. Click to open review submission.`}
    >
      <FileEdit size={14} className="text-warning" />
      <span>
        {count} pending
      </span>
    </button>
  );
}
