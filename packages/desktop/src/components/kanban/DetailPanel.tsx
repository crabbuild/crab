import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { marked } from "marked";
import {
  X,
  ChevronsUp,
  ChevronUp,
  Minus,
  ChevronDown,
  Circle,
  Link2,
  GitPullRequest,
  ExternalLink,
  Plus,
  Play,
  Search,
  ArrowRight,
  Lock,
  Users,
  RotateCcw,
  Ban,
  UnlockKeyhole,
  Terminal,
  MessageSquare,
} from "lucide-react";
import type {
  KanbanBoardConfig,
  KanbanCardInfo,
  KanbanCardPriority,
  KanbanPipelineState,
  RelationType,
  CardRelation,
} from "../../ipc";

// ── Props ───────────────────────────────────────────────────────────

export interface DetailPanelProps {
  card: KanbanCardInfo;
  board?: KanbanBoardConfig | null;
  isOpen: boolean;
  onClose: () => void;
  onUpdateCard: (cardId: string, updates: Partial<KanbanCardInfo>) => void;
  onRunAgent?: () => void;
  onNavigateToCard: (cardId: string) => void;
  onCreateRelation: (sourceId: string, targetId: string, type: RelationType) => void;
  onDeleteRelation: (sourceId: string, targetId: string, type: RelationType) => void;
  /** All cards on the board, used for relation search and display. */
  allCards?: KanbanCardInfo[];
  onRetrySymphonyIssue?: (issueId: string) => void;
  onCancelSymphonyIssue?: (issueId: string) => void;
  onReleaseSymphonyIssue?: (issueId: string) => void;
}

// ── Constants ───────────────────────────────────────────────────────

const PIPELINE_STATE_LABELS: Record<KanbanPipelineState, string> = {
  idle: "Idle",
  agent_running: "Agent Running",
  pr_created: "PR Created",
  ci_pending: "CI Pending",
  ci_passed: "CI Passed",
  ci_failed: "CI Failed",
  review_pending: "Review Pending",
  review_approved: "Review Approved",
  review_changes_requested: "Changes Requested",
  merging: "Merging",
  done: "Done",
  blocked: "Blocked",
};

const PRIORITY_LABELS: Record<KanbanCardPriority, string> = {
  critical: "Critical",
  high: "High",
  medium: "Medium",
  low: "Low",
  none: "None",
};

const RELATION_TYPE_LABELS: Record<RelationType, string> = {
  parent: "Parent",
  child: "Children",
  blocks: "Blocks",
  "blocked-by": "Blocked By",
  "relates-to": "Relates To",
};

const RELATION_TYPES: RelationType[] = ["parent", "child", "blocks", "blocked-by", "relates-to"];

// ── Helpers ─────────────────────────────────────────────────────────

function formatTimestamp(ts: number): string {
  const date = new Date(ts);
  const now = new Date();
  const diffMs = now.getTime() - ts;

  if (diffMs < 60_000) return "just now";
  if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
  if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;

  if (date.toDateString() === now.toDateString()) {
    return date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  }

  return date.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatDate(ts: number): string {
  return new Date(ts).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatCompactNumber(value: number): string {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

function formatFutureTime(timestamp: number): string {
  const diffMs = timestamp - Date.now();
  if (diffMs <= 0) return "now";
  if (diffMs < 60_000) return `${Math.ceil(diffMs / 1_000)}s`;
  if (diffMs < 3_600_000) return `${Math.ceil(diffMs / 60_000)}m`;
  return `${Math.ceil(diffMs / 3_600_000)}h`;
}

function renderMarkdown(text: string): string {
  return marked.parse(text, { async: false, breaks: true, gfm: true }) as string;
}

// ── Sub-components ──────────────────────────────────────────────────

function PriorityIcon({ priority }: { priority: KanbanCardPriority }) {
  const classes = clsx("shrink-0", {
    "text-gx-error": priority === "critical",
    "text-orange-400": priority === "high",
    "text-yellow-400": priority === "medium",
    "text-blue-400": priority === "low",
    "text-gx-muted": priority === "none",
  });

  switch (priority) {
    case "critical":
      return <ChevronsUp size={14} className={classes} />;
    case "high":
      return <ChevronUp size={14} className={classes} />;
    case "medium":
      return <Minus size={14} className={classes} />;
    case "low":
      return <ChevronDown size={14} className={classes} />;
    case "none":
      return <Circle size={14} className={classes} />;
  }
}

// ── Inline Title Editor ─────────────────────────────────────────────

function InlineTitleEditor({
  value,
  onSave,
  onCancel,
}: {
  value: string;
  onSave: (newValue: string) => void;
  onCancel: () => void;
}) {
  const [text, setText] = useState(value);
  const [hasError, setHasError] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.select();
  }, []);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const trimmed = text.trim();
      if (!trimmed) {
        setHasError(true);
        return;
      }
      onSave(trimmed);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onCancel();
    }
  };

  return (
    <input
      ref={inputRef}
      type="text"
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        setHasError(false);
      }}
      onKeyDown={handleKeyDown}
      onBlur={onCancel}
      className={clsx(
        "w-full rounded px-2 py-1 text-sm font-medium text-gx-text bg-gx-bg-sub border outline-none",
        hasError ? "border-gx-error" : "border-gx-border focus:border-gx-accent",
      )}
    />
  );
}

