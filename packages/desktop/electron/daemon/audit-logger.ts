/**
 * Optional audit logging for the PTY daemon.
 *
 * Enabled by setting the CRAB_DAEMON_AUDIT_LOG env var to a file path.
 * When enabled, logs session lifecycle events and agent state transitions
 * in JSON-lines format (one JSON object per line).
 *
 * File rotation: rotates at 50 MB, keeping 5 rotated files.
 * Files: audit.log, audit.log.1, audit.log.2, ..., audit.log.5
 *
 * When the env var is not set, the audit logger is a no-op.
 */

import * as fs from "fs";
import * as path from "path";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Maximum file size before rotation (50 MB). */
const MAX_FILE_SIZE_BYTES = 50 * 1024 * 1024;

/** Number of rotated files to keep. */
const MAX_ROTATED_FILES = 5;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type AuditEventType =
  | "session.created"
  | "session.attached"
  | "session.detached"
  | "session.killed"
  | "session.exited"
  | "agent.state_changed";

export interface AuditEvent {
  ts: string;
  event: AuditEventType;
  sessionId: string;
  [key: string]: unknown;
}

export interface AuditLoggerOptions {
  /** Path to the audit log file. If not provided, reads from CRAB_DAEMON_AUDIT_LOG. */
  filePath?: string;
  /** Maximum file size in bytes before rotation. Default: 50 MB. */
  maxFileSizeBytes?: number;
  /** Number of rotated files to keep. Default: 5. */
  maxRotatedFiles?: number;
  /** Function to get current file size. Exposed for testing. */
  getFileSize?: (filePath: string) => number;
  /** Function to check if a file exists. Exposed for testing. */
  fileExists?: (filePath: string) => boolean;
  /** Function to rename a file. Exposed for testing. */
  renameFile?: (oldPath: string, newPath: string) => void;
  /** Function to unlink a file. Exposed for testing. */
  unlinkFile?: (filePath: string) => void;
  /** Function to append to a file. Exposed for testing. */
  appendFile?: (filePath: string, data: string) => void;
  /** Function to create directories. Exposed for testing. */
  mkdirSync?: (dirPath: string, opts: { recursive: boolean }) => void;
}

// ---------------------------------------------------------------------------
// AuditLogger
// ---------------------------------------------------------------------------

export class AuditLogger {
  private readonly filePath: string | null;
  private readonly maxFileSizeBytes: number;
  private readonly maxRotatedFiles: number;
  private readonly getFileSize: (filePath: string) => number;
  private readonly fileExists: (filePath: string) => boolean;
  private readonly renameFile: (oldPath: string, newPath: string) => void;
  private readonly unlinkFile: (filePath: string) => void;
  private readonly appendFile: (filePath: string, data: string) => void;
  private readonly mkdirSync: (dirPath: string, opts: { recursive: boolean }) => void;
  private disposed = false;

  constructor(opts?: AuditLoggerOptions) {
    const envPath = opts?.filePath ?? process.env.CRAB_DAEMON_AUDIT_LOG;
    this.filePath = envPath?.trim() || null;
    this.maxFileSizeBytes = opts?.maxFileSizeBytes ?? MAX_FILE_SIZE_BYTES;
    this.maxRotatedFiles = opts?.maxRotatedFiles ?? MAX_ROTATED_FILES;

    this.getFileSize = opts?.getFileSize ?? defaultGetFileSize;
    this.fileExists = opts?.fileExists ?? defaultFileExists;
    this.renameFile = opts?.renameFile ?? defaultRenameFile;
    this.unlinkFile = opts?.unlinkFile ?? defaultUnlinkFile;
    this.appendFile = opts?.appendFile ?? defaultAppendFile;
    this.mkdirSync = opts?.mkdirSync ?? defaultMkdirSync;

    // Ensure the directory exists on construction if logging is enabled
    if (this.filePath) {
      const dir = path.dirname(this.filePath);
      this.mkdirSync(dir, { recursive: true });
    }
  }

