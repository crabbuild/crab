import { Button } from "@/components/ui/button";
import {
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  Command as CommandRoot,
} from "@/components/ui/command";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import { FolderGit2, Search, Terminal, Zap } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { SlashCommand } from "./SlashCommandDropdown";
import type { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

export interface PaletteAction {
  id: string;
  label: string;
  shortcut?: string;
  icon?: "workspace" | "pr" | "search" | "settings" | "template";
}

export interface OrchestratorPaletteProps {
  visible: boolean;
  onDismiss: () => void;
  onSelectWorkspace: (id: string) => void;
  onAction: (action: string) => void;
  tasks: OrchestratorTask[];
  /** Initial mode: "command" (Cmd+K) or "search" (Cmd+Shift+F). */
  mode?: "command" | "search";
}

interface SearchResult {
  workspaceId: string;
  matchField: string;
  snippet: string;
  score: number;
}

type PaletteItem =
  | { kind: "workspace"; task: OrchestratorTask }
  | { kind: "action"; action: PaletteAction }
  | { kind: "command"; command: SlashCommand }
  | { kind: "search-result"; result: SearchResult };

// ── Default actions ─────────────────────────────────────────────────

const DEFAULT_ACTIONS: PaletteAction[] = [
  { id: "new-workspace", label: "New workspace", shortcut: "⌘N", icon: "workspace" },
  { id: "create-pr", label: "Create PR", icon: "pr" },
  { id: "search-all", label: "Search all workspaces", shortcut: "⌘⇧F", icon: "search" },
  { id: "settings", label: "Settings", shortcut: "⌘,", icon: "settings" },
  { id: "new-from-template", label: "New from template", shortcut: "⌘⇧N", icon: "template" },
];

// ── Helpers ─────────────────────────────────────────────────────────

function matchesQuery(text: string, query: string): boolean {
  const lower = query.toLowerCase();
  return text.toLowerCase().includes(lower);
}

const STATUS_INDICATOR: Record<string, string> = {
  running: "🟢",
  planning: "🟡",
  reviewing: "🔵",
  iterating: "🟣",
  queued: "🟠",
  applied: "✅",
  archived: "⚪",
  failed: "🔴",
};

// ── Component ───────────────────────────────────────────────────────

/**
 * Orchestrator-specific command palette. Triggered by Cmd+K in the
 * orchestrator/tasks view. Provides workspace quick-switch, action
 * execution, slash command access, and full workspace search.
 *
 * Supports two modes:
 * - "command" (default, Cmd+K): shows recent workspaces, actions, commands
 * - "search" (Cmd+Shift+F): full workspace search via IPC
 */
export function OrchestratorPalette({
  visible,
  onDismiss,
  onSelectWorkspace,
  onAction,
  tasks,
  mode: initialMode = "command",
}: OrchestratorPaletteProps) {
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const [commands, setCommands] = useState<SlashCommand[]>([]);
  const [searchResults, setSearchResults] = useState<SearchResult[]>([]);
  const [mode, setMode] = useState<"command" | "search">(initialMode);

  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // Sync mode when prop changes (e.g., opened via different shortcut).
  useEffect(() => {
    if (visible) {
      setMode(initialMode);
    }
  }, [visible, initialMode]);

  // Load slash commands on open.
  useEffect(() => {
    if (!visible) return;
    (window as any).crab?.orchestrator
      ?.listCommands?.("")
      .then((result: { success?: boolean; commands?: SlashCommand[] }) => {
        if (result?.success && result.commands) {
          setCommands(result.commands);
        }
      })
      .catch(() => {});
  }, [visible]);

  // Reset state on open.
  useEffect(() => {
    if (visible) {
      setQuery("");
      setActiveIndex(0);
      setSearchResults([]);
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [visible]);

  // Perform IPC search when in search mode and query changes.
  useEffect(() => {
    if (!visible || mode !== "search" || !query.trim()) {
      setSearchResults([]);
      return;
    }

    const timeout = setTimeout(() => {
      (window as any).crab?.orchestrator
        ?.search?.(query)
        .then((result: { success?: boolean; results?: SearchResult[] }) => {
          if (result?.success && result.results) {
            setSearchResults(result.results);
          }
        })
        .catch(() => {});
    }, 150); // debounce

    return () => clearTimeout(timeout);
  }, [visible, mode, query]);

  // Build the flat item list for rendering and navigation.
  const items: PaletteItem[] = useMemo(() => {
    if (mode === "search") {
      // In search mode, show search results or recent workspaces if no query.
      if (query.trim() && searchResults.length > 0) {
        return searchResults.map((r) => ({ kind: "search-result" as const, result: r }));
      }
      if (!query.trim()) {
        // Show all workspaces sorted by recency.
        return tasks
          .slice()
          .sort((a, b) => b.createdAt - a.createdAt)
          .slice(0, 20)
          .map((task) => ({ kind: "workspace" as const, task }));
      }
      return [];
    }

    // Command mode: filter across workspaces, actions, and commands.
    const result: PaletteItem[] = [];

    // Recent workspaces (filtered).
    const recentTasks = tasks
      .slice()
      .sort((a, b) => b.createdAt - a.createdAt)
      .slice(0, 5);

    const filteredTasks = query
      ? recentTasks.filter(
          (t) => matchesQuery(t.title, query) || matchesQuery(t.agentName, query),
        )
      : recentTasks;

    for (const task of filteredTasks) {
      result.push({ kind: "workspace", task });
    }

    // Actions (filtered).
    const filteredActions = query
      ? DEFAULT_ACTIONS.filter((a) => matchesQuery(a.label, query))
      : DEFAULT_ACTIONS;

    for (const action of filteredActions) {
      result.push({ kind: "action", action });
    }

    // Slash commands (filtered).
    const filteredCommands = query
      ? commands.filter(
          (c) => matchesQuery(c.name, query) || matchesQuery(c.description, query),
        )
      : commands.slice(0, 5);

    for (const cmd of filteredCommands) {
      result.push({ kind: "command", command: cmd });
    }

    return result;
  }, [mode, query, tasks, commands, searchResults]);

  // Keep active index in bounds.
  useEffect(() => {
    setActiveIndex((prev) => Math.min(prev, Math.max(0, items.length - 1)));
  }, [items.length]);

  // Scroll active item into view.
  useEffect(() => {
    if (!listRef.current) return;
    const el = listRef.current.querySelector(`[data-idx="${activeIndex}"]`);
    el?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  // Activate the selected item.
  const activateItem = useCallback(
    (item: PaletteItem) => {
      switch (item.kind) {
        case "workspace":
          onSelectWorkspace(item.task.id);
          onDismiss();
          break;
        case "action":
          onAction(item.action.id);
          onDismiss();
          break;
        case "command":
          onAction(`command:${item.command.name}`);
          onDismiss();
          break;
        case "search-result":
          onSelectWorkspace(item.result.workspaceId);
          onDismiss();
          break;
      }
    },
    [onSelectWorkspace, onAction, onDismiss],
  );

  // Keyboard handler.
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "Enter":
          e.preventDefault();
          if (items[activeIndex]) {
            activateItem(items[activeIndex]);
          }
          break;
        case "Escape":
          e.preventDefault();
          onDismiss();
          break;
        case "Tab":
          // Toggle between command and search mode.
          e.preventDefault();
          setMode((m) => (m === "command" ? "search" : "command"));
          setActiveIndex(0);
          break;
      }
    },
    [items, activeIndex, activateItem, onDismiss],
  );

  if (!visible) return null;

  // Determine section headers for grouped display in command mode.
  const sections = useMemo(() => {
    if (mode === "search") return null;

    const groups: { label: string; startIdx: number; endIdx: number }[] = [];
    let idx = 0;

    const workspaceItems = items.filter((i) => i.kind === "workspace");
    if (workspaceItems.length > 0) {
      groups.push({ label: "Recent Workspaces", startIdx: idx, endIdx: idx + workspaceItems.length });
      idx += workspaceItems.length;
    }

    const actionItems = items.filter((i) => i.kind === "action");
    if (actionItems.length > 0) {
      groups.push({ label: "Actions", startIdx: idx, endIdx: idx + actionItems.length });
      idx += actionItems.length;
    }

    const commandItems = items.filter((i) => i.kind === "command");
    if (commandItems.length > 0) {
      groups.push({ label: "Slash Commands", startIdx: idx, endIdx: idx + commandItems.length });
    }

    return groups;
  }, [mode, items]);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onDismiss();
      }}
    >
      {/* Backdrop */}

      {/* Backdrop */}
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className={
          "flex w-full max-w-lg flex-col overflow-hidden rounded-lg border border-border bg-card shadow-2xl" +
          " p-0 gap-0"
        }
      >
        <DialogTitle className="sr-only">Agent command palette</DialogTitle>
        <CommandRoot
          shouldFilter={false}
          value={String(activeIndex)}
          onValueChange={(value) => setActiveIndex(Number(value))}
          onKeyDown={handleKeyDown}
        >
          {/* Search input */}
          <div className="flex items-center gap-2 border-b border-border px-3 py-2">
            <CommandInput
              ref={inputRef}
              className="flex-1 bg-transparent text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-none"
              placeholder={
              mode === "search"
                ? "Search workspaces…"
                : "Search workspaces, actions, commands…"
            }
              value={query}
              onValueChange={(value) => {
                setQuery(value);
                setActiveIndex(0);
              }}
              aria-label={mode === "search" ? "Search workspaces" : "Search commands"}
            />
            {/* Mode indicator */}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx(
              "shrink-0 rounded px-2 py-0.5 text-[10px] font-medium transition",
              mode === "search"
                ? "bg-primary/20 text-primary"
                : "bg-background text-muted-foreground hover:text-foreground",
            )}
              onClick={() => setMode((m) => (m === "command" ? "search" : "command"))}
              aria-label={`Switch to ${mode === "command" ? "search" : "command"} mode`}
              tabIndex={-1}
            >
              {mode === "search" ? "Search" : "Commands"}
            </Button>
          </div>

          {/* Results list */}
          <CommandList
            ref={listRef}
            id="orchestrator-palette-results"
            className="max-h-[360px] overflow-y-auto py-1"
          >
            <CommandGroup>
              {items.length === 0 ? (
                <div className="px-3 py-4 text-center text-sm text-muted-foreground">
                  {query ? "No results found" : "No workspaces yet"}
                </div>
              ) : (
                <>
                  {mode === "command" && sections
                    ? sections.map((section) => (
                        <div key={section.label}>
                          <div className="px-3 pt-2 pb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
                            {section.label}
                          </div>
                          {items.slice(section.startIdx, section.endIdx).map((item, i) => {
                        const idx = section.startIdx + i;
                        return (
                          <PaletteRow
                            key={`${item.kind}-${idx}`}
                            item={item}
                            idx={idx}
                            isActive={idx === activeIndex}
                            onActivate={() => activateItem(item)}
                            onHover={() => setActiveIndex(idx)}
                          />
                        );
                      })}
                        </div>
                      ))
                    : items.map((item, idx) => (
                    <PaletteRow
                      key={`${item.kind}-${idx}`}
                      item={item}
                      idx={idx}
                      isActive={idx === activeIndex}
                      onActivate={() => activateItem(item)}
                      onHover={() => setActiveIndex(idx)}
                    />
                  ))}
                </>
              )}
            </CommandGroup>
          </CommandList>

          {/* Footer hint */}
          <div className="flex items-center justify-between border-t border-border px-3 py-1.5 text-[10px] text-muted-foreground">
            <span>↑↓ navigate · ↵ select · esc dismiss · tab switch mode</span>
            <span>{mode === "search" ? "⌘⇧F" : "⌘K"}</span>
          </div>
        </CommandRoot>
      </DialogContent>
    </Dialog>
  );
}

