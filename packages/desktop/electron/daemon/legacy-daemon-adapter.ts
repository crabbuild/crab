/**
 * Legacy Daemon Adapter — routes operations to an old-protocol-version daemon.
 *
 * When the DaemonClient detects a running daemon from a previous protocol
 * version, it creates a LegacyDaemonAdapter. The adapter:
 *
 * 1. Connects to the old daemon using its protocol version
 * 2. Routes operations for existing sessions to the old daemon
 * 3. Directs new session creation to the new daemon
 * 4. Monitors legacy session count; shuts down the old daemon when zero
 * 5. Handles legacy daemon death gracefully (synthesizes exit events)
 *
 * Socket filenames include the protocol version (daemon-v<N>.sock), so
 * multiple versions coexist on disk without conflict.
 */

import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import { socketAddress } from "./socket-address";

import {
  encodeFrame,
  FrameDecoder,
  type HelloMessage,
  type HelloResponse,
  type DaemonRequest,
  type DaemonResponse,
  type DaemonEvent,
} from "./protocol";

import type { SessionInfo } from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Timeout for connecting to the legacy daemon socket (ms). */
const LEGACY_CONNECT_TIMEOUT_MS = 5000;

/** Timeout for RPC requests to the legacy daemon (ms). */
const LEGACY_REQUEST_TIMEOUT_MS = 30_000;

/** Interval for polling legacy session count (ms). */
const SESSION_POLL_INTERVAL_MS = 10_000;

/** Maximum time to keep a legacy daemon alive with no sessions (ms). 7 days. */
const MAX_LEGACY_LIFETIME_MS = 7 * 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface LegacyDaemonAdapterOptions {
  /** Directory where socket/token/pid files live. */
  socketDir: string;
  /** Protocol version of the legacy daemon. */
  legacyVersion: number;
  /** Called when a legacy session emits PTY data. */
  onSessionData: (sessionId: string, data: string) => void;
  /** Called when a legacy session exits. */
  onSessionExit: (sessionId: string, code: number) => void;
  /** Called when the legacy daemon has been fully shut down. */
  onShutdownComplete?: () => void;
  /** Logger for adapter events. */
  log?: (level: "info" | "warn" | "error", message: string) => void;
  /** Clock function for testability. */
  now?: () => number;
  /** Override poll interval for testing. */
  pollIntervalMs?: number;
  /** Override max lifetime for testing. */
  maxLifetimeMs?: number;
}

export type LegacyAdapterState = "connecting" | "active" | "draining" | "shutdown";

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

// ---------------------------------------------------------------------------
// LegacyDaemonAdapter
// ---------------------------------------------------------------------------

export class LegacyDaemonAdapter {
  private controlSocket: net.Socket | null = null;
  private streamSocket: net.Socket | null = null;
  private controlDecoder: FrameDecoder | null = null;
  private streamDecoder: FrameDecoder | null = null;
  private pendingRequests: Map<string, PendingRequest> = new Map();
  private legacySessionIds: Set<string> = new Set();
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private _state: LegacyAdapterState = "connecting";
  private createdAt: number;
  private requestCounter = 0;

  private readonly socketDir: string;
  private readonly legacyVersion: number;
  private readonly onSessionData: (sessionId: string, data: string) => void;
  private readonly onSessionExit: (sessionId: string, code: number) => void;
  private readonly onShutdownComplete?: () => void;
  private readonly log: (level: "info" | "warn" | "error", message: string) => void;
  private readonly now: () => number;
  private readonly pollIntervalMs: number;
  private readonly maxLifetimeMs: number;

  constructor(opts: LegacyDaemonAdapterOptions) {
    this.socketDir = opts.socketDir;
    this.legacyVersion = opts.legacyVersion;
    this.onSessionData = opts.onSessionData;
    this.onSessionExit = opts.onSessionExit;
    this.onShutdownComplete = opts.onShutdownComplete;
    this.log = opts.log ?? (() => {});
    this.now = opts.now ?? (() => Date.now());
    this.pollIntervalMs = opts.pollIntervalMs ?? SESSION_POLL_INTERVAL_MS;
    this.maxLifetimeMs = opts.maxLifetimeMs ?? MAX_LEGACY_LIFETIME_MS;
    this.createdAt = this.now();
  }

