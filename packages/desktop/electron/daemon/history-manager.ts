/**
 * Checkpoint-based crash recovery for terminal sessions.
 *
 * Writes periodic terminal snapshots to disk so sessions can be
 * cold-restored after daemon death. Uses atomic writes (tmp + rename)
 * to prevent half-written checkpoints.
 *
 * Base path: ~/.crab/desktop/terminal-history/<sessionId>/
 * Files:
 *   meta.json     — session metadata (cwd, cols, rows, env, timestamps)
 *   checkpoint.json — serialized TerminalSnapshot + checkpoint timestamp
 */

import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import type { TerminalSnapshot } from "./types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface SessionMeta {
  sessionId: string;
  cwd: string;
  cols: number;
  rows: number;
  env?: Record<string, string>;
  /** Keys that were stripped from the original env by sanitizeEnv. */
  sanitizedKeys?: string[];
  createdAt: string;
  endedAt?: string;
  exitCode?: number;
  sensitive?: boolean;
}

export interface CheckpointData {
  snapshot: TerminalSnapshot;
  checkpointedAt: string;
}

export interface HistoryManagerOptions {
  basePath?: string;
  maxAgeDays?: number;
  maxSizeBytes?: number;
}

interface SessionWriter {
  sessionId: string;
  dirPath: string;
  sensitive: boolean;
}

// ---------------------------------------------------------------------------
// Environment variable sanitization
// ---------------------------------------------------------------------------

/** Known secret variable names to always strip. */
const KNOWN_SECRET_VARS = new Set([
  "AWS_SECRET_ACCESS_KEY",
  "GITHUB_TOKEN",
  "ANTHROPIC_API_KEY",
]);

/** Patterns that match secret-like variable names. */
const SECRET_PATTERNS = [/_SECRET/i, /_TOKEN/i, /_KEY/i];

/**
 * Returns true if the environment variable name looks like a secret.
 */
function isSecretVar(name: string): boolean {
  if (KNOWN_SECRET_VARS.has(name)) return true;
  return SECRET_PATTERNS.some((pattern) => pattern.test(name));
}

/**
 * Strip secret environment variables from a record before persisting.
 */
export function sanitizeEnv(
  env: Record<string, string> | undefined,
): Record<string, string> | undefined {
  if (!env) return undefined;
  const sanitized: Record<string, string> = {};
  for (const [key, value] of Object.entries(env)) {
    if (!isSecretVar(key)) {
      sanitized[key] = value;
    }
  }
  return sanitized;
}

/**
 * Returns the list of keys that would be stripped by sanitizeEnv.
 * Used to record which secrets were omitted so cold-restore can prompt
 * the user to supply replacement values.
 */
export function getSanitizedKeys(env: Record<string, string> | undefined): string[] {
  if (!env) return [];
  return Object.keys(env).filter((key) => isSecretVar(key));
}

// ---------------------------------------------------------------------------
// HistoryManager
// ---------------------------------------------------------------------------

export class HistoryManager {
  private readonly basePath: string;
  private readonly maxAgeDays: number;
  private readonly maxSizeBytes: number;
  private writers: Map<string, SessionWriter> = new Map();
  private dirtyVersions: Map<string, number> = new Map();
  private checkpointedVersions: Map<string, number> = new Map();
  private disabled = false;

  constructor(opts?: HistoryManagerOptions) {
    this.basePath =
      opts?.basePath ?? path.join(os.homedir(), ".crab", "desktop", "terminal-history");
    this.maxAgeDays = opts?.maxAgeDays ?? 7;
    this.maxSizeBytes = opts?.maxSizeBytes ?? 100 * 1024 * 1024; // 100 MB
  }

