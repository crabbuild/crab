import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FileTree as PierreFileTree, useFileTree, useFileTreeSelection } from "@pierre/trees/react";
import type { GitStatus, GitStatusEntry } from "@pierre/trees";
import { themeToTreeStyles, prepareFileTreeInput } from "@pierre/trees";
import clsx from "clsx";
import { Clipboard, ClipboardPaste, Clock, Copy, Download, Edit3, ExternalLink, FileX, FilePlus, FolderOpen, FolderPlus, Scissors, Trash2 } from "lucide-react";
import { DirEntry, HydrationEntry, mainProcess, rpc } from "../ipc";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { isEligible, type HydrateVerb } from "../lib/hydrateScope";

// Module-level clipboard for cut/copy operations within the tree.
let clipboardPath: string | null = null;
let clipboardOp: "cut" | "copy" | null = null;

interface Props {
  entries: DirEntry[];
  /** Map of path → git status letter (M, A, D, ...). */
  gitStatus?: Record<string, string>;
  /** Hydration state keyed by path, for tree-row decorations. */
  hydrationByPath?: Map<string, HydrationEntry>;
  /** Native working-tree transfers. An empty map disables mutation in history. */
  fileTransfers?: Partial<Record<HydrateVerb, (paths: string[]) => Promise<unknown>>>;
  onOpenFile: (path: string, entry: DirEntry) => void;
  /** Workspace root for filesystem actions and auxiliary-worktree CLI transfers. */
  workspacePath: string | undefined;
  /** Repo ID passed to file system operations for provider routing. */
  repoId?: string;
  /** Whether the repo supports file editing (create/rename/delete).
   *  Replaces the old `is_remote` guard. Defaults to true. */
  canEdit?: boolean;
  /** How much of the tree to expand by default. `"open"` expands
   *  every folder — the sensible choice for historical trees
   *  (ref/commit views) where the user is exploring an unfamiliar
   *  shape. The live Explorer defaults to collapsed.
   */
  initialExpansion?: "open" | "closed" | number;
  /** Paths to expand on initial mount (overrides initialExpansion for
   *  these specific directories). Used for session restore. */
  initialExpandedPaths?: readonly string[];
  /** Fired when the set of expanded directory paths changes. The
   *  callback receives the full list of currently expanded paths. */
  onExpandedChange?: (expandedPaths: string[]) => void;
  /** Called after a file operation (create/rename/delete) so the
   *  parent can refresh the entry list. */
  onRefresh?: () => void;
  /** Ref callback to access the scroll container element inside the
   *  tree's Shadow DOM (for scroll position persistence). */
  scrollContainerRef?: (el: HTMLElement | null) => void;
}

function toTreesStatus(code: string): GitStatus {
  const trimmed = code.trim();
  switch (trimmed[0]) {
    case "A":
      return "added";
    case "M":
      return "modified";
    case "D":
      return "deleted";
    case "R":
      return "renamed";
    case "?":
      return "untracked";
    case "!":
      return "ignored";
    default:
      return "modified";
  }
}

/**
 * Custom CSS injected into the @pierre/trees Shadow DOM.
 *
 * 1. Reduces left padding (--trees-padding-inline) for a more compact tree.
 * 2. Improves scroll performance by promoting the scroll container to its
 *    own compositor layer and disabling will-change on the sticky overlay
 *    which can cause scrollbar desync.
 */
const TREE_CUSTOM_CSS = `
  /* Reduce left/right padding for a more compact tree */
  :host {
    --trees-padding-inline-override: 6px;
    --trees-item-padding-x-override: 4px;
    --trees-item-margin-x-override: 2px;
    --trees-level-gap-override: 6px;
  }

  /* Promote scroll container to its own layer for smooth scrollbar */
  [data-file-tree-virtualized-scroll='true'] {
    will-change: scroll-position;
    contain: layout style;
  }

  /* Remove will-change: transform from sticky overlay — it causes the
     scrollbar thumb to desync from content during scroll because the
     compositor handles the transform independently from the scroll. */
  [data-file-tree-virtualized-sticky='true'] {
    will-change: auto;
  }

  /* Help the browser skip layout for off-screen rows during fast scroll */
  [data-type='item'] {
    contain: layout style;
    content-visibility: auto;
    contain-intrinsic-size: auto 22px;
  }
`;

