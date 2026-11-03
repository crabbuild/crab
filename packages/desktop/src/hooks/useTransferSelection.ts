/**
 * useTransferSelection — custom hook for managing transfer selection state
 * with checkbox toggle and shift-click range selection support.
 */

import { useCallback, useRef } from "react";
import type { TransferEntry } from "../../electron/ssh-types";
import type { SshEnhancedAction } from "../state/ssh-enhanced-state";

export interface UseTransferSelectionOptions {
  /** Ordered list of transfer IDs as displayed in the panel */
  orderedIds: string[];
  /** Currently selected transfer IDs */
  selectedIds: Set<string>;
  /** Dispatch function for state updates */
  dispatch: (action: SshEnhancedAction) => void;
}

export interface UseTransferSelectionResult {
  /** Toggle a single transfer's selection (checkbox click) */
  toggleSelect: (id: string) => void;
  /** Handle shift-click to select a range from the last clicked to the target */
  shiftSelect: (id: string) => void;
  /** Select all transfers */
  selectAll: () => void;
  /** Deselect all transfers */
  deselectAll: () => void;
}

/**
 * Provides selection logic for the transfer list including:
 * - Single checkbox toggle (select/deselect one item)
 * - Shift-click range selection (select all items between last click and current)
 */
export function useTransferSelection({
  orderedIds,
  selectedIds,
  dispatch,
}: UseTransferSelectionOptions): UseTransferSelectionResult {
  const lastClickedRef = useRef<string | null>(null);

  const toggleSelect = useCallback(
    (id: string) => {
      if (selectedIds.has(id)) {
        dispatch({ type: "TRANSFER_DESELECT", id });
      } else {
        dispatch({ type: "TRANSFER_SELECT", id });
      }
      lastClickedRef.current = id;
    },
    [selectedIds, dispatch],
  );

  const shiftSelect = useCallback(
    (id: string) => {
      const lastClicked = lastClickedRef.current;
      if (!lastClicked || lastClicked === id) {
        // No previous click or same item — just toggle
        toggleSelect(id);
        return;
      }

      const startIndex = orderedIds.indexOf(lastClicked);
      const endIndex = orderedIds.indexOf(id);

      if (startIndex === -1 || endIndex === -1) {
        // One of the IDs isn't in the list — fall back to toggle
        toggleSelect(id);
        return;
      }

      const from = Math.min(startIndex, endIndex);
      const to = Math.max(startIndex, endIndex);
      const rangeIds = orderedIds.slice(from, to + 1);

      dispatch({ type: "TRANSFER_SELECT_BATCH", ids: rangeIds });
      lastClickedRef.current = id;
    },
    [orderedIds, toggleSelect, dispatch],
  );

  const selectAll = useCallback(() => {
    dispatch({ type: "TRANSFER_SELECT_BATCH", ids: orderedIds });
  }, [orderedIds, dispatch]);

  const deselectAll = useCallback(() => {
    dispatch({ type: "TRANSFER_DESELECT_ALL" });
    lastClickedRef.current = null;
  }, [dispatch]);

  return { toggleSelect, shiftSelect, selectAll, deselectAll };
}

/**
 * Determines which batch actions should be enabled based on the statuses
 * of the selected transfers.
 */
export function getEnabledBatchActions(
  selectedIds: Set<string>,
  transfers: Map<string, TransferEntry>,
): { canCancel: boolean; canRetry: boolean; canClear: boolean } {
  let canCancel = false;
  let canRetry = false;
  let canClear = false;

  for (const id of selectedIds) {
    const entry = transfers.get(id);
    if (!entry) continue;

    switch (entry.status) {
      case "queued":
      case "active":
      case "scheduled":
        canCancel = true;
        break;
      case "failed":
        canRetry = true;
        canClear = true;
        break;
      case "completed":
        canClear = true;
        break;
    }

    // Short-circuit if all actions are already enabled
    if (canCancel && canRetry && canClear) break;
  }

  return { canCancel, canRetry, canClear };
}

/**
 * Computes the aggregate size of selected transfers.
 */
export function computeSelectedSize(
  selectedIds: Set<string>,
  transfers: Map<string, TransferEntry>,
): number {
  let total = 0;
  for (const id of selectedIds) {
    const entry = transfers.get(id);
    if (entry) {
      total += entry.totalBytes;
    }
  }
  return total;
}
