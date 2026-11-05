/**
 * TerminalHost — session container managing the lifecycle of all PTY sessions.
 *
 * Responsibilities:
 * - createOrAttach with idempotent semantics
 * - Tombstone tracking (LRU at 1000) to prevent resurrection of killed sessions
 * - Capacity enforcement (max 50 concurrent sessions)
 * - Dimension normalization (clamp 1×1 minimum)
 * - Routing write/resize/kill/signal/getCwd/getSnapshot/clearScrollback/listSessions
 * - dispose() with forceKillAndDisposeSubprocess on all sessions
 */

import { Session } from "./session";
import type { SubprocessSpawner } from "./subprocess-handle";
import { AdoptedPtySubprocessHandle } from "./adopted-pty-subprocess-handle";
import type {
  AgentMeta,
  SessionInfo,
  StreamClient,
  TerminalSnapshot,
} from "./types";
import type { LiveSessionInfo, AdoptedSession } from "./fd-handoff";
import type { QuotaManagedSession } from "./resource-quotas";
import type { ManagedSession } from "./memory-management";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Maximum number of concurrent live sessions. */
const MAX_SESSIONS = 50;

/** Maximum number of tombstones before LRU eviction. */
const MAX_TOMBSTONES = 1000;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

export interface CreateOrAttachOptions {
  sessionId: string;
  cols: number;
  rows: number;
  cwd?: string;
  env?: Record<string, string>;
  command?: string;
  shellReadySupported?: boolean;
  agentMeta?: AgentMeta;
  streamClient: StreamClient;
}

export interface CreateOrAttachResult {
  isNew: boolean;
  snapshot: TerminalSnapshot | null;
  pid: number | null;
  shellState: string;
}

export interface TerminalHostOptions {
  spawnSubprocess: SubprocessSpawner;
  maxSessions?: number;
  maxTombstones?: number;
  /** Called when a session exits — used for checkpoint writes. */
  onSessionExit?: (sessionId: string, exitCode: number) => void;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export class SessionTombstonedError extends Error {
  constructor(sessionId: string) {
    super(`Session has been killed and cannot be resurrected: ${sessionId}`);
    this.name = "SessionTombstonedError";
  }
}

export class SessionCapacityError extends Error {
  constructor(max: number) {
    super(`Maximum session capacity reached: ${max}`);
    this.name = "SessionCapacityError";
  }
}

export class SessionNotFoundError extends Error {
  constructor(sessionId: string) {
    super(`Session not found: ${sessionId}`);
    this.name = "SessionNotFoundError";
  }
}

// ---------------------------------------------------------------------------
// TerminalHost
// ---------------------------------------------------------------------------

export class TerminalHost {
  private sessions: Map<string, Session> = new Map();
  private tombstones: Map<string, number> = new Map(); // sessionId → timestamp
  private spawnSubprocess: SubprocessSpawner;
  private maxSessions: number;
  private maxTombstones: number;
  private onSessionExitCallback?: (sessionId: string, exitCode: number) => void;
  private disposed = false;

  constructor(opts: TerminalHostOptions) {
    this.spawnSubprocess = opts.spawnSubprocess;
    this.maxSessions = opts.maxSessions ?? MAX_SESSIONS;
    this.maxTombstones = opts.maxTombstones ?? MAX_TOMBSTONES;
    this.onSessionExitCallback = opts.onSessionExit;
  }

