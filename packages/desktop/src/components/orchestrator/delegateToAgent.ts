/**
 * "Delegate to agent" context menu integration.
 *
 * Exports a helper that other views (File Explorer, Timeline, Changes)
 * can import to add a "Delegate to agent" option to their context menus.
 * When invoked, it dispatches a custom event that the AgentTasksView
 * listens for to open the New Task dialog with context pre-filled.
 */

export interface DelegateContext {
  /** Pre-filled prompt text (e.g. file path, commit message). */
  prompt?: string;
  /** Files to attach as context. */
  contextFiles?: string[];
}

/**
 * Opens the orchestrator New Task dialog with pre-filled context.
 * Call this from context menu handlers in Explorer, Timeline, Changes views.
 */
export function delegateToAgent(context: DelegateContext): void {
  window.dispatchEvent(
    new CustomEvent("crab:delegate-to-agent", { detail: context }),
  );
}

/**
 * Context menu item definition for use in context menu arrays.
 * Import this in views that want to offer "Delegate to agent".
 */
export function getDelegateMenuItem(context: DelegateContext) {
  return {
    label: "Delegate to agent",
    icon: "ListTodo",
    action: () => delegateToAgent(context),
  };
}
