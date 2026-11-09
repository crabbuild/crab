import { Component, lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import type { ErrorInfo, ReactNode } from "react";
import clsx from "clsx";
import { Binary, Code2, Eye, GitCompareArrows, Loader2, Pin, X } from "lucide-react";
import { DirEntry, mainProcess, rpc } from "../ipc";
import { OpenRepo } from "../state";
import { base64ToBytes, decodeText, hexDump } from "../lib/binary";
import { previewKindFor, PreviewKind } from "../lib/preview-kind";
import { useSetting } from "../settings";
import { useRepoCapabilities } from "../hooks/useRepoCapabilities";
import { useRemoteAutoSave } from "../hooks/useRemoteAutoSave";
import { useExternalFileChange } from "../hooks/useExternalFileChange";
import { ExternalChangePrompt } from "./ExternalChangePrompt";
import { FileInspector } from "./FileInspector";
import { FileIcon } from "./FileIcon";
import { CodeEditor } from "./CodeEditor";
import { formatBytes } from "../lib/format";
import { ImageView } from "./previews/ImageView";
import { MarkdownView } from "./previews/MarkdownView";
import { JsonView } from "./previews/JsonView";
import { CsvView } from "./previews/CsvView";
import { ParquetView } from "./previews/ParquetView";
import { SafetensorsView } from "./previews/SafetensorsView";
import { NumpyView } from "./previews/NumpyView";
import { ZipView } from "./previews/ZipView";
import { NotebookView } from "./previews/NotebookView";
import { PdfView } from "./previews/PdfView";
import { VideoView } from "./previews/VideoView";
import { AudioView } from "./previews/AudioView";
import { GeoJsonView } from "./previews/GeoJsonView";
import { SqliteView } from "./previews/SqliteView";
import { OnnxView } from "./previews/OnnxView";
import { Hdf5View } from "./previews/Hdf5View";
import { ProtobufView } from "./previews/ProtobufView";
const Model3dView = lazy(() => import("./previews/Model3dView").then(m => ({ default: m.Model3dView })));
import { TorchCheckpointView } from "./previews/TorchCheckpointView";
import { CardEditorView } from "../views/CardEditorView";

export interface EditorTab {
  key: string;
  path: string;
  rev: string | null;
  entry: DirEntry;
  /** Optional line to jump to when the tab opens. */
  line?: number;
}

interface DiffTab {
  key: string;
  path: string;
}

interface Props {
  repo: OpenRepo;
  tabs: EditorTab[];
  diffTabs: DiffTab[];
  activeKey: string | null;
  onActivate: (key: string) => void;
  onClose: (key: string) => void;
  /** Close all tabs. */
  onCloseAll?: () => void;
  /** Close all tabs except the given key. */
  onCloseOthers?: (key: string) => void;
  /** Close all tabs to the right of the given key. */
  onCloseToRight?: (key: string) => void;
  /** Close all saved (non-dirty) tabs. Receives the current dirty keys set. */
  onCloseSaved?: (dirtyKeys: Set<string>) => void;
  /** Pin or unpin a tab. */
  onPin?: (key: string, pinned: boolean) => void;
  /** Split a tab into a new editor group (right). */
  onSplit?: (key: string) => void;
  /** Split a tab into a new editor group (down). */
  onSplitDown?: (key: string) => void;
  /** When the active tab is a diff, the caller passes the rendered
   *  DiffView here so this component doesn't need to import it and
   *  we keep EditorPane free of @pierre/diffs at the top level. */
  renderDiff: React.ReactNode;
  /** Set of pinned tab keys (managed externally). */
  pinnedKeys?: Set<string>;
}

export function EditorPane({
  repo,
  tabs,
  diffTabs,
  activeKey,
  onActivate,
  onClose,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onCloseSaved,
  onPin,
  onSplit,
  onSplitDown,
  renderDiff,
  pinnedKeys,
}: Props) {
  const totalTabs = tabs.length + diffTabs.length;
  const activeEditor = tabs.find((t) => t.key === activeKey) ?? null;
  const activeDiff = diffTabs.find((t) => t.key === activeKey) ?? null;

  // Track which tabs have unsaved changes (dirty state).
  const [dirtyKeys, setDirtyKeys] = useState<Set<string>>(new Set());
  const markDirty = useCallback((key: string, dirty: boolean) => {
    setDirtyKeys((prev) => {
      const next = new Set(prev);
      if (dirty) next.add(key);
      else next.delete(key);
      return next;
    });
  }, []);

  // Combine all tab keys for context menu index calculations.
  const allKeys = [...tabs.map((t) => t.key), ...diffTabs.map((t) => t.key)];

  if (totalTabs === 0) {
    return <div className="h-full bg-gx-bg-sub" />;
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <div className="flex shrink-0 items-center overflow-x-auto scrollbar-none border-b border-gx-border bg-gx-surface">
        {tabs.map((t, idx) => (
          <TabPill
            key={t.key}
            active={t.key === activeKey}
            dirty={dirtyKeys.has(t.key)}
            pinned={pinnedKeys?.has(t.key) ?? false}
            onActivate={() => onActivate(t.key)}
            onClose={() => onClose(t.key)}
            onCloseAll={onCloseAll}
            onCloseOthers={onCloseOthers ? () => onCloseOthers(t.key) : undefined}
            onCloseToRight={onCloseToRight ? () => onCloseToRight(t.key) : undefined}
            onCloseSaved={onCloseSaved ? () => onCloseSaved(dirtyKeys) : undefined}
            onPin={onPin ? (pinned) => onPin(t.key, pinned) : undefined}
            onSplit={onSplit ? () => onSplit(t.key) : undefined}
            onSplitDown={onSplitDown ? () => onSplitDown(t.key) : undefined}
            label={t.path.split("/").pop() ?? t.path}
            icon={<FileIcon path={t.path} kind={t.entry.kind} />}
            totalTabs={totalTabs}
            isLastTab={idx === allKeys.length - 1}
            filePath={t.path}
            repoPath={repo.path ?? null}
          />
        ))}
        {diffTabs.map((t, idx) => (
          <TabPill
            key={t.key}
            active={t.key === activeKey}
            dirty={false}
            pinned={pinnedKeys?.has(t.key) ?? false}
            onActivate={() => onActivate(t.key)}
            onClose={() => onClose(t.key)}
            onCloseAll={onCloseAll}
            onCloseOthers={onCloseOthers ? () => onCloseOthers(t.key) : undefined}
            onCloseToRight={onCloseToRight ? () => onCloseToRight(t.key) : undefined}
            onCloseSaved={onCloseSaved ? () => onCloseSaved(dirtyKeys) : undefined}
            onPin={onPin ? (pinned) => onPin(t.key, pinned) : undefined}
            onSplit={onSplit ? () => onSplit(t.key) : undefined}
            onSplitDown={onSplitDown ? () => onSplitDown(t.key) : undefined}
            label={(t.path.split("/").pop() ?? t.path) + " · diff"}
            icon={<GitCompareArrows size={13} className="text-gx-accent" />}
            totalTabs={totalTabs}
            isLastTab={tabs.length + idx === allKeys.length - 1}
            filePath={t.path}
            repoPath={repo.path ?? null}
          />
        ))}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        {activeEditor && (
          <div className="h-full w-full" key={activeEditor.key}>
            <EditorErrorBoundary path={activeEditor.path}>
              <Router repo={repo} tab={activeEditor} onDirtyChange={markDirty} />
            </EditorErrorBoundary>
          </div>
        )}
        {activeDiff && renderDiff}
      </div>
    </div>
  );
}

function TabPill({
  active,
  dirty,
  pinned,
  onActivate,
  onClose,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onCloseSaved,
  onPin,
  onSplit,
  onSplitDown,
  label,
  icon,
  totalTabs,
  isLastTab,
  filePath,
  repoPath,
}: {
  active: boolean;
  dirty: boolean;
  pinned: boolean;
  onActivate: () => void;
  onClose: () => void;
  onCloseAll?: () => void;
  onCloseOthers?: () => void;
  onCloseToRight?: () => void;
  onCloseSaved?: () => void;
  onPin?: (pinned: boolean) => void;
  onSplit?: () => void;
  onSplitDown?: () => void;
  label: string;
  icon: React.ReactNode;
  totalTabs: number;
  isLastTab: boolean;
  filePath: string;
  repoPath: string | null;
}) {
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu({ x: e.clientX, y: e.clientY });
  }, []);

  const handleMiddleClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.button === 1 && !pinned) {
        e.preventDefault();
        onClose();
      }
    },
    [pinned, onClose],
  );

  return (
    <>
      <div
        className={clsx(
          "group flex h-9 shrink-0 items-center gap-2 border-r border-gx-border px-3 text-[12px] transition",
          active ? "bg-gx-bg-sub text-gx-text" : "text-gx-muted hover:bg-gx-surface-hover",
          pinned && "italic",
        )}
        onContextMenu={handleContextMenu}
        onMouseDown={handleMiddleClick}
      >
        {/* Pin indicator */}
        {pinned && (
          <Pin size={10} className="shrink-0 text-gx-accent rotate-45" />
        )}

        <button type="button" onClick={onActivate} className="flex items-center gap-2">
          {icon}
          <span>{label}</span>
        </button>
        {dirty ? (
          <span
            className="h-2.5 w-2.5 shrink-0 rounded-full bg-gx-muted/70"
            title="Unsaved changes"
            aria-label="Unsaved changes"
          />
        ) : !pinned ? (
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onClose();
            }}
            className="opacity-0 transition group-hover:opacity-100 hover:text-gx-text"
            aria-label="Close tab"
            title="Close"
          >
            <X size={12} />
          </button>
        ) : null}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <TabPillContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          pinned={pinned}
          totalTabs={totalTabs}
          isLastTab={isLastTab}
          filePath={filePath}
          repoPath={repoPath}
          onClose={() => { setContextMenu(null); onClose(); }}
          onCloseAll={onCloseAll ? () => { setContextMenu(null); onCloseAll(); } : undefined}
          onCloseOthers={onCloseOthers ? () => { setContextMenu(null); onCloseOthers(); } : undefined}
          onCloseToRight={onCloseToRight ? () => { setContextMenu(null); onCloseToRight(); } : undefined}
          onCloseSaved={onCloseSaved ? () => { setContextMenu(null); onCloseSaved(); } : undefined}
          onPin={onPin ? (p) => { setContextMenu(null); onPin(p); } : undefined}
          onSplit={onSplit ? () => { setContextMenu(null); onSplit(); } : undefined}
          onSplitDown={onSplitDown ? () => { setContextMenu(null); onSplitDown(); } : undefined}
          onDismiss={() => setContextMenu(null)}
        />
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// TabPillContextMenu — VS Code-style right-click context menu for tabs.
// ---------------------------------------------------------------------------

