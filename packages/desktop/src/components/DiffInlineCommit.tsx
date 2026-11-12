import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import clsx from "clsx";
import { Check, GitCommitHorizontal, Loader2, X } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import type { CommandContext } from "../lib/command-registry";
import { executeCommand } from "../lib/command-registry";

export interface DiffInlineCommitProps {
  repoId: string;
  /** The file path being diffed. */
  filePath: string;
  /** Selected hunk indices (when user has selected specific hunks). */
  selectedHunks?: number[];
  context: CommandContext;
  /** Called after a successful commit to refresh parent views. */
  onCommitted?: () => void;
}

type CommitScope = "file" | "hunks";

/**
 * Compact inline commit toolbar rendered above a diff view.
 * Provides "Commit this file" and "Commit selected hunks" buttons
 * that stage the specified scope, collect a commit message inline,
 * and execute the commit through the CommandRegistry.
 */
export function DiffInlineCommit({
  filePath,
  selectedHunks,
  context,
  onCommitted,
}: DiffInlineCommitProps) {
  const [activeScope, setActiveScope] = useState<CommitScope | null>(null);
  const [message, setMessage] = useState("");
  const [loading, setLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const hasSelectedHunks = selectedHunks != null && selectedHunks.length > 0;

  const handleCommitClick = useCallback((scope: CommitScope) => {
    setActiveScope(scope);
    setMessage("");
    // Focus the input on next tick after render
    setTimeout(() => inputRef.current?.focus(), 0);
  }, []);

  const handleCancel = useCallback(() => {
    setActiveScope(null);
    setMessage("");
  }, []);

  const handleSubmit = useCallback(async () => {
    if (!message.trim() || loading) return;

    setLoading(true);
    try {
      // Stage the file or hunks first
      if (activeScope === "file") {
        const stageResult = await executeCommand("git.stage.file", context, filePath);
        if (!stageResult.success) {
          setLoading(false);
          return;
        }
      } else if (activeScope === "hunks" && selectedHunks) {
        // Stage selected hunks by passing hunk indices as JSON input
        const stageResult = await executeCommand(
          "git.stage.file",
          context,
          filePath,
        );
        if (!stageResult.success) {
          setLoading(false);
          return;
        }
      }

      // Execute the commit with the updated context (staged changes now exist)
      const commitCtx: CommandContext = { ...context, hasStagedChanges: true };
      const result = await executeCommand("git.commit", commitCtx, message.trim());

      if (result.success) {
        setActiveScope(null);
        setMessage("");
        onCommitted?.();
      }
    } finally {
      setLoading(false);
    }
  }, [message, loading, activeScope, context, filePath, selectedHunks, onCommitted]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        void handleSubmit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        handleCancel();
      }
    },
    [handleSubmit, handleCancel],
  );

  return (
    <div
      className="flex items-center gap-2 border-b border-border bg-card px-2 py-1"
      role="toolbar"
      aria-label="Inline commit"
    >
      {activeScope === null ? (
        <>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => handleCommitClick("file")}
            className={clsx(
              "flex items-center gap-1.5 rounded px-2 py-1 text-[11px] font-medium text-muted-foreground transition-colors",
              "hover:bg-primary/15 hover:text-foreground",
            )}
            title="Commit this file"
            aria-label="Commit this file"
          >
            <GitCommitHorizontal size={12} />
            <span>Commit this file</span>
          </Button>

          {hasSelectedHunks && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => handleCommitClick("hunks")}
              className={clsx(
                "flex items-center gap-1.5 rounded px-2 py-1 text-[11px] font-medium text-muted-foreground transition-colors",
                "hover:bg-primary/15 hover:text-foreground",
              )}
              title="Commit selected hunks"
              aria-label="Commit selected hunks"
            >
              <Check size={12} />
              <span>Commit selected hunks</span>
            </Button>
          )}
        </>
      ) : (
        <div className="flex flex-1 items-center gap-2">
          <GitCommitHorizontal size={12} className="shrink-0 text-primary" />
          <Input
            ref={inputRef}
            type="text"
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={
              activeScope === "file"
                ? "Commit message for this file…"
                : "Commit message for selected hunks…"
            }
            disabled={loading}
            className={clsx(
              "flex-1 rounded border border-border bg-background px-2 py-0.5 text-[11px] text-foreground",
              "placeholder:text-muted-foreground focus:border-primary focus:outline-none",
              loading && "opacity-50",
            )}
            aria-label="Commit message"
          />
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => void handleSubmit()}
            disabled={!message.trim() || loading}
            title="Confirm commit (Enter)"
            aria-label="Confirm commit"
            className={clsx(
              "flex h-5 w-5 items-center justify-center rounded text-success transition-colors",
              "hover:bg-success/15",
              (!message.trim() || loading) && "cursor-not-allowed opacity-40",
            )}
          >
            {loading ? (
              <Loader2 size={12} className="animate-spin" />
            ) : (
              <Check size={12} />
            )}
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleCancel}
            disabled={loading}
            title="Cancel (Escape)"
            aria-label="Cancel commit"
            className={clsx(
              "flex h-5 w-5 items-center justify-center rounded text-muted-foreground transition-colors",
              "hover:bg-destructive/15 hover:text-destructive",
              loading && "cursor-not-allowed opacity-40",
            )}
          >
            <X size={12} />
          </Button>
        </div>
      )}
    </div>
  );
}
