import type { OrchestratorTask, TaskStatus } from "../components/orchestrator/TaskCard";

export interface TaskFilter {
  query: string;
  status: TaskStatus | "all";
}

export const ACTIVE_TASK_STATUSES = new Set<TaskStatus>([
  "running",
  "iterating",
  "queued",
  "planning",
]);

const STATUS_PRIORITY: Record<TaskStatus, number> = {
  running: 0,
  iterating: 0,
  queued: 1,
  planning: 1,
  reviewing: 2,
  applied: 3,
  failed: 3,
  stopped: 3,
  interrupted: 3,
  created: 3,
  discarded: 4,
  archived: 4,
};

const RECENT_BUCKETS = ["Today", "Yesterday", "Last 7 days", "Older"] as const;
export type RecentBucket = typeof RECENT_BUCKETS[number];
export const RECENT_BUCKET_ORDER: readonly RecentBucket[] = RECENT_BUCKETS;

export interface TaskSidebarModel {
  sorted: OrchestratorTask[];
  taskById: Map<string, OrchestratorTask>;
  childrenByParent: Map<string, OrchestratorTask[]>;
  pinnedIds: Set<string>;
  pinnedTasks: OrchestratorTask[];
  collectionGroups: Array<[string, OrchestratorTask[]]>;
  activeTasks: OrchestratorTask[];
  recentTasksByDate: Record<RecentBucket, OrchestratorTask[]>;
  archivedTasks: OrchestratorTask[];
}

export function taskActivityTime(task: OrchestratorTask): number {
  return task.lastActivityAt ?? task.completedAt ?? task.createdAt;
}

export function sortTasks(tasks: OrchestratorTask[]): OrchestratorTask[] {
  return [...tasks].sort((a, b) => {
    const pa = STATUS_PRIORITY[a.status] ?? 3;
    const pb = STATUS_PRIORITY[b.status] ?? 3;
    if (pa !== pb) return pa - pb;
    return taskActivityTime(b) - taskActivityTime(a);
  });
}

export function dateBucket(timestamp: number, reference = new Date()): RecentBucket {
  const date = new Date(timestamp);
  const startToday = new Date(reference.getFullYear(), reference.getMonth(), reference.getDate()).getTime();
  const startYesterday = startToday - 24 * 60 * 60 * 1000;
  const startWeek = startToday - 6 * 24 * 60 * 60 * 1000;
  if (date.getTime() >= startToday) return "Today";
  if (date.getTime() >= startYesterday) return "Yesterday";
  if (date.getTime() >= startWeek) return "Last 7 days";
  return "Older";
}

export function collectionName(task: OrchestratorTask): string | null {
  const value = task.sidebarGroup?.trim();
  return value ? value : null;
}

function matchesStatus(task: OrchestratorTask, status: TaskFilter["status"]): boolean {
  return status === "all" || task.status === status;
}

function searchableText(task: OrchestratorTask): string {
  return [
    task.sidebarLabel,
    task.title,
    task.agentName,
    task.status,
    task.sidebarGroup,
    task.worktreePath,
    task.branch,
    task.baseBranch,
    task.spawnedRole,
    task.parentTaskId,
    task.rootTaskId,
    task.id,
    task.pinnedAt ? "pinned" : "",
    task.prNumber != null ? `#${task.prNumber}` : "",
  ].filter(Boolean).join(" ").toLowerCase();
}

export function applyTaskFilter(tasks: OrchestratorTask[], filter: TaskFilter): OrchestratorTask[] {
  const q = filter.query.trim().toLowerCase();
  const statusFiltered = tasks.filter((task) => matchesStatus(task, filter.status));
  if (!q) return statusFiltered;

  const taskById = new Map(tasks.map((task) => [task.id, task]));
  const childrenByParent = new Map<string, OrchestratorTask[]>();
  for (const task of tasks) {
    if (!task.parentTaskId) continue;
    const children = childrenByParent.get(task.parentTaskId) ?? [];
    children.push(task);
    childrenByParent.set(task.parentTaskId, children);
  }

  const included = new Set<string>();
  const addAncestors = (task: OrchestratorTask) => {
    let current: OrchestratorTask | undefined = task;
    const seen = new Set<string>();
    while (current?.parentTaskId && !seen.has(current.parentTaskId)) {
      seen.add(current.parentTaskId);
      const parent = taskById.get(current.parentTaskId);
      if (!parent) break;
      included.add(parent.id);
      current = parent;
    }
  };
  const addDescendants = (task: OrchestratorTask) => {
    for (const child of childrenByParent.get(task.id) ?? []) {
      if (included.has(child.id)) continue;
      included.add(child.id);
      addDescendants(child);
    }
  };

  for (const task of statusFiltered) {
    if (!searchableText(task).includes(q)) continue;
    included.add(task.id);
    addAncestors(task);
    addDescendants(task);
  }

  return tasks.filter((task) => included.has(task.id));
}

export function buildTaskSidebarModel(tasks: OrchestratorTask[], filter: TaskFilter): TaskSidebarModel {
  const filtered = applyTaskFilter(tasks, filter);
  const sorted = sortTasks(filtered);

  const taskById = new Map(sorted.map((task) => [task.id, task]));
  const childrenByParent = new Map<string, OrchestratorTask[]>();
  for (const task of sorted) {
    if (!task.parentTaskId) continue;
    const children = childrenByParent.get(task.parentTaskId) ?? [];
    children.push(task);
    childrenByParent.set(task.parentTaskId, children);
  }
  for (const children of childrenByParent.values()) {
    children.sort((a, b) => taskActivityTime(a) - taskActivityTime(b));
  }

  const pinnedIds = new Set(sorted.filter((task) => task.pinnedAt).map((task) => task.id));
  const pinnedTasks = sorted
    .filter((task) => task.pinnedAt)
    .sort((a, b) => (b.pinnedAt ?? 0) - (a.pinnedAt ?? 0));

  const rootTasks = sorted.filter((task) => {
    if (pinnedIds.has(task.id)) return false;
    if (!task.parentTaskId) return true;
    const parent = taskById.get(task.parentTaskId);
    if (!parent) return true;
    const group = collectionName(task);
    const parentGroup = collectionName(parent);
    return Boolean(group && group !== parentGroup);
  });

  const collectionMap = new Map<string, OrchestratorTask[]>();
  for (const task of rootTasks) {
    const group = collectionName(task);
    if (!group) continue;
    const groupTasks = collectionMap.get(group) ?? [];
    groupTasks.push(task);
    collectionMap.set(group, groupTasks);
  }
  const collectionGroups = Array.from(collectionMap.entries()).sort(([a], [b]) => a.localeCompare(b));

  const ungroupedRootTasks = rootTasks.filter((task) => !collectionName(task));
  const activeTasks = ungroupedRootTasks.filter(
    (task) => task.status !== "archived" && ACTIVE_TASK_STATUSES.has(task.status),
  );
  const recentTasksByDate: Record<RecentBucket, OrchestratorTask[]> = {
    Today: [],
    Yesterday: [],
    "Last 7 days": [],
    Older: [],
  };
  for (const task of ungroupedRootTasks) {
    if (task.status === "archived" || ACTIVE_TASK_STATUSES.has(task.status)) continue;
    recentTasksByDate[dateBucket(taskActivityTime(task))].push(task);
  }
  const archivedTasks = ungroupedRootTasks.filter((task) => task.status === "archived");

  return {
    sorted,
    taskById,
    childrenByParent,
    pinnedIds,
    pinnedTasks,
    collectionGroups,
    activeTasks,
    recentTasksByDate,
    archivedTasks,
  };
}
