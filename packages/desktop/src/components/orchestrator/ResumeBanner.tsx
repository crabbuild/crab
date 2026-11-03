/**
 * Resume banner — surfaces "pick up where you left off" when the app
 * opens and the selected task's most-recent checkpoint is older than
 * the current render but newer than any in-memory activity, suggesting
 * the previous session was interrupted.
 *
 * The banner is explicitly dismissible and debounces itself so toggling
 * tasks or restarts don't hammer the user with repeat prompts.
 */

import { useCallback, useEffect, useState } from "react";
import { Clock, X, RotateCcw } from "lucide-react";

interface Props {
  taskId: string;
  /** Most recent chat message timestamp known to the client. */
  lastMessageTs: number | null;
  /**
   * Invoked when the user clicks "Resume" — the caller drops the user
   * on the Chat tab and focuses the composer.
   */
  onResume: () => void;
}

interface LatestCheckpoint {
  ref: string;
  turnNumber: number;
  sha: string;
  createdAt?: number;
}

const LS_DISMISS_PREFIX = "crab.orchestrator.resumeDismissed.";

export function ResumeBanner({ taskId, lastMessageTs, onResume }: Props) {
  const [latest, setLatest] = useState<LatestCheckpoint | null>(null);
  const [dismissed, setDismissed] = useState(() => {
    try {
      return localStorage.getItem(LS_DISMISS_PREFIX + taskId) === "1";
    } catch {
      return false;
    }
  });

  // Fetch the most recent checkpoint once on mount / task change.
  useEffect(() => {
    let cancelled = false;
    (window as any).crab?.orchestrator
      ?.latestCheckpoint?.(taskId)
      .then((result: { success?: boolean; latest?: LatestCheckpoint | null }) => {
        if (cancelled) return;
        setLatest(result?.success ? (result.latest ?? null) : null);
      })
      .catch(() => { if (!cancelled) setLatest(null); });
    return () => { cancelled = true; };
  }, [taskId]);

  // Reset dismissed flag when the task changes.
  useEffect(() => {
    try {
      setDismissed(localStorage.getItem(LS_DISMISS_PREFIX + taskId) === "1");
    } catch {
      setDismissed(false);
    }
  }, [taskId]);

  const dismiss = useCallback(() => {
    setDismissed(true);
    try {
      localStorage.setItem(LS_DISMISS_PREFIX + taskId, "1");
    } catch {
      // Best-effort.
    }
  }, [taskId]);

  // Only show when:
  //   1. There's at least one checkpoint (turn 2 or later means agent ran).
  //   2. The user hasn't dismissed the banner for this task.
  //   3. There's either no chat history yet, or the history is older than
  //      the checkpoint by a meaningful margin (>10s) — suggesting the
  //      previous session's final messages were lost on restart.
  if (dismissed || !latest || latest.turnNumber < 2) return null;

  const checkpointTs = latest.createdAt ?? null;
  if (checkpointTs && lastMessageTs && lastMessageTs >= checkpointTs - 10_000) {
    return null;
  }

  return (
    <div className="mx-3 mt-2 flex items-center gap-2 rounded-md border border-gx-info/40 bg-gx-info/10 px-3 py-2 text-[11px] text-gx-text">
      <Clock size={12} className="shrink-0 text-gx-info" />
      <span className="flex-1">
        Previous session reached <strong>turn {latest.turnNumber}</strong>
        {checkpointTs ? ` (${formatAgo(checkpointTs)})` : ""}. Resume where you left off?
      </span>
      <button
        type="button"
        onClick={onResume}
        className="inline-flex items-center gap-1 rounded-md bg-gx-accent px-2 py-0.5 text-white hover:bg-gx-accent-hover transition"
      >
        <RotateCcw size={10} /> Resume
      </button>
      <button
        type="button"
        onClick={dismiss}
        aria-label="Dismiss resume prompt"
        className="rounded p-0.5 text-gx-muted hover:text-gx-text"
      >
        <X size={12} />
      </button>
    </div>
  );
}

function formatAgo(ts: number): string {
  const seconds = Math.floor((Date.now() - ts) / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}
