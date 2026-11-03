import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { ChevronsDownUp, FilePlus, Filter, FolderPlus, Loader2, RefreshCw, Search } from "lucide-react";
import { OpenRepo } from "../state";
import { DirEntry, HydrationEntry, mainProcess, RefEntry, rpc } from "../ipc";
import { FileTree } from "../components/FileTree";
import { TransferDropZone } from "../components/ssh/TransferDropZone";
import { useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";
import { useSession, useActiveRepoPath } from "../lib/session-context";
import { useRepoCapabilities } from "../hooks/useRepoCapabilities";

interface Props {
  repo: OpenRepo;
  rev: string | null;
  onChangeRev: (rev: string | null) => void;
  onOpenFile: (path: string, entry: DirEntry) => void;
  activePath: string | null;
  /** Optional dirty-file map from the Changes view. */
  gitStatus?: Record<string, string>;
  /** Callback to refresh git status after file operations. */
  onRefreshGitStatus?: () => void;
  /** Hydration state map keyed by path, for tree decorations. */
  hydrationByPath?: Map<string, HydrationEntry>;
  /** Fired after hydrate/dehydrate finishes so hydration state can
   *  be refreshed by the owning container. */
  onHydrateComplete?: () => void;
}

/**
 * Sidebar content for the Explorer view. Owns:
 *   - the ref dropdown
 *   - the repo stats strip
 *   - the tree listing + filter
 *
 * The tree itself (`<FileTree/>`) is shared with the Changes view —
 * anywhere we render a path-first hierarchy we route through the
 * same @pierre/trees model.
 */
export const ExplorerView = memo(function ExplorerView({
  repo,
  rev,
  onChangeRev,
  onOpenFile,
  gitStatus,
  onRefreshGitStatus,
  hydrationByPath,
  onHydrateComplete,
}: Props) {
  const sessionManager = useSession();
  const repoPath = useActiveRepoPath();

  const [refs, setRefs] = useState<RefEntry[] | null>(null);
  const [entries, setEntries] = useState<DirEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [trackFilter, setTrackFilter] = useState<"all" | "tracked" | "untracked">("all");
  const [collapseKey, setCollapseKey] = useState(0);
  const [newItemMode, setNewItemMode] = useState<"file" | "directory" | null>(null);
  const [newItemName, setNewItemName] = useState("");

  // Capability-based gating for file operations (create/rename/delete).
  const { canEdit } = useRepoCapabilities();

  // Resolve the repo root filesystem path for file operations.
  // Local repos use repo.path; remote repos extract the path from repo_id.
  const repoRootPath = useMemo(() => {
    if (repo.path) return repo.path;
    if (repo.is_remote && repo.repo_id.startsWith("ssh://")) {
      return repo.repo_id.slice(repo.repo_id.indexOf("/", "ssh://".length));
    }
    return undefined;
  }, [repo.path, repo.is_remote, repo.repo_id]);

  // Extract the SSH profile ID from the repo_id for remote repos.
  // Used to route drag-and-drop uploads through the FileTransferManager.
  const remoteProfileId = useMemo(() => {
    if (!repo.is_remote || !repo.repo_id.startsWith("ssh://")) return undefined;
    const withoutScheme = repo.repo_id.slice("ssh://".length);
    const slashIdx = withoutScheme.indexOf("/");
    return slashIdx > 0 ? withoutScheme.slice(0, slashIdx) : undefined;
  }, [repo.is_remote, repo.repo_id]);

  // Restore expanded folder paths from session on mount.
  const initialExpandedPaths = useMemo(() => {
    if (!repoPath) return undefined;
    const session = sessionManager.activate(repoPath);
    const explorer = session?.viewState?.explorer as { expanded?: string[] } | undefined;
    return explorer?.expanded;
  }, [repoPath, sessionManager]);

  // Save expanded folder state to session when it changes.
  const expandedSaveTimer = useRef<ReturnType<typeof setTimeout>>();
  const onExpandedChange = useCallback(
    (expandedPaths: string[]) => {
      if (!repoPath) return;
      clearTimeout(expandedSaveTimer.current);
      expandedSaveTimer.current = setTimeout(() => {
        const session = sessionManager.activate(repoPath);
        const existingViewState = session?.viewState ?? {};
        sessionManager.update(repoPath, {
          viewState: { ...existingViewState, explorer: { expanded: expandedPaths } },
        });
      }, 300);
    },
    [repoPath, sessionManager],
  );

  // Scroll position persistence for the tree's internal scroll container.
  const scrollElRef = useRef<HTMLElement | null>(null);
  const scrollRestoredRef = useRef(false);

  const scrollContainerRef = useCallback(
    (el: HTMLElement | null) => {
      scrollElRef.current = el;
      if (!el || !repoPath || scrollRestoredRef.current) return;

      // Restore scroll position after content renders.
      const saved = sessionManager.getScrollPosition(repoPath, "explorer");
      if (saved == null) {
        scrollRestoredRef.current = true;
        return;
      }

      const attemptRestore = (retries: number) => {
        if (!el) return;
        if (el.scrollHeight < 50 && retries > 0) {
          setTimeout(() => attemptRestore(retries - 1), 100);
          return;
        }
        const maxScroll = el.scrollHeight - el.clientHeight;
        el.scrollTop = Math.min(saved, Math.max(0, maxScroll));
        scrollRestoredRef.current = true;
      };

      requestAnimationFrame(() => attemptRestore(3));
    },
    [repoPath, sessionManager],
  );

  // Save scroll position on scroll (debounced) and on unmount.
  useEffect(() => {
    const el = scrollElRef.current;
    if (!el || !repoPath) return;

    let timer: ReturnType<typeof setTimeout>;
    const onScroll = () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        sessionManager.updateScrollPosition(repoPath, "explorer", el.scrollTop);
      }, 250);
    };

    el.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      el.removeEventListener("scroll", onScroll);
      clearTimeout(timer);
      sessionManager.updateScrollPosition(repoPath, "explorer", el.scrollTop);
    };
  }, [repoPath, sessionManager]);

  // Reset scroll restored flag when repo changes.
  useEffect(() => {
    scrollRestoredRef.current = false;
  }, [repoPath]);

  useEffect(() => {
    setRefs(null);
    rpc
      .listRefs(repo.repo_id)
      .then((r) => setRefs(r.refs))
      .catch(() => setRefs([]));
  }, [repo.repo_id]);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setEntries(null);

    (async () => {
      // Peel annotated tags to a commit before listing: the SDK's
      // tree walk fails on a tag object sha. For branches / HEAD the
      // peel is a no-op. We keep the user-facing dropdown value as
      // the ref name so the UI reads "v3.7.1" rather than a sha.
      let pinned: string | undefined;
      if (rev) {
        try {
          const r = await rpc.resolveRef(repo.repo_id, rev);
          pinned = r.sha;
        } catch {
          pinned = rev; // URL-opened repos; let SDK error surface below
        }
      }

      try {
        const res = await rpc.list(repo.repo_id, pinned ? { rev: pinned } : {});
        if (!cancelled) setEntries(res.entries);
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, rev]);

  const branches = useMemo(() => refs?.filter((r) => r.kind === "branch") ?? [], [refs]);
  const tags = useMemo(() => refs?.filter((r) => r.kind === "tag") ?? [], [refs]);

  const filtered = useMemo(() => {
    if (!entries) return null;
    let result = entries;

    // Merge untracked files from gitStatus into the entry list.
    // `rpc.list()` only returns committed (tracked) files. Untracked
    // files exist only in the gitStatus map with "?" status.
    // Note: git status reports untracked directories with a trailing
    // slash (e.g. "abc/"). We skip those — the tree infers directories
    // from child file paths. Opening a directory path as a file causes
    // EISDIR errors.
    if (gitStatus && trackFilter !== "tracked") {
      const existingPaths = new Set(entries.map((e) => e.path));
      const untrackedEntries: DirEntry[] = Object.entries(gitStatus)
        .filter(([, code]) => code.trim().startsWith("?"))
        .filter(([p]) => !p.endsWith("/"))
        .filter(([p]) => !existingPaths.has(p))
        .map(([p]) => ({
          path: p,
          kind: "file" as const,
          size: null,
          pointer_hash: null,
        }));
      if (untrackedEntries.length > 0) {
        result = [...result, ...untrackedEntries];
      }
    }

    // Apply tracked/untracked filter.
    if (trackFilter !== "all" && gitStatus) {
      const untrackedPaths = new Set(
        Object.entries(gitStatus)
          .filter(([, code]) => code.trim().startsWith("?") || code.trim().startsWith("!"))
          .map(([p]) => p),
      );
      if (trackFilter === "untracked") {
        result = result.filter(
          (e) => e.kind === "dir" || untrackedPaths.has(e.path),
        );
      } else {
        // "tracked" — exclude untracked files.
        result = result.filter(
          (e) => e.kind === "dir" || !untrackedPaths.has(e.path),
        );
      }
    }

    // Apply text filter.
    const f = filter.trim().toLowerCase();
    if (f) {
      result = result.filter((e) => e.path.toLowerCase().includes(f));
    }
    return result;
  }, [entries, filter, trackFilter, gitStatus]);

  const refresh = useCallback(async () => {
    let pinned: string | undefined;
    if (rev) {
      try {
        const r = await rpc.resolveRef(repo.repo_id, rev);
        pinned = r.sha;
      } catch {
        pinned = rev;
      }
    }
    rpc
      .list(repo.repo_id, pinned ? { rev: pinned } : {})
      .then((res) => setEntries(res.entries))
      .catch((e: Error) => setError(e.message));
    // Also refresh git status so newly created/deleted files appear.
    onRefreshGitStatus?.();
  }, [repo.repo_id, rev, onRefreshGitStatus]);

  // Auto-refresh file tree when files change on disk. Uses a moderate
  // debounce since listing the full tree is a non-trivial RPC.
  useFileWatcher(getWatchPath(repo), refresh, 400);

  return (
    <>
      <div className="shrink-0 border-b border-gx-border px-3 py-2">
        <div className="mb-2 flex items-center justify-between text-[10px] uppercase tracking-wider text-gx-muted">
          <span>Revision</span>
          <button type="button" className="text-gx-muted hover:text-gx-text" onClick={refresh} title="Refresh" aria-label="Refresh file tree">
            <RefreshCw size={11} />
          </button>
        </div>
        <select
          className="mb-2 w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
          value={rev ?? ""}
          onChange={(e) => onChangeRev(e.target.value || null)}
        >
          <option value="">HEAD (default)</option>
          {branches.length > 0 && (
            <optgroup label="Branches">
              {branches.map((r) => (
                <option key={r.name} value={r.name}>
                  {r.name.replace(/^refs\/heads\//, "")}
                </option>
              ))}
            </optgroup>
          )}
          {tags.length > 0 && (
            <optgroup label="Tags">
              {tags.map((r) => (
                <option key={r.name} value={r.name}>
                  {r.name.replace(/^refs\/tags\//, "")}
                </option>
              ))}
            </optgroup>
          )}
        </select>
        <div className="relative mt-2">
          <Search size={12} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-gx-muted" />
          <input
            className="w-full rounded border border-gx-border bg-gx-bg-sub pl-7 pr-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
            placeholder="Filter files…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </div>
        {/* Tracked / Untracked filter + action icons */}
        <div className="mt-2 flex items-center gap-1">
          <Filter size={10} className="shrink-0 text-gx-muted" />
          {(["all", "tracked", "untracked"] as const).map((v) => (
            <button
              key={v}
              type="button"
              onClick={() => setTrackFilter(v)}
              className={clsx(
                "rounded px-1.5 py-0.5 text-[10px] transition",
                trackFilter === v
                  ? "bg-gx-accent/20 text-gx-accent"
                  : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
              )}
            >
              {v === "all" ? "All" : v === "tracked" ? "Tracked" : "Untracked"}
            </button>
          ))}
          {/* Right-aligned action icons */}
          <div className="ml-auto flex items-center gap-0.5">
            <button
              type="button"
              className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text disabled:opacity-40 disabled:pointer-events-none"
              title="New File"
              aria-label="New File"
              disabled={!canEdit}
              onClick={() => {
                setNewItemMode("file");
                setNewItemName("");
              }}
            >
              <FilePlus size={13} />
            </button>
            <button
              type="button"
              className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text disabled:opacity-40 disabled:pointer-events-none"
              title="New Folder"
              aria-label="New Folder"
              disabled={!canEdit}
              onClick={() => {
                setNewItemMode("directory");
                setNewItemName("");
              }}
            >
              <FolderPlus size={13} />
            </button>
            <button
              type="button"
              className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
              title="Collapse All"
              aria-label="Collapse All"
              onClick={() => setCollapseKey((k) => k + 1)}
            >
              <ChevronsDownUp size={13} />
            </button>
          </div>
        </div>
      </div>
      <div className="flex min-h-0 flex-1 flex-col">
        {/* Inline new file/folder input */}
        {newItemMode && canEdit && (
          <div className="shrink-0 border-b border-gx-border px-3 py-2">
            <div className="text-[10px] text-gx-muted mb-1">
              {newItemMode === "file" ? "New File" : "New Folder"}
            </div>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const name = newItemName.trim();
                if (!name || !canEdit) return;
                if (!repoRootPath) return;
                const createPath = `${repoRootPath}/${name}`;
                if (newItemMode === "directory") {
                  // Route through the provider: create the directory, then
                  // add a .gitkeep so git tracks it.
                  void mainProcess.fsCreate(repo.repo_id, createPath, "directory").then(() =>
                    mainProcess.fsCreate(repo.repo_id, `${createPath}/.gitkeep`, "file"),
                  ).then(() => {
                    setNewItemMode(null);
                    setNewItemName("");
                    refresh();
                  });
                } else {
                  void mainProcess.fsCreate(repo.repo_id, createPath, "file").then(() => {
                    setNewItemMode(null);
                    setNewItemName("");
                    refresh();
                  });
                }
              }}
              className="flex gap-1"
            >
              <input
                className="flex-1 rounded border border-gx-border bg-gx-bg-sub px-2 py-0.5 text-xs focus-visible:outline-none focus:border-gx-accent"
                placeholder={newItemMode === "file" ? "filename.txt" : "folder-name"}
                value={newItemName}
                onChange={(e) => setNewItemName(e.target.value)}
                autoFocus
                onKeyDown={(e) => {
                  if (e.key === "Escape") {
                    setNewItemMode(null);
                    setNewItemName("");
                  }
                }}
              />
              <button type="submit" className="btn btn-primary text-[10px] px-2 py-0.5" disabled={!newItemName.trim()}>
                Create
              </button>
              <button type="button" className="btn text-[10px] px-2 py-0.5" onClick={() => { setNewItemMode(null); setNewItemName(""); }}>
                Cancel
              </button>
            </form>
          </div>
        )}
        {loading && (
          <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading tree…
          </div>
        )}
        {error && (
          <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {error}
          </div>
        )}
        {!loading && !error && filtered && (
          <MaybeTransferDropZone
            isRemote={repo.is_remote}
            profileId={remoteProfileId}
            remoteDir={repoRootPath}
            onUploadComplete={refresh}
          >
            <FileTree
              key={collapseKey}
              entries={filtered}
              gitStatus={gitStatus}
              hydrationByPath={hydrationByPath}
              onOpenFile={onOpenFile}
              onHydrateComplete={onHydrateComplete}
              workspacePath={repoRootPath}
              repoId={repo.repo_id}
              canEdit={canEdit}
              onRefresh={refresh}
              initialExpandedPaths={initialExpandedPaths}
              onExpandedChange={onExpandedChange}
              scrollContainerRef={scrollContainerRef}
            />
          </MaybeTransferDropZone>
        )}
      </div>
    </>
  );
});


/**
 * Conditionally wraps children with TransferDropZone for remote repos.
 * For local repos, renders children directly without the drop zone overhead.
 */
function MaybeTransferDropZone({
  isRemote,
  profileId,
  remoteDir,
  onUploadComplete,
  children,
}: {
  isRemote: boolean;
  profileId: string | undefined;
  remoteDir: string | undefined;
  onUploadComplete: () => void;
  children: React.ReactNode;
}) {
  if (!isRemote || !profileId || !remoteDir) {
    return <>{children}</>;
  }

  return (
    <TransferDropZone
      profileId={profileId}
      remoteDir={remoteDir}
      enabled={true}
      onUploadComplete={onUploadComplete}
    >
      {children}
    </TransferDropZone>
  );
}