  /**
   * Create a new session or reattach to an existing one.
   *
   * Idempotent semantics:
   * - Existing live session → detach all clients, attach new, return snapshot
   * - Session in "exiting" state → treat as non-existent, dispose old, spawn new
   * - Tombstoned session → reject with error
   * - No session → spawn new PTY
   */
  createOrAttach(opts: CreateOrAttachOptions): CreateOrAttachResult {
    const { sessionId } = opts;
    this.pruneExitedSessions();

    // Clamp dimensions to minimum 1×1
    const cols = Math.max(1, Math.floor(opts.cols) || 1);
    const rows = Math.max(1, Math.floor(opts.rows) || 1);

    // Check tombstone — reject resurrection attempts
    if (this.tombstones.has(sessionId)) {
      throw new SessionTombstonedError(sessionId);
    }

    const existing = this.sessions.get(sessionId);

    if (existing) {
      // Session in "exiting" state → treat as non-existent
      if (existing.state === "exiting") {
        existing.forceKillAndDisposeSubprocess();
        this.sessions.delete(sessionId);
        return this.spawnNewSession(opts, cols, rows);
      }

      // Session already exited → clean up and spawn new
      if (existing.state === "exited") {
        this.sessions.delete(sessionId);
        return this.spawnNewSession(opts, cols, rows);
      }

      // Live session → detach all + attach new + return snapshot
      existing.detachAllClients();
      existing.attachClient(opts.streamClient);

      const snapshot = existing.getSnapshot();
      return {
        isNew: false,
        snapshot,
        pid: existing.pid,
        shellState: existing.shellState,
      };
    }

    // No existing session → spawn new
    return this.spawnNewSession(opts, cols, rows);
  }

  /**
   * Write data to a session's PTY stdin.
   */
  write(sessionId: string, data: string): void {
    const session = this.getSessionOrThrow(sessionId);
    session.write(data);
  }

  /**
   * Resize a session's PTY and headless emulator.
   * Dimensions are clamped to minimum 1×1.
   */
  resize(sessionId: string, cols: number, rows: number): void {
    const session = this.getSessionOrThrow(sessionId);
    const clampedCols = Math.max(1, Math.floor(cols) || 1);
    const clampedRows = Math.max(1, Math.floor(rows) || 1);
    session.resize(clampedCols, clampedRows);
  }

  /**
   * Kill a session: records tombstone, sends SIGTERM + 5s SIGKILL timer.
   */
  kill(sessionId: string): void {
    const session = this.getSessionOrThrow(sessionId);
    this.addTombstone(sessionId);
    session.kill();
  }

  /**
   * Send an arbitrary signal to a session's subprocess.
   */
  signal(sessionId: string, sig: string): void {
    const session = this.getSessionOrThrow(sessionId);
    session.signal(sig);
  }

  /**
   * Get the current working directory from OSC-7 tracking.
   */
  getCwd(sessionId: string): string | null {
    const session = this.getSessionOrThrow(sessionId);
    return session.getCwd();
  }

  /**
   * Get the current terminal snapshot from the headless emulator.
   */
  getSnapshot(sessionId: string): TerminalSnapshot | null {
    const session = this.getSessionOrThrow(sessionId);
    return session.getSnapshot();
  }

  /**
   * Clear the headless emulator's scrollback buffer.
   */
  clearScrollback(sessionId: string): void {
    const session = this.getSessionOrThrow(sessionId);
    session.clearScrollback();
  }

  /**
   * Detach one stream client from a live session without killing the PTY.
   */
  detachClient(sessionId: string, token: symbol): void {
    const session = this.sessions.get(sessionId);
    if (!session || !session.isAlive) return;
    session.detachClient(token);
  }

  /**
   * List all live sessions with their metadata.
   */
  listSessions(): SessionInfo[] {
    this.pruneExitedSessions();
    const result: SessionInfo[] = [];
    for (const session of this.sessions.values()) {
      result.push({
        sessionId: session.sessionId,
        state: session.state,
        shellState: session.shellState,
        isAlive: session.isAlive,
        pid: session.pid,
        initialCwd: session.initialCwd,
        cwd: session.getCwd(),
        cols: session.cols,
        rows: session.rows,
        createdAt: session.createdAt,
        lastDataAt: session.lastDataAt,
        agentMeta: session.agentMeta
          ? {
              ...session.agentMeta,
              activityState: "idle",
              stateChangedAt: session.createdAt,
            }
          : undefined,
      });
    }
    return result;
  }