function TabPillContextMenu({
  x,
  y,
  pinned,
  totalTabs,
  isLastTab,
  filePath,
  repoPath,
  onClose,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onCloseSaved,
  onPin,
  onSplit,
  onSplitDown,
  onDismiss,
}: {
  x: number;
  y: number;
  pinned: boolean;
  totalTabs: number;
  isLastTab: boolean;
  filePath: string;
  repoPath: string | null;
  onClose: () => void;
  onCloseAll?: () => void;
  onCloseOthers?: () => void;
  onCloseToRight?: () => void;
  onCloseSaved?: () => void;
  onPin?: (pinned: boolean) => void;
  onSplit?: () => void;
  onSplitDown?: () => void;
  onDismiss: () => void;
}) {
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        onDismiss();
      }
    };
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDismiss();
    };
    document.addEventListener("mousedown", handleClickOutside);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleClickOutside);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [onDismiss]);

  // Adjust position so the menu doesn't overflow the viewport.
  const [pos, setPos] = useState({ x, y });
  useEffect(() => {
    if (!menuRef.current) return;
    const rect = menuRef.current.getBoundingClientRect();
    let adjustedX = x;
    let adjustedY = y;
    if (x + rect.width > window.innerWidth) {
      adjustedX = window.innerWidth - rect.width - 4;
    }
    if (y + rect.height > window.innerHeight) {
      adjustedY = window.innerHeight - rect.height - 4;
    }
    setPos({ x: adjustedX, y: adjustedY });
  }, [x, y]);

  const absolutePath = repoPath ? `${repoPath}/${filePath}` : filePath;

  const handleCopyPath = useCallback(() => {
    void navigator.clipboard.writeText(absolutePath);
    onDismiss();
  }, [absolutePath, onDismiss]);

  const handleCopyRelativePath = useCallback(() => {
    void navigator.clipboard.writeText(filePath);
    onDismiss();
  }, [filePath, onDismiss]);

  const handleRevealInFinder = useCallback(() => {
    if (repoPath) {
      void mainProcess.fsOpenExternal(absolutePath);
    }
    onDismiss();
  }, [repoPath, absolutePath, onDismiss]);

  return (
    <div
      ref={menuRef}
      className="fixed z-50 min-w-[220px] rounded-md border border-gx-border bg-gx-surface py-1 shadow-lg text-[12px]"
      style={{ left: pos.x, top: pos.y }}
      role="menu"
    >
      {/* --- Close group --- */}
      {!pinned && (
        <ContextMenuItemPane label="Close" shortcut="⌘W" onClick={onClose} />
      )}
      {onCloseOthers && totalTabs > 1 && (
        <ContextMenuItemPane label="Close Others" shortcut="⌥⌘T" onClick={onCloseOthers} />
      )}
      {onCloseToRight && !isLastTab && (
        <ContextMenuItemPane label="Close to the Right" onClick={onCloseToRight} />
      )}
      {onCloseSaved && (
        <ContextMenuItemPane label="Close Saved" shortcut="⌘K U" onClick={onCloseSaved} />
      )}
      {onCloseAll && (
        <ContextMenuItemPane label="Close All" shortcut="⌘K W" onClick={onCloseAll} />
      )}

      {/* --- Separator --- */}
      <div className="my-1 border-t border-gx-border" role="separator" />

      {/* --- Copy path group --- */}
      <ContextMenuItemPane label="Copy Path" shortcut="⇧⌘C" onClick={handleCopyPath} />
      <ContextMenuItemPane label="Copy Relative Path" shortcut="⌥⇧⌘C" onClick={handleCopyRelativePath} />

      {/* --- Separator --- */}
      <div className="my-1 border-t border-gx-border" role="separator" />

      {/* --- Reveal --- */}
      {repoPath && (
        <ContextMenuItemPane label="Reveal in Finder" shortcut="⌥⌘R" onClick={handleRevealInFinder} />
      )}

      {/* --- Separator --- */}
      {repoPath && (
        <div className="my-1 border-t border-gx-border" role="separator" />
      )}

      {/* --- Pin --- */}
      {onPin && (
        <ContextMenuItemPane
          label={pinned ? "Unpin" : "Pin"}
          shortcut="⌘K ⇧Enter"
          onClick={() => onPin(!pinned)}
        />
      )}

      {/* --- Separator --- */}
      {(onSplit || onSplitDown) && (
        <div className="my-1 border-t border-gx-border" role="separator" />
      )}

      {/* --- Split group --- */}
      {onSplit && (
        <ContextMenuItemPane label="Split Right" shortcut="⌘\\" onClick={onSplit} />
      )}
      {onSplitDown && (
        <ContextMenuItemPane label="Split Down" onClick={onSplitDown} />
      )}
    </div>
  );
}

