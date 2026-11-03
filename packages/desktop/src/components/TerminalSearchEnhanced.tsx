/**
 * Enhanced terminal search bar with:
 * - Find and replace (Task 26.3)
 * - Timestamp/time-range filtering (Task 26.6)
 * - Semantic output filtering (Task 27.3)
 */

import { useState } from "react";
import {
  Search,
  ChevronUp,
  ChevronDown,
  X,
  Replace,
  Clock,
  AlertCircle,
  AlertTriangle,
  FileText,
  Link,
} from "lucide-react";
import clsx from "clsx";
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
    <div className="shrink-0 border-b border-gx-border bg-gx-surface px-3 py-1.5">
      {/* Main search row */}
      <div className="flex items-center gap-2">
        <Search size={12} className="text-gx-muted shrink-0" />
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => onQueryChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") { e.shiftKey ? onPrev() : onNext(); }
            if (e.key === "Escape") onClose();
          }}
          placeholder="Search terminal…"
          className="flex-1 bg-transparent text-[12px] text-gx-text placeholder:text-gx-muted outline-none"
          autoFocus
        />

        {/* Toggle buttons */}
        <button
          type="button"
          onClick={() => onCaseSensitiveChange(!caseSensitive)}
          className={clsx(
            "rounded px-1.5 py-0.5 text-[10px] font-medium",
            caseSensitive ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:text-gx-text",
          )}
          title="Match case"
        >
          Aa
        </button>
        <button
          type="button"
          onClick={() => onRegexChange(!regex)}
          className={clsx(
            "rounded px-1.5 py-0.5 text-[10px] font-medium",
            regex ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:text-gx-text",
          )}
          title="Use regex"
        >
          .*
        </button>

        {/* Replace toggle */}
        {onReplace && (
          <button
            type="button"
            onClick={() => setShowReplace(!showReplace)}
            className={clsx(
              "rounded px-1 py-0.5",
              showReplace ? "text-gx-accent" : "text-gx-muted hover:text-gx-text",
            )}
            title="Find and Replace"
          >
            <Replace size={12} />
          </button>
        )}

        {/* Time filter toggle */}
        {onTimeFilter && (
          <button
            type="button"
            onClick={() => setShowTimeFilter(!showTimeFilter)}
            className={clsx(
              "rounded px-1 py-0.5",
              showTimeFilter || activeTimeFilter ? "text-gx-accent" : "text-gx-muted hover:text-gx-text",
            )}
            title="Filter by time"
          >
            <Clock size={12} />
          </button>
        )}

        {/* Navigation */}
        <button type="button" onClick={onPrev} className="text-gx-muted hover:text-gx-text" title="Previous (Shift+Enter)">
          <ChevronUp size={14} />
        </button>
        <button type="button" onClick={onNext} className="text-gx-muted hover:text-gx-text" title="Next (Enter)">
          <ChevronDown size={14} />
        </button>
        <button type="button" onClick={onClose} className="text-gx-muted hover:text-gx-text" title="Close (Esc)">
          <X size={14} />
        </button>
      </div>

      {/* Replace row (Task 26.3) */}
      {showReplace && onReplace && (
        <div className="mt-1.5 flex items-center gap-2 pl-5">
          <Replace size={10} className="text-gx-muted shrink-0" />
          <input
            type="text"
            value={replacement}
            onChange={(e) => setReplacement(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") onReplace(replacement);
              if (e.key === "Escape") setShowReplace(false);
            }}
            placeholder="Replace with…"
            className="flex-1 bg-transparent text-[12px] text-gx-text placeholder:text-gx-muted outline-none"
          />
          <button
            type="button"
            onClick={() => onReplace(replacement)}
            className="rounded px-2 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title="Replace next"
          >
            Replace
          </button>
          {onReplaceAll && (
            <button
              type="button"
              onClick={() => onReplaceAll(replacement)}
              className="rounded px-2 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
              title="Replace all"
            >
              All
            </button>
          )}
        </div>
      )}

      {/* Time filter row (Task 26.6) */}
      {showTimeFilter && onTimeFilter && (
        <div className="mt-1.5 flex items-center gap-1.5 pl-5">
          <span className="text-[10px] text-gx-muted">Show last:</span>
          {TIME_PRESETS.map((preset) => (
            <button
              key={preset.minutes}
              type="button"
              onClick={() => onTimeFilter(activeTimeFilter === preset.minutes ? null : preset.minutes)}
              className={clsx(
                "rounded px-1.5 py-0.5 text-[10px]",
                activeTimeFilter === preset.minutes
                  ? "bg-gx-accent/20 text-gx-accent"
                  : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
              )}
            >
              {preset.label}
            </button>
          ))}
          {activeTimeFilter && (
            <button
              type="button"
              onClick={() => onTimeFilter(null)}
              className="rounded px-1.5 py-0.5 text-[9px] text-gx-muted hover:text-gx-error"
              title="Clear time filter"
            >
              ✕
            </button>
          )}
        </div>
      )}

      {/* Semantic filter buttons (Task 27.3) */}
      {onSemanticFilter && (
        <div className="mt-1.5 flex items-center gap-1.5 pl-5">
          <span className="text-[10px] text-gx-muted">Filter:</span>
          <FilterButton
            active={activeSemanticFilter === "errors"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "errors" ? "all" : "errors")}
            icon={<AlertCircle size={10} />}
            label="Errors"
            color="text-red-400"
          />
          <FilterButton
            active={activeSemanticFilter === "warnings"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "warnings" ? "all" : "warnings")}
            icon={<AlertTriangle size={10} />}
            label="Warnings"
            color="text-yellow-400"
          />
          <FilterButton
            active={activeSemanticFilter === "paths"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "paths" ? "all" : "paths")}
            icon={<FileText size={10} />}
            label="Paths"
            color="text-blue-400"
          />
          <FilterButton
            active={activeSemanticFilter === "urls"}
            onClick={() => onSemanticFilter(activeSemanticFilter === "urls" ? "all" : "urls")}
            icon={<Link size={10} />}
            label="URLs"
            color="text-cyan-400"
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
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] transition",
        active
          ? `bg-gx-surface-hover ${color}`
          : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
      )}
      title={label}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}
