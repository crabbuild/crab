/**
 * Transfer Panel — displays all file transfers grouped by status
 * with sorting, direction indicators, empty state, clear button,
 * and batch selection support.
 */

import { useMemo, useCallback } from "react";
import { ArrowUpCircle, ArrowDownCircle, Inbox, Trash2 } from "lucide-react";
import clsx from "clsx";
import type { TransferEntry, TransferStatus } from "../../../electron/ssh-types";
import type { SshEnhancedAction } from "../../state/ssh-enhanced-state";
import { TransferBatchBar } from "./TransferBatchBar";
import { useTransferSelection } from "../../hooks/useTransferSelection";

// ── Types ────────────────────────────────────────────────────────

interface TransferPanelProps {
  transfers: Map<string, TransferEntry>;
  /** Currently selected transfer IDs (for batch operations) */
  selectedIds?: Set<string>;
  /** Dispatch function for state updates */
  dispatch?: (action: SshEnhancedAction) => void;
  /** Callback to cancel selected transfers */
  onCancelSelected?: (ids: string[]) => void;
  /** Callback to retry selected transfers */
  onRetrySelected?: (ids: string[]) => void;
  /** Callback to clear selected transfers from display */
  onClearSelected?: (ids: string[]) => void;
}

// ── Constants ────────────────────────────────────────────────────

/** Display order for status groups. */
const STATUS_GROUP_ORDER: TransferStatus[] = [
  "active",
  "queued",
  "completed",
  "failed",
  "scheduled",
];

const STATUS_LABELS: Record<TransferStatus, string> = {
  active: "Active",
  queued: "Queued",
  resuming: "Resuming",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
  scheduled: "Scheduled",
};

const STATUS_BADGE_CLASSES: Record<TransferStatus, string> = {
  active: "bg-info/10 text-info bg-info/30 text-info",
  queued: "bg-card text-muted-foreground bg-card text-muted-foreground",
  resuming: "bg-warning/10 text-warning bg-warning/30 text-warning",
  completed: "bg-success/10 text-success bg-success/30 text-success",
  failed: "bg-destructive/10 text-destructive bg-destructive/30 text-destructive",
  cancelled: "bg-card text-muted-foreground bg-card text-muted-foreground",
  scheduled: "bg-thinking/10 text-thinking bg-thinking/30 text-thinking",
};

// ── Helpers ──────────────────────────────────────────────────────

function getFileName(filePath: string): string {
  return filePath.split("/").pop() ?? filePath;
}

function getFilePath(entry: TransferEntry): string {
  return entry.direction === "upload" ? entry.localPath : entry.remotePath;
}

/**
 * Groups transfers by status and sorts within each group by startedAt
 * descending (most recent first). Entries without startedAt sort last.
 */
function groupAndSort(
  transfers: Map<string, TransferEntry>,
): Array<{ status: TransferStatus; entries: TransferEntry[] }> {
  const groups = new Map<TransferStatus, TransferEntry[]>();

  for (const entry of transfers.values()) {
    const list = groups.get(entry.status) ?? [];
    list.push(entry);
    groups.set(entry.status, list);
  }

  // Sort within each group by startedAt descending
  for (const entries of groups.values()) {
    entries.sort((a, b) => (b.startedAt ?? 0) - (a.startedAt ?? 0));
  }

  // Return groups in the defined display order, skipping empty groups
  const result: Array<{ status: TransferStatus; entries: TransferEntry[] }> = [];
  for (const status of STATUS_GROUP_ORDER) {
    const entries = groups.get(status);
    if (entries && entries.length > 0) {
      result.push({ status, entries });
    }
  }

  return result;
}

// ── Component ────────────────────────────────────────────────────

