import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  Search,
  X,
  ChevronDown,
  Save,
  Trash2,
  Pencil,
  Check,
  Layers,
  LayoutGrid,
  User,
  Tag,
  AlertTriangle,
} from "lucide-react";
import type {
  ActiveFilters,
  DensityMode,
  GroupingMode,
  KanbanCardPriority,
  KanbanPipelineState,
  SavedView,
} from "../../ipc";
import type { FilterOptions } from "../../lib/kanban/filter-engine";
import { fuzzyMatchOptions } from "../../lib/kanban/filter-engine";

export interface FilterBarProps {
  filters: ActiveFilters;
  searchText: string;
  groupingMode: GroupingMode;
  densityMode: DensityMode;
  savedViews: SavedView[];
  activeViewId: string | null;
  visibleCount: number;
  totalCount: number;
  availableOptions: FilterOptions;
  onFiltersChange: (filters: ActiveFilters) => void;
  onSearchChange: (text: string) => void;
  onGroupingChange: (mode: GroupingMode) => void;
  onDensityChange: (mode: DensityMode) => void;
  onSaveView: (name: string) => void;
  onSelectView: (viewId: string) => void;
  onDeleteView: (viewId: string) => void;
  onRenameView: (viewId: string, name: string) => void;
  onClearFilters: () => void;
}

const PRIORITY_LABELS: Record<KanbanCardPriority, string> = {
  critical: "Critical",
  high: "High",
  medium: "Medium",
  low: "Low",
  none: "None",
};

const PIPELINE_LABELS: Record<KanbanPipelineState, string> = {
  idle: "Idle",
  agent_running: "Agent Running",
  pr_created: "PR Created",
  ci_pending: "CI Pending",
  ci_passed: "CI Passed",
  ci_failed: "CI Failed",
  review_pending: "Review Pending",
  review_approved: "Review Approved",
  review_changes_requested: "Changes Requested",
  merging: "Merging",
  done: "Done",
  blocked: "Blocked",
};

function hasActiveFilters(filters: ActiveFilters): boolean {
  return (
    filters.assignees.length > 0 ||
    filters.labels.length > 0 ||
    filters.priorities.length > 0 ||
    filters.pipelineStates.length > 0
  );
}

/**
 * A generic multi-select filter dropdown with fuzzy-match search.
 */
