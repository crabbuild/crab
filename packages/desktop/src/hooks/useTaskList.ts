import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { mainProcess, rpc } from "../ipc";
import { useActiveRepo } from "../state";

const WORKSPACE_SETTINGS_REL = ".crab/desktop/settings.json";

/**
 * A single task item stored in workspace settings.
 */
export interface TaskItem {
  id: string;
  title: string;
  done: boolean;
  anchor?: string; // "file:src/main.ts:42" or "commit:abc1234"
  createdAt: number; // Unix timestamp ms
}

export interface UseTaskList {
  tasks: TaskItem[];
  loading: boolean;
  addTask: (title: string, anchor?: string) => Promise<void>;
  toggleTask: (id: string) => Promise<void>;
  removeTask: (id: string) => Promise<void>;
  updateTask: (id: string, updates: Partial<Pick<TaskItem, "title" | "anchor" | "done">>) => Promise<void>;
  remaining: number;
  total: number;
}

/**
 * Hook that reads/writes a `tasks` array from the workspace settings
 * file at `<repo>/.crab/desktop/settings.json`.
 *
 * The tasks array lives alongside other workspace settings (theme,
 * pull-mode, etc.) and is persisted per-repo. This makes tasks
 * committable — teams can share a task list by checking in the
 * settings file.
 */
export function useTaskList(): UseTaskList {
  const activeRepo = useActiveRepo();
  const [tasks, setTasks] = useState<TaskItem[]>([]);
  const [loading, setLoading] = useState(false);

  // Keep a ref to avoid stale closures in async callbacks.
  const tasksRef = useRef<TaskItem[]>([]);
  tasksRef.current = tasks;

  // Load tasks when the active repo changes.
  useEffect(() => {
    if (!activeRepo) {
      setTasks([]);
      return;
    }

    let cancelled = false;
    setLoading(true);

    readTasks(activeRepo.repo_id)
      .then((loaded) => {
        if (cancelled) return;
        setTasks(loaded);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [activeRepo?.repo_id]);

  const persist = useCallback(
    async (updated: TaskItem[]) => {
      if (!activeRepo?.path) return;
      // Read the full settings file, merge in the tasks array, write back.
      const existing = await readFullSettings(activeRepo.repo_id);
      const merged = { ...existing, tasks: updated };
      await writeFullSettings(activeRepo.path, merged);
      setTasks(updated);
    },
    [activeRepo],
  );

  const addTask = useCallback(
    async (title: string, anchor?: string) => {
      const item: TaskItem = {
        id: generateId(),
        title,
        done: false,
        createdAt: Date.now(),
        ...(anchor ? { anchor } : {}),
      };
      const updated = [...tasksRef.current, item];
      await persist(updated);
    },
    [persist],
  );

  const toggleTask = useCallback(
    async (id: string) => {
      const updated = tasksRef.current.map((t) =>
        t.id === id ? { ...t, done: !t.done } : t,
      );
      await persist(updated);
    },
    [persist],
  );

  const removeTask = useCallback(
    async (id: string) => {
      const updated = tasksRef.current.filter((t) => t.id !== id);
      await persist(updated);
    },
    [persist],
  );

  const updateTask = useCallback(
    async (id: string, updates: Partial<Pick<TaskItem, "title" | "anchor" | "done">>) => {
      const updated = tasksRef.current.map((t) =>
        t.id === id ? { ...t, ...updates } : t,
      );
      await persist(updated);
    },
    [persist],
  );

  const remaining = useMemo(() => tasks.filter((t) => !t.done).length, [tasks]);
  const total = tasks.length;

  return useMemo<UseTaskList>(
    () => ({ tasks, loading, addTask, toggleTask, removeTask, updateTask, remaining, total }),
    [tasks, loading, addTask, toggleTask, removeTask, updateTask, remaining, total],
  );
}

// ── Internal helpers ────────────────────────────────────────────────

function generateId(): string {
  return `task-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

async function readFullSettings(
  repoId: string,
): Promise<Record<string, unknown>> {
  try {
    const res = await rpc.readText(repoId, WORKSPACE_SETTINGS_REL, {
      max_bytes: 64 * 1024,
    });
    if (!res.text) return {};
    const parsed = JSON.parse(res.text);
    if (!parsed || typeof parsed !== "object") return {};
    return parsed as Record<string, unknown>;
  } catch {
    return {};
  }
}

async function readTasks(repoId: string): Promise<TaskItem[]> {
  const settings = await readFullSettings(repoId);
  const raw = settings.tasks;
  if (!Array.isArray(raw)) return [];
  // Validate each entry has the minimum required fields.
  return raw.filter(
    (item): item is TaskItem =>
      item !== null &&
      typeof item === "object" &&
      typeof item.id === "string" &&
      typeof item.title === "string" &&
      typeof item.done === "boolean",
  );
}

async function writeFullSettings(
  repoPath: string,
  settings: Record<string, unknown>,
): Promise<void> {
  const content = JSON.stringify(settings, null, 2) + "\n";
  const fullPath = `${repoPath}/${WORKSPACE_SETTINGS_REL}`;
  await mainProcess.fsWriteFile(fullPath, content);
}
