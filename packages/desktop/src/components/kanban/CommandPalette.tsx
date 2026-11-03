import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import Fuse from "fuse.js";
import { Search, FileText, Columns, Zap, Keyboard } from "lucide-react";
import type { KanbanCardInfo, KanbanColumnInfo } from "../../ipc";

export interface PaletteAction {
  id: string;
  label: string;
  category: "cards" | "columns" | "actions" | "shortcuts";
  icon?: string;
  shortcut?: string;
  execute: () => void;
}

export interface CommandPaletteProps {
  isOpen: boolean;
  cards: KanbanCardInfo[];
  columns: KanbanColumnInfo[];
  onClose: () => void;
  onSelectCard: (cardId: string) => void;
  onScrollToColumn: (columnId: string) => void;
  onExecuteAction: (action: PaletteAction) => void;
}

interface PaletteItem {
  id: string;
  label: string;
  category: "cards" | "columns" | "actions" | "shortcuts";
  shortcut?: string;
  action: PaletteAction;
}

const CATEGORY_LABELS: Record<PaletteItem["category"], string> = {
  cards: "Cards",
  columns: "Columns",
  actions: "Actions",
  shortcuts: "Shortcuts",
};

const CATEGORY_ICONS: Record<PaletteItem["category"], React.ReactNode> = {
  cards: <FileText className="w-3 h-3" />,
  columns: <Columns className="w-3 h-3" />,
  actions: <Zap className="w-3 h-3" />,
  shortcuts: <Keyboard className="w-3 h-3" />,
};

/** Default board actions available in the command palette. */
function buildBoardActions(onExecuteAction: (action: PaletteAction) => void): PaletteItem[] {
  const actions: Array<Omit<PaletteAction, "execute"> & { execute?: () => void }> = [
    { id: "action:create-card", label: "Create new card", category: "actions", shortcut: "C" },
    { id: "action:toggle-density-compact", label: "Set density: Compact", category: "actions" },
    { id: "action:toggle-density-comfortable", label: "Set density: Comfortable", category: "actions" },
    { id: "action:toggle-density-spacious", label: "Set density: Spacious", category: "actions" },
    { id: "action:group-default", label: "Group by: Status (default)", category: "actions" },
    { id: "action:group-priority", label: "Group by: Priority", category: "actions" },
    { id: "action:group-label", label: "Group by: Label", category: "actions" },
    { id: "action:group-assignee", label: "Group by: Assignee", category: "actions" },
  ];

  const shortcuts: Array<Omit<PaletteAction, "execute"> & { execute?: () => void }> = [
    { id: "shortcut:create", label: "Create card", category: "shortcuts", shortcut: "C" },
    { id: "shortcut:delete", label: "Delete card", category: "shortcuts", shortcut: "D" },
    { id: "shortcut:edit", label: "Edit title", category: "shortcuts", shortcut: "E" },
    { id: "shortcut:open", label: "Open detail panel", category: "shortcuts", shortcut: "Enter" },
    { id: "shortcut:select", label: "Toggle selection", category: "shortcuts", shortcut: "X" },
    { id: "shortcut:search", label: "Focus search", category: "shortcuts", shortcut: "/" },
    { id: "shortcut:palette", label: "Command palette", category: "shortcuts", shortcut: "⌘K" },
    { id: "shortcut:priority-critical", label: "Set priority: Critical", category: "shortcuts", shortcut: "1" },
    { id: "shortcut:priority-high", label: "Set priority: High", category: "shortcuts", shortcut: "2" },
    { id: "shortcut:priority-medium", label: "Set priority: Medium", category: "shortcuts", shortcut: "3" },
    { id: "shortcut:priority-low", label: "Set priority: Low", category: "shortcuts", shortcut: "4" },
    { id: "shortcut:priority-none", label: "Set priority: None", category: "shortcuts", shortcut: "0" },
  ];

  return [...actions, ...shortcuts].map((item) => {
    const paletteAction: PaletteAction = {
      id: item.id,
      label: item.label,
      category: item.category,
      shortcut: item.shortcut,
      execute: () => onExecuteAction({
        id: item.id,
        label: item.label,
        category: item.category,
        shortcut: item.shortcut,
        execute: () => {},
      }),
    };
    return {
      id: item.id,
      label: item.label,
      category: item.category,
      shortcut: item.shortcut,
      action: paletteAction,
    };
  });
}

