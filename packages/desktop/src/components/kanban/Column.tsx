import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import { ChevronRight, Plus } from "lucide-react";
import { useDroppable } from "@dnd-kit/core";
import { SortableContext, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { FixedSizeList as List } from "react-window";
import { Card } from "./Card";
import type { DensityMode, KanbanCardInfo, KanbanColumnInfo } from "../../ipc";

/** 6-color palette for column header dots, cycling by position index. */
const COLUMN_COLORS = [
  "#6366f1", // indigo
  "#8b5cf6", // violet
  "#ec4899", // pink
  "#f59e0b", // amber
  "#10b981", // emerald
  "#06b6d4", // cyan
];

interface ColumnProps {
  column: KanbanColumnInfo;
  cards: KanbanCardInfo[];
  densityMode: DensityMode;
  isCollapsed: boolean;
  selectedCardIds: Set<string>;
  searchHighlight?: string;
  onCardClick: (card: KanbanCardInfo, e: React.MouseEvent) => void;
  onCardContextMenu: (card: KanbanCardInfo, e: React.MouseEvent) => void;
  onCardTitleEdit: (cardId: string, newTitle: string) => void;
  onCardRelationClick?: (cardId: string) => void;
  onCreateCard?: (columnId: string, title: string, description?: string) => void;
  onRunAgent?: (card: KanbanCardInfo) => void;
  onToggleCollapse: (columnId: string) => void;
  activeCardId?: string | null;
  overItemId?: string | null;
  /** Position index for color cycling (0-based). */
  columnIndex?: number;
}

const CARD_HEIGHT = 100;
const VIRTUALIZATION_THRESHOLD = 50;

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

/**
 * Kanban column component. Supports collapsible state (40px vertical strip),
 * polished headers with colored dot and WIP limit badge, density mode passthrough,
 * and hover-reveal action buttons. Uses react-window for virtualization when
 * cards exceed 50. Acts as a drop target for @dnd-kit drag-and-drop.
 */
export function Column({
  column,
  cards,
  densityMode,
  isCollapsed,
  selectedCardIds,
  searchHighlight,
  onCardClick,
  onCardContextMenu,
  onCardTitleEdit,
  onCardRelationClick,
  onCreateCard,
  onRunAgent,
  onToggleCollapse,
  activeCardId,
  overItemId,
  columnIndex = 0,
}: ColumnProps) {
  const reducedMotion = useReducedMotion();
  const { setNodeRef, isOver } = useDroppable({
    id: column.id,
    disabled: isCollapsed,
  });
  const [isAdding, setIsAdding] = useState(false);
  const [newTitle, setNewTitle] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const [isHeaderHovered, setIsHeaderHovered] = useState(false);

  const cardIds = cards.map((c) => c.id);
  const isOverLimit = column.wipLimit != null && cards.length > column.wipLimit;
  const dotColor = COLUMN_COLORS[columnIndex % 6];

  // Drop highlight logic (only when not collapsed)
  const isDragActive = activeCardId != null;
  const isOverThisColumn = overItemId === column.id || cards.some((c) => c.id === overItemId);
  const showDropHighlight = !isCollapsed && isDragActive && (isOver || isOverThisColumn);
  const wouldExceedWip = column.wipLimit != null && cards.length + 1 > column.wipLimit;

  // Insertion line index for drag feedback
  const insertionIndex = overItemId != null ? cards.findIndex((c) => c.id === overItemId) : -1;

  // Close card creation form when column is collapsed
  useEffect(() => {
    if (isCollapsed && isAdding) {
      setIsAdding(false);
      setNewTitle("");
      setNewDescription("");
    }
  }, [isCollapsed, isAdding]);

  const handleSubmitCard = useCallback(() => {
    const trimmed = newTitle.trim();
    if (trimmed && onCreateCard) {
      onCreateCard(column.id, trimmed, newDescription.trim() || undefined);
      setNewTitle("");
      setNewDescription("");
      setIsAdding(false);
    }
  }, [newTitle, newDescription, onCreateCard, column.id]);

  // No-op fallback for relation click if not provided
  const handleRelationClick = onCardRelationClick ?? (() => {});

  const renderCard = useCallback(
    (card: KanbanCardInfo, index: number) => (
      <div key={card.id}>
        {insertionIndex === index && (
          <div className="h-0.5 bg-gx-accent rounded-full mx-0 mb-1.5" />
        )}
        <div className="mb-1.5">
          <Card
            card={card}
            densityMode={densityMode}
            isSelected={selectedCardIds.has(card.id)}
            isEditing={false}
            searchHighlight={searchHighlight}
            onClick={(e) => onCardClick(card, e)}
            onContextMenu={(e) => onCardContextMenu(card, e)}
            onTitleEdit={onCardTitleEdit}
            onRelationClick={handleRelationClick}
            onRunAgent={onRunAgent}
          />
        </div>
      </div>
    ),
    [
      onCardClick,
      onCardContextMenu,
      onCardTitleEdit,
      handleRelationClick,
      onRunAgent,
      insertionIndex,
      densityMode,
      selectedCardIds,
      searchHighlight,
    ],
  );

  // Collapsed state: 40px vertical strip with rotated name and card count
  if (isCollapsed) {
    return (
      <div
        className={clsx(
          "flex h-full w-10 shrink-0 flex-col items-center rounded border border-gx-border bg-gx-bg-sub cursor-pointer select-none",
          !reducedMotion && "transition-kanban-column-collapse",
        )}
        onClick={() => onToggleCollapse(column.id)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggleCollapse(column.id);
          }
        }}
        title={`Expand ${column.name}`}
      >
        {/* Rotated column name */}
        <div className="flex-1 flex items-center justify-center min-h-0">
          <span
            className="text-xs font-medium text-gx-text whitespace-nowrap"
            style={{ writingMode: "vertical-rl", transform: "rotate(180deg)" }}
          >
            {column.name}
          </span>
        </div>
        {/* Card count badge */}
        <div className="pb-2">
          <span
            className={clsx(
              "inline-flex items-center justify-center rounded-full px-1.5 py-0.5 text-[10px] min-w-[20px]",
              isOverLimit
                ? "bg-gx-error/10 text-gx-error"
                : "bg-gx-surface text-gx-muted",
            )}
          >
            {cards.length}
          </span>
        </div>
      </div>
    );
  }

  return (
    <div
      ref={setNodeRef}
      className={clsx(
        "flex h-full w-64 shrink-0 flex-col rounded border bg-gx-bg-sub",
        !reducedMotion && "transition-kanban-column-collapse",
        showDropHighlight
          ? wouldExceedWip
            ? "border-gx-error"
            : "border-gx-accent/60"
          : "border-gx-border",
      )}
    >
      {/* Column header — fixed 36px height */}
      <div
        className="flex h-9 items-center gap-2 border-b border-gx-border px-3 shrink-0"
        onMouseEnter={() => setIsHeaderHovered(true)}
        onMouseLeave={() => setIsHeaderHovered(false)}
      >
        {/* Colored dot */}
        <span
          className="h-1 w-1 shrink-0 rounded-full"
          style={{ backgroundColor: dotColor }}
        />
        {/* Column name */}
        <span className="text-xs font-medium text-gx-text truncate">
          {column.name}
        </span>
        {/* Card count badge */}
        <span
          className={clsx(
            "inline-flex items-center justify-center rounded-full px-1.5 text-[10px] min-w-[16px]",
            isOverLimit
              ? "bg-gx-error/10 text-gx-error font-medium"
              : "text-gx-muted",
          )}
        >
          {cards.length}
          {column.wipLimit != null && (
            <span className="text-gx-muted">/{column.wipLimit}</span>
          )}
        </span>
        <div className="flex-1" />
        {/* Hover-reveal actions: add card + collapse toggle */}
        <div
          className={clsx(
            "flex items-center gap-1 transition-opacity duration-100",
            isHeaderHovered ? "opacity-100" : "opacity-0",
          )}
        >
          {onCreateCard && (
            <button
              type="button"
              className="btn-icon h-5 w-5"
              onClick={() => setIsAdding(true)}
              title="Add card"
              aria-label="Add card to column"
            >
              <Plus size={12} />
            </button>
          )}
          <button
            type="button"
            className="btn-icon h-5 w-5"
            onClick={() => onToggleCollapse(column.id)}
            title="Collapse column"
            aria-label="Collapse column"
          >
            <ChevronRight size={12} />
          </button>
        </div>
      </div>

      {/* Card list */}
      <SortableContext items={cardIds} strategy={verticalListSortingStrategy}>
        {cards.length > VIRTUALIZATION_THRESHOLD ? (
          <VirtualizedCardList
            cards={cards}
            densityMode={densityMode}
            selectedCardIds={selectedCardIds}
            searchHighlight={searchHighlight}
            onCardClick={onCardClick}
            onCardContextMenu={onCardContextMenu}
            onCardTitleEdit={onCardTitleEdit}
            onRelationClick={handleRelationClick}
            onRunAgent={onRunAgent}
          />
        ) : (
          <div className="flex-1 overflow-y-auto p-2 space-y-0">
            {/* Inline card creation form */}
            {isAdding && (
              <div className="rounded border border-gx-accent/60 bg-gx-surface p-2 mb-1.5">
                <input
                  type="text"
                  className="input-field w-full text-xs mb-1.5"
                  placeholder="Card title…"
                  value={newTitle}
                  onChange={(e) => setNewTitle(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") handleSubmitCard();
                    if (e.key === "Escape") { setIsAdding(false); setNewTitle(""); setNewDescription(""); }
                  }}
                  autoFocus
                />
                <textarea
                  className="input-field w-full text-xs mb-1.5 resize-none"
                  placeholder="Description (what should the agent do?)…"
                  rows={3}
                  value={newDescription}
                  onChange={(e) => setNewDescription(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Escape") { setIsAdding(false); setNewTitle(""); setNewDescription(""); }
                  }}
                />
                <div className="flex gap-1.5">
                  <button
                    type="button"
                    className="btn-primary text-[10px] px-2 py-0.5"
                    onClick={handleSubmitCard}
                    disabled={!newTitle.trim()}
                  >
                    Add
                  </button>
                  <button
                    type="button"
                    className="btn text-[10px] px-2 py-0.5"
                    onClick={() => { setIsAdding(false); setNewTitle(""); setNewDescription(""); }}
                  >
                    Cancel
                  </button>
                </div>
              </div>
            )}
            {cards.map((card, index) => renderCard(card, index))}
          </div>
        )}
      </SortableContext>
    </div>
  );
}