  /** Current adapter state. */
  get state(): LegacyAdapterState {
    return this._state;
  }

  /** Protocol version of the legacy daemon this adapter connects to. */
  get protocolVersion(): number {
    return this.legacyVersion;
  }

  /** Set of session IDs currently managed by the legacy daemon. */
  get sessionIds(): ReadonlySet<string> {
    return this.legacySessionIds;
  }

  /** Number of active sessions on the legacy daemon. */
  get sessionCount(): number {
    return this.legacySessionIds.size;
  }

  /** Socket path for the legacy daemon. */
  get socketPath(): string {
    return path.join(this.socketDir, `daemon-v${this.legacyVersion}.sock`);
  }

  /** Token path for the legacy daemon. */
  get tokenPath(): string {
    return path.join(this.socketDir, `daemon-v${this.legacyVersion}.token`);
  }

  /** PID file path for the legacy daemon. */
  get pidPath(): string {
    return path.join(this.socketDir, `daemon-v${this.legacyVersion}.pid`);
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /**
   * Connect to the legacy daemon and discover its active sessions.
   * After connecting, starts polling for session count changes.
   */
  async connect(): Promise<void> {
    this._state = "connecting";
    const token = this.readToken();

    if (!token) {
      throw new Error(`No token file found for legacy daemon v${this.legacyVersion}`);
    }

    // Connect control socket
    this.controlSocket = await this.connectSocket(this.socketPath);
    this.controlDecoder = new FrameDecoder();

    // Hello handshake on control socket
    await this.performHello(this.controlSocket, this.controlDecoder, token, "control");

    // Connect stream socket
    this.streamSocket = await this.connectSocket(this.socketPath);
    this.streamDecoder = new FrameDecoder();

    // Hello handshake on stream socket
    await this.performHello(this.streamSocket, this.streamDecoder, token, "stream");

    // Wire up data handlers
    this.wireControlSocket();
    this.wireStreamSocket();

    // Discover existing sessions
    const sessions = await this.listSessions();
    for (const session of sessions) {
      if (session.isAlive) {
        this.legacySessionIds.add(session.sessionId);
      }
    }

    this._state = this.legacySessionIds.size > 0 ? "active" : "draining";
    this.log("info", `Legacy adapter connected to daemon v${this.legacyVersion} with ${this.legacySessionIds.size} sessions`);

    // Start polling for session count
    this.startPolling();

    // If no sessions from the start, initiate shutdown
    if (this.legacySessionIds.size === 0) {
      await this.initiateShutdown();
    }
  }

  /**
   * Check if a session ID belongs to this legacy adapter.
   */
  ownsSession(sessionId: string): boolean {
    return this.legacySessionIds.has(sessionId);
  }

  /**
   * Forward a write operation to the legacy daemon.
   */
  async write(sessionId: string, data: string): Promise<void> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    await this.request("write", { sessionId, data });
  }

