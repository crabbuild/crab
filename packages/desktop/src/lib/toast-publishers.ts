// Toast publishers — subscribe to agent events and publish toasts
// for job completion, PR updates, and agent errors.

import { onAgentEvent, type AgentEvent, type ProgressComplete } from "../ipc";
import { dismissToast, publishToast } from "./toasts";
import { getSettings } from "../settings";

// --- DND helper ---

/**
 * Returns true if "Do not disturb" is currently active.
 * Checks both the flag and the expiry timestamp. If the timestamp
 * has passed, DND is considered expired (auto-expire).
 */
function isDndActive(): boolean {
  const { notifications } = getSettings();
  if (!notifications.doNotDisturb) return false;
  if (notifications.doNotDisturbUntil === null) return true; // indefinite
  return Date.now() < notifications.doNotDisturbUntil;
}

// --- Job completion publisher ---

/**
 * Subscribes to `progress/complete` events and publishes a toast
 * for each finished job. Success → "success" toast, failure →
 * "error" toast with the op name as context.
 *
 * Returns an unsubscribe function.
 */
export function setupJobCompletionPublisher(): () => void {
  return onAgentEvent((evt: AgentEvent) => {
    if (evt.method !== "progress/complete") return;
    if (isDndActive()) return;
    const { notifications } = getSettings();
    if (notifications.muteJobCompletion) return;

    const p = evt.params as ProgressComplete;
    const label = formatOpLabel(p.op);

    if (p.outcome === "cancelled") {
      publishToast({ kind: "info", title: `${label} cancelled`, body: "The operation stopped. Files already published remain in place." });
    } else if (p.status === 0) {
      publishToast({
        kind: "success",
        title: `${label} completed`,
        body: `Job ${p.job_id} finished successfully.`,
      });
    } else {
      publishToast({
        kind: "error",
        title: `${label} failed`,
        body: `Exited with status ${p.status}.`,
      });
    }
  });
}

// --- PR update publisher ---

export interface PrUpdateParams {
  pr_number: number;
  action: string;
  actor?: string;
  title?: string;
}

/**
 * Subscribes to `forge/pr_activity` events emitted by the agent
 * when a PR receives new activity (comment, review, status change).
 *
 * Returns an unsubscribe function.
 */
export function setupPrUpdatePublisher(): () => void {
  return onAgentEvent((evt: AgentEvent) => {
    if (evt.method !== "forge/pr_activity") return;
    if (isDndActive()) return;
    const { notifications } = getSettings();
    if (notifications.mutePrUpdates) return;

    const p = evt.params as PrUpdateParams;
    const title = `PR #${p.pr_number} updated`;
    const body = p.actor
      ? `${formatPrAction(p.action)} from @${p.actor}`
      : formatPrAction(p.action);

    publishToast({ kind: "info", title, body });
  });
}

// --- Agent error publisher ---

export interface AgentErrorParams {
  code?: string;
  message: string;
}

/**
 * Subscribes to `agent/error` notifications and the `agent/exit`
 * event (unexpected crash). Publishes an error toast so the user
 * is aware without needing to check the log drawer.
 *
 * Returns an unsubscribe function.
 */
export function setupAgentErrorPublisher(): () => void {
  let unavailable: string | undefined;
  return onAgentEvent((evt: AgentEvent) => {
    // Local negotiation repairs service availability independently of each
    // repository's replay. Retire its outage even if notifications are muted.
    if (evt.method === "agent/ready") {
      if (unavailable) dismissToast(unavailable);
      unavailable = undefined;
      return;
    }
    if (isDndActive()) return;
    const { notifications } = getSettings();
    if (notifications.muteAgentErrors && evt.method === "agent/error") return;
    if (notifications.muteAgentErrors && evt.method === "agent/failed") return;
    if (notifications.muteAgentErrors && evt.method === "agent/exit") return;

    if (evt.method === "agent/error" || evt.method === "agent/failed") {
      const p = evt.params as AgentErrorParams;
      if (evt.method === "agent/failed" && unavailable) dismissToast(unavailable);
      const id = publishToast({
        kind: "error",
        title: evt.method === "agent/failed" ? "Desktop services unavailable" : "Agent error",
        body: p.message,
      });
      if (evt.method === "agent/failed") unavailable = id;
    } else if (evt.method === "agent/exit") {
      const p = evt.params as { code: number | null; signal: string | null };
      // Only toast unexpected exits (non-zero or signal-killed).
      if (p.code !== 0 || p.signal) {
        const reason = p.signal
          ? `Killed by signal ${p.signal}`
          : `Exited with code ${p.code}`;
        if (unavailable) dismissToast(unavailable);
        unavailable = publishToast({
          kind: "error",
          title: "Agent crashed",
          body: reason,
        });
      }
    }
  });
}

// --- Setup all publishers ---

/**
 * Convenience function that wires up all toast publishers at once.
 * Call from the app root (e.g. inside a `useEffect`). Returns a
 * teardown function that unsubscribes all listeners.
 */
export function setupAllToastPublishers(): () => void {
  const unsubs = [
    setupJobCompletionPublisher(),
    setupPrUpdatePublisher(),
    setupAgentErrorPublisher(),
  ];
  return () => {
    for (const unsub of unsubs) unsub();
  };
}

// --- Helpers ---

/** Capitalise and humanise an op string like "push" → "Push". */
function formatOpLabel(op: string): string {
  if (!op) return "Job";
  return op.charAt(0).toUpperCase() + op.slice(1).replace(/_/g, " ");
}

/** Map a PR action string to a human-readable phrase. */
function formatPrAction(action: string): string {
  switch (action) {
    case "comment":
      return "New comment";
    case "review":
      return "New review";
    case "status_change":
      return "Status changed";
    case "merged":
      return "Merged";
    case "closed":
      return "Closed";
    case "reopened":
      return "Reopened";
    default:
      return action.replace(/_/g, " ");
  }
}
