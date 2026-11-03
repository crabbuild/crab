/**
 * App Path Mismatch Detection — detects when the daemon was spawned from
 * a different app binary path than the current one.
 *
 * This happens when the app is updated to a new location (e.g., moved from
 * /Applications/Crab.app to /Applications/Crab-2.app, or auto-updated to
 * a new version directory). The old daemon is stale because it was spawned
 * from a binary that may no longer exist or is outdated.
 *
 * On startup, the DaemonSpawner reads the PID file's `entryPath` field and
 * compares it to the current daemon entry path. If they differ, the old
 * daemon is considered stale, but a live daemon is not killed here: doing so
 * can terminate long-running PTY sessions. Instead, the app logs the mismatch
 * and continues to probe/reuse the live daemon. Files are cleaned up only when
 * the recorded process is already dead.
 */

import * as fs from "node:fs";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PidFileData {
  pid: number;
  startedAtMs: number;
  entryPath: string;
}

export interface PathMismatchResult {
  /** Whether a path mismatch was detected. */
  mismatch: boolean;
  /** The entry path recorded in the PID file (null if PID file unreadable). */
  pidFileEntryPath: string | null;
  /** The current daemon entry path being compared against. */
  currentEntryPath: string;
  /** Whether the old daemon process was successfully killed. */
  killed: boolean;
  /** Whether stale files were cleaned up. */
  cleanedUp: boolean;
}

export interface PathMismatchDetectionOptions {
  /** Path to the PID file. */
  pidPath: string;
  /** Path to the socket file. */
  socketPath: string;
  /** Path to the token file. */
  tokenPath: string;
  /** The current daemon entry path (what we'd spawn now). */
  currentEntryPath: string;
  /** Logger function for warnings. */
  log?: (level: "info" | "warn" | "error", message: string, fields?: Record<string, unknown>) => void;
  /** Override for process alive check (testability). */
  isProcessAlive?: (pid: number) => boolean;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/**
 * Read the PID file and check if the recorded `entryPath` matches the
 * current daemon entry path.
 *
 * Returns null if the PID file doesn't exist or is unreadable (no mismatch
 * can be determined — caller should proceed with normal startup).
 */
export function readPidFile(pidPath: string): PidFileData | null {
  try {
    const raw = fs.readFileSync(pidPath, "utf-8");
    const data = JSON.parse(raw) as PidFileData;

    // Validate required fields
    if (
      typeof data.pid !== "number" ||
      typeof data.startedAtMs !== "number" ||
      typeof data.entryPath !== "string"
    ) {
      return null;
    }

    return data;
  } catch {
    return null;
  }
}

/**
 * Detect whether the PID file's entry path differs from the current
 * daemon entry path. If a mismatch is found and the old process is alive,
 * preserve it so existing PTY sessions survive app path changes. Stale files
 * are cleaned up only when the old process is gone.
 *
 * Returns a result describing what happened.
 */
export function detectAndHandlePathMismatch(
  opts: PathMismatchDetectionOptions,
): PathMismatchResult {
  const {
    pidPath,
    socketPath,
    tokenPath,
    currentEntryPath,
    log = () => {},
    isProcessAlive = defaultIsProcessAlive,
  } = opts;

  const pidData = readPidFile(pidPath);

  // No PID file or unreadable — no mismatch can be determined
  if (!pidData) {
    return {
      mismatch: false,
      pidFileEntryPath: null,
      currentEntryPath,
      killed: false,
      cleanedUp: false,
    };
  }

  // Compare paths — normalize to handle trailing slashes or case differences
  const recordedPath = normalizePath(pidData.entryPath);
  const currentPath = normalizePath(currentEntryPath);

  if (recordedPath === currentPath) {
    // Paths match — no mismatch
    return {
      mismatch: false,
      pidFileEntryPath: pidData.entryPath,
      currentEntryPath,
      killed: false,
      cleanedUp: false,
    };
  }

  // Path mismatch detected
  log("warn", "App path mismatch detected: daemon was spawned from a different location", {
    pidFileEntryPath: pidData.entryPath,
    currentEntryPath,
    pid: pidData.pid,
    startedAtMs: pidData.startedAtMs,
  });

  let killed = false;
  let cleanedUp = false;

  if (isProcessAlive(pidData.pid)) {
    log("warn", "Preserving live daemon despite app path mismatch to keep PTY sessions alive", {
      pid: pidData.pid,
      oldPath: pidData.entryPath,
      newPath: currentEntryPath,
    });
    return {
      mismatch: true,
      pidFileEntryPath: pidData.entryPath,
      currentEntryPath,
      killed: false,
      cleanedUp: false,
    };
  } else {
    log("info", "Old daemon process is no longer alive, cleaning up stale files", {
      pid: pidData.pid,
    });
  }

  // Clean up stale files regardless of whether kill succeeded
  cleanedUp = cleanupDaemonFiles(socketPath, tokenPath, pidPath);

  if (cleanedUp) {
    log("info", "Cleaned up stale daemon files after path mismatch replacement", {
      socketPath,
      tokenPath,
      pidPath,
    });
  }

  return {
    mismatch: true,
    pidFileEntryPath: pidData.entryPath,
    currentEntryPath,
    killed,
    cleanedUp,
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Normalize a file path for comparison: resolve relative segments and
 * remove trailing slashes.
 */
function normalizePath(p: string): string {
  // Remove trailing slashes (but keep root "/")
  let normalized = p.replace(/\/+$/, "") || "/";
  // Collapse multiple slashes
  normalized = normalized.replace(/\/+/g, "/");
  return normalized;
}

/**
 * Remove daemon socket, token, and PID files.
 * Returns true if at least one file was successfully removed.
 */
function cleanupDaemonFiles(
  socketPath: string,
  tokenPath: string,
  pidPath: string,
): boolean {
  let anyRemoved = false;

  anyRemoved = safeUnlink(socketPath) || anyRemoved;
  anyRemoved = safeUnlink(tokenPath) || anyRemoved;
  anyRemoved = safeUnlink(pidPath) || anyRemoved;

  return anyRemoved;
}

/**
 * Safely unlink a file. Returns true if the file was removed.
 */
function safeUnlink(filePath: string): boolean {
  try {
    fs.unlinkSync(filePath);
    return true;
  } catch {
    return false;
  }
}

/**
 * Default implementation: check if a process is alive using signal 0.
 */
function defaultIsProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}


// ---------------------------------------------------------------------------
// Exports for testing
// ---------------------------------------------------------------------------

export { normalizePath, cleanupDaemonFiles, safeUnlink };