/**
 * VS Code-style explorer backed by @pierre/trees.
 *
 * Theming: we feed `themeToTreeStyles` with a Shiki-shaped theme
 * derived from the active Crab theme so the tree's Shadow DOM
 * picks up our dark/light colours without a wall of `unsafeCSS`.
 */
export const FileTree = memo(function FileTree({
  entries,
  gitStatus,
  hydrationByPath,
  fileTransfers,
  onOpenFile,
  workspacePath,
  repoId,
  canEdit = true,
  initialExpansion = "closed",
  initialExpandedPaths,
  onExpandedChange,
  onRefresh,
  scrollContainerRef,
}: Props) {
  const [transferError, setTransferError] = useState<string | null>(null);
  const paths = useMemo(
    () =>
      entries
        .filter((e) => e.kind !== "dir" && e.kind !== "submodule")
        .map((e) => e.path),
    [entries],
  );

  // Let @pierre/trees handle VS Code-style sorting: folders before files,
  // dot-prefixed items first, case-insensitive alphabetical.
  const preparedInput = useMemo(
    () => prepareFileTreeInput(paths, { sort: "default" }),
    [paths],
  );

  const entryByPath = useMemo(() => {
    const m = new Map<string, DirEntry>();
    for (const e of entries) m.set(e.path, e);
    return m;
  }, [entries]);

  const { model } = useFileTree({
    preparedInput,
    initialExpansion,
    initialExpandedPaths,
    dragAndDrop: false,
    renaming: false,
    // Very large overscan (100 rows above/below) ensures that even fast
    // scrolling rarely hits the virtualizer boundary where new rows need
    // to be rendered. This eliminates the scrollbar "jump" on fast scroll.
    // At 22px/row this is ~4400px of pre-rendered content — trivial for
    // a desktop app.
    itemHeight: 22,
    overscan: 100,
    // Use the complete built-in icon set with semantic per-file-type colors.
    icons: { set: "complete", colored: true },
    // Inject CSS into the Shadow DOM to reduce padding and fix scroll.
    unsafeCSS: TREE_CUSTOM_CSS,
  });

  // Keep the tree model in sync with the paths prop, but only when
  // the actual path list changes — not on every re-render caused by
  // the file watcher refreshing entries. Without this guard,
  // `resetPaths` collapses all expanded directories on every refresh.
  const prevPathsRef = useRef<string[]>(paths);
  useEffect(() => {
    const prev = prevPathsRef.current;
    if (prev === paths) return;
    // Fast length check before expensive element-wise comparison.
    if (prev.length === paths.length && prev.every((p, i) => p === paths[i])) {
      prevPathsRef.current = paths;
      return;
    }
    prevPathsRef.current = paths;
    model.resetPaths(paths, { preparedInput });
  }, [model, paths, preparedInput]);

  useEffect(() => {
    if (!gitStatus) return;
    const arr: GitStatusEntry[] = Object.entries(gitStatus).map(([path, code]) => ({
      path,
      status: toTreesStatus(code),
    }));
    model.setGitStatus(arr);
  }, [model, gitStatus]);

  // Report expanded directory paths to the parent when they change.
  useEffect(() => {
    if (!onExpandedChange) return;
    return model.subscribe(() => {
      // Derive all directory paths from the file paths, then check
      // each directory's expansion state via the model API.
      const dirPaths = new Set<string>();
      for (const p of paths) {
        const parts = p.split("/");
        for (let i = 1; i < parts.length; i++) {
          dirPaths.add(parts.slice(0, i).join("/") + "/");
        }
      }
      const expandedPaths: string[] = [];
      for (const dirPath of dirPaths) {
        const item = model.getItem(dirPath);
        if (!item || !item.isDirectory()) continue;
        // After isDirectory() returns true, the item is a FileTreeDirectoryHandle
        // which has isExpanded(). TypeScript doesn't narrow on literal return types,
        // so we use 'in' check.
        if ("isExpanded" in item && (item as { isExpanded(): boolean }).isExpanded()) {
          expandedPaths.push(dirPath);
        }
      }
      onExpandedChange(expandedPaths);
    });
  }, [model, onExpandedChange, paths]);

  // Expose the Shadow DOM scroll container to the parent via callback ref.
  useEffect(() => {
    if (!scrollContainerRef) return;
    // Wait a frame for the Shadow DOM to be ready.
    const rafId = requestAnimationFrame(() => {
      const container = model.getFileTreeContainer();
      if (!container) {
        scrollContainerRef(null);
        return;
      }
      const shadowRoot = container.shadowRoot;
      if (!shadowRoot) {
        scrollContainerRef(null);
        return;
      }
      const scrollEl = shadowRoot.querySelector('[data-file-tree-virtualized-scroll]') as HTMLElement | null;
      scrollContainerRef(scrollEl);
    });
    return () => {
      cancelAnimationFrame(rafId);
      scrollContainerRef(null);
    };
  }, [model, scrollContainerRef]);

  // Selection → open file. Handled in a separate component to avoid
  // re-rendering the entire tree when selection changes.

  // Theme-aware CSS variables fed into the Shadow DOM.
  // Cache the styles at module level to avoid calling getComputedStyle
  // (which forces layout) on every render.
  const effective = useEffectiveTheme();
  const themeStyles = useMemo(() => getCachedTreeThemeStyles(effective), [effective]);

  const runHydrate = useCallback(
    async (paths: string[], verb: HydrateVerb) => {
      setTransferError(null);
      try {
        if (fileTransfers) {
          if (verb === "dehydrate" && !await mainProcess.dialogConfirm(`Dehydrate ${paths.length} ${paths.length === 1 ? "file" : "files"}?`)) return;
          await fileTransfers[verb]?.(paths);
        } else if (workspacePath && verb === "hydrate") {
          await rpc.hydrate(workspacePath, paths);
        } else if (workspacePath) {
          await rpc.dehydrate(workspacePath, paths);
        }
      } catch (error) {
        setTransferError(error instanceof Error ? error.message : String(error));
      }
    },
    [workspacePath, fileTransfers],
  );

  // Context menu with full file operations.
  const renderContextMenu = useCallback(
    (
      item: { kind: "directory" | "file"; path: string },
      context: { close: () => void },
    ) => {
      const isDir = item.kind === "directory";
      const entry = entryByPath.get(item.path);
      const isPointer =
        entry?.kind === "pointer" ||
        entry?.kind === "lfs" ||
        isDir;
      const transferPaths = (verb: HydrateVerb) => {
        if (!fileTransfers) return isPointer && workspacePath ? [isDir ? `${item.path}/**` : item.path] : [];
        if (!fileTransfers[verb] || !hydrationByPath) return [];
        if (isDir) {
          const prefix = `${item.path.replace(/\/$/, "")}/`;
          return Array.from(hydrationByPath.values()).filter(entry => entry.path.startsWith(prefix) && isEligible(entry, verb)).map(entry => entry.path);
        }
        const status = hydrationByPath.get(item.path);
        return status && isEligible(status, verb) ? [item.path] : [];
      };
      const hydratePaths = transferPaths("hydrate");
      const dehydratePaths = transferPaths("dehydrate");

      const fullPath = workspacePath ? `${workspacePath}/${item.path}` : null;

      const handleCreate = async (kind: "file" | "directory") => {
        context.close();
        if (!fullPath) return;
        const name = await mainProcess.dialogPrompt(
          kind === "file" ? "New file name:" : "New folder name:",
        );
        if (!name) return;
        const base = isDir ? fullPath : fullPath.slice(0, fullPath.lastIndexOf("/"));
        if (kind === "directory") {
          if (repoId) {
            // Route through the provider which handles both local and remote.
            // Create the directory itself, then add a .gitkeep so git tracks it.
            await mainProcess.fsCreate(repoId, `${base}/${name}`, "directory");
            await mainProcess.fsCreate(repoId, `${base}/${name}/.gitkeep`, "file");
          } else {
            await mainProcess.fsCreate(`${base}/${name}/.gitkeep`, "file");
          }
        } else {
          if (repoId) {
            await mainProcess.fsCreate(repoId, `${base}/${name}`, "file");
          } else {
            await mainProcess.fsCreate(`${base}/${name}`, "file");
          }
        }
        onRefresh?.();
      };

      const handleRename = async () => {
        context.close();
        if (!fullPath) return;
        const oldName = item.path.split("/").pop() ?? item.path;
        const newName = await mainProcess.dialogPrompt("Rename to:", oldName);
        if (!newName || newName === oldName) return;
        const parent = fullPath.slice(0, fullPath.lastIndexOf("/"));
        if (repoId) {
          await mainProcess.fsRename(repoId, fullPath, `${parent}/${newName}`);
        } else {
          await mainProcess.fsRename(fullPath, `${parent}/${newName}`);
        }
        onRefresh?.();
      };

      const handleDelete = async () => {
        context.close();
        if (!fullPath) return;
        const name = item.path.split("/").pop() ?? item.path;
        const ok = await mainProcess.dialogConfirm(`Move "${name}" to Trash?`);
        if (!ok) return;
        if (repoId) {
          await mainProcess.fsDelete(repoId, fullPath, true);
        } else {
          await mainProcess.fsDelete(fullPath, true);
        }
        onRefresh?.();
      };

      return (
        <ContextMenuBody>
          {/* File operations */}
          {canEdit && workspacePath && (
            <>
              <MenuItem
                icon={<FilePlus size={12} />}
                label="New File"
                onClick={() => void handleCreate("file")}
              />
              <MenuItem
                icon={<FolderPlus size={12} />}
                label="New Folder"
                onClick={() => void handleCreate("directory")}
              />
              <Divider />
            </>
          )}

          {/* Cut / Copy / Paste file */}
          {canEdit && workspacePath && (
            <>
              <MenuItem
                icon={<Scissors size={12} />}
                label="Cut"
                onClick={() => {
                  context.close();
                  clipboardPath = fullPath;
                  clipboardOp = "cut";
                }}
              />
              <MenuItem
                icon={<Clipboard size={12} />}
                label="Copy"
                onClick={() => {
                  context.close();
                  clipboardPath = fullPath;
                  clipboardOp = "copy";
                }}
              />
              {clipboardPath && (
                <MenuItem
                  icon={<ClipboardPaste size={12} />}
                  label="Paste"
                  onClick={() => {
                    context.close();
                    if (!clipboardPath || !fullPath) return;
                    const srcName = clipboardPath.split("/").pop() ?? "";
                    const destDir = isDir ? fullPath : fullPath.slice(0, fullPath.lastIndexOf("/"));
                    const destPath = `${destDir}/${srcName}`;
                    if (clipboardOp === "cut") {
                      const renamePromise = repoId
                        ? mainProcess.fsRename(repoId, clipboardPath, destPath)
                        : mainProcess.fsRename(clipboardPath, destPath);
                      void renamePromise.then(() => {
                        clipboardPath = null;
                        clipboardOp = null;
                        onRefresh?.();
                      });
                    } else {
                      // Copy: use the clipboard path to read and write
                      void navigator.clipboard.writeText(clipboardPath);
                      // For now, copy is a rename to a new name (append " copy")
                      // A proper copy would need a new IPC. For files, we can
                      // just inform the user.
                      const renamePromise = repoId
                        ? mainProcess.fsRename(repoId, clipboardPath, destPath)
                        : mainProcess.fsRename(clipboardPath, destPath);
                      void renamePromise.catch(() => {
                        // If rename fails (cross-device), fall back to nothing
                      }).then(() => onRefresh?.());
                    }
                  }}
                />
              )}
              <Divider />
            </>
          )}

          {/* Copy path */}
          <MenuItem
            icon={<Copy size={12} />}
            label="Copy Path"
            onClick={() => {
              context.close();
              void navigator.clipboard.writeText(item.path);
            }}
          />
          {fullPath && (
            <MenuItem
              icon={<Copy size={12} />}
              label="Copy Absolute Path"
              onClick={() => {
                context.close();
                void navigator.clipboard.writeText(fullPath);
              }}
            />
          )}
          {canEdit && workspacePath && (
            <>
              <Divider />
              <MenuItem
                icon={<Edit3 size={12} />}
                label="Rename"
                onClick={() => void handleRename()}
              />
              <MenuItem
                icon={<Trash2 size={12} />}
                label="Move to Trash"
                onClick={() => void handleDelete()}
              />
            </>
          )}

          {/* Reveal */}
          {fullPath && (
            <>
              <Divider />
              <MenuItem
                icon={<FolderOpen size={12} />}
                label="Reveal in Finder"
                onClick={() => {
                  context.close();
                  void mainProcess.fsReveal(fullPath);
                }}
              />
              <MenuItem
                icon={<ExternalLink size={12} />}
                label="Open with Default App"
                onClick={() => {
                  context.close();
                  void mainProcess.fsOpenExternal(fullPath);
                }}
              />
            </>
          )}

          {/* Git Time Machine (files only) */}
          {!isDir && (
            <>
              <Divider />
              <MenuItem
                icon={<Clock size={12} />}
                label="Git Time Machine"
                onClick={() => {
                  context.close();
                  window.dispatchEvent(
                    new CustomEvent("crab:open-time-machine", {
                      detail: { path: item.path },
                    }),
                  );
                }}
              />
            </>
          )}

          {/* Hydrate / Dehydrate */}
          {(hydratePaths.length > 0 || dehydratePaths.length > 0) && (
            <>
              <Divider />
              {hydratePaths.length > 0 && <MenuItem
                icon={<Download size={12} />}
                label={isDir ? "Hydrate Subtree" : "Hydrate File"}
                onClick={() => {
                  context.close();
                  void runHydrate(hydratePaths, "hydrate");
                }}
              />}
              {dehydratePaths.length > 0 && <MenuItem
                icon={<FileX size={12} />}
                label={isDir ? "Dehydrate Subtree" : "Dehydrate File"}
                onClick={() => {
                  context.close();
                  void runHydrate(dehydratePaths, "dehydrate");
                }}
              />}
            </>
          )}

          {/* Pointer hash */}
          {entry?.pointer_hash && (
            <>
              <Divider />
              <MenuItem
                icon={<Copy size={12} />}
                label="Copy Pointer Hash"
                onClick={() => {
                  context.close();
                  void navigator.clipboard.writeText(entry.pointer_hash ?? "");
                }}
              />
            </>
          )}
        </ContextMenuBody>
      );
    },
    [entryByPath, fileTransfers, hydrationByPath, runHydrate, workspacePath, repoId, canEdit, onRefresh],
  );

  return (
    <div className="relative min-h-0 flex-1 overflow-hidden">
      {transferError && <div role="alert" className="px-3 py-2 text-xs text-gx-error">{transferError}</div>}
      <SelectionHandler model={model} entryByPath={entryByPath} onOpenFile={onOpenFile} />
      <PierreFileTree
        model={model}
        className="block h-full"
        style={themeStyles}
        renderContextMenu={
          renderContextMenu as unknown as (
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            item: any,
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            context: any,
          ) => React.ReactNode
        }
      />
      {hydrationByPath && hydrationByPath.size > 0 && (
        <div className="absolute bottom-0 left-0 right-0 bg-gx-surface/80 px-3 py-1 text-[10px] text-gx-muted">
          <HydrationLegend byPath={hydrationByPath} />
        </div>
      )}
    </div>
  );
});

