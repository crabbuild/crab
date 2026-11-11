import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Archive,
  ArrowUpFromLine,
  ChevronDown,
  ChevronRight,
  Copy,
  FileText,
  FilePlus2,
  FileX2,
  FilePen,
  Layers,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import clsx from "clsx";
import { OpenRepo } from "../state";
import { rpc } from "../ipc";
import { ViewToolbar, ToolbarAction } from "../components/ViewToolbar";
import { ContextMenu, MenuItemDef } from "../components/ContextMenu";
import { executeCommand, CommandContext } from "../lib/command-registry";
import { publishToast } from "../lib/toasts";
import { useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";

interface Props {
  repo: OpenRepo;
}

interface StashEntry {
  ref: string;
  message: string;
}

interface StashFile {
  status: string;
  path: string;
}

// ── Helpers ──────────────────────────────────────────────────────────

/** Parse stash index from ref like "stash@{0}" → 0 */
function stashIndex(ref: string): number {
  const m = ref.match(/\{(\d+)\}/);
  return m ? parseInt(m[1], 10) : 0;
}

/** File status code → icon + color */
function fileStatusIcon(status: string) {
  switch (status.charAt(0).toUpperCase()) {
    case "A":
      return { Icon: FilePlus2, color: "text-success" };
    case "D":
      return { Icon: FileX2, color: "text-destructive" };
    case "M":
      return { Icon: FilePen, color: "text-warning" };
    default:
      return { Icon: FileText, color: "text-muted-foreground" };
  }
}

/**
 * Stash management view. Provides a complete workflow for creating,
 * previewing, applying, and dropping git stashes with inline forms,
 * file previews, keyboard navigation, and conflict handling.
 */
export function StashView({ repo }: Props) {
  const [stashes, setStashes] = useState<StashEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Create stash form
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [createMessage, setCreateMessage] = useState("");
  const [includeUntracked, setIncludeUntracked] = useState(true);
  const createInputRef = useRef<HTMLInputElement>(null);

  // Selection & expansion
  const [selectedRef, setSelectedRef] = useState<string | null>(null);
  const [expandedRef, setExpandedRef] = useState<string | null>(null);
  const [expandedFiles, setExpandedFiles] = useState<StashFile[] | null>(null);
  const [loadingFiles, setLoadingFiles] = useState(false);

  // Context menu & confirmation
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    stash: StashEntry;
  } | null>(null);
  const [confirmDrop, setConfirmDrop] = useState<StashEntry | null>(null);

  const listRef = useRef<HTMLUListElement>(null);

  // ── Data fetching ──────────────────────────────────────────────────

  const refresh = useCallback(() => {
    setLoading(true);
    setError(null);
    rpc
      .gitStashList(repo.repo_id)
      .then((r) => {
        setStashes(r.stashes);
        // Clear expanded state if the expanded stash no longer exists
        if (expandedRef && !r.stashes.some((s) => s.ref === expandedRef)) {
          setExpandedRef(null);
          setExpandedFiles(null);
        }
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repo.repo_id, expandedRef]);

  useEffect(refresh, [refresh]);
  useFileWatcher(getWatchPath(repo), refresh);

  // ── Expand stash to show files ─────────────────────────────────────

  const toggleExpand = useCallback(
    (stashRef: string) => {
      if (expandedRef === stashRef) {
        setExpandedRef(null);
        setExpandedFiles(null);
        return;
      }
      setExpandedRef(stashRef);
      setExpandedFiles(null);
      setLoadingFiles(true);
      rpc
        .gitStashShow(repo.repo_id, stashRef)
        .then((r) => setExpandedFiles(r.files))
        .catch(() => setExpandedFiles([]))
        .finally(() => setLoadingFiles(false));
    },
    [repo.repo_id, expandedRef],
  );

  // ── CommandContext ─────────────────────────────────────────────────

  const commandContext = useMemo<CommandContext>(
    () => ({
      repoId: repo.repo_id,
      currentBranch: "",
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount: 0,
      hasStagedChanges: false,
      activeOperation: null,
      forgeProvider: null,
    }),
    [repo.repo_id],
  );

  // ── Create stash ──────────────────────────────────────────────────

  const handleCreate = useCallback(() => {
    setBusy(true);
    setError(null);
    const commandId = includeUntracked ? "git.stash.untracked" : "git.stash";
    void executeCommand(commandId, commandContext, createMessage.trim() || undefined).then(
      (result) => {
        setBusy(false);
        if (result.success) {
          setShowCreateForm(false);
          setCreateMessage("");
          refresh();
        }
      },
    );
  }, [commandContext, createMessage, includeUntracked, refresh]);

  const handleCreateKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        handleCreate();
      } else if (e.key === "Escape") {
        setShowCreateForm(false);
        setCreateMessage("");
      }
    },
    [handleCreate],
  );

  // ── Toolbar ────────────────────────────────────────────────────────

  const toolbarActions = useMemo<ToolbarAction[]>(
    () => [{ id: "create", icon: Plus, label: "New Stash", shortcut: "⌘⇧S", loading: busy }],
    [busy],
  );

  const handleToolbarAction = useCallback(
    (actionId: string) => {
      if (actionId === "create") {
        setShowCreateForm(true);
        setTimeout(() => createInputRef.current?.focus(), 50);
      }
    },
    [],
  );

  // ── Stash actions ─────────────────────────────────────────────────

  const handleAction = useCallback(
    (actionId: string, stashRef: string) => {
      if (actionId === "git.stash.drop") {
        const stash = stashes?.find((s) => s.ref === stashRef);
        if (stash) setConfirmDrop(stash);
        return;
      }
      setBusy(true);
      setError(null);
      void executeCommand(actionId, commandContext, stashRef).then((result) => {
        setBusy(false);
        if (!result.success && result.message.toLowerCase().includes("conflict")) {
          handleConflict(result.message);
        }
        refresh();
      });
    },
    [commandContext, refresh, stashes],
  );

  // ── Context menu ──────────────────────────────────────────────────

  const stashContextMenuItems = useMemo<MenuItemDef[]>(
    () => [
      { id: "git.stash.pop", label: "Pop (restore & remove)", icon: ArrowUpFromLine },
      { id: "git.stash.apply", label: "Apply (restore & keep)", icon: Copy },
      { id: "git.stash.drop", label: "Drop", icon: Trash2, danger: true, separator: true },
    ],
    [],
  );

  const handleContextMenu = useCallback(
    (e: React.MouseEvent, stash: StashEntry) => {
      e.preventDefault();
      setSelectedRef(stash.ref);
      setContextMenu({ x: e.clientX, y: e.clientY, stash });
    },
    [],
  );

  const handleContextMenuSelect = useCallback(
    (actionId: string) => {
      if (!contextMenu) return;
      handleAction(actionId, contextMenu.stash.ref);
      setContextMenu(null);
    },
    [contextMenu, handleAction],
  );

  // ── Drop confirmation ─────────────────────────────────────────────

  const handleConfirmDrop = useCallback(() => {
    if (!confirmDrop) return;
    setBusy(true);
    setError(null);
    void executeCommand("git.stash.drop", commandContext, confirmDrop.ref).then((result) => {
      setBusy(false);
      if (result.success && expandedRef === confirmDrop.ref) {
        setExpandedRef(null);
        setExpandedFiles(null);
      }
      refresh();
    });
    setConfirmDrop(null);
  }, [confirmDrop, commandContext, refresh, expandedRef]);

  // ── Keyboard navigation ───────────────────────────────────────────

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (!stashes || stashes.length === 0) return;

      const currentIdx = stashes.findIndex((s) => s.ref === selectedRef);

      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          const next = Math.min(currentIdx + 1, stashes.length - 1);
          setSelectedRef(stashes[next].ref);
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          const prev = Math.max(currentIdx - 1, 0);
          setSelectedRef(stashes[prev].ref);
          break;
        }
        case "Enter": {
          e.preventDefault();
          if (selectedRef) toggleExpand(selectedRef);
          break;
        }
        case "ArrowRight": {
          e.preventDefault();
          if (selectedRef && expandedRef !== selectedRef) toggleExpand(selectedRef);
          break;
        }
        case "ArrowLeft": {
          e.preventDefault();
          if (selectedRef && expandedRef === selectedRef) toggleExpand(selectedRef);
          break;
        }
        case "Delete":
        case "Backspace": {
          if (e.metaKey || e.ctrlKey) {
            e.preventDefault();
            if (selectedRef) {
              const stash = stashes.find((s) => s.ref === selectedRef);
              if (stash) setConfirmDrop(stash);
            }
          }
          break;
        }
      }
    },
    [stashes, selectedRef, expandedRef, toggleExpand],
  );

  // ── Conflict handling ─────────────────────────────────────────────

  function handleConflict(message: string) {
    const countMatch = message.match(/(\d+)\s*conflict/i);
    const count = countMatch ? parseInt(countMatch[1], 10) : undefined;
    publishToast({
      kind: "warning",
      title: "Merge conflicts",
      body: count
        ? `${count} conflicted file${count > 1 ? "s" : ""}. Resolve in the Conflicts view.`
        : "Conflicts detected. Resolve in the Conflicts view.",
    });
  }

  // ── Render ────────────────────────────────────────────────────────

  return (
    <>
      <ViewToolbar
        title="Stashes"
        icon={Archive}
        actions={toolbarActions}
        onAction={handleToolbarAction}
        trailing={
          <button
            type="button"
            className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-foreground"
            onClick={refresh}
            title="Refresh"
            aria-label="Refresh stashes"
          >
            <RefreshCw size={14} />
          </button>
        }
      />

      {/* ── Inline create form ─────────────────────────────────────── */}
      {showCreateForm && (
        <div className="border-b border-border bg-background px-3 py-2.5">
          <div className="flex items-center gap-2">
            <Input
              ref={createInputRef}
              type="text"
              className="flex-1 text-xs"
              placeholder="Stash message (optional)"
              value={createMessage}
              onChange={(e) => setCreateMessage(e.target.value)}
              onKeyDown={handleCreateKeyDown}
              disabled={busy}
              aria-label="Stash message"
            />
            <Button variant="default"
              type="button"
              className="px-2.5 py-1 text-xs"
              onClick={handleCreate}
              disabled={busy}
            >
              {busy ? <Loader2 size={12} className="animate-spin" /> : "Stash"}
            </Button>
            <button
              type="button"
              className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-foreground"
              onClick={() => {
                setShowCreateForm(false);
                setCreateMessage("");
              }}
              aria-label="Cancel"
            >
              <X size={14} />
            </button>
          </div>
          <label className="mt-2 flex cursor-pointer items-center gap-2 text-[11px] text-muted-foreground">
            <input
              type="checkbox"
              checked={includeUntracked}
              onChange={(e) => setIncludeUntracked(e.target.checked)}
              className="h-3.5 w-3.5 rounded border-border accent-primary"
            />
            Include untracked files
          </label>
        </div>
      )}

      {/* ── Main content ───────────────────────────────────────────── */}
      <div
        className="min-h-0 flex-1 overflow-auto focus:outline-none"
        tabIndex={0}
        onKeyDown={handleKeyDown}
        role="listbox"
        aria-label="Stash list"
      >
        {loading && !stashes && (
          <div className="flex items-center gap-2 p-4 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading stashes…
          </div>
        )}

        {error && (
          <div className="m-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {error}
          </div>
        )}

        {/* ── Empty state ──────────────────────────────────────────── */}
        {!loading && stashes && stashes.length === 0 && (
          <div className="flex flex-col items-center px-6 py-10 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-full bg-card">
              <Layers size={22} className="text-muted-foreground" />
            </div>
            <p className="mt-3 text-sm font-medium text-foreground">No stashes</p>
            <p className="mt-1 max-w-[220px] text-xs leading-relaxed text-muted-foreground">
              Stash your uncommitted changes to save them for later without committing.
            </p>
            <Button variant="default"
              type="button"
              className="mt-4 gap-1.5 px-3 py-1.5 text-xs"
              onClick={() => {
                setShowCreateForm(true);
                setTimeout(() => createInputRef.current?.focus(), 50);
              }}
            >
              <Plus size={13} />
              Stash Changes
            </Button>
            <p className="mt-3 text-[11px] text-muted-foreground">
              <kbd className="kbd">⌘⇧S</kbd> to stash from anywhere
            </p>
          </div>
        )}

        {/* ── Stash list ───────────────────────────────────────────── */}
        {stashes && stashes.length > 0 && (
          <ul ref={listRef} className="py-0.5">
            {stashes.map((s) => {
              const idx = stashIndex(s.ref);
              const isSelected = selectedRef === s.ref;
              const isExpanded = expandedRef === s.ref;

              return (
                <li
                  key={s.ref}
                  role="option"
                  aria-selected={isSelected}
                  className={clsx(
                    "group cursor-pointer border-b border-border/40 transition-colors",
                    isSelected
                      ? "bg-primary/10"
                      : "hover:bg-accent",
                  )}
                  onClick={() => setSelectedRef(s.ref)}
                  onDoubleClick={() => toggleExpand(s.ref)}
                  onContextMenu={(e) => handleContextMenu(e, s)}
                >
                  {/* Main row */}
                  <div className="flex items-center gap-2 px-3 py-2">
                    {/* Expand chevron */}
                    <button
                      type="button"
                      className="flex h-5 w-5 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground"
                      onClick={(e) => {
                        e.stopPropagation();
                        toggleExpand(s.ref);
                      }}
                      aria-label={isExpanded ? "Collapse" : "Expand"}
                    >
                      {isExpanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
                    </button>

                    {/* Index badge */}
                    <span className="flex h-5 min-w-[20px] shrink-0 items-center justify-center rounded bg-card px-1 font-mono text-[10px] text-muted-foreground">
                      {idx}
                    </span>

                    {/* Message */}
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-[12.5px] text-foreground">
                        {s.message || <span className="italic text-muted-foreground">No message</span>}
                      </div>
                    </div>

                    {/* Inline action buttons (visible on hover or selection) */}
                    <div
                      className={clsx(
                        "ml-1 flex shrink-0 gap-0.5 transition-opacity",
                        isSelected ? "opacity-100" : "opacity-0 group-hover:opacity-100",
                      )}
                    >
                      <button
                        type="button"
                        className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-success/20 hover:text-success"
                        onClick={(e) => {
                          e.stopPropagation();
                          handleAction("git.stash.pop", s.ref);
                        }}
                        disabled={busy}
                        title="Pop — restore changes and remove stash"
                        aria-label={`Pop ${s.ref}`}
                      >
                        <ArrowUpFromLine size={13} />
                      </button>
                      <button
                        type="button"
                        className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-primary"
                        onClick={(e) => {
                          e.stopPropagation();
                          handleAction("git.stash.apply", s.ref);
                        }}
                        disabled={busy}
                        title="Apply — restore changes and keep stash"
                        aria-label={`Apply ${s.ref}`}
                      >
                        <Copy size={13} />
                      </button>
                      <button
                        type="button"
                        className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-destructive/20 hover:text-destructive"
                        onClick={(e) => {
                          e.stopPropagation();
                          handleAction("git.stash.drop", s.ref);
                        }}
                        disabled={busy}
                        title="Drop — permanently delete stash"
                        aria-label={`Drop ${s.ref}`}
                      >
                        <Trash2 size={13} />
                      </button>
                    </div>
                  </div>

                  {/* Expanded file list */}
                  {isExpanded && (
                    <div className="border-t border-border/30 bg-background px-3 py-1.5">
                      {loadingFiles && (
                        <div className="flex items-center gap-2 py-1 text-[11px] text-muted-foreground">
                          <Loader2 size={10} className="animate-spin" /> Loading files…
                        </div>
                      )}
                      {!loadingFiles && expandedFiles && expandedFiles.length === 0 && (
                        <div className="py-1 text-[11px] text-muted-foreground">No file changes</div>
                      )}
                      {!loadingFiles && expandedFiles && expandedFiles.length > 0 && (
                        <ul className="space-y-0.5">
                          {expandedFiles.map((f) => {
                            const { Icon, color } = fileStatusIcon(f.status);
                            return (
                              <li
                                key={f.path}
                                className="flex items-center gap-2 rounded px-1.5 py-0.5 text-[11px] hover:bg-accent"
                              >
                                <Icon size={12} className={color} />
                                <span className="min-w-0 flex-1 truncate font-mono text-foreground">
                                  {f.path}
                                </span>
                                <span className="shrink-0 text-[10px] uppercase text-muted-foreground">
                                  {f.status}
                                </span>
                              </li>
                            );
                          })}
                        </ul>
                      )}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}

        {/* ── Stash count footer ───────────────────────────────────── */}
        {stashes && stashes.length > 0 && (
          <div className="px-3 py-2 text-[11px] text-muted-foreground">
            {stashes.length} stash{stashes.length !== 1 ? "es" : ""}
          </div>
        )}
      </div>

      {/* ── Context menu ───────────────────────────────────────────── */}
      {contextMenu && (
        <ContextMenu
          items={stashContextMenuItems}
          position={{ x: contextMenu.x, y: contextMenu.y }}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}

      {/* ── Drop confirmation dialog ──────────────────────────────── */}
      {confirmDrop && (
        <div
          className="fixed inset-0 z-[9999] flex items-center justify-center bg-black/50"
          onClick={() => setConfirmDrop(null)}
          role="dialog"
          aria-modal="true"
          aria-labelledby="drop-dialog-title"
        >
          <div
            className="w-80 rounded-lg border border-border bg-card p-5 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <h3 id="drop-dialog-title" className="text-sm font-medium text-foreground">
              Drop stash?
            </h3>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
              This will permanently delete{" "}
              <span className="font-mono font-medium text-foreground">{confirmDrop.ref}</span>.
            </p>
            {confirmDrop.message && (
              <p className="mt-1 truncate text-xs italic text-muted-foreground">
                "{confirmDrop.message}"
              </p>
            )}
            <p className="mt-2 text-[11px] text-destructive/80">This action cannot be undone.</p>
            <div className="mt-4 flex justify-end gap-2">
              <Button variant="outline"
                type="button"
                className="text-xs"
                onClick={() => setConfirmDrop(null)}
                autoFocus
              >
                Cancel
              </Button>
              <Button variant="destructive"
                type="button"
                className="text-xs"
                onClick={handleConfirmDrop}
              >
                Drop Stash
              </Button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