  /**
   * Forward a resize operation to the legacy daemon.
   */
  async resize(sessionId: string, cols: number, rows: number): Promise<void> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    await this.request("resize", { sessionId, cols, rows });
  }

  /**
   * Forward a kill operation to the legacy daemon.
   */
  async kill(sessionId: string): Promise<void> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    await this.request("kill", { sessionId });
  }

  /**
   * Forward a signal operation to the legacy daemon.
   */
  async signal(sessionId: string, sig: string): Promise<void> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    await this.request("signal", { sessionId, signal: sig });
  }

  /**
   * Get the snapshot for a legacy session.
   */
  async getSnapshot(sessionId: string): Promise<unknown> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    return this.request("getSnapshot", { sessionId });
  }

  /**
   * Get the CWD for a legacy session.
   */
  async getCwd(sessionId: string): Promise<string | null> {
    if (!this.ownsSession(sessionId)) {
      throw new Error(`Session ${sessionId} not owned by legacy adapter`);
    }
    return this.request<string | null>("getCwd", { sessionId });
  }

  /**
   * List all sessions on the legacy daemon.
   */
  async listSessions(): Promise<SessionInfo[]> {
    return this.request<SessionInfo[]>("listSessions");
  }

  /**
   * Dispose the adapter: stop polling, disconnect sockets, clean up.
   * Does NOT shut down the legacy daemon (use initiateShutdown for that).
   */
  dispose(): void {
    this.stopPolling();
    this.disconnectSockets();
    this._state = "shutdown";
  }

  // -------------------------------------------------------------------------
  // Private: Connection
  // -------------------------------------------------------------------------

  private readToken(): string | null {
    try {
      return fs.readFileSync(this.tokenPath, "utf-8").trim();
    } catch {
      return null;
    }
  }

  private connectSocket(socketPath: string): Promise<net.Socket> {
    const address = socketAddress(socketPath);
    return new Promise<net.Socket>((resolve, reject) => {
      const socket = net.createConnection(address);

      const timer = setTimeout(() => {
        socket.destroy();
        reject(new Error(`Legacy daemon connect timed out after ${LEGACY_CONNECT_TIMEOUT_MS}ms`));
      }, LEGACY_CONNECT_TIMEOUT_MS);

      socket.on("connect", () => {
        clearTimeout(timer);
        resolve(socket);
      });

      socket.on("error", (err) => {
        clearTimeout(timer);
        reject(new Error(`Legacy daemon connect failed: ${err.message}`));
      });
    });
  }

  private performHello(
    socket: net.Socket,
    decoder: FrameDecoder,
    token: string,
    role: "control" | "stream",
  ): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const hello: HelloMessage = {
        type: "hello",
        version: this.legacyVersion,
        token,
        clientId: `legacy-adapter-v${this.legacyVersion}`,
        role,
      };

      const timer = setTimeout(() => {
        reject(new Error(`Legacy hello handshake timed out for ${role} socket`));
      }, LEGACY_CONNECT_TIMEOUT_MS);

      const onData = (chunk: Buffer) => {
        decoder.push(chunk);
        const frames = decoder.frames();

        for (const frame of frames) {
          const resp = frame.json as HelloResponse;
          clearTimeout(timer);
          socket.removeListener("data", onData);

          if (resp.ok) {
            resolve();
          } else {
            reject(new Error(`Legacy hello rejected on ${role} socket: ${resp.error ?? "unknown"}`));
          }
          return;
        }
      };

      socket.on("data", onData);
      socket.write(encodeFrame(hello));
    });
  }

  // -------------------------------------------------------------------------
  // Private: Socket wiring
  // -------------------------------------------------------------------------

  private wireControlSocket(): void {
    if (!this.controlSocket || !this.controlDecoder) return;

    const socket = this.controlSocket;
    const decoder = this.controlDecoder;

    socket.on("data", (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        const msg = frame.json as DaemonResponse;
        this.handleControlResponse(msg);
      }
    });

    socket.on("close", () => {
      this.handleLegacyDaemonDeath();
    });

    socket.on("error", () => {
      this.handleLegacyDaemonDeath();
    });
  }

  private wireStreamSocket(): void {
    if (!this.streamSocket || !this.streamDecoder) return;

    const socket = this.streamSocket;
    const decoder = this.streamDecoder;

    socket.on("data", (chunk: Buffer) => {
      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        this.handleStreamEvent(frame.json as DaemonEvent, frame.payload);
      }
    });

    socket.on("close", () => {
      this.handleLegacyDaemonDeath();
    });

    socket.on("error", () => {
      this.handleLegacyDaemonDeath();
    });
  }

  // -------------------------------------------------------------------------
  // Private: Message handling
  // -------------------------------------------------------------------------

  private handleControlResponse(msg: DaemonResponse): void {
    const pending = this.pendingRequests.get(msg.id);
    if (!pending) return;

    this.pendingRequests.delete(msg.id);
    clearTimeout(pending.timer);

    if (msg.ok) {
      pending.resolve(msg.payload);
    } else {
      pending.reject(new Error(msg.error));
    }
  }

  private handleStreamEvent(event: DaemonEvent, payload: Buffer | null): void {
    switch (event.event) {
      case "data": {
        const data = payload ? payload.toString("utf-8") : "";
        this.onSessionData(event.sessionId, data);
        break;
      }
      case "exit": {
        this.onSessionExit(event.sessionId, event.payload.code);
        this.legacySessionIds.delete(event.sessionId);
        this.log("info", `Legacy session ${event.sessionId} exited (code ${event.payload.code}), ${this.legacySessionIds.size} remaining`);
        this.checkDrainComplete();
        break;
      }
    }
  }

  // -------------------------------------------------------------------------
  // Private: Legacy daemon death handling
  // -------------------------------------------------------------------------

  /**
   * Handle unexpected death of the legacy daemon.
   * Synthesizes exit events for all remaining legacy sessions.
   */
  private handleLegacyDaemonDeath(): void {
    if (this._state === "shutdown") return;

    this.log("warn", `Legacy daemon v${this.legacyVersion} died unexpectedly with ${this.legacySessionIds.size} sessions`);

    // Synthesize exit events for all remaining sessions
    for (const sessionId of this.legacySessionIds) {
      this.onSessionExit(sessionId, -1);
    }
    this.legacySessionIds.clear();

    // Clean up
    this.stopPolling();
    this.disconnectSockets();
    this.cleanupFiles();
    this._state = "shutdown";
    this.onShutdownComplete?.();
  }

  // -------------------------------------------------------------------------
  // Private: Session polling and drain
  // -------------------------------------------------------------------------

  private startPolling(): void {
    this.pollTimer = setInterval(() => {
      this.pollSessions();
    }, this.pollIntervalMs);
  }

  private stopPolling(): void {
    if (this.pollTimer !== null) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  private async pollSessions(): Promise<void> {
    if (this._state === "shutdown") return;

    // Check max lifetime
    const elapsed = this.now() - this.createdAt;
    if (elapsed >= this.maxLifetimeMs) {
      this.log("warn", `Legacy daemon v${this.legacyVersion} exceeded max lifetime (${this.maxLifetimeMs}ms), forcing shutdown`);
      await this.forceShutdown();
      return;
    }

    try {
      const sessions = await this.listSessions();
      const aliveSessions = sessions.filter((s) => s.isAlive);

      // Update our tracked set
      this.legacySessionIds.clear();
      for (const session of aliveSessions) {
        this.legacySessionIds.add(session.sessionId);
      }

      if (this.legacySessionIds.size === 0) {
        this._state = "draining";
        await this.initiateShutdown();
      }
    } catch {
      // If we can't reach the legacy daemon, it may have died
      this.handleLegacyDaemonDeath();
    }
  }

  private checkDrainComplete(): void {
    if (this.legacySessionIds.size === 0 && this._state !== "shutdown") {
      this._state = "draining";
      this.initiateShutdown().catch((err) => {
        this.log("error", `Failed to shut down legacy daemon: ${(err as Error).message}`);
      });
    }
  }

  // -------------------------------------------------------------------------
  // Private: Shutdown
  // -------------------------------------------------------------------------

  /**
   * Gracefully shut down the legacy daemon (all sessions have exited).
   */
  private async initiateShutdown(): Promise<void> {
    this.log("info", `All legacy sessions exited, shutting down daemon v${this.legacyVersion}`);
    this.stopPolling();

    try {
      await this.request("shutdown", { killSessions: false });
    } catch {
      // Daemon may already be gone — that's fine
    }

    this.disconnectSockets();
    this.cleanupFiles();
    this._state = "shutdown";
    this.onShutdownComplete?.();
  }

  /**
   * Force shutdown the legacy daemon (max lifetime exceeded).
   */
  private async forceShutdown(): Promise<void> {
    this.log("warn", `Force-shutting down legacy daemon v${this.legacyVersion}`);
    this.stopPolling();

    // Synthesize exit events for remaining sessions
    for (const sessionId of this.legacySessionIds) {
      this.onSessionExit(sessionId, -1);
    }
    this.legacySessionIds.clear();

    try {
      await this.request("shutdown", { killSessions: true });
    } catch {
      // Daemon may already be gone
    }

    this.disconnectSockets();
    this.cleanupFiles();
    this._state = "shutdown";
    this.onShutdownComplete?.();
  }

  // -------------------------------------------------------------------------
  // Private: Request/Response
  // -------------------------------------------------------------------------

  private request<T>(type: string, payload?: unknown): Promise<T> {
    if (!this.controlSocket || this.controlSocket.destroyed) {
      return Promise.reject(new Error("Legacy daemon control socket not connected"));
    }

    const id = `legacy-${++this.requestCounter}`;
    const request: DaemonRequest = payload !== undefined
      ? { id, type, payload } as DaemonRequest
      : { id, type } as DaemonRequest;

    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingRequests.delete(id);
        reject(new Error(`Legacy request "${type}" timed out after ${LEGACY_REQUEST_TIMEOUT_MS}ms`));
      }, LEGACY_REQUEST_TIMEOUT_MS);

      this.pendingRequests.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        timer,
      });

      try {
        this.controlSocket!.write(encodeFrame(request));
      } catch (err) {
        this.pendingRequests.delete(id);
        clearTimeout(timer);
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  // -------------------------------------------------------------------------
  // Private: Cleanup
  // -------------------------------------------------------------------------

  private disconnectSockets(): void {
    // Reject all pending requests
    for (const [, pending] of this.pendingRequests) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Legacy adapter disconnected"));
    }
    this.pendingRequests.clear();

    if (this.controlSocket) {
      this.controlSocket.removeAllListeners();
      this.controlSocket.destroy();
      this.controlSocket = null;
    }
    if (this.streamSocket) {
      this.streamSocket.removeAllListeners();
      this.streamSocket.destroy();
      this.streamSocket = null;
    }

    this.controlDecoder = null;
    this.streamDecoder = null;
  }

  private cleanupFiles(): void {
    safeUnlink(this.socketPath);
    safeUnlink(this.tokenPath);
    safeUnlink(this.pidPath);
    this.log("info", `Cleaned up legacy daemon v${this.legacyVersion} files`);
  }
}

