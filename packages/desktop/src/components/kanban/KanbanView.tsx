import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import type {
  AgentWorkflowDefinition,
  ActiveFilters,
  ColumnUIState,
  DensityMode,
  GroupingMode,
  KanbanBoardConfig,
  KanbanCardInfo,
  KanbanCardPriority,
  KanbanRunTaskOptions,
  KanbanColumnInfo,
  RelationType,
  SavedView,
} from "../../ipc";
import {
  kanbanReducer,
  initialBoardUIState,
  type KanbanState,
  initialKanbanState,
} from "../../state/kanban-store";
import { filterCards, extractFilterOptions } from "../../lib/kanban/filter-engine";
import { searchCards } from "../../lib/kanban/search-engine";
import { groupCards, type VirtualColumn } from "../../lib/kanban/group-engine";
import { FilterBar } from "./FilterBar";
import { Board } from "./Board";
import { DetailPanel } from "./DetailPanel";
import { CommandPalette, type PaletteAction } from "./CommandPalette";
import { BulkActionBar } from "./BulkActionBar";
import { ContextMenu } from "./ContextMenu";
import { ShortcutOverlay } from "./ShortcutOverlay";
import { RunTaskSheet } from "./RunTaskSheet";

// ── Props ───────────────────────────────────────────────────────────

