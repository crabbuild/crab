import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  AlertTriangle,
  Bot,
  Check,
  MessageSquare,
  Plus,
  Trash2,
  User,
} from "lucide-react";
import { useCallback, useState } from "react";

// ── Types ───────────────────────────────────────────────────────────

export interface TodoItem {
  id: string;
  text: string;
  done: boolean;
  source: "user" | "agent" | "comment";
}

interface Props {
  taskId: string;
  todos: TodoItem[];
  onToggle: (todoId: string) => void;
  onAdd: (text: string, source?: "user" | "agent" | "comment") => void;
  onRemove: (todoId: string) => void;
  /** When true, shows as a compact collapsible section. */
  compact?: boolean;
  /** When true, shows merge-blocker warnings for incomplete todos. */
  showBlockers?: boolean;
}

// ── Source icon ─────────────────────────────────────────────────────

function SourceIcon({ source }: { source: TodoItem["source"] }) {
  switch (source) {
    case "agent":
      return <Bot size={10} className="text-primary" />;
    case "comment":
      return <MessageSquare size={10} className="text-warning" />;
    default:
      return <User size={10} className="text-info" />;
  }
}

// ── Component ───────────────────────────────────────────────────────

export function TodoList({ taskId: _taskId, todos, onToggle, onAdd, onRemove, compact, showBlockers }: Props) {
  const [newText, setNewText] = useState("");
  const [isAdding, setIsAdding] = useState(false);

  const incompleteTodos = todos.filter((t) => !t.done);

  const handleAdd = useCallback(() => {
    const text = newText.trim();
    if (!text) return;
    onAdd(text, "user");
    setNewText("");
    setIsAdding(false);
  }, [newText, onAdd]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAdd();
      } else if (e.key === "Escape") {
        setIsAdding(false);
        setNewText("");
      }
    },
    [handleAdd],
  );

  return (
    <div className="space-y-2">
      {/* Merge blocker warning */}
      {showBlockers && incompleteTodos.length > 0 && (
        <div className="flex items-center gap-1.5 rounded-md border border-warning/30 bg-warning/10 px-2 py-1.5 text-[11px] text-warning">
          <AlertTriangle size={11} />
          <span>
            {incompleteTodos.length} incomplete todo
            {incompleteTodos.length > 1 ? "s" : ""} blocking merge
          </span>
        </div>
      )}

      {/* Todo items */}
      <div className="space-y-1">
        {todos.map((todo) => (
          <div
            key={todo.id}
            className="group flex items-center gap-2 rounded px-2 py-1 hover:bg-accent transition"
          >
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onToggle(todo.id)}
              className={`flex h-4 w-4 shrink-0 items-center justify-center rounded border transition ${
                todo.done
                  ? "border-success bg-success/20 text-success"
                  : "border-border hover:border-primary"
              }`}
              aria-label={todo.done ? "Mark incomplete" : "Mark complete"}
            >
              {todo.done && <Check size={10} />}
            </Button>

            <span
              className={`flex-1 text-xs ${
                todo.done ? "text-muted-foreground line-through" : "text-foreground"
              }`}
            >
              {todo.text}
            </span>

            <SourceIcon source={todo.source} />

            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onRemove(todo.id)}
              className="shrink-0 opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-destructive transition"
              aria-label="Remove todo"
            >
              <Trash2 size={11} />
            </Button>
          </div>
        ))}
      </div>

      {/* Add todo */}
      {isAdding ? (
        <div className="flex items-center gap-2 px-2">
          <Input
            type="text"
            className="flex-1 rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
            placeholder="Add a todo…"
            value={newText}
            onChange={(e) => setNewText(e.target.value)}
            onKeyDown={handleKeyDown}
            autoFocus
          />
          <Button
            variant="default"
            size="compact"
            type="button"
            onClick={handleAdd}
            disabled={!newText.trim()}
            className="rounded bg-primary px-2 py-1 text-[10px] font-medium text-primary-foreground disabled:opacity-50"
          >
            Add
          </Button>
        </div>
      ) : (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setIsAdding(true)}
          className="flex items-center gap-1.5 px-2 py-1 text-[11px] text-muted-foreground hover:text-foreground transition"
        >
          <Plus size={11} />
          Add todo
        </Button>
      )}

      {/* Empty state */}
      {todos.length === 0 && !isAdding && !compact && (
        <div className="px-2 py-2 text-[11px] text-muted-foreground">
          No todos yet. Add items to track before merge.
        </div>
      )}
    </div>
  );
}

// ── Helper: Extract todos from agent messages ───────────────────────

/**
 * Extracts todo items from agent plan messages.
 * Detects `- [ ]` markdown checkboxes and `TODO:` patterns.
 */
export function extractTodosFromMessage(content: string): string[] {
  const todos: string[] = [];

  const lines = content.split("\n");
  for (const line of lines) {
    // Match `- [ ] some text` or `* [ ] some text`
    const checkboxMatch = line.match(/^[\s]*[-*]\s*\[\s*\]\s+(.+)/);
    if (checkboxMatch) {
      todos.push(checkboxMatch[1].trim());
      continue;
    }

    // Match `TODO: some text` or `TODO some text`
    const todoMatch = line.match(/\bTODO:?\s+(.+)/i);
    if (todoMatch) {
      todos.push(todoMatch[1].trim());
    }
  }

  return todos;
}