  /**
   * Get live session info for fd-handoff serialization.
   * Returns info for all sessions in "running" state.
   */
  getLiveSessionInfo(): LiveSessionInfo[] {
    this.pruneExitedSessions();
    const result: LiveSessionInfo[] = [];
    for (const session of this.sessions.values()) {
      if (session.isAlive) {
        result.push({
          id: session.sessionId,
          pid: session.pid,
          initialCwd: session.initialCwd,
          cwd: session.getCwd(),
          cols: session.cols,
          rows: session.rows,
          agentMeta: session.agentMeta,
        });
      }
    }
    return result;
  }

  /**
   * Adopt a session from a handoff snapshot.
   *
   * Wraps the inherited fd in an AdoptedPtySubprocessHandle, constructs a
   * Session bypassing the capacity gate (these sessions existed in the
   * predecessor), inserts it into the sessions map, and replays the pending
   * buffer through the data path so attached clients see no gap.
   *
   * If the adopted session carries agentMeta, the caller (DaemonServer) is
   * responsible for re-registering it with AgentTracker.
   */
  adoptSession(adopted: AdoptedSession): Session {
    const handle = new AdoptedPtySubprocessHandle(adopted);

    let session!: Session;
    session = new Session({
      sessionId: adopted.id,
      cols: adopted.meta.cols,
      rows: adopted.meta.rows,
      cwd: adopted.meta.initialCwd ?? adopted.meta.cwd,
      subprocess: handle,
      shellReadySupported: false,
      agentMeta: adopted.meta.agentMeta,
      onExit: (sessionId, exitCode) => {
        if (this.sessions.get(sessionId) === session) {
          this.sessions.delete(sessionId);
        }
        this.onSessionExitCallback?.(sessionId, exitCode);
      },
    });

    // Insert directly — bypass capacity gate since these sessions already existed
    this.sessions.set(adopted.id, session);

    // Replay pending buffer through the data path so clients see no gap.
    // The Session wired its handleSubprocessData via subprocess.onData in its
    // constructor, so emitting through the read stream drives the full path
    // (emulator write + client fan-out). We emit directly on the read stream.
    if (adopted.buffer.length > 0) {
      handle.replayBuffer(adopted.buffer);
    }

    return session;
  }

  /**
   * Return a quota-manager view of all sessions: sessionId, isAlive,
   * createdAt, and a kill callback.  Used by ResourceQuotaManager to
   * enforce the maxSessionLifetimeHours quota.
   */
  getQuotaSessions(): QuotaManagedSession[] {
    this.pruneExitedSessions();
    const result: QuotaManagedSession[] = [];
    for (const session of this.sessions.values()) {
      const s = session; // capture for closure
      result.push({
        sessionId: s.sessionId,
        isAlive: s.isAlive,
        createdAt: s.createdAt,
        kill: () => {
          try {
            this.kill(s.sessionId);
          } catch {
            // Session may have already exited or been tombstoned
          }
        },
      });
    }
    return result;
  }

  /**
   * Return a memory-manager view of all sessions.
   * Used by MemoryManager for idle trimming and high-throughput detection.
   */
  listManagedSessions(): ManagedSession[] {
    this.pruneExitedSessions();
    const result: ManagedSession[] = [];
    for (const session of this.sessions.values()) {
      const s = session; // capture for closure
      result.push({
        sessionId: s.sessionId,
        isAlive: s.isAlive,
        lastDataAt: s.lastDataAt,
        getScrollbackLines: () => {
          const snapshot = s.getSnapshot();
          return snapshot?.scrollbackLines ?? 0;
        },
        clearScrollback: () => {
          s.clearScrollback();
        },
      });
    }
    return result;
  }

  /**
   * Toggle high-throughput mode for a session.
   * In high-throughput mode the headless emulator skips writes to reduce CPU cost.
   */
  setHighThroughput(sessionId: string, on: boolean): void {
    const session = this.sessions.get(sessionId);
    if (session) {
      session.setHighThroughput(on);
    }
  }

