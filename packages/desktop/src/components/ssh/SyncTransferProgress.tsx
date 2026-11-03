/**
 * SyncTransferProgress — compact progress indicator for active sync file
 * transfers. Listens for `ssh:sync-progress` IPC events and displays the
 * current file, bytes transferred/total, speed, and optional delta savings.
 * Auto-hides when no active transfers are in progress.
 */

import { useState, useEffect, useRef } from "react";
import { ArrowUpDown, Zap } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────

interface SyncProgressPayload {
  syncPairId: string;
  file: string;
  bytesTransferred: number;
  totalBytes: number;
  speed: number;
}

interface ActiveTransfer {
  syncPairId: string;
  file: string;
  bytesTransferred: number;
  totalBytes: number;
  speed: number;
  lastUpdated: number;
  deltaSavingsPercent?: number;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatBytes(bytes: number): string {
  if (bytes >= 1_073_741_824) {
    return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  }
  if (bytes >= 1_048_576) {
    return `${(bytes / 1_048_576).toFixed(1)} MB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${bytes} B`;
}

function formatSpeed(bytesPerSec: number): string {
  if (bytesPerSec >= 1_048_576) {
    return `${(bytesPerSec / 1_048_576).toFixed(1)} MB/s`;
  }
  if (bytesPerSec >= 1024) {
    return `${(bytesPerSec / 1024).toFixed(0)} KB/s`;
  }
  return `${bytesPerSec} B/s`;
}

function getFileName(filePath: string): string {
  return filePath.split("/").pop() ?? filePath;
}

/** Stale threshold: if no update in 5s, consider the transfer done. */
const STALE_THRESHOLD_MS = 5000;

// ── Component ────────────────────────────────────────────────────

export function SyncTransferProgress() {
  const [transfers, setTransfers] = useState<Map<string, ActiveTransfer>>(new Map());
  const staleTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // Listen for sync progress events
  useEffect(() => {
    const cleanup = (window as any).crab.ssh.onSyncProgress(
      (payload: SyncProgressPayload) => {
        setTransfers((prev) => {
          const next = new Map(prev);
          const key = `${payload.syncPairId}:${payload.file}`;

          // Compute delta savings if the transfer is sending less than total
          // (bytesTransferred can exceed what was actually sent over the wire
          // when delta sync skips unchanged chunks)
          let deltaSavingsPercent: number | undefined;
          const existing = prev.get(key);
          if (existing?.deltaSavingsPercent != null) {
            deltaSavingsPercent = existing.deltaSavingsPercent;
          }

          next.set(key, {
            syncPairId: payload.syncPairId,
            file: payload.file,
            bytesTransferred: payload.bytesTransferred,
            totalBytes: payload.totalBytes,
            speed: payload.speed,
            lastUpdated: Date.now(),
            deltaSavingsPercent,
          });

          // Remove completed transfers
          if (payload.bytesTransferred >= payload.totalBytes) {
            setTimeout(() => {
              setTransfers((current) => {
                const updated = new Map(current);
                updated.delete(key);
                return updated;
              });
            }, 1000);
          }

          return next;
        });
      },
    );
    return () => { cleanup(); };
  }, []);

  // Periodically prune stale transfers (no update in 5s)
  useEffect(() => {
    staleTimerRef.current = setInterval(() => {
      const now = Date.now();
      setTransfers((prev) => {
        let changed = false;
        const next = new Map(prev);
        for (const [key, transfer] of next) {
          if (now - transfer.lastUpdated > STALE_THRESHOLD_MS) {
            next.delete(key);
            changed = true;
          }
        }
        return changed ? next : prev;
      });
    }, 2000);

    return () => {
      if (staleTimerRef.current) clearInterval(staleTimerRef.current);
    };
  }, []);

  // Auto-hide when no active transfers
  if (transfers.size === 0) return null;

  const transferList = Array.from(transfers.values());
  const totalBytesTransferred = transferList.reduce((sum, t) => sum + t.bytesTransferred, 0);
  const totalBytesAll = transferList.reduce((sum, t) => sum + t.totalBytes, 0);
  const aggregatePercent = totalBytesAll > 0
    ? Math.min(100, Math.round((totalBytesTransferred / totalBytesAll) * 100))
    : 0;
  const avgSpeed = transferList.reduce((sum, t) => sum + t.speed, 0) / transferList.length;

  // Show the most recent transfer as the "current" file
  const current = transferList.reduce((latest, t) =>
    t.lastUpdated > latest.lastUpdated ? t : latest,
  );

  const hasDeltaSavings = transferList.some((t) => t.deltaSavingsPercent != null && t.deltaSavingsPercent > 0);
  const avgDeltaSavings = hasDeltaSavings
    ? transferList
        .filter((t) => t.deltaSavingsPercent != null)
        .reduce((sum, t) => sum + (t.deltaSavingsPercent ?? 0), 0) /
      transferList.filter((t) => t.deltaSavingsPercent != null).length
    : undefined;

  return (
    <div
      className="flex flex-col gap-1 rounded-md border border-zinc-200 dark:border-zinc-700 bg-zinc-50 dark:bg-zinc-800/50 px-3 py-2"
      role="region"
      aria-label="Sync transfer progress"
    >
      {/* Header row */}
      <div className="flex items-center gap-1.5 text-xs text-zinc-600 dark:text-zinc-300">
        <ArrowUpDown className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
        <span className="font-medium truncate" title={current.file}>
          {getFileName(current.file)}
        </span>
        {transfers.size > 1 && (
          <span className="ml-auto shrink-0 text-[11px] text-zinc-500 dark:text-zinc-400">
            +{transfers.size - 1} more
          </span>
        )}
      </div>

      {/* Progress bar */}
      <div className="relative h-1.5 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
        <div
          className="absolute inset-y-0 left-0 rounded-full bg-blue-500 transition-[width] duration-300"
          style={{ width: `${aggregatePercent}%` }}
          role="progressbar"
          aria-valuenow={aggregatePercent}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-label={`${aggregatePercent}% synced`}
        />
      </div>

      {/* Stats row */}
      <div className="flex items-center gap-2 text-[11px] text-zinc-500 dark:text-zinc-400">
        <span>
          {formatBytes(totalBytesTransferred)} / {formatBytes(totalBytesAll)}
        </span>
        <span>·</span>
        <span>{formatSpeed(avgSpeed)}</span>
        {avgDeltaSavings != null && avgDeltaSavings > 0 && (
          <>
            <span>·</span>
            <span className="inline-flex items-center gap-0.5 text-green-600 dark:text-green-400">
              <Zap className="h-3 w-3" aria-hidden="true" />
              {Math.round(avgDeltaSavings)}% saved
            </span>
          </>
        )}
      </div>
    </div>
  );
}
