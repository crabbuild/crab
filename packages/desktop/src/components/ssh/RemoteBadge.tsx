/**
 * SSH badge component for remote repos in the sidebar.
 *
 * Shows a coloured "SSH" badge with the connection label, matching
 * the profile colour. Used in the sidebar repo list to visually
 * distinguish remote repos from local ones.
 */

import clsx from "clsx";
import { Wifi, WifiOff } from "lucide-react";

interface RemoteBadgeProps {
  /** Profile colour for the badge accent. */
  colour?: string;
  /** Connection label (e.g., hostname or profile name). */
  label?: string;
  /** Whether the connection is currently active. */
  connected?: boolean;
  /** Compact mode — just the badge, no label. */
  compact?: boolean;
  className?: string;
}

export function RemoteBadge({
  colour,
  label,
  connected = true,
  compact = false,
  className,
}: RemoteBadgeProps) {
  const StatusIcon = connected ? Wifi : WifiOff;

  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded text-[10px] font-medium leading-none",
        compact ? "px-1 py-0.5" : "px-1.5 py-0.5",
        connected
          ? "bg-blue-100 text-blue-700 dark:bg-blue-900/30 dark:text-blue-300"
          : "bg-gray-100 text-gray-500 dark:bg-gray-800 dark:text-gray-400",
        className,
      )}
      style={colour ? { borderLeft: `2px solid ${colour}` } : undefined}
      title={connected ? `Connected to ${label ?? "remote"}` : `Disconnected from ${label ?? "remote"}`}
      aria-label={`SSH ${connected ? "connected" : "disconnected"}${label ? `: ${label}` : ""}`}
    >
      <StatusIcon className="w-2.5 h-2.5" />
      <span>SSH</span>
      {!compact && label && (
        <span className="text-[9px] opacity-75 truncate max-w-[60px]">
          {label}
        </span>
      )}
    </span>
  );
}
