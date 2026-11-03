/**
 * Task Search — Filter/search across all tasks by title, status, or agent type.
 */

import { type RefObject, useCallback, useState } from "react";
import { Search, X } from "lucide-react";
import type { TaskStatus } from "./TaskCard";
import { applyTaskFilter, type TaskFilter } from "../../lib/orchestrator-sidebar";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  onFilterChange: (filter: TaskFilter) => void;
  compact?: boolean;
  inputRef?: RefObject<HTMLInputElement>;
}

// ── Component ───────────────────────────────────────────────────────

export function TaskSearch({ onFilterChange, compact, inputRef }: Props) {
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<TaskStatus | "all">("all");

  const handleQueryChange = useCallback(
    (value: string) => {
      setQuery(value);
      onFilterChange({ query: value, status });
    },
    [status, onFilterChange],
  );

  const handleStatusChange = useCallback(
    (value: TaskStatus | "all") => {
      setStatus(value);
      onFilterChange({ query, status: value });
    },
    [query, onFilterChange],
  );

  const handleClear = useCallback(() => {
    setQuery("");
    setStatus("all");
    onFilterChange({ query: "", status: "all" });
  }, [onFilterChange]);

  if (compact) {
    return (
      <button
        type="button"
        onClick={() => {
          // In compact mode, just show a search icon that could expand
        }}
        className="flex h-7 w-7 items-center justify-center rounded text-gx-muted hover:text-gx-text transition"
        aria-label="Search tasks"
      >
        <Search size={12} />
      </button>
    );
  }

  return (
    <div className="space-y-1.5 px-2 pb-2">
      {/* Search input */}
      <div className="relative">
        <Search size={11} className="absolute left-2 top-1/2 -translate-y-1/2 text-gx-muted" />
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => handleQueryChange(e.target.value)}
          placeholder="Search tasks…"
          className="w-full rounded border border-gx-border bg-gx-bg pl-6 pr-6 py-1 text-[11px] text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
          aria-label="Search tasks by title"
        />
        {(query || status !== "all") && (
          <button
            type="button"
            onClick={handleClear}
            className="absolute right-1.5 top-1/2 -translate-y-1/2 text-gx-muted hover:text-gx-text"
            aria-label="Clear search"
          >
            <X size={11} />
          </button>
        )}
      </div>

      {/* Status filter */}
      <select
        value={status}
        onChange={(e) => handleStatusChange(e.target.value as TaskStatus | "all")}
        className="w-full rounded border border-gx-border bg-gx-bg px-2 py-1 text-[10px] text-gx-text focus:border-gx-accent focus:outline-none"
        aria-label="Filter by status"
      >
        <option value="all">All statuses</option>
        <option value="running">Running</option>
        <option value="planning">Planning</option>
        <option value="reviewing">Reviewing</option>
        <option value="iterating">Iterating</option>
        <option value="queued">Queued</option>
        <option value="applied">Applied</option>
        <option value="archived">Archived</option>
        <option value="failed">Failed</option>
      </select>
    </div>
  );
}

export { applyTaskFilter };
export type { TaskFilter };
