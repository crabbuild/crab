import { useCallback, useState } from "react";
import { AlertCircle, Check, Clock, GitCommitHorizontal, Loader2 } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────────

export interface CodeSuggestionBlockProps {
  /** The original code that the suggestion replaces */
  originalCode: string;
  /** The suggested replacement code */
  suggestedCode: string;
  /** Whether the suggestion has been applied */
  applied: boolean;
  /** Whether the suggestion is outdated (lines changed since suggestion was made) */
  outdated: boolean;
  /** Whether the current user has write access to apply the suggestion */
  hasWriteAccess: boolean;
  /** Called when user clicks "Apply suggestion" */
  onApply(): Promise<void>;
}

// ── Utility ──────────────────────────────────────────────────────────

/**
 * Extracts a code suggestion block from a comment body.
 *
 * Detects fenced code blocks with the `suggestion` tag and returns
 * the suggested code. The caller provides the original code separately
 * (from the thread's line context).
 *
 * Returns null if no suggestion block is found.
 */
export function parseSuggestionFromBody(
  body: string,
): { originalCode: string; suggestedCode: string } | null {
  // Match ```suggestion ... ``` blocks (with optional language after "suggestion")
  const regex = /```suggestion\b[^\n]*\n([\s\S]*?)```/;
  const match = body.match(regex);
  if (!match) return null;

  const suggestedCode = match[1].replace(/\n$/, "");

  // Extract original code: everything outside the suggestion block is context.
  // For the "before" view, we strip the suggestion block from the body and
  // look for any preceding code fence that might contain the original.
  // In practice, the original code comes from the thread's line context,
  // so we return an empty string here — the caller should provide it.
  return { originalCode: "", suggestedCode };
}

// ── Component ────────────────────────────────────────────────────────

/**
 * Renders a code suggestion as a before/after diff-style view with
 * an "Apply suggestion" button.
 *
 * Shows "Applied" or "Outdated" labels when the suggestion is no longer
 * actionable.
 */
export function CodeSuggestionBlock({
  originalCode,
  suggestedCode,
  applied,
  outdated,
  hasWriteAccess,
  onApply,
}: CodeSuggestionBlockProps) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleApply = useCallback(async () => {
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      await onApply();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Failed to apply suggestion";
      setError(message);
    } finally {
      setLoading(false);
    }
  }, [loading, onApply]);

  const canApply = hasWriteAccess && !applied && !outdated && !loading;

  return (
    <div className="my-1.5 rounded border border-gx-border bg-gx-bg-sub overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-gx-border/50 px-3 py-1.5">
        <div className="flex items-center gap-1.5 text-[10px] text-gx-muted">
          <GitCommitHorizontal size={10} className="shrink-0" />
          <span className="font-medium">Suggested change</span>
        </div>

        <div className="flex items-center gap-1.5">
          {applied && (
            <span className="inline-flex items-center gap-1 rounded bg-green-500/15 px-1.5 py-0.5 text-[9px] font-medium text-green-400">
              <Check size={9} />
              Applied
            </span>
          )}
          {outdated && !applied && (
            <span className="inline-flex items-center gap-1 rounded bg-orange-500/15 px-1.5 py-0.5 text-[9px] font-medium text-orange-400">
              <Clock size={9} />
              Outdated
            </span>
          )}
        </div>
      </div>

      {/* Diff view */}
      <div className="divide-y divide-gx-border/30">
        {/* Before (original) */}
        <div className="px-3 py-1.5">
          <div className="mb-1 text-[9px] font-medium uppercase tracking-wide text-gx-muted">
            Before
          </div>
          <pre className="overflow-x-auto rounded bg-red-500/5 px-2 py-1.5 font-mono text-[11px] leading-relaxed text-gx-text">
            {originalCode.split("\n").map((line, i) => (
              <div key={i} className="flex">
                <span className="mr-2 select-none text-red-400/70">−</span>
                <span>{line}</span>
              </div>
            ))}
          </pre>
        </div>

        {/* After (suggested) */}
        <div className="px-3 py-1.5">
          <div className="mb-1 text-[9px] font-medium uppercase tracking-wide text-gx-muted">
            After
          </div>
          <pre className="overflow-x-auto rounded bg-green-500/5 px-2 py-1.5 font-mono text-[11px] leading-relaxed text-gx-text">
            {suggestedCode.split("\n").map((line, i) => (
              <div key={i} className="flex">
                <span className="mr-2 select-none text-green-400/70">+</span>
                <span>{line}</span>
              </div>
            ))}
          </pre>
        </div>
      </div>

      {/* Footer: Apply button or status */}
      <div className="border-t border-gx-border/50 px-3 py-1.5">
        {error && (
          <div className="mb-1.5 flex items-center gap-1.5 text-[10px] text-red-400">
            <AlertCircle size={10} />
            {error}
          </div>
        )}

        {canApply && (
          <button
            type="button"
            className="inline-flex items-center gap-1 rounded bg-gx-accent/10 px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/20 disabled:cursor-not-allowed disabled:opacity-50"
            onClick={handleApply}
            disabled={loading}
            aria-label="Apply suggestion"
          >
            {loading ? (
              <>
                <Loader2 size={10} className="animate-spin" />
                Applying…
              </>
            ) : (
              <>
                <GitCommitHorizontal size={10} />
                Apply suggestion
              </>
            )}
          </button>
        )}

        {!canApply && !applied && !outdated && !hasWriteAccess && (
          <span className="text-[10px] text-gx-muted">
            Write access required to apply suggestions
          </span>
        )}
      </div>
    </div>
  );
}
