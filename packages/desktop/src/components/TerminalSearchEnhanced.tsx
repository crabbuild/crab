import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/**
 * Enhanced terminal search bar with:
 * - Find and replace (Task 26.3)
 * - Timestamp/time-range filtering (Task 26.6)
 * - Semantic output filtering (Task 27.3)
 */

import clsx from "clsx";
import {
  AlertCircle,
  AlertTriangle,
  ChevronDown,
  ChevronUp,
  Clock,
  FileText,
  Link,
  Replace,
  Search,
  X,
} from "lucide-react";
import { useState } from "react";
import type { SemanticFilter } from "../lib/terminal-ux-enhancements";

interface Props {
  query: string;
  onQueryChange: (q: string) => void;
  caseSensitive: boolean;
  onCaseSensitiveChange: (v: boolean) => void;
  regex: boolean;
  onRegexChange: (v: boolean) => void;
  onNext: () => void;
  onPrev: () => void;
  onClose: () => void;
  inputRef: React.RefObject<HTMLInputElement>;
  // Replace functionality (Task 26.3)
  onReplace?: (replacement: string) => void;
  onReplaceAll?: (replacement: string) => void;
  // Time filter (Task 26.6)
  onTimeFilter?: (minutes: number | null) => void;
  activeTimeFilter?: number | null;
  // Semantic filter (Task 27.3)
  onSemanticFilter?: (filter: SemanticFilter) => void;
  activeSemanticFilter?: SemanticFilter;
}

const TIME_PRESETS = [
  { label: "1m", minutes: 1 },
  { label: "5m", minutes: 5 },
  { label: "15m", minutes: 15 },
  { label: "30m", minutes: 30 },
  { label: "1h", minutes: 60 },
];