/**
 * Lightweight component that subscribes to selection changes without
 * causing the parent FileTree to re-render. This prevents scroll jank
 * from useSyncExternalStore firing on every model event.
 */
function SelectionHandler({
  model,
  entryByPath,
  onOpenFile,
}: {
  model: ReturnType<typeof useFileTree>["model"];
  entryByPath: Map<string, DirEntry>;
  onOpenFile: (path: string, entry: DirEntry) => void;
}) {
  const selection = useFileTreeSelection(model);
  const lastOpened = useRef<string | null>(null);
  useEffect(() => {
    const first = selection[0];
    if (!first || first === lastOpened.current) return;
    // Skip directory paths (trailing slash from git status or tree inference).
    if (first.endsWith("/")) return;
    const entry = entryByPath.get(first);
    if (!entry) return;
    if (entry.kind === "dir" || entry.kind === "submodule") return;
    lastOpened.current = first;
    onOpenFile(first, entry);
  }, [selection, entryByPath, onOpenFile]);
  return null;
}

// Module-level cache for tree theme styles. Avoids calling
// getComputedStyle (which forces synchronous layout) on every render.
let _cachedThemeMode: "dark" | "light" | null = null;
let _cachedThemeStyles: React.CSSProperties | null = null;
let _cacheVersion = 0;
const CURRENT_CACHE_VERSION = 2; // Bump to invalidate after code changes

