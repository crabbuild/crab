import { Button } from "@/components/ui/button";
import clsx from "clsx";
import {
  ArrowUp,
  Check,
  ChevronDown,
  ChevronUp,
  Circle,
  ListTodo,
  Loader2,
} from "lucide-react";
import { useState } from "react";

// ── Types ───────────────────────────────────────────────────────────

interface TodoItem {
  id?: string;
  content: string;
  status: "completed" | "in_progress" | "pending";
}

export interface TodoProgressPanelProps {
  /** The latest TodoWrite input (full todos array). */
  input: unknown;
  /** Previous TodoWrite input for computing deltas. */
  previousInput?: unknown;
}

// ── Helpers ─────────────────────────────────────────────────────────

function parseTodos(input: unknown): TodoItem[] {
  if (!input || typeof input !== "object") return [];
  const obj = input as Record<string, unknown>;
  if (!Array.isArray(obj.todos)) return [];
  return (obj.todos as unknown[]).map((item) => {
    if (typeof item === "object" && item !== null) {
      const t = item as Record<string, unknown>;
      return {
        id: typeof t.id === "string" ? t.id : undefined,
        content: String(t.content ?? t.text ?? t.title ?? ""),
        status: (t.status as TodoItem["status"]) ?? "pending",
      };
    }
    return { content: String(item), status: "pending" as const };
  });
}

function computeDeltas(
  prev: TodoItem[],
  curr: TodoItem[],
): { changed: Map<string, { from: TodoItem["status"]; to: TodoItem["status"] }>; added: Set<string> } {
  const changed = new Map<string, { from: TodoItem["status"]; to: TodoItem["status"] }>();
  const added = new Set<string>();

  const prevMap = new Map<string, TodoItem>();
  for (const item of prev) {
    const key = item.id ?? item.content;
    prevMap.set(key, item);
  }

  for (const item of curr) {
    const key = item.id ?? item.content;
    const prevItem = prevMap.get(key);
    if (!prevItem) {
      added.add(key);
    } else if (prevItem.status !== item.status) {
      changed.set(key, { from: prevItem.status, to: item.status });
    }
  }

  return { changed, added };
}

// ── Status rendering ────────────────────────────────────────────────

function StatusIcon({ status }: { status: TodoItem["status"] }) {
  switch (status) {
    case "completed":
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-success/20 text-success">
          <Check size={10} strokeWidth={3} />
        </div>
      );
    case "in_progress":
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-primary/20 text-primary">
          <Loader2 size={10} className="animate-spin" />
        </div>
      );
    default:
      return (
        <div className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full border border-border text-muted-foreground">
          <Circle size={8} />
        </div>
      );
  }
}

function StatusBadge({ status }: { status: TodoItem["status"] }) {
  const styles = {
    completed: "bg-success/15 text-success border-success/30",
    in_progress: "bg-primary/15 text-primary border-primary/30",
    pending: "bg-card text-muted-foreground border-border",
  };
  const labels = {
    completed: "done",
    in_progress: "active",
    pending: "pending",
  };
  return (
    <span className={clsx("rounded px-1.5 py-0.5 text-[9px] font-medium border", styles[status])}>
      {labels[status]}
    </span>
  );
}

// ── Progress bar ────────────────────────────────────────────────────

function ProgressBar({ completed, total }: { completed: number; total: number }) {
  const pct = total > 0 ? Math.round((completed / total) * 100) : 0;
  return (
    <div className="flex items-center gap-2">
      <div className="flex-1 h-1.5 rounded-full bg-card overflow-hidden">
        <div
          className="h-full rounded-full bg-success/70 transition-all duration-300"
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="text-[9px] text-muted-foreground font-mono tabular-nums">
        {completed}/{total}
      </span>
    </div>
  );
}

// ── Main component ──────────────────────────────────────────────────

