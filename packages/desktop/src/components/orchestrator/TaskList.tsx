import { type KeyboardEvent, useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown, ChevronRight, Folder, PanelLeftClose, PanelLeftOpen, Pin, Plus, Radio } from "lucide-react";
import { TaskCard, OrchestratorTask } from "./TaskCard";
import { TaskSearch } from "./TaskSearch";
import {
  buildTaskSidebarModel,
  collectionName,
  RECENT_BUCKET_ORDER,
  type TaskFilter,
} from "../../lib/orchestrator-sidebar";

// ── Component ───────────────────────────────────────────────────────

interface Props {
  tasks: OrchestratorTask[];
  selectedTaskId: string | null;
  onSelectTask: (id: string) => void;
  onNewTask: () => void;
  onDeleteTask?: (taskId: string) => void;
  onTogglePin?: (taskId: string, pinned: boolean) => void;
  onSetGroup?: (taskId: string, currentGroup?: string) => void;
  onRename?: (taskId: string, currentTitle: string) => void;
  /** When true, renders icons-only (responsive collapse below 1200px). */
  compact?: boolean;
  /** Manual sidebar collapse state. */
  collapsed?: boolean;
  onToggleCollapsed?: () => void;
}

const LS_SECTION_STATE = "crab.orchestrator.taskSidebar.sections";
const LS_BRANCH_STATE = "crab.orchestrator.taskSidebar.branches";
const LS_ARCHIVED_EXPANDED = "crab.orchestrator.taskSidebar.archivedExpanded";

function loadRecord(key: string): Record<string, boolean> {
  try {
    const parsed = JSON.parse(localStorage.getItem(key) ?? "{}");
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed as Record<string, boolean> : {};
  } catch {
    return {};
  }
}