  /**
   * Get PTY master file descriptors for all live sessions (in same order
   * as getLiveSessionInfo).
   */
  getPtyFds(): number[] {
    this.pruneExitedSessions();
    const result: number[] = [];
    for (const session of this.sessions.values()) {
      if (session.isAlive) {
        result.push(session.fd);
      }
    }
    return result;
  }

  /**
   * Dispose all sessions — forceKill + dispose subprocess on each.
   * Called during daemon graceful shutdown.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    for (const session of this.sessions.values()) {
      session.forceKillAndDisposeSubprocess();
    }
    this.sessions.clear();
    this.tombstones.clear();
  }

  /**
   * Number of active sessions.
   */
  get sessionCount(): number {
    this.pruneExitedSessions();
    return this.sessions.size;
  }

  /**
   * Whether createOrAttach can reuse an existing live session without spawning.
   * Used by the daemon server to avoid applying new-session quotas to reattach.
   */
  hasAttachableSession(sessionId: string): boolean {
    const session = this.sessions.get(sessionId);
    return !!session && session.isAlive && session.state !== "exiting";
  }

  /**
   * Number of tombstones tracked.
   */
  get tombstoneCount(): number {
    return this.tombstones.size;
  }

  /**
   * Pause a session's PTY read stream (backpressure).
   */
  pauseSession(sessionId: string): void {
    const session = this.getSessionOrThrow(sessionId);
    session.pauseSubprocess();
  }

  /**
   * Resume a session's PTY read stream (backpressure relief).
   */
  resumeSession(sessionId: string): void {
    const session = this.getSessionOrThrow(sessionId);
    session.resumeSubprocess();
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private spawnNewSession(
    opts: CreateOrAttachOptions,
    cols: number,
    rows: number,
  ): CreateOrAttachResult {
    // Enforce capacity limit
    if (this.sessions.size >= this.maxSessions) {
      throw new SessionCapacityError(this.maxSessions);
    }

    // GUI-launched Windows daemons have no Unix SHELL; use the native command
    // processor when the RPC caller has not selected an executable.
    const defaultShell = process.platform === "win32"
      ? process.env.COMSPEC || "cmd.exe"
      : process.env.SHELL ?? "/bin/sh";
    const file = opts.command ?? defaultShell;
    const subprocess = this.spawnSubprocess({
      file,
      cols,
      rows,
      cwd: opts.cwd,
      env: opts.env,
    });

    let session!: Session;
    session = new Session({
      sessionId: opts.sessionId,
      cols,
      rows,
      cwd: opts.cwd ?? null,
      subprocess,
      shellReadySupported: opts.shellReadySupported ?? false,
      agentMeta: opts.agentMeta,
      onExit: (sessionId, exitCode) => {
        if (this.sessions.get(sessionId) === session) {
          this.sessions.delete(sessionId);
        }
        this.onSessionExitCallback?.(sessionId, exitCode);
      },
    });

    this.sessions.set(opts.sessionId, session);
    session.attachClient(opts.streamClient);

    return {
      isNew: true,
      snapshot: null,
      pid: session.pid,
      shellState: session.shellState,
    };
  }

  private getSessionOrThrow(sessionId: string): Session {
    const session = this.sessions.get(sessionId);
    if (!session) {
      throw new SessionNotFoundError(sessionId);
    }
    return session;
  }

  private addTombstone(sessionId: string): void {
    // If already at capacity, evict the oldest entry (LRU)
    if (this.tombstones.size >= this.maxTombstones) {
      // Map iteration order is insertion order — first key is oldest
      const oldest = this.tombstones.keys().next().value;
      if (oldest !== undefined) {
        this.tombstones.delete(oldest);
      }
    }
    this.tombstones.set(sessionId, Date.now());
  }

  private pruneExitedSessions(): void {
    for (const [sessionId, session] of this.sessions) {
      if (session.state === "exited") {
        this.sessions.delete(sessionId);
      }
    }
  }
}