  /**
   * Open a session for history tracking. Creates the session directory,
   * writes initial meta.json, and cleans stale files from previous runs.
   */
  openSession(
    sessionId: string,
    opts: {
      cwd: string;
      cols: number;
      rows: number;
      env?: Record<string, string>;
      sanitizedKeys?: string[];
      sensitive?: boolean;
    },
  ): void {
    if (this.disabled) return;

    try {
      const dirPath = this.sessionDir(sessionId);
      fs.mkdirSync(dirPath, { recursive: true });

      // Clean stale checkpoint from previous run
      const checkpointPath = path.join(dirPath, "checkpoint.json");
      if (fs.existsSync(checkpointPath)) {
        fs.unlinkSync(checkpointPath);
      }

      // Write meta.json with sanitized env
      const meta: SessionMeta = {
        sessionId,
        cwd: opts.cwd,
        cols: opts.cols,
        rows: opts.rows,
        env: sanitizeEnv(opts.env),
        sanitizedKeys: opts.sanitizedKeys,
        createdAt: new Date().toISOString(),
        sensitive: opts.sensitive,
      };
      const metaPath = path.join(dirPath, "meta.json");
      this.atomicWrite(metaPath, JSON.stringify(meta, null, 2));

      const writer: SessionWriter = {
        sessionId,
        dirPath,
        sensitive: opts.sensitive ?? false,
      };
      this.writers.set(sessionId, writer);
      this.dirtyVersions.set(sessionId, 0);
      this.checkpointedVersions.set(sessionId, 0);
    } catch (err) {
      this.handleError("openSession", sessionId, err);
    }
  }

  /**
   * Register a writer for warm reattach — session already has history
   * on disk, just track it in memory for future checkpoints.
   */
  registerWriter(sessionId: string, sensitive?: boolean): void {
    if (this.disabled) return;

    try {
      const dirPath = this.sessionDir(sessionId);
      if (!fs.existsSync(dirPath)) {
        fs.mkdirSync(dirPath, { recursive: true });
      }

      const writer: SessionWriter = {
        sessionId,
        dirPath,
        sensitive: sensitive ?? false,
      };
      this.writers.set(sessionId, writer);
      this.dirtyVersions.set(sessionId, 0);
      this.checkpointedVersions.set(sessionId, 0);
    } catch (err) {
      this.handleError("registerWriter", sessionId, err);
    }
  }

  /**
   * Mark a session as having new data. Only sessions with incremented
   * dirty versions will be checkpointed.
   */
  markDirty(sessionId: string): void {
    const current = this.dirtyVersions.get(sessionId) ?? 0;
    this.dirtyVersions.set(sessionId, current + 1);
  }

  /**
   * Returns true if the session has new data since the last checkpoint.
   */
  isDirty(sessionId: string): boolean {
    const dirty = this.dirtyVersions.get(sessionId) ?? 0;
    const checkpointed = this.checkpointedVersions.get(sessionId) ?? 0;
    return dirty > checkpointed;
  }

  /**
   * Write a checkpoint for a session. Uses atomic tmp+rename.
   * Skips if the session is marked sensitive or has no new data.
   */
  checkpoint(sessionId: string, snapshot: TerminalSnapshot): void {
    if (this.disabled) return;

    const writer = this.writers.get(sessionId);
    if (!writer) return;

    // Sensitive sessions skip checkpoint entirely
    if (writer.sensitive) return;

    // Only checkpoint if dirty
    if (!this.isDirty(sessionId)) return;

    try {
      const data: CheckpointData = {
        snapshot,
        checkpointedAt: new Date().toISOString(),
      };

      const checkpointPath = path.join(writer.dirPath, "checkpoint.json");
      this.atomicWrite(checkpointPath, JSON.stringify(data));

      // Update checkpointed version to match dirty version
      const currentDirty = this.dirtyVersions.get(sessionId) ?? 0;
      this.checkpointedVersions.set(sessionId, currentDirty);
    } catch (err) {
      this.handleError("checkpoint", sessionId, err);
    }
  }

  /**
   * Close a session — update meta.json with ended timestamp and exit code.
   */
  closeSession(sessionId: string, exitCode: number): void {
    if (this.disabled) return;

    const writer = this.writers.get(sessionId);
    if (!writer) return;

    try {
      const metaPath = path.join(writer.dirPath, "meta.json");
      if (fs.existsSync(metaPath)) {
        const raw = fs.readFileSync(metaPath, "utf-8");
        const meta: SessionMeta = JSON.parse(raw);
        meta.endedAt = new Date().toISOString();
        meta.exitCode = exitCode;
        this.atomicWrite(metaPath, JSON.stringify(meta, null, 2));
      }

      this.writers.delete(sessionId);
      this.dirtyVersions.delete(sessionId);
      this.checkpointedVersions.delete(sessionId);
    } catch (err) {
      this.handleError("closeSession", sessionId, err);
    }
  }

