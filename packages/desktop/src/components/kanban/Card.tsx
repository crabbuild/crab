import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  Bot,
  ChevronsUp,
  ChevronUp,
  Minus,
  ChevronDown,
  Circle,
  MoreHorizontal,
  Lock,
  ArrowRight,
} from "lucide-react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import type { KanbanCardInfo, KanbanCardPriority, KanbanPipelineState, DensityMode } from "../../ipc";
import { findMatchRanges } from "../../lib/kanban/search-engine";

export interface CardProps {
  card: KanbanCardInfo;
  densityMode: DensityMode;
  isSelected: boolean;
  isEditing: boolean;
  searchHighlight?: string;
  onClick: (e: React.MouseEvent) => void;
  onContextMenu: (e: React.MouseEvent) => void;
  onTitleEdit: (cardId: string, newTitle: string) => void;
  onRelationClick: (cardId: string) => void;
  onRunAgent?: (card: KanbanCardInfo) => void;
}

const PIPELINE_LABELS: Record<KanbanPipelineState, string> = {
  idle: "Idle",
  agent_running: "Agent",
  pr_created: "PR",
  ci_pending: "CI…",
  ci_passed: "CI ✓",
  ci_failed: "CI ✗",
  review_pending: "Review",
  review_approved: "Approved",
  review_changes_requested: "Changes",
  merging: "Merging",
  done: "Done",
  blocked: "Blocked",
};

/**
 * Renders the priority icon with the appropriate shape and color.
 */
function PriorityIcon({ priority }: { priority: KanbanCardPriority }) {
  switch (priority) {
    case "critical":
      return <ChevronsUp size={12} className="shrink-0 text-gx-error" />;
    case "high":
      return <ChevronUp size={12} className="shrink-0 text-gx-warn" />;
    case "medium":
      return <Minus size={12} className="shrink-0 text-yellow-400" />;
    case "low":
      return <ChevronDown size={12} className="shrink-0 text-gx-info" />;
    case "none":
      return <Circle size={10} className="shrink-0 text-gx-muted" />;
  }
}

/**
 * Highlights matching substrings in text with bg-gx-accent/20.
 */
function HighlightedText({ text, query }: { text: string; query?: string }) {
  if (!query || query.trim().length === 0) {
    return <>{text}</>;
  }

  const ranges = findMatchRanges(text, query);
  if (ranges.length === 0) {
    return <>{text}</>;
  }

  const parts: React.ReactNode[] = [];
  let lastEnd = 0;

  for (const range of ranges) {
    if (range.start > lastEnd) {
      parts.push(text.slice(lastEnd, range.start));
    }
    parts.push(
      <mark key={range.start} className="bg-gx-accent/20 text-inherit rounded-sm">
        {text.slice(range.start, range.end)}
      </mark>,
    );
    lastEnd = range.end;
  }

  if (lastEnd < text.length) {
    parts.push(text.slice(lastEnd));
  }

  return <>{parts}</>;
}

/**
 * Inline title editor with auto-select, Enter to save, Escape to cancel,
 * and empty rejection with red border.
 */
function InlineEditor({
  initialTitle,
  onSave,
  onCancel,
}: {
  initialTitle: string;
  onSave: (newTitle: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(initialTitle);
  const [hasError, setHasError] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (inputRef.current) {
      inputRef.current.focus();
      inputRef.current.select();
    }
  }, []);

  const handleSubmit = useCallback(() => {
    const trimmed = value.trim();
    if (trimmed.length === 0) {
      setHasError(true);
      return;
    }
    setHasError(false);
    onSave(trimmed);
  }, [value, onSave]);

  return (
    <input
      ref={inputRef}
      type="text"
      className={clsx(
        "input-field w-full text-xs px-1 py-0.5",
        hasError && "border-gx-error ring-1 ring-gx-error",
      )}
      value={value}
      onChange={(e) => {
        setValue(e.target.value);
        if (hasError && e.target.value.trim().length > 0) {
          setHasError(false);
        }
      }}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") {
          e.preventDefault();
          handleSubmit();
        } else if (e.key === "Escape") {
          e.preventDefault();
          onCancel();
        }
      }}
      onClick={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
    />
  );
}

/**
 * Sub-issue progress indicator showing completed/total children.
 */
function SubIssueProgress({ card, allCards }: { card: KanbanCardInfo; allCards?: Map<string, KanbanCardInfo> }) {
  const childIds = card.childCardIds;
  if (!childIds || childIds.length === 0) return null;

  let completedCount = 0;
  if (allCards) {
    for (const childId of childIds) {
      const child = allCards.get(childId);
      if (child && child.pipelineState === "done") {
        completedCount++;
      }
    }
  }

  return (
    <span className="text-[10px] text-gx-muted" title={`${completedCount}/${childIds.length} sub-tasks completed`}>
      {completedCount}/{childIds.length}
    </span>
  );
}

/**
 * Relation indicators showing blocked-by (lock) and blocks (arrow) counts.
 */
