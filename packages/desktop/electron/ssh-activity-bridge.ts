/**
 * SSH Activity Bridge — connects the RPC router's notification stream
 * to the orchestrator so that remote agent events flow through the
 * same event pipeline as local tasks.
 *
 * When the remote agent emits JSON-RPC notifications (task_event,
 * task_completed, task_failed), the RPC router parses them and emits
 * a "notification" event. This bridge listens for those events and
 * forwards them to the orchestrator's handleRemoteTaskNotification()
 * method, which in turn emits orchestrator:event and
 * orchestrator:task-status — the same events the renderer already
 * listens for.
 *
 * Result: remote task activity displays in the Task Panel identically
 * to local tasks with no perceptible difference in UX.
 */

import type { RpcRouter } from "./ssh-router";
import type { Orchestrator } from "./orchestrator";

/**
 * Wire the RPC router's notification events to the orchestrator's
 * remote task notification handler.
 *
 * Call this once during app initialization after both the orchestrator
 * and the RPC router have been created.
 *
 * Also sets the RPC router on the orchestrator so it can route
 * worktree creation and task spawning to remote agents.
 */
export function bridgeRemoteActivityFeed(
  router: RpcRouter,
  orchestrator: Orchestrator,
): void {
  // Give the orchestrator a reference to the router for outbound RPCs
  // (worktree creation, task spawning, cancellation).
  orchestrator.setRpcRouter(router);

  // Listen for inbound notifications from the remote agent and forward
  // them to the orchestrator's handler. The orchestrator emits the
  // standard orchestrator:event and orchestrator:task-status events,
  // which the existing IPC relay in orchestrator-ipc.ts picks up and
  // sends to the renderer.
  router.on("notification", (notification: {
    profileId: string;
    method: string;
    params: unknown;
  }) => {
    // Only forward task-related notifications to the orchestrator.
    // Other notifications (e.g., progress for file operations) are
    // handled by their respective subsystems.
    const taskMethods = new Set(["task_event", "task_completed", "task_failed"]);
    if (!taskMethods.has(notification.method)) return;

    // Fire-and-forget — handleRemoteTaskNotification is async but we
    // don't need to await it here. Errors are handled internally by
    // the orchestrator (emitted as orchestrator:error events).
    orchestrator.handleRemoteTaskNotification(notification).catch((err) => {
      // Defensive: if the handler throws unexpectedly, log it but
      // don't crash the notification listener.
      console.error(
        "[ssh-activity-bridge] Error handling remote notification:",
        err instanceof Error ? err.message : String(err),
      );
    });
  });
}
