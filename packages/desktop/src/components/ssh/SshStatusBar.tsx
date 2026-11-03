/**
 * SSH Status Bar Integration — shows connection status in the
 * application status bar (bottom). Displays "SSH: N connected"
 * or individual server names with status dots.
 */

// Status bar component

// ── Types ────────────────────────────────────────────────────────

interface SshStatusBarConnection {
  profileId: string;
  label: string;
  status: "connected" | "connecting" | "reconnecting" | "error";
  latencyMs?: number;
  colour?: string;
}

interface SshStatusBarProps {
  connections: SshStatusBarConnection[];
  onClick: () => void;
}

// ── Helpers ──────────────────────────────────────────────────────

function getStatusDotColor(status: string, latencyMs?: number): string {
  if (status === "error") return "var(--color-error)";
  if (status === "connecting" || status === "reconnecting") return "var(--color-warning)";
  if (status === "connected") {
    if (latencyMs !== undefined) {
      if (latencyMs < 50) return "var(--color-success)";
      if (latencyMs < 200) return "var(--color-warning)";
      return "var(--color-error)";
    }
    return "var(--color-success)";
  }
  return "var(--color-muted)";
}

// ── Component ────────────────────────────────────────────────────

export function SshStatusBar({ connections, onClick }: SshStatusBarProps) {
  if (connections.length === 0) return null;

  const connectedCount = connections.filter((c) => c.status === "connected").length;
  const hasIssues = connections.some((c) => c.status === "error" || c.status === "reconnecting");

  return (
    <button
      className="ssh-status-bar"
      onClick={onClick}
      aria-label={`SSH: ${connectedCount} connected${hasIssues ? " (issues detected)" : ""}`}
      title={connections.map((c) => `${c.label}: ${c.status}`).join("\n")}
    >
      <span className="ssh-status-bar__icon" aria-hidden="true">⬡</span>
      {connections.length <= 2 ? (
        // Show individual names for 1-2 connections
        <span className="ssh-status-bar__names">
          {connections.map((conn) => (
            <span key={conn.profileId} className="ssh-status-bar__connection">
              <span
                className="ssh-status-bar__dot"
                style={{ backgroundColor: getStatusDotColor(conn.status, conn.latencyMs) }}
                aria-hidden="true"
              />
              <span className="ssh-status-bar__label">{conn.label}</span>
            </span>
          ))}
        </span>
      ) : (
        // Show count for 3+ connections
        <span className="ssh-status-bar__count">
          SSH: {connectedCount}/{connections.length}
          {hasIssues && <span className="ssh-status-bar__warning" aria-hidden="true">⚠</span>}
        </span>
      )}
    </button>
  );
}

// ── Notification Types ───────────────────────────────────────────

export type SshNotificationType =
  | "connected"
  | "disconnected"
  | "reconnect_failed"
  | "deploy_complete"
  | "port_forward_detected"
  | "idle_timeout";

export interface SshNotification {
  type: SshNotificationType;
  profileLabel: string;
  message: string;
  colour: string;
  action?: { label: string; handler: () => void };
  autoDismissMs?: number;
}

/**
 * Generate a notification for an SSH event.
 */
export function createSshNotification(
  type: SshNotificationType,
  profileLabel: string,
  detail?: string,
  action?: { label: string; handler: () => void },
): SshNotification {
  switch (type) {
    case "connected":
      return {
        type,
        profileLabel,
        message: `Connected to ${profileLabel}`,
        colour: "var(--color-success)",
        autoDismissMs: 3000,
      };
    case "disconnected":
      return {
        type,
        profileLabel,
        message: `Disconnected from ${profileLabel}`,
        colour: "var(--color-muted)",
        autoDismissMs: 5000,
      };
    case "reconnect_failed":
      return {
        type,
        profileLabel,
        message: `Failed to reconnect to ${profileLabel}`,
        colour: "var(--color-error)",
        action: action ?? { label: "Retry", handler: () => {} },
      };
    case "deploy_complete":
      return {
        type,
        profileLabel,
        message: `Agent deployed to ${profileLabel}`,
        colour: "var(--color-info)",
        autoDismissMs: 3000,
      };
    case "port_forward_detected":
      return {
        type,
        profileLabel,
        message: detail ?? `Port detected on ${profileLabel}`,
        colour: "var(--color-accent)",
        action: action ?? { label: "Forward", handler: () => {} },
      };
    case "idle_timeout":
      return {
        type,
        profileLabel,
        message: `Disconnected from ${profileLabel} due to inactivity`,
        colour: "var(--color-muted)",
        action: { label: "Reconnect", handler: () => {} },
      };
  }
}

// ── Idle Timeout ─────────────────────────────────────────────────

/**
 * Idle timeout manager — disconnects after N minutes of inactivity.
 * Configurable per profile (default: never).
 */
export class IdleTimeoutManager {
  private timers = new Map<string, ReturnType<typeof setTimeout>>();
  private onTimeout: (profileId: string) => void;

  constructor(onTimeout: (profileId: string) => void) {
    this.onTimeout = onTimeout;
  }

  /**
   * Start or reset the idle timer for a profile.
   * Call this on any user activity for the connection.
   */
  resetTimer(profileId: string, timeoutMinutes: number): void {
    this.clearTimer(profileId);

    if (timeoutMinutes <= 0) return; // 0 = never timeout

    const timer = setTimeout(() => {
      this.timers.delete(profileId);
      this.onTimeout(profileId);
    }, timeoutMinutes * 60 * 1000);

    this.timers.set(profileId, timer);
  }

  /**
   * Clear the idle timer (e.g., on disconnect).
   */
  clearTimer(profileId: string): void {
    const timer = this.timers.get(profileId);
    if (timer) {
      clearTimeout(timer);
      this.timers.delete(profileId);
    }
  }

  /**
   * Clear all timers (e.g., on app quit).
   */
  clearAll(): void {
    for (const timer of this.timers.values()) {
      clearTimeout(timer);
    }
    this.timers.clear();
  }
}

// ── Copy SSH Command ─────────────────────────────────────────────

/**
 * Generate an SSH command string for a profile (for clipboard copy).
 */
export function generateSshCommand(profile: {
  host: string;
  port: number;
  username: string;
  authMethod: { type: string; keyPath?: string };
  jumpHost?: string;
}): string {
  const parts = ["ssh"];

  if (profile.authMethod.type === "key" || profile.authMethod.type === "key_passphrase") {
    if (profile.authMethod.keyPath) {
      parts.push(`-i ${profile.authMethod.keyPath}`);
    }
  }

  if (profile.port !== 22) {
    parts.push(`-p ${profile.port}`);
  }

  if (profile.jumpHost) {
    parts.push(`-J ${profile.jumpHost}`);
  }

  parts.push(`${profile.username}@${profile.host}`);

  return parts.join(" ");
}