export function TaskList({
  tasks,
  selectedTaskId,
  onSelectTask,
  onNewTask,
  onDeleteTask,
  onTogglePin,
  onSetGroup,
  onRename,
  compact,
  collapsed,
  onToggleCollapsed,
}: Props) {
  const [filter, setFilter] = useState<TaskFilter>({ query: "", status: "all" });
  const [archivedExpanded, setArchivedExpanded] = useState(() => {
    try {
      return localStorage.getItem(LS_ARCHIVED_EXPANDED) === "1";
    } catch {
      return false;
    }
  });
  const [collapsedGroups, setCollapsedGroups] = useState<Record<string, boolean>>(() => loadRecord(LS_SECTION_STATE));
  const [collapsedBranches, setCollapsedBranches] = useState<Record<string, boolean>>(() => loadRecord(LS_BRANCH_STATE));
  const searchInputRef = useRef<HTMLInputElement>(null);

  const model = useMemo(() => buildTaskSidebarModel(tasks, filter), [tasks, filter]);
  const searchActive = filter.query.trim().length > 0 || filter.status !== "all";

  useEffect(() => {
    try {
      localStorage.setItem(LS_SECTION_STATE, JSON.stringify(collapsedGroups));
    } catch {
      // best-effort
    }
  }, [collapsedGroups]);

  useEffect(() => {
    try {
      localStorage.setItem(LS_BRANCH_STATE, JSON.stringify(collapsedBranches));
    } catch {
      // best-effort
    }
  }, [collapsedBranches]);

  useEffect(() => {
    try {
      localStorage.setItem(LS_ARCHIVED_EXPANDED, archivedExpanded ? "1" : "0");
    } catch {
      // best-effort
    }
  }, [archivedExpanded]);

  const toggleGroup = (id: string) => {
    setCollapsedGroups((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const renderTaskTree = (task: OrchestratorTask, depth = 0): JSX.Element => {
    const taskGroup = collectionName(task);
    const children = (model.childrenByParent.get(task.id) ?? []).filter((child) => {
      if (!model.taskById.has(child.id) || model.pinnedIds.has(child.id)) return false;
      const childGroup = collectionName(child);
      return !childGroup || childGroup === taskGroup;
    });
    const branchCollapsed = collapsedBranches[task.id] ?? false;
    const hasChildren = children.length > 0;
    return (
      <div key={task.id} className="space-y-1">
        <div className="flex items-stretch gap-1">
          {!compact && hasChildren && (
            <button
              type="button"
              onClick={() => setCollapsedBranches((prev) => ({ ...prev, [task.id]: !branchCollapsed }))}
              className="mt-2 h-5 w-4 shrink-0 rounded text-muted-foreground transition hover:bg-accent hover:text-foreground"
              aria-label={branchCollapsed ? "Expand child chats" : "Collapse child chats"}
              aria-expanded={!branchCollapsed}
            >
              {branchCollapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
            </button>
          )}
          {!compact && !hasChildren && depth === 0 && <div className="w-5 shrink-0" />}
          <div className="min-w-0 flex-1">
            <TaskCard
              task={task}
              isSelected={selectedTaskId === task.id}
              onClick={() => onSelectTask(task.id)}
              compact={compact}
              onDelete={onDeleteTask}
              onTogglePin={onTogglePin}
              onSetGroup={onSetGroup}
              onRename={onRename}
              depth={depth}
              hasChildren={hasChildren}
            />
          </div>
        </div>
        {!branchCollapsed && hasChildren && (
          <div className="space-y-1 border-l border-border/70 pl-2 ml-2">
            {children.map((child) => renderTaskTree(child, depth + 1))}
          </div>
        )}
      </div>
    );
  };

  const visibleTaskIds = useMemo(() => {
    const ids: string[] = [];
    const addTree = (task: OrchestratorTask) => {
      ids.push(task.id);
      if (collapsedBranches[task.id]) return;
      const taskGroup = collectionName(task);
      for (const child of model.childrenByParent.get(task.id) ?? []) {
        if (model.pinnedIds.has(child.id)) continue;
        const childGroup = collectionName(child);
        if (childGroup && childGroup !== taskGroup) continue;
        addTree(child);
      }
    };
    const addSection = (id: string, sectionTasks: OrchestratorTask[]) => {
      if (sectionTasks.length === 0) return;
      if (!compact && !searchActive && collapsedGroups[id]) return;
      sectionTasks.forEach(addTree);
    };
    addSection("pinned", model.pinnedTasks);
    for (const [group, groupTasks] of model.collectionGroups) addSection(`collection:${group}`, groupTasks);
    addSection("active", model.activeTasks);
    for (const bucket of RECENT_BUCKET_ORDER) addSection(`recent:${bucket}`, model.recentTasksByDate[bucket]);
    if (archivedExpanded || searchActive) model.archivedTasks.forEach(addTree);
    return ids;
  }, [archivedExpanded, collapsedBranches, collapsedGroups, compact, model, searchActive]);

  const handleKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest("input, textarea, select")) {
      if (event.key !== "/") return;
    }
    if (event.key === "/") {
      event.preventDefault();
      searchInputRef.current?.focus();
      return;
    }
    const currentIndex = selectedTaskId ? visibleTaskIds.indexOf(selectedTaskId) : -1;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      const next = visibleTaskIds[Math.min(visibleTaskIds.length - 1, currentIndex + 1)] ?? visibleTaskIds[0];
      if (next) onSelectTask(next);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      const previous = visibleTaskIds[Math.max(0, currentIndex - 1)] ?? visibleTaskIds[0];
      if (previous) onSelectTask(previous);
    } else if (event.key === "ArrowRight" && selectedTaskId) {
      const selected = model.taskById.get(selectedTaskId);
      if (selected && (model.childrenByParent.get(selected.id)?.length ?? 0) > 0) {
        event.preventDefault();
        setCollapsedBranches((prev) => ({ ...prev, [selected.id]: false }));
      }
    } else if (event.key === "ArrowLeft" && selectedTaskId) {
      const selected = model.taskById.get(selectedTaskId);
      if (selected && (model.childrenByParent.get(selected.id)?.length ?? 0) > 0) {
        event.preventDefault();
        setCollapsedBranches((prev) => ({ ...prev, [selected.id]: true }));
      }
    } else if (event.key.toLowerCase() === "p" && selectedTaskId && onTogglePin) {
      const selected = model.taskById.get(selectedTaskId);
      if (selected) {
        event.preventDefault();
        onTogglePin(selected.id, !selected.pinnedAt);
      }
    }
  };

  const renderSection = (id: string, label: string, sectionTasks: OrchestratorTask[], icon?: JSX.Element) => {
    if (sectionTasks.length === 0) return null;
    const isCollapsed = !searchActive && (collapsedGroups[id] ?? false);
    if (compact) {
      return (
        <div className="space-y-1">
          {sectionTasks.map((task) => renderTaskTree(task))}
        </div>
      );
    }
    return (
      <section className="space-y-1.5" aria-label={label}>
        <button
          type="button"
          onClick={() => toggleGroup(id)}
          className="flex w-full items-center gap-1.5 px-1 py-1 text-[10px] font-medium uppercase tracking-wide text-muted-foreground transition hover:text-foreground"
          aria-expanded={!isCollapsed}
        >
          {isCollapsed ? <ChevronRight size={10} /> : <ChevronDown size={10} />}
          {icon}
          <span>{label}</span>
          <span className="text-muted-foreground/70">({sectionTasks.length})</span>
        </button>
        {!isCollapsed && <div className="space-y-1.5">{sectionTasks.map((task) => renderTaskTree(task))}</div>}
      </section>
    );
  };

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-2 py-2">
        {!compact && <span className="text-xs font-medium text-foreground">Chats</span>}
        <div className="flex-1" />
        {onToggleCollapsed && (
          <button
            type="button"
            onClick={onToggleCollapsed}
            className="rounded p-1 text-muted-foreground transition hover:bg-accent hover:text-foreground"
            aria-label={collapsed ? "Expand chat sidebar" : "Collapse chat sidebar"}
            title={collapsed ? "Expand sidebar" : "Collapse sidebar"}
          >
            {collapsed ? <PanelLeftOpen size={14} /> : <PanelLeftClose size={14} />}
          </button>
        )}
      </div>

      {/* Search */}
      {!compact && <TaskSearch onFilterChange={setFilter} compact={compact} inputRef={searchInputRef} />}

      <div
        className="flex-1 overflow-auto px-2 py-2 space-y-1.5"
        role="listbox"
        aria-label="Task list"
        tabIndex={0}
        onKeyDown={handleKeyDown}
      >
        {compact && (
          <div className="mb-1 flex justify-center text-[10px] font-medium text-muted-foreground" title={`${model.activeTasks.length} active chats`}>
            {model.activeTasks.length}
          </div>
        )}
        {renderSection("pinned", "Pinned", model.pinnedTasks, <Pin size={10} className="fill-primary text-primary" />)}
        {model.collectionGroups.map(([group, groupTasks]) =>
          renderSection(`collection:${group}`, group, groupTasks, <Folder size={10} className="text-info" />),
        )}
        {renderSection("active", "Active", model.activeTasks, <Radio size={10} className="text-success" />)}
        {RECENT_BUCKET_ORDER.map((bucket) =>
          renderSection(`recent:${bucket}`, bucket, model.recentTasksByDate[bucket]),
        )}

        {/* Archived section (collapsed by default) */}
        {model.archivedTasks.length > 0 && (
          <div className="mt-3">
            <button
              type="button"
              onClick={() => setArchivedExpanded(!archivedExpanded)}
              className="flex w-full items-center gap-1.5 px-1 py-1 text-[10px] text-muted-foreground hover:text-foreground transition"
              aria-expanded={archivedExpanded}
            >
              {archivedExpanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
              Archived ({model.archivedTasks.length})
            </button>
            {(archivedExpanded || searchActive) && (
              <div className="mt-1 space-y-1.5">
                {model.archivedTasks.map((task) => renderTaskTree(task))}
              </div>
            )}
          </div>
        )}
      </div>

      {/* New Chat button at the bottom */}
      <div className="shrink-0 border-t border-border p-2">
        <button
          type="button"
          onClick={onNewTask}
          className="flex w-full items-center justify-center gap-1.5 rounded-md border border-dashed border-border px-3 py-2 text-xs text-muted-foreground transition hover:border-primary hover:text-primary"
          aria-label="New chat"
        >
          <Plus size={12} />
          {!compact && <span>New Chat</span>}
        </button>
      </div>
    </div>
  );
}
