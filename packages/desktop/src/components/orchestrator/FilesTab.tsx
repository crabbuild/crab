/**
 * Files Tab — Full file explorer for the agent's isolated worktree.
 *
 * Three-part layout:
 *   1. File tree on the left (resizable) — same FileTree as the main Explorer
 *      with a filter input and collapse-all action
 *   2. Tab strip at the top of the editor area — multi-tab support
 *   3. Monaco editor below — shows the active tab's file content
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { ChevronsDownUp, Loader2, Search, X } from "lucide-react";
import clsx from "clsx";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { FileTree } from "../FileTree";
import { FileIcon } from "../FileIcon";
import { CodeEditor } from "../CodeEditor";
import { DirEntry, mainProcess } from "../../ipc";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

interface EditorTab {
  key: string;
  label: string;
  path: string;
}

interface Props {
  task: OrchestratorTask;
  selectedFile?: string | null;
  onSelectFile?: (path: string) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function FilesTab({ task, selectedFile, onSelectFile }: Props) {
  const [entries, setEntries] = useState<DirEntry[]>([]);
  const [treeLoading, setTreeLoading] = useState(true);

  // Filter & collapse state.
  const [filter, setFilter] = useState("");
  const [collapseKey, setCollapseKey] = useState(0);

  // Multi-tab state.
  const [openTabs, setOpenTabs] = useState<EditorTab[]>([]);
  const [activeKey, setActiveKey] = useState<string | null>(null);

  // File content cache.
  const [contentCache, setContentCache] = useState<
    Record<string, { content: string | null; error: string | null; loading: boolean }>
  >({});

  // Fetch the worktree file list.
  const fetchEntries = useCallback(() => {
    setTreeLoading(true);
    (window as any).crab.orchestrator
      .listWorktreeFiles(task.id)
      .then((result: { success: boolean; files?: string[]; error?: string }) => {
        if (result.success && result.files) {
          const dirEntries: DirEntry[] = result.files.map((path) => ({
            path,
            kind: "file" as const,
            size: null,
            pointer_hash: null,
          }));
          setEntries(dirEntries);
        } else {
          setEntries([]);
        }
      })
      .catch(() => setEntries([]))
      .finally(() => setTreeLoading(false));
  }, [task.id]);

  useEffect(() => {
    fetchEntries();
  }, [fetchEntries]);

  // Apply text filter to entries.
  const filteredEntries = useMemo(() => {
    const f = filter.trim().toLowerCase();
    if (!f) return entries;
    return entries.filter((e) => e.path.toLowerCase().includes(f));
  }, [entries, filter]);

  // Sync external selection.
  useEffect(() => {
    if (selectedFile) {
      openFile(selectedFile);
    }
  }, [selectedFile]);

  // Load file content (with caching).
  const loadFileContent = useCallback(
    (path: string) => {
      if (!task.worktreePath) return;
      setContentCache((prev) => {
        if (prev[path] && !prev[path].error) return prev;
        return { ...prev, [path]: { content: null, error: null, loading: true } };
      });

      const fullPath = `${task.worktreePath}/${path}`;
      mainProcess
        .fsReadFile(fullPath)
        .then((result) => {
          if (result.exists && result.content != null) {
            setContentCache((prev) => ({
              ...prev,
              [path]: { content: result.content, error: null, loading: false },
            }));
          } else {
            setContentCache((prev) => ({
              ...prev,
              [path]: { content: null, error: "File not found or is binary", loading: false },
            }));
          }
        })
        .catch((err: Error) => {
          setContentCache((prev) => ({
            ...prev,
            [path]: { content: null, error: err.message, loading: false },
          }));
        });
    },
    [task.worktreePath],
  );

  // Open a file: add to tabs (or activate existing) and load content.
  const openFile = useCallback(
    (path: string) => {
      const key = path;
      setOpenTabs((prev) => {
        if (prev.some((t) => t.key === key)) return prev;
        const label = path.split("/").pop() ?? path;
        return [...prev, { key, label, path }];
      });
      setActiveKey(key);
      loadFileContent(path);
      onSelectFile?.(path);
    },
    [loadFileContent, onSelectFile],
  );

  // Close a tab.
  const closeTab = useCallback(
    (key: string) => {
      setOpenTabs((prev) => {
        const next = prev.filter((t) => t.key !== key);
        if (activeKey === key) {
          const closedIdx = prev.findIndex((t) => t.key === key);
          const newActive = next[Math.min(closedIdx, next.length - 1)]?.key ?? null;
          setActiveKey(newActive);
        }
        return next;
      });
    },
    [activeKey],
  );

  const handleOpenFile = useCallback(
    (path: string) => {
      openFile(path);
    },
    [openFile],
  );

  const activeContent = activeKey ? contentCache[activeKey] : null;

  return (
    <PanelGroup direction="horizontal" className="h-full">
      {/* Left: File tree with toolbar */}
      <Panel defaultSize={22} minSize={12} maxSize={45}>
        <div className="flex h-full flex-col overflow-hidden border-r border-gx-border">
          {/* Toolbar: filter + collapse */}
          <div className="shrink-0 border-b border-gx-border px-2 py-2">
            {/* Filter input */}
            <div className="relative">
              <Search size={11} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-gx-muted" />
              <input
                className="w-full rounded border border-gx-border bg-gx-bg-sub pl-6 pr-2 py-1 text-[11px] focus-visible:outline-none focus:border-gx-accent placeholder:text-gx-muted/60"
                placeholder="Filter files…"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              />
              {filter && (
                <button
                  type="button"
                  onClick={() => setFilter("")}
                  className="absolute right-1.5 top-1/2 -translate-y-1/2 rounded p-0.5 text-gx-muted hover:text-gx-text"
                  aria-label="Clear filter"
                >
                  <X size={10} />
                </button>
              )}
            </div>
            {/* Action bar */}
            <div className="mt-1.5 flex items-center justify-between">
              <span className="text-[10px] text-gx-muted">
                {filteredEntries.length} file{filteredEntries.length !== 1 ? "s" : ""}
              </span>
              <button
                type="button"
                className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
                title="Collapse All"
                aria-label="Collapse All"
                onClick={() => setCollapseKey((k) => k + 1)}
              >
                <ChevronsDownUp size={12} />
              </button>
            </div>
          </div>

          {/* Tree */}
          <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
            {treeLoading ? (
              <div className="flex h-full items-center justify-center">
                <Loader2 size={16} className="animate-spin text-gx-muted" />
              </div>
            ) : filteredEntries.length === 0 ? (
              <div className="flex h-full items-center justify-center text-[11px] text-gx-muted">
                {filter ? "No matching files." : "No files in worktree."}
              </div>
            ) : (
              <FileTree
                key={collapseKey}
                entries={filteredEntries}
                onOpenFile={handleOpenFile}
                workspacePath={task.worktreePath}
                initialExpansion={1}
                onRefresh={fetchEntries}
              />
            )}
          </div>
        </div>
      </Panel>

      <PanelResizeHandle className="w-[3px] cursor-col-resize bg-transparent transition-colors hover:bg-gx-accent/50" />

      {/* Right: Tab strip + Editor */}
      <Panel minSize={30}>
        <div className="flex h-full flex-col overflow-hidden">
          {/* Tab strip */}
          {openTabs.length > 0 && (
            <div className="flex shrink-0 items-center overflow-x-auto scrollbar-none border-b border-gx-border bg-gx-surface">
              {openTabs.map((tab) => (
                <EditorTabPill
                  key={tab.key}
                  tab={tab}
                  active={tab.key === activeKey}
                  onActivate={() => setActiveKey(tab.key)}
                  onClose={() => closeTab(tab.key)}
                />
              ))}
            </div>
          )}

          {/* Editor content */}
          <div className="min-h-0 flex-1">
            {openTabs.length === 0 && (
              <div className="flex h-full items-center justify-center text-xs text-gx-muted">
                Select a file to view its contents.
              </div>
            )}
            {activeKey && activeContent?.loading && (
              <div className="flex h-full items-center justify-center">
                <Loader2 size={16} className="animate-spin text-gx-muted" />
              </div>
            )}
            {activeKey && activeContent && !activeContent.loading && activeContent.error && (
              <div className="flex h-full items-center justify-center text-xs text-gx-muted">
                {activeContent.error}
              </div>
            )}
            {activeKey && activeContent && !activeContent.loading && !activeContent.error && activeContent.content != null && (
              <CodeEditor
                key={activeKey}
                path={activeKey}
                cacheKey={`agent-${task.id}::${activeKey}`}
                value={activeContent.content}
                readOnly
              />
            )}
          </div>
        </div>
      </Panel>
    </PanelGroup>
  );
}

// ── Tab Pill ────────────────────────────────────────────────────────

function EditorTabPill({
  tab,
  active,
  onActivate,
  onClose,
}: {
  tab: EditorTab;
  active: boolean;
  onActivate: () => void;
  onClose: () => void;
}) {
  return (
    <div
      className={clsx(
        "group flex shrink-0 items-center gap-1.5 border-r border-gx-border/50 px-3 py-1.5 text-[11px] cursor-pointer transition select-none",
        active
          ? "bg-gx-bg-sub text-gx-text"
          : "bg-gx-surface text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
      )}
      onClick={onActivate}
      title={tab.path}
    >
      <FileIcon path={tab.path} kind="file" />
      <span className="max-w-[120px] truncate">{tab.label}</span>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
        className={clsx(
          "ml-1 shrink-0 rounded p-0.5 transition",
          active
            ? "text-gx-muted hover:bg-gx-border/50 hover:text-gx-text"
            : "text-transparent group-hover:text-gx-muted group-hover:hover:bg-gx-border/50 group-hover:hover:text-gx-text",
        )}
        aria-label={`Close ${tab.label}`}
      >
        <X size={10} />
      </button>
    </div>
  );
}
