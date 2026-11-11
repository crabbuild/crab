import { useState } from "react";
import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Circle,
  ListTodo,
  Loader2,
  ArrowUp,
} from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface TodoWriteItem {
  id?: string;
  content: string;
  status: "completed" | "in_progress" | "pending";
}

export interface TodoWriteCardProps {
  /** The full input object from the TodoWrite tool call. */
  input?: unknown;
  /** Previous input (from the last TodoWrite call) for computing deltas. */
  previousInput?: unknown;
  /** Whether the card starts expanded. */
  defaultExpanded?: boolean;
}

// ── Helpers ─────────────────────────────────────────────────────────

function parseTodos(input: unknown): TodoWriteItem[] {
  if (!input || typeof input !== "object") return [];
  const obj = input as Record<string, unknown>;
  if (!Array.isArray(obj.todos)) return [];
  return (obj.todos as unknown[]).map((item) => {
    if (typeof item === "object" && item !== null) {
      const t = item as Record<string, unknown>;
      return {
        id: typeof t.id === "string" ? t.id : undefined,
        content: String(t.content ?? t.text ?? t.title ?? ""),
        status: (t.status as TodoWriteItem["status"]) ?? "pending",
      };
    }
    return { content: String(item), status: "pending" as const };
  });
}

/** Compute which items changed between previous and current state. */
function computeDeltas(
  prev: TodoWriteItem[],
  curr: TodoWriteItem[],
): { changed: Map<string, { from: TodoWriteItem["status"]; to: TodoWriteItem["status"] }>; added: Set<string> } {
  const changed = new Map<string, { from: TodoWriteItem["status"]; to: TodoWriteItem["status"] }>();
  const added = new Set<string>();

  const prevMap = new Map<string, TodoWriteItem>();
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

function StatusIcon({ status }: { status: TodoWriteItem["status"] }) {
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

function StatusBadge({ status }: { status: TodoWriteItem["status"] }) {
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

// ── Delta section ───────────────────────────────────────────────────

function DeltaSection({ changed, added, todos }: {
  changed: Map<string, { from: TodoWriteItem["status"]; to: TodoWriteItem["status"] }>;
  added: Set<string>;
  todos: TodoWriteItem[];
}) {
  const entries: { key: string; content: string; type: "changed" | "added"; from?: string; to?: string }[] = [];

  for (const item of todos) {
    const key = item.id ?? item.content;
    if (changed.has(key)) {
      const delta = changed.get(key)!;
      entries.push({ key, content: item.content, type: "changed", from: delta.from, to: delta.to });
    } else if (added.has(key)) {
      entries.push({ key, content: item.content, type: "added" });
    }
  }

  if (entries.length === 0) return null;

  return (
    <div className="space-y-1 mb-2">
      <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">Changes</span>
      {entries.map((entry) => (
        <div key={entry.key} className="flex items-center gap-2 text-[10px]">
          {entry.type === "added" ? (
            <>
              <span className="text-success">+</span>
              <span className="flex-1 truncate text-foreground">{entry.content}</span>
              <span className="text-[9px] text-success/70 italic">new</span>
            </>
          ) : (
            <>
              <ArrowUp size={9} className="text-primary shrink-0" />
              <span className="flex-1 truncate text-foreground">{entry.content}</span>
              <span className="text-[9px] text-muted-foreground">
                {entry.from} → <span className="text-primary">{entry.to}</span>
              </span>
            </>
          )}
        </div>
      ))}
    </div>
  );
}

// ── Main component ──────────────────────────────────────────────────

export function TodoWriteCard({ input, previousInput, defaultExpanded = false }: TodoWriteCardProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);

  const todos = parseTodos(input);
  const prevTodos = parseTodos(previousInput);

  if (todos.length === 0) {
    // Fallback: nothing to render.
    return null;
  }

  const completed = todos.filter((t) => t.status === "completed").length;
  const inProgress = todos.filter((t) => t.status === "in_progress").length;
  const pending = todos.filter((t) => t.status === "pending").length;
  const total = todos.length;

  // Compute deltas if we have a previous state.
  const { changed, added } = prevTodos.length > 0
    ? computeDeltas(prevTodos, todos)
    : { changed: new Map<string, { from: TodoWriteItem["status"]; to: TodoWriteItem["status"] }>(), added: new Set<string>() };

  const hasDeltas = changed.size > 0 || added.size > 0;

  return (
    <div className="rounded-md border border-border/50 bg-background overflow-hidden">
      {/* Header */}
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-accent transition cursor-pointer"
      >
        <ListTodo size={11} className="text-warning shrink-0" />
        <span className="shrink-0 text-[10px] font-medium text-muted-foreground/80">Todos</span>

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
          {pending > 0 && (
            <span className="flex items-center gap-0.5 text-muted-foreground">
              <Circle size={8} />
              {pending}
            </span>
          )}
        </span>

        {/* Delta indicator */}
        {hasDeltas && (
          <span className="rounded bg-primary/15 px-1 py-0.5 text-[8px] font-medium text-primary">
            {changed.size + added.size} updated
          </span>
        )}

        <span className="flex-1" />
        {expanded
          ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
          : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        }
      </button>

      {/* Expanded content */}
      {expanded && (
        <div className="border-t border-border/30 px-2.5 py-2 space-y-2">
          {/* Progress bar */}
          <ProgressBar completed={completed} total={total} />

          {/* Delta section (what changed this call) */}
          {hasDeltas && (
            <DeltaSection changed={changed} added={added} todos={todos} />
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
