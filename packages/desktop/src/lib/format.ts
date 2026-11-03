/**
 * Strip embedded credentials from a git remote URL before displaying in the UI.
 * Handles patterns like:
 *   https://user:token@github.com/org/repo.git → https://github.com/org/repo.git
 *   https://x-access-token:ghp_xxx@github.com/... → https://github.com/...
 */
export function sanitizeRemoteUrl(url: string): string {
  try {
    // Match https://anything@host/... pattern
    return url.replace(/^(https?:\/\/)[^@/]+@/, "$1");
  } catch {
    return url;
  }
}

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

// Formats a byte count with a scaled SI unit and one decimal. We
// intentionally use decimal units (KB = 1000 B) because every crab
// CLI surface already reports sizes this way; mixing binary and
// decimal would confuse users cross-referencing the two.
export function formatBytes(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "–";
  if (n < 1000) return `${n} B`;
  let value = n;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit++;
  }
  return `${value.toFixed(value < 10 ? 2 : 1)} ${UNITS[unit]}`;
}

/**
 * Format a duration in milliseconds to a human-readable string.
 * Examples: "120ms", "3.2s", "1m 24s", "2h 5m".
 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "–";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  const remainSec = Math.round(seconds % 60);
  if (minutes < 60) return `${minutes}m ${remainSec}s`;
  const hours = Math.floor(minutes / 60);
  const remainMin = minutes % 60;
  return `${hours}h ${remainMin}m`;
}