function getCachedTreeThemeStyles(mode: "dark" | "light"): React.CSSProperties {
  if (_cachedThemeMode === mode && _cachedThemeStyles && _cacheVersion === CURRENT_CACHE_VERSION) {
    return _cachedThemeStyles;
  }
  _cachedThemeStyles = buildTreeThemeStyles(mode);
  _cachedThemeMode = mode;
  _cacheVersion = CURRENT_CACHE_VERSION;
  return _cachedThemeStyles;
}

/**
 * Build Shiki-shaped theme input for `themeToTreeStyles`, using the
 * same colour values the rest of the app reads from CSS variables.
 * We rely on `getComputedStyle(document.documentElement)` to read
 * the active `--gx-*` tokens so the tree stays visually aligned
 * with sidebars, buttons, etc.
 */
function buildTreeThemeStyles(mode: "dark" | "light"): React.CSSProperties {
  const cs =
    typeof window !== "undefined"
      ? getComputedStyle(document.documentElement)
      : null;
  const rgb = (name: string, fallback: string): string => {
    const raw = cs?.getPropertyValue(name).trim();
    if (!raw) return fallback;
    return `rgb(${raw})`;
  };
  const colors: Record<string, string> = {
    "sideBar.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "editor.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "sideBar.foreground": rgb("--gx-text", mode === "dark" ? "#e7eaf1" : "#171b23"),
    "sideBarSectionHeader.foreground": rgb("--gx-muted", "#8a93a5"),
    "sideBar.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "list.hoverBackground": rgb(
      "--gx-surface-hover",
      mode === "dark" ? "#1e222b" : "#e9ecf3",
    ),
    "list.activeSelectionBackground":
      mode === "dark" ? "#4f8cff26" : "#2c6def1f",
    "list.activeSelectionForeground":
      mode === "dark" ? "#e7eaf1" : "#171b23",
    "focusBorder": rgb("--gx-accent", mode === "dark" ? "#4f8cff" : "#2c6def"),
    "input.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "input.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "scrollbarSlider.background": rgb(
      "--gx-border",
      mode === "dark" ? "#242832" : "#dde0e8",
    ),
    "gitDecoration.addedResourceForeground": rgb(
      "--gx-success",
      mode === "dark" ? "#4ade80" : "#16a34a",
    ),
    "gitDecoration.modifiedResourceForeground": rgb(
      "--gx-accent",
      mode === "dark" ? "#4f8cff" : "#2c6def",
    ),
    "gitDecoration.deletedResourceForeground": rgb(
      "--gx-error",
      mode === "dark" ? "#f87171" : "#dc2626",
    ),
  };
  const styles = themeToTreeStyles({
    type: mode,
    bg: colors["sideBar.background"],
    fg: colors["sideBar.foreground"],
    colors,
  });
  return styles as unknown as React.CSSProperties;
}

