import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import clsx from "clsx";
import {
  ArrowUpFromLine,
  ChevronDown,
  ChevronRight,
  Copy,
  FilePen,
  FilePlus2,
  FileText,
  FileX2,
  Layers,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useFileWatcher } from "../hooks/useFileWatcher";
import { rpc } from "../ipc";
import { CommandContext, executeCommand } from "../lib/command-registry";

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
      <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-1.5">
        <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Layers size={12} />
          <span>
            <strong className="text-foreground">{stashCount}</strong> stash
            {stashCount !== 1 ? "es" : ""}
          </span>
        </div>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-foreground"
            onClick={() => {
              setShowCreate(true);
              setTimeout(() => inputRef.current?.focus(), 50);
            }}
            title="New Stash"
            aria-label="New stash"
          >
            <Plus size={13} />
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-foreground"
            onClick={refresh}
            title="Refresh"
            aria-label="Refresh stashes"
          >
            <RefreshCw size={12} />
          </Button>
        </div>
      </div>

      {/* Create form */}
      {showCreate && (
        <div className="shrink-0 border-b border-border bg-background px-3 py-2">
          <div className="flex items-center gap-2">
            <Input
              ref={inputRef}
              type="text"
              className="flex-1 text-xs"
              placeholder="Message (optional)"
              value={createMsg}
              onChange={(e) => setCreateMsg(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleCreate();
                if (e.key === "Escape") { setShowCreate(false); setCreateMsg(""); }
              }}
              disabled={busy}
            />
            <Button variant="default"
              type="button"
              className="px-2 py-1 text-xs"
              onClick={handleCreate}
              disabled={busy}
            >
              Stash
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:text-foreground"
              onClick={() => { setShowCreate(false); setCreateMsg(""); }}
            >
              <X size={12} />
            </Button>
          </div>
          <label className="mt-1.5 flex cursor-pointer items-center gap-2 text-[10px] text-muted-foreground">
            <Checkbox
              checked={includeUntracked}
              onCheckedChange={(checked) =>
                setIncludeUntracked(checked === true)
              }
              className="h-3 w-3 rounded border-border accent-primary"
            />
            Include untracked files
          </label>
        </div>
      )}

      {/* Stash list */}
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && !stashes && (
          <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading…
          </div>
        )}
        {error && (
          <div className="m-2 rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {error}
          </div>
        )}

        {/* Stash list */}
        {!loading && stashes && stashes.length === 0 && (
          <div className="flex flex-col items-center px-4 py-8 text-center">
            <Layers size={20} className="text-muted-foreground" />
            <p className="mt-2 text-xs text-muted-foreground">No stashes</p>
            <Button variant="default"
              type="button"
              className="mt-3 gap-1 px-2.5 py-1 text-xs"
              onClick={() => {
                setShowCreate(true);
                setTimeout(() => inputRef.current?.focus(), 50);
              }}
            >
              <Plus size={11} /> Stash Changes
            </Button>
          </div>
        )}

        {/* Empty state */}
        {stashes && stashes.length > 0 && (
          <ul>
            {stashes.map((s) => {
              const idx = parseInt(s.ref.match(/\{(\d+)\}/)?.[1] ?? "0", 10);
              const isExpanded = expandedRefs.has(s.ref);
              const isSelected = selectedStashRef === s.ref && !selectedFilePath;
              const files = filesByRef[s.ref];
              const isLoadingFiles = loadingFiles.has(s.ref);

              return (
                <li key={s.ref} className="border-b border-border/40">
                  {/* Stash entries */}
                  <div
                    className={clsx(
                      "group flex cursor-pointer items-center gap-1.5 px-2 py-1.5 text-[12px] transition",
                      isSelected ? "bg-primary/10" : "hover:bg-accent",
                    )}
                    onClick={() => onSelectStash(s.ref)}
                  >
                    {/* Stash row */}
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      className="flex h-4 w-4 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground"
                      onClick={(e) => { e.stopPropagation(); toggleExpand(s.ref); }}
                    >
                      {isExpanded ? (
                        <ChevronDown size={11} />
                      ) : (
                        <ChevronRight size={11} />
                      )}
                    </Button>

                    {/* Expand chevron */}
                    <span className="flex h-4 min-w-[16px] shrink-0 items-center justify-center rounded bg-card px-0.5 font-mono text-[9px] text-muted-foreground">
                      {idx}
                    </span>

                    {/* Index badge */}
                    <span className="min-w-0 flex-1 truncate text-foreground">
                      {s.message || (
                        <span className="italic text-muted-foreground">
                          No message
                        </span>
                      )}
                    </span>

                    {/* Message */}
                    <div className="ml-1 flex shrink-0 gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-success/20 hover:text-success"
                        onClick={(e) => { e.stopPropagation(); handleAction("pop", s.ref); }}
                        disabled={busy}
                        title="Pop"
                      >
                        <ArrowUpFromLine size={11} />
                      </Button>
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-primary"
                        onClick={(e) => { e.stopPropagation(); handleAction("apply", s.ref); }}
                        disabled={busy}
                        title="Apply"
                      >
                        <Copy size={11} />
                      </Button>
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:bg-destructive/20 hover:text-destructive"
                        onClick={(e) => { e.stopPropagation(); handleAction("drop", s.ref); }}
                        disabled={busy}
                        title="Drop"
                      >
                        <Trash2 size={11} />
                      </Button>
                    </div>
                  </div>

                  {/* Expanded file list */}
                  {isExpanded && (
                    <div className="bg-background pl-7 pr-2 py-0.5">
                      {isLoadingFiles && (
                        <div className="flex items-center gap-1.5 py-1 text-[10px] text-muted-foreground">
                          <Loader2 size={9} className="animate-spin" /> Loading
                          files…
                        </div>
                      )}
                      {!isLoadingFiles && files && files.length === 0 && (
                        <div className="py-1 text-[10px] text-muted-foreground">
                          No changes
                        </div>
                      )}
                      {!isLoadingFiles && files &&
                        files.map((f) => {
                          const { Icon, color } = fileStatusIcon(f.status);
                          const isFileSelected = selectedStashRef === s.ref && selectedFilePath === f.path;
                          return (
                            <div
                            key={f.path}
                            className={clsx(
                              "flex cursor-pointer items-center gap-1.5 rounded px-1.5 py-0.5 text-[11px] transition",
                              isFileSelected ? "bg-primary/15" : "hover:bg-accent",
                            )}
                            onClick={(e) => { e.stopPropagation(); onSelectStashFile(s.ref, f.path); }}
                          >
                              <Icon size={11} className={color} />
                              <span className="min-w-0 flex-1 truncate font-mono text-foreground">
                                {f.path.split("/").pop()}
                              </span>
                              <span className="text-[9px] uppercase text-muted-foreground">
                                {f.status}
                              </span>
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
        <Dialog
          open
          onOpenChange={(open) => {
            if (!open) {
              setConfirmDrop(null);
            }
          }}
        >
          <DialogContent
            showCloseButton={false}
            aria-describedby={undefined}
            className="block gap-0 p-0 w-72 rounded-lg border border-border bg-card p-4 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <DialogTitle className="text-sm font-medium text-foreground">
              Drop stash?
            </DialogTitle>
            <p className="mt-1.5 text-xs text-muted-foreground">
              Permanently delete{" "}
              <span className="font-mono text-foreground">
                {confirmDrop.ref}
              </span>
              . Cannot be undone.
            </p>
            <div className="mt-3 flex justify-end gap-2">
              <Button variant="outline" type="button" className="text-xs" onClick={() => setConfirmDrop(null)}>
                Cancel
              </Button>
              <Button variant="destructive" type="button" className="text-xs" onClick={handleConfirmDrop}>
                Drop
              </Button>
            </div>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}