function ContextMenuItemPane({
  label,
  shortcut,
  onClick,
  disabled,
}: {
  label: string;
  shortcut?: string;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      disabled={disabled}
      onClick={onClick}
      className={clsx(
        "flex w-full items-center justify-between px-3 py-1.5 text-left transition",
        disabled
          ? "text-gx-muted cursor-not-allowed"
          : "text-gx-text hover:bg-gx-surface-hover",
      )}
    >
      <span>{label}</span>
      {shortcut && <span className="ml-4 text-[10px] text-gx-muted">{shortcut}</span>}
    </button>
  );
}

function Router(props: { repo: OpenRepo; tab: EditorTab; onDirtyChange: (key: string, dirty: boolean) => void }) {
  const [details, setDetails] = useState(false);
  const { repo, tab } = props;
  const pointer = tab.entry.kind === "pointer" || tab.entry.kind === "lfs";
  if (!pointer || previewKindFor(tab.path, tab.entry.kind) === "pointer") return <PreviewBody {...props} />;
  return <div className="flex h-full min-h-0 flex-col">
    <div className="flex shrink-0 justify-end border-b border-gx-border px-3 py-1">
      <button type="button" className="btn text-xs" aria-pressed={details} onClick={() => setDetails(!details)}>
        {details ? "Back to preview" : "File details"}
      </button>
    </div>
    <div className={clsx("min-h-0 flex-1", details && "hidden")}><PreviewBody {...props} /></div>
    {details && <div className="min-h-0 flex-1"><FileInspector repo={repo} path={tab.path} rev={tab.rev} /></div>}
  </div>;
}

