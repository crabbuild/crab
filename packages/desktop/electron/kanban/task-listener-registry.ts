/**
 * Manages one event listener per active agent task to prevent duplicates
 * and enable O(1) cleanup on task completion.
 */

export interface OrchestratorEventPayload {
  taskId: string;
  event: {
    type: "progress" | "output" | "exit" | "error" | "complete" | string;
    timestamp?: number;
    error?: string;
    exitCode?: number;
    [key: string]: unknown;
  };
}

export class TaskListenerRegistry {
  private listeners = new Map<string, (payload: OrchestratorEventPayload) => void>();

  /**
   * Registers a listener for a task. No-op if one already exists.
   * Returns true if registered, false if already existed.
   */
  register(taskId: string, listener: (payload: OrchestratorEventPayload) => void): boolean {
    if (this.listeners.has(taskId)) {
      return false;
    }
    this.listeners.set(taskId, listener);
    return true;
  }

  /**
   * Removes the listener for a task.
   * Returns true if it existed and was removed, false otherwise.
   */
  unregister(taskId: string): boolean {
    return this.listeners.delete(taskId);
  }

  /**
   * Returns whether a listener exists for the given task.
   */
  has(taskId: string): boolean {
    return this.listeners.has(taskId);
  }

  /**
   * Returns the count of active listeners.
   */
  get size(): number {
    return this.listeners.size;
  }
}
