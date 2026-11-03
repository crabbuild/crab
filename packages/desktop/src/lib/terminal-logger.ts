/**
 * Terminal session logging — disk-based log coordinator.
 *
 * Writes terminal output to `~/.crab-desktop/logs/{session-id}.log` with
 * support for ANSI-preserved and ANSI-stripped formats, log rotation at
 * 10 MB, and a configurable retention policy (default: 7 days).
 */

// ── Types ───────────────────────────────────────────────────────────

export type LogFormat = "ansi" | "plain";

export interface LoggerConfig {
  /** Whether logging is enabled. */
  enabled: boolean;
  /** Log format: "ansi" preserves escape sequences, "plain" strips them. */
  format: LogFormat;
  /** Maximum log file size in bytes before rotation (default: 10 MB). */
  maxSizeBytes: number;
  /** Retention period in days for rotated log files (default: 7). */
  retentionDays: number;
}

export interface LogFileInfo {
  path: string;
  exists: boolean;
  sizeBytes: number;
}

// ── Constants ───────────────────────────────────────────────────────

export const DEFAULT_LOGGER_CONFIG: LoggerConfig = {
  enabled: true,
  format: "ansi",
  maxSizeBytes: 10 * 1024 * 1024, // 10 MB
  retentionDays: 7,
};

const ANSI_REGEX = /\x1b\[[0-9;]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b\[[0-9;]*m|\x1b\(B|\x1b\[[\?]?[0-9;]*[hlJKHfG]|\x1b[=>]/g;

// ── ANSI stripping ──────────────────────────────────────────────────

/**
 * Strip all ANSI escape sequences from a string, preserving printable text.
 *
 * Removes CSI sequences (\x1b[...X), OSC sequences (\x1b]...BEL),
 * and other common terminal escape patterns.
 */
export function stripAnsi(input: string): string {
  return input.replace(ANSI_REGEX, "");
}

// ── Session Logger ──────────────────────────────────────────────────

/**
 * Per-session logger that writes terminal output to disk.
 *
 * This class is designed to be used in the main process (Node.js) where
 * it has access to the filesystem. The renderer communicates via IPC.
 */
export class SessionLogger {
  private sessionId: string;
  private config: LoggerConfig;
  private logDir: string;
  private currentPath: string;
  private currentSize: number;
  private disabled: boolean;
  private writeBuffer: string[];
  private flushTimer: ReturnType<typeof setTimeout> | null;

  constructor(sessionId: string, logDir: string, config: Partial<LoggerConfig> = {}) {
    this.sessionId = sessionId;
    this.config = { ...DEFAULT_LOGGER_CONFIG, ...config };
    this.logDir = logDir;
    this.currentPath = this.buildLogPath(sessionId);
    this.currentSize = 0;
    this.disabled = !this.config.enabled;
    this.writeBuffer = [];
    this.flushTimer = null;
  }

  private buildLogPath(sessionId: string): string {
    return `${this.logDir}/${sessionId}.log`;
  }

  /** Get the current log file path. */
  getLogPath(): string {
    return this.currentPath;
  }

  /** Check if logging is disabled (e.g., due to disk errors). */
  isDisabled(): boolean {
    return this.disabled;
  }

  /**
   * Append data to the session log.
   * Buffers writes and flushes periodically for performance.
   */
  write(data: string): void {
    if (this.disabled) return;

    const content = this.config.format === "plain" ? stripAnsi(data) : data;
    this.writeBuffer.push(content);

    // Flush every 500ms to batch writes
    if (!this.flushTimer) {
      this.flushTimer = setTimeout(() => {
        this.flush();
        this.flushTimer = null;
      }, 500);
    }
  }

  /**
   * Flush buffered writes to disk.
   * Returns the content to write, or null if nothing to flush.
   */
  flush(): string | null {
    if (this.disabled || this.writeBuffer.length === 0) return null;

    const content = this.writeBuffer.join("");
    this.writeBuffer = [];

    this.currentSize += content.length;
    return content; // Return content for the caller to write to disk
  }

  /**
   * Get rotation info if the current file needs rotating.
   * Returns { currentPath, rotatedPath } or null if no rotation needed.
   */
  getRotationInfo(): { currentPath: string; rotatedPath: string } | null {
    if (this.currentSize >= this.config.maxSizeBytes) {
      const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
      const rotatedPath = `${this.logDir}/${this.sessionId}.${timestamp}.log`;
      return { currentPath: this.currentPath, rotatedPath };
    }
    return null;
  }

  /** Reset the current size counter after rotation. */
  resetAfterRotation(): void {
    this.currentSize = 0;
  }

  /** Update the tracked file size (e.g., after reading from disk). */
  setCurrentSize(size: number): void {
    this.currentSize = size;
  }

  /** Disable logging for this session (e.g., on disk-full error). */
  disable(): void {
    this.disabled = true;
    this.writeBuffer = [];
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
  }

  /** Dispose the logger, flushing any remaining data. */
  dispose(): string | null {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    return this.flush();
  }
}

// ── Retention policy ────────────────────────────────────────────────

/**
 * Determine which rotated log files should be deleted based on retention policy.
 *
 * Given a list of file entries with names and modification times, returns
 * the paths of files older than the retention period.
 */
export function getExpiredLogFiles(
  files: Array<{ name: string; mtimeMs: number }>,
  retentionDays: number,
  nowMs: number = Date.now(),
): string[] {
  const cutoffMs = nowMs - retentionDays * 24 * 60 * 60 * 1000;
  return files
    .filter((f) => f.mtimeMs < cutoffMs)
    .map((f) => f.name);
}

/**
 * Check if a filename matches the rotated log pattern.
 * Rotated logs have the format: {session-id}.{timestamp}.log
 */
export function isRotatedLogFile(filename: string): boolean {
  // Match pattern: anything.YYYY-MM-DDTHH-MM-SS-mmmZ.log
  return /\.\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z\.log$/.test(filename);
}