function PreviewBody({ repo, tab, onDirtyChange }: { repo: OpenRepo; tab: EditorTab; onDirtyChange: (key: string, dirty: boolean) => void }) {
  const kind: PreviewKind = previewKindFor(tab.path, tab.entry.kind);
  const common = { repoId: repo.repo_id, path: tab.path, rev: tab.rev } as const;
  const capabilities = useRepoCapabilities();

  // Editable markdown: when viewing the working tree of a repo with
  // edit capabilities, open markdown in the code editor so the user can edit it.
  const canEdit = capabilities.canEdit && tab.rev === null;

  if (kind === "pointer") return <FileInspector repo={repo} path={tab.path} rev={tab.rev} />;
  if (kind === "card-editor") return <CardEditorView repo={repo} path={tab.path} />;
  if (kind === "image") return <ImageView {...common} entryKind={tab.entry.kind} />;
  if (kind === "video") return <VideoView {...common} />;
  if (kind === "audio") return <AudioView {...common} />;
  if (kind === "markdown") {
    if (canEdit) {
      return <MarkdownEditorBody repo={repo} tab={tab} onDirtyChange={onDirtyChange} />;
    }
    return <MarkdownView {...common} />;
  }
  if (kind === "json") return <JsonView {...common} />;
  if (kind === "jsonl") return <JsonView {...common} jsonl />;
  if (kind === "csv") return <CsvView {...common} sep="," />;
  if (kind === "tsv") return <CsvView {...common} sep={"\t"} />;
  if (kind === "parquet") return <ParquetView {...common} />;
  if (kind === "arrow") return <ParquetView {...common} />;
  if (kind === "safetensors") return <SafetensorsView {...common} />;
  if (kind === "npy") return <NumpyView kind="npy" {...common} />;
  if (kind === "npz") return <NumpyView kind="npz" {...common} />;
  if (kind === "zip") return <ZipView {...common} />;
  if (kind === "notebook") return <NotebookView {...common} />;
  if (kind === "pdf") return <PdfView {...common} />;
  if (kind === "geojson") return <GeoJsonView {...common} />;
  if (kind === "sqlite") return <SqliteView {...common} />;
  if (kind === "onnx") return <OnnxView {...common} />;
  if (kind === "hdf5") return <Hdf5View {...common} />;
  if (kind === "protobuf") return <ProtobufView {...common} />;
  if (kind === "model3d") return <Suspense fallback={<div className="flex h-full items-center justify-center text-sm text-gx-muted">Loading 3D viewer…</div>}><Model3dView {...common} /></Suspense>;
  if (kind === "torch-checkpoint") return <TorchCheckpointView {...common} />;
  return <TextBody repo={repo} tab={tab} onDirtyChange={onDirtyChange} />;
}

