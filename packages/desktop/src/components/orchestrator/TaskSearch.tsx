import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
/**
 * Task Search — Filter/search across all tasks by title, status, or agent type.
 */

import { Search, X } from "lucide-react";
import { type RefObject, useCallback, useState } from "react";
import { applyTaskFilter, type TaskFilter } from "../../lib/orchestrator-sidebar";
import type { TaskStatus } from "./TaskCard";

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
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => {
          // In compact mode, just show a search icon that could expand
        }}
        className="flex h-7 w-7 items-center justify-center rounded text-muted-foreground hover:text-foreground transition"
        aria-label="Search tasks"
      >
        <Search size={12} />
      </Button>
    );
  }

  return (
    <div className="space-y-1.5 px-2 pb-2">
      {/* Search input */}
      <div className="relative">
        <Search size={11} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted-foreground" />
        <Input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => handleQueryChange(e.target.value)}
          placeholder="Search tasks…"
          className="w-full rounded border border-border bg-background pl-6 pr-6 py-1 text-[11px] text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
          aria-label="Search tasks by title"
        />
        {(query || status !== "all") && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleClear}
            className="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            aria-label="Clear search"
          >
            <X size={11} />
          </Button>
        )}
      </div>

      {/* Status filter */}
      <Select
        value={":" + String(status)}
        onValueChange={(selectedValue) => {
          const value = selectedValue.slice(1);
          return handleStatusChange(value as TaskStatus | "all");
        }}
      >
        <SelectTrigger
          className="w-full rounded border border-border bg-background px-2 py-1 text-[10px] text-foreground focus:border-primary focus:outline-none"
        aria-label="Filter by status"
      >
        <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectGroup>
            <SelectItem value={":" + String("all")}>All statuses</SelectItem>
            <SelectItem value={":" + String("running")}>Running</SelectItem>
            <SelectItem value={":" + String("planning")}>Planning</SelectItem>
            <SelectItem value={":" + String("reviewing")}>Reviewing</SelectItem>
            <SelectItem value={":" + String("iterating")}>Iterating</SelectItem>
            <SelectItem value={":" + String("queued")}>Queued</SelectItem>
            <SelectItem value={":" + String("applied")}>Applied</SelectItem>
            <SelectItem value={":" + String("archived")}>Archived</SelectItem>
            <SelectItem value={":" + String("failed")}>Failed</SelectItem>
          </SelectGroup>
        </SelectContent>
      </Select>
    </div>
  );
}

export { applyTaskFilter };
export type { TaskFilter };