  /**
   * Whether audit logging is enabled.
   */
  get enabled(): boolean {
    return this.filePath !== null && !this.disposed;
  }

  /**
   * Log a session created event.
   */
  sessionCreated(sessionId: string, fields?: Record<string, unknown>): void {
    this.write({
      ts: new Date().toISOString(),
      event: "session.created",
      sessionId,
      ...fields,
    });
  }

  /**
   * Log a session attached event.
   */
  sessionAttached(sessionId: string, fields?: Record<string, unknown>): void {
    this.write({
      ts: new Date().toISOString(),
      event: "session.attached",
      sessionId,
      ...fields,
    });
  }

  /**
   * Log a session detached event.
   */
  sessionDetached(sessionId: string, fields?: Record<string, unknown>): void {
    this.write({
      ts: new Date().toISOString(),
      event: "session.detached",
      sessionId,
      ...fields,
    });
  }

  /**
   * Log a session killed event.
   */
  sessionKilled(sessionId: string, fields?: Record<string, unknown>): void {
    this.write({
      ts: new Date().toISOString(),
      event: "session.killed",
      sessionId,
      ...fields,
    });
  }

  /**
   * Log a session exited event.
   */
  sessionExited(sessionId: string, fields?: Record<string, unknown>): void {
    this.write({
      ts: new Date().toISOString(),
      event: "session.exited",
      sessionId,
      ...fields,
    });
  }

  /**
   * Log an agent state transition.
   */
  agentStateChanged(
    sessionId: string,
    state: string,
    previousState: string,
    fields?: Record<string, unknown>,
  ): void {
    this.write({
      ts: new Date().toISOString(),
      event: "agent.state_changed",
      sessionId,
      state,
      previousState,
      ...fields,
    });
  }

  /**
   * Dispose the audit logger. After disposal, all writes are no-ops.
   */
  dispose(): void {
    this.disposed = true;
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private write(entry: AuditEvent): void {
    if (!this.filePath || this.disposed) return;

    try {
      this.rotateIfNeeded();
      const line = JSON.stringify(entry) + "\n";
      this.appendFile(this.filePath, line);
    } catch {
      // Audit logging is best-effort — never crash the daemon
    }
  }

  private rotateIfNeeded(): void {
    if (!this.filePath) return;

    let currentSize: number;
    try {
      currentSize = this.getFileSize(this.filePath);
    } catch {
      // File doesn't exist yet — no rotation needed
      return;
    }

    if (currentSize < this.maxFileSizeBytes) return;

    // Rotate: remove oldest, shift others up, rename current to .1
    const oldest = `${this.filePath}.${this.maxRotatedFiles}`;
    if (this.fileExists(oldest)) {
      this.unlinkFile(oldest);
    }

    for (let i = this.maxRotatedFiles - 1; i >= 1; i--) {
      const from = `${this.filePath}.${i}`;
      const to = `${this.filePath}.${i + 1}`;
      if (this.fileExists(from)) {
        this.renameFile(from, to);
      }
    }

    this.renameFile(this.filePath, `${this.filePath}.1`);
  }
}

// ---------------------------------------------------------------------------
// Default filesystem operations
// ---------------------------------------------------------------------------

function defaultGetFileSize(filePath: string): number {
  return fs.statSync(filePath).size;
}

function defaultFileExists(filePath: string): boolean {
  return fs.existsSync(filePath);
}

function defaultRenameFile(oldPath: string, newPath: string): void {
  fs.renameSync(oldPath, newPath);
}

function defaultUnlinkFile(filePath: string): void {
  fs.unlinkSync(filePath);
}

function defaultAppendFile(filePath: string, data: string): void {
  fs.appendFileSync(filePath, data, "utf-8");
}

function defaultMkdirSync(dirPath: string, opts: { recursive: boolean }): void {
  fs.mkdirSync(dirPath, opts);
}
