import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUpFromLine,
  ChevronDown,
  ChevronRight,
  Copy,
  FilePen,
  FilePlus2,
  FileX2,
  FileText,
  Layers,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import clsx from "clsx";
import { rpc } from "../ipc";
import { executeCommand, CommandContext } from "../lib/command-registry";
import { useFileWatcher } from "../hooks/useFileWatcher";

interface StashEntry {
  ref: string;
  message: string;
}

interface StashFile {
  status: string;
  path: string;
}

interface Props {
  repoId: string;
  repoPath?: string;
  /** Called when user selects a stash (shows full stash diff in right pane). */
  onSelectStash: (stashRef: string) => void;
  /** Called when user selects a specific file within a stash. */
  onSelectStashFile: (stashRef: string, filePath: string) => void;
  /** Currently selected stash ref (for highlighting). */
  selectedStashRef?: string | null;
  /** Currently selected file path within a stash. */
  selectedFilePath?: string | null;
}

function fileStatusIcon(status: string) {
  switch (status.charAt(0).toUpperCase()) {
    case "A":
      return { Icon: FilePlus2, color: "text-gx-success" };
    case "D":
      return { Icon: FileX2, color: "text-gx-error" };
    case "M":
      return { Icon: FilePen, color: "text-gx-warn" };
    default:
      return { Icon: FileText, color: "text-gx-muted" };
  }
}

/**
 * Left-panel stash list for the tabbed Changes+Stashes view.
 * Shows stash entries with expand/collapse for file lists,
 * inline actions (pop/apply/drop), and a create form.
 */
