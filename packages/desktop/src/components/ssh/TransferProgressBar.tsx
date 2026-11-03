/**
 * TransferProgressBar — per-transfer progress display showing a progress bar
 * with percentage, auto-scaled speed, ETA, bytes transferred/total, and
 * a "Resuming" indicator when the transfer is in resume state.
 *
 * Receives updated TransferEntry data at ≤500ms intervals from the parent.
 */

import clsx from "clsx";
import { ArrowDown, ArrowUp, Loader2 } from "lucide-react";
import type { TransferEntry } from "../../../electron/ssh-types";

// ── Helpers ──────────────────────────────────────────────────────

const UNITS = ["B/s", "KB/s", "MB/s", "GB/s"] as const;

/**
 * Auto-scale bytes/sec to the largest whole unit with one decimal place.
 * E.g. 1536 → "1.5 KB/s", 2_500_000 → "2.4 MB/s"
 */
export function formatSpeed(bytesPerSecond: number): string {
  if (bytesPerSecond < 0) return "0.0 B/s";

  let value = bytesPerSecond;
  let unitIndex = 0;

  while (value >= 1024 && unitIndex < UNITS.length - 1) {
    value /= 1024;
    unitIndex++;
  }

  return `${value.toFixed(1)} ${UNITS[unitIndex]}`;
}

/**
 * Format seconds remaining as human-readable ETA.
 * E.g. 150 → "2m 30s", 3661 → "1h 1m", 45 → "45s"
 */
export function formatEta(seconds: number): string {
  if (seconds <= 0 || !Number.isFinite(seconds)) return "—";

  const s = Math.ceil(seconds);

  if (s < 60) return `${s}s`;

  const hours = Math.floor(s / 3600);
  const minutes = Math.floor((s % 3600) / 60);
  const secs = s % 60;

  if (hours > 0) {
    return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  }

  return secs > 0 ? `${minutes}m ${secs}s` : `${minutes}m`;
}

/**
 * Format byte counts as human-readable with one decimal place,
 * auto-scaled to the largest whole unit.
 * E.g. 47_400_000 → "45.2 MB", 1_073_741_824 → "1.0 GB"
 */
export function formatBytes(bytes: number): string {
  if (bytes < 0) return "0 B";

  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;

  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }

  if (unitIndex === 0) return `${Math.round(value)} B`;
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}

// ── Component ────────────────────────────────────────────────────

export interface TransferProgressBarProps {
  transfer: TransferEntry;
}

export function TransferProgressBar({ transfer }: TransferProgressBarProps) {
  const { status, bytesTransferred, totalBytes, speed, eta, direction } = transfer;

  const percent = totalBytes > 0 ? Math.min(100, Math.round((bytesTransferred / totalBytes) * 100)) : 0;
  const isResuming = status === "resuming";
  const isActive = status === "active" || isResuming;

  return (
    <div
      className={clsx(
        "flex flex-col gap-1.5 rounded-md border border-gx-border bg-gx-surface px-3 py-2",
        isResuming && "border-gx-warning/40",
      )}
      role="group"
      aria-label={`Transfer progress: ${transfer.fileName}`}
    >
      {/* Top row: direction icon, file name, percentage */}
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-1.5 min-w-0">
          {direction === "upload" ? (
            <ArrowUp size={14} className="shrink-0 text-gx-accent" aria-label="Upload" />
          ) : (
            <ArrowDown size={14} className="shrink-0 text-gx-info" aria-label="Download" />
          )}
          <span className="truncate text-[12px] font-medium text-gx-text" title={transfer.fileName}>
            {transfer.fileName}
          </span>
        </div>

        <span className="shrink-0 text-[11px] font-medium text-gx-muted">
          {percent}%
        </span>
      </div>

      {/* Progress bar */}
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-gx-bg-sub"
        role="progressbar"
        aria-valuenow={percent}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={`${percent}% complete`}
      >
        <div
          className={clsx(
            "h-full rounded-full transition-all duration-300 ease-out",
            isResuming ? "bg-gx-warning" : direction === "upload" ? "bg-gx-accent" : "bg-gx-info",
          )}
          style={{ width: `${percent}%` }}
        />
      </div>

      {/* Bottom row: stats */}
      <div className="flex items-center justify-between text-[11px] text-gx-muted">
        {/* Left: resuming indicator or speed + ETA */}
        <div className="flex items-center gap-2">
          {isResuming ? (
            <span className="flex items-center gap-1 text-gx-warning font-medium">
              <Loader2 size={12} className="animate-spin" aria-hidden="true" />
              Resuming…
            </span>
          ) : isActive ? (
            <>
              <span>{formatSpeed(speed)}</span>
              <span className="text-gx-border">·</span>
              <span>{formatEta(eta)}</span>
            </>
          ) : null}
        </div>

        {/* Right: bytes transferred / total */}
        {totalBytes > 0 && (
          <span>
            {formatBytes(bytesTransferred)} / {formatBytes(totalBytes)}
          </span>
        )}
      </div>
    </div>
  );
}