// ---------------------------------------------------------------------------
// Detection helper
// ---------------------------------------------------------------------------

/**
 * Detect legacy daemons by scanning the socket directory for daemon-v<N>.sock
 * files where N < currentVersion.
 *
 * Returns the protocol versions of any detected legacy daemons.
 */
export function detectLegacyDaemons(
  socketDir: string,
  currentVersion: number,
): number[] {
  try {
    const files = fs.readdirSync(socketDir);
    const versions: number[] = [];

    for (const file of files) {
      const match = file.match(/^daemon-v(\d+)\.sock$/);
      if (match) {
        const version = parseInt(match[1], 10);
        if (version < currentVersion) {
          versions.push(version);
        }
      }
    }

    return versions.sort((a, b) => a - b);
  } catch {
    // Directory may not exist yet
    return [];
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function safeUnlink(filePath: string): void {
  try {
    fs.unlinkSync(filePath);
  } catch {
    // File may not exist — that's fine
  }
}

// ---------------------------------------------------------------------------
// Exports for testing
// ---------------------------------------------------------------------------

export {
  LEGACY_CONNECT_TIMEOUT_MS,
  LEGACY_REQUEST_TIMEOUT_MS,
  SESSION_POLL_INTERVAL_MS,
  MAX_LEGACY_LIFETIME_MS,
};