function TextBody({ repo, tab, onDirtyChange }: { repo: OpenRepo; tab: EditorTab; onDirtyChange: (key: string, dirty: boolean) => void }) {
  type LoadState =
    | { state: "loading" }
    | { state: "error"; message: string }
    | { state: "text"; text: string }
    | { state: "binary"; bytes: Uint8Array; size: number }
    | { state: "truncated"; reason: string; size: number };

  const [load, setLoad] = useState<LoadState>({ state: "loading" });
  const maxKib = useSetting("maxPreviewKib");
  const maxBytes = Math.max(maxKib * 1024, 64 * 1024);
  const capabilities = useRepoCapabilities();

  // Editing is allowed when the repo supports editing and viewing the working tree (rev === null).
  const canEdit = capabilities.canEdit && tab.rev === null;

  // Transport routing: local files save via fs, remote files save via RPC.
  const isLocal = !!repo.path && !repo.is_remote;

  // Remote auto-save hook — handles debounced writes, failure persistence,
  // and reconnection retry for remote files.
  const remoteAutoSave = useRemoteAutoSave(
    repo.repo_id,
    tab.path,
    tab.key,
    onDirtyChange,
  );

  // Local auto-save: debounced write to disk on every content change.
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isSavingRef = useRef(false);
  const pendingContentRef = useRef<string | null>(null);

  // Track dirty state for external change detection.
  const [isDirty, setIsDirty] = useState(false);

  // External file change detection: when a file change event arrives,
  // re-read the file and either silently reload or prompt the user.
  const handleExternalReload = useCallback((newContent: string) => {
    setLoad({ state: "text", text: newContent });
    setIsDirty(false);
    onDirtyChange(tab.key, false);
    // Clear any pending local save since we're reloading.
    pendingContentRef.current = null;
    if (saveTimerRef.current) {
      clearTimeout(saveTimerRef.current);
      saveTimerRef.current = null;
    }
  }, [onDirtyChange, tab.key]);

  const { showPrompt: showExternalChangePrompt, reloadError, acceptReload, keepMine } = useExternalFileChange({
    repo,
    filePath: tab.path,
    isDirty,
    currentContent: load.state === "text" ? load.text : null,
    maxBytes,
    enabled: canEdit && load.state === "text",
    onReload: handleExternalReload,
  });

  const writeToDisk = useCallback(
    async (content: string) => {
      isSavingRef.current = true;
      try {
        if (isLocal) {
          const fullPath = `${repo.path}/${tab.path}`;
          await mainProcess.fsWriteFile(fullPath, content);
        } else {
          await rpc.writeText(repo.repo_id, tab.path, content);
        }
        pendingContentRef.current = null;
        setIsDirty(false);
        onDirtyChange(tab.key, false);
      } catch (e) {
        console.error("Failed to save file:", e);
      } finally {
        isSavingRef.current = false;
      }
    },
    [isLocal, repo.path, repo.repo_id, tab.path, onDirtyChange, tab.key],
  );

  // Called on every keystroke — debounces the actual write.
  const handleChange = useCallback(
    (content: string) => {
      if (!isLocal) {
        // Remote file: delegate to the remote auto-save hook.
        remoteAutoSave.handleChange(content);
        setIsDirty(true);
        return;
      }
      pendingContentRef.current = content;
      setIsDirty(true);
      onDirtyChange(tab.key, true);
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
      }
      saveTimerRef.current = setTimeout(() => {
        saveTimerRef.current = null;
        void writeToDisk(content);
      }, 300);
    },
    [isLocal, remoteAutoSave, writeToDisk, onDirtyChange, tab.key],
  );

  // Immediate save on Cmd+S.
  const handleSave = useCallback(
    (content: string) => {
      if (!isLocal) {
        remoteAutoSave.handleSave(content);
        setIsDirty(false);
        return;
      }
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      pendingContentRef.current = null;
      setIsDirty(false);
      void writeToDisk(content);
    },
    [isLocal, remoteAutoSave, writeToDisk],
  );

  // Flush any pending save when the component unmounts (tab closed).
  useEffect(() => {
    return () => {
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      // Flush unsaved content synchronously on unmount (local only).
      const pending = pendingContentRef.current;
      if (pending !== null && repo.path && isLocal) {
        const fullPath = `${repo.path}/${tab.path}`;
        // Fire-and-forget — component is unmounting so we can't await.
        void mainProcess.fsWriteFile(fullPath, pending);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo.path, tab.path, isLocal]);

  useEffect(() => {
    const controller = new AbortController();
    setLoad({ state: "loading" });
    rpc.readBytes(repo.repo_id, tab.path, {
      rev: tab.rev ?? undefined, max_bytes: maxBytes, signal: controller.signal,
    }).then((res) => {
      if (controller.signal.aborted) return;
      if (res.truncated) {
        setLoad({ state: "truncated", reason: res.reason ?? "File exceeds the preview limit", size: res.size });
        return;
      }
      const bytes = res.bytes_b64 ? base64ToBytes(res.bytes_b64) : new Uint8Array();
      const text = decodeText(bytes);
      if (text === null) setLoad({ state: "binary", bytes, size: res.size });
      else setLoad({ state: "text", text });
    }).catch((error) => {
      if (!controller.signal.aborted) setLoad({ state: "error", message: error instanceof Error ? error.message : String(error) });
    });
    return () => controller.abort();
  }, [repo.repo_id, tab.key, tab.path, tab.rev, maxBytes]);

  if (load.state === "loading") {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Loading {tab.path}…
      </div>
    );
  }
  if (load.state === "error") {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        Failed to read <code className="font-mono">{tab.path}</code>: {load.message}
      </div>
    );
  }
  if (load.state === "truncated") {
    return (
      <div className="m-4 space-y-2 rounded border border-gx-border bg-gx-surface p-4 text-sm">
        <div className="flex items-center gap-2 font-semibold">
          <Binary size={14} /> File too large to preview
        </div>
        <div className="text-xs text-gx-muted">
          {tab.path} is {formatBytes(load.size)}. {load.reason}
        </div>
      </div>
    );
  }
  if (load.state === "binary") {
    return (
      <div className="flex h-full flex-col">
        <div className="flex items-center justify-between border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
          <span>Binary · {formatBytes(load.size)} · showing first 4 KiB as hex</span>
        </div>
        <pre className="h-full overflow-auto bg-gx-bg-sub p-4 font-mono text-[11.5px] leading-5 text-gx-text">
          {hexDump(load.bytes, 4096)}
        </pre>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Save pending indicator for remote files */}
      {!isLocal && remoteAutoSave.savePending && (
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-warning/30 bg-gx-warning/10 px-3 py-1 text-[11px] text-gx-warning">
          <Loader2 size={12} className="animate-spin" />
          Save pending — reconnecting
        </div>
      )}
      {/* External file change prompt */}
      {showExternalChangePrompt && (
        <ExternalChangePrompt reloadError={reloadError} onReload={acceptReload} onKeepMine={keepMine} />
      )}
      <div className="min-h-0 flex-1">
        <CodeEditor
          path={tab.path}
          cacheKey={`${repo.path ?? repo.repo_id}::${tab.path}`}
          value={load.text}
          line={tab.line}
          readOnly={!canEdit}
          onChange={canEdit ? handleChange : undefined}
          onSave={canEdit ? handleSave : undefined}
          onBlur={canEdit ? handleSave : undefined}
          repoRoot={repo.path ?? undefined}
        />
      </div>
    </div>
  );
}

