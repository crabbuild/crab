/**
 * Two-zone staging layout for the Changes view.
 *
 * Splits files into "Staged for Commit" (top) and "Working Changes"
 * (bottom) sections. Supports drag-and-drop between zones to stage/unstage
 * files intuitively.
 *
 * Design principles:
 * - Staged on top (closer to the commit composer below)
 * - Plain language headers ("Working Changes" not "Worktree")
 * - Drag files between zones to stage/unstage
 * - Per-file action buttons on hover: Stage/Unstage, Discard, Open File
 * - Collapsible sections for focus
 */
import { useCallback, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronRight,
  FileText,
  GripHorizontal,
  Minus,
  Plus,
  Undo2,
} from "lucide-react";
import type { TriStateValue } from "./TriStateCheckbox";

interface StatusEntry {
  path: string;
  code: string;
}

interface StagingZonesProps {
  entries: StatusEntry[];
  fileStates: Record<string, TriStateValue>;
  selectedPath: string | null;
  multiSelected: Set<string>;
  togglingPaths: Set<string>;
  onSelect: (path: string, e: React.MouseEvent) => void;
  onDoubleClick: (path: string) => void;
  onToggleFile: (path: string, intent: "stage" | "unstage") => void;
  onDiscardFile: (path: string) => void;
  onOpenFile?: (path: string) => void;
  onContextMenu: (e: React.MouseEvent, path: string) => void;
}

// Status code helpers
function codeBadge(code: string): string {
  const t = code.trim();
  if (t === "??") return "U";
  if (!t) return "·";
  return (t[1] && t[1] !== " " ? t[1] : t[0]) ?? "·";
}

function statusColor(code: string): string {
  const t = code.trim();
  if (t === "??") return "text-gx-success";
  const c = t[0] !== " " ? t[0] : t[1];
  switch (c) {
    case "M": return "text-gx-warn";
    case "A": return "text-gx-success";
    case "D": return "text-gx-error";
    case "R": return "text-gx-accent";
    default: return "text-gx-muted";
  }
}

function statusTooltip(code: string): string {
  const t = code.trim();
  if (t === "??") return "New untracked file. Stage it to include in your next commit.";
  switch (t[0] !== " " ? t[0] : t[1]) {
    case "M": return "Modified since last commit.";
    case "A": return "Newly added to the repository.";
    case "D": return "Deleted. Stage to remove in next commit.";
    case "R": return "Renamed.";
    default: return "Has changes.";
  }
}

/** Determine if a file belongs in the "staged" zone. */
function isStaged(state: TriStateValue): boolean {
  return state === "full";
}

/** Determine if a file belongs in the "working" zone. */
function isWorking(state: TriStateValue): boolean {
  return state === "empty" || state === "partial";
}

/** MIME type for drag-and-drop data transfer. */
const DND_MIME = "application/x-crab-staging";

