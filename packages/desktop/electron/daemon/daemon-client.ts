/**
 * DaemonClient — Electron main process interface to the PTY daemon.
 *
 * Manages dual-socket connection lifecycle (control + stream),
 * request/response correlation, connection generation guards,
 * and event forwarding to the renderer.
 */

import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

import {
PROTOCOL_VERSION,
NOTIFY_PREFIX,
encodeFrame,
FrameDecoder,
type HelloMessage,
type HelloResponse,
type DaemonRequest,
type DaemonResponse,
type DaemonEvent,type ColdRestorePayload
} from "./protocol";

import type {
AgentActivityState,
SessionInfo
} from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Timeout for individual RPC requests (ms). */
const REQUEST_TIMEOUT_MS = 30_000;

/** Timeout for socket connect attempts (ms). */
const CONNECT_TIMEOUT_MS = 5_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DaemonClientOptions {
  /** Directory where socket/token/pid files live. */
  socketDir: string;
  /** Protocol version for socket filename. */
  protocolVersion?: number;
  /** Path to the daemon entry script. */
  daemonEntryPath: string;
  /** Called when a session emits PTY data. */
  onSessionData: (sessionId: string, data: string) => void;
  /** Called when a session exits. */
  onSessionExit: (sessionId: string, code: number) => void;
  /** Called when an agent session changes state. */
  onAgentStateChange: (sessionId: string, state: AgentActivityState, previousState: AgentActivityState) => void;
  /** Called when the client disconnects from the daemon. */
  onDisconnected?: () => void;
  /** Called when a cold-restore-available event is received for a session. */
  onColdRestoreAvailable?: (sessionId: string, payload: ColdRestorePayload) => void;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

// ---------------------------------------------------------------------------
// DaemonClient
// ---------------------------------------------------------------------------

export class DaemonClient {
  private controlSocket: net.Socket | null = null;
  private streamSocket: net.Socket | null = null;
  private controlDecoder: FrameDecoder | null = null;
  private streamDecoder: FrameDecoder | null = null;
  private pendingRequests: Map<string, PendingRequest> = new Map();
  private sessionIdMap: Map<string, string> = new Map(); // renderer ID → daemon session ID
  private connectionGeneration = 0;
  private connectPromise: Promise<void> | null = null;
  private connected = false;
  private clientId: string = randomUUID();

  private readonly socketDir: string;
  private readonly protocolVersion: number;
  private readonly onSessionData: (sessionId: string, data: string) => void;
  private readonly onSessionExit: (sessionId: string, code: number) => void;
  private readonly onAgentStateChange: (sessionId: string, state: AgentActivityState, previousState: AgentActivityState) => void;
  private readonly onDisconnected?: () => void;
  private readonly onColdRestoreAvailable?: (sessionId: string, payload: ColdRestorePayload) => void;

  constructor(opts: DaemonClientOptions) {
    this.socketDir = opts.socketDir;
    this.protocolVersion = opts.protocolVersion ?? PROTOCOL_VERSION;
    this.onSessionData = opts.onSessionData;
    this.onSessionExit = opts.onSessionExit;
    this.onAgentStateChange = opts.onAgentStateChange;
    this.onDisconnected = opts.onDisconnected;
    this.onColdRestoreAvailable = opts.onColdRestoreAvailable;
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /** Whether the client is currently connected to the daemon. */
  get isConnected(): boolean {
    return this.connected;
  }

  /**
   * Ensure the client is connected to the daemon. Uses a connect-lock
   * to prevent concurrent connection attempts.
   */
  async ensureConnected(): Promise<void> {
    if (this.connected) return;

    if (this.connectPromise) {
      return this.connectPromise;
    }

    this.connectPromise = this.doConnect();
    try {
      await this.connectPromise;
    } finally {
      this.connectPromise = null;
    }
  }

  /**
   * Send an RPC request and wait for the correlated response.
   * Times out after 30 seconds.
   */
  async request<T>(type: string, payload?: unknown, timeoutMs?: number): Promise<T> {
    await this.ensureConnected();

    const id = randomUUID();
    const request: DaemonRequest = payload !== undefined
      ? { id, type, payload } as DaemonRequest
      : { id, type } as DaemonRequest;

    const timeout = timeoutMs ?? REQUEST_TIMEOUT_MS;

    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingRequests.delete(id);
        reject(new Error(`Request "${type}" timed out after ${timeout}ms`));
      }, timeout);

      this.pendingRequests.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        timer,
      });

      try {
        this.writeControl(request);
      } catch (err) {
        this.pendingRequests.delete(id);
        clearTimeout(timer);
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  /**
   * Send a fire-and-forget notification (no response expected).
   * Uses the NOTIFY_PREFIX on the request ID.
   */
  async notify(type: string, payload?: unknown): Promise<void> {
    await this.ensureConnected();

    const id = `${NOTIFY_PREFIX}${randomUUID()}`;
    const request: DaemonRequest = payload !== undefined
      ? { id, type, payload } as DaemonRequest
      : { id, type } as DaemonRequest;

    this.writeControl(request);
  }

  /**
   * Get all agent sessions from the daemon. Filters listSessions results
   * to only those with agentMeta, providing a convenient view for the
   * agent dashboard and orchestrator reattach.
   */
  async getAgentSessions(): Promise<SessionInfo[]> {
    const sessions = await this.request<SessionInfo[]>("listSessions");
    return sessions.filter((s) => s.agentMeta !== undefined);
  }

  /**
   * Disconnect from the daemon. Rejects all pending requests.
   */
  disconnect(): void {
    this.connected = false;
    this.connectionGeneration++;

    // Reject all pending requests
    for (const [, pending] of this.pendingRequests) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Client disconnected"));
    }
    this.pendingRequests.clear();

    // Destroy sockets
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

  // -------------------------------------------------------------------------
  // Session ID mapping
  // -------------------------------------------------------------------------

  /** Map a renderer-facing session ID to a daemon session ID. */
  mapSessionId(rendererId: string, daemonId: string): void {
    this.sessionIdMap.set(rendererId, daemonId);
  }

  /** Get the daemon session ID for a renderer-facing ID. */
  getDaemonSessionId(rendererId: string): string | undefined {
    return this.sessionIdMap.get(rendererId);
  }

  /** Remove a session ID mapping. */
  unmapSessionId(rendererId: string): void {
    this.sessionIdMap.delete(rendererId);
  }

  // -------------------------------------------------------------------------
  // Private: Connection
  // -------------------------------------------------------------------------

  private get socketPath(): string {
    return path.join(this.socketDir, `daemon-v${this.protocolVersion}.sock`);
  }

  private get tokenPath(): string {
    return path.join(this.socketDir, `daemon-v${this.protocolVersion}.token`);
  }

  private readToken(): string | null {
    try {
      return fs.readFileSync(this.tokenPath, "utf-8").trim();
    } catch {
      return null;
    }
  }

  private async doConnect(): Promise<void> {
    const generation = ++this.connectionGeneration;
    const token = this.readToken();

    if (!token) {
      throw new Error("No daemon token file found — is the daemon running?");
    }

    // Connect control socket
    this.controlSocket = await this.connectSocket(this.socketPath);
    this.controlDecoder = new FrameDecoder();

    // Verify generation hasn't changed (disconnect called during connect)
    if (this.connectionGeneration !== generation) {
      this.controlSocket.destroy();
      this.controlSocket = null;
      this.controlDecoder = null;
      throw new Error("Connection aborted: generation changed");
    }

    // Hello handshake on control socket
    await this.performHello(this.controlSocket, this.controlDecoder, token, "control");

    // Connect stream socket
    this.streamSocket = await this.connectSocket(this.socketPath);
    this.streamDecoder = new FrameDecoder();

    if (this.connectionGeneration !== generation) {
      this.controlSocket.destroy();
      this.streamSocket.destroy();
      this.controlSocket = null;
      this.streamSocket = null;
      this.controlDecoder = null;
      this.streamDecoder = null;
      throw new Error("Connection aborted: generation changed");
    }

    // Hello handshake on stream socket
    await this.performHello(this.streamSocket, this.streamDecoder, token, "stream");

    // Wire up data handlers
    this.wireControlSocket(generation);
    this.wireStreamSocket(generation);

    this.connected = true;
  }

  private connectSocket(socketPath: string): Promise<net.Socket> {
    return new Promise<net.Socket>((resolve, reject) => {
      const socket = net.createConnection(socketPath);

      const timer = setTimeout(() => {
        socket.destroy();
        reject(new Error(`Socket connect timed out after ${CONNECT_TIMEOUT_MS}ms`));
      }, CONNECT_TIMEOUT_MS);

      socket.on("connect", () => {
        clearTimeout(timer);
        resolve(socket);
      });

      socket.on("error", (err) => {
        clearTimeout(timer);
        reject(new Error(`Socket connect failed: ${err.message}`));
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
        version: this.protocolVersion,
        token,
        clientId: this.clientId,
        role,
      };

      const timer = setTimeout(() => {
        reject(new Error(`Hello handshake timed out for ${role} socket`));
      }, CONNECT_TIMEOUT_MS);

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
            reject(new Error(`Hello rejected on ${role} socket: ${resp.error ?? "unknown"}`));
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

  private wireControlSocket(generation: number): void {
    if (!this.controlSocket || !this.controlDecoder) return;

    const socket = this.controlSocket;
    const decoder = this.controlDecoder;

    socket.on("data", (chunk: Buffer) => {
      if (this.connectionGeneration !== generation) return;

      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        const msg = frame.json as DaemonResponse;
        this.handleControlResponse(msg);
      }
    });

    socket.on("close", () => {
      if (this.connectionGeneration !== generation) return;
      this.handleDisconnect();
    });

    socket.on("error", () => {
      if (this.connectionGeneration !== generation) return;
      this.handleDisconnect();
    });
  }

  private wireStreamSocket(generation: number): void {
    if (!this.streamSocket || !this.streamDecoder) return;

    const socket = this.streamSocket;
    const decoder = this.streamDecoder;

    socket.on("data", (chunk: Buffer) => {
      if (this.connectionGeneration !== generation) return;

      decoder.push(chunk);
      const frames = decoder.frames();

      for (const frame of frames) {
        this.handleStreamEvent(frame.json as DaemonEvent, frame.payload);
      }
    });

    socket.on("close", () => {
      if (this.connectionGeneration !== generation) return;
      this.handleDisconnect();
    });

    socket.on("error", () => {
      if (this.connectionGeneration !== generation) return;
      this.handleDisconnect();
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
        break;
      }
      case "state-changed": {
        this.onAgentStateChange(
          event.sessionId,
          event.payload.state,
          event.payload.previousState,
        );
        break;
      }
      case "cold-restore-available": {
        this.onColdRestoreAvailable?.(event.sessionId, event.payload);
        break;
      }
    }
  }

  private handleDisconnect(): void {
    if (!this.connected) return;

    this.connected = false;

    // Reject all pending requests
    for (const [, pending] of this.pendingRequests) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Daemon connection lost"));
    }
    this.pendingRequests.clear();

    this.controlSocket = null;
    this.streamSocket = null;
    this.controlDecoder = null;
    this.streamDecoder = null;

    this.onDisconnected?.();
  }

  // -------------------------------------------------------------------------
  // Private: Write helpers
  // -------------------------------------------------------------------------

  private writeControl(msg: DaemonRequest): void {
    if (!this.controlSocket || this.controlSocket.destroyed) {
      throw new Error("Control socket not connected");
    }
    this.controlSocket.write(encodeFrame(msg));
  }
}
