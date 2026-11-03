import { useCallback, useEffect, useReducer, useRef } from "react";
import {
  kanbanReducer,
  initialKanbanState,
  type QueuedCardMovedEvent,
} from "../state/kanban-store";
import type {
  KanbanCardInfo,
  KanbanColumnInfo,
} from "../ipc";

/** Maximum queued events before oldest are dropped. */
const MAX_QUEUE_SIZE = 100;

/**
 * Hook that wraps the kanban store and manages IPC subscriptions for
 * board state updates. Provides methods for board operations and
 * implements an event queue for card:moved events that retries
 * delivery when the Automation_Engine becomes available.
 */
export function useKanbanBoard() {
  const [state, dispatch] = useReducer(kanbanReducer, initialKanbanState);

  // Event queue for card:moved events that failed to deliver
  const eventQueueRef = useRef<QueuedCardMovedEvent[]>([]);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // ── IPC Event Subscriptions ─────────────────────────────────────

  useEffect(() => {
    const kanban = window.crab?.kanban;
    if (!kanban) return;

    const offBoardUpdated = kanban.onBoardUpdated(
      (payload: { boardId: string; cards: KanbanCardInfo[]; columns: KanbanColumnInfo[] }) => {
        dispatch({
          type: "KANBAN_BOARD_UPDATED",
          cards: payload.cards,
          columns: payload.columns,
        });
        dispatch({ type: "KANBAN_SYNC_COMPLETE", timestamp: Date.now() });
      },
    );

    const offCardMoved = kanban.onCardMoved(
      (payload: { cardId: string; fromColumn: string; toColumn: string }) => {
        dispatch({
          type: "KANBAN_CARD_MOVED",
          cardId: payload.cardId,
          fromColumn: payload.fromColumn,
          toColumn: payload.toColumn,
        });
      },
    );

    const offPipelineStatus = kanban.onPipelineStatus(
      (payload: { cardId: string; pipelineState: string; detail?: string }) => {
        dispatch({
          type: "KANBAN_PIPELINE_STATUS",
          cardId: payload.cardId,
          pipelineState: payload.pipelineState,
          detail: payload.detail,
        });
      },
    );

    const offRuleExecuted = kanban.onRuleExecuted(() => {
      // Rule execution events are informational — the board state
      // update will arrive via onBoardUpdated. We could dispatch a
      // notification action here if needed in the future.
    });

    const offSyncConflict = kanban.onSyncConflict(() => {
      // Sync conflicts are surfaced via the board update cycle.
      // A dedicated conflict UI can subscribe to this in the future.
    });

    const offAuthExpired = kanban.onAuthExpired(
      (payload: { trackerType: string }) => {
        dispatch({
          type: "KANBAN_SYNC_STATUS",
          status: "error",
          error: `Authentication expired for ${payload.trackerType}. Please re-authenticate.`,
        });
      },
    );

    const offNotification = kanban.onNotification(() => {
      // Notifications are handled by the main process (Electron
      // Notification API). This listener is a placeholder for
      // in-app notification toasts if needed.
    });

    return () => {
      offBoardUpdated();
      offCardMoved();
      offPipelineStatus();
      offRuleExecuted();
      offSyncConflict();
      offAuthExpired();
      offNotification();
    };
  }, []);

  // ── Event Queue: Retry Delivery ─────────────────────────────────

  const flushEventQueue = useCallback(async () => {
    const kanban = window.crab?.kanban;
    if (!kanban || eventQueueRef.current.length === 0) return;

    const queue = [...eventQueueRef.current];
    eventQueueRef.current = [];

    for (const event of queue) {
      try {
        await kanban.moveCard(event.cardId, event.targetColumnId, event.positionIndex);
      } catch {
        // Re-queue failed events (put them back at the front)
        eventQueueRef.current.unshift(event);
      }
    }

    // If there are still queued events, schedule another retry
    if (eventQueueRef.current.length > 0) {
      scheduleRetry();
    }
  }, []);

  const scheduleRetry = useCallback(() => {
    if (retryTimerRef.current) return;
    retryTimerRef.current = setTimeout(() => {
      retryTimerRef.current = null;
      flushEventQueue();
    }, 5000);
  }, [flushEventQueue]);

  // Clean up retry timer on unmount
  useEffect(() => {
    return () => {
      if (retryTimerRef.current) {
        clearTimeout(retryTimerRef.current);
      }
    };
  }, []);

  // ── Board Operations ────────────────────────────────────────────

  /** Load all boards from the main process. */
  const loadBoards = useCallback(async () => {
    const kanban = window.crab?.kanban;
    if (!kanban) return;
    try {
      const boards = await kanban.getBoards();
      dispatch({ type: "KANBAN_SET_BOARDS", boards });
    } catch {
      dispatch({ type: "KANBAN_SYNC_STATUS", status: "error", error: "Failed to load boards" });
    }
  }, []);

  /** Select and load a board by ID. */
  const selectBoard = useCallback(async (boardId: string) => {
    const kanban = window.crab?.kanban;
    if (!kanban) return;

    dispatch({ type: "KANBAN_SET_ACTIVE_BOARD", boardId });
    dispatch({ type: "KANBAN_SYNC_STATUS", status: "syncing" });

    try {
      const boardState = await kanban.getBoard(boardId);
      if (boardState) {
        dispatch({
          type: "KANBAN_BOARD_UPDATED",
          cards: boardState.cards,
          columns: boardState.columns,
        });
        dispatch({ type: "KANBAN_SYNC_COMPLETE", timestamp: Date.now() });
      }
    } catch {
      dispatch({ type: "KANBAN_SYNC_STATUS", status: "error", error: "Failed to load board" });
    }

    // Also load rules for this board
    try {
      const rules = await kanban.getRules(boardId);
      dispatch({ type: "KANBAN_SET_RULES", rules });
    } catch {
      // Non-critical — rules can be loaded later
    }
  }, []);

  /**
   * Move a card to a new column. Updates local state immediately for
   * responsiveness, then sends the IPC call. If the IPC call fails
   * (e.g., Automation_Engine unavailable), the event is queued for
   * retry on reconnect.
   */
  const moveCard = useCallback(async (
    cardId: string,
    targetColumnId: string,
    positionIndex: number,
  ) => {
    // Optimistic local update
    dispatch({
      type: "KANBAN_MOVE_CARD_LOCAL",
      cardId,
      targetColumnId,
      positionIndex,
    });

    const kanban = window.crab?.kanban;
    if (!kanban) {
      // No IPC available — queue the event
      enqueueEvent({ cardId, targetColumnId, positionIndex, queuedAt: Date.now() });
      return;
    }

    try {
      await kanban.moveCard(cardId, targetColumnId, positionIndex);
      // On successful delivery, attempt to flush any previously queued events
      flushEventQueue();
    } catch {
      // IPC call failed — queue for retry
      enqueueEvent({ cardId, targetColumnId, positionIndex, queuedAt: Date.now() });
    }
  }, [flushEventQueue]);

  /** Force a manual sync of the active board. */
  const forceSync = useCallback(async () => {
    const kanban = window.crab?.kanban;
    if (!kanban || !state.activeBoardId) return;

    dispatch({ type: "KANBAN_SYNC_STATUS", status: "syncing" });
    try {
      await kanban.forceSync(state.activeBoardId);
    } catch {
      dispatch({ type: "KANBAN_SYNC_STATUS", status: "error", error: "Sync failed" });
    }
  }, [state.activeBoardId]);

  // ── Helpers ─────────────────────────────────────────────────────

  function enqueueEvent(event: QueuedCardMovedEvent) {
    if (eventQueueRef.current.length >= MAX_QUEUE_SIZE) {
      // Drop oldest event to prevent unbounded growth
      eventQueueRef.current.shift();
    }
    eventQueueRef.current.push(event);
    scheduleRetry();
  }

  /** Get cards for a specific column, sorted by positionIndex. */
  const getColumnCards = useCallback((columnId: string): KanbanCardInfo[] => {
    const cards: KanbanCardInfo[] = [];
    for (const card of state.cards.values()) {
      if (card.columnId === columnId) {
        cards.push(card);
      }
    }
    return cards.sort((a, b) => a.positionIndex - b.positionIndex);
  }, [state.cards]);

  /** Number of queued events pending delivery. */
  const pendingEventCount = eventQueueRef.current.length;

  return {
    state,
    dispatch,
    loadBoards,
    selectBoard,
    moveCard,
    forceSync,
    getColumnCards,
    pendingEventCount,
  };
}
