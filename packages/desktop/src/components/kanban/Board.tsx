import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  DndContext,
  DragEndEvent,
  DragOverEvent,
  DragStartEvent,
  DragCancelEvent,
  DragOverlay,
  KeyboardSensor,
  MeasuringStrategy,
  pointerWithin,
  rectIntersection,
  PointerSensor,
  useSensor,
  useSensors,
  type CollisionDetection,
} from "@dnd-kit/core";
import { arrayMove, sortableKeyboardCoordinates } from "@dnd-kit/sortable";
import { Column } from "./Column";
import { Card } from "./Card";
import type { DensityMode, KanbanCardInfo, KanbanCardPriority, KanbanColumnInfo } from "../../ipc";

/** Returns true when the user prefers reduced motion. */
function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  });

  useEffect(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const handler = (e: MediaQueryListEvent) => setReduced(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  return reduced;
}

const measuringConfig = {
  droppable: {
    strategy: MeasuringStrategy.Always,
  },
};

/**
 * Custom collision detection for kanban boards.
 * Uses pointerWithin first (detects which column the pointer is inside),
 * then falls back to rectIntersection for edge cases.
 * This ensures empty columns are properly detected as drop targets.
 */
const kanbanCollisionDetection: CollisionDetection = (args) => {
  // First try pointerWithin — this reliably detects the column the pointer is in
  const pointerCollisions = pointerWithin(args);
  if (pointerCollisions.length > 0) {
    return pointerCollisions;
  }
  // Fallback to rect intersection for edge cases (e.g., keyboard drag)
  return rectIntersection(args);
};

interface BoardProps {
  columns: KanbanColumnInfo[];
  cards: KanbanCardInfo[];
  densityMode?: DensityMode;
  selectedCardIds?: Set<string>;
  editingCardId?: string | null;
  searchHighlight?: string;
  groupingMode?: string;
  /** Maps cardId → latest agent status message for live progress display. */
  agentProgress?: Map<string, string>;
  onCardMove: (cardId: string, targetColumnId: string, positionIndex: number) => void;
  onCardClick: (card: KanbanCardInfo, e?: React.MouseEvent) => void;
  onCardContextMenu?: (card: KanbanCardInfo, e: React.MouseEvent) => void;
  onCardTitleEdit?: (cardId: string, newTitle: string) => void;
  onCardRelationClick?: (cardId: string) => void;
  onCreateCard?: (columnId: string, title: string, description?: string) => void;
  onRunAgent?: (card: KanbanCardInfo) => void;
  onToggleColumnCollapse?: (columnId: string) => void;
  columnStates?: Record<string, { collapsed: boolean }>;
  // Keyboard shortcut callbacks
  onOpenDetail?: (cardId: string) => void;
  onDeleteCard?: (cardId: string) => void;
  onSetPriority?: (cardId: string, priority: KanbanCardPriority) => void;
  onStartEditing?: (cardId: string) => void;
  onToggleSelect?: (cardId: string) => void;
  onClearSelection?: () => void;
  onToggleCommandPalette?: () => void;
  onToggleShortcutOverlay?: () => void;
  onFocusSearch?: () => void;
  // Multi-selection callbacks
  onShiftClick?: (cardId: string) => void;
  onMetaClick?: (cardId: string) => void;
}

/**
 * Kanban board component. Renders columns in a horizontal layout
 * with DndContext from @dnd-kit/core for drag-and-drop card movement.
 * Uses DragOverlay for a floating ghost card and KeyboardSensor for accessibility.
 */
export function Board({
  columns,
  cards,
  densityMode = "comfortable",
  selectedCardIds = new Set(),
  editingCardId: _editingCardId = null,
  searchHighlight,
  groupingMode,
  onCardMove,
  onCardClick,
  onCardContextMenu,
  onCardTitleEdit,
  onCardRelationClick,
  onCreateCard,
  onRunAgent,
  onToggleColumnCollapse,
  columnStates = {},
  onOpenDetail,
  onDeleteCard,
  onSetPriority,
  onStartEditing,
  onToggleSelect,
  onClearSelection,
  onToggleCommandPalette,
  onToggleShortcutOverlay,
  onFocusSearch,
  onShiftClick,
  onMetaClick,
}: BoardProps) {
  const reducedMotion = useReducedMotion();
  const [activeCardId, setActiveCardId] = useState<string | null>(null);
  const [overItemId, setOverItemId] = useState<string | null>(null);
  const [announcement, setAnnouncement] = useState<string>("");
  const announcementTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isKeyboardDragRef = useRef(false);
  const boardRef = useRef<HTMLDivElement>(null);

  // Track which card currently has keyboard focus
  const [focusedCardId, setFocusedCardId] = useState<string | null>(null);

  // Clear announcement after 1 second to prevent stale readings
  const announce = useCallback((message: string) => {
    if (announcementTimeoutRef.current) {
      clearTimeout(announcementTimeoutRef.current);
    }
    setAnnouncement(message);
    announcementTimeoutRef.current = setTimeout(() => {
      setAnnouncement("");
      announcementTimeoutRef.current = null;
    }, 1000);
  }, []);

  // Clean up timeout on unmount
  useEffect(() => {
    return () => {
      if (announcementTimeoutRef.current) {
        clearTimeout(announcementTimeoutRef.current);
      }
    };
  }, []);

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  // Group cards by column
  const cardsByColumn = useMemo(() => {
    const map = new Map<string, KanbanCardInfo[]>();
    for (const col of columns) {
      map.set(col.id, []);
    }
    for (const card of cards) {
      const list = map.get(card.columnId);
      if (list) {
        list.push(card);
      }
    }
    // Sort cards within each column by positionIndex
    for (const [, list] of map) {
      list.sort((a, b) => a.positionIndex - b.positionIndex);
    }
    return map;
  }, [columns, cards]);

  const activeCard = useMemo(
    () => (activeCardId ? cards.find((c) => c.id === activeCardId) ?? null : null),
    [activeCardId, cards],
  );

  // Flat ordered list of visible card IDs for arrow-key navigation
  // (respects column order and card position within each column)
  const flatCardIds = useMemo(() => {
    const ids: string[] = [];
    for (const col of columns) {
      if (columnStates[col.id]?.collapsed) continue;
      const colCards = cardsByColumn.get(col.id) ?? [];
      for (const card of colCards) {
        ids.push(card.id);
      }
    }
    return ids;
  }, [columns, cardsByColumn, columnStates]);

  // Keyboard shortcut handler
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      // Suppress shortcuts when typing in input/textarea/contenteditable
      const target = e.target as HTMLElement;
      if (
        target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.isContentEditable
      ) {
        return;
      }

      // Cmd+K / Ctrl+K — command palette
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        onToggleCommandPalette?.();
        return;
      }

      // Don't handle shortcuts with other modifier keys (except Escape)
      if (e.metaKey || e.ctrlKey || e.altKey) {
        return;
      }

      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          if (flatCardIds.length === 0) break;
          const currentIdx = focusedCardId ? flatCardIds.indexOf(focusedCardId) : -1;
          const nextIdx = currentIdx < flatCardIds.length - 1 ? currentIdx + 1 : 0;
          setFocusedCardId(flatCardIds[nextIdx]);
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          if (flatCardIds.length === 0) break;
          const currentIdx = focusedCardId ? flatCardIds.indexOf(focusedCardId) : -1;
          const prevIdx = currentIdx > 0 ? currentIdx - 1 : flatCardIds.length - 1;
          setFocusedCardId(flatCardIds[prevIdx]);
          break;
        }
        case "ArrowRight": {
          e.preventDefault();
          if (flatCardIds.length === 0) break;
          // Move focus to first card of next column
          const currentCard = focusedCardId ? cards.find((c) => c.id === focusedCardId) : null;
          const visibleColumns = columns.filter((col) => !columnStates[col.id]?.collapsed);
          if (!currentCard) {
            const firstCol = visibleColumns[0];
            if (firstCol) {
              const firstCards = cardsByColumn.get(firstCol.id) ?? [];
              if (firstCards.length > 0) setFocusedCardId(firstCards[0].id);
            }
            break;
          }
          const colIdx = visibleColumns.findIndex((col) => col.id === currentCard.columnId);
          const nextColIdx = colIdx < visibleColumns.length - 1 ? colIdx + 1 : 0;
          const nextColCards = cardsByColumn.get(visibleColumns[nextColIdx].id) ?? [];
          if (nextColCards.length > 0) setFocusedCardId(nextColCards[0].id);
          break;
        }
        case "ArrowLeft": {
          e.preventDefault();
          if (flatCardIds.length === 0) break;
          // Move focus to first card of previous column
          const currentCard = focusedCardId ? cards.find((c) => c.id === focusedCardId) : null;
          const visibleColumns = columns.filter((col) => !columnStates[col.id]?.collapsed);
          if (!currentCard) {
            const lastCol = visibleColumns[visibleColumns.length - 1];
            if (lastCol) {
              const lastCards = cardsByColumn.get(lastCol.id) ?? [];
              if (lastCards.length > 0) setFocusedCardId(lastCards[0].id);
            }
            break;
          }
          const colIdx = visibleColumns.findIndex((col) => col.id === currentCard.columnId);
          const prevColIdx = colIdx > 0 ? colIdx - 1 : visibleColumns.length - 1;
          const prevColCards = cardsByColumn.get(visibleColumns[prevColIdx].id) ?? [];
          if (prevColCards.length > 0) setFocusedCardId(prevColCards[0].id);
          break;
        }
        case "c":
        case "C": {
          // Create card in the focused card's column
          if (focusedCardId && onCreateCard) {
            const card = cards.find((c) => c.id === focusedCardId);
            if (card) {
              e.preventDefault();
              onCreateCard(card.columnId, "", undefined);
            }
          }
          break;
        }
        case "d":
        case "D":
        case "Backspace": {
          if (focusedCardId && onDeleteCard) {
            e.preventDefault();
            onDeleteCard(focusedCardId);
          }
          break;
        }
        case "0": {
          if (focusedCardId && onSetPriority) {
            e.preventDefault();
            onSetPriority(focusedCardId, "none");
          }
          break;
        }
        case "1": {
          if (focusedCardId && onSetPriority) {
            e.preventDefault();
            onSetPriority(focusedCardId, "critical");
          }
          break;
        }
        case "2": {
          if (focusedCardId && onSetPriority) {
            e.preventDefault();
            onSetPriority(focusedCardId, "high");
          }
          break;
        }
        case "3": {
          if (focusedCardId && onSetPriority) {
            e.preventDefault();
            onSetPriority(focusedCardId, "medium");
          }
          break;
        }
        case "4": {
          if (focusedCardId && onSetPriority) {
            e.preventDefault();
            onSetPriority(focusedCardId, "low");
          }
          break;
        }
        case "e":
        case "E": {
          if (focusedCardId && onStartEditing) {
            e.preventDefault();
            onStartEditing(focusedCardId);
          }
          break;
        }
        case "Enter": {
          if (focusedCardId && onOpenDetail) {
            e.preventDefault();
            onOpenDetail(focusedCardId);
          }
          break;
        }
        case "x":
        case "X": {
          if (focusedCardId && onToggleSelect) {
            e.preventDefault();
            onToggleSelect(focusedCardId);
          }
          break;
        }
        case "/": {
          e.preventDefault();
          onFocusSearch?.();
          break;
        }
        case "?": {
          e.preventDefault();
          onToggleShortcutOverlay?.();
          break;
        }
        case "Escape": {
          e.preventDefault();
          onClearSelection?.();
          setFocusedCardId(null);
          break;
        }
      }
    }

    const boardEl = boardRef.current;
    if (boardEl) {
      boardEl.addEventListener("keydown", handleKeyDown);
      return () => boardEl.removeEventListener("keydown", handleKeyDown);
    }
  }, [
    flatCardIds,
    focusedCardId,
    cards,
    columns,
    cardsByColumn,
    columnStates,
    onCreateCard,
    onDeleteCard,
    onSetPriority,
    onStartEditing,
    onOpenDetail,
    onToggleSelect,
    onClearSelection,
    onToggleCommandPalette,
    onToggleShortcutOverlay,
    onFocusSearch,
  ]);

  // Cancel active drag when grouping mode changes
  const prevGroupingModeRef = useRef(groupingMode);
  useEffect(() => {
    if (prevGroupingModeRef.current !== groupingMode && activeCardId != null) {
      // Grouping changed during an active drag — cancel the drag
      setActiveCardId(null);
      setOverItemId(null);
      if (boardRef.current) {
        boardRef.current.classList.remove("kanban-dragging");
      }
    }
    prevGroupingModeRef.current = groupingMode;
  }, [groupingMode, activeCardId]);

  // Handle card click with modifier key detection for multi-selection
  const handleCardClickWithModifiers = useCallback(
    (card: KanbanCardInfo, e?: React.MouseEvent) => {
      // Update focus to the clicked card
      setFocusedCardId(card.id);

      if (e?.shiftKey && onShiftClick) {
        e.preventDefault();
        onShiftClick(card.id);
        return;
      }
      if ((e?.metaKey || e?.ctrlKey) && onMetaClick) {
        e.preventDefault();
        onMetaClick(card.id);
        return;
      }
      // Default: single click without modifiers
      onCardClick(card, e);
    },
    [onCardClick, onShiftClick, onMetaClick],
  );

  const handleDragStart = useCallback((event: DragStartEvent) => {
    setActiveCardId(event.active.id as string);
    // Detect keyboard-initiated drag via activatorEvent type
    const isKeyboard = event.activatorEvent instanceof KeyboardEvent;
    isKeyboardDragRef.current = isKeyboard;
    if (isKeyboard) {
      const card = cards.find((c) => c.id === event.active.id);
      if (card) {
        announce(`Picked up card: ${card.title}`);
      }
    }
    // Add will-change: transform to all card elements during drag
    if (boardRef.current) {
      boardRef.current.classList.add("kanban-dragging");
    }
  }, [cards, announce]);

  const handleDragOver = useCallback((event: DragOverEvent) => {
    setOverItemId(event.over?.id as string | null ?? null);
  }, []);

  const handleDragCancel = useCallback((_event: DragCancelEvent) => {
    if (isKeyboardDragRef.current) {
      announce("Drag cancelled");
    }
    isKeyboardDragRef.current = false;
    setActiveCardId(null);
    setOverItemId(null);
    // Remove will-change hint after drag ends
    if (boardRef.current) {
      boardRef.current.classList.remove("kanban-dragging");
    }
  }, [announce]);

  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      const wasKeyboardDrag = isKeyboardDragRef.current;
      isKeyboardDragRef.current = false;
      setActiveCardId(null);
      setOverItemId(null);
      // Remove will-change hint after drag ends
      if (boardRef.current) {
        boardRef.current.classList.remove("kanban-dragging");
      }

      const { active, over } = event;
      if (!over) return;

      const cardId = active.id as string;
      const overId = over.id as string;

      const sourceCard = cards.find((c) => c.id === cardId);
      if (!sourceCard) return;

      // Determine target column: if dropped on a column, use that column;
      // if dropped on a card, use that card's column.
      let targetColumnId: string;
      let positionIndex: number;

      const isColumn = columns.some((col) => col.id === overId);
      if (isColumn) {
        targetColumnId = overId;
        const colCards = cardsByColumn.get(targetColumnId) ?? [];
        positionIndex = colCards.length; // append to end
      } else {
        // Dropped on another card — find its column and position
        const targetCard = cards.find((c) => c.id === overId);
        if (!targetCard) return;
        targetColumnId = targetCard.columnId;
        const colCards = cardsByColumn.get(targetColumnId) ?? [];

        // Intra-column reorder: use arrayMove to compute new position
        if (sourceCard.columnId === targetColumnId) {
          const sourceIndex = colCards.findIndex((c) => c.id === cardId);
          const destinationIndex = colCards.findIndex((c) => c.id === overId);
          if (sourceIndex === -1 || destinationIndex === -1) return;
          if (sourceIndex === destinationIndex) return;

          // Use arrayMove to compute the reordered list
          const reordered = arrayMove(colCards, sourceIndex, destinationIndex);
          // The card's new positionIndex is its index in the reordered array
          positionIndex = reordered.findIndex((c) => c.id === cardId);
        } else {
          // Cross-column move: position is where the target card is
          positionIndex = colCards.findIndex((c) => c.id === overId);
          if (positionIndex === -1) positionIndex = colCards.length;
        }
      }

      // Only fire if something actually changed
      if (sourceCard.columnId === targetColumnId && sourceCard.positionIndex === positionIndex) {
        return;
      }

      // Announce drop for keyboard users
      if (wasKeyboardDrag) {
        const targetColumn = columns.find((col) => col.id === targetColumnId);
        const columnName = targetColumn?.name ?? targetColumnId;
        announce(`Dropped card: ${sourceCard.title} in ${columnName} at position ${positionIndex}`);
      }

      onCardMove(cardId, targetColumnId, positionIndex);
    },
    [columns, cards, cardsByColumn, onCardMove, announce],
  );

  return (
    <DndContext
      sensors={sensors}
      collisionDetection={kanbanCollisionDetection}
      measuring={measuringConfig}
      onDragStart={handleDragStart}
      onDragOver={handleDragOver}
      onDragCancel={handleDragCancel}
      onDragEnd={handleDragEnd}
    >
      <div
        ref={boardRef}
        className="flex h-full gap-2 overflow-x-auto p-2"
        tabIndex={0}
        role="region"
        aria-label="Kanban board"
      >
        {columns.map((column, index) => (
          <Column
            key={column.id}
            column={column}
            cards={cardsByColumn.get(column.id) ?? []}
            densityMode={densityMode}
            isCollapsed={columnStates[column.id]?.collapsed ?? false}
            selectedCardIds={selectedCardIds}
            searchHighlight={searchHighlight}
            onCardClick={(card, e) => handleCardClickWithModifiers(card, e)}
            onCardContextMenu={onCardContextMenu ?? (() => {})}
            onCardTitleEdit={onCardTitleEdit ?? (() => {})}
            onCardRelationClick={onCardRelationClick}
            onCreateCard={onCreateCard}
            onRunAgent={onRunAgent}
            onToggleCollapse={onToggleColumnCollapse ?? (() => {})}
            activeCardId={activeCardId}
            overItemId={overItemId}
            columnIndex={index}
          />
        ))}
      </div>
      <DragOverlay
        dropAnimation={reducedMotion ? null : undefined}
        transition={reducedMotion ? "none" : undefined}
      >
        {activeCard ? (
          <div className="shadow-lg scale-[1.02] kanban-drag-active">
            <Card
              card={activeCard}
              densityMode={densityMode}
              isSelected={selectedCardIds.has(activeCard.id)}
              isEditing={false}
              onClick={() => {}}
              onContextMenu={() => {}}
              onTitleEdit={() => {}}
              onRelationClick={() => {}}
            />
          </div>
        ) : null}
      </DragOverlay>
      <div
        role="status"
        aria-live="assertive"
        aria-atomic="true"
        className="sr-only"
      >
        {announcement}
      </div>
    </DndContext>
  );
}
