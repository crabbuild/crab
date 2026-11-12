import { Button } from "@/components/ui/button";
/**
 * TransferBatchActions — IPC call logic for batch transfer operations.
 *
 * Provides the cancel, retry, and clear batch action handlers that the
 * TransferBatchBar buttons invoke. Communicates with the main process
 * via window.crab.ssh.* IPC calls and displays notifications for
 * mixed success/failure results.
 */

import { useCallback, useState } from "react";
import type { TransferEntry } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

export interface BatchResult {
  succeeded: number;
  failed: number;
  action: "cancel" | "retry" | "clear";
}

export interface BatchNotification {
  id: string;
  result: BatchResult;
  timestamp: number;
}

export interface UseBatchActionsOptions {
  /** All transfers keyed by ID */
  transfers: Map<string, TransferEntry>;
  /** Callback after batch completes to refresh state */
  onBatchComplete?: (result: BatchResult) => void;
}

export interface UseBatchActionsResult {
  /** Cancel all selected queued/active transfers, remove partial data */
  cancelBatch: (ids: string[]) => Promise<void>;
  /** Re-queue failed transfers in original order */
  retryBatch: (ids: string[]) => Promise<void>;
  /** Remove selected completed entries from display */
  clearBatch: (ids: string[]) => Promise<void>;
  /** Whether a batch operation is currently in progress */
  isProcessing: boolean;
  /** Most recent batch notification (for mixed results) */
  notification: BatchNotification | null;
  /** Dismiss the current notification */
  dismissNotification: () => void;
}

// ── Hook ─────────────────────────────────────────────────────────

export function useBatchActions({
  transfers,
  onBatchComplete,
}: UseBatchActionsOptions): UseBatchActionsResult {
  const [isProcessing, setIsProcessing] = useState(false);
  const [notification, setNotification] = useState<BatchNotification | null>(null);

  const dismissNotification = useCallback(() => setNotification(null), []);

  /**
   * Cancel batch: cancel all selected queued/active transfers.
   * The main process removes partial data for active transfers.
   */
  const cancelBatch = useCallback(
    async (ids: string[]) => {
      // Filter to only cancellable transfers (queued, active, scheduled)
      const cancellableIds = ids.filter((id) => {
        const entry = transfers.get(id);
        return (
          entry && (entry.status === "queued" || entry.status === "active" || entry.status === "scheduled")
        );
      });

      if (cancellableIds.length === 0) return;

      setIsProcessing(true);
      try {
        const result = await window.crab.ssh.transferCancelBatch(cancellableIds);
        const batchResult: BatchResult = {
          succeeded: result.succeeded,
          failed: result.failed,
          action: "cancel",
        };

        // Show notification for mixed results
        if (result.failed > 0 && result.succeeded > 0) {
          setNotification({
            id: crypto.randomUUID(),
            result: batchResult,
            timestamp: Date.now(),
          });
        }

        onBatchComplete?.(batchResult);
      } catch {
        setNotification({
          id: crypto.randomUUID(),
          result: { succeeded: 0, failed: cancellableIds.length, action: "cancel" },
          timestamp: Date.now(),
        });
      } finally {
        setIsProcessing(false);
      }
    },
    [transfers, onBatchComplete],
  );

  /**
   * Retry batch: re-queue failed transfers in their original order.
   */
  const retryBatch = useCallback(
    async (ids: string[]) => {
      // Filter to only failed transfers
      const failedIds = ids.filter((id) => {
        const entry = transfers.get(id);
        return entry && entry.status === "failed";
      });

      if (failedIds.length === 0) return;

      // Sort by original start time to preserve order
      const sorted = failedIds.sort((a, b) => {
        const entryA = transfers.get(a);
        const entryB = transfers.get(b);
        return (entryA?.startedAt ?? 0) - (entryB?.startedAt ?? 0);
      });

      setIsProcessing(true);
      try {
        const newIds = await window.crab.ssh.transferRetryBatch(sorted);
        const succeeded = newIds.length;
        const failed = sorted.length - succeeded;
        const batchResult: BatchResult = { succeeded, failed, action: "retry" };

        if (failed > 0 && succeeded > 0) {
          setNotification({
            id: crypto.randomUUID(),
            result: batchResult,
            timestamp: Date.now(),
          });
        }

        onBatchComplete?.(batchResult);
      } catch {
        setNotification({
          id: crypto.randomUUID(),
          result: { succeeded: 0, failed: sorted.length, action: "retry" },
          timestamp: Date.now(),
        });
      } finally {
        setIsProcessing(false);
      }
    },
    [transfers, onBatchComplete],
  );

  /**
   * Clear batch: remove selected completed entries from the display.
   */
  const clearBatch = useCallback(
    async (ids: string[]) => {
      // Filter to only clearable transfers (completed, failed)
      const clearableIds = ids.filter((id) => {
        const entry = transfers.get(id);
        return (
          entry && (entry.status === "completed" || entry.status === "failed")
        );
      });

      if (clearableIds.length === 0) return;

      setIsProcessing(true);
      try {
        await window.crab.ssh.transferClear(clearableIds);
        const batchResult: BatchResult = {
          succeeded: clearableIds.length,
          failed: 0,
          action: "clear",
        };
        onBatchComplete?.(batchResult);
      } catch {
        setNotification({
          id: crypto.randomUUID(),
          result: { succeeded: 0, failed: clearableIds.length, action: "clear" },
          timestamp: Date.now(),
        });
      } finally {
        setIsProcessing(false);
      }
    },
    [transfers, onBatchComplete],
  );

  return {
    cancelBatch,
    retryBatch,
    clearBatch,
    isProcessing,
    notification,
    dismissNotification,
  };
}

// ── Notification Display Component ───────────────────────────────

export interface BatchNotificationBannerProps {
  notification: BatchNotification | null;
  onDismiss: () => void;
}

const ACTION_LABELS: Record<string, string> = {
  cancel: "cancelled",
  retry: "retried",
  clear: "cleared",
};

export function BatchNotificationBanner({ notification, onDismiss }: BatchNotificationBannerProps) {
  if (!notification) return null;

  const { result } = notification;
  const actionLabel = ACTION_LABELS[result.action] ?? result.action;

  return (
    <div
      className="flex items-center gap-2 px-3 py-2 bg-warning/10 bg-warning/20 border-b border-warning border-warning"
      role="alert"
    >
      <span className="text-sm text-warning text-warning">
        Batch {actionLabel}: {result.succeeded} succeeded, {result.failed}{" "}
        failed
      </span>
      <div className="flex-1" />
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={onDismiss}
        className="text-xs text-warning text-warning hover:text-warning hover:text-warning font-medium"
        aria-label="Dismiss notification"
      >
        Dismiss
      </Button>
    </div>
  );
}