/**
 * Markdown editor with a toggle between source editing and rendered preview.
 * Defaults to the source editor so the user can immediately type.
 */
function MarkdownEditorBody({ repo, tab, onDirtyChange }: { repo: OpenRepo; tab: EditorTab; onDirtyChange: (key: string, dirty: boolean) => void }) {
  const maxKib = useSetting("maxPreviewKib");
  const maxBytes = Math.max(maxKib * 1024, 64 * 1024);
  const [mode, setMode] = useState<"edit" | "preview">("edit");
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pendingContentRef = useRef<string | null>(null);

  // Transport routing: local files save via fs, remote files save via RPC.
  const isLocal = !!repo.path && !repo.is_remote;

  // Track dirty state for external change detection.
  const [isDirty, setIsDirty] = useState(false);

  // Remote auto-save hook for remote files.
  const remoteAutoSave = useRemoteAutoSave(
    repo.repo_id,
    tab.path,
    tab.key,
    onDirtyChange,
  );

  // External file change detection.
  const handleExternalReload = useCallback((newContent: string) => {
    setContent(newContent);
    setIsDirty(false);
    onDirtyChange(tab.key, false);
    pendingContentRef.current = null;
    if (saveTimerRef.current) {
      clearTimeout(saveTimerRef.current);
      saveTimerRef.current = null;
    }
  }, [onDirtyChange, tab.key]);

  const { showPrompt: showExternalChangePrompt, reloadError, acceptReload, keepMine } = useExternalFileChange({
    repo,
    filePath: tab.path,
    isDirty,
    currentContent: content,
    maxBytes,
    enabled: !loading && !error,
    onReload: handleExternalReload,
  });

  const writeToDisk = useCallback(
    async (text: string) => {
      try {
        if (isLocal) {
          const fullPath = `${repo.path}/${tab.path}`;
          await mainProcess.fsWriteFile(fullPath, text);
        } else {
          await rpc.writeText(repo.repo_id, tab.path, text);
        }
        pendingContentRef.current = null;
        setIsDirty(false);
        onDirtyChange(tab.key, false);
      } catch (e) {
        console.error("Failed to save markdown:", e);
      }
    },
    [isLocal, repo.path, repo.repo_id, tab.path, onDirtyChange, tab.key],
  );

  const handleChange = useCallback(
    (text: string) => {
      setContent(text);
      if (!isLocal) {
        remoteAutoSave.handleChange(text);
        setIsDirty(true);
        return;
      }
      pendingContentRef.current = text;
      setIsDirty(true);
      onDirtyChange(tab.key, true);
      if (saveTimerRef.current) clearTimeout(saveTimerRef.current);
      saveTimerRef.current = setTimeout(() => {
        saveTimerRef.current = null;
        void writeToDisk(text);
      }, 300);
    },
    [isLocal, remoteAutoSave, writeToDisk, onDirtyChange, tab.key],
  );

  const handleSave = useCallback(
    (text: string) => {
      if (!isLocal) {
        remoteAutoSave.handleSave(text);
        setIsDirty(false);
        return;
      }
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      pendingContentRef.current = null;
      setIsDirty(false);
      void writeToDisk(text);
    },
    [isLocal, remoteAutoSave, writeToDisk],
  );

  // Flush pending save on unmount (local only).
  useEffect(() => {
    return () => {
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      const pending = pendingContentRef.current;
      if (pending !== null && repo.path && isLocal) {
        void mainProcess.fsWriteFile(`${repo.path}/${tab.path}`, pending);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo.path, tab.path, isLocal]);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    rpc.readText(repo.repo_id, tab.path, { max_bytes: maxBytes, signal: controller.signal })
      .then((result) => {
        if (controller.signal.aborted) return;
        if (result.truncated) setError(result.reason ?? "File exceeds the editor limit");
        else setContent(result.text ?? "");
      })
      .catch((error) => { if (!controller.signal.aborted) setError(error instanceof Error ? error.message : String(error)); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [repo.repo_id, tab.key, tab.path, maxBytes]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Loading {tab.path}…
      </div>
    );
  }
  if (error) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        Failed to read <code className="font-mono">{tab.path}</code>: {error}
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Save pending indicator for remote files */}
      {!isLocal && remoteAutoSave.savePending && (
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-warning/30 bg-gx-warning/10 px-3 py-1 text-[11px] text-gx-warning">
          <Loader2 size={12} className="animate-spin" />
          Save pending — reconnecting
        </div>
      )}
      {/* External file change prompt */}
      {showExternalChangePrompt && (
        <ExternalChangePrompt reloadError={reloadError} onReload={acceptReload} onKeepMine={keepMine} />
      )}
      {/* Mode toggle toolbar */}
      <div className="flex shrink-0 items-center gap-1 border-b border-gx-border bg-gx-surface px-3 py-1">
        <button
          type="button"
          onClick={() => setMode("edit")}
          className={clsx(
            "flex items-center gap-1 rounded px-2 py-0.5 text-[11px] transition",
            mode === "edit" ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:text-gx-text",
          )}
        >
          <Code2 size={12} /> Edit
        </button>
        <button
          type="button"
          onClick={() => setMode("preview")}
          className={clsx(
            "flex items-center gap-1 rounded px-2 py-0.5 text-[11px] transition",
            mode === "preview" ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:text-gx-text",
          )}
        >
          <Eye size={12} /> Preview
        </button>
      </div>
      {/* Content area */}
      <div className="min-h-0 flex-1">
        {mode === "edit" ? (
          <CodeEditor
            path={tab.path}
            cacheKey={`${repo.path ?? repo.repo_id}::${tab.path}`}
            value={content ?? ""}
            line={tab.line}
            readOnly={false}
            onChange={handleChange}
            onSave={handleSave}
            onBlur={handleSave}
            repoRoot={repo.path ?? undefined}
          />
        ) : (
          <MarkdownView repoId={repo.repo_id} path={tab.path} rev={tab.rev} />
        )}
      </div>
    </div>
  );
}