export function TransferPanel({
  transfers,
  selectedIds = new Set(),
  dispatch,
  onCancelSelected,
  onRetrySelected,
  onClearSelected,
}: TransferPanelProps) {
  const grouped = useMemo(() => groupAndSort(transfers), [transfers]);

  // Flat ordered list of IDs for shift-click range selection
  const orderedIds = useMemo(
    () => grouped.flatMap(({ entries }) => entries.map((e) => e.id)),
    [grouped],
  );

  // Selection hook (only active when dispatch is provided)
  const noopDispatch = useCallback(() => {}, []);
  const { toggleSelect, shiftSelect, deselectAll } = useTransferSelection({
    orderedIds,
    selectedIds,
    dispatch: dispatch ?? noopDispatch,
  });

  // Whether there are any completed or failed entries to clear
  const hasClearable = useMemo(() => {
    for (const entry of transfers.values()) {
      if (entry.status === "completed" || entry.status === "failed") return true;
    }
    return false;
  }, [transfers]);

  const handleClearCompleted = useCallback(() => {
    if (dispatch) {
      dispatch({ type: "TRANSFER_CLEAR_COMPLETED" });
    }
  }, [dispatch]);

  if (transfers.size === 0) {
    return (
      <div
        className="flex flex-col items-center justify-center h-full py-12 px-4"
        role="region"
        aria-label="File transfers"
      >
        <Inbox className="w-10 h-10 text-muted-foreground text-muted-foreground mb-3" />
        <p className="text-sm text-muted-foreground text-muted-foreground text-center">
          No transfers in progress or recorded.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="File transfers">
      {/* Header with clear button */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <span className="text-xs font-semibold text-muted-foreground text-muted-foreground">
          Transfers
        </span>
        <button
          type="button"
          onClick={handleClearCompleted}
          disabled={!hasClearable}
          className={clsx(
            "flex items-center gap-1 px-2 py-1 rounded text-xs font-medium transition-colors",
            hasClearable
              ? "text-destructive text-destructive hover:bg-destructive/10 hover:bg-destructive/30"
              : "text-muted-foreground text-muted-foreground cursor-not-allowed",
          )}
          aria-label="Clear completed and failed transfers"
          title="Clear completed and failed transfers"
        >
          <Trash2 size={12} />
          <span>Clear</span>
        </button>
      </div>

      {/* Batch selection bar */}
      {dispatch && (
        <TransferBatchBar
          selectedIds={selectedIds}
          transfers={transfers}
          onCancelSelected={onCancelSelected ?? (() => {})}
          onRetrySelected={onRetrySelected ?? (() => {})}
          onClearSelected={onClearSelected ?? (() => {})}
          onDeselectAll={deselectAll}
        />
      )}

      {/* Transfer list */}
      <div className="flex-1 overflow-y-auto">
        {grouped.map(({ status, entries }) => (
          <TransferGroup
            key={status}
            status={status}
            entries={entries}
            selectedIds={selectedIds}
            onToggleSelect={dispatch ? toggleSelect : undefined}
            onShiftSelect={dispatch ? shiftSelect : undefined}
          />
        ))}
      </div>
    </div>
  );
}

// ── Transfer Group ───────────────────────────────────────────────

interface TransferGroupProps {
  status: TransferStatus;
  entries: TransferEntry[];
  selectedIds?: Set<string>;
  onToggleSelect?: (id: string) => void;
  onShiftSelect?: (id: string) => void;
}

function TransferGroup({ status, entries, selectedIds, onToggleSelect, onShiftSelect }: TransferGroupProps) {
  return (
    <section aria-label={`${STATUS_LABELS[status]} transfers`}>
      <header className="sticky top-0 z-10 flex items-center gap-2 px-3 py-1.5 bg-background bg-card/80 border-b border-border border-border">
        <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground text-muted-foreground">
          {STATUS_LABELS[status]}
        </h3>
        <span className="text-xs text-muted-foreground text-muted-foreground">
          ({entries.length})
        </span>
      </header>
      <ul role="list" className="divide-y divide-border divide-border">
        {entries.map((entry) => (
          <TransferRow
            key={entry.id}
            entry={entry}
            isSelected={selectedIds?.has(entry.id) ?? false}
            onToggleSelect={onToggleSelect}
            onShiftSelect={onShiftSelect}
          />
        ))}
      </ul>
    </section>
  );
}

// ── Transfer Row ─────────────────────────────────────────────────

interface TransferRowProps {
  entry: TransferEntry;
  isSelected: boolean;
  onToggleSelect?: (id: string) => void;
  onShiftSelect?: (id: string) => void;
}

function TransferRow({ entry, isSelected, onToggleSelect, onShiftSelect }: TransferRowProps) {
  const fileName = entry.fileName || getFileName(getFilePath(entry));
  const filePath = getFilePath(entry);
  const DirectionIcon = entry.direction === "upload" ? ArrowUpCircle : ArrowDownCircle;

  const handleClick = useCallback(
    (e: React.MouseEvent) => {
      if (!onToggleSelect && !onShiftSelect) return;
      if (e.shiftKey && onShiftSelect) {
        onShiftSelect(entry.id);
      } else if (onToggleSelect) {
        onToggleSelect(entry.id);
      }
    },
    [entry.id, onToggleSelect, onShiftSelect],
  );

  return (
    <li
      className={clsx(
        "flex items-center gap-2 px-3 py-2 hover:bg-background hover:bg-card/50",
        isSelected && "bg-info/10 bg-info/20",
      )}
    >
      {/* Checkbox for selection */}
      {onToggleSelect && (
        <input
          type="checkbox"
          checked={isSelected}
          onChange={() => onToggleSelect(entry.id)}
          onClick={(e) => {
            if (e.shiftKey && onShiftSelect) {
              e.preventDefault();
              onShiftSelect(entry.id);
            }
          }}
          className="shrink-0 w-3.5 h-3.5 rounded border-border border-border text-info focus:ring-info"
          aria-label={`Select transfer ${entry.fileName || fileName}`}
        />
      )}
      <DirectionIcon
        className={clsx(
          "w-4 h-4 shrink-0",
          entry.direction === "upload"
            ? "text-info text-info"
            : "text-success text-success",
        )}
        aria-label={entry.direction === "upload" ? "Upload" : "Download"}
      />
      <div className="flex-1 min-w-0 cursor-pointer" onClick={handleClick}>
        <p className="text-sm font-medium text-foreground text-foreground truncate">
          {fileName}
        </p>
        <p className="text-xs text-muted-foreground text-muted-foreground truncate" title={filePath}>
          {filePath}
        </p>
      </div>
      <span
        className={clsx(
          "shrink-0 px-2 py-0.5 text-xs font-medium rounded-full",
          STATUS_BADGE_CLASSES[entry.status],
        )}
      >
        {STATUS_LABELS[entry.status]}
      </span>
    </li>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { groupAndSort, STATUS_GROUP_ORDER };