export function StagingZones({
  entries,
  fileStates,
  selectedPath,
  multiSelected,
  togglingPaths,
  onSelect,
  onDoubleClick,
  onToggleFile,
  onDiscardFile,
  onOpenFile,
  onContextMenu,
}: StagingZonesProps) {
  const [workingCollapsed, setWorkingCollapsed] = useState(false);
  const [stagedCollapsed, setStagedCollapsed] = useState(false);
  const [dragOverZone, setDragOverZone] = useState<"staged" | "working" | null>(null);

  // Split entries into two zones
  const { workingEntries, stagedEntries } = useMemo(() => {
    const working: StatusEntry[] = [];
    const staged: StatusEntry[] = [];
    for (const e of entries) {
      const state = fileStates[e.path] ?? "empty";
      if (isStaged(state)) {
        staged.push(e);
      }
      // Partial files appear in both zones (they have both staged and unstaged hunks)
      if (isWorking(state)) {
        working.push(e);
      }
    }
    return { workingEntries: working, stagedEntries: staged };
  }, [entries, fileStates]);

  // --- Drag-and-drop handlers ---

  const handleDragOver = useCallback((e: React.DragEvent, zone: "staged" | "working") => {
    if (!e.dataTransfer.types.includes(DND_MIME)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    setDragOverZone(zone);
  }, []);

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    // Only clear if we're leaving the zone container itself
    const related = e.relatedTarget as HTMLElement | null;
    if (related && (e.currentTarget as HTMLElement).contains(related)) return;
    setDragOverZone(null);
  }, []);

  const handleDrop = useCallback((e: React.DragEvent, targetZone: "staged" | "working") => {
    e.preventDefault();
    setDragOverZone(null);
    const data = e.dataTransfer.getData(DND_MIME);
    if (!data) return;
    try {
      const { paths, sourceZone } = JSON.parse(data) as { paths: string[]; sourceZone: string };
      if (sourceZone === targetZone) return; // No-op: dropped in same zone
      const intent = targetZone === "staged" ? "stage" : "unstage";
      for (const path of paths) {
        onToggleFile(path, intent);
      }
    } catch {
      // Invalid data — ignore
    }
  }, [onToggleFile]);

  return (
    <PanelGroup direction="vertical" className="h-full">
      {/* Staged for Commit zone (top) */}
      <Panel defaultSize={50} minSize={10} className="flex min-h-0 flex-col">
        <ZoneSection
          title="Staged for Commit"
          subtitle={stagedEntries.length > 0 ? "Ready for your next commit" : "Drop files here to stage"}
          count={stagedEntries.length}
          collapsed={stagedCollapsed}
          onToggleCollapse={() => setStagedCollapsed((v) => !v)}
          accentColor="text-gx-success"
          zone="staged"
          isDragOver={dragOverZone === "staged"}
          onDragOver={handleDragOver}
          onDragLeave={handleDragLeave}
          onDrop={handleDrop}
        >
          {stagedEntries.map((e) => (
            <FileRow
              key={`staged-${e.path}`}
              entry={e}
              state={fileStates[e.path] ?? "empty"}
              zone="staged"
              isSelected={selectedPath === e.path}
              isMultiSelected={multiSelected.has(e.path)}
              toggling={togglingPaths.has(e.path)}
              onSelect={onSelect}
              onDoubleClick={onDoubleClick}
              onStage={() => onToggleFile(e.path, "stage")}
              onUnstage={() => onToggleFile(e.path, "unstage")}
              onDiscard={() => onDiscardFile(e.path)}
              onOpenFile={onOpenFile ? () => onOpenFile(e.path) : undefined}
              onContextMenu={onContextMenu}
            />
          ))}
          {stagedEntries.length === 0 && (
            <div className="px-3 py-4 text-center text-[11px] text-gx-muted italic">
              No files staged yet. Drag files here or click + to stage.
            </div>
          )}
        </ZoneSection>
      </Panel>

      {/* Resize handle between zones */}
      <PanelResizeHandle className="group flex h-5 items-center justify-center border-y border-gx-border/50 bg-gx-bg-sub/50">
        <div className="flex items-center gap-2">
          <ArrowDown size={10} className="text-gx-warn" />
          <span className="text-[10px] text-gx-muted">Unstage</span>
          <GripHorizontal size={10} className="text-gx-muted/50 group-hover:text-gx-muted transition" />
          <span className="text-[10px] text-gx-muted">Stage</span>
          <ArrowUp size={10} className="text-gx-success" />
        </div>
      </PanelResizeHandle>

      {/* Working Changes zone (bottom) */}
      <Panel defaultSize={50} minSize={10} className="flex min-h-0 flex-col">
        <ZoneSection
          title="Working Changes"
          subtitle="Unstaged edits in your working tree"
          count={workingEntries.length}
          collapsed={workingCollapsed}
          onToggleCollapse={() => setWorkingCollapsed((v) => !v)}
          accentColor="text-gx-warn"
          zone="working"
          isDragOver={dragOverZone === "working"}
          onDragOver={handleDragOver}
          onDragLeave={handleDragLeave}
          onDrop={handleDrop}
        >
          {workingEntries.map((e) => (
            <FileRow
              key={`working-${e.path}`}
              entry={e}
              state={fileStates[e.path] ?? "empty"}
              zone="working"
              isSelected={selectedPath === e.path}
              isMultiSelected={multiSelected.has(e.path)}
              toggling={togglingPaths.has(e.path)}
              onSelect={onSelect}
              onDoubleClick={onDoubleClick}
              onStage={() => onToggleFile(e.path, "stage")}
              onUnstage={() => onToggleFile(e.path, "unstage")}
              onDiscard={() => onDiscardFile(e.path)}
              onOpenFile={onOpenFile ? () => onOpenFile(e.path) : undefined}
              onContextMenu={onContextMenu}
            />
          ))}
          {workingEntries.length === 0 && (
            <div className="px-3 py-4 text-center text-[11px] text-gx-muted italic">
              All changes are staged — ready to commit!
            </div>
          )}
        </ZoneSection>
      </Panel>
    </PanelGroup>
  );
}