function FilterDropdown<T extends string>({
  label,
  options,
  selected,
  displayLabel,
  onChange,
}: {
  label: string;
  options: T[];
  selected: T[];
  displayLabel?: (value: T) => string;
  onChange: (selected: T[]) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const dropdownRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const filteredOptions = fuzzyMatchOptions(
    query,
    options.map((o) => displayLabel?.(o) ?? o),
  );

  // Map display labels back to values
  const filteredValues = filteredOptions.map((displayStr) => {
    const idx = options.findIndex((o) => (displayLabel?.(o) ?? o) === displayStr);
    return options[idx];
  });

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false);
        setQuery("");
      }
    }
    if (open) {
      document.addEventListener("mousedown", handleClickOutside);
      return () => document.removeEventListener("mousedown", handleClickOutside);
    }
  }, [open]);

  useEffect(() => {
    if (open && inputRef.current) {
      inputRef.current.focus();
    }
  }, [open]);

  const toggleValue = (value: T) => {
    if (selected.includes(value)) {
      onChange(selected.filter((v) => v !== value));
    } else {
      onChange([...selected, value]);
    }
  };

  return (
    <div className="relative" ref={dropdownRef}>
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={clsx(
          "btn flex items-center gap-1 text-xs",
          selected.length > 0 && "border-gx-accent text-gx-accent",
        )}
      >
        <span>{label}</span>
        {selected.length > 0 && (
          <span className="rounded-full bg-gx-accent/20 px-1.5 text-[10px] font-medium text-gx-accent">
            {selected.length}
          </span>
        )}
        <ChevronDown size={12} className="text-gx-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-50 mt-1 w-48 rounded-md border border-gx-border bg-gx-surface shadow-lg">
          <div className="border-b border-gx-border p-1.5">
            <input
              ref={inputRef}
              type="text"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={`Filter ${label.toLowerCase()}…`}
              className="input-field w-full"
            />
          </div>
          <div className="max-h-48 overflow-y-auto p-1">
            {filteredValues.length === 0 ? (
              <div className="px-2 py-1.5 text-xs text-gx-muted">No matches</div>
            ) : (
              filteredValues.map((value) => (
                <button
                  key={value}
                  type="button"
                  onClick={() => toggleValue(value)}
                  className={clsx(
                    "flex w-full items-center gap-2 rounded px-2 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover",
                    selected.includes(value) && "bg-gx-accent/10",
                  )}
                >
                  <span
                    className={clsx(
                      "flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-sm border",
                      selected.includes(value)
                        ? "border-gx-accent bg-gx-accent text-white"
                        : "border-gx-border",
                    )}
                  >
                    {selected.includes(value) && <Check size={10} />}
                  </span>
                  <span className="truncate">{displayLabel?.(value) ?? value}</span>
                </button>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Density toggle with three-mode icon selector.
 * Shows three horizontal-line icons with varying spacing.
 */
function DensityToggle({
  mode,
  onChange,
}: {
  mode: DensityMode;
  onChange: (mode: DensityMode) => void;
}) {
  const modes: DensityMode[] = ["compact", "comfortable", "spacious"];

  return (
    <div className="flex items-center rounded border border-gx-border">
      {modes.map((m) => (
        <button
          key={m}
          type="button"
          onClick={() => onChange(m)}
          title={m.charAt(0).toUpperCase() + m.slice(1)}
          className={clsx(
            "flex h-6 w-6 items-center justify-center transition-colors",
            mode === m ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:text-gx-text",
          )}
        >
          <DensityIcon mode={m} />
        </button>
      ))}
    </div>
  );
}

function DensityIcon({ mode }: { mode: DensityMode }) {
  // Three horizontal lines with varying spacing
  const gaps: Record<DensityMode, number> = {
    compact: 2,
    comfortable: 3.5,
    spacious: 5,
  };
  const gap = gaps[mode];
  const cy = 7; // center y
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
      <rect x="2" y={cy - gap} width="10" height="1.5" rx="0.5" fill="currentColor" />
      <rect x="2" y={cy} width="10" height="1.5" rx="0.5" fill="currentColor" />
      <rect x="2" y={cy + gap} width="10" height="1.5" rx="0.5" fill="currentColor" />
    </svg>
  );
}

/**
 * Saved views section with tabs and management actions.
 */
function SavedViewsSection({
  views,
  activeViewId,
  onSelect,
  onSave,
  onDelete,
  onRename,
}: {
  views: SavedView[];
  activeViewId: string | null;
  onSelect: (viewId: string) => void;
  onSave: (name: string) => void;
  onDelete: (viewId: string) => void;
  onRename: (viewId: string, name: string) => void;
}) {
  const [showSaveInput, setShowSaveInput] = useState(false);
  const [saveName, setSaveName] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editName, setEditName] = useState("");
  const [contextMenuId, setContextMenuId] = useState<string | null>(null);
  const [limitMessage, setLimitMessage] = useState(false);
  const saveInputRef = useRef<HTMLInputElement>(null);
  const editInputRef = useRef<HTMLInputElement>(null);
  const contextRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (showSaveInput && saveInputRef.current) {
      saveInputRef.current.focus();
    }
  }, [showSaveInput]);

  useEffect(() => {
    if (editingId && editInputRef.current) {
      editInputRef.current.focus();
      editInputRef.current.select();
    }
  }, [editingId]);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (contextRef.current && !contextRef.current.contains(e.target as Node)) {
        setContextMenuId(null);
      }
    }
    if (contextMenuId) {
      document.addEventListener("mousedown", handleClickOutside);
      return () => document.removeEventListener("mousedown", handleClickOutside);
    }
  }, [contextMenuId]);

  const handleSave = () => {
    const trimmed = saveName.trim();
    if (trimmed) {
      if (views.length >= 20) {
        setLimitMessage(true);
        setTimeout(() => setLimitMessage(false), 3000);
        return;
      }
      onSave(trimmed);
      setSaveName("");
      setShowSaveInput(false);
    }
  };

  const handleRename = (viewId: string) => {
    const trimmed = editName.trim();
    if (trimmed) {
      onRename(viewId, trimmed);
      setEditingId(null);
      setEditName("");
    }
  };

  return (
    <div className="flex items-center gap-1">
      {views.map((view) => (
        <div key={view.id} className="relative">
          {editingId === view.id ? (
            <input
              ref={editInputRef}
              type="text"
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleRename(view.id);
                if (e.key === "Escape") {
                  setEditingId(null);
                  setEditName("");
                }
              }}
              onBlur={() => {
                setEditingId(null);
                setEditName("");
              }}
              className="input-field w-20 text-[10px]"
            />
          ) : (
            <button
              type="button"
              onClick={() => onSelect(view.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                setContextMenuId(view.id);
              }}
              className={clsx(
                "rounded px-2 py-0.5 text-[10px] font-medium transition-colors",
                activeViewId === view.id
                  ? "bg-gx-accent/20 text-gx-accent"
                  : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
              )}
            >
              {view.name}
            </button>
          )}

          {contextMenuId === view.id && (
            <div
              ref={contextRef}
              className="absolute left-0 top-full z-50 mt-1 w-28 rounded-md border border-gx-border bg-gx-surface shadow-lg"
            >
              <button
                type="button"
                onClick={() => {
                  setEditingId(view.id);
                  setEditName(view.name);
                  setContextMenuId(null);
                }}
                className="flex w-full items-center gap-2 px-2 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover"
              >
                <Pencil size={10} />
                Rename
              </button>
              <button
                type="button"
                onClick={() => {
                  onDelete(view.id);
                  setContextMenuId(null);
                }}
                className="flex w-full items-center gap-2 px-2 py-1.5 text-xs text-gx-error hover:bg-gx-surface-hover"
              >
                <Trash2 size={10} />
                Delete
              </button>
            </div>
          )}
        </div>
      ))}

      {showSaveInput ? (
        <div className="flex items-center gap-1">
          <input
            ref={saveInputRef}
            type="text"
            value={saveName}
            onChange={(e) => setSaveName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleSave();
              if (e.key === "Escape") {
                setShowSaveInput(false);
                setSaveName("");
              }
            }}
            placeholder="View name…"
            className="input-field w-24 text-[10px]"
          />
          <button
            type="button"
            onClick={handleSave}
            className="btn-icon text-gx-accent"
            title="Save view"
          >
            <Check size={12} />
          </button>
          <button
            type="button"
            onClick={() => {
              setShowSaveInput(false);
              setSaveName("");
            }}
            className="btn-icon text-gx-muted"
            title="Cancel"
          >
            <X size={12} />
          </button>
        </div>
      ) : (
        <button
          type="button"
          onClick={() => {
            if (views.length >= 20) {
              setLimitMessage(true);
              setTimeout(() => setLimitMessage(false), 3000);
              return;
            }
            setShowSaveInput(true);
          }}
          className="btn flex items-center gap-1 text-[10px]"
          title="Save current view"
        >
          <Save size={10} />
          <span>Save</span>
        </button>
      )}

      {limitMessage && (
        <span className="text-[10px] text-gx-error">
          Maximum 20 views reached. Delete a view to save a new one.
        </span>
      )}
    </div>
  );
}