export function TerminalSearchEnhanced({
  query,
  onQueryChange,
  caseSensitive,
  onCaseSensitiveChange,
  regex,
  onRegexChange,
  onNext,
  onPrev,
  onClose,
  inputRef,
  onReplace,
  onReplaceAll,
  onTimeFilter,
  activeTimeFilter,
  onSemanticFilter,
  activeSemanticFilter = "all",
}: Props) {
  const [showReplace, setShowReplace] = useState(false);
  const [replacement, setReplacement] = useState("");
  const [showTimeFilter, setShowTimeFilter] = useState(false);

  return (
    <div className="shrink-0 border-b border-border bg-card px-3 py-1.5">
      {/* Main search row */}
      <div className="flex items-center gap-2">
        <Search size={12} className="text-muted-foreground shrink-0" />
        <Input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => onQueryChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") { e.shiftKey ? onPrev() : onNext(); }
            if (e.key === "Escape") onClose();
          }}
          placeholder="Search terminal…"
          className="flex-1 bg-transparent text-[12px] text-foreground placeholder:text-muted-foreground outline-none"
          autoFocus
        />

        {/* Toggle buttons */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => onCaseSensitiveChange(!caseSensitive)}
          className={clsx(
            "rounded px-1.5 py-0.5 text-[10px] font-medium",
            caseSensitive ? "bg-primary/20 text-primary" : "text-muted-foreground hover:text-foreground",
          )}
          title="Match case"
        >
          Aa
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => onRegexChange(!regex)}
          className={clsx(
            "rounded px-1.5 py-0.5 text-[10px] font-medium",
            regex ? "bg-primary/20 text-primary" : "text-muted-foreground hover:text-foreground",
          )}
          title="Use regex"
        >
          .*
        </Button>

        {/* Replace toggle */}
        {onReplace && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setShowReplace(!showReplace)}
            className={clsx(
              "rounded px-1 py-0.5",
              showReplace ? "text-primary" : "text-muted-foreground hover:text-foreground",
            )}
            title="Find and Replace"
          >
            <Replace size={12} />
          </Button>
        )}

        {/* Time filter toggle */}
        {onTimeFilter && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setShowTimeFilter(!showTimeFilter)}
            className={clsx(
              "rounded px-1 py-0.5",
              showTimeFilter || activeTimeFilter ? "text-primary" : "text-muted-foreground hover:text-foreground",
            )}
            title="Filter by time"
          >
            <Clock size={12} />
          </Button>
        )}

        {/* Navigation */}
        <Button
          variant="ghost"
          size="compact"
          type="button" onClick={onPrev} className="text-muted-foreground hover:text-foreground" title="Previous (Shift+Enter)">
          <ChevronUp size={14} />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button" onClick={onNext} className="text-muted-foreground hover:text-foreground" title="Next (Enter)">
          <ChevronDown size={14} />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button" onClick={onClose} className="text-muted-foreground hover:text-foreground" title="Close (Esc)">
          <X size={14} />
        </Button>
      </div>

      {/* Replace row (Task 26.3) */}
      {showReplace && onReplace && (
        <div className="mt-1.5 flex items-center gap-2 pl-5">
          <Replace size={10} className="text-muted-foreground shrink-0" />
          <Input
            type="text"
            value={replacement}
            onChange={(e) => setReplacement(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") onReplace(replacement);
              if (e.key === "Escape") setShowReplace(false);
            }}
            placeholder="Replace with…"
            className="flex-1 bg-transparent text-[12px] text-foreground placeholder:text-muted-foreground outline-none"
          />
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => onReplace(replacement)}
            className="rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent"
            title="Replace next"
          >
            Replace
          </Button>
          {onReplaceAll && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onReplaceAll(replacement)}
              className="rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent"
              title="Replace all"
            >
              All
            </Button>
          )}
        </div>
      )}

      {/* Time filter row (Task 26.6) */}
      {showTimeFilter && onTimeFilter && (
        <div className="mt-1.5 flex items-center gap-1.5 pl-5">
          <span className="text-[10px] text-muted-foreground">Show last:</span>
          {TIME_PRESETS.map((preset) => (
            <Button
              variant="ghost"
              size="compact"
              key={preset.minutes}
              type="button"
              onClick={() => onTimeFilter(activeTimeFilter === preset.minutes ? null : preset.minutes)}
              className={clsx(
                "rounded px-1.5 py-0.5 text-[10px]",
                activeTimeFilter === preset.minutes
                  ? "bg-primary/20 text-primary"
                  : "text-muted-foreground hover:text-foreground hover:bg-accent",
              )}
            >
              {preset.label}
            </Button>
          ))}
          {activeTimeFilter && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => onTimeFilter(null)}
              className="rounded px-1.5 py-0.5 text-[9px] text-muted-foreground hover:text-destructive"
              title="Clear time filter"
            >
              ✕
            </Button>
          )}
        </div>
      )}

      {/* Semantic filter buttons (Task 27.3) */}
      {onSemanticFilter && (
        <div className="mt-1.5 flex items-center gap-1.5 pl-5">
          <span className="text-[10px] text-muted-foreground">Filter:</span>
          <FilterButton
            active={activeSemanticFilter === "errors"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "errors" ? "all" : "errors")}
            icon={<AlertCircle size={10} />}
            label="Errors"
            color="text-destructive"
          />
          <FilterButton
            active={activeSemanticFilter === "warnings"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "warnings" ? "all" : "warnings")}
            icon={<AlertTriangle size={10} />}
            label="Warnings"
            color="text-warning"
          />
          <FilterButton
            active={activeSemanticFilter === "paths"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "paths" ? "all" : "paths")}
            icon={<FileText size={10} />}
            label="Paths"
            color="text-info"
          />
          <FilterButton
            active={activeSemanticFilter === "urls"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "urls" ? "all" : "urls")}
            icon={<Link size={10} />}
            label="URLs"
            color="text-info"
          />
        </div>
      )}
    </div>
  );
}

function FilterButton({
  active,
  onClick,
  icon,
  label,
  color,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  color: string;
}) {
  return (
    <Button
      variant="ghost"
      size="compact"
      type="button"
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] transition",
        active
          ? `bg-accent ${color}`
          : "text-muted-foreground hover:text-foreground hover:bg-accent",
      )}
      title={label}
    >
      {icon}
      <span>{label}</span>
    </Button>
  );
}
