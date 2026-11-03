import type {
  KanbanCardInfo,
  KanbanColumnInfo,
  KanbanBoardConfig,
  KanbanAutomationRule,
  DensityMode,
  GroupingMode,
  RelationType,
  ActiveFilters,
  SavedView,
  ColumnUIState,
} from "../ipc";

// ── State Interfaces ────────────────────────────────────────────────

export type SyncStatus = "idle" | "syncing" | "error" | "disconnected";

export interface KanbanState {
  /** All board configurations the user has created */
  boards: KanbanBoardConfig[];
  /** Currently active board ID (null when no board is selected) */
  activeBoardId: string | null;
  /** Cards for the active board, keyed by card ID */
  cards: Map<string, KanbanCardInfo>;
  /** Columns for the active board, ordered */
  columns: KanbanColumnInfo[];
  /** Current sync status with the main process / tracker */
  syncStatus: SyncStatus;
  /** Last successful sync timestamp (ms) */
  lastSyncAt: number | null;
  /** Sync error message (set when syncStatus is "error") */
  syncError: string | null;
  /** Automation rules for the active board */
  rules: KanbanAutomationRule[];
  /** Board UI state (selection, filters, panels, etc.) */
  boardUI: BoardUIState;
}

// ── Board UI State ──────────────────────────────────────────────────

export interface BoardUIState {
  /** Multi-select card IDs */
  selectedCardIds: Set<string>;

  /** Active filter criteria */
  activeFilters: ActiveFilters;
  /** Board-wide search text */
  searchText: string;
  /** Currently active saved view ID */
  activeViewId: string | null;

  /** How cards are grouped into columns */
  groupingMode: GroupingMode;

  /** Card layout density */
  densityMode: DensityMode;

  /** Card ID shown in the detail panel */
  detailPanelCardId: string | null;
  /** Whether the detail panel is open */
  detailPanelOpen: boolean;

  /** Position of the context menu (null when closed) */
  contextMenuPosition: { x: number; y: number } | null;
  /** Card IDs targeted by the context menu */
  contextMenuCardIds: string[];

  /** Whether the command palette is open */
  commandPaletteOpen: boolean;

  /** Whether the keyboard shortcut overlay is open */
  shortcutOverlayOpen: boolean;

  /** Card ID currently being inline-edited (null when not editing) */
  editingCardId: string | null;

  /** Per-column collapsed/expanded state */
  columnStates: ColumnUIState;
}

export const initialBoardUIState: BoardUIState = {
  selectedCardIds: new Set(),
  activeFilters: { assignees: [], labels: [], priorities: [], pipelineStates: [] },
  searchText: "",
  activeViewId: null,
  groupingMode: "default",
  densityMode: "comfortable",
  detailPanelCardId: null,
  detailPanelOpen: false,
  contextMenuPosition: null,
  contextMenuCardIds: [],
  commandPaletteOpen: false,
  shortcutOverlayOpen: false,
  editingCardId: null,
  columnStates: {},
};

// ── Event Queue ─────────────────────────────────────────────────────

export interface QueuedCardMovedEvent {
  cardId: string;
  targetColumnId: string;
  positionIndex: number;
  queuedAt: number;
}

// ── Action Types ────────────────────────────────────────────────────

