import { useCallback } from "react";
import { useWorkspaceDocument } from "../settings";
import { documentTasks, type TaskItem } from "../lib/workspace-settings-store";

/** Edit tasks through the same serialized document as settings and theme. */
export function useTaskList() {
  const state = useWorkspaceDocument(), tasks = documentTasks(state.document);
  const addTask = useCallback((title: string, anchor?: string) => {
    const item: TaskItem = { id: crypto.randomUUID(), title, done: false, createdAt: Date.now(), ...(anchor ? { anchor } : {}) };
    return state.change(document => { document.tasks = [...documentTasks(document), item]; });
  }, [state.change]);
  const toggleTask = useCallback((id: string) => state.change(document => {
    document.tasks = documentTasks(document).map(task => task.id === id ? { ...task, done: !task.done } : task);
  }), [state.change]);
  const removeTask = useCallback((id: string) => state.change(document => {
    document.tasks = documentTasks(document).filter(task => task.id !== id);
  }), [state.change]);
  return { key: state.key, tasks, loading: state.loading || state.saving, error: state.error, canEdit: state.canEdit,
    reload: state.reload, addTask, toggleTask, removeTask, remaining: tasks.filter(task => !task.done).length, total: tasks.length };
}