function RelationIndicators({
  card,
  onRelationClick,
}: {
  card: KanbanCardInfo;
  onRelationClick: (cardId: string) => void;
}) {
  const relations = card.relations;
  if (!relations || relations.length === 0) return null;

  const blockedByCount = relations.filter((r) => r.type === "blocked-by").length;
  const blocksCount = relations.filter((r) => r.type === "blocks").length;

  if (blockedByCount === 0 && blocksCount === 0) return null;

  return (
    <span className="flex items-center gap-1">
      {blockedByCount > 0 && (
        <button
          type="button"
          className="flex items-center gap-0.5 text-gx-error hover:text-gx-error/80"
          title={`Blocked by ${blockedByCount} card${blockedByCount > 1 ? "s" : ""}`}
          onClick={(e) => {
            e.stopPropagation();
            onRelationClick(card.id);
          }}
        >
          <Lock size={10} />
          <span className="text-[9px]">{blockedByCount}</span>
        </button>
      )}
      {blocksCount > 0 && (
        <button
          type="button"
          className="flex items-center gap-0.5 text-gx-warn hover:text-gx-warn/80"
          title={`Blocks ${blocksCount} card${blocksCount > 1 ? "s" : ""}`}
          onClick={(e) => {
            e.stopPropagation();
            onRelationClick(card.id);
          }}
        >
          <ArrowRight size={10} />
          <span className="text-[9px]">{blocksCount}</span>
        </button>
      )}
    </span>
  );
}

/**
 * Density-specific configuration for padding, font size, and line clamping.
 */
const DENSITY_CONFIG: Record<DensityMode, { padding: string; fontSize: string; lineClamp: string; maxHeight?: string }> = {
  compact: {
    padding: "py-1.5 px-2",
    fontSize: "text-xs",
    lineClamp: "line-clamp-1",
    maxHeight: "max-h-10",
  },
  comfortable: {
    padding: "py-2 px-2",
    fontSize: "text-xs",
    lineClamp: "line-clamp-2",
  },
  spacious: {
    padding: "py-3 px-2",
    fontSize: "text-sm",
    lineClamp: "line-clamp-3",
  },
};

/**
 * Kanban card component. Draggable via @dnd-kit useSortable.
 * Supports density modes, priority icons, selection state, hover actions,
 * inline title editing, sub-issue progress, relation indicators, and search highlighting.
 */