  /**
   * Remove a session's history directory entirely.
   */
  removeSession(sessionId: string): void {
    if (this.disabled) return;

    try {
      const dirPath = this.sessionDir(sessionId);
      if (fs.existsSync(dirPath)) {
        fs.rmSync(dirPath, { recursive: true, force: true });
      }
      this.writers.delete(sessionId);
      this.dirtyVersions.delete(sessionId);
      this.checkpointedVersions.delete(sessionId);
    } catch (err) {
      this.handleError("removeSession", sessionId, err);
    }
  }

  /**
   * Read the on-disk meta for a session, including the list of env keys
   * that were stripped at save time. Returns null if the session directory
   * or meta.json does not exist, or if the file cannot be parsed.
   *
   * Used by the cold-restore path to surface sanitizedKeys to the renderer
   * so the user can be prompted to supply replacement values.
   */
  readMeta(sessionId: string): SessionMeta | null {
    try {
      const metaPath = path.join(this.sessionDir(sessionId), "meta.json");
      if (!fs.existsSync(metaPath)) return null;
      const raw = fs.readFileSync(metaPath, "utf-8");
      const meta: SessionMeta = JSON.parse(raw);
      // Ensure sanitizedKeys is always an array (older meta.json files may omit it)
      if (!meta.sanitizedKeys) {
        meta.sanitizedKeys = [];
      }
      return meta;
    } catch {
      return null;
    }
  }

  /**
   * Scan all session directories and return the list of sessions eligible
   * for cold restore: meta.json present without endedAt AND checkpoint.json
   * exists. Returns full restore info for each such session so the caller
   * can broadcast it to connected clients without additional disk reads.
   */
  listColdRestoreSessions(): Array<{
    sessionId: string;
    snapshot: TerminalSnapshot;
    savedAt: number;
    sanitizedKeys: string[];
    cwd: string | null;
    cols: number;
    rows: number;
  }> {
    const results: Array<{
      sessionId: string;
      snapshot: TerminalSnapshot;
      savedAt: number;
      sanitizedKeys: string[];
      cwd: string | null;
      cols: number;
      rows: number;
    }> = [];

    try {
      if (!fs.existsSync(this.basePath)) return results;

      const entries = fs.readdirSync(this.basePath, { withFileTypes: true });
      for (const entry of entries) {
        if (!entry.isDirectory()) continue;

        const sessionId = entry.name;
        const dirPath = path.join(this.basePath, sessionId);
        const metaPath = path.join(dirPath, "meta.json");
        const checkpointPath = path.join(dirPath, "checkpoint.json");

        if (!fs.existsSync(metaPath) || !fs.existsSync(checkpointPath)) continue;

        try {
          const meta: SessionMeta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
          if (meta.endedAt) continue; // cleanly ended — not a cold restore

          const checkpoint: CheckpointData = JSON.parse(
            fs.readFileSync(checkpointPath, "utf-8"),
          );

          results.push({
            sessionId,
            snapshot: checkpoint.snapshot,
            savedAt: new Date(checkpoint.checkpointedAt).getTime(),
            sanitizedKeys: meta.sanitizedKeys ?? [],
            cwd: meta.cwd ?? null,
            cols: meta.cols,
            rows: meta.rows,
          });
        } catch {
          // Malformed files — skip this session
        }
      }
    } catch (err) {
      this.handleError("listColdRestoreSessions", "all", err);
    }

    return results;
  }

  /**
   * Detect cold restore — check if a session has an unclean shutdown
   * (meta.json exists without endedAt timestamp and checkpoint exists).
   * Returns the recovered snapshot or null.
   */
  detectColdRestore(sessionId: string): TerminalSnapshot | null {
    try {
      const dirPath = this.sessionDir(sessionId);
      const metaPath = path.join(dirPath, "meta.json");
      const checkpointPath = path.join(dirPath, "checkpoint.json");

      if (!fs.existsSync(metaPath)) return null;
      if (!fs.existsSync(checkpointPath)) return null;

      const metaRaw = fs.readFileSync(metaPath, "utf-8");
      const meta: SessionMeta = JSON.parse(metaRaw);

      // If session was cleanly ended, no cold restore needed
      if (meta.endedAt) return null;

      const checkpointRaw = fs.readFileSync(checkpointPath, "utf-8");
      const checkpoint: CheckpointData = JSON.parse(checkpointRaw);

      return checkpoint.snapshot;
    } catch {
      // If we can't read the files, no restore possible
      return null;
    }
  }