export function TodoProgressPanel({ input, previousInput }: TodoProgressPanelProps) {
  const [expanded, setExpanded] = useState(true);

  const todos = parseTodos(input);
  const prevTodos = parseTodos(previousInput);

  if (todos.length === 0) return null;

  const completed = todos.filter((t) => t.status === "completed").length;
  const inProgress = todos.filter((t) => t.status === "in_progress").length;
  const total = todos.length;

  const { changed, added } = prevTodos.length > 0
    ? computeDeltas(prevTodos, todos)
    : { changed: new Map<string, { from: TodoItem["status"]; to: TodoItem["status"] }>(), added: new Set<string>() };

  const hasDeltas = changed.size > 0 || added.size > 0;

  // Determine overall status label
  const statusLabel = completed === total
    ? "Completed"
    : inProgress > 0
      ? "In progress"
      : "Pending";

  return (
    <div className="shrink-0 border-t border-border bg-background">
      {/* Header — always visible */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="justify-start flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-accent transition cursor-pointer"
      >
        <ListTodo size={12} className="text-warning shrink-0" />
        <span className="text-[11px] font-medium text-foreground">Todos</span>

        {/* Compact progress summary */}
        <span className="flex items-center gap-1.5 text-[10px] text-foreground/80">
          {completed > 0 && (
            <span className="flex items-center gap-0.5 text-success">
              <Check size={9} strokeWidth={3} />
              {completed}
            </span>
          )}
          {inProgress > 0 && (
            <span className="flex items-center gap-0.5 text-primary">
              <Loader2 size={9} />
              {inProgress}
            </span>
          )}
        </span>

        {/* Status label */}
        <span className={clsx(
          "rounded px-1.5 py-0.5 text-[9px] font-medium border",
          completed === total
            ? "bg-success/15 text-success border-success/30"
            : "bg-primary/15 text-primary border-primary/30",
        )}>
          {statusLabel}
        </span>

        {/* Progress fraction */}
        <span className="text-[10px] text-muted-foreground font-mono tabular-nums ml-auto mr-1">
          {completed}/{total}
        </span>

        {expanded ? (
          <ChevronDown size={11} className="shrink-0 text-muted-foreground/60" />
        ) : (
          <ChevronUp size={11} className="shrink-0 text-muted-foreground/60" />
        )}
      </Button>

      {/* Expanded content */}
      {expanded && (
        <div className="border-t border-border/30 px-3 py-2 space-y-2 max-h-[200px] overflow-auto">
          {/* Expanded content */}
          <ProgressBar completed={completed} total={total} />

          {/* Progress bar */}
          {hasDeltas && (
            <div className="space-y-1 mb-2">
              <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">
                Changes
              </span>
              {todos.map((item) => {
                const key = item.id ?? item.content;
                const delta = changed.get(key);
                const isAdded = added.has(key);
                if (!delta && !isAdded) return null;
                return (
                  <div key={key} className="flex items-center gap-2 text-[10px]">
                    {isAdded ? (
                      <>
                        <span className="text-success">+</span>
                        <span className="flex-1 truncate text-foreground">
                          {item.content}
                        </span>
                        <span className="text-[9px] text-success/70 italic">
                          new
                        </span>
                      </>
                    ) : (
                      <>
                        <ArrowUp size={9} className="text-primary shrink-0" />
                        <span className="flex-1 truncate text-foreground">
                          {item.content}
                        </span>
                        <span className="text-[9px] text-muted-foreground">
                          {delta!.from} →{" "}
                          <span className="text-primary">{delta!.to}</span>
                        </span>
                      </>
                    )}
                  </div>
                );
              })}
            </div>
          )}

          {/* Full checklist */}
          <div className="space-y-0.5">
            {todos.map((item, idx) => {
              const key = item.id ?? `${idx}-${item.content}`;
              const itemKey = item.id ?? item.content;
              const isChanged = changed.has(itemKey) || added.has(itemKey);

              return (
                <div
                  key={key}
                  className={clsx(
                    "flex items-center gap-2 rounded px-1.5 py-1 transition",
                    isChanged && "bg-primary/5 border-l-2 border-primary/40",
                    !isChanged && "border-l-2 border-transparent",
                  )}
                >
                  <StatusIcon status={item.status} />
                  <span
                    className={clsx(
                      "flex-1 text-[10px] leading-tight",
                      item.status === "completed" && "text-muted-foreground line-through",
                      item.status === "in_progress" && "text-foreground font-medium",
                      item.status === "pending" && "text-foreground/80",
                    )}
                  >
                    {item.content}
                  </span>
                  <StatusBadge status={item.status} />
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
