import clsx from "clsx";
import { AlertTriangle, ArrowDown, ArrowUp, Check, Loader2 } from "lucide-react";

export type FileSyncStatus =
  | "synced"
  | "pending-upload"
  | "pending-download"
  | "syncing"
  | "conflict";

interface SyncStatusIconProps {
  status: FileSyncStatus;
  size?: "sm" | "md";
}

const SIZE_CLASSES = {
  sm: "w-3 h-3",
  md: "w-4 h-4",
} as const;

const STATUS_CONFIG: Record<
  FileSyncStatus,
  { icon: typeof Check; colorClass: string; label: string }
> = {
  synced: {
    icon: Check,
    colorClass: "text-success",
    label: "Synced",
  },
  "pending-upload": {
    icon: ArrowUp,
    colorClass: "text-info",
    label: "Pending upload",
  },
  "pending-download": {
    icon: ArrowDown,
    colorClass: "text-info",
    label: "Pending download",
  },
  syncing: {
    icon: Loader2,
    colorClass: "text-primary",
    label: "Syncing",
  },
  conflict: {
    icon: AlertTriangle,
    colorClass: "text-warning",
    label: "Conflict",
  },
};

export function SyncStatusIcon({ status, size = "sm" }: SyncStatusIconProps) {
  const config = STATUS_CONFIG[status];
  const Icon = config.icon;

  return (
    <Icon
      className={clsx(
        SIZE_CLASSES[size],
        config.colorClass,
        status === "syncing" && "animate-spin",
      )}
      aria-label={config.label}
      aria-hidden="false"
    />
  );
}