export function StashesPanel({
  repoId,
  repoPath,
  onSelectStash,
  onSelectStashFile,
  selectedStashRef,
  selectedFilePath,
}: Props) {
  const [stashes, setStashes] = useState<StashEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Create form
  const [showCreate, setShowCreate] = useState(false);
  const [createMsg, setCreateMsg] = useState("");
  const [includeUntracked, setIncludeUntracked] = useState(true);
  const inputRef = useRef<HTMLInputElement>(null);

  // Expanded stashes (show file list)
  const [expandedRefs, setExpandedRefs] = useState<Set<string>>(new Set());
  const [filesByRef, setFilesByRef] = useState<Record<string, StashFile[]>>({});
  const [loadingFiles, setLoadingFiles] = useState<Set<string>>(new Set());

  // Drop confirmation
  const [confirmDrop, setConfirmDrop] = useState<StashEntry | null>(null);

  const commandContext = useMemo<CommandContext>(
    () => ({
      repoId,
      currentBranch: "",
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount: 0,
      hasStagedChanges: false,
      activeOperation: null,
      forgeProvider: null,
    }),
    [repoId],
  );

  // ── Data fetching ──────────────────────────────────────────────────

  const refresh = useCallback(() => {
    if (!repoPath) return;
    setLoading(true);
    setError(null);
    rpc
      .gitStashList(repoId)
      .then((r) => setStashes(r.stashes))
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repoId, repoPath]);

  useEffect(refresh, [refresh]);
  useFileWatcher(repoPath, refresh);

  // ── Expand/collapse ────────────────────────────────────────────────

  const toggleExpand = useCallback(
    (stashRef: string) => {
      setExpandedRefs((prev) => {
        const next = new Set(prev);
        if (next.has(stashRef)) {
          next.delete(stashRef);
        } else {
          next.add(stashRef);
          // Fetch files if not already loaded
          if (!filesByRef[stashRef]) {
            setLoadingFiles((s) => new Set(s).add(stashRef));
            rpc
              .gitStashShow(repoId, stashRef)
              .then((r) => {
                setFilesByRef((prev) => ({ ...prev, [stashRef]: r.files }));
              })
              .catch(() => {
                setFilesByRef((prev) => ({ ...prev, [stashRef]: [] }));
              })
              .finally(() => {
                setLoadingFiles((s) => {
                  const n = new Set(s);
                  n.delete(stashRef);
                  return n;
                });
              });
          }
        }
        return next;
      });
    },
    [repoId, filesByRef],
  );

  // ── Create stash ──────────────────────────────────────────────────

  const handleCreate = useCallback(() => {
    setBusy(true);
    const cmdId = includeUntracked ? "git.stash.untracked" : "git.stash";
    void executeCommand(cmdId, commandContext, createMsg.trim() || undefined).then((result) => {
      setBusy(false);
      if (result.success) {
        setShowCreate(false);
        setCreateMsg("");
        refresh();
      }
    });
  }, [commandContext, createMsg, includeUntracked, refresh]);

  // ── Actions ────────────────────────────────────────────────────────

  const handleAction = useCallback(
    (actionId: string, stashRef: string) => {
      if (actionId === "drop") {
        const stash = stashes?.find((s) => s.ref === stashRef);
        if (stash) setConfirmDrop(stash);
        return;
      }
      setBusy(true);
      void executeCommand(`git.stash.${actionId}`, commandContext, stashRef).then(() => {
        setBusy(false);
        refresh();
      });
    },
    [commandContext, refresh, stashes],
  );

  const handleConfirmDrop = useCallback(() => {
    if (!confirmDrop) return;
    setBusy(true);
    void executeCommand("git.stash.drop", commandContext, confirmDrop.ref).then(() => {
      setBusy(false);
      refresh();
    });
    setConfirmDrop(null);
  }, [confirmDrop, commandContext, refresh]);

  // ── Render ────────────────────────────────────────────────────────

  const stashCount = stashes?.length ?? 0;

  return (
    <div className="flex h-full flex-col">
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-1.5">
        <div className="flex items-center gap-1.5 text-[11px] text-gx-muted">
          <Layers size={12} />
          <span>
            <strong className="text-gx-text">{stashCount}</strong> stash{stashCount !== 1 ? "es" : ""}
          </span>
        </div>
        <div className="flex items-center gap-1">
          <button
            type="button"
            className="flex h-6 w-6 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-text"
            onClick={() => {
              setShowCreate(true);
              setTimeout(() => inputRef.current?.focus(), 50);
            }}
            title="New Stash"
            aria-label="New stash"
          >
            <Plus size={13} />
          </button>
          <button
            type="button"
            className="flex h-6 w-6 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-text"
            onClick={refresh}
            title="Refresh"
            aria-label="Refresh stashes"
          >
            <RefreshCw size={12} />
          </button>
        </div>
      </div>

      {/* Create form */}
      {showCreate && (
        <div className="shrink-0 border-b border-gx-border bg-gx-bg-sub px-3 py-2">
          <div className="flex items-center gap-2">
            <input
              ref={inputRef}
              type="text"
              className="input-field flex-1 text-xs"
              placeholder="Message (optional)"
              value={createMsg}
              onChange={(e) => setCreateMsg(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleCreate();
                if (e.key === "Escape") { setShowCreate(false); setCreateMsg(""); }
              }}
              disabled={busy}
            />
            <button
              type="button"
              className="btn btn-primary px-2 py-1 text-xs"
              onClick={handleCreate}
              disabled={busy}
            >
              Stash
            </button>
            <button
              type="button"
              className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:text-gx-text"
              onClick={() => { setShowCreate(false); setCreateMsg(""); }}
            >
              <X size={12} />
            </button>
          </div>
          <label className="mt-1.5 flex cursor-pointer items-center gap-2 text-[10px] text-gx-muted">
            <input
              type="checkbox"
              checked={includeUntracked}
              onChange={(e) => setIncludeUntracked(e.target.checked)}
              className="h-3 w-3 rounded border-gx-border accent-gx-accent"
            />
            Include untracked files
          </label>
        </div>
      )}

      {/* Stash list */}
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && !stashes && (
          <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading…
          </div>
        )}
        {error && (
          <div className="m-2 rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
            {error}
          </div>
        )}

        {/* Empty state */}
        {!loading && stashes && stashes.length === 0 && (
          <div className="flex flex-col items-center px-4 py-8 text-center">
            <Layers size={20} className="text-gx-muted" />
            <p className="mt-2 text-xs text-gx-muted">No stashes</p>
            <button
              type="button"
              className="btn btn-primary mt-3 gap-1 px-2.5 py-1 text-xs"
              onClick={() => {
                setShowCreate(true);
                setTimeout(() => inputRef.current?.focus(), 50);
              }}
            >
              <Plus size={11} /> Stash Changes
            </button>
          </div>
        )}

        {/* Stash entries */}
        {stashes && stashes.length > 0 && (
          <ul>
            {stashes.map((s) => {
              const idx = parseInt(s.ref.match(/\{(\d+)\}/)?.[1] ?? "0", 10);
              const isExpanded = expandedRefs.has(s.ref);
              const isSelected = selectedStashRef === s.ref && !selectedFilePath;
              const files = filesByRef[s.ref];
              const isLoadingFiles = loadingFiles.has(s.ref);

              return (
                <li key={s.ref} className="border-b border-gx-border/40">
                  {/* Stash row */}
                  <div
                    className={clsx(
                      "group flex cursor-pointer items-center gap-1.5 px-2 py-1.5 text-[12px] transition",
                      isSelected ? "bg-gx-accent/10" : "hover:bg-gx-surface-hover",
                    )}
                    onClick={() => onSelectStash(s.ref)}
                  >
                    {/* Expand chevron */}
                    <button
                      type="button"
                      className="flex h-4 w-4 shrink-0 items-center justify-center rounded text-gx-muted hover:text-gx-text"
                      onClick={(e) => { e.stopPropagation(); toggleExpand(s.ref); }}
                    >
                      {isExpanded ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
                    </button>

                    {/* Index badge */}
                    <span className="flex h-4 min-w-[16px] shrink-0 items-center justify-center rounded bg-gx-surface px-0.5 font-mono text-[9px] text-gx-muted">
                      {idx}
                    </span>

                    {/* Message */}
                    <span className="min-w-0 flex-1 truncate text-gx-text">
                      {s.message || <span className="italic text-gx-muted">No message</span>}
                    </span>

                    {/* Inline actions */}
                    <div className="ml-1 flex shrink-0 gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
                      <button
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-success/20 hover:text-gx-success"
                        onClick={(e) => { e.stopPropagation(); handleAction("pop", s.ref); }}
                        disabled={busy}
                        title="Pop"
                      >
                        <ArrowUpFromLine size={11} />
                      </button>
                      <button
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-accent"
                        onClick={(e) => { e.stopPropagation(); handleAction("apply", s.ref); }}
                        disabled={busy}
                        title="Apply"
                      >
                        <Copy size={11} />
                      </button>
                      <button
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-error/20 hover:text-gx-error"
                        onClick={(e) => { e.stopPropagation(); handleAction("drop", s.ref); }}
                        disabled={busy}
                        title="Drop"
                      >
                        <Trash2 size={11} />
                      </button>
                    </div>
                  </div>

                  {/* Expanded file list */}
                  {isExpanded && (
                    <div className="bg-gx-bg-sub pl-7 pr-2 py-0.5">
                      {isLoadingFiles && (
                        <div className="flex items-center gap-1.5 py-1 text-[10px] text-gx-muted">
                          <Loader2 size={9} className="animate-spin" /> Loading files…
                        </div>
                      )}
                      {!isLoadingFiles && files && files.length === 0 && (
                        <div className="py-1 text-[10px] text-gx-muted">No changes</div>
                      )}
                      {!isLoadingFiles && files && files.map((f) => {
                        const { Icon, color } = fileStatusIcon(f.status);
                        const isFileSelected = selectedStashRef === s.ref && selectedFilePath === f.path;
                        return (
                          <div
                            key={f.path}
                            className={clsx(
                              "flex cursor-pointer items-center gap-1.5 rounded px-1.5 py-0.5 text-[11px] transition",
                              isFileSelected ? "bg-gx-accent/15" : "hover:bg-gx-surface-hover",
                            )}
                            onClick={(e) => { e.stopPropagation(); onSelectStashFile(s.ref, f.path); }}
                          >
                            <Icon size={11} className={color} />
                            <span className="min-w-0 flex-1 truncate font-mono text-gx-text">
                              {f.path.split("/").pop()}
                            </span>
                            <span className="text-[9px] uppercase text-gx-muted">{f.status}</span>
                          </div>
                        );
                      })}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {/* Drop confirmation */}
      {confirmDrop && (
        <div className="fixed inset-0 z-[9999] flex items-center justify-center bg-black/50" onClick={() => setConfirmDrop(null)}>
          <div className="w-72 rounded-lg border border-gx-border bg-gx-surface p-4 shadow-2xl" onClick={(e) => e.stopPropagation()}>
            <h3 className="text-sm font-medium text-gx-text">Drop stash?</h3>
            <p className="mt-1.5 text-xs text-gx-muted">
              Permanently delete <span className="font-mono text-gx-text">{confirmDrop.ref}</span>. Cannot be undone.
            </p>
            <div className="mt-3 flex justify-end gap-2">
              <button type="button" className="btn text-xs" onClick={() => setConfirmDrop(null)}>Cancel</button>
              <button type="button" className="btn btn-danger text-xs" onClick={handleConfirmDrop}>Drop</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