// ── Row renderer ────────────────────────────────────────────────────

interface PaletteRowProps {
  item: PaletteItem;
  idx: number;
  isActive: boolean;
  onActivate: () => void;
  onHover: () => void;
}

function PaletteRow({ item, idx, isActive, onActivate, onHover }: PaletteRowProps) {
  return (
    <CommandItem
      value={String(idx)}
      data-idx={idx}
      aria-selected={isActive}
      className={clsx(
        "flex cursor-pointer items-center gap-2 px-3 py-1.5 text-sm transition",
        isActive
          ? "bg-primary/15 text-foreground"
          : "text-foreground/80 hover:bg-accent",
      )}
      onSelect={onActivate}
      onMouseEnter={onHover}
    >
      {item.kind === "workspace" && (
        <>
          <FolderGit2 size={13} className="shrink-0 text-muted-foreground" />
          <span className="shrink-0 text-xs">
            {STATUS_INDICATOR[item.task.status] ?? "⚪"}
          </span>
          <span className="min-w-0 flex-1 truncate text-xs">
            {item.task.title}
          </span>
          <span className="shrink-0 text-[10px] text-muted-foreground">
            {item.task.agentName}
          </span>
        </>
      )}

      {item.kind === "action" && (
        <>
          <Zap size={13} className="shrink-0 text-primary" />
          <span className="min-w-0 flex-1 truncate text-xs">
            {item.action.label}
          </span>
          {item.action.shortcut && (
            <span className="shrink-0 rounded bg-background px-1.5 py-0.5 text-[10px] text-muted-foreground font-mono">
              {item.action.shortcut}
            </span>
          )}
        </>
      )}

      {item.kind === "command" && (
        <>
          <Terminal size={13} className="shrink-0 text-primary" />
          <span className="shrink-0 text-xs font-medium">
            /{item.command.name}
          </span>
          <span className="min-w-0 flex-1 truncate text-[10px] text-muted-foreground">
            {item.command.description}
          </span>
          <span className="shrink-0 rounded-full bg-card px-1.5 py-0.5 text-[9px] text-muted-foreground">
            {item.command.source}
          </span>
        </>
      )}

      {item.kind === "search-result" && (
        <>
          <Search size={13} className="shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate text-xs">
            {item.result.snippet}
          </span>
          <span className="shrink-0 rounded bg-background px-1.5 py-0.5 text-[9px] text-muted-foreground">
            {item.result.matchField}
          </span>
        </>
      )}
    </CommandItem>
  );
}

// ── Keyboard shortcut hook ──────────────────────────────────────────

/**
 * Hook that listens for Cmd+K and Cmd+Shift+F to open the palette.
 * Returns state and callbacks to wire into the parent component.
 */
export function useOrchestratorPalette() {
  const [visible, setVisible] = useState(false);
  const [mode, setMode] = useState<"command" | "search">("command");

  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      const meta = e.metaKey || e.ctrlKey;

      // Cmd+K → command mode
      if (meta && e.key === "k" && !e.shiftKey) {
        e.preventDefault();
        setMode("command");
        setVisible(true);
      }

      // Cmd+Shift+F → search mode
      if (meta && e.key === "f" && e.shiftKey) {
        e.preventDefault();
        setMode("search");
        setVisible(true);
      }
    }

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, []);

  const dismiss = useCallback(() => setVisible(false), []);

  return { visible, mode, dismiss };
}
