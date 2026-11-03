import { useCallback, useState } from "react";
import type { KanbanBoardAnalytics, KanbanCardAnalytics } from "../ipc";

interface UseKanbanAnalyticsResult {
  boardAnalytics: KanbanBoardAnalytics | null;
  cardAnalytics: KanbanCardAnalytics | null;
  loading: boolean;
  error: string | null;
  fetchBoardAnalytics: (boardId: string) => Promise<void>;
  fetchCardAnalytics: (cardId: string) => Promise<void>;
  clearCardAnalytics: () => void;
}

/**
 * Hook that fetches analytics data on-demand via kanban IPC.
 * Analytics are computed when the panel opens, not continuously.
 */
export function useKanbanAnalytics(): UseKanbanAnalyticsResult {
  const [boardAnalytics, setBoardAnalytics] = useState<KanbanBoardAnalytics | null>(null);
  const [cardAnalytics, setCardAnalytics] = useState<KanbanCardAnalytics | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchBoardAnalytics = useCallback(async (boardId: string) => {
    const kanban = window.crab?.kanban;
    if (!kanban) {
      setError("IPC not available");
      return;
    }

    setLoading(true);
    setError(null);

    try {
      const data = await kanban.getAnalytics(boardId);
      setBoardAnalytics(data);
    } catch {
      setError("Failed to load analytics");
      setBoardAnalytics(null);
    } finally {
      setLoading(false);
    }
  }, []);

  const fetchCardAnalytics = useCallback(async (cardId: string) => {
    const kanban = window.crab?.kanban;
    if (!kanban) {
      setError("IPC not available");
      return;
    }

    setLoading(true);
    setError(null);

    try {
      const data = await kanban.getCardAnalytics(cardId);
      setCardAnalytics(data);
    } catch {
      setError("Failed to load card analytics");
      setCardAnalytics(null);
    } finally {
      setLoading(false);
    }
  }, []);

  const clearCardAnalytics = useCallback(() => {
    setCardAnalytics(null);
  }, []);

  return {
    boardAnalytics,
    cardAnalytics,
    loading,
    error,
    fetchBoardAnalytics,
    fetchCardAnalytics,
    clearCardAnalytics,
  };
}