export type KanbanAction =
  // Board management
  | { type: "KANBAN_SET_BOARDS"; boards: KanbanBoardConfig[] }
  | { type: "KANBAN_SET_ACTIVE_BOARD"; boardId: string | null }
  | { type: "KANBAN_BOARD_UPDATED"; cards: KanbanCardInfo[]; columns: KanbanColumnInfo[] }
  // Card actions
  | { type: "KANBAN_CARD_MOVED"; cardId: string; fromColumn: string; toColumn: string }
  | { type: "KANBAN_MOVE_CARD_LOCAL"; cardId: string; targetColumnId: string; positionIndex: number }
  // Sync status
  | { type: "KANBAN_SYNC_STATUS"; status: SyncStatus; error?: string }
  | { type: "KANBAN_SYNC_COMPLETE"; timestamp: number }
  // Rules
  | { type: "KANBAN_SET_RULES"; rules: KanbanAutomationRule[] }
  // Pipeline status
  | { type: "KANBAN_PIPELINE_STATUS"; cardId: string; pipelineState: string; detail?: string }
  // Selection
  | { type: "KANBAN_SELECT_CARD"; cardId: string; mode: "single" | "shift" | "toggle" }
  | { type: "KANBAN_CLEAR_SELECTION" }
  // Filters
  | { type: "KANBAN_SET_FILTERS"; filters: ActiveFilters }
  | { type: "KANBAN_SET_SEARCH"; text: string }
  | { type: "KANBAN_CLEAR_FILTERS" }
  // Views
  | { type: "KANBAN_SET_ACTIVE_VIEW"; viewId: string | null }
  | { type: "KANBAN_SAVE_VIEW"; view: SavedView }
  | { type: "KANBAN_DELETE_VIEW"; viewId: string }
  | { type: "KANBAN_RENAME_VIEW"; viewId: string; name: string }
  // Grouping
  | { type: "KANBAN_SET_GROUPING"; mode: GroupingMode }
  // Density
  | { type: "KANBAN_SET_DENSITY"; mode: DensityMode }
  // Detail panel
  | { type: "KANBAN_OPEN_DETAIL"; cardId: string }
  | { type: "KANBAN_CLOSE_DETAIL" }
  // Context menu
  | { type: "KANBAN_OPEN_CONTEXT_MENU"; position: { x: number; y: number }; cardIds: string[] }
  | { type: "KANBAN_CLOSE_CONTEXT_MENU" }
  // Command palette
  | { type: "KANBAN_TOGGLE_COMMAND_PALETTE" }
  // Shortcut overlay
  | { type: "KANBAN_TOGGLE_SHORTCUT_OVERLAY" }
  // Inline editing
  | { type: "KANBAN_START_EDITING"; cardId: string }
  | { type: "KANBAN_STOP_EDITING" }
  // Column collapse
  | { type: "KANBAN_TOGGLE_COLUMN_COLLAPSE"; columnId: string }
  // Card updates (for inline editing, relations, etc.)
  | { type: "KANBAN_UPDATE_CARD"; cardId: string; updates: Partial<KanbanCardInfo> }
  // Relations
  | { type: "KANBAN_ADD_RELATION"; sourceId: string; targetId: string; relationType: RelationType }
  | { type: "KANBAN_REMOVE_RELATION"; sourceId: string; targetId: string; relationType: RelationType };

// ── Initial State ───────────────────────────────────────────────────

export const initialKanbanState: KanbanState = {
  boards: [],
  activeBoardId: null,
  cards: new Map(),
  columns: [],
  syncStatus: "idle",
  lastSyncAt: null,
  syncError: null,
  rules: [],
  boardUI: initialBoardUIState,
};

// ── Reducer ─────────────────────────────────────────────────────────