  /**
   * Dispose — mark all tracked sessions as ended.
   */
  dispose(): void {
    for (const [sessionId] of this.writers) {
      try {
        const writer = this.writers.get(sessionId);
        if (!writer) continue;

        const metaPath = path.join(writer.dirPath, "meta.json");
        if (fs.existsSync(metaPath)) {
          const raw = fs.readFileSync(metaPath, "utf-8");
          const meta: SessionMeta = JSON.parse(raw);
          if (!meta.endedAt) {
            meta.endedAt = new Date().toISOString();
            this.atomicWrite(metaPath, JSON.stringify(meta, null, 2));
          }
        }
      } catch {
        // Best-effort during dispose
      }
    }
    this.writers.clear();
    this.dirtyVersions.clear();
    this.checkpointedVersions.clear();
  }

  /**
   * Prune old checkpoint directories. Removes sessions older than
   * maxAgeDays and enforces the maxSizeBytes cap.
   */
  prune(): void {
    if (this.disabled) return;

    try {
      if (!fs.existsSync(this.basePath)) return;

      const entries = fs.readdirSync(this.basePath, { withFileTypes: true });
      const now = Date.now();
      const maxAgeMs = this.maxAgeDays * 24 * 60 * 60 * 1000;

      interface DirInfo {
        name: string;
        fullPath: string;
        mtime: number;
        size: number;
      }

      const dirs: DirInfo[] = [];

      for (const entry of entries) {
        if (!entry.isDirectory()) continue;

        const fullPath = path.join(this.basePath, entry.name);
        const stat = fs.statSync(fullPath);
        const size = this.dirSize(fullPath);

        dirs.push({
          name: entry.name,
          fullPath,
          mtime: stat.mtimeMs,
          size,
        });
      }

      // Remove directories older than maxAgeDays
      const remaining: DirInfo[] = [];
      for (const dir of dirs) {
        if (now - dir.mtime > maxAgeMs) {
          fs.rmSync(dir.fullPath, { recursive: true, force: true });
        } else {
          remaining.push(dir);
        }
      }

      // Enforce size cap — remove oldest first until under limit
      remaining.sort((a, b) => a.mtime - b.mtime);
      let totalSize = remaining.reduce((sum, d) => sum + d.size, 0);

      while (totalSize > this.maxSizeBytes && remaining.length > 0) {
        const oldest = remaining.shift()!;
        fs.rmSync(oldest.fullPath, { recursive: true, force: true });
        totalSize -= oldest.size;
      }
    } catch (err) {
      this.handleError("prune", "all", err);
    }
  }

  /**
   * Whether the history manager has been disabled due to errors.
   */
  get isDisabled(): boolean {
    return this.disabled;
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private sessionDir(sessionId: string): string {
    return path.join(this.basePath, sessionId);
  }

  /**
   * Atomic write: write to a tmp file, then rename over the target.
   * This prevents half-written files if the process crashes mid-write.
   */
  private atomicWrite(targetPath: string, content: string): void {
    const tmpPath = targetPath + ".tmp." + process.pid;
    fs.writeFileSync(tmpPath, content, "utf-8");
    fs.renameSync(tmpPath, targetPath);
  }

  /**
   * Calculate total size of files in a directory (non-recursive for simplicity).
   */
  private dirSize(dirPath: string): number {
    try {
      const files = fs.readdirSync(dirPath);
      let total = 0;
      for (const file of files) {
        const filePath = path.join(dirPath, file);
        const stat = fs.statSync(filePath);
        if (stat.isFile()) {
          total += stat.size;
        }
      }
      return total;
    } catch {
      return 0;
    }
  }

  /**
   * Handle errors by disabling the history manager and logging a warning.
   * The daemon continues operating without history.
   */
  private handleError(operation: string, sessionId: string, err: unknown): void {
    this.disabled = true;
    const message = err instanceof Error ? err.message : String(err);
    // Log to stderr as a structured warning (matches daemon-logger pattern)
    const entry = {
      ts: new Date().toISOString(),
      level: "warn",
      msg: `HistoryManager disabled after error in ${operation}`,
      sessionId,
      error: message,
    };
    process.stderr.write(JSON.stringify(entry) + "\n");
  }
}
