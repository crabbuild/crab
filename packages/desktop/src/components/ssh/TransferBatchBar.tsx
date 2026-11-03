/**
 * TransferBatchBar — summary bar displayed when 2+ transfers are selected.
 * Shows selection count, aggregate size, and batch action buttons that
 * enable/disable based on the statuses of selected transfers.
 */

import { useMemo } from "react";
import { XCircle, RotateCcw, Trash2, X } from "lucide-react";
import clsx from "clsx";
import type { TransferEntry } from "../../../electron/ssh-types";
import { formatBytes } from "./TransferProgressBar";
import {
  getEnabledBatchActions,
  computeSelectedSize,
} from "../../hooks/useTransferSelection";

// ── Types ────────────────────────────────────────────────────────

export interface TransferBatchBarProps {
  /** Set of currently selected transfer IDs */
  selectedIds: Set<string>;
  /** All transfers keyed by ID */
  transfers: Map<string, TransferEntry>;
  /** Cancel selected queued/active transfers */
  onCancelSelected: (ids: string[]) => void;
  /** Retry selected failed transfers */
  onRetrySelected: (ids: string[]) => void;
  /** Clear (remove) selected completed/failed transfers from display */
  onClearSelected: (ids: string[]) => void;
  /** Deselect all transfers */
  onDeselectAll: () => void;
}

// ── Component ────────────────────────────────────────────────────

export function TransferBatchBar({
  selectedIds,
  transfers,
  onCancelSelected,
  onRetrySelected,
  onClearSelected,
  onDeselectAll,
}: TransferBatchBarProps) {
  const count = selectedIds.size;

  const { canCancel, canRetry, canClear } = useMemo(
    () => getEnabledBatchActions(selectedIds, transfers),
    [selectedIds, transfers],
  );

  const aggregateSize = useMemo(
    () => computeSelectedSize(selectedIds, transfers),
    [selectedIds, transfers],
  );

  // Only show when 2+ transfers are selected
  if (count < 2) return null;

  const selectedArray = Array.from(selectedIds);

  return (
    <div
      className={clsx(
        "flex items-center gap-3 px-3 py-2",
        "bg-blue-50 dark:bg-blue-900/20",
        "border-b border-blue-200 dark:border-blue-800",
      )}
      role="toolbar"
      aria-label="Batch transfer actions"
    >
      {/* Summary text */}
      <span className="text-sm font-medium text-blue-700 dark:text-blue-300 whitespace-nowrap">
        {count} transfers selected ({formatBytes(aggregateSize)} total)
      </span>

      {/* Spacer */}
      <div className="flex-1" />

      {/* Action buttons */}
      <div className="flex items-center gap-1.5">
        <BatchButton
          icon={<XCircle size={14} />}
          label="Cancel"
          disabled={!canCancel}
          onClick={() => onCancelSelected(selectedArray)}
          variant="warning"
        />
        <BatchButton
          icon={<RotateCcw size={14} />}
          label="Retry"
          disabled={!canRetry}
          onClick={() => onRetrySelected(selectedArray)}
          variant="default"
        />
        <BatchButton
          icon={<Trash2 size={14} />}
          label="Clear"
          disabled={!canClear}
          onClick={() => onClearSelected(selectedArray)}
          variant="danger"
        />
      </div>

      {/* Deselect button */}
      <button
        type="button"
        onClick={onDeselectAll}
        className="ml-1 p-1 rounded hover:bg-blue-100 dark:hover:bg-blue-800/40 text-blue-500 dark:text-blue-400"
        aria-label="Deselect all"
        title="Deselect all"
      >
        <X size={14} />
      </button>
    </div>
  );
}

// ── Batch Button ─────────────────────────────────────────────────

interface BatchButtonProps {
  icon: React.ReactNode;
  label: string;
  disabled: boolean;
  onClick: () => void;
  variant: "default" | "warning" | "danger";
}

const VARIANT_CLASSES: Record<string, { enabled: string; disabled: string }> = {
  default: {
    enabled:
      "text-gray-700 dark:text-gray-200 hover:bg-gray-100 dark:hover:bg-gray-700",
    disabled: "text-gray-300 dark:text-gray-600 cursor-not-allowed",
  },
  warning: {
    enabled:
      "text-yellow-700 dark:text-yellow-300 hover:bg-yellow-50 dark:hover:bg-yellow-900/30",
    disabled: "text-gray-300 dark:text-gray-600 cursor-not-allowed",
  },
  danger: {
    enabled:
      "text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/30",
    disabled: "text-gray-300 dark:text-gray-600 cursor-not-allowed",
  },
};

function BatchButton({ icon, label, disabled, onClick, variant }: BatchButtonProps) {
  const classes = VARIANT_CLASSES[variant];

  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1 px-2 py-1 rounded text-xs font-medium transition-colors",
        disabled ? classes.disabled : classes.enabled,
      )}
      aria-label={label}
      title={disabled ? `${label} (no applicable transfers selected)` : label}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { BatchButton };