/**
 * Command palette modal with fuzzy search across cards, columns, actions, and shortcuts.
 * Triggered by Cmd+K / Ctrl+K. Uses fuse.js for sub-50ms fuzzy matching.
 */
export function CommandPalette({
  isOpen,
  cards,
  columns,
  onClose,
  onSelectCard,
  onScrollToColumn,
  onExecuteAction,
}: CommandPaletteProps) {
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const backdropRef = useRef<HTMLDivElement>(null);

  // Build the full index of searchable items
  const allItems = useMemo<PaletteItem[]>(() => {
    const cardItems: PaletteItem[] = cards.map((card) => ({
      id: `card:${card.id}`,
      label: card.title,
      category: "cards" as const,
      action: {
        id: `card:${card.id}`,
        label: card.title,
        category: "cards" as const,
        execute: () => onSelectCard(card.id),
      },
    }));

    const columnItems: PaletteItem[] = columns.map((col) => ({
      id: `column:${col.id}`,
      label: col.name,
      category: "columns" as const,
      action: {
        id: `column:${col.id}`,
        label: col.name,
        category: "columns" as const,
        execute: () => onScrollToColumn(col.id),
      },
    }));

    const boardActions = buildBoardActions(onExecuteAction);

    return [...cardItems, ...columnItems, ...boardActions];
  }, [cards, columns, onSelectCard, onScrollToColumn, onExecuteAction]);

  // Create fuse instance for fuzzy search
  const fuse = useMemo(
    () =>
      new Fuse(allItems, {
        keys: ["label"],
        threshold: 0.4,
        includeScore: true,
        shouldSort: true,
        minMatchCharLength: 1,
      }),
    [allItems],
  );

  // Compute results based on query
  const results = useMemo<PaletteItem[]>(() => {
    if (!query.trim()) {
      // Show a default set: first few from each category
      const grouped: PaletteItem[] = [];
      const categories: PaletteItem["category"][] = ["actions", "columns", "cards", "shortcuts"];
      for (const cat of categories) {
        const items = allItems.filter((item) => item.category === cat);
        grouped.push(...items.slice(0, 3));
      }
      return grouped.slice(0, 10);
    }
    const fuseResults = fuse.search(query);
    return fuseResults.slice(0, 10).map((r) => r.item);
  }, [query, fuse, allItems]);

  // Group results by category for section headers
  const groupedResults = useMemo(() => {
    const groups: Array<{ category: PaletteItem["category"]; items: PaletteItem[] }> = [];
    const seen = new Set<PaletteItem["category"]>();

    for (const item of results) {
      if (!seen.has(item.category)) {
        seen.add(item.category);
        groups.push({ category: item.category, items: [] });
      }
      const group = groups.find((g) => g.category === item.category);
      if (group) group.items.push(item);
    }

    return groups;
  }, [results]);

  // Flatten grouped results for index-based navigation
  const flatResults = useMemo(() => results, [results]);

  // Reset state when opening
  useEffect(() => {
    if (isOpen) {
      setQuery("");
      setSelectedIndex(0);
      // Focus input after render
      requestAnimationFrame(() => {
        inputRef.current?.focus();
      });
    }
  }, [isOpen]);

  // Keep selected index in bounds
  useEffect(() => {
    if (selectedIndex >= flatResults.length) {
      setSelectedIndex(Math.max(0, flatResults.length - 1));
    }
  }, [flatResults.length, selectedIndex]);

  // Scroll selected item into view
  useEffect(() => {
    if (!listRef.current) return;
    const selected = listRef.current.querySelector(`[data-index="${selectedIndex}"]`);
    if (selected) {
      selected.scrollIntoView({ block: "nearest" });
    }
  }, [selectedIndex]);

  const executeSelected = useCallback(() => {
    const item = flatResults[selectedIndex];
    if (!item) return;
    item.action.execute();
    onClose();
  }, [flatResults, selectedIndex, onClose]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          setSelectedIndex((prev) => Math.min(prev + 1, flatResults.length - 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setSelectedIndex((prev) => Math.max(prev - 1, 0));
          break;
        case "Enter":
          e.preventDefault();
          executeSelected();
          break;
        case "Escape":
          e.preventDefault();
          onClose();
          break;
      }
    },
    [flatResults.length, executeSelected, onClose],
  );

  const handleBackdropClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.target === backdropRef.current) {
        onClose();
      }
    },
    [onClose],
  );

  const handleItemClick = useCallback(
    (index: number) => {
      const item = flatResults[index];
      if (!item) return;
      item.action.execute();
      onClose();
    },
    [flatResults, onClose],
  );

  if (!isOpen) return null;

  // Build a flat index counter for rendering
  let flatIndex = 0;

  return (
    <div
      ref={backdropRef}
      className="fixed inset-0 z-50 flex items-start justify-center pt-[20vh] bg-black/50"
      onClick={handleBackdropClick}
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
    >
      <div
        className={clsx(
          "w-full max-w-[560px] bg-gx-surface border border-gx-border rounded-lg shadow-xl",
          "flex flex-col overflow-hidden",
        )}
        onKeyDown={handleKeyDown}
      >
        {/* Search input */}
        <div className="flex items-center gap-2 px-3 py-2 border-b border-gx-border">
          <Search className="w-4 h-4 text-gx-muted flex-shrink-0" />
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setSelectedIndex(0);
            }}
            placeholder="Search cards, columns, actions…"
            className={clsx(
              "flex-1 bg-transparent text-sm text-gx-text placeholder:text-gx-muted",
              "outline-none border-none",
            )}
            aria-label="Command palette search"
            aria-activedescendant={
              flatResults[selectedIndex]
                ? `palette-item-${flatResults[selectedIndex].id}`
                : undefined
            }
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-results"
            aria-autocomplete="list"
          />
          <kbd className="kbd text-[10px]">Esc</kbd>
        </div>

        {/* Results list */}
        <div
          ref={listRef}
          id="palette-results"
          role="listbox"
          className="max-h-[360px] overflow-y-auto py-1"
        >
          {flatResults.length === 0 && query.trim() && (
            <div className="px-3 py-6 text-center text-xs text-gx-muted">
              No results for &ldquo;{query}&rdquo;
            </div>
          )}

          {groupedResults.map((group) => (
            <div key={group.category}>
              {/* Category header */}
              <div className="flex items-center gap-1.5 px-3 py-1 text-[10px] font-medium text-gx-muted uppercase tracking-wide">
                {CATEGORY_ICONS[group.category]}
                {CATEGORY_LABELS[group.category]}
              </div>

              {/* Items in this category */}
              {group.items.map((item) => {
                const currentIndex = flatIndex++;
                return (
                  <div
                    key={item.id}
                    id={`palette-item-${item.id}`}
                    data-index={currentIndex}
                    role="option"
                    aria-selected={currentIndex === selectedIndex}
                    className={clsx(
                      "flex items-center gap-2 px-3 py-1.5 mx-1 rounded cursor-pointer text-sm",
                      currentIndex === selectedIndex
                        ? "bg-gx-accent/10 text-gx-text"
                        : "text-gx-text hover:bg-gx-surface-hover",
                    )}
                    onClick={() => handleItemClick(currentIndex)}
                    onMouseEnter={() => setSelectedIndex(currentIndex)}
                  >
                    <span className="flex-1 truncate">{item.label}</span>
                    {item.shortcut && (
                      <kbd className="kbd text-[10px] flex-shrink-0">{item.shortcut}</kbd>
                    )}
                  </div>
                );
              })}
            </div>
          ))}
        </div>

        {/* Footer hint */}
        <div className="flex items-center gap-3 px-3 py-1.5 border-t border-gx-border text-[10px] text-gx-muted">
          <span className="flex items-center gap-1">
            <kbd className="kbd text-[10px]">↑↓</kbd> navigate
          </span>
          <span className="flex items-center gap-1">
            <kbd className="kbd text-[10px]">↵</kbd> select
          </span>
          <span className="flex items-center gap-1">
            <kbd className="kbd text-[10px]">esc</kbd> close
          </span>
        </div>
      </div>
    </div>
  );
}
