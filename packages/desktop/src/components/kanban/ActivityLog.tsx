import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { Activity, CheckCircle2, XCircle, Clock } from "lucide-react";
import type { KanbanAutomationRule, KanbanCardInfo } from "../../ipc";

// ── Types ───────────────────────────────────────────────────────────

interface RuleExecutionEntry {
  id: string;
  ruleId: string;
  ruleName: string;
  cardId: string;
  cardTitle: string;
  status: "success" | "failed" | "running";
  timestamp: number;
  actionResults?: Array<{
    actionType: string;
    status: "success" | "failed" | "skipped";
    error?: string;
    durationMs: number;
  }>;
}

interface ActivityLogProps {
  rules: KanbanAutomationRule[];
  cards: KanbanCardInfo[];
  /** Maximum entries to keep in the feed. */
  maxEntries?: number;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTimestamp(ts: number): string {
  const date = new Date(ts);
  const now = new Date();
  const diffMs = now.getTime() - ts;

  if (diffMs < 60_000) return "just now";
  if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
  if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;

  // Same day: show time
  if (date.toDateString() === now.toDateString()) {
    return date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  }

  return date.toLocaleDateString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

const STATUS_ICON = {
  success: CheckCircle2,
  failed: XCircle,
  running: Clock,
} as const;

const STATUS_COLOR = {
  success: "text-gx-success",
  failed: "text-gx-error",
  running: "text-gx-warn",
} as const;

// ── Component ───────────────────────────────────────────────────────

/**
 * Real-time activity feed for automation rule executions. Subscribes
 * to kanban:rule-executed IPC events and displays rule name, card
 * title, status, timestamp, and action results.
 */
export function ActivityLog({ rules, cards, maxEntries = 50 }: ActivityLogProps) {
  const [entries, setEntries] = useState<RuleExecutionEntry[]>([]);
  const [expandedEntry, setExpandedEntry] = useState<string | null>(null);
  const entriesRef = useRef(entries);
  entriesRef.current = entries;

  // Build lookup maps for rule names and card titles
  const ruleNameMap = useRef(new Map<string, string>());
  const cardTitleMap = useRef(new Map<string, string>());

  useEffect(() => {
    const map = new Map<string, string>();
    for (const rule of rules) {
      map.set(rule.id, rule.name);
    }
    ruleNameMap.current = map;
  }, [rules]);

  useEffect(() => {
    const map = new Map<string, string>();
    for (const card of cards) {
      map.set(card.id, card.title);
    }
    cardTitleMap.current = map;
  }, [cards]);

  // Subscribe to rule-executed IPC events
  useEffect(() => {
    const kanban = window.crab?.kanban;
    if (!kanban) return;

    const unsub = kanban.onRuleExecuted((payload) => {
      const entry: RuleExecutionEntry = {
        id: `${payload.ruleId}-${payload.cardId}-${Date.now()}`,
        ruleId: payload.ruleId,
        ruleName: ruleNameMap.current.get(payload.ruleId) ?? payload.ruleId,
        cardId: payload.cardId,
        cardTitle: cardTitleMap.current.get(payload.cardId) ?? payload.cardId,
        status: payload.status as RuleExecutionEntry["status"],
        timestamp: Date.now(),
      };

      setEntries((prev) => {
        const next = [entry, ...prev];
        if (next.length > maxEntries) next.length = maxEntries;
        return next;
      });
    });

    return unsub;
  }, [maxEntries]);

  const toggleExpand = useCallback((entryId: string) => {
    setExpandedEntry((prev) => (prev === entryId ? null : entryId));
  }, []);

  if (entries.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-8 text-center">
        <Activity size={20} className="text-gx-muted mb-2" />
        <p className="text-xs text-gx-muted">No activity yet.</p>
        <p className="text-[10px] text-gx-muted mt-0.5">
          Rule executions will appear here in real time.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-1">
      {entries.map((entry) => {
        const Icon = STATUS_ICON[entry.status];
        const isExpanded = expandedEntry === entry.id;

        return (
          <div
            key={entry.id}
            className="rounded border border-gx-border bg-gx-bg-sub p-2 transition hover:bg-gx-surface-hover"
          >
            <button
              type="button"
              className="flex w-full items-start gap-2 text-left"
              onClick={() => toggleExpand(entry.id)}
            >
              <Icon
                size={14}
                className={clsx("mt-0.5 shrink-0", STATUS_COLOR[entry.status])}
              />
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-1.5">
                  <span className="text-xs font-medium text-gx-text truncate">
                    {entry.ruleName}
                  </span>
                  <span className="text-[10px] text-gx-muted shrink-0">
                    {formatTimestamp(entry.timestamp)}
                  </span>
                </div>
                <p className="text-[10px] text-gx-muted truncate mt-0.5">
                  Card: {entry.cardTitle}
                </p>
              </div>
            </button>

            {/* Expanded action results */}
            {isExpanded && entry.actionResults && entry.actionResults.length > 0 && (
              <div className="mt-2 ml-5 space-y-1 border-t border-gx-border pt-2">
                {entry.actionResults.map((result, idx) => (
                  <div key={idx} className="flex items-center gap-1.5 text-[10px]">
                    <span
                      className={clsx(
                        "h-1.5 w-1.5 rounded-full shrink-0",
                        result.status === "success" && "bg-gx-success",
                        result.status === "failed" && "bg-gx-error",
                        result.status === "skipped" && "bg-gx-muted",
                      )}
                    />
                    <span className="text-gx-text">{formatActionType(result.actionType)}</span>
                    <span className="text-gx-muted">
                      {result.durationMs > 0 ? `${(result.durationMs / 1000).toFixed(1)}s` : "—"}
                    </span>
                    {result.error && (
                      <span className="text-gx-error truncate" title={result.error}>
                        {result.error}
                      </span>
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatActionType(type: string): string {
  return type
    .replace(/_/g, " ")
    .replace(/\b\w/g, (c) => c.toUpperCase());
}
