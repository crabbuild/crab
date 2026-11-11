import { NativeSelect } from "@/components/ui/native-select";
import clsx from "clsx";
import { GripVertical, Loader2 } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { rpc, TodoActionInput, TodoItemInput } from "../ipc";

/**
 * Interactive rebase todo editor.
 *
 * Renders a list of commits with per-row action selectors and
 * drag-and-drop reordering (HTML5 DnD, no library). The user edits
 * the todo and clicks "Apply" to submit via `rpc.opEditTodo`.
 *
 * This component is rendered when there's an active paused rebase
 * and the user wants to edit the todo list.
 */
export interface RebaseTodoProps {
  /** Active repo id. */
  repoId: string;
  /** Active operation id (from the paused rebase). */
  opId: string;
  /** Initial todo items (commits in rebase order). */
  initialTodo: TodoItemInput[];
  /** Called after a successful todo edit. */
  onApplied: () => void;
  /** Called to dismiss without applying. */
  onCancel: () => void;
}

/** The set of actions available per commit row. */
const TODO_ACTIONS: readonly TodoActionInput[] = [
  "Pick",
  "Reword",
  "Squash",
  "Fixup",
  "Drop",
] as const;

/** Display label for a todo action. */
function actionLabel(action: TodoActionInput): string {
  if (typeof action === "string") {
    return action.toLowerCase();
  }
  return `exec: ${action.Exec}`;
}

/** Parse a display label back to a TodoActionInput. */
function parseAction(value: string): TodoActionInput {
  switch (value) {
    case "Pick":
    case "Reword":
    case "Squash":
    case "Fixup":
    case "Drop":
      return value;
    default:
      return "Pick";
  }
}

export function RebaseTodo({
  repoId,
  opId,
  initialTodo,
  onApplied,
  onCancel,
}: RebaseTodoProps) {
  const [items, setItems] = useState<TodoItemInput[]>(() => [...initialTodo]);
  const [applying, setApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Drag state
  const dragIndexRef = useRef<number | null>(null);
  const [dragOverIndex, setDragOverIndex] = useState<number | null>(null);

  const handleActionChange = useCallback((index: number, action: TodoActionInput) => {
    setItems((prev) => {
      const next = [...prev];
      next[index] = { ...next[index], action };
      return next;
    });
  }, []);

  // ── Drag-and-drop handlers ──────────────────────────────────────

  function handleDragStart(e: React.DragEvent<HTMLDivElement>, index: number) {
    dragIndexRef.current = index;
    e.dataTransfer.effectAllowed = "move";
    // Set minimal drag data for Firefox compatibility
    e.dataTransfer.setData("text/plain", String(index));
  }

  function handleDragOver(e: React.DragEvent<HTMLDivElement>, index: number) {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    setDragOverIndex(index);
  }

  function handleDragLeave() {
    setDragOverIndex(null);
  }

  function handleDrop(e: React.DragEvent<HTMLDivElement>, dropIndex: number) {
    e.preventDefault();
    const fromIndex = dragIndexRef.current;
    setDragOverIndex(null);
    dragIndexRef.current = null;

    if (fromIndex === null || fromIndex === dropIndex) return;

    setItems((prev) => {
      const next = [...prev];
      const [moved] = next.splice(fromIndex, 1);
      next.splice(dropIndex, 0, moved);
      return next;
    });
  }

  function handleDragEnd() {
    dragIndexRef.current = null;
    setDragOverIndex(null);
  }

  // ── Apply ───────────────────────────────────────────────────────

  async function handleApply() {
    setApplying(true);
    setError(null);
    try {
      await rpc.opEditTodo(repoId, opId, items);
      onApplied();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setApplying(false);
    }
  }

  return (
    <div className="flex flex-col rounded-lg border border-border bg-card shadow-lg">
      {/* Header */}
      <div className="border-b border-border px-4 py-3">
        <h3 className="text-[13px] font-semibold text-foreground">
          Interactive Rebase
        </h3>
        <p className="mt-0.5 text-[11px] text-muted-foreground">
          Reorder commits by dragging. Change the action per commit using the dropdown.
        </p>
      </div>

      {/* Todo list */}
      <div className="max-h-[400px] min-h-[120px] overflow-auto px-2 py-2">
        {items.length === 0 && (
          <div className="px-2 py-4 text-center text-[12px] text-muted-foreground">
            No commits in the rebase todo.
          </div>
        )}
        {items.map((item, index) => (
          <div
            key={`${item.sha}-${index}`}
            draggable
            onDragStart={(e) => handleDragStart(e, index)}
            onDragOver={(e) => handleDragOver(e, index)}
            onDragLeave={handleDragLeave}
            onDrop={(e) => handleDrop(e, index)}
            onDragEnd={handleDragEnd}
            className={clsx(
              "flex items-center gap-2 rounded px-2 py-1.5 transition",
              dragOverIndex === index
                ? "border border-primary/50 bg-primary/5"
                : "border border-transparent hover:bg-background",
            )}
          >
            {/* Drag handle */}
            <span
              className="shrink-0 cursor-grab text-muted-foreground active:cursor-grabbing"
              aria-label="Drag to reorder"
            >
              <GripVertical size={14} />
            </span>

            {/* Action selector */}
            <NativeSelect
              value={typeof item.action === "string" ? item.action : "Pick"}
              onChange={(e) => handleActionChange(index, parseAction(e.target.value))}
              className="shrink-0 rounded border border-border bg-background px-1.5 py-0.5 text-[11px] text-foreground focus-visible:outline-none focus:ring-1 focus:ring-primary"
              aria-label={`Action for commit ${item.sha.slice(0, 7)}`}
              disabled={applying}
            >
              {TODO_ACTIONS.map((action) => (
                <option key={typeof action === "string" ? action : action.Exec} value={typeof action === "string" ? action : "Pick"}>
                  {actionLabel(action)}
                </option>
              ))}
            </NativeSelect>

            {/* SHA */}
            <span className="shrink-0 font-mono text-[11px] text-primary">
              {item.sha.slice(0, 7)}
            </span>

            {/* Subject */}
            <span className="min-w-0 flex-1 truncate text-[12px] text-foreground">
              {item.subject}
            </span>
          </div>
        ))}
      </div>

      {/* Error */}
      {error && (
        <div className="mx-4 mb-2 rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
          {error}
        </div>
      )}

      {/* Footer */}
      <div className="flex items-center justify-end gap-2 border-t border-border p-3">
        <button
          type="button"
          onClick={onCancel}
          disabled={applying}
          className={clsx(
            "rounded border border-border bg-card px-3 py-1.5 text-[12px] text-foreground transition",
            "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            "disabled:opacity-50",
          )}
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={handleApply}
          disabled={applying || items.length === 0}
          className={clsx(
            "flex items-center gap-1.5 rounded border border-primary/60 bg-primary/15 px-3 py-1.5 text-[12px] font-medium text-primary transition",
            "hover:bg-primary/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
            "disabled:opacity-50",
          )}
        >
          {applying && <Loader2 size={12} className="animate-spin" />}
          Apply
        </button>
      </div>
    </div>
  );
}
