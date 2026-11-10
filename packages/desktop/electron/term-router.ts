/**
 * TermRouter — per-session IPC routing for terminal sessions.
 *
 * Registers crab:term-spawn / write / resize / kill exactly once and
 * dispatches each request to either the DaemonBridge or the
 * InProcessBridge based on the resolved session type.
 *
 * Renderer IDs are partitioned:
 *   term-d-UUID  → daemon-backed session
 *   term-l-UUID  → in-process (local) session
 *
 * Refusing daemon spawns when the daemon is unavailable or the circuit
 * breaker is open is a hard error — no silent fallback.
 */

import type { CircuitBreaker } from "./daemon/circuit-breaker";
import type { AgentMeta } from "./daemon/types";
import { randomUUID } from "node:crypto";

// ---------------------------------------------------------------------------
// Minimal interface types (avoid pulling in full Electron at test time)
// ---------------------------------------------------------------------------

export interface IpcMainLike {
  handle(channel: string, listener: (...args: unknown[]) => unknown): void;
  removeHandler(channel: string): void;
}

export interface BrowserWindowLike {
  isDestroyed(): boolean;
  webContents: {
    send(channel: string, ...args: unknown[]): void;
  };
}

// ---------------------------------------------------------------------------
// Session type
// ---------------------------------------------------------------------------

export type SessionType = "daemon" | "in-process";

// ---------------------------------------------------------------------------
// Spawn payload (renderer → main)
// ---------------------------------------------------------------------------

export interface TermSpawnPayload {
  cwd: string;
  ownerPath?: string;
  sessionType?: SessionType;
  shell?: string;
  env?: Record<string, string>;
  startupCommand?: string;
  agentMeta?: AgentMeta;
}

// ---------------------------------------------------------------------------
// Spawn result (main → renderer)
// ---------------------------------------------------------------------------

export interface TermSpawnResult {
  /** Renderer-facing session ID: term-d-UUID or term-l-UUID. */
  id: string;
  resolvedSessionType: SessionType;
}

// ---------------------------------------------------------------------------
// Typed error result (returned instead of throwing for refusals)
// ---------------------------------------------------------------------------

export interface TermSpawnError {
  ok: false;
  error: "daemon-unavailable" | "daemon-circuit-open" | "app-shutting-down";
  message: string;
  /** Remaining cooldown in ms — only present for daemon-circuit-open. */
  cooldownRemainingMs?: number;
}

// ---------------------------------------------------------------------------
// Bridge interfaces
//
// Both bridges expose the same surface so the router can call them
// uniformly. The actual implementations live in:
//   - electron/daemon/daemon-bridge.ts  (task 1.3)
//   - electron/in-process-bridge.ts     (task 1.2)
// ---------------------------------------------------------------------------

export interface DaemonBridge {
  handleSpawn(
    payload: TermSpawnPayload,
    rendererId: string,
  ): Promise<{ daemonSessionId: string; snapshot?: unknown }>;
  handleWrite(rendererId: string, data: string): Promise<void>;
  handleResize(rendererId: string, cols: number, rows: number): Promise<void>;
  handleDetach(rendererId: string): Promise<void>;
  handleKill(rendererId: string): Promise<void>;
  /** Forward daemon session data to the renderer. */
  handleSessionData(daemonSessionId: string, data: string): void;
  /** Forward daemon session exit to the renderer. */
  handleSessionExit(daemonSessionId: string, code: number): void;
  dispose(): void;
}

export interface InProcessBridge {
  handleSpawn(
    payload: TermSpawnPayload,
    rendererId: string,
  ): Promise<void>;
  handleWrite(rendererId: string, data: string): Promise<void>;
  handleResize(rendererId: string, cols: number, rows: number): Promise<void>;
  handleKill(rendererId: string): Promise<void>;
  dispose(): void;
}

// ---------------------------------------------------------------------------
// Mode resolution
// ---------------------------------------------------------------------------