/**
 * Grouping mode selector dropdown.
 */
function GroupingSelector({
  mode,
  onChange,
}: {
  mode: GroupingMode;
  onChange: (mode: GroupingMode) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    if (open) {
      document.addEventListener("mousedown", handleClickOutside);
      return () => document.removeEventListener("mousedown", handleClickOutside);
    }
  }, [open]);

  const options: { value: GroupingMode; label: string; icon: React.ReactNode }[] = [
    { value: "default", label: "Default", icon: <LayoutGrid size={12} /> },
    { value: "priority", label: "Priority", icon: <AlertTriangle size={12} /> },
    { value: "label", label: "Label", icon: <Tag size={12} /> },
    { value: "assignee", label: "Assignee", icon: <User size={12} /> },
  ];

  const current = options.find((o) => o.value === mode);

  return (
    <div className="relative" ref={ref}>
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={clsx(
          "btn flex items-center gap-1 text-xs",
          mode !== "default" && "border-gx-accent text-gx-accent",
        )}
        title="Group by"
      >
        <Layers size={12} />
        <span>{current?.label ?? "Group"}</span>
        <ChevronDown size={12} className="text-gx-muted" />
      </button>

      {open && (
        <div className="absolute left-0 top-full z-50 mt-1 w-36 rounded-md border border-gx-border bg-gx-surface shadow-lg">
          <div className="p-1">
            {options.map((opt) => (
              <button
                key={opt.value}
                type="button"
                onClick={() => {
                  onChange(opt.value);
                  setOpen(false);
                }}
                className={clsx(
                  "flex w-full items-center gap-2 rounded px-2 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover",
                  mode === opt.value && "bg-gx-accent/10 text-gx-accent",
                )}
              >
                {opt.icon}
                <span>{opt.label}</span>
                {mode === opt.value && <Check size={10} className="ml-auto" />}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * FilterBar renders above the Board as a horizontal toolbar with filter dropdowns,
 * search input, saved views, grouping selector, and density toggle.
 */
export function FilterBar({
  filters,
  searchText,
  groupingMode,
  densityMode,
  savedViews,
  activeViewId,
  visibleCount,
  totalCount,
  availableOptions,
  onFiltersChange,
  onSearchChange,
  onGroupingChange,
  onDensityChange,
  onSaveView,
  onSelectView,
  onDeleteView,
  onRenameView,
  onClearFilters,
}: FilterBarProps) {
  const searchInputRef = useRef<HTMLInputElement>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [localSearch, setLocalSearch] = useState(searchText);

  // Sync external searchText changes
  useEffect(() => {
    setLocalSearch(searchText);
  }, [searchText]);

  // "/" keyboard shortcut to focus search
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      const target = e.target as HTMLElement;
      const isInput =
        target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.isContentEditable;

      if (e.key === "/" && !isInput) {
        e.preventDefault();
        searchInputRef.current?.focus();
      }
    }
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, []);

  const handleSearchChange = useCallback(
    (value: string) => {
      setLocalSearch(value);
      if (debounceRef.current) {
        clearTimeout(debounceRef.current);
      }
      debounceRef.current = setTimeout(() => {
        onSearchChange(value);
      }, 100);
    },
    [onSearchChange],
  );

  // Cleanup debounce on unmount
  useEffect(() => {
    return () => {
      if (debounceRef.current) {
        clearTimeout(debounceRef.current);
      }
    };
  }, []);

  const filtersActive = hasActiveFilters(filters) || searchText.length > 0;

  const handleAssigneesChange = useCallback(
    (assignees: string[]) => {
      onFiltersChange({ ...filters, assignees });
    },
    [filters, onFiltersChange],
  );

  const handleLabelsChange = useCallback(
    (labels: string[]) => {
      onFiltersChange({ ...filters, labels });
    },
    [filters, onFiltersChange],
  );

  const handlePrioritiesChange = useCallback(
    (priorities: KanbanCardPriority[]) => {
      onFiltersChange({ ...filters, priorities });
    },
    [filters, onFiltersChange],
  );

  const handlePipelineStatesChange = useCallback(
    (pipelineStates: KanbanPipelineState[]) => {
      onFiltersChange({ ...filters, pipelineStates });
    },
    [filters, onFiltersChange],
  );

  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-gx-border px-3 py-2">
      {/* Search input */}
      <div className="relative flex items-center">
        <Search size={12} className="absolute left-2 text-gx-muted" />
        <input
          ref={searchInputRef}
          type="text"
          value={localSearch}
          onChange={(e) => handleSearchChange(e.target.value)}
          placeholder="Search cards…"
          className="input-field w-40 pl-6 pr-6"
        />
        {localSearch && (
          <button
            type="button"
            onClick={() => handleSearchChange("")}
            className="absolute right-1.5 text-gx-muted hover:text-gx-text"
          >
            <X size={12} />
          </button>
        )}
        <kbd className="kbd ml-1">/</kbd>
      </div>

      {/* Filter dropdowns */}
      <FilterDropdown
        label="Assignee"
        options={availableOptions.assignees}
        selected={filters.assignees}
        onChange={handleAssigneesChange}
      />
      <FilterDropdown
        label="Label"
        options={availableOptions.labels}
        selected={filters.labels}
        onChange={handleLabelsChange}
      />
      <FilterDropdown<KanbanCardPriority>
        label="Priority"
        options={availableOptions.priorities}
        selected={filters.priorities}
        displayLabel={(p) => PRIORITY_LABELS[p]}
        onChange={handlePrioritiesChange}
      />
      <FilterDropdown<KanbanPipelineState>
        label="State"
        options={availableOptions.pipelineStates}
        selected={filters.pipelineStates}
        displayLabel={(s) => PIPELINE_LABELS[s]}
        onChange={handlePipelineStatesChange}
      />

      {/* Visible/total count and clear */}
      {filtersActive && (
        <div className="flex items-center gap-2">
          <span className="text-xs text-gx-muted">
            {visibleCount} of {totalCount}
          </span>
          <button
            type="button"
            onClick={onClearFilters}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
          >
            <X size={10} />
            Clear all
          </button>
        </div>
      )}

      {/* Spacer */}
      <div className="flex-1" />

      {/* Saved views */}
      <SavedViewsSection
        views={savedViews}
        activeViewId={activeViewId}
        onSelect={onSelectView}
        onSave={onSaveView}
        onDelete={onDeleteView}
        onRename={onRenameView}
      />

      {/* Separator */}
      <div className="h-4 w-px bg-gx-border" />

      {/* Grouping selector */}
      <GroupingSelector mode={groupingMode} onChange={onGroupingChange} />

      {/* Density toggle */}
      <DensityToggle mode={densityMode} onChange={onDensityChange} />
    </div>
  );
}