function ContextMenuBody({ children }: { children: React.ReactNode }) {
  return (
    <div
      className="min-w-[240px] overflow-hidden rounded-md border border-gx-border bg-gx-surface p-1 shadow-2xl"
      data-file-tree-context-menu-root="true"
    >
      {children}
    </div>
  );
}

function MenuItem({
  icon,
  label,
  hint,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "flex w-full items-center gap-2 whitespace-nowrap rounded px-2 py-1.5 text-left text-[12.5px] text-gx-text transition",
        "hover:bg-gx-surface-hover",
      )}
    >
      <span className="shrink-0 text-gx-muted">{icon}</span>
      <span className="flex-1">{label}</span>
      {hint && <span className="ml-3 text-[10px] text-gx-muted">{hint}</span>}
    </button>
  );
}

function Divider() {
  return <div className="my-1 border-t border-gx-border/60" />;
}

function HydrationLegend({ byPath }: { byPath: Map<string, HydrationEntry> }) {
  let hydrated = 0;
  let pointer = 0;
  let partial = 0;
  for (const e of byPath.values()) {
    if (e.state === "hydrated") hydrated++;
    else if (e.state === "pointer") pointer++;
    else if (e.state === "partial") partial++;
  }
  return (
    <div className="flex items-center gap-3">
      <span className="flex items-center gap-1">
        <span className="inline-block h-2 w-2 rounded-full bg-gx-success" /> {hydrated} hydrated
      </span>
      <span className="flex items-center gap-1">
        <span className="inline-block h-2 w-2 rounded-full bg-gx-accent/60" /> {pointer} pointer
      </span>
      {partial > 0 && (
        <span className="flex items-center gap-1">
          <span className="inline-block h-2 w-2 rounded-full bg-gx-warn" /> {partial} partial
        </span>
      )}
    </div>
  );
}