export interface ModeResolution {
  /** Whether the daemon is currently available for new sessions. */
  daemonReady: boolean;
  configuredMode?: "auto" | "daemon" | "in-process";
}

export interface ModeResolutionProvider {
  current(): ModeResolution;
}

// ---------------------------------------------------------------------------
// TermRouter options
// ---------------------------------------------------------------------------

export interface TermRouterOptions {
  ipcMain: IpcMainLike;
  daemonBridge: DaemonBridge;
  inProcessBridge: InProcessBridge;
  modeResolution: ModeResolutionProvider;
  circuitBreaker: CircuitBreaker;
  getMainWindow: () => BrowserWindowLike | null;
}

// ---------------------------------------------------------------------------
// TermRouter
// ---------------------------------------------------------------------------

export class TermRouter {
  private readonly ipcMain: IpcMainLike;
  private readonly daemonBridge: DaemonBridge;
  private readonly inProcessBridge: InProcessBridge;
  private readonly modeResolution: ModeResolutionProvider;
  private readonly circuitBreaker: CircuitBreaker;

  /** Renderer ID → session type, so write/resize/kill route correctly. */
  private readonly sessionTypeMap = new Map<string, SessionType>();

  /** Whether register() has been called. Prevents double-registration. */
  private registered = false;

  constructor(opts: TermRouterOptions) {
    this.ipcMain = opts.ipcMain;
    this.daemonBridge = opts.daemonBridge;
    this.inProcessBridge = opts.inProcessBridge;
    this.modeResolution = opts.modeResolution;
    this.circuitBreaker = opts.circuitBreaker;
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /**
   * Register all crab:term-* IPC handlers exactly once.
   * Calling register() a second time is a no-op.
   */
  register(): void {
    if (this.registered) return;
    this.registered = true;

    this.ipcMain.handle(
      "crab:term-spawn",
      ((_evt: unknown, payload: TermSpawnPayload) =>
        this.handleSpawn(payload)) as (...args: unknown[]) => unknown,
    );

    this.ipcMain.handle(
      "crab:term-write",
      ((_evt: unknown, id: string, data: string) =>
        this.handleWrite(id, data)) as (...args: unknown[]) => unknown,
    );

    this.ipcMain.handle(
      "crab:term-resize",
      ((_evt: unknown, id: string, cols: number, rows: number) =>
        this.handleResize(id, cols, rows)) as (...args: unknown[]) => unknown,
    );

    this.ipcMain.handle(
      "crab:term-kill",
      ((_evt: unknown, id: string) =>
        this.handleKill(id)) as (...args: unknown[]) => unknown,
    );
  }

  /**
   * Resolve the session type for a given spawn payload.
   *
   * Rules (in priority order):
   * 1. Explicit `sessionType` from the caller is respected.
   * 2. All ordinary terminals are durable: an agent can be started later
   *    inside any shell, after the spawn payload has been resolved.
   */
  resolveSessionType(
    payload: TermSpawnPayload,
    mode: ModeResolution,
  ): SessionType {
    if (payload.sessionType !== undefined) {
      return payload.sessionType;
    }
    if (payload.agentMeta) return "daemon";
    return mode.configuredMode === "in-process" ? "in-process" : "daemon";
  }

  /**
   * Returns the current breakdown of active sessions by type.
   * Used by the diagnostics IPC to populate resolvedSessionTypeBreakdown.
   */
  getSessionTypeBreakdown(): { daemon: number; inProcess: number } {
    let daemon = 0;
    let inProcess = 0;
    for (const type of this.sessionTypeMap.values()) {
      if (type === "daemon") daemon++;
      else inProcess++;
    }
    return { daemon, inProcess };
  }

  /**
   * Register an externally-created session (e.g. from reattach) so that
   * subsequent write/resize/kill calls route to the correct backend.
   */
  registerSession(rendererId: string, type: SessionType): void {
    this.sessionTypeMap.set(rendererId, type);
  }

  /** Remove a renderer session from routing without killing the backend. */
  unregisterSession(rendererId: string): void {
    this.sessionTypeMap.delete(rendererId);
  }

  /** Remove all registered IPC handlers and clear internal state. */
  dispose(): void {
    if (!this.registered) return;

    this.ipcMain.removeHandler("crab:term-spawn");
    this.ipcMain.removeHandler("crab:term-write");
    this.ipcMain.removeHandler("crab:term-resize");
    this.ipcMain.removeHandler("crab:term-kill");

    this.sessionTypeMap.clear();
    this.registered = false;
  }

  // -------------------------------------------------------------------------
  // Private: spawn
  // -------------------------------------------------------------------------

  private async handleSpawn(
    payload: TermSpawnPayload,
  ): Promise<TermSpawnResult | TermSpawnError> {
    const mode = this.modeResolution.current();
    const resolved = this.resolveSessionType(payload, mode);

    if (resolved === "daemon") {
      if (!mode.daemonReady) {
        return { ok: false, error: "daemon-unavailable", message: "Terminal service is unavailable. Retry or explicitly open a local terminal." };
      }
      if (!this.circuitBreaker.shouldAttemptDaemon) {
        const snap = this.circuitBreaker.getSnapshot();
        return {
          ok: false, error: "daemon-circuit-open",
          message: "Terminal service is temporarily unavailable after repeated failures.",
          cooldownRemainingMs: snap.cooldownRemainingMs ?? undefined,
        };
      }
      // IDs outlive this Electron process. Counters would attach new tabs to
      // an old background PTY after relaunch, even in a different repository.
      const rendererId = `term-d-${randomUUID()}`;
      try {
        await this.daemonBridge.handleSpawn(payload, rendererId);
        this.sessionTypeMap.set(rendererId, "daemon");
        return { id: rendererId, resolvedSessionType: "daemon" };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { ok: false, error: "daemon-unavailable", message: `Cannot start terminal: ${message}` };
      }
    }

    // in-process path
    return this.spawnInProcess(payload);
  }

  /** Spawn an explicitly ephemeral terminal session. */
  private async spawnInProcess(
    payload: TermSpawnPayload,
  ): Promise<TermSpawnResult> {
    const rendererId = `term-l-${randomUUID()}`;
    try {
      await this.inProcessBridge.handleSpawn(payload, rendererId);
      this.sessionTypeMap.set(rendererId, "in-process");
      return {
        id: rendererId,
        resolvedSessionType: "in-process",
      };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      // Re-throw in-process failures — they are unexpected and should
      // surface as IPC errors rather than typed results.
      throw new Error(`In-process spawn failed: ${message}`);
    }
  }

  // -------------------------------------------------------------------------
  // Private: write / resize / kill
  // -------------------------------------------------------------------------

  private async handleWrite(id: string, data: string): Promise<void> {
    const type = this.sessionTypeMap.get(id);
    if (type === "daemon") {
      await this.daemonBridge.handleWrite(id, data);
    } else if (type === "in-process") {
      await this.inProcessBridge.handleWrite(id, data);
    }
    // Unknown ID — session may have already exited; silently ignore.
  }

  private async handleResize(
    id: string,
    cols: number,
    rows: number,
  ): Promise<void> {
    const type = this.sessionTypeMap.get(id);
    if (type === "daemon") {
      await this.daemonBridge.handleResize(id, cols, rows);
    } else if (type === "in-process") {
      await this.inProcessBridge.handleResize(id, cols, rows);
    }
  }

  private async handleKill(id: string): Promise<void> {
    const type = this.sessionTypeMap.get(id);
    if (type === "daemon") {
      await this.daemonBridge.handleKill(id);
    } else if (type === "in-process") {
      await this.inProcessBridge.handleKill(id);
    }
    // Clean up the routing entry regardless of which bridge handled it.
    this.sessionTypeMap.delete(id);
  }
}
