import { Textarea } from "@/components/ui/textarea";
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { FileText, Loader2, Send, X } from "lucide-react";
import type { PendingComment, Verdict } from "../../review/types";

// ── Types ────────────────────────────────────────────────────────────

export interface ReviewSubmissionDialogProps {
  open: boolean;
  onClose(): void;
  pendingComments: PendingComment[];
  onRemoveComment(id: string): void;
  onSubmit(body: string, verdict: Verdict): Promise<void>;
  isAuthor: boolean;
}

// ── Helpers ──────────────────────────────────────────────────────────

/** Truncate text to maxLen characters, appending ellipsis if truncated. */
function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + "…";
}

/** Group pending comments by file path, preserving insertion order. */
function groupByFile(comments: PendingComment[]): Map<string, PendingComment[]> {
  const map = new Map<string, PendingComment[]>();
  for (const c of comments) {
    const group = map.get(c.path);
    if (group) {
      group.push(c);
    } else {
      map.set(c.path, [c]);
    }
  }
  return map;
}

// ── Verdict options ──────────────────────────────────────────────────

const VERDICT_OPTIONS: { value: Verdict; label: string }[] = [
  { value: "comment", label: "Comment" },
  { value: "approve", label: "Approve" },
  { value: "request_changes", label: "Request Changes" },
];

// ── Component ────────────────────────────────────────────────────────

/**
 * Modal dialog for submitting a review with a body, verdict, and
 * accumulated pending inline comments.
 *
 * Displays pending comments grouped by file, allows removing individual
 * comments, and validates that either a body or at least one comment is
 * present before submission.
 */