// --- Zone Section ---

function ZoneSection({
  title,
  subtitle,
  count,
  collapsed,
  onToggleCollapse,
  accentColor,
  zone,
  isDragOver,
  onDragOver,
  onDragLeave,
  onDrop,
  children,
}: {
  title: string;
  subtitle: string;
  count: number;
  collapsed: boolean;
  onToggleCollapse: () => void;
  accentColor: string;
  zone: "staged" | "working";
  isDragOver: boolean;
  onDragOver: (e: React.DragEvent, zone: "staged" | "working") => void;
  onDragLeave: (e: React.DragEvent) => void;
  onDrop: (e: React.DragEvent, zone: "staged" | "working") => void;
  children: React.ReactNode;
}) {
  return (
    <div
      className={clsx(
        "flex min-h-0 flex-1 flex-col transition-colors",
        isDragOver && "bg-gx-accent/5 ring-1 ring-inset ring-gx-accent/30",
      )}
      onDragOver={(e) => onDragOver(e, zone)}
      onDragLeave={onDragLeave}
      onDrop={(e) => onDrop(e, zone)}
    >
      <button
        type="button"
        onClick={onToggleCollapse}
        aria-expanded={!collapsed}
        className="grid shrink-0 grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-2 gap-y-0.5 px-3 py-1.5 text-left hover:bg-gx-surface-hover transition"
      >
        {collapsed ? (
          <ChevronRight size={12} className="text-gx-muted" />
        ) : (
          <ChevronDown size={12} className="text-gx-muted" />
        )}
        <span className={clsx("min-w-0 text-[11px] font-semibold leading-4", accentColor)}>
          {title}
        </span>
        <span className="rounded-full bg-gx-surface-hover px-1.5 text-[10px] tabular-nums text-gx-muted">
          {count}
        </span>
        <span className="col-span-2 col-start-2 text-[10px] leading-snug text-gx-muted">{subtitle}</span>
      </button>
      {!collapsed && <div className="min-h-0 flex-1 overflow-auto">{children}</div>}
    </div>
  );
}

// --- File Row ---