export function kanbanReducer(state: KanbanState, action: KanbanAction): KanbanState {
  switch (action.type) {
    case "KANBAN_SET_BOARDS": {
      return { ...state, boards: action.boards };
    }

    case "KANBAN_SET_ACTIVE_BOARD": {
      return {
        ...state,
        activeBoardId: action.boardId,
        // Clear cards/columns when switching boards — they'll be repopulated
        cards: new Map(),
        columns: [],
        syncStatus: "idle",
        syncError: null,
        boardUI: initialBoardUIState,
      };
    }

    case "KANBAN_BOARD_UPDATED": {
      const cards = new Map<string, KanbanCardInfo>();
      for (const card of action.cards) {
        cards.set(card.id, card);
      }
      return { ...state, cards, columns: action.columns };
    }

    case "KANBAN_CARD_MOVED": {
      const card = state.cards.get(action.cardId);
      if (!card) return state;
      const cards = new Map(state.cards);
      cards.set(action.cardId, { ...card, columnId: action.toColumn, updatedAt: Date.now() });
      return { ...state, cards };
    }

    case "KANBAN_MOVE_CARD_LOCAL": {
      const card = state.cards.get(action.cardId);
      if (!card) return state;
      const cards = new Map(state.cards);
      cards.set(action.cardId, {
        ...card,
        columnId: action.targetColumnId,
        positionIndex: action.positionIndex,
        updatedAt: Date.now(),
      });
      return { ...state, cards };
    }

    case "KANBAN_SYNC_STATUS": {
      return {
        ...state,
        syncStatus: action.status,
        syncError: action.error ?? null,
      };
    }

    case "KANBAN_SYNC_COMPLETE": {
      return {
        ...state,
        syncStatus: "idle",
        lastSyncAt: action.timestamp,
        syncError: null,
      };
    }

    case "KANBAN_SET_RULES": {
      return { ...state, rules: action.rules };
    }

    case "KANBAN_PIPELINE_STATUS": {
      const card = state.cards.get(action.cardId);
      if (!card) return state;
      const cards = new Map(state.cards);
      cards.set(action.cardId, {
        ...card,
        pipelineState: action.pipelineState as KanbanCardInfo["pipelineState"],
        updatedAt: Date.now(),
      });
      return { ...state, cards };
    }

    // Selection
    case "KANBAN_SELECT_CARD": {
      const { cardId, mode } = action;
      const prev = state.boardUI.selectedCardIds;
      let next: Set<string>;

      if (mode === "single") {
        next = new Set([cardId]);
      } else if (mode === "toggle") {
        next = new Set(prev);
        if (next.has(cardId)) {
          next.delete(cardId);
        } else {
          next.add(cardId);
        }
      } else {
        // "shift" — add to existing selection
        next = new Set(prev);
        next.add(cardId);
      }

      return { ...state, boardUI: { ...state.boardUI, selectedCardIds: next } };
    }

    case "KANBAN_CLEAR_SELECTION": {
      return { ...state, boardUI: { ...state.boardUI, selectedCardIds: new Set() } };
    }

    // Filters
    case "KANBAN_SET_FILTERS": {
      return { ...state, boardUI: { ...state.boardUI, activeFilters: action.filters } };
    }

    case "KANBAN_SET_SEARCH": {
      return { ...state, boardUI: { ...state.boardUI, searchText: action.text } };
    }

    case "KANBAN_CLEAR_FILTERS": {
      return {
        ...state,
        boardUI: {
          ...state.boardUI,
          activeFilters: { assignees: [], labels: [], priorities: [], pipelineStates: [] },
          searchText: "",
          activeViewId: null,
        },
      };
    }

    // Views
    case "KANBAN_SET_ACTIVE_VIEW": {
      return { ...state, boardUI: { ...state.boardUI, activeViewId: action.viewId } };
    }

    case "KANBAN_SAVE_VIEW": {
      const boards = state.boards.map((b) => {
        if (b.id !== state.activeBoardId) return b;
        const existing = b.savedViews ?? [];
        if (existing.length >= 20) return b;
        return { ...b, savedViews: [...existing, action.view] };
      });
      return { ...state, boards };
    }

    case "KANBAN_DELETE_VIEW": {
      const boards = state.boards.map((b) => {
        if (b.id !== state.activeBoardId) return b;
        return { ...b, savedViews: (b.savedViews ?? []).filter((v) => v.id !== action.viewId) };
      });
      const activeViewId =
        state.boardUI.activeViewId === action.viewId ? null : state.boardUI.activeViewId;
      return { ...state, boards, boardUI: { ...state.boardUI, activeViewId } };
    }

    case "KANBAN_RENAME_VIEW": {
      const boards = state.boards.map((b) => {
        if (b.id !== state.activeBoardId) return b;
        return {
          ...b,
          savedViews: (b.savedViews ?? []).map((v) =>
            v.id === action.viewId ? { ...v, name: action.name } : v,
          ),
        };
      });
      return { ...state, boards };
    }

    // Grouping
    case "KANBAN_SET_GROUPING": {
      return { ...state, boardUI: { ...state.boardUI, groupingMode: action.mode } };
    }

    // Density
    case "KANBAN_SET_DENSITY": {
      return { ...state, boardUI: { ...state.boardUI, densityMode: action.mode } };
    }

    // Detail panel
    case "KANBAN_OPEN_DETAIL": {
      return {
        ...state,
        boardUI: { ...state.boardUI, detailPanelCardId: action.cardId, detailPanelOpen: true },
      };
    }

    case "KANBAN_CLOSE_DETAIL": {
      return {
        ...state,
        boardUI: { ...state.boardUI, detailPanelCardId: null, detailPanelOpen: false },
      };
    }

    // Context menu
    case "KANBAN_OPEN_CONTEXT_MENU": {
      return {
        ...state,
        boardUI: {
          ...state.boardUI,
          contextMenuPosition: action.position,
          contextMenuCardIds: action.cardIds,
        },
      };
    }

    case "KANBAN_CLOSE_CONTEXT_MENU": {
      return {
        ...state,
        boardUI: { ...state.boardUI, contextMenuPosition: null, contextMenuCardIds: [] },
      };
    }

    // Command palette
    case "KANBAN_TOGGLE_COMMAND_PALETTE": {
      return {
        ...state,
        boardUI: { ...state.boardUI, commandPaletteOpen: !state.boardUI.commandPaletteOpen },
      };
    }

    // Shortcut overlay
    case "KANBAN_TOGGLE_SHORTCUT_OVERLAY": {
      return {
        ...state,
        boardUI: { ...state.boardUI, shortcutOverlayOpen: !state.boardUI.shortcutOverlayOpen },
      };
    }

    // Inline editing
    case "KANBAN_START_EDITING": {
      return { ...state, boardUI: { ...state.boardUI, editingCardId: action.cardId } };
    }

    case "KANBAN_STOP_EDITING": {
      return { ...state, boardUI: { ...state.boardUI, editingCardId: null } };
    }

    // Column collapse
    case "KANBAN_TOGGLE_COLUMN_COLLAPSE": {
      const { columnId } = action;
      const prev = state.boardUI.columnStates[columnId];
      const collapsed = prev ? !prev.collapsed : true;
      return {
        ...state,
        boardUI: {
          ...state.boardUI,
          columnStates: { ...state.boardUI.columnStates, [columnId]: { collapsed } },
        },
      };
    }

    // Card updates
    case "KANBAN_UPDATE_CARD": {
      const card = state.cards.get(action.cardId);
      if (!card) return state;
      const cards = new Map(state.cards);
      cards.set(action.cardId, { ...card, ...action.updates, updatedAt: Date.now() });
      return { ...state, cards };
    }

    // Relations
    case "KANBAN_ADD_RELATION": {
      const { sourceId, targetId, relationType } = action;
      const sourceCard = state.cards.get(sourceId);
      if (!sourceCard) return state;

      const cards = new Map(state.cards);
      const sourceRelations = [...(sourceCard.relations ?? [])];
      const alreadyExists = sourceRelations.some(
        (r) => r.targetCardId === targetId && r.type === relationType,
      );
      if (!alreadyExists) {
        sourceRelations.push({ targetCardId: targetId, type: relationType });
        cards.set(sourceId, { ...sourceCard, relations: sourceRelations, updatedAt: Date.now() });
      }

      // Add inverse relation on target if it exists
      const targetCard = state.cards.get(targetId);
      if (targetCard) {
        const inverseType = getInverseRelationType(relationType);
        const targetRelations = [...(targetCard.relations ?? [])];
        const inverseExists = targetRelations.some(
          (r) => r.targetCardId === sourceId && r.type === inverseType,
        );
        if (!inverseExists) {
          targetRelations.push({ targetCardId: sourceId, type: inverseType });
          cards.set(targetId, { ...targetCard, relations: targetRelations, updatedAt: Date.now() });
        }
      }

      return { ...state, cards };
    }

    case "KANBAN_REMOVE_RELATION": {
      const { sourceId, targetId, relationType } = action;
      const sourceCard = state.cards.get(sourceId);
      if (!sourceCard) return state;

      const cards = new Map(state.cards);
      const sourceRelations = (sourceCard.relations ?? []).filter(
        (r) => !(r.targetCardId === targetId && r.type === relationType),
      );
      cards.set(sourceId, { ...sourceCard, relations: sourceRelations, updatedAt: Date.now() });

      // Remove inverse relation on target if it exists
      const targetCard = state.cards.get(targetId);
      if (targetCard) {
        const inverseType = getInverseRelationType(relationType);
        const targetRelations = (targetCard.relations ?? []).filter(
          (r) => !(r.targetCardId === sourceId && r.type === inverseType),
        );
        cards.set(targetId, { ...targetCard, relations: targetRelations, updatedAt: Date.now() });
      }

      return { ...state, cards };
    }

    default:
      return state;
  }
}

function getInverseRelationType(type: RelationType): RelationType {
  switch (type) {
    case "parent":
      return "child";
    case "child":
      return "parent";
    case "blocks":
      return "blocked-by";
    case "blocked-by":
      return "blocks";
    case "relates-to":
      return "relates-to";
  }
}