// ── Error boundary for the editor/preview area ──────────────────────

interface EBProps {
  path: string;
  children: ReactNode;
}
interface EBState {
  error: string | null;
}

class EditorErrorBoundary extends Component<EBProps, EBState> {
  state: EBState = { error: null };

  static getDerivedStateFromError(error: unknown): EBState {
    const msg = error instanceof Error
      ? `${error.message}\n\nStack: ${error.stack ?? "none"}`
      : String(error);
    return { error: msg };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("[EditorErrorBoundary]", error, info.componentStack);
  }

  componentDidUpdate(prevProps: EBProps) {
    // Reset error when the file changes so the user can try another file.
    if (prevProps.path !== this.props.path && this.state.error) {
      this.setState({ error: null });
    }
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex h-full items-center justify-center bg-gx-bg-sub p-6">
          <div className="max-w-lg rounded border border-gx-error/40 bg-gx-error/10 p-4 text-sm">
            <div className="mb-2 font-semibold text-gx-error">Failed to render file</div>
            <div className="mb-3 text-xs text-gx-muted break-all">{this.props.path}</div>
            <pre className="text-[10px] text-gx-error/80 whitespace-pre-wrap break-all max-h-[300px] overflow-auto">
              {this.state.error}
            </pre>
            <button
              type="button"
              className="btn mt-3 text-xs"
              onClick={() => this.setState({ error: null })}
            >
              Try again
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
