import { Input } from "@/components/ui/input";
import { CheckSquare, Link2, Plus, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { useTaskList } from "../hooks/useTaskList";
import type { TaskItem } from "../lib/workspace-settings-store";
import { formatAnchor, parseAnchor } from "../lib/task-anchor";

interface TaskListPopoverProps {
  /** Callback when the user clicks a file anchor (navigate to file:line). */
  onNavigateFile?: (path: string, line: number) => void;
  /** Callback when the user clicks a commit anchor (open commit detail). */
  onNavigateCommit?: (sha: string) => void;
}

/**
 * Status-bar button + popover for the workspace task list.
 *
 * Shows a "✓ N/M tasks" chip in the status bar. Clicking opens a
 * popover with the full task list, checkboxes, delete buttons, and
 * an "Add task" input at the bottom.
 */
export function TaskListPopover({ onNavigateFile, onNavigateCommit }: TaskListPopoverProps) {
  const { key: workspaceKey, tasks, loading, error, canEdit, reload, addTask, toggleTask, removeTask, remaining, total } = useTaskList();
  const [open, setOpen] = useState(false);
  const [newTitle, setNewTitle] = useState("");
  const popoverRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const keyRef = useRef(workspaceKey); keyRef.current = workspaceKey;
  useEffect(() => { setNewTitle(""); setOpen(false); }, [workspaceKey]);

  // Close on outside click.
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (
        popoverRef.current &&
        !popoverRef.current.contains(e.target as Node) &&
        buttonRef.current &&
        !buttonRef.current.contains(e.target as Node)
      ) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // Close on Escape.
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open]);

  // Focus input when popover opens.
  useEffect(() => {
    if (open) {
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [open]);

  const handleAdd = useCallback(async () => {
    const title = newTitle.trim();
    if (!title) return;
    if (await addTask(title) && keyRef.current === workspaceKey) setNewTitle(current => current === title ? "" : current);
  }, [newTitle, addTask, workspaceKey]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAdd();
      }
    },
    [handleAdd],
  );

  const handleAnchorClick = useCallback(
    (anchor: string) => {
      const parsed = parseAnchor(anchor);
      if (!parsed) return;
      if (parsed.type === "file" && onNavigateFile) {
        onNavigateFile(parsed.path, parsed.line);
      } else if (parsed.type === "commit" && onNavigateCommit) {
        onNavigateCommit(parsed.sha);
      }
    },
    [onNavigateFile, onNavigateCommit],
  );

  const done = total - remaining;

  return (
    <div className="relative">
      {/* Status bar button */}
      <button
        ref={buttonRef}
        type="button"
        onClick={() => setOpen((v) => !v)}
        className={clsx(
          "flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] hover:bg-background hover:text-foreground",
          open ? "bg-background text-foreground" : "text-muted-foreground",
        )}
        title="Task list"
        aria-label={error ? "Task list: unavailable" : `Task list: ${done} of ${total} done`}
      >
        <CheckSquare size={11} />
        <span className="tabular-nums">
          {loading ? "…" : error ? "!" : `${done}/${total}`}
        </span>
      </button>

      {/* Popover */}
      {open && (
        <div
          ref={popoverRef}
          className="absolute bottom-full right-0 z-50 mb-2 w-80 rounded-lg border border-border bg-card shadow-xl"
          role="dialog"
          aria-label="Task list"
        >
          {/* Header */}
          <div className="flex items-center justify-between border-b border-border px-3 py-2">
            <span className="text-xs font-medium text-foreground">Tasks</span>
            <button
              type="button"
              onClick={() => setOpen(false)}
              className="text-muted-foreground hover:text-foreground"
              aria-label="Close task list"
            >
              <X size={14} />
            </button>
          </div>

          {error && <div role="alert" className="px-3 py-2 text-xs text-destructive">{error} <button type="button" className="underline" onClick={reload}>Retry loading tasks</button></div>}
          {/* Task list */}
          <fieldset disabled={!canEdit || loading}>
          <div className="max-h-64 overflow-y-auto">
            {tasks.length === 0 && !loading && !error && canEdit && (
              <div className="px-3 py-4 text-center text-xs text-muted-foreground">
                No tasks yet. Add one below.
              </div>
            )}
            {tasks.map((task) => (
              <TaskRow
                key={task.id}
                task={task}
                onToggle={() => toggleTask(task.id)}
                onRemove={() => removeTask(task.id)}
                onAnchorClick={handleAnchorClick}
              />
            ))}
          </div>

          {/* Add task input */}
          <div className="border-t border-border px-3 py-2">
            <div className="flex items-center gap-2">
              <Plus size={12} className="shrink-0 text-muted-foreground" />
              <Input
                ref={inputRef}
                type="text"
                value={newTitle}
                onChange={(e) => setNewTitle(e.target.value)}
                onKeyDown={handleKeyDown}
                placeholder="Add a task…"
                className="flex-1 bg-transparent text-xs text-foreground placeholder:text-muted-foreground outline-none"
                aria-label="New task title"
              />
              <button
                type="button"
                onClick={handleAdd}
                disabled={!canEdit || loading || !newTitle.trim()}
                className="text-xs text-primary hover:text-primary/80 disabled:opacity-40"
                aria-label="Add task"
              >
                Add
              </button>
            </div>
          </div>
          </fieldset>
        </div>
      )}
    </div>
  );
}

// ── Task row ────────────────────────────────────────────────────────

interface TaskRowProps {
  task: TaskItem;
  onToggle: () => void;
  onRemove: () => void;
  onAnchorClick: (anchor: string) => void;
}

function TaskRow({ task, onToggle, onRemove, onAnchorClick }: TaskRowProps) {
  const [hovered, setHovered] = useState(false);

  return (
    <div
      className="group flex items-center gap-2 px-3 py-1.5 hover:bg-background"
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
    >
      {/* Checkbox */}
      <button
        type="button"
        onClick={onToggle}
        className={clsx(
          "flex h-4 w-4 shrink-0 items-center justify-center rounded border",
          task.done
            ? "border-primary bg-primary text-primary-foreground"
            : "border-border hover:border-primary",
        )}
        aria-label={task.done ? `Mark "${task.title}" as not done` : `Mark "${task.title}" as done`}
      >
        {task.done && (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
            <path d="M2 5L4 7L8 3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        )}
      </button>

      {/* Title */}
      <span
        className={clsx(
          "flex-1 truncate text-xs",
          task.done ? "text-muted-foreground line-through" : "text-foreground",
        )}
        title={task.title}
      >
        {task.title}
      </span>

      {/* Anchor link */}
      {task.anchor && (
        <button
          type="button"
          onClick={() => onAnchorClick(task.anchor!)}
          className="flex items-center gap-0.5 text-[10px] text-primary hover:underline"
          title={task.anchor}
          aria-label={`Navigate to ${formatAnchor(task.anchor)}`}
        >
          <Link2 size={10} />
          <span>{formatAnchor(task.anchor)}</span>
        </button>
      )}

      {/* Delete button (visible on hover) */}
      <button
        type="button"
        onClick={onRemove}
        className={clsx(
          "shrink-0 text-muted-foreground hover:text-destructive",
          hovered ? "opacity-100" : "opacity-0",
        )}
        aria-label={`Delete task "${task.title}"`}
      >
        <Trash2 size={12} />
      </button>
    </div>
  );
}