/** Virtualized card list using react-window for columns with >50 cards */
function VirtualizedCardList({
  cards,
  densityMode,
  selectedCardIds,
  searchHighlight,
  onCardClick,
  onCardContextMenu,
  onCardTitleEdit,
  onRelationClick,
  onRunAgent,
}: {
  cards: KanbanCardInfo[];
  densityMode: DensityMode;
  selectedCardIds: Set<string>;
  searchHighlight?: string;
  onCardClick: (card: KanbanCardInfo, e: React.MouseEvent) => void;
  onCardContextMenu: (card: KanbanCardInfo, e: React.MouseEvent) => void;
  onCardTitleEdit: (cardId: string, newTitle: string) => void;
  onRelationClick: (cardId: string) => void;
  onRunAgent?: (card: KanbanCardInfo) => void;
}) {
  const Row = useCallback(
    ({ index, style }: { index: number; style: React.CSSProperties }) => {
      const card = cards[index];
      return (
        <div style={{ ...style, paddingLeft: 8, paddingRight: 8, paddingTop: 4 }}>
          <Card
            card={card}
            densityMode={densityMode}
            isSelected={selectedCardIds.has(card.id)}
            isEditing={false}
            searchHighlight={searchHighlight}
            onClick={(e) => onCardClick(card, e)}
            onContextMenu={(e) => onCardContextMenu(card, e)}
            onTitleEdit={onCardTitleEdit}
            onRelationClick={onRelationClick}
            onRunAgent={onRunAgent}
          />
        </div>
      );
    },
    [cards, densityMode, selectedCardIds, searchHighlight, onCardClick, onCardContextMenu, onCardTitleEdit, onRelationClick, onRunAgent],
  );

  return (
    <div className="flex-1 min-h-0">
      <List
        height={600}
        itemCount={cards.length}
        itemSize={CARD_HEIGHT}
        width="100%"
      >
        {Row}
      </List>
    </div>
  );
}
