import { useEffect, useId, useRef, useState } from "react";
import clsx from "clsx";
import { AlertTriangle, ShieldAlert, Clipboard } from "lucide-react";
import type { PasteAnalysis } from "../lib/terminal-paste-guard";

/**
 * Confirmation dialog shown by the Paste Guard before writing
 * potentially dangerous clipboard content to the terminal PTY.
 *
 * Three tiers of severity:
 *   1. Multi-line paste — informational preview with line count.
 *   2. Control characters — highlights non-printable chars in the preview.
 *   3. Dangerous patterns — requires explicit "I understand the risk" checkbox.
 *
 * Accessibility:
 *   - `role="alertdialog"` for destructive-action semantics.
 *   - `aria-labelledby` / `aria-describedby` for screen readers.
 *   - Focus lands on Cancel (non-destructive default).
 *   - Esc dismisses via `onCancel`.
 *   - Tab cycles through interactive elements.
 */
export interface PasteConfirmDialogProps {
  /** Analysis result from `analyzePaste()`. */
  analysis: PasteAnalysis;
  /** Called when the user confirms the paste. */
  onConfirm: () => void;
  /** Called when the user cancels the paste. */
  onCancel: () => void;
}

export function PasteConfirmDialog({
  analysis,
  onConfirm,
  onCancel,
}: PasteConfirmDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);
  const [riskAcknowledged, setRiskAcknowledged] = useState(false);

  const hasDangerousPatterns = analysis.reasons.includes("dangerous-pattern");
  const hasControlChars = analysis.reasons.includes("control-chars");
  const hasMultiLine = analysis.reasons.includes("multi-line");

  // Paste button is disabled when dangerous patterns are present
  // and the user hasn't acknowledged the risk.
  const pasteDisabled = hasDangerousPatterns && !riskAcknowledged;

  // Focus management: grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current =
      (document.activeElement as HTMLElement | null) ?? null;
    cancelRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Esc cancels.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onCancel();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onCancel]);

  // Determine the severity icon and color.
  const isDangerous = hasDangerousPatterns;
  const iconColor = isDangerous ? "text-destructive" : "text-warning";
  const iconBg = isDangerous ? "bg-destructive/15" : "bg-warning/15";

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={bodyId}
        className="w-[32rem] max-w-[calc(100vw-2rem)] rounded-lg border border-border bg-card shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-border p-4">
          <div
            className={clsx(
              "mt-0.5 shrink-0 rounded p-2",
              iconBg,
              iconColor,
            )}
          >
            {isDangerous ? (
              <ShieldAlert size={18} aria-hidden="true" />
            ) : (
              <AlertTriangle size={18} aria-hidden="true" />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-foreground"
            >
              {isDangerous
                ? "Dangerous paste detected"
                : "Review paste content"}
            </h2>
            <p
              id={bodyId}
              className="mt-1 text-[12px] leading-relaxed text-muted-foreground"
            >
              {buildDescription(analysis)}
            </p>
          </div>
        </div>

        {/* Preview section */}
        <div className="border-b border-border bg-background px-4 py-3">
          {hasMultiLine && (
            <div className="mb-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
              <Clipboard size={12} aria-hidden="true" />
              <span>{analysis.lineCount} lines</span>
            </div>
          )}
          <pre
            className="max-h-48 overflow-auto rounded border border-border/50 bg-background p-3 font-mono text-[11px] leading-relaxed text-foreground scrollbar-thin"
            aria-label="Paste preview"
          >
            {renderPreview(analysis.preview, hasControlChars)}
          </pre>
          {hasControlChars && (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {analysis.controlChars.map((ch) => (
                <span
                  key={ch}
                  className="inline-block rounded bg-warning/20 px-1.5 py-0.5 font-mono text-[10px] text-warning"
                >
                  {ch}
                </span>
              ))}
            </div>
          )}
          {hasDangerousPatterns && (
            <div className="mt-2 text-[11px] text-destructive">
              Matched dangerous pattern
              {analysis.dangerousPatterns.length > 1 ? "s" : ""}:{" "}
              {analysis.dangerousPatterns
                .map((p) => `\`${p}\``)
                .join(", ")}
            </div>
          )}
        </div>

        {/* Risk acknowledgment checkbox (dangerous patterns only) */}
        {hasDangerousPatterns && (
          <div className="border-b border-border px-4 py-3">
            <label className="flex cursor-pointer items-center gap-2.5">
              <input
                type="checkbox"
                checked={riskAcknowledged}
                onChange={(e) => setRiskAcknowledged(e.target.checked)}
                className="h-4 w-4 rounded border-border text-destructive accent-destructive focus:ring-destructive"
              />
              <span className="text-[12px] font-medium text-foreground">
                I understand the risk and want to paste this content
              </span>
            </label>
          </div>
        )}

        {/* Footer buttons */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className={clsx(
              "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            )}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onConfirm}
            disabled={pasteDisabled}
            className={clsx(
              "rounded border px-3 py-1.5 text-[12px] font-medium transition",
              "focus-visible:outline-none focus-visible:ring-2",
              pasteDisabled
                ? "cursor-not-allowed border-border bg-card text-muted-foreground opacity-50"
                : isDangerous
                  ? "border-destructive/60 bg-destructive/15 text-destructive hover:bg-destructive/25 focus-visible:ring-destructive"
                  : "border-primary/60 bg-primary/15 text-primary hover:bg-primary/25 focus-visible:ring-primary",
            )}
          >
            Paste
          </button>
        </div>
      </div>
    </div>
  );
}

// --- Helpers ---

/** Build a human-readable description of why the paste was intercepted. */
function buildDescription(analysis: PasteAnalysis): string {
  const parts: string[] = [];

  if (analysis.reasons.includes("dangerous-pattern")) {
    parts.push(
      "This paste matches a known dangerous command pattern.",
    );
  }
  if (analysis.reasons.includes("control-chars")) {
    parts.push(
      `Contains ${analysis.controlChars.length} hidden control character${analysis.controlChars.length === 1 ? "" : "s"}.`,
    );
  }
  if (analysis.reasons.includes("multi-line")) {
    parts.push(
      `Multi-line paste (${analysis.lineCount} lines).`,
    );
  }

  return parts.join(" ");
}

/**
 * Render the preview text, highlighting control character representations
 * (caret-notation like ^C, ^[, etc.) with a colored background.
 *
 * The preview from `buildPreview()` already has control chars replaced
 * with their caret-notation. We detect those patterns and wrap them in
 * styled spans.
 */
function renderPreview(
  preview: string,
  highlightControls: boolean,
): React.ReactNode {
  if (!highlightControls) {
    return preview;
  }

  // Match caret-notation control chars: ^@ through ^_ , ^?, and ^[
  // Also match hex representations like 0x1b
  const controlPattern = /(\^[@A-Z\[\\\]^_?]|0x[0-9a-f]{2})/g;
  const parts: React.ReactNode[] = [];
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = controlPattern.exec(preview)) !== null) {
    // Text before the match
    if (match.index > lastIndex) {
      parts.push(preview.slice(lastIndex, match.index));
    }
    // The control char representation, highlighted
    parts.push(
      <span
        key={match.index}
        className="rounded bg-warning/25 px-0.5 text-warning"
        title={`Control character: ${match[0]}`}
      >
        {match[0]}
      </span>,
    );
    lastIndex = match.index + match[0].length;
  }

  // Remaining text after last match
  if (lastIndex < preview.length) {
    parts.push(preview.slice(lastIndex));
  }

  return parts.length > 0 ? <>{parts}</> : preview;
}