function FileRow({
  entry,
  state: _state,
  zone,
  isSelected,
  isMultiSelected,
  toggling,
  onSelect,
  onDoubleClick,
  onStage,
  onUnstage,
  onDiscard,
  onOpenFile,
  onContextMenu,
}: {
  entry: StatusEntry;
  state: TriStateValue;
  zone: "working" | "staged";
  isSelected: boolean;
  isMultiSelected: boolean;
  toggling: boolean;
  onSelect: (path: string, e: React.MouseEvent) => void;
  onDoubleClick: (path: string) => void;
  onStage: () => void;
  onUnstage: () => void;
  onDiscard: () => void;
  onOpenFile?: () => void;
  onContextMenu: (e: React.MouseEvent, path: string) => void;
}) {
  const fileName = entry.path.split("/").pop() ?? entry.path;
  const dirPath = entry.path.includes("/")
    ? entry.path.slice(0, entry.path.lastIndexOf("/"))
    : "";

  const rowRef = useRef<HTMLDivElement>(null);
  const [isDragging, setIsDragging] = useState(false);

  const handleClick = useCallback(
    (e: React.MouseEvent) => {
      onSelect(entry.path, e);
    },
    [entry.path, onSelect],
  );

  const handleDragStart = useCallback(
    (e: React.DragEvent) => {
      const payload = JSON.stringify({ paths: [entry.path], sourceZone: zone });
      e.dataTransfer.setData(DND_MIME, payload);
      e.dataTransfer.effectAllowed = "move";
      setIsDragging(true);
    },
    [entry.path, zone],
  );

  const handleDragEnd = useCallback(() => {
    setIsDragging(false);
  }, []);

  return (
    <div
      ref={rowRef}
      draggable
      onDragStart={handleDragStart}
      onDragEnd={handleDragEnd}
      onClick={handleClick}
      onDoubleClick={() => onDoubleClick(entry.path)}
      onContextMenu={(e) => onContextMenu(e, entry.path)}
      className={clsx(
        "group/row flex h-7 items-center gap-2 px-3 pl-7 text-[12px] cursor-grab transition",
        isDragging && "opacity-40",
        isSelected && "bg-gx-accent/20 text-gx-text",
        isMultiSelected && !isSelected && "bg-gx-accent/10 text-gx-text",
        !isSelected && !isMultiSelected && "text-gx-text hover:bg-gx-surface-hover",
      )}
      title={statusTooltip(entry.code)}
    >
      {/* Status pill */}
      <span
        className={clsx(
          "inline-flex items-center gap-1 rounded-full px-1.5 py-0 text-[10px] font-medium",
          statusColor(entry.code),
          "bg-current/10",
        )}
      >
        <span className="font-mono">{codeBadge(entry.code)}</span>
      </span>

      {/* File name */}
      <span className="min-w-0 flex-1 truncate font-mono text-[11.5px]">
        {fileName}
      </span>

      {/* Directory path (hidden on hover to make room for actions) */}
      <span className="shrink-0 text-[10px] text-gx-muted group-hover/row:hidden">
        {dirPath}
      </span>

      {/* Hover action buttons */}
      <span className="hidden shrink-0 items-center gap-0.5 group-hover/row:flex">
        {zone === "working" && (
          <button
            type="button"
            title={`Stage ${entry.path}`}
            aria-label={`Stage ${entry.path}`}
            disabled={toggling}
            onClick={(ev) => { ev.stopPropagation(); onStage(); }}
            className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-success/20 hover:text-gx-success disabled:opacity-50 transition"
          >
            <Plus size={12} />
          </button>
        )}
        {zone === "staged" && (
          <button
            type="button"
            title={`Unstage ${entry.path}`}
            aria-label={`Unstage ${entry.path}`}
            disabled={toggling}
            onClick={(ev) => { ev.stopPropagation(); onUnstage(); }}
            className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-warn/20 hover:text-gx-warn disabled:opacity-50 transition"
          >
            <Minus size={12} />
          </button>
        )}
        {zone === "working" && (
          <button
            type="button"
            title={`Discard changes in ${entry.path}`}
            aria-label={`Discard changes in ${entry.path}`}
            disabled={toggling}
            onClick={(ev) => { ev.stopPropagation(); onDiscard(); }}
            className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-error/20 hover:text-gx-error disabled:opacity-50 transition"
          >
            <Undo2 size={12} />
          </button>
        )}
        {onOpenFile && (
          <button
            type="button"
            title={`Open file ${entry.path}`}
            aria-label={`Open file ${entry.path}`}
            onClick={(ev) => { ev.stopPropagation(); onOpenFile(); }}
            className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-accent transition"
          >
            <FileText size={12} />
          </button>
        )}
      </span>
    </div>
  );
}