export interface KanbanViewOrchestratorProps {
  /** Board ID for persistence calls. */
  boardId: string;
  /** All cards on the board. */
  cards: KanbanCardInfo[];
  /** All columns on the board. */
  columns: KanbanColumnInfo[];
  /** Persisted board config values to restore on mount. */
  boardConfig?: KanbanBoardConfig | null;
  initialConfig?: {
    densityMode?: DensityMode;
    savedViews?: SavedView[];
    columnStates?: ColumnUIState;
  };
  /** Called when a card is moved via drag-and-drop. */
  onCardMove: (cardId: string, targetColumnId: string, positionIndex: number) => void;
  /** Called when a new card should be created. */
  onCreateCard?: (columnId: string, title: string, description?: string) => void;
  /** Called when the agent should be run on a card. */
  onRunAgent?: (card: KanbanCardInfo, options?: KanbanRunTaskOptions) => void;
  /** Workflow definitions available from the context menu. */
  agentWorkflows?: AgentWorkflowDefinition[];
  /** Called when a workflow should be run for selected cards. */
  onRunWorkflow?: (definitionId: string, cards: KanbanCardInfo[]) => void;
  /** Symphony issue action callbacks for the detail panel. */
  onRetrySymphonyIssue?: (issueId: string) => void;
  onCancelSymphonyIssue?: (issueId: string) => void;
  onReleaseSymphonyIssue?: (issueId: string) => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Orchestrator component that wires FilterBar, Board, DetailPanel,
 * CommandPalette, BulkActionBar, ContextMenu, and ShortcutOverlay.
 *
 * Data flow:
 *   Raw cards → filterCards(filters) → searchCards(query) → groupCards(mode) → Board
 *
 * UI state (selection, panels, menus) managed via useReducer with kanbanReducer.
 * Config changes (density, views, column states) persisted via IPC on change.
 */
export function KanbanViewOrchestrator({
  boardId,
  cards,
  columns,
  boardConfig,
  initialConfig,
  onCardMove,
  onCreateCard,
  onRunAgent,
  agentWorkflows = [],
  onRunWorkflow,
  onRetrySymphonyIssue,
  onCancelSymphonyIssue,
  onReleaseSymphonyIssue,
}: KanbanViewOrchestratorProps) {
  // Initialize state with persisted config
  const [state, dispatch] = useReducer(kanbanReducer, {
    ...initialKanbanState,
    cards: new Map(cards.map((c) => [c.id, c])),
    columns,
    activeBoardId: boardId,
    boardUI: {
      ...initialBoardUIState,
      densityMode: initialConfig?.densityMode ?? "comfortable",
      columnStates: initialConfig?.columnStates ?? {},
    },
  } as KanbanState);

  const { boardUI } = state;
  const searchInputRef = useRef<HTMLInputElement>(null);
  const [runSheetCardId, setRunSheetCardId] = useState<string | null>(null);

  // Keep cards/columns in sync with props
  useEffect(() => {
    dispatch({ type: "KANBAN_BOARD_UPDATED", cards, columns });
  }, [cards, columns]);

  // Restore saved views into boards state on mount
  useEffect(() => {
    if (initialConfig?.savedViews && initialConfig.savedViews.length > 0) {
      // The saved views are stored in the board config; we set them via
      // the boards array so the reducer can manage them.
      dispatch({
        type: "KANBAN_SET_BOARDS",
        boards: [
          {
            id: boardId,
            name: "",
            repoPath: "",
            columns,
            trackerType: "local",
            trackerConfig: {},
            autonomyLevel: "conservative",
            pollIntervalMs: 60000,
            ciPollIntervalMs: 60000,
            maxIterations: 3,
            filterCriteria: {},
            savedViews: initialConfig.savedViews,
            densityMode: initialConfig.densityMode,
            columnStates: initialConfig.columnStates,
          },
        ],
      });
      dispatch({ type: "KANBAN_SET_ACTIVE_BOARD", boardId });
      // Re-apply the UI state after board switch resets it
      if (initialConfig.densityMode) {
        dispatch({ type: "KANBAN_SET_DENSITY", mode: initialConfig.densityMode });
      }
      if (initialConfig.columnStates) {
        for (const [colId, colState] of Object.entries(initialConfig.columnStates)) {
          if (colState.collapsed) {
            dispatch({ type: "KANBAN_TOGGLE_COLUMN_COLLAPSE", columnId: colId });
          }
        }
      }
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Derived data (filter → search → group pipeline) ─────────────

  const allCards = useMemo(() => Array.from(state.cards.values()), [state.cards]);

  const filteredCards = useMemo(
    () => filterCards(allCards, boardUI.activeFilters),
    [allCards, boardUI.activeFilters],
  );

  const searchedCards = useMemo(
    () => searchCards(filteredCards, boardUI.searchText),
    [filteredCards, boardUI.searchText],
  );

  const virtualColumns: VirtualColumn[] = useMemo(
    () => groupCards(searchedCards, state.columns, boardUI.groupingMode),
    [searchedCards, state.columns, boardUI.groupingMode],
  );

  // Convert virtual columns back to the format Board expects
  const boardColumns: KanbanColumnInfo[] = useMemo(
    () =>
      virtualColumns.map((vc) => {
        const original = state.columns.find((c) => c.id === vc.id);
        return original ?? { id: vc.id, name: vc.name, trackerStatusMapping: [] };
      }),
    [virtualColumns, state.columns],
  );

  const boardCards: KanbanCardInfo[] = useMemo(
    () => virtualColumns.flatMap((vc) => vc.cards),
    [virtualColumns],
  );

  const filterOptions = useMemo(() => extractFilterOptions(allCards), [allCards]);

  // Saved views from the active board config
  const savedViews: SavedView[] = useMemo(() => {
    const board = state.boards.find((b) => b.id === boardId);
    return board?.savedViews ?? initialConfig?.savedViews ?? [];
  }, [state.boards, boardId, initialConfig?.savedViews]);

  // Detail panel card
  const detailCard = useMemo(
    () =>
      boardUI.detailPanelCardId
        ? state.cards.get(boardUI.detailPanelCardId) ?? null
        : null,
    [boardUI.detailPanelCardId, state.cards],
  );

  // Close detail panel if the card was deleted (no longer in state)
  useEffect(() => {
    if (boardUI.detailPanelOpen && boardUI.detailPanelCardId && !state.cards.has(boardUI.detailPanelCardId)) {
      dispatch({ type: "KANBAN_CLOSE_DETAIL" });
    }
  }, [boardUI.detailPanelOpen, boardUI.detailPanelCardId, state.cards]);

  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ taskId?: string }>).detail;
      if (!detail?.taskId || !state.cards.has(detail.taskId)) return;
      dispatch({ type: "KANBAN_OPEN_DETAIL", cardId: detail.taskId });
    };
    window.addEventListener("crab:select-kanban-task", handler);
    return () => window.removeEventListener("crab:select-kanban-task", handler);
  }, [state.cards]);

  // Context menu target cards
  const contextMenuCards = useMemo(
    () =>
      boardUI.contextMenuCardIds
        .map((id) => state.cards.get(id))
        .filter(Boolean) as KanbanCardInfo[],
    [boardUI.contextMenuCardIds, state.cards],
  );

  const runSheetCard = useMemo(
    () => (runSheetCardId ? state.cards.get(runSheetCardId) ?? null : null),
    [runSheetCardId, state.cards],
  );

  // ── Persistence (debounced) ─────────────────────────────────────

  const persistTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const persistConfig = useCallback(
    (config: { densityMode?: DensityMode; savedViews?: SavedView[]; columnStates?: ColumnUIState }) => {
      if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
      persistTimerRef.current = setTimeout(async () => {
        try {
          await window.crab.kanban.saveBoardConfig(boardId, config);
        } catch {
          // Retry once after 500ms
          setTimeout(async () => {
            try {
              await window.crab.kanban.saveBoardConfig(boardId, config);
            } catch {
              // Settings not saved — in-memory state remains correct
            }
          }, 500);
        }
      }, 300);
    },
    [boardId],
  );

  // Persist density changes
  useEffect(() => {
    persistConfig({ densityMode: boardUI.densityMode });
  }, [boardUI.densityMode, persistConfig]);

  // Persist column state changes
  useEffect(() => {
    persistConfig({ columnStates: boardUI.columnStates });
  }, [boardUI.columnStates, persistConfig]);

  // Persist saved view changes
  useEffect(() => {
    persistConfig({ savedViews });
  }, [savedViews, persistConfig]);

  // Cleanup persist timer
  useEffect(() => {
    return () => {
      if (persistTimerRef.current) clearTimeout(persistTimerRef.current);
    };
  }, []);

  // ── FilterBar callbacks ─────────────────────────────────────────

  const handleFiltersChange = useCallback(
    (filters: ActiveFilters) => dispatch({ type: "KANBAN_SET_FILTERS", filters }),
    [],
  );

  const handleSearchChange = useCallback(
    (text: string) => dispatch({ type: "KANBAN_SET_SEARCH", text }),
    [],
  );

  const handleGroupingChange = useCallback(
    (mode: GroupingMode) => dispatch({ type: "KANBAN_SET_GROUPING", mode }),
    [],
  );

  const handleDensityChange = useCallback(
    (mode: DensityMode) => dispatch({ type: "KANBAN_SET_DENSITY", mode }),
    [],
  );

  const handleSaveView = useCallback(
    (name: string) => {
      const view: SavedView = {
        id: crypto.randomUUID(),
        name,
        filters: boardUI.activeFilters,
        groupingMode: boardUI.groupingMode,
        createdAt: Date.now(),
      };
      dispatch({ type: "KANBAN_SAVE_VIEW", view });
    },
    [boardUI.activeFilters, boardUI.groupingMode],
  );

  const handleSelectView = useCallback(
    (viewId: string) => {
      const view = savedViews.find((v) => v.id === viewId);
      if (!view) return;
      dispatch({ type: "KANBAN_SET_ACTIVE_VIEW", viewId });
      dispatch({ type: "KANBAN_SET_FILTERS", filters: view.filters });
      dispatch({ type: "KANBAN_SET_GROUPING", mode: view.groupingMode });
    },
    [savedViews],
  );

  const handleDeleteView = useCallback(
    (viewId: string) => dispatch({ type: "KANBAN_DELETE_VIEW", viewId }),
    [],
  );

  const handleRenameView = useCallback(
    (viewId: string, name: string) => dispatch({ type: "KANBAN_RENAME_VIEW", viewId, name }),
    [],
  );

  const handleClearFilters = useCallback(
    () => dispatch({ type: "KANBAN_CLEAR_FILTERS" }),
    [],
  );

  // ── Board callbacks ─────────────────────────────────────────────

  const handleCardClick = useCallback(
    (card: KanbanCardInfo, e?: React.MouseEvent) => {
      // Single click without modifiers opens detail panel
      if (!e?.shiftKey && !e?.metaKey && !e?.ctrlKey) {
        dispatch({ type: "KANBAN_OPEN_DETAIL", cardId: card.id });
      }
    },
    [],
  );

  const handleCardContextMenu = useCallback(
    (card: KanbanCardInfo, e: React.MouseEvent) => {
      e.preventDefault();
      const cardIds = boardUI.selectedCardIds.has(card.id)
        ? Array.from(boardUI.selectedCardIds)
        : [card.id];
      dispatch({
        type: "KANBAN_OPEN_CONTEXT_MENU",
        position: { x: e.clientX, y: e.clientY },
        cardIds,
      });
    },
    [boardUI.selectedCardIds],
  );

  const handleCardTitleEdit = useCallback(
    async (cardId: string, newTitle: string) => {
      const card = state.cards.get(cardId);
      if (card && isCardRuntimeLocked(card)) return;
      dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { title: newTitle } });
      dispatch({ type: "KANBAN_STOP_EDITING" });
      try {
        await window.crab.kanban.updateCard(cardId, { title: newTitle });
      } catch {
        // Revert will happen on next board-updated event
      }
    },
    [state.cards],
  );

  const handleCardRelationClick = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_OPEN_DETAIL", cardId }),
    [],
  );

  const handleToggleColumnCollapse = useCallback(
    (columnId: string) => dispatch({ type: "KANBAN_TOGGLE_COLUMN_COLLAPSE", columnId }),
    [],
  );

  // Keyboard shortcut callbacks for Board
  const handleOpenDetail = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_OPEN_DETAIL", cardId }),
    [],
  );

  const handleDeleteCard = useCallback(
    async (cardId: string) => {
      const card = state.cards.get(cardId);
      if (card && isCardRuntimeLocked(card)) return;
      const confirmed = window.confirm("Delete this card?");
      if (!confirmed) return;
      try {
        await window.crab.kanban.bulkDeleteCards([cardId]);
      } catch {
        // Deletion failed
      }
    },
    [state.cards],
  );

  const handleSetPriority = useCallback(
    async (cardId: string, priority: KanbanCardPriority) => {
      const card = state.cards.get(cardId);
      if (card && isCardRuntimeLocked(card)) return;
      dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { priority } });
      try {
        await window.crab.kanban.updateCard(cardId, { priority });
      } catch {
        // Revert on next sync
      }
    },
    [state.cards],
  );

  const handleStartEditing = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_START_EDITING", cardId }),
    [],
  );

  const handleToggleSelect = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_SELECT_CARD", cardId, mode: "toggle" }),
    [],
  );

  const handleClearSelection = useCallback(
    () => dispatch({ type: "KANBAN_CLEAR_SELECTION" }),
    [],
  );

  const handleToggleCommandPalette = useCallback(
    () => dispatch({ type: "KANBAN_TOGGLE_COMMAND_PALETTE" }),
    [],
  );

  const handleToggleShortcutOverlay = useCallback(
    () => dispatch({ type: "KANBAN_TOGGLE_SHORTCUT_OVERLAY" }),
    [],
  );

  const handleFocusSearch = useCallback(() => {
    searchInputRef.current?.focus();
  }, []);

  const handleShiftClick = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_SELECT_CARD", cardId, mode: "shift" }),
    [],
  );

  const handleMetaClick = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_SELECT_CARD", cardId, mode: "toggle" }),
    [],
  );

  // ── Detail Panel callbacks ──────────────────────────────────────

  const handleCloseDetail = useCallback(
    () => dispatch({ type: "KANBAN_CLOSE_DETAIL" }),
    [],
  );

  const handleUpdateCard = useCallback(
    async (cardId: string, updates: Partial<KanbanCardInfo>) => {
      const card = state.cards.get(cardId);
      if (card && isCardRuntimeLocked(card)) return;
      dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates });
      try {
        await window.crab.kanban.updateCard(cardId, updates);
      } catch {
        // Revert on next sync
      }
    },
    [state.cards],
  );

  const handleNavigateToCard = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_OPEN_DETAIL", cardId }),
    [],
  );

  const handleCreateRelation = useCallback(
    async (sourceId: string, targetId: string, type: RelationType) => {
      dispatch({ type: "KANBAN_ADD_RELATION", sourceId, targetId, relationType: type });
      try {
        await window.crab.kanban.addRelation(sourceId, targetId, type);
      } catch {
        // Revert on next sync
      }
    },
    [],
  );

  const handleDeleteRelation = useCallback(
    async (sourceId: string, targetId: string, type: RelationType) => {
      dispatch({ type: "KANBAN_REMOVE_RELATION", sourceId, targetId, relationType: type });
      try {
        await window.crab.kanban.removeRelation(sourceId, targetId, type);
      } catch {
        // Revert on next sync
      }
    },
    [],
  );

  // ── Context Menu callbacks ──────────────────────────────────────

  const handleCloseContextMenu = useCallback(
    () => dispatch({ type: "KANBAN_CLOSE_CONTEXT_MENU" }),
    [],
  );

  const handleContextSetPriority = useCallback(
    async (priority: KanbanCardPriority) => {
      const cardIds = boardUI.contextMenuCardIds.filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { priority } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { priority });
      } catch {
        // Revert on next sync
      }
    },
    [boardUI.contextMenuCardIds, state.cards],
  );

  const handleContextAssign = useCallback(
    async (assignee: string) => {
      const cardIds = boardUI.contextMenuCardIds.filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { assignee } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { assignee });
      } catch {
        // Revert on next sync
      }
    },
    [boardUI.contextMenuCardIds, state.cards],
  );

  const handleContextMoveToColumn = useCallback(
    async (columnId: string) => {
      const cardIds = boardUI.contextMenuCardIds.filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { columnId } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { columnId });
      } catch {
        // Revert on next sync
      }
    },
    [boardUI.contextMenuCardIds, state.cards],
  );

  const handleContextCopyId = useCallback(() => {
    const ids = boardUI.contextMenuCardIds.join(", ");
    navigator.clipboard.writeText(ids).catch(() => {});
  }, [boardUI.contextMenuCardIds]);

  const handleRunAgentRequest = useCallback((card: KanbanCardInfo) => {
    if (isCardRuntimeLocked(card)) return;
    setRunSheetCardId(card.id);
  }, []);

  const handleContextRunAgent = useCallback(() => {
    const cardId = boardUI.contextMenuCardIds.find((candidate) => {
      const card = state.cards.get(candidate);
      return card && !isCardRuntimeLocked(card);
    });
    if (cardId) setRunSheetCardId(cardId);
  }, [boardUI.contextMenuCardIds, state.cards]);

  const handleContextRunWorkflow = useCallback((definitionId: string) => {
    if (!onRunWorkflow) return;
    const cardsToRun = contextMenuCards.filter((card) => !isCardRuntimeLocked(card));
    if (cardsToRun.length > 0) onRunWorkflow(definitionId, cardsToRun);
  }, [contextMenuCards, onRunWorkflow]);

  const handleContextDelete = useCallback(async () => {
    const cardIds = boardUI.contextMenuCardIds.filter((cardId) => {
      const card = state.cards.get(cardId);
      return !card || !isCardRuntimeLocked(card);
    });
    const count = cardIds.length;
    if (count === 0) return;
    const confirmed = window.confirm(`Delete ${count} card${count > 1 ? "s" : ""}?`);
    if (!confirmed) return;
    try {
      await window.crab.kanban.bulkDeleteCards(cardIds);
    } catch {
      // Deletion failed
    }
  }, [boardUI.contextMenuCardIds, state.cards]);

  // ── Command Palette callbacks ───────────────────────────────────

  const handleCloseCommandPalette = useCallback(
    () => dispatch({ type: "KANBAN_TOGGLE_COMMAND_PALETTE" }),
    [],
  );

  const handleSelectCardFromPalette = useCallback(
    (cardId: string) => dispatch({ type: "KANBAN_OPEN_DETAIL", cardId }),
    [],
  );

  const handleScrollToColumn = useCallback((_columnId: string) => {
    // Scroll the column into view — Board handles this internally
    // For now, just close the palette
  }, []);

  const handleExecuteAction = useCallback(
    (action: PaletteAction) => {
      const id = action.id;
      if (id === "action:toggle-density-compact") {
        dispatch({ type: "KANBAN_SET_DENSITY", mode: "compact" });
      } else if (id === "action:toggle-density-comfortable") {
        dispatch({ type: "KANBAN_SET_DENSITY", mode: "comfortable" });
      } else if (id === "action:toggle-density-spacious") {
        dispatch({ type: "KANBAN_SET_DENSITY", mode: "spacious" });
      } else if (id === "action:group-default") {
        dispatch({ type: "KANBAN_SET_GROUPING", mode: "default" });
      } else if (id === "action:group-priority") {
        dispatch({ type: "KANBAN_SET_GROUPING", mode: "priority" });
      } else if (id === "action:group-label") {
        dispatch({ type: "KANBAN_SET_GROUPING", mode: "label" });
      } else if (id === "action:group-assignee") {
        dispatch({ type: "KANBAN_SET_GROUPING", mode: "assignee" });
      } else if (id === "action:create-card" && onCreateCard && columns.length > 0) {
        onCreateCard(columns[0].id, "", undefined);
      }
    },
    [columns, onCreateCard],
  );

  // ── Bulk Action Bar callbacks ───────────────────────────────────

  const handleBulkMoveToColumn = useCallback(
    async (columnId: string) => {
      const cardIds = Array.from(boardUI.selectedCardIds).filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { columnId } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { columnId });
      } catch {
        // Revert on next sync
      }
      dispatch({ type: "KANBAN_CLEAR_SELECTION" });
    },
    [boardUI.selectedCardIds, state.cards],
  );

  const handleBulkSetPriority = useCallback(
    async (priority: KanbanCardPriority) => {
      const cardIds = Array.from(boardUI.selectedCardIds).filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { priority } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { priority });
      } catch {
        // Revert on next sync
      }
      dispatch({ type: "KANBAN_CLEAR_SELECTION" });
    },
    [boardUI.selectedCardIds, state.cards],
  );

  const handleBulkAssign = useCallback(
    async (assignee: string) => {
      const cardIds = Array.from(boardUI.selectedCardIds).filter((cardId) => {
        const card = state.cards.get(cardId);
        return !card || !isCardRuntimeLocked(card);
      });
      for (const cardId of cardIds) {
        dispatch({ type: "KANBAN_UPDATE_CARD", cardId, updates: { assignee } });
      }
      try {
        await window.crab.kanban.bulkUpdateCards(cardIds, { assignee });
      } catch {
        // Revert on next sync
      }
      dispatch({ type: "KANBAN_CLEAR_SELECTION" });
    },
    [boardUI.selectedCardIds, state.cards],
  );

  const handleBulkDelete = useCallback(async () => {
    const cardIds = Array.from(boardUI.selectedCardIds).filter((cardId) => {
      const card = state.cards.get(cardId);
      return !card || !isCardRuntimeLocked(card);
    });
    const count = cardIds.length;
    if (count === 0) return;
    const confirmed = window.confirm(`Delete ${count} card${count > 1 ? "s" : ""}?`);
    if (!confirmed) return;
    try {
      await window.crab.kanban.bulkDeleteCards(cardIds);
    } catch {
      // Deletion failed
    }
    dispatch({ type: "KANBAN_CLEAR_SELECTION" });
  }, [boardUI.selectedCardIds, state.cards]);

  // ── Shortcut Overlay ────────────────────────────────────────────

  const handleCloseShortcutOverlay = useCallback(
    () => dispatch({ type: "KANBAN_TOGGLE_SHORTCUT_OVERLAY" }),
    [],
  );

  // ── Render ──────────────────────────────────────────────────────

  // Empty state for zero search results
  const showEmptyState = boardUI.searchText.length > 0 && searchedCards.length === 0;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Filter Bar */}
      <FilterBar
        filters={boardUI.activeFilters}
        searchText={boardUI.searchText}
        groupingMode={boardUI.groupingMode}
        densityMode={boardUI.densityMode}
        savedViews={savedViews}
        activeViewId={boardUI.activeViewId}
        visibleCount={searchedCards.length}
        totalCount={allCards.length}
        availableOptions={filterOptions}
        onFiltersChange={handleFiltersChange}
        onSearchChange={handleSearchChange}
        onGroupingChange={handleGroupingChange}
        onDensityChange={handleDensityChange}
        onSaveView={handleSaveView}
        onSelectView={handleSelectView}
        onDeleteView={handleDeleteView}
        onRenameView={handleRenameView}
        onClearFilters={handleClearFilters}
      />

      {/* Board area */}
      <div className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
        <div className="min-w-0 flex-1 overflow-hidden">
          {showEmptyState ? (
            <div className="flex h-full items-center justify-center">
              <p className="text-sm text-gx-muted">No cards match your search</p>
            </div>
          ) : (
            <Board
              columns={boardColumns}
              cards={boardCards}
              densityMode={boardUI.densityMode}
              selectedCardIds={boardUI.selectedCardIds}
              editingCardId={boardUI.editingCardId}
              searchHighlight={boardUI.searchText || undefined}
              groupingMode={boardUI.groupingMode}
              onCardMove={onCardMove}
              onCardClick={handleCardClick}
              onCardContextMenu={handleCardContextMenu}
              onCardTitleEdit={handleCardTitleEdit}
              onCardRelationClick={handleCardRelationClick}
              onCreateCard={onCreateCard}
              onRunAgent={handleRunAgentRequest}
              onToggleColumnCollapse={handleToggleColumnCollapse}
              columnStates={boardUI.columnStates}
              onOpenDetail={handleOpenDetail}
              onDeleteCard={handleDeleteCard}
              onSetPriority={handleSetPriority}
              onStartEditing={handleStartEditing}
              onToggleSelect={handleToggleSelect}
              onClearSelection={handleClearSelection}
              onToggleCommandPalette={handleToggleCommandPalette}
              onToggleShortcutOverlay={handleToggleShortcutOverlay}
              onFocusSearch={handleFocusSearch}
              onShiftClick={handleShiftClick}
              onMetaClick={handleMetaClick}
            />
          )}
        </div>

        {/* Detail Panel (sibling to Board, not nested) */}
        {detailCard && (
          <DetailPanel
            key={detailCard.id}
            card={detailCard}
            board={boardConfig ?? null}
            isOpen={boardUI.detailPanelOpen}
            onClose={handleCloseDetail}
            onUpdateCard={handleUpdateCard}
            onRunAgent={() => setRunSheetCardId(detailCard.id)}
            onNavigateToCard={handleNavigateToCard}
            onCreateRelation={handleCreateRelation}
            onDeleteRelation={handleDeleteRelation}
            allCards={allCards}
            onRetrySymphonyIssue={onRetrySymphonyIssue}
            onCancelSymphonyIssue={onCancelSymphonyIssue}
            onReleaseSymphonyIssue={onReleaseSymphonyIssue}
          />
        )}
      </div>

      {/* Context Menu (portal-rendered) */}
      {boardUI.contextMenuPosition && contextMenuCards.length > 0 && (
        <ContextMenu
          position={boardUI.contextMenuPosition}
          targetCards={contextMenuCards}
          columns={state.columns}
          onSetPriority={handleContextSetPriority}
          onAssign={handleContextAssign}
          onMoveToColumn={handleContextMoveToColumn}
          onCopyId={handleContextCopyId}
          onRunAgent={handleContextRunAgent}
          agentWorkflows={agentWorkflows}
          onRunWorkflow={onRunWorkflow ? handleContextRunWorkflow : undefined}
          onDelete={handleContextDelete}
          onClose={handleCloseContextMenu}
        />
      )}

      {/* Bulk Action Bar (fixed bottom) */}
      <BulkActionBar
        selectedCount={boardUI.selectedCardIds.size}
        columns={state.columns}
        onMoveToColumn={handleBulkMoveToColumn}
        onSetPriority={handleBulkSetPriority}
        onAssign={handleBulkAssign}
        onDelete={handleBulkDelete}
        onClearSelection={handleClearSelection}
      />

      {/* Command Palette (modal overlay) */}
      <CommandPalette
        isOpen={boardUI.commandPaletteOpen}
        cards={allCards}
        columns={state.columns}
        onClose={handleCloseCommandPalette}
        onSelectCard={handleSelectCardFromPalette}
        onScrollToColumn={handleScrollToColumn}
        onExecuteAction={handleExecuteAction}
      />

      {/* Shortcut Overlay (modal) */}
      <ShortcutOverlay
        isOpen={boardUI.shortcutOverlayOpen}
        onClose={handleCloseShortcutOverlay}
      />

      <RunTaskSheet
        card={runSheetCard}
        board={boardConfig ?? null}
        onClose={() => setRunSheetCardId(null)}
        onRun={(options) => {
          if (runSheetCard && onRunAgent) onRunAgent(runSheetCard, options);
          setRunSheetCardId(null);
        }}
      />
    </div>
  );
}

function isCardRuntimeLocked(card: KanbanCardInfo): boolean {
  const status = card.symphonyRuntime?.status;
  return status === "running" || status === "retrying" || status === "blocked" || status === "claimed";
}
