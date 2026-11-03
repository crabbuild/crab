/**
 * Terminal notification logic for command completion events.
 *
 * Determines when to fire desktop notifications based on command duration,
 * tab focus state, and user configuration. Also formats notification payloads
 * with appropriate title, body, and icon.
 */

export interface NotificationConfig {
  enabled: boolean;
  durationThresholdMs: number; // default: 15000
  soundEnabled: boolean;
  soundFile?: string; // custom audio file path
}

export interface CommandCompletion {
  command: string;
  exitCode: number;
  durationMs: number;
  terminalId: string;
  tabFocused: boolean;
}

export const DEFAULT_NOTIFICATION_CONFIG: NotificationConfig = {
  enabled: true,
  durationThresholdMs: 15000,
  soundEnabled: false,
};

/**
 * Determine whether a notification should fire for a completed command.
 *
 * A notification fires iff ALL three conditions hold:
 * 1. Notifications are enabled in config
 * 2. Command duration exceeds the configured threshold
 * 3. The terminal tab is NOT focused/visible
 */
export function shouldNotify(
  completion: CommandCompletion,
  config: NotificationConfig,
): boolean {
  if (!config.enabled) return false;
  if (completion.durationMs <= config.durationThresholdMs) return false;
  if (completion.tabFocused) return false;
  return true;
}

/**
 * Format the notification payload for a completed command.
 *
 * Returns a title (truncated command text), body (duration + exit status),
 * and icon type (success for exit 0, error for non-zero).
 */
export function formatNotification(completion: CommandCompletion): {
  title: string;
  body: string;
  icon: "success" | "error";
} {
  const icon: "success" | "error" = completion.exitCode === 0 ? "success" : "error";
  const status = completion.exitCode === 0 ? "completed" : `failed (exit ${completion.exitCode})`;

  // Truncate command to 80 characters per requirement 6.1
  const truncatedCommand =
    completion.command.length > 80
      ? completion.command.slice(0, 77) + "..."
      : completion.command;

  const title = `Command ${status}`;
  const body = `${truncatedCommand}\nDuration: ${formatDurationMs(completion.durationMs)}`;

  return { title, body, icon };
}

/** Format milliseconds into a human-readable duration string. */
function formatDurationMs(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  if (minutes < 60) return `${minutes}m ${remainingSeconds}s`;
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  return `${hours}h ${remainingMinutes}m`;
}
