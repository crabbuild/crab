import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Code, Send, X } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────────

export interface InlineCommentComposerProps {
  /** File path for the comment */
  path: string;
  /** Line or range being commented */
  line: number;
  endLine?: number;
  side: "LEFT" | "RIGHT";
  /** Code content of the selected lines (for suggest pre-fill) */
  codeContext: string;
  /** Called when user submits the comment */
  onSubmit(body: string): void;
  /** Called when user cancels */
  onCancel(): void;
  /** Initial body (for editing an existing pending comment) */
  initialBody?: string;
}

// ── Component ────────────────────────────────────────────────────────

/**
 * Inline comment composer anchored below a diff line or range.
 *
 * Displays the code context above a multi-line textarea, with a toolbar
 * for inserting suggestion markdown and buttons for submit/cancel.
 * Supports Cmd+Enter / Ctrl+Enter to submit and Escape to cancel.
 */
export function InlineCommentComposer({
  path,
  line,
  endLine,
  side,
  codeContext,
  onSubmit,
  onCancel,
  initialBody = "",
}: InlineCommentComposerProps) {
  const [body, setBody] = useState(initialBody);
  const [validationError, setValidationError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Auto-focus the textarea on mount.
  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const handleSubmit = useCallback(() => {
    const trimmed = body.trim();
    if (!trimmed) {
      setValidationError("Comment text is required.");
      return;
    }
    setValidationError(null);
    onSubmit(trimmed);
  }, [body, onSubmit]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleSubmit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      }
    },
    [handleSubmit, onCancel],
  );

  const insertSuggestion = useCallback(() => {
    const suggestion = `\`\`\`suggestion\n${codeContext}\n\`\`\`\n`;
    setBody((prev) => {
      // Insert at cursor position if textarea is focused, otherwise append.
      const textarea = textareaRef.current;
      if (textarea && document.activeElement === textarea) {
        const start = textarea.selectionStart;
        const end = textarea.selectionEnd;
        const next = prev.slice(0, start) + suggestion + prev.slice(end);
        // Schedule cursor repositioning after state update.
        requestAnimationFrame(() => {
          textarea.selectionStart = start + suggestion.length;
          textarea.selectionEnd = start + suggestion.length;
          textarea.focus();
        });
        return next;
      }
      return prev + suggestion;
    });
    setValidationError(null);
  }, [codeContext]);

  const handleBodyChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    setBody(e.target.value);
    if (validationError) setValidationError(null);
  }, [validationError]);

  const lineLabel = endLine != null && endLine !== line
    ? `L${line}–${endLine}`
    : `L${line}`;

  return (
    <div className="my-1 rounded border border-gx-border bg-gx-surface shadow-sm">
      {/* Code context block */}
      {codeContext && (
        <div className="border-b border-gx-border/50 bg-gx-bg-sub px-3 py-2">
          <div className="mb-1 flex items-center gap-2 text-[10px] text-gx-muted">
            <Code size={10} />
            <span className="font-mono">
              {path}:{lineLabel} ({side})
            </span>
          </div>
          <pre className="overflow-x-auto whitespace-pre font-mono text-[11px] leading-relaxed text-gx-text/80">
            {codeContext}
          </pre>
        </div>
      )}

      {/* Composer body */}
      <div className="px-3 py-2">
        {/* Toolbar */}
        <div className="mb-1.5 flex items-center gap-1">
          <button
            type="button"
            className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-accent/10 hover:text-gx-accent"
            onClick={insertSuggestion}
            title="Insert suggestion block pre-filled with selected code"
            aria-label="Suggest change"
          >
            <Code size={10} />
            Suggest change
          </button>
        </div>

        {/* Textarea */}
        <textarea
          ref={textareaRef}
          className="w-full resize-y rounded border border-gx-border bg-gx-bg-sub p-2 text-[11px] leading-relaxed text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus-visible:outline-none"
          rows={3}
          placeholder="Write a review comment…"
          value={body}
          onChange={handleBodyChange}
          onKeyDown={handleKeyDown}
          aria-label={`Comment on ${path} line ${lineLabel}`}
          aria-invalid={validationError ? "true" : undefined}
          aria-describedby={validationError ? "composer-error" : undefined}
        />

        {/* Validation error */}
        {validationError && (
          <p
            id="composer-error"
            className="mt-1 text-[10px] text-red-400"
            role="alert"
          >
            {validationError}
          </p>
        )}

        {/* Actions */}
        <div className="mt-2 flex items-center justify-between">
          <span className="text-[9px] text-gx-muted">
            {navigator.platform?.includes("Mac") ? "⌘" : "Ctrl"}+Enter to submit · Esc to cancel
          </span>
          <div className="flex items-center gap-2">
            <button
              type="button"
              className="inline-flex items-center gap-1 rounded px-2 py-1 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
              onClick={onCancel}
              aria-label="Cancel comment"
            >
              <X size={10} />
              Cancel
            </button>
            <button
              type="button"
              className="inline-flex items-center gap-1 rounded bg-gx-accent/10 px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/20 disabled:cursor-not-allowed disabled:opacity-50"
              disabled={!body.trim()}
              onClick={handleSubmit}
              aria-label="Submit comment"
            >
              <Send size={10} />
              Submit
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