export function ReviewSubmissionDialog({
  open,
  onClose,
  pendingComments,
  onRemoveComment,
  onSubmit,
  isAuthor,
}: ReviewSubmissionDialogProps) {
  const [body, setBody] = useState("");
  const [verdict, setVerdict] = useState<Verdict>("comment");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Focus textarea when dialog opens.
  useEffect(() => {
    if (open) {
      // Small delay to allow the modal to render before focusing.
      const timer = setTimeout(() => textareaRef.current?.focus(), 50);
      return () => clearTimeout(timer);
    }
  }, [open]);

  // Reset state when dialog opens.
  useEffect(() => {
    if (open) {
      setBody("");
      setVerdict("comment");
      setSubmitting(false);
      setError(null);
    }
  }, [open]);

  const grouped = useMemo(() => groupByFile(pendingComments), [pendingComments]);

  const canSubmit = body.trim().length > 0 || pendingComments.length > 0;

  const handleSubmit = useCallback(async () => {
    if (!canSubmit || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(body.trim(), verdict);
      onClose();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Submission failed. Please try again.";
      setError(message);
      setSubmitting(false);
    }
  }, [canSubmit, submitting, body, verdict, onSubmit, onClose]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent<HTMLDivElement>) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleSubmit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    },
    [handleSubmit, onClose],
  );

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      role="dialog"
      aria-modal="true"
      aria-labelledby="review-dialog-title"
      onKeyDown={handleKeyDown}
    >
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/60"
        onClick={onClose}
        aria-hidden="true"
      />

      {/* Dialog panel */}
      <div className="relative z-10 flex max-h-[80vh] w-full max-w-lg flex-col rounded-lg border border-border bg-card shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <h2 id="review-dialog-title" className="text-sm font-semibold text-foreground">
            Submit Review
          </h2>
          <button
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onClose}
            aria-label="Close dialog"
          >
            <X size={14} />
          </button>
        </div>

        {/* Scrollable content */}
        <div className="flex-1 overflow-y-auto px-4 py-3 space-y-4">
          {/* Review body textarea */}
          <div>
            <Textarea
              ref={textareaRef}
              className="w-full resize-y rounded border border-border bg-background p-2.5 text-[11px] leading-relaxed text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
              rows={4}
              placeholder="Leave a review summary…"
              value={body}
              onChange={(e) => {
                setBody(e.target.value);
                if (error) setError(null);
              }}
              disabled={submitting}
              aria-label="Review body"
            />
          </div>

          {/* Pending comments grouped by file */}
          {pendingComments.length > 0 && (
            <div>
              <h3 className="mb-2 text-[11px] font-medium text-muted-foreground">
                Pending Comments ({pendingComments.length})
              </h3>
              <div className="space-y-2">
                {Array.from(grouped.entries()).map(([filePath, comments]) => (
                  <div key={filePath} className="rounded border border-border/50 bg-background">
                    {/* File header */}
                    <div className="flex items-center gap-1.5 border-b border-border/30 px-2.5 py-1.5">
                      <FileText size={10} className="text-muted-foreground" />
                      <span className="truncate font-mono text-[10px] text-foreground/80">
                        {filePath}
                      </span>
                    </div>
                    {/* Comments in this file */}
                    <div className="divide-y divide-border/20">
                      {comments.map((comment) => (
                        <div
                          key={comment.id}
                          className="flex items-start justify-between gap-2 px-2.5 py-1.5"
                        >
                          <div className="min-w-0 flex-1">
                            <span className="mr-1.5 font-mono text-[10px] text-muted-foreground">
                              L{comment.line}:
                            </span>
                            <span className="text-[10px] text-foreground/80">
                              {truncate(comment.body, 100)}
                            </span>
                          </div>
                          <button
                            type="button"
                            className="shrink-0 inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[9px] text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                            onClick={() => onRemoveComment(comment.id)}
                            disabled={submitting}
                            aria-label={`Remove comment on line ${comment.line}`}
                          >
                            <X size={9} />
                            remove
                          </button>
                        </div>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}

          {/* Verdict radio buttons */}
          <div>
            <h3 className="mb-2 text-[11px] font-medium text-muted-foreground">Verdict</h3>
            <div className="flex items-center gap-4">
              {VERDICT_OPTIONS.map(({ value, label }) => {
                const disabled =
                  submitting || (isAuthor && (value === "approve" || value === "request_changes"));
                return (
                  <label
                    key={value}
                    className={`inline-flex items-center gap-1.5 text-[11px] ${
                      disabled ? "cursor-not-allowed opacity-50" : "cursor-pointer"
                    }`}
                  >
                    <input
                      type="radio"
                      name="verdict"
                      value={value}
                      checked={verdict === value}
                      onChange={() => setVerdict(value)}
                      disabled={disabled}
                      className="accent-primary"
                    />
                    <span className="text-foreground">{label}</span>
                  </label>
                );
              })}
            </div>
            {isAuthor && (
              <p className="mt-1 text-[9px] text-muted-foreground">
                Authors cannot approve or request changes on their own PR.
              </p>
            )}
          </div>
        </div>

        {/* Footer */}
        <div className="border-t border-border px-4 py-3">
          {/* Error message */}
          {error && (
            <p className="mb-2 text-[10px] text-destructive" role="alert">
              {error}
            </p>
          )}

          {/* Validation hint */}
          {!canSubmit && (
            <p className="mb-2 text-[10px] text-warning" role="alert">
              A review body or at least one comment is required.
            </p>
          )}

          <div className="flex items-center justify-between">
            <span className="text-[9px] text-muted-foreground">
              {navigator.platform?.includes("Mac") ? "⌘" : "Ctrl"}+Enter to submit
            </span>
            <div className="flex items-center gap-2">
              <button
                type="button"
                className="rounded px-3 py-1.5 text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground"
                onClick={onClose}
                disabled={submitting}
              >
                Cancel
              </button>
              <button
                type="button"
                className="inline-flex items-center gap-1.5 rounded bg-primary px-3 py-1.5 text-[11px] font-medium text-primary-foreground hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!canSubmit || submitting}
                onClick={handleSubmit}
                aria-label="Submit review"
              >
                {submitting ? (
                  <Loader2 size={12} className="animate-spin" />
                ) : (
                  <Send size={12} />
                )}
                {submitting ? "Submitting…" : "Submit Review"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