export function Card({
  card,
  densityMode,
  isSelected,
  isEditing,
  searchHighlight,
  onClick,
  onContextMenu,
  onTitleEdit,
  onRelationClick,
  onRunAgent,
}: CardProps) {
  const isWorkflowManaged = card.workflowProjection?.managed === true;
  const symphonyManaged = isActiveSymphonyRuntime(card);
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: card.id, disabled: isEditing || isWorkflowManaged || symphonyManaged });

  const [isHovered, setIsHovered] = useState(false);

  const density = DENSITY_CONFIG[densityMode];

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.4 : undefined,
    zIndex: isDragging ? 50 : undefined,
  };

  const handleTitleSave = useCallback(
    (newTitle: string) => {
      onTitleEdit(card.id, newTitle);
    },
    [card.id, onTitleEdit],
  );

  const handleTitleCancel = useCallback(() => {
    onTitleEdit(card.id, card.title);
  }, [card.id, card.title, onTitleEdit]);

  const cardIdPrefix = card.id.slice(0, 6);

  return (
    <div
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...(isEditing || isWorkflowManaged || symphonyManaged ? {} : listeners)}
      role="button"
      tabIndex={0}
      onClick={onClick}
      onContextMenu={onContextMenu}
      onMouseEnter={() => setIsHovered(true)}
      onMouseLeave={() => setIsHovered(false)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick(e as unknown as React.MouseEvent);
        }
      }}
      className={clsx(
        "relative rounded border bg-gx-surface cursor-grab select-none",
        "transition-colors duration-100",
        density.padding,
        (isWorkflowManaged || symphonyManaged) && "cursor-default",
        isSelected
          ? "border-gx-border border-l-2 border-l-gx-accent"
          : "border-gx-border",
        isHovered && !isDragging && "bg-gx-surface-hover",
        isDragging && "shadow-lg",
      )}
    >
      {/* Hover action icons — top-right corner */}
      <div
        className={clsx(
          "absolute top-1 right-1 flex items-center gap-0.5 transition-opacity duration-100",
          isHovered && !isEditing ? "opacity-100" : "opacity-0 pointer-events-none",
        )}
      >
        <button
          type="button"
          className="btn-icon h-4 w-4 text-gx-muted hover:text-gx-text"
          title="More actions"
          aria-label="Open context menu"
          onClick={(e) => {
            e.stopPropagation();
            onContextMenu(e);
          }}
          onMouseDown={(e) => e.stopPropagation()}
        >
          <MoreHorizontal size={12} />
        </button>
        {onRunAgent && card.pipelineState === "idle" && !isWorkflowManaged && !symphonyManaged && (
          <button
            type="button"
            className="btn-icon h-4 w-4 text-gx-muted hover:text-gx-accent"
            title="Run agent on this card"
            aria-label="Run agent"
            onClick={(e) => {
              e.stopPropagation();
              onRunAgent(card);
            }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <Bot size={12} />
          </button>
        )}
      </div>

      {/* Parent breadcrumb */}
      {card.parentCardId && (
        <div className="text-[9px] text-gx-muted mb-0.5 truncate">
          ↑ {card.parentCardId.slice(0, 6)}
        </div>
      )}

      {/* Header: card ID prefix + priority icon + title */}
      <div className="flex items-start gap-1.5">
        <span className="text-[9px] text-gx-muted shrink-0 mt-0.5 font-mono">
          {cardIdPrefix}
        </span>
        <span className="mt-0.5">
          <PriorityIcon priority={card.priority} />
        </span>
        <div className={clsx("min-w-0 flex-1", density.fontSize, "text-gx-text leading-tight")}>
          {isEditing ? (
            <InlineEditor
              initialTitle={card.title}
              onSave={handleTitleSave}
              onCancel={handleTitleCancel}
            />
          ) : (
            <span className={clsx("break-words", density.lineClamp)}>
              <HighlightedText text={card.title} query={searchHighlight} />
            </span>
          )}
        </div>
      </div>

      {isWorkflowManaged && (
        <div className="mt-1 flex items-center gap-1 text-[10px] text-gx-accent">
          <Lock size={10} />
          <span>Workflow managed</span>
        </div>
      )}

      {card.symphonyRuntime && (
        <div
          className={clsx(
            "mt-1 flex min-w-0 items-center gap-1 text-[10px]",
            card.symphonyRuntime.status === "blocked" ? "text-gx-error" : "text-gx-accent",
          )}
          title={card.symphonyRuntime.lastMessage ?? card.symphonyRuntime.identifier}
        >
          <Lock size={10} className="shrink-0" />
          <span className="truncate">
            Symphony {card.symphonyRuntime.status}
            {card.symphonyRuntime.retryDueAtMs
              ? ` - retry ${formatShortRelative(card.symphonyRuntime.retryDueAtMs)}`
              : ""}
          </span>
        </div>
      )}

      {/* Labels (max 3) */}
      {card.labels.length > 0 && densityMode !== "compact" && (
        <div className="mt-1 flex flex-wrap gap-1">
          {card.labels.slice(0, 3).map((label) => (
            <span key={label} className="chip text-[10px] px-1.5 py-0">
              {label}
            </span>
          ))}
          {card.labels.length > 3 && (
            <span className="text-[9px] text-gx-muted">+{card.labels.length - 3}</span>
          )}
        </div>
      )}

      {/* Footer: pipeline badge + sub-issues + relations + assignee */}
      <div className="mt-1 flex items-center gap-1.5">
        {card.pipelineState !== "idle" && (
          <span
            className={clsx(
              "chip text-[10px] px-1.5 py-0",
              card.pipelineState === "blocked" && "border-gx-error/50 text-gx-error",
              card.pipelineState === "ci_failed" && "border-gx-error/50 text-gx-error",
              card.pipelineState === "ci_passed" && "border-gx-success/50 text-gx-success",
              card.pipelineState === "done" && "border-gx-success/50 text-gx-success",
            )}
          >
            {PIPELINE_LABELS[card.pipelineState]}
          </span>
        )}
        {card.iterationCount > 0 && (
          <span className="text-[10px] text-gx-muted" title="Iteration count">
            ×{card.iterationCount}
          </span>
        )}
        <SubIssueProgress card={card} />
        <RelationIndicators card={card} onRelationClick={onRelationClick} />
        <div className="flex-1" />
        {card.assignee && (
          card.assigneeAvatarUrl ? (
            <img
              src={card.assigneeAvatarUrl}
              alt={card.assignee}
              title={card.assignee}
              className="h-4 w-4 rounded-full"
            />
          ) : (
            <span
              className="flex h-4 w-4 items-center justify-center rounded-full bg-gx-accent/20 text-[9px] text-gx-accent"
              title={card.assignee}
            >
              {card.assignee.charAt(0).toUpperCase()}
            </span>
          )
        )}
      </div>
    </div>
  );
}

function isActiveSymphonyRuntime(card: KanbanCardInfo): boolean {
  const status = card.symphonyRuntime?.status;
  return status === "running" || status === "retrying" || status === "blocked" || status === "claimed";
}

function formatShortRelative(timestamp: number): string {
  const diffMs = timestamp - Date.now();
  if (diffMs <= 0) return "now";
  if (diffMs < 60_000) return `${Math.ceil(diffMs / 1_000)}s`;
  if (diffMs < 3_600_000) return `${Math.ceil(diffMs / 60_000)}m`;
  return `${Math.ceil(diffMs / 3_600_000)}h`;
}