// ── Inline Description Editor ───────────────────────────────────────

function InlineDescriptionEditor({
  value,
  onSave,
  onCancel,
}: {
  value: string;
  onSave: (newValue: string) => void;
  onCancel: () => void;
}) {
  const [text, setText] = useState(value);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    textareaRef.current?.select();
  }, []);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      onSave(text);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onCancel();
    }
  };

  return (
    <div className="space-y-1">
      <textarea
        ref={textareaRef}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={handleKeyDown}
        onBlur={onCancel}
        rows={6}
        className="w-full rounded px-2 py-1.5 text-xs text-gx-text bg-gx-bg-sub border border-gx-border outline-none focus:border-gx-accent resize-y font-mono leading-relaxed"
        placeholder="Write description in markdown..."
      />
      <p className="text-[10px] text-gx-muted">
        Cmd+Enter to save · Escape to cancel
      </p>
    </div>
  );
}

// ── Relation Search ─────────────────────────────────────────────────

function RelationSearch({
  sourceId,
  allCards,
  existingRelationIds,
  onCreateRelation,
  onClose,
}: {
  sourceId: string;
  allCards: KanbanCardInfo[];
  existingRelationIds: Set<string>;
  onCreateRelation: (targetId: string, type: RelationType) => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [selectedType, setSelectedType] = useState<RelationType>("relates-to");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const results = useMemo(() => {
    if (!query.trim()) return [];
    const q = query.toLowerCase();
    return allCards
      .filter(
        (c) =>
          c.id !== sourceId &&
          !existingRelationIds.has(c.id) &&
          (c.title.toLowerCase().includes(q) || c.id.toLowerCase().includes(q)),
      )
      .slice(0, 5);
  }, [query, allCards, sourceId, existingRelationIds]);

  return (
    <div className="space-y-2 border border-gx-border rounded p-2 bg-gx-bg-sub">
      <div className="flex items-center gap-2">
        <select
          value={selectedType}
          onChange={(e) => setSelectedType(e.target.value as RelationType)}
          className="text-[10px] rounded border border-gx-border bg-gx-bg-sub text-gx-text px-1 py-1 outline-none"
        >
          {RELATION_TYPES.map((t) => (
            <option key={t} value={t}>
              {RELATION_TYPE_LABELS[t]}
            </option>
          ))}
        </select>
        <div className="flex-1 relative flex items-center my-1">
          <Search size={10} className="absolute left-1.5 top-1/2 -translate-y-1/2 text-gx-muted" />
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                onClose();
              }
            }}
            placeholder="Search by title or ID..."
            className="w-full rounded border border-gx-border bg-gx-bg-sub text-[10px] text-gx-text pl-5 pr-1.5 py-0.5 outline-none focus:border-gx-accent"
          />
        </div>
        <button
          onClick={onClose}
          className="btn-icon text-gx-muted hover:text-gx-text"
          aria-label="Close search"
        >
          <X size={12} />
        </button>
      </div>
      {results.length > 0 && (
        <div className="space-y-0.5">
          {results.map((card) => (
            <button
              key={card.id}
              type="button"
              onClick={() => onCreateRelation(card.id, selectedType)}
              className="flex w-full items-center gap-2 rounded px-1.5 py-1 text-left hover:bg-gx-surface-hover transition"
            >
              <span className="text-[10px] text-gx-muted font-mono">
                {card.id.slice(0, 6)}
              </span>
              <span className="text-[10px] text-gx-text truncate flex-1">
                {card.title}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Main Component ──────────────────────────────────────────────────

export function DetailPanel({
  card,
  board,
  isOpen,
  onClose,
  onUpdateCard,
  onRunAgent,
  onNavigateToCard,
  onCreateRelation,
  onDeleteRelation,
  allCards = [],
  onRetrySymphonyIssue,
  onCancelSymphonyIssue,
  onReleaseSymphonyIssue,
}: DetailPanelProps) {
  const [editingTitle, setEditingTitle] = useState(false);
  const [editingDescription, setEditingDescription] = useState(false);
  const [showRelationSearch, setShowRelationSearch] = useState(false);
  const symphonyRuntime = card.symphonyRuntime;
  const symphonyAttempt = symphonyRuntime?.retryAttempt ?? symphonyRuntime?.attempt ?? null;
  const symphonyTokens = symphonyRuntime?.tokens ?? symphonyRuntime?.tokenTotals;

  const handleClose = useCallback(() => {
    onClose();
  }, [onClose]);

  // Reset state when card changes
  useEffect(() => {
    setEditingTitle(false);
    setEditingDescription(false);
    setShowRelationSearch(false);
  }, [card.id]);

  // Close on Escape
  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !editingTitle && !editingDescription) {
        handleClose();
      }
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [isOpen, editingTitle, editingDescription, handleClose]);

  // Title save
  const handleTitleSave = useCallback(
    (newTitle: string) => {
      setEditingTitle(false);
      if (newTitle !== card.title) {
        onUpdateCard(card.id, { title: newTitle });
      }
    },
    [card.id, card.title, onUpdateCard],
  );

  // Description save
  const handleDescriptionSave = useCallback(
    (newDescription: string) => {
      setEditingDescription(false);
      if (newDescription !== card.description) {
        onUpdateCard(card.id, { description: newDescription });
      }
    },
    [card.id, card.description, onUpdateCard],
  );

  const updateLabels = useCallback(
    (value: string) => {
      const labels = value
        .split(",")
        .map((item) => item.trim())
        .filter(Boolean);
      if (labels.join(",") !== card.labels.join(",")) {
        onUpdateCard(card.id, { labels });
      }
    },
    [card.id, card.labels, onUpdateCard],
  );

  const updateCustomField = useCallback(
    (fieldId: string, value: string) => {
      const nextCustomFields = { ...(card.customFields ?? {}), [fieldId]: value };
      onUpdateCard(card.id, { customFields: nextCustomFields });
    },
    [card.customFields, card.id, onUpdateCard],
  );

  const updateRunDefault = useCallback(
    (field: string, value: string) => {
      onUpdateCard(card.id, {
        runDefaults: {
          ...(card.runDefaults ?? {}),
          [field]: value.trim() || undefined,
        },
      });
    },
    [card.id, card.runDefaults, onUpdateCard],
  );

  const openLinkedChat = useCallback(() => {
    if (!card.linkedTaskId || !board?.repoPath) return;
    localStorage.setItem(
      "crab.orchestrator.pendingOpenTask",
      JSON.stringify({ taskId: card.linkedTaskId, repoPath: board.repoPath }),
    );
    window.dispatchEvent(
      new CustomEvent("crab:open-agent-task", {
        detail: { taskId: card.linkedTaskId, repoPath: board.repoPath },
      }),
    );
  }, [board?.repoPath, card.linkedTaskId]);

  // Group relations by type
  const groupedRelations = useMemo(() => {
    const groups: Record<RelationType, CardRelation[]> = {
      parent: [],
      child: [],
      blocks: [],
      "blocked-by": [],
      "relates-to": [],
    };

    // Add parent as a relation if present
    if (card.parentCardId) {
      groups.parent.push({ targetCardId: card.parentCardId, type: "parent" });
    }

    // Add children
    if (card.childCardIds) {
      for (const childId of card.childCardIds) {
        groups.child.push({ targetCardId: childId, type: "child" });
      }
    }

    // Add explicit relations
    if (card.relations) {
      for (const rel of card.relations) {
        if (groups[rel.type]) {
          groups[rel.type].push(rel);
        }
      }
    }

    return groups;
  }, [card.parentCardId, card.childCardIds, card.relations]);

  // Set of existing relation target IDs (for search exclusion)
  const existingRelationIds = useMemo(() => {
    const ids = new Set<string>();
    for (const rels of Object.values(groupedRelations)) {
      for (const rel of rels) {
        ids.add(rel.targetCardId);
      }
    }
    return ids;
  }, [groupedRelations]);

  // Card lookup for relation display
  const cardLookup = useMemo(() => {
    const map = new Map<string, KanbanCardInfo>();
    for (const c of allCards) {
      map.set(c.id, c);
    }
    return map;
  }, [allCards]);

  // Has any relations
  const hasRelations = Object.values(groupedRelations).some((rels) => rels.length > 0);

  if (!isOpen) return null;

  return (
    <div
      className="flex h-full w-[clamp(340px,30vw,420px)] shrink-0 flex-col overflow-hidden border-l border-gx-border bg-gx-surface"
      role="dialog"
      aria-label="Task detail panel"
    >
      {/* Header */}
      <div className="flex items-center justify-between border-b border-gx-border px-4 py-2 shrink-0 my-1">
        <div className="flex items-center gap-2 min-w-0">
          <span className="text-[10px] font-mono text-gx-muted">
            {card.id.slice(0, 6)}
          </span>
          {card.externalId && (
            <span className="text-[10px] text-gx-muted truncate">
              {card.externalId}
            </span>
          )}
        </div>
        <div className="flex items-center gap-1">
          {card.linkedTaskId && (
            <button
              type="button"
              onClick={openLinkedChat}
              className="btn-icon text-gx-muted hover:text-gx-accent"
              aria-label="Open linked chat"
              title="Open linked chat"
            >
              <MessageSquare size={14} />
            </button>
          )}
          {onRunAgent && card.pipelineState !== "agent_running" && (
            <button
              type="button"
              onClick={onRunAgent}
              className="btn-icon text-gx-muted hover:text-gx-accent"
              aria-label="Run task"
              title="Run task"
            >
              <Play size={14} />
            </button>
          )}
        <button
          onClick={handleClose}
          className="btn-icon text-gx-muted hover:text-gx-text shrink-0"
          aria-label="Close detail panel"
        >
          <X size={16} />
        </button>
        </div>
      </div>

      {/* Scrollable content */}
      <div className="flex-1 overflow-y-auto px-4 py-3 space-y-5">
        {/* Title */}
        <section>
          {editingTitle ? (
            <InlineTitleEditor
              value={card.title}
              onSave={handleTitleSave}
              onCancel={() => setEditingTitle(false)}
            />
          ) : (
            <h2
              className="text-sm font-medium text-gx-text leading-snug cursor-pointer hover:bg-gx-surface-hover rounded px-1 -mx-1 py-0.5 transition"
              onClick={() => setEditingTitle(true)}
              title="Click to edit title"
            >
              {card.title}
            </h2>
          )}
        </section>

        {/* Description */}
        <section>
          <h3 className="text-[10px] font-medium text-gx-muted uppercase tracking-wide mb-1.5">
            Description
          </h3>
          {editingDescription ? (
            <InlineDescriptionEditor
              value={card.description}
              onSave={handleDescriptionSave}
              onCancel={() => setEditingDescription(false)}
            />
          ) : card.description ? (
            <div
              className="prose-detail text-xs text-gx-text leading-relaxed cursor-pointer hover:bg-gx-surface-hover rounded px-1 -mx-1 py-1 transition"
              onClick={() => setEditingDescription(true)}
              title="Click to edit description"
              dangerouslySetInnerHTML={{ __html: renderMarkdown(card.description) }}
            />
          ) : (
            <p
              className="text-xs text-gx-muted italic cursor-pointer hover:bg-gx-surface-hover rounded px-1 -mx-1 py-1 transition"
              onClick={() => setEditingDescription(true)}
            >
              Add a description...
            </p>
          )}
        </section>

        {/* Field Grid */}
        <section className="space-y-2.5">
          {/* Priority */}
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">Priority</span>
            <div className="flex items-center gap-1.5">
              <PriorityIcon priority={card.priority} />
              <select
                className="rounded border border-transparent bg-transparent px-1 py-0.5 text-xs text-gx-text outline-none hover:border-gx-border focus:border-gx-accent"
                value={card.priority}
                onChange={(event) => onUpdateCard(card.id, { priority: event.target.value as KanbanCardPriority })}
              >
                {Object.entries(PRIORITY_LABELS).map(([value, label]) => (
                  <option key={value} value={value}>{label}</option>
                ))}
              </select>
            </div>
          </div>

          {/* Board column */}
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">State</span>
            <select
              className="rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs text-gx-text outline-none focus:border-gx-accent"
              value={card.columnId}
              onChange={(event) => onUpdateCard(card.id, { columnId: event.target.value })}
            >
              {(board?.columns ?? []).map((column) => (
                <option key={column.id} value={column.id}>{column.name}</option>
              ))}
            </select>
          </div>

          {/* Pipeline State */}
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">Pipeline</span>
            <span
              className={clsx(
                "chip px-2 py-0.5 text-[10px]",
                card.pipelineState === "blocked" && "border-gx-error/50 text-gx-error",
                card.pipelineState === "ci_failed" && "border-gx-error/50 text-gx-error",
                card.pipelineState === "done" && "border-gx-success/50 text-gx-success",
                card.pipelineState === "agent_running" && "border-gx-thinking/50 text-gx-thinking",
              )}
            >
              {PIPELINE_STATE_LABELS[card.pipelineState]}
            </span>
          </div>

          {symphonyRuntime && (
            <div className="rounded border border-gx-border bg-gx-surface px-2.5 py-2">
              <div className="flex items-center justify-between gap-2">
                <div className="min-w-0">
                  <div className="flex items-center gap-1.5">
                    <Terminal size={12} className="shrink-0 text-gx-accent" />
                    <span className="truncate text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                      Symphony
                    </span>
                    <span
                      className={clsx(
                        "chip px-1.5 py-0 text-[10px]",
                        symphonyRuntime.status === "running" && "border-gx-thinking/50 text-gx-thinking",
                        symphonyRuntime.status === "retrying" && "border-gx-warn/50 text-gx-warn",
                        symphonyRuntime.status === "blocked" && "border-gx-error/50 text-gx-error",
                      )}
                    >
                      {symphonyRuntime.status}
                    </span>
                  </div>
                  <div className="mt-1 min-w-0 text-[10px] text-gx-muted">
                    <span className="font-mono">{symphonyRuntime.identifier}</span>
                    {symphonyAttempt ? (
                      <span className="ml-1.5">attempt {symphonyAttempt}</span>
                    ) : null}
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  {symphonyRuntime.status === "retrying" && onRetrySymphonyIssue && (
                    <button
                      type="button"
                      className="btn-icon h-6 w-6"
                      title="Retry now"
                      aria-label="Retry Symphony issue"
                      onClick={() => onRetrySymphonyIssue(symphonyRuntime.issueId)}
                    >
                      <RotateCcw size={12} />
                    </button>
                  )}
                  {symphonyRuntime.status === "running" && onCancelSymphonyIssue && (
                    <button
                      type="button"
                      className="btn-icon h-6 w-6 text-gx-muted hover:text-gx-error"
                      title="Cancel run"
                      aria-label="Cancel Symphony run"
                      onClick={() => onCancelSymphonyIssue(symphonyRuntime.issueId)}
                    >
                      <Ban size={12} />
                    </button>
                  )}
                  {symphonyRuntime.status === "blocked" && onReleaseSymphonyIssue && (
                    <button
                      type="button"
                      className="btn-icon h-6 w-6"
                      title="Release blocked issue"
                      aria-label="Release blocked Symphony issue"
                      onClick={() => onReleaseSymphonyIssue(symphonyRuntime.issueId)}
                    >
                      <UnlockKeyhole size={12} />
                    </button>
                  )}
                </div>
              </div>

              <div className="mt-2 space-y-1 text-[10px]">
                {symphonyRuntime.workspacePath && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Workspace</span>
                    <span className="truncate font-mono text-gx-text" title={symphonyRuntime.workspacePath}>
                      {symphonyRuntime.workspacePath}
                    </span>
                  </div>
                )}
                {symphonyRuntime.sessionId && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Session</span>
                    <span className="truncate font-mono text-gx-text">{symphonyRuntime.sessionId}</span>
                  </div>
                )}
                {symphonyRuntime.retryDueAtMs && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Retry</span>
                    <span className="text-gx-text">{formatFutureTime(symphonyRuntime.retryDueAtMs)}</span>
                  </div>
                )}
                {symphonyRuntime.blockedReason && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Blocked</span>
                    <span className="min-w-0 break-words text-gx-error">{symphonyRuntime.blockedReason}</span>
                  </div>
                )}
                {symphonyRuntime.lastMessage && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Last</span>
                    <span className="min-w-0 break-words text-gx-text">{symphonyRuntime.lastMessage}</span>
                  </div>
                )}
                {symphonyTokens && symphonyTokens.totalTokens > 0 && (
                  <div className="flex min-w-0 gap-2">
                    <span className="w-16 shrink-0 text-gx-muted">Tokens</span>
                    <span className="text-gx-text">
                      {formatCompactNumber(symphonyTokens.totalTokens)} total
                    </span>
                  </div>
                )}
              </div>
            </div>
          )}

          {/* Assignee */}
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">Assignee</span>
            <input
              className="min-w-0 flex-1 rounded border border-transparent bg-transparent px-1 py-0.5 text-xs text-gx-text outline-none placeholder:text-gx-muted hover:border-gx-border focus:border-gx-accent"
              defaultValue={card.assignee ?? ""}
              placeholder="Unassigned"
              onBlur={(event) => {
                const assignee = event.target.value.trim();
                if (assignee !== (card.assignee ?? "")) {
                  onUpdateCard(card.id, { assignee: assignee || undefined });
                }
              }}
            />
          </div>

          {/* Labels */}
          <div className="flex items-start gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0 pt-0.5">Labels</span>
            <input
              className="min-w-0 flex-1 rounded border border-transparent bg-transparent px-1 py-0.5 text-xs text-gx-text outline-none placeholder:text-gx-muted hover:border-gx-border focus:border-gx-accent"
              defaultValue={card.labels.join(", ")}
              placeholder="No labels"
              onBlur={(event) => updateLabels(event.target.value)}
            />
          </div>

          {board?.fields?.filter((field) => field.taskScoped !== false).map((field) => (
            <div key={field.id} className="flex items-center gap-3">
              <span className="w-20 shrink-0 truncate text-[10px] text-gx-muted" title={field.name}>
                {field.name}
              </span>
              <input
                className="min-w-0 flex-1 rounded border border-transparent bg-transparent px-1 py-0.5 text-xs text-gx-text outline-none placeholder:text-gx-muted hover:border-gx-border focus:border-gx-accent"
                defaultValue={String(card.customFields?.[field.id] ?? "")}
                placeholder={field.type}
                onBlur={(event) => updateCustomField(field.id, event.target.value)}
              />
            </div>
          ))}

          <div className="pt-2">
            <h3 className="mb-2 text-[10px] font-medium uppercase tracking-wide text-gx-muted">
              Agent Defaults
            </h3>
            <div className="space-y-2">
              <div className="flex items-center gap-3">
                <span className="w-20 shrink-0 text-[10px] text-gx-muted">Base ref</span>
                <input
                  className="input-field min-w-0 flex-1"
                  defaultValue={card.runDefaults?.baseRef ?? board?.runDefaults?.baseRef ?? ""}
                  placeholder="HEAD"
                  onBlur={(event) => updateRunDefault("baseRef", event.target.value)}
                />
              </div>
              <div className="flex items-center gap-3">
                <span className="w-20 shrink-0 text-[10px] text-gx-muted">Branch</span>
                <input
                  className="input-field min-w-0 flex-1"
                  defaultValue={card.runDefaults?.branchName ?? ""}
                  placeholder="agent generated"
                  onBlur={(event) => updateRunDefault("branchName", event.target.value)}
                />
              </div>
              <div className="flex items-center gap-3">
                <span className="w-20 shrink-0 text-[10px] text-gx-muted">Agent</span>
                <select
                  className="input-field min-w-0 flex-1"
                  value={card.runDefaults?.agentType ?? board?.runDefaults?.agentType ?? "codex"}
                  onChange={(event) => updateRunDefault("agentType", event.target.value)}
                >
                  <option value="codex">Codex</option>
                  <option value="claude-code">Claude Code</option>
                  <option value="custom">Custom</option>
                </select>
              </div>
              <div className="flex items-center gap-3">
                <span className="w-20 shrink-0 text-[10px] text-gx-muted">Model</span>
                <input
                  className="input-field min-w-0 flex-1"
                  defaultValue={card.runDefaults?.model ?? board?.runDefaults?.model ?? ""}
                  placeholder="Agent default"
                  onBlur={(event) => updateRunDefault("model", event.target.value)}
                />
              </div>
            </div>
          </div>

          {/* Linked PR */}
          {card.linkedPrUrl && (
            <div className="flex items-center gap-3">
              <span className="text-[10px] text-gx-muted w-20 shrink-0">PR</span>
              <a
                href={card.linkedPrUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1 text-xs text-gx-accent hover:underline"
              >
                <GitPullRequest size={12} />
                #{card.linkedPrNumber}
              </a>
            </div>
          )}

          {/* External Issue Link */}
          {card.externalUrl && (
            <div className="flex items-center gap-3">
              <span className="text-[10px] text-gx-muted w-20 shrink-0">Issue</span>
              <a
                href={card.externalUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-1 text-xs text-gx-accent hover:underline"
              >
                <ExternalLink size={12} />
                {card.externalId || "View Issue"}
              </a>
            </div>
          )}

          {/* Dates */}
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">Created</span>
            <span className="text-xs text-gx-text">{formatDate(card.createdAt)}</span>
          </div>
          <div className="flex items-center gap-3">
            <span className="text-[10px] text-gx-muted w-20 shrink-0">Updated</span>
            <span className="text-xs text-gx-text">{formatDate(card.updatedAt)}</span>
          </div>
        </section>

        {(card.latestRun || card.linkedTaskId) && (
          <section>
            <h3 className="mb-2 text-[10px] font-medium uppercase tracking-wide text-gx-muted">
              Runs
            </h3>
            <div className="rounded border border-gx-border bg-gx-bg-sub px-2.5 py-2 text-[10px]">
              <div className="flex min-w-0 items-center justify-between gap-2">
                <div className="min-w-0">
                  <div className="truncate text-gx-text">
                    {card.latestRun?.agentType ?? "Agent"} · {card.latestRun?.status ?? card.pipelineState}
                  </div>
                  <div className="mt-0.5 truncate font-mono text-gx-muted">
                    {card.latestRun?.orchestratorTaskId ?? card.linkedTaskId}
                  </div>
                </div>
                {card.linkedTaskId && (
                  <button
                    type="button"
                    className="btn flex shrink-0 items-center gap-1 text-[10px]"
                    onClick={openLinkedChat}
                  >
                    <MessageSquare size={11} />
                    Open chat
                  </button>
                )}
              </div>
              {(card.latestRun?.branch || card.latestRun?.worktreePath) && (
                <div className="mt-2 space-y-1 text-gx-muted">
                  {card.latestRun.branch && <div className="truncate">Branch {card.latestRun.branch}</div>}
                  {card.latestRun.worktreePath && (
                    <div className="truncate" title={card.latestRun.worktreePath}>
                      Worktree {card.latestRun.worktreePath}
                    </div>
                  )}
                </div>
              )}
            </div>
          </section>
        )}

        {/* Relations Section */}
        <section>
          <div className="flex items-center justify-between mb-2">
            <h3 className="text-[10px] font-medium text-gx-muted uppercase tracking-wide">
              Relations
            </h3>
            <button
              onClick={() => setShowRelationSearch(!showRelationSearch)}
              className="btn-icon text-gx-muted hover:text-gx-accent"
              aria-label="Add relation"
              title="Add relation"
            >
              <Plus size={12} />
            </button>
          </div>

          {showRelationSearch && (
            <RelationSearch
              sourceId={card.id}
              allCards={allCards}
              existingRelationIds={existingRelationIds}
              onCreateRelation={(targetId, type) => {
                onCreateRelation(card.id, targetId, type);
                setShowRelationSearch(false);
              }}
              onClose={() => setShowRelationSearch(false)}
            />
          )}

          {hasRelations ? (
            <div className="space-y-3 mt-2">
              {RELATION_TYPES.map((type) => {
                const rels = groupedRelations[type];
                if (rels.length === 0) return null;

                return (
                  <div key={type}>
                    <h4 className="text-[10px] text-gx-muted mb-1">
                      {RELATION_TYPE_LABELS[type]}
                    </h4>
                    <div className="space-y-0.5">
                      {rels.map((rel) => {
                        const targetCard = cardLookup.get(rel.targetCardId);
                        return (
                          <div
                            key={`${type}-${rel.targetCardId}`}
                            className="flex items-center gap-2 group"
                          >
                            <RelationIcon type={type} />
                            <button
                              type="button"
                              onClick={() => onNavigateToCard(rel.targetCardId)}
                              className="flex-1 flex items-center gap-1.5 text-left rounded px-1 py-0.5 hover:bg-gx-surface-hover transition min-w-0"
                            >
                              <span className="text-[10px] font-mono text-gx-muted shrink-0">
                                {rel.targetCardId.slice(0, 6)}
                              </span>
                              <span className="text-[10px] text-gx-text truncate">
                                {targetCard?.title ?? "Unknown card"}
                              </span>
                            </button>
                            <button
                              type="button"
                              onClick={() => onDeleteRelation(card.id, rel.targetCardId, type)}
                              className="opacity-0 group-hover:opacity-100 btn-icon text-gx-muted hover:text-gx-error transition"
                              aria-label="Remove relation"
                            >
                              <X size={10} />
                            </button>
                          </div>
                        );
                      })}
                    </div>
                  </div>
                );
              })}
            </div>
          ) : (
            !showRelationSearch && (
              <p className="text-[10px] text-gx-muted italic">No relations</p>
            )
          )}
        </section>

        {/* Activity Log */}
        <section>
          <h3 className="text-[10px] font-medium text-gx-muted uppercase tracking-wide mb-2">
            Activity
          </h3>
          {card.activity && card.activity.length > 0 ? (
            <div className="space-y-1.5">
              {card.activity.map((entry) => (
                <div key={entry.id} className="flex items-start gap-2 text-[10px]">
                  <span className="mt-1 h-1.5 w-1.5 shrink-0 rounded-full bg-gx-accent" />
                  <div className="min-w-0 flex-1">
                    <span className="text-gx-text">{entry.message}</span>
                    <span className="ml-1.5 text-gx-muted">{formatTimestamp(entry.createdAt)}</span>
                  </div>
                </div>
              ))}
            </div>
          ) : card.activityLog && card.activityLog.length > 0 ? (
            <div className="space-y-1.5">
              {card.activityLog
                .slice()
                .sort((a, b) => b.timestamp - a.timestamp)
                .map((entry, idx) => (
                  <div
                    key={idx}
                    className="flex items-start gap-2 text-[10px]"
                  >
                    <span className="h-1.5 w-1.5 rounded-full bg-gx-accent mt-1 shrink-0" />
                    <div className="flex-1 min-w-0">
                      <span className="text-gx-text">
                        {entry.fromState} → {entry.toState}
                      </span>
                      <span className="text-gx-muted ml-1.5">
                        {formatTimestamp(entry.timestamp)}
                      </span>
                    </div>
                  </div>
                ))}
            </div>
          ) : (
            <p className="text-[10px] text-gx-muted italic">No activity recorded</p>
          )}
        </section>
      </div>
    </div>
  );
}

// ── Relation Icon ───────────────────────────────────────────────────

function RelationIcon({ type }: { type: RelationType }) {
  switch (type) {
    case "parent":
      return <Users size={10} className="text-gx-muted shrink-0" />;
    case "child":
      return <Users size={10} className="text-gx-muted shrink-0" />;
    case "blocks":
      return <ArrowRight size={10} className="text-orange-400 shrink-0" />;
    case "blocked-by":
      return <Lock size={10} className="text-gx-error shrink-0" />;
    case "relates-to":
      return <Link2 size={10} className="text-gx-muted shrink-0" />;
  }
}
