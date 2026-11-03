/**
 * Stale data indicator badge.
 *
 * Shown when data is served from cache during SSH reconnection.
 * Indicates to the user that the displayed information may be
 * outdated and will refresh once the connection is restored.
 */

import clsx from "clsx";
import { Clock } from "lucide-react";

interface StaleBadgeProps {
  /** Optional label override (default: "Stale"). */
  label?: string;
  /** Compact mode — icon only, no text. */
  compact?: boolean;
  className?: string;
}

export function StaleBadge({
  label = "Stale",
  compact = false,
  className,
}: StaleBadgeProps) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded font-medium leading-none",
        "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-300",
        compact ? "px-1 py-0.5 text-[9px]" : "px-1.5 py-0.5 text-[10px]",
        className,
      )}
      title="Data may be outdated — reconnecting to server"
      aria-label={`${label}: data may be outdated while reconnecting`}
      role="status"
    >
      <Clock className={clsx(compact ? "w-2.5 h-2.5" : "w-3 h-3")} aria-hidden="true" />
      {!compact && <span>{label}</span>}
    </span>
  );
}
