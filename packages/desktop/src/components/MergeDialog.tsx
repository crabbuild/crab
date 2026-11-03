import clsx from "clsx";
import { GitMerge, Loader2 } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { MergeStrategy, rpc } from "../ipc";

/**
 * Modal dialog for initiating a merge operation.
 *
 * Pre-fills source (from context menu selection) and target (current
 * HEAD branch). The user picks a strategy and optionally edits the
 * commit message before confirming.
 *
 * On success the dialog closes and fires `onSuccess` so the parent
 * can refresh branches and show a toast. On conflict (state "paused")
 * it closes and fires `onConflicts` so the parent can surface a
 * "merge paused" toast. Errors are shown inline.
 */
export interface MergeDialogProps {
  /** Branch being merged (source). */
  sourceBranch: string;
  /** Current HEAD branch (target). */
  targetBranch: string;
  /** Active repo id for the RPC call. */
  repoId: string;
  /** Called after a successful merge (state "done"). */
  onSuccess: () => void;
  /** Called when the merge pauses due to conflicts. */
  onConflicts: (conflictCount: number) => void;
  /** Called to close the dialog without action. */
  onCancel: () => void;
}

export function MergeDialog({
  sourceBranch,
  targetBranch,
  repoId,
  onSuccess,
  onConflicts,
  onCancel,
}: MergeDialogProps) {
  const headingId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);

  const [strategy, setStrategy] = useState<MergeStrategy>("MergeCommit");
  const [message, setMessage] = useState(
    `Merge branch '${sourceBranch}' into ${targetBranch}`,
  );
  const [merging, setMerging] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Focus management: grab focus on mount, restore on unmount.
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
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

  async function handleMerge() {
    setMerging(true);
    setError(null);
    try {
      const result = await rpc.opStart(repoId, {
        kind: "Merge",
        from: sourceBranch,
        into: targetBranch,
        strategy,
        message: message.trim() || undefined,
      });

      if (result.state === "done") {
        onSuccess();
      } else if (result.state === "paused") {
        onConflicts(result.conflicts?.length ?? 0);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setMerging(false);
    }
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={bodyId}
        className="w-[30rem] max-w-[calc(100vw-2rem)] rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-start gap-3 border-b border-gx-border p-4">
          <div className="mt-0.5 shrink-0 rounded bg-gx-accent/15 p-2 text-gx-accent">
            <GitMerge size={18} aria-hidden="true" />
          </div>
          <div className="min-w-0 flex-1">
            <h2
              id={headingId}
              className="text-[14px] font-semibold text-gx-text"
            >
              Merge branch
            </h2>
            <p id={bodyId} className="mt-1 text-[12px] leading-relaxed text-gx-muted">
              Merge{" "}
              <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                {sourceBranch}
              </code>{" "}
              into{" "}
              <code className="rounded bg-gx-bg-sub px-1 font-mono text-gx-text">
                {targetBranch}
              </code>
            </p>
          </div>
        </div>

        {/* Body */}
        <div className="space-y-4 border-b border-gx-border px-4 py-4">
          {/* Source / Target (read-only) */}
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-[11px] font-medium text-gx-muted">
                Source
              </label>
              <div className="mt-1 truncate rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text">
                {sourceBranch}
              </div>
            </div>
            <div>
              <label className="block text-[11px] font-medium text-gx-muted">
                Target
              </label>
              <div className="mt-1 truncate rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text">
                {targetBranch}
              </div>
            </div>
          </div>

          {/* Strategy */}
          <fieldset>
            <legend className="text-[11px] font-medium text-gx-muted">
              Strategy
            </legend>
            <div className="mt-1.5 flex gap-2">
              <StrategyOption
                value="MergeCommit"
                label="Merge commit"
                selected={strategy}
                onChange={setStrategy}
              />
              <StrategyOption
                value="Squash"
                label="Squash"
                selected={strategy}
                onChange={setStrategy}
              />
              <StrategyOption
                value="FastForwardOnly"
                label="Fast-forward only"
                selected={strategy}
                onChange={setStrategy}
              />
            </div>
          </fieldset>

          {/* Commit message */}
          <div>
            <label
              htmlFor="merge-message"
              className="block text-[11px] font-medium text-gx-muted"
            >
              Commit message
            </label>
            <input
              id="merge-message"
              type="text"
              className="mt-1 w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5 font-mono text-[12px] text-gx-text focus-visible:outline-none transition focus:ring-1 focus:ring-gx-accent"
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !merging) {
                  e.preventDefault();
                  handleMerge();
                }
              }}
              disabled={merging}
              autoComplete="off"
              spellCheck={false}
            />
          </div>

          {/* Error */}
          {error && (
            <div className="rounded border border-gx-error/40 bg-gx-error/10 px-3 py-2 text-[11px] text-gx-error">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 p-3">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            disabled={merging}
            className={clsx(
              "rounded border border-gx-border bg-gx-surface px-3 py-1.5 text-[12px] text-gx-text transition",
              "hover:bg-gx-surface-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              "disabled:opacity-50",
            )}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={handleMerge}
            disabled={merging}
            className={clsx(
              "flex items-center gap-1.5 rounded border border-gx-accent/60 bg-gx-accent/15 px-3 py-1.5 text-[12px] font-medium text-gx-accent transition",
              "hover:bg-gx-accent/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
              "disabled:opacity-50",
            )}
          >
            {merging && <Loader2 size={12} className="animate-spin" />}
            Merge
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Strategy radio option ──────────────────────────────────────────

function StrategyOption({
  value,
  label,
  selected,
  onChange,
}: {
  value: MergeStrategy;
  label: string;
  selected: MergeStrategy;
  onChange: (v: MergeStrategy) => void;
}) {
  const isSelected = selected === value;
  return (
    <button
      type="button"
      role="radio"
      aria-checked={isSelected}
      onClick={() => onChange(value)}
      className={clsx(
        "rounded border px-2.5 py-1 text-[11px] transition",
        isSelected
          ? "border-gx-accent bg-gx-accent/15 font-medium text-gx-accent"
          : "border-gx-border bg-gx-surface text-gx-muted hover:border-gx-accent/40 hover:text-gx-text",
      )}
    >
      {label}
    </button>
  );
}
