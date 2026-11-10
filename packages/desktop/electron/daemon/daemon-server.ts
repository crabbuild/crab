/**
 * DaemonServer — local IPC server, auth, and request routing.
 *
 * Responsibilities:
 * - Unix socket or Windows named pipe
 * - Token-based auth (UUID written to file)
 * - Hello handshake with version + token verification
 * - Dual-socket support (control + stream per client)
 * - Request routing to TerminalHost
 * - StreamDataBatcher (8ms timer coalescing) with explicit flush before exit
 * - Outbound buffer cap (8 MB, destroy socket on exceed)
 * - Fire-and-forget notifications (notify_ prefix)
 * - Extended ping response with health metadata
 * - shutdown with killSessions flag
 * - Idle timer (24h self-terminate, reset on client connect or session create)
 * - Client disconnect as implicit detach for all sessions
 * - Multi-client fan-out: broadcast output to all stream sockets
 * - Backpressure: pause PTY read when socket writableLength exceeds threshold
 */

import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as crypto from "node:crypto";
import { socketAddress } from "./socket-address";

import { TerminalHost } from "./terminal-host";
import type { SubprocessSpawner } from "./subprocess-handle";
import type { AgentActivityState,StreamClient } from "./types";
import type { AdoptedSession } from "./fd-handoff";
import {
PROTOCOL_VERSION,
snapshotReplayData,
NOTIFY_PREFIX,
encodeFrame,
FrameDecoder,
type DaemonRequest,
type DaemonResponse,
type DaemonEvent,
type HelloMessage,
type HelloResponse,
type PingResponse,type ColdRestorePayload
} from "./protocol";
import { StreamDataBatcher,type BatchEntry } from "./stream-data-batcher";
import { AgentTracker } from "./agent-tracker";
import { prepareUpgrade } from "./prepare-upgrade";
import { DaemonMetrics } from "./daemon-metrics";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Outbound buffer cap: 8 MB. Destroy socket if exceeded. */
const OUTBOUND_BUFFER_CAP = 8 * 1024 * 1024;

/** Backpressure threshold: pause PTY when socket writableLength exceeds this. */
const BACKPRESSURE_THRESHOLD = 4 * 1024 * 1024;

/** Default idle timeout: 24 hours. */
const DEFAULT_IDLE_TIMEOUT_MS = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/**
 * Lifecycle hooks injected into DaemonServer by daemon-main.ts.
 * Keeps the server testable and the durability modules orthogonal.
 */
export interface DaemonServerHooks {
  /** Called before a new session is spawned. Throw to refuse the spawn. */
  beforeSpawn?: () => void;
  /** Called after a new session is created (isNew === true). */
  onSessionCreated?: (id: string, opts: import("./terminal-host.js").CreateOrAttachOptions) => void;
  /** Called after an existing session is reattached (isNew === false). */
  onSessionAttached?: (id: string) => void;
  /** Called when a client disconnects and implicitly detaches from a session. */
  onSessionDetached?: (id: string) => void;
  /** Called when a session is explicitly killed via the kill request. */
  onSessionKilled?: (id: string) => void;
  /** Called when a session exits. */
  onSessionExit?: (id: string, code: number) => void;
  /** Called on each data chunk sent to stream clients — byte length only, not the data. */
  onSessionData?: (id: string, byteLength: number) => void;
  /** Called when an agent session's activity state changes. */
  onAgentState?: (id: string, state: AgentActivityState, prev: AgentActivityState) => void;
  /** Called when a checkpoint request is received for a session. */
  onCheckpointRequested?: (id: string) => void;
  /**
   * Called when a cold-restore-resume request arrives. Implementations
   * should call historyManager.openSession to overwrite the stale meta
   * with a fresh createdAt and cleared endedAt.
   */
  onColdRestoreResume?: (
    id: string,
    opts: import("./terminal-host.js").CreateOrAttachOptions,
  ) => void;
  /**
   * Called when a cold-restore-discard request arrives. Implementations
   * should call historyManager.removeSession to clean up the history dir.
   */
  onColdRestoreDiscard?: (id: string) => void;
  /**
   * Returns the sanitized env that was persisted for a cold-restore session.
   * Called during cold-restore-resume to merge with user-provided env.
   * Returns undefined if the session has no persisted env.
   */
  getPersistedEnv?: (id: string) => Record<string, string> | undefined;
}

export interface DaemonServerOptions {
  socketPath: string;
  tokenPath: string;
  pidPath: string;
  idleTimeoutMs?: number;
  maxSessions?: number;
  spawnSubprocess: SubprocessSpawner;
  /** Lifecycle hooks for durability subsystems (history, audit, quotas, etc.). */
  hooks?: DaemonServerHooks;
  /** Called when the idle timer fires (self-terminate). */
  onIdleShutdown?: () => void;
}

interface ConnectedClient {
  clientId: string;
  controlSocket: net.Socket | null;
  streamSocket: net.Socket | null;
  controlDecoder: FrameDecoder | null;
  streamDecoder: FrameDecoder | null;
  /** Session IDs this client is attached to, with their attach tokens. */
  attachedSessions: Map<string, symbol>;
}

// ---------------------------------------------------------------------------
// DaemonServer
// ---------------------------------------------------------------------------

export class DaemonServer {
  private server: net.Server | null = null;
  private host: TerminalHost;
  private agentTracker: AgentTracker;
  private clients: Map<string, ConnectedClient> = new Map();
  private token: string = "";
  private batcher: StreamDataBatcher;
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private idleTimeoutMs: number;
  private startedAt: number = Date.now();
  private disposed = false;
  private metrics: DaemonMetrics;
  private hooks: DaemonServerHooks;

  private socketPath: string;
  private tokenPath: string;
  private pidPath: string;
  private onIdleShutdown?: () => void;

  /** Tracks which sessions have backpressure active (PTY paused). */
  private backpressuredSessions: Set<string> = new Set();

  // Fields populated by daemon-main.ts for getDaemonInfo()
  private _auditLogPath: string | null = null;
  private _quotasPath: string | null = null;
  private _historyBasePath: string | null = null;
  private _pathMismatch: boolean = false;
  private _previousEntryPath: string | null = null;
  private _memoryPressure: "ok" | "warn" | "critical" = "ok";
  private _highThroughputSessionCount: number = 0;

  /**
   * Pending cold-restore sessions: sessionId → the ColdRestorePayload that
   * was broadcast to clients. Populated by broadcastToAllClients when a
   * cold-restore-available event is sent; consumed by cold-restore-resume
   * and cold-restore-discard handlers.
   */
  private pendingColdRestores: Map<string, ColdRestorePayload> = new Map();

  constructor(opts: DaemonServerOptions) {
    this.socketPath = opts.socketPath;
    this.tokenPath = opts.tokenPath;
    this.pidPath = opts.pidPath;
    this.idleTimeoutMs = opts.idleTimeoutMs ?? DEFAULT_IDLE_TIMEOUT_MS;
    this.onIdleShutdown = opts.onIdleShutdown;
    this.hooks = opts.hooks ?? {};

    this.host = new TerminalHost({
      spawnSubprocess: opts.spawnSubprocess,
      maxSessions: opts.maxSessions,
      onSessionExit: (sessionId, exitCode) => this.handleSessionExit(sessionId, exitCode),
    });

    this.agentTracker = new AgentTracker({
      onTimeout: (sessionId) => {
        try {
          this.host.kill(sessionId);
        } catch {
          // Session may have already exited
        }
      },
    });

    // Wire AgentTracker state changes to broadcast state-changed events
    this.agentTracker.onStateChange = (sessionId, state, previousState) => {
      this.hooks.onAgentState?.(sessionId, state, previousState);
      this.broadcastStateChanged(sessionId, state, previousState);
    };

    this.batcher = new StreamDataBatcher({
      onFlush: (clientId, entries) => this.handleBatchFlush(clientId, entries),
    });

    this.metrics = new DaemonMetrics({
      getActiveSessionCount: () => this.host.sessionCount,
    });

    this.resetIdleTimer();
  }

  // -------------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------------

  /**
   * Start the server: generate token, bind socket, write PID file.
   */
  async start(): Promise<void> {
    const address = socketAddress(this.socketPath);
    if (process.platform !== "win32" && address !== this.socketPath) {
      // A short endpoint lives outside the home directory. Require a private,
      // owned directory so another user cannot replace its socket pathname.
      const directory = path.dirname(address);
      fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
      const stat = fs.lstatSync(directory);
      if (!stat.isDirectory() || stat.uid !== process.getuid!() || (stat.mode & 0o777) !== 0o700) {
        throw new Error(`Daemon socket directory is not private and owned: ${directory}`);
      }
    }
    // Generate auth token
    this.token = crypto.randomUUID();

    // Write token file with 0600 permissions
    fs.writeFileSync(this.tokenPath, this.token, { mode: 0o600 });

    // Write PID file
    const pidData = JSON.stringify({
      pid: process.pid,
      startedAtMs: this.startedAt,
      entryPath: process.argv[1] ?? "",
    });
    fs.writeFileSync(this.pidPath, pidData, { mode: 0o600 });

    // Tokens and PID metadata retain their home-scoped control-file identity.
    this.server = net.createServer((socket) => this.handleConnection(socket));

    await new Promise<void>((resolve, reject) => {
      this.server!.on("error", reject);
      this.server!.listen(address, () => {
        // Set socket file permissions to 0600
        try {
          fs.chmodSync(address, 0o600);
        } catch {
          // May fail on some systems, non-fatal
        }
        resolve();
      });
    });
  }

  /**
   * Graceful shutdown: close server, disconnect clients, dispose host.
   */
  async shutdown(opts: { killSessions: boolean } = { killSessions: false }): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;

    this.clearIdleTimer();
    this.batcher.flushAll();
    this.batcher.dispose();
    this.agentTracker.dispose();
    this.metrics.dispose();

    // Close all client connections
    for (const client of this.clients.values()) {
      client.controlSocket?.destroy();
      client.streamSocket?.destroy();
    }
    this.clients.clear();

    // Dispose host (kills all sessions if requested, otherwise just releases)
    if (opts.killSessions) {
      this.host.dispose();
    }

    // Close server
    if (this.server) {
      await new Promise<void>((resolve) => {
        this.server!.close(() => resolve());
      });
      this.server = null;
    }

    // Clean up files
    this.cleanupFiles();
  }

  /**
   * Get the auth token (for testing).
   */
  get authToken(): string {
    return this.token;
  }

  /**
   * Number of connected clients.
   */
  get clientCount(): number {
    return this.clients.size;
  }

  /**
   * Number of active sessions.
   */
  get sessionCount(): number {
    return this.host.sessionCount;
  }

  /**
   * Access to the TerminalHost (for testing).
   */
  get terminalHost(): TerminalHost {
    return this.host;
  }

  /**
   * Adopt a list of sessions from a handoff snapshot.
   *
   * Loops over the adopted sessions, calls TerminalHost.adoptSession for each,
   * and re-registers agentMeta with AgentTracker when present so state
   * tracking resumes seamlessly after the upgrade.
   */
  adoptSessions(list: AdoptedSession[]): void {
    for (const adopted of list) {
      this.host.adoptSession(adopted);

      if (adopted.meta.agentMeta) {
        this.agentTracker.registerSession(adopted.id, adopted.meta.agentMeta);
      }
    }
  }

  /**
   * Access to the AgentTracker (for testing).
   */
  get tracker(): AgentTracker {
    return this.agentTracker;
  }

  /**
   * Access to the DaemonMetrics instance.
   */
  get daemonMetrics(): DaemonMetrics {
    return this.metrics;
  }

  /**
   * Returns diagnostic info populated by daemon-main.ts.
   */
  getDaemonInfo(): {
    auditLogPath: string | null;
    quotasPath: string | null;
    historyBasePath: string | null;
    pathMismatch: boolean;
    previousEntryPath: string | null;
    memoryPressure: "ok" | "warn" | "critical";
    highThroughputSessionCount: number;
  } {
    return {
      auditLogPath: this._auditLogPath,
      quotasPath: this._quotasPath,
      historyBasePath: this._historyBasePath,
      pathMismatch: this._pathMismatch,
      previousEntryPath: this._previousEntryPath,
      memoryPressure: this._memoryPressure,
      highThroughputSessionCount: this._highThroughputSessionCount,
    };
  }

  /** Set the audit log path (called by daemon-main.ts). */
  setAuditLogPath(p: string | null): void { this._auditLogPath = p; }
  /** Set the quotas config path (called by daemon-main.ts). */
  setQuotasPath(p: string | null): void { this._quotasPath = p; }
  /** Set the history base path (called by daemon-main.ts). */
  setHistoryBasePath(p: string | null): void { this._historyBasePath = p; }
  /** Set whether a path mismatch was detected (called by daemon-main.ts). */
  setPathMismatch(v: boolean): void { this._pathMismatch = v; }
  /** Set the previous entry path from path-mismatch detection (called by daemon-main.ts). */
  setPreviousEntryPath(p: string | null): void { this._previousEntryPath = p; }
  /** Set the current memory pressure level (called by MemoryManager callback). */
  setMemoryPressure(v: "ok" | "warn" | "critical"): void { this._memoryPressure = v; }
  /** Set the count of sessions in high-throughput mode (called by MemoryManager callback). */
  setHighThroughputSessionCount(n: number): void { this._highThroughputSessionCount = n; }

  /**
   * Broadcast a stream event to all currently connected clients.
   * Used for daemon-initiated events like cold-restore-available.
   *
   * When broadcasting a cold-restore-available event, the payload is also
   * stored in pendingColdRestores so the resume/discard handlers can look
   * it up without re-reading disk.
   */
  broadcastToAllClients(event: DaemonEvent): void {
    // Track pending cold-restore payloads so resume/discard can find them.
    if (event.event === "cold-restore-available") {
      this.pendingColdRestores.set(event.sessionId, event.payload);
    }

    for (const client of this.clients.values()) {
      if (client.streamSocket && !client.streamSocket.destroyed) {
        this.sendStreamEvent(client, event);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Connection handling
  // -------------------------------------------------------------------------

  private handleConnection(socket: net.Socket): void {
    if (this.disposed) {
      socket.destroy();
      return;
    }

    // Each new connection must send a hello message first.
    // We use a temporary decoder to parse the hello.
    const decoder = new FrameDecoder();
    let helloReceived = false;

    const onData = (chunk: Buffer) => {
      if (helloReceived) return;

      try {
        decoder.push(chunk);
      } catch {
        socket.destroy();
        return;
      }

      const frames = decoder.frames();
      if (frames.length === 0) return;

      helloReceived = true;
      socket.removeListener("data", onData);

      const hello = frames[0]!.json as HelloMessage;
      this.handleHello(socket, decoder, hello, frames.slice(1));
    };

    socket.on("data", onData);
    socket.on("error", () => {
      socket.destroy();
    });
  }

  private handleHello(
    socket: net.Socket,
    decoder: FrameDecoder,
    hello: HelloMessage,
    remainingFrames: Array<{ json: unknown; payload: Buffer | null }>,
  ): void {
    // Validate hello message shape
    if (!hello || hello.type !== "hello") {
      this.sendHelloResponse(socket, false, "Expected hello message");
      socket.destroy();
      return;
    }

    // Validate protocol version
    if (hello.version !== PROTOCOL_VERSION) {
      this.sendHelloResponse(
        socket,
        false,
        `Version mismatch: expected ${PROTOCOL_VERSION}, got ${hello.version}`,
      );
      socket.destroy();
      return;
    }

    // Validate token
    if (hello.token !== this.token) {
      this.sendHelloResponse(socket, false, "Invalid token");
      socket.destroy();
      return;
    }

    // Validate role
    if (hello.role !== "control" && hello.role !== "stream") {
      this.sendHelloResponse(socket, false, `Invalid role: ${hello.role}`);
      socket.destroy();
      return;
    }

    // Send success response
    this.sendHelloResponse(socket, true);

    // Register client
    const clientId = hello.clientId;
    let client = this.clients.get(clientId);
    if (!client) {
      client = {
        clientId,
        controlSocket: null,
        streamSocket: null,
        controlDecoder: null,
        streamDecoder: null,
        attachedSessions: new Map(),
      };
      this.clients.set(clientId, client);
      this.metrics.recordClientConnected();
    }

    if (hello.role === "control") {
      // Replace existing control socket if any
      if (client.controlSocket) {
        client.controlSocket.destroy();
      }
      client.controlSocket = socket;
      client.controlDecoder = decoder;
      this.wireControlSocket(client, remainingFrames);
    } else {
      // Replace existing stream socket if any
      if (client.streamSocket) {
        client.streamSocket.destroy();
      }
      client.streamSocket = socket;
      client.streamDecoder = decoder;
      this.wireStreamSocket(client);
    }

    // Reset idle timer on client connect
    this.resetIdleTimer();
  }

  // -------------------------------------------------------------------------
  // Control socket wiring
  // -------------------------------------------------------------------------

  private wireControlSocket(
    client: ConnectedClient,
    initialFrames: Array<{ json: unknown; payload: Buffer | null }>,
  ): void {
    const socket = client.controlSocket!;
    const decoder = client.controlDecoder!;

    // Process any frames that arrived in the same chunk as hello
    for (const frame of initialFrames) {
      this.handleControlFrame(client, frame.json as DaemonRequest, frame.payload);
    }

    socket.on("data", (chunk: Buffer) => {
      try {
        decoder.push(chunk);
      } catch {
        socket.destroy();
        return;
      }

      const frames = decoder.frames();
      for (const frame of frames) {
        this.handleControlFrame(client, frame.json as DaemonRequest, frame.payload);
      }
    });

    socket.on("close", () => {
      this.handleClientDisconnect(client, "control");
    });

    socket.on("error", () => {
      // Error already handled by close event
    });
  }

  private wireStreamSocket(client: ConnectedClient): void {
    const socket = client.streamSocket!;

    // Stream socket is primarily daemon → client, but handle close
    socket.on("close", () => {
      this.handleClientDisconnect(client, "stream");
    });

    socket.on("error", () => {
      // Error already handled by close event
    });

    // Handle drain event for backpressure relief
    socket.on("drain", () => {
      this.handleSocketDrain(client);
    });
  }

  // -------------------------------------------------------------------------
  // Request routing
  // -------------------------------------------------------------------------

  private handleControlFrame(
    client: ConnectedClient,
    request: DaemonRequest,
    payload: Buffer | null,
  ): void {
    if (!request || !request.type) return;

    const isNotification = request.id?.startsWith(NOTIFY_PREFIX) ?? false;

    // Handle async requests separately
    if (request.type === "prepare-upgrade") {
      if (!isNotification && request.id) {
        void this.handlePrepareUpgrade(client, request.id, request.payload);
      }
      return;
    }

    if (request.type === "cold-restore-resume") {
      if (!isNotification && request.id) {
        void this.handleColdRestoreResume(client, request.id, request.payload);
      }
      return;
    }

    try {
      const result = this.routeRequest(client, request, payload);

      // Fire-and-forget: no response for notifications
      if (!isNotification && request.id) {
        this.sendResponse(client, { id: request.id, ok: true, payload: result });
      }
    } catch (err: unknown) {
      if (!isNotification && request.id) {
        const message = err instanceof Error ? err.message : String(err);
        this.sendResponse(client, { id: request.id, ok: false, error: message });
      }
    }
  }

  private routeRequest(
    client: ConnectedClient,
    request: DaemonRequest,
    payload: Buffer | null,
  ): unknown {
    switch (request.type) {
      case "createOrAttach": {
        const p = request.payload;
        const streamClient = this.createStreamClient(client, p.sessionId);

        // beforeSpawn may throw to refuse a new spawn (e.g. quota exceeded).
        // Reattach must bypass new-session quotas so existing long-running
        // sessions remain reachable even when the daemon is at capacity.
        if (!this.host.hasAttachableSession(p.sessionId)) {
          this.hooks.beforeSpawn?.();
        }

        const result = this.host.createOrAttach({
          sessionId: p.sessionId,
          cols: p.cols,
          rows: p.rows,
          cwd: p.cwd,
          env: p.env,
          command: p.command,
          shell: p.shell,
          startupCommand: p.startupCommand,
          shellReadySupported: p.shellReadySupported,
          agentMeta: p.agentMeta,
          streamClient,
        });

        // Track attachment
        const token = Symbol(`attach:${client.clientId}:${p.sessionId}`);
        client.attachedSessions.set(p.sessionId, token);

        // Register agent session with AgentTracker for OSC state parsing
        if (p.agentMeta && result.isNew) {
          this.agentTracker.registerSession(p.sessionId, p.agentMeta);
        }

        if (result.isNew) {
          // Reset idle timer on session create
          this.resetIdleTimer();
          this.metrics.recordSessionSpawned();
          this.hooks.onSessionCreated?.(p.sessionId, {
            sessionId: p.sessionId,
            cols: p.cols,
            rows: p.rows,
            cwd: p.cwd,
            env: p.env,
            command: p.command,
            shellReadySupported: p.shellReadySupported,
            agentMeta: p.agentMeta,
            streamClient,
          });
        } else {
          this.hooks.onSessionAttached?.(p.sessionId);
        }

        return result;
      }

      case "write": {
        const sessionId = request.payload.sessionId;
        // Data comes in the binary payload tail, or in the JSON payload
        // for fire-and-forget notifications from the IPC bridge.
        if (payload) {
          this.host.write(sessionId, payload.toString("utf-8"));
        } else if ((request.payload as Record<string, unknown>).data) {
          this.host.write(sessionId, (request.payload as Record<string, unknown>).data as string);
        }
        return null;
      }

      case "resize": {
        const { sessionId, cols, rows } = request.payload;
        this.host.resize(sessionId, cols, rows);
        return null;
      }

      case "detach": {
        const { sessionId } = request.payload;
        const token = client.attachedSessions.get(sessionId);
        if (!token) return { detached: false };
        this.host.detachClient(sessionId, token);
        client.attachedSessions.delete(sessionId);
        this.backpressuredSessions.delete(sessionId);
        this.hooks.onSessionDetached?.(sessionId);
        return { detached: true };
      }

      case "kill": {
        const { sessionId } = request.payload;
        this.hooks.onSessionKilled?.(sessionId);
        this.host.kill(sessionId);
        return null;
      }

      case "signal": {
        const { sessionId, signal } = request.payload;
        this.host.signal(sessionId, signal);
        return null;
      }

      case "getCwd": {
        const { sessionId } = request.payload;
        return { cwd: this.host.getCwd(sessionId) };
      }

      case "getSnapshot": {
        const { sessionId } = request.payload;
        return this.host.getSnapshot(sessionId);
      }

      case "clearScrollback": {
        const { sessionId } = request.payload;
        this.host.clearScrollback(sessionId);
        return null;
      }

      case "listSessions": {
        const sessions = this.host.listSessions();
        // Enrich agent sessions with real activity state from AgentTracker
        for (const session of sessions) {
          if (session.agentMeta) {
            const trackerState = this.agentTracker.getState(session.sessionId);
            if (trackerState) {
              session.agentMeta.activityState = trackerState;
            }
            const agentInfo = this.agentTracker.listAgentSessions()
              .find(a => a.sessionId === session.sessionId);
            if (agentInfo) {
              session.agentMeta.stateChangedAt = agentInfo.stateChangedAt;
            }
          }
        }
        return sessions;
      }

      case "ping": {
        return this.buildPingResponse();
      }

      case "getMetrics": {
        return this.metrics.getMetrics();
      }

      case "getDaemonInfo": {
        return this.getDaemonInfo();
      }

      case "checkpoint": {
        const { sessionId } = request.payload;
        this.hooks.onCheckpointRequested?.(sessionId);
        return null;
      }

      case "cold-restore-discard": {
        const { sessionId } = request.payload;
        // Remove from pending map so a subsequent resume is a no-op
        this.pendingColdRestores.delete(sessionId);
        // Delegate history cleanup to the hook (calls historyManager.removeSession)
        this.hooks.onColdRestoreDiscard?.(sessionId);
        return null;
      }

      case "shutdown": {
        const { killSessions } = request.payload;
        // Perform shutdown asynchronously
        setImmediate(() => this.shutdown({ killSessions }));
        return { acknowledged: true };
      }

      default:
        throw new Error(`Unknown request type: ${(request as { type: string }).type}`);
    }
  }

  // -------------------------------------------------------------------------
  // Async request handlers
  // -------------------------------------------------------------------------

  /**
   * Handle the prepare-upgrade request asynchronously.
   * Serializes sessions, spawns successor, waits for ack/nak/timeout.
   */
  private async handlePrepareUpgrade(
    client: ConnectedClient,
    requestId: string,
    payload: { successorPath: string },
  ): Promise<void> {
    try {
      // Application upgrades reuse this daemon. The adopted-fd backend cannot
      // resize live PTYs, so replacing their owner would break running TUIs.
      if (this.host.sessionCount > 0) {
        this.sendResponse(client, { id: requestId, ok: false, error: "Daemon replacement is deferred while terminals are running. Application updates preserve these sessions; end the terminals before replacing their daemon." });
        return;
      }
      // Gather live session info for serialization
      const sessions = this.host.getLiveSessionInfo();
      const ptyFds = this.host.getPtyFds();
      const snapshotDir = path.dirname(this.socketPath);

      const result = await prepareUpgrade({
        successorPath: payload.successorPath,
        socketPath: this.socketPath,
        sessions,
        ptyFds,
        snapshotDir,
        onSuccess: () => {
          // Predecessor should exit — perform graceful shutdown without killing sessions
          setImmediate(() => void this.shutdown({ killSessions: false }));
        },
      });

      this.sendResponse(client, {
        id: requestId,
        ok: result.success,
        payload: result,
        ...(result.success ? {} : { error: result.error }),
      } as DaemonResponse);
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      this.sendResponse(client, { id: requestId, ok: false, error: message });
    }
  }

  /**
   * Handle a cold-restore-resume request.
   *
   * Steps:
   * 1. Look up the pending cold-restore payload for the session.
   * 2. Validate that the persisted cwd exists; fall back to os.homedir() if not.
   * 3. Merge the sanitized persisted env with the user-provided env.
   * 4. Spawn a fresh subprocess via createOrAttach.
   * 5. Write snapshot.snapshotAnsi + snapshot.rehydrateSequences to all
   *    attached stream clients via crab:term-data before any input flows.
   * 6. Call hooks.onColdRestoreResume so historyManager.openSession is
   *    invoked (overwrites stale meta with fresh createdAt + cleared endedAt).
   * 7. On any failure, broadcast cold-restore-failed and return an error response.
   */
  private async handleColdRestoreResume(
    client: ConnectedClient,
    requestId: string,
    payload: import("./protocol.js").ColdRestoreResumePayload,
  ): Promise<void> {
    const { sessionId, env: userEnv } = payload;

    try {
      // Look up the pending cold-restore payload
      const pending = this.pendingColdRestores.get(sessionId);
      if (!pending) {
        throw new Error(`No pending cold-restore for session: ${sessionId}`);
      }

      // Validate cwd — fall back to homedir if missing
      let resolvedCwd: string;
      let cwdMissing = false;
      const persistedCwd = pending.cwd;
      if (persistedCwd && fs.existsSync(persistedCwd)) {
        resolvedCwd = persistedCwd;
      } else {
        resolvedCwd = os.homedir();
        cwdMissing = true;
      }

      // Merge envs: persisted sanitized env is the base; user-provided values
      // override (allowing the user to supply stripped secrets).
      const persistedEnv = this.hooks.getPersistedEnv?.(sessionId);
      const mergedEnv: Record<string, string> = {
        ...persistedEnv,
        ...userEnv,
      };

      // Build a stream client for the resuming session
      const streamClient = this.createStreamClient(client, sessionId);

      // Spawn a fresh subprocess via createOrAttach
      const result = this.host.createOrAttach({
        sessionId,
        cols: pending.cols,
        rows: pending.rows,
        cwd: resolvedCwd,
        env: Object.keys(mergedEnv).length > 0 ? mergedEnv : undefined,
        streamClient,
      });

      // Track attachment for the requesting client
      const token = Symbol(`attach:${client.clientId}:${sessionId}`);
      client.attachedSessions.set(sessionId, token);

      // Register agent session with AgentTracker if agentMeta is present
      // (cold-restored sessions don't carry agentMeta, but guard anyway)
      if (result.isNew) {
        this.resetIdleTimer();
        this.metrics.recordSessionSpawned();
        // Notify hooks — this calls historyManager.openSession, overwriting
        // the stale meta with a fresh createdAt and cleared endedAt.
        this.hooks.onColdRestoreResume?.(sessionId, {
          sessionId,
          cols: pending.cols,
          rows: pending.rows,
          cwd: resolvedCwd,
          env: Object.keys(mergedEnv).length > 0 ? mergedEnv : undefined,
          streamClient,
        });
      }

      // Replay snapshot content to all attached stream clients before any
      // input flows. Replay uses the same ordering as warm attachment so
      // entering the alternate buffer cannot erase the restored display.
      // Send a data event so xterm receives the restored screen state.
      const snapshot = pending.snapshot;
      const replayData = snapshotReplayData(snapshot);
      if (replayData.length > 0) {
        const replayBuf = Buffer.from(replayData, "utf-8");
        for (const c of this.clients.values()) {
          if (c.attachedSessions.has(sessionId) && c.streamSocket) {
            this.writeToSocket(c.streamSocket, {
              type: "event",
              event: "data",
              sessionId,
            } satisfies DaemonEvent, replayBuf);
          }
        }
      }

      // Remove from pending map — resume is complete
      this.pendingColdRestores.delete(sessionId);

      this.sendResponse(client, {
        id: requestId,
        ok: true,
        payload: {
          cwdMissing: cwdMissing || undefined,
          resolvedCwd,
          pid: result.pid,
        } satisfies import("./protocol.js").ColdRestoreResumeResult,
      });
    } catch (err: unknown) {
      const reason = err instanceof Error ? err.message : String(err);

      // Broadcast cold-restore-failed to all clients
      this.broadcastToAllClients({
        type: "event",
        event: "cold-restore-failed",
        sessionId,
        payload: { reason },
      });

      this.sendResponse(client, { id: requestId, ok: false, error: reason });
    }
  }

  // -------------------------------------------------------------------------
  // Stream client factory (multi-client fan-out)
  // -------------------------------------------------------------------------

  private createStreamClient(client: ConnectedClient, sessionId: string): StreamClient {
    return {
      id: client.clientId,
      sendData: (_sid: string, data: Buffer) => {
        // Feed data to AgentTracker for OSC state parsing
        this.agentTracker.handleData(sessionId, data.toString("utf-8"));

        // Notify hooks with byte length only (not the data itself)
        this.hooks.onSessionData?.(sessionId, data.length);

        // Fan-out: broadcast to ALL clients attached to this session
        for (const c of this.clients.values()) {
          if (c.attachedSessions.has(sessionId) && c.streamSocket) {
            this.batcher.enqueue(c.clientId, sessionId, data);
          }
        }
      },
      sendExit: (_sid: string, code: number) => {
        // Notify AgentTracker of session exit
        this.agentTracker.handleExit(sessionId, code);

        // Flush batcher before sending exit to ensure final output arrives first
        for (const c of this.clients.values()) {
          if (c.attachedSessions.has(sessionId)) {
            this.batcher.flush(c.clientId);
          }
        }
        // Broadcast exit to all attached clients
        for (const c of this.clients.values()) {
          if (c.attachedSessions.has(sessionId) && c.streamSocket) {
            this.sendStreamEvent(c, {
              type: "event",
              event: "exit",
              sessionId,
              payload: { code },
            });
          }
        }
      },
      sendStateChanged: (
        _sid: string,
        state: string,
        previousState: string,
        timestamp: number,
      ) => {
        for (const c of this.clients.values()) {
          if (c.attachedSessions.has(sessionId) && c.streamSocket) {
            this.sendStreamEvent(c, {
              type: "event",
              event: "state-changed",
              sessionId,
              payload: { state, previousState, timestamp },
            } as unknown as DaemonEvent);
          }
        }
      },
    };
  }

  // -------------------------------------------------------------------------
  // Agent state broadcast
  // -------------------------------------------------------------------------

  /**
   * Broadcast a state-changed event to all clients attached to the session.
   * Called by AgentTracker.onStateChange.
   */
  private broadcastStateChanged(
    sessionId: string,
    state: AgentActivityState,
    previousState: AgentActivityState,
  ): void {
    const timestamp = Date.now();
    for (const c of this.clients.values()) {
      if (c.attachedSessions.has(sessionId) && c.streamSocket) {
        this.sendStreamEvent(c, {
          type: "event",
          event: "state-changed",
          sessionId,
          payload: { state, previousState, timestamp },
        } as unknown as DaemonEvent);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Batch flush handler
  // -------------------------------------------------------------------------

  private handleBatchFlush(clientId: string, entries: BatchEntry[]): void {
    const client = this.clients.get(clientId);
    if (!client || !client.streamSocket) return;

    for (const entry of entries) {
      const event: DaemonEvent = {
        type: "event",
        event: "data",
        sessionId: entry.sessionId,
      };
      this.writeToSocket(client.streamSocket, event, entry.data);
      this.metrics.recordBytesRelayed(entry.data.length);

      // Check backpressure after write
      this.checkBackpressure(client, entry.sessionId);
    }
  }

  // -------------------------------------------------------------------------
  // Backpressure
  // -------------------------------------------------------------------------

  private checkBackpressure(client: ConnectedClient, sessionId: string): void {
    if (!client.streamSocket) return;

    if (client.streamSocket.writableLength > BACKPRESSURE_THRESHOLD) {
      if (!this.backpressuredSessions.has(sessionId)) {
        this.backpressuredSessions.add(sessionId);
        // Pause the PTY read stream for this session
        try {
          // Access the session through the host to pause its subprocess
          this.pauseSession(sessionId);
        } catch {
          // Session may have exited
        }
      }
    }
  }

  private handleSocketDrain(client: ConnectedClient): void {
    // Resume all sessions that were paused due to this client's backpressure
    for (const sessionId of this.backpressuredSessions) {
      if (client.attachedSessions.has(sessionId)) {
        this.backpressuredSessions.delete(sessionId);
        try {
          this.resumeSession(sessionId);
        } catch {
          // Session may have exited
        }
      }
    }
  }

  private pauseSession(sessionId: string): void {
    this.host.pauseSession(sessionId);
  }

  private resumeSession(sessionId: string): void {
    this.host.resumeSession(sessionId);
  }

  // -------------------------------------------------------------------------
  // Session exit handling
  // -------------------------------------------------------------------------

  private handleSessionExit(sessionId: string, exitCode: number): void {
    // The exit event is already sent via the StreamClient.sendExit callback
    // which is wired in createStreamClient. Clean up attachment tracking.
    for (const client of this.clients.values()) {
      client.attachedSessions.delete(sessionId);
    }

    // Remove from backpressure tracking
    this.backpressuredSessions.delete(sessionId);

    // Notify hooks
    this.hooks.onSessionExit?.(sessionId, exitCode);
  }

  // -------------------------------------------------------------------------
  // Client disconnect
  // -------------------------------------------------------------------------

  private handleClientDisconnect(client: ConnectedClient, role: "control" | "stream"): void {
    if (role === "control") {
      client.controlSocket = null;
      client.controlDecoder = null;
    } else {
      client.streamSocket = null;
      client.streamDecoder = null;
    }

    // If both sockets are gone, fully disconnect the client
    if (!client.controlSocket && !client.streamSocket) {
      // Implicit detach for all sessions this client was attached to
      // (sessions continue running, just no longer streaming to this client)
      for (const sessionId of client.attachedSessions.keys()) {
        this.hooks.onSessionDetached?.(sessionId);
      }
      client.attachedSessions.clear();
      this.batcher.clear(client.clientId);
      this.clients.delete(client.clientId);
      this.metrics.recordClientDisconnected();
    }
  }

  // -------------------------------------------------------------------------
  // Ping response
  // -------------------------------------------------------------------------

  private buildPingResponse(): PingResponse {
    return {
      uptime: Date.now() - this.startedAt,
      sessions: this.host.sessionCount,
      clients: this.clients.size,
      memoryUsage: process.memoryUsage().heapUsed,
      protocolVersion: PROTOCOL_VERSION,
      fdCount: this.metrics.getFdCount(),
    };
  }

  // -------------------------------------------------------------------------
  // Idle timer
  // -------------------------------------------------------------------------

  private resetIdleTimer(): void {
    this.clearIdleTimer();

    if (this.idleTimeoutMs <= 0) return;

    this.idleTimer = setTimeout(() => {
      this.idleTimer = null;
      // Only self-terminate if no clients and no sessions
      if (this.clients.size === 0 && this.host.sessionCount === 0) {
        this.onIdleShutdown?.();
      } else {
        // Still active — reset
        this.resetIdleTimer();
      }
    }, this.idleTimeoutMs);

    // Unref so it doesn't keep the process alive
    if (this.idleTimer.unref) {
      this.idleTimer.unref();
    }
  }

  private clearIdleTimer(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
  }

  // -------------------------------------------------------------------------
  // Socket I/O helpers
  // -------------------------------------------------------------------------

  private sendHelloResponse(socket: net.Socket, ok: boolean, error?: string): void {
    const response: HelloResponse = { type: "hello", ok, error };
    const frame = encodeFrame(response);
    socket.write(frame);
  }

  private sendResponse(client: ConnectedClient, response: DaemonResponse): void {
    if (!client.controlSocket || client.controlSocket.destroyed) return;
    this.writeToSocket(client.controlSocket, response);
  }

  private sendStreamEvent(client: ConnectedClient, event: DaemonEvent): void {
    if (!client.streamSocket || client.streamSocket.destroyed) return;
    this.writeToSocket(client.streamSocket, event);
  }

  /**
   * Write a framed message to a socket with outbound buffer cap enforcement.
   */
  private writeToSocket(
    socket: net.Socket,
    json: unknown,
    payload?: Buffer | Uint8Array,
  ): void {
    if (socket.destroyed) return;

    // Outbound buffer cap: destroy socket if exceeded
    if (socket.writableLength > OUTBOUND_BUFFER_CAP) {
      socket.destroy();
      return;
    }

    const frame = encodeFrame(json, payload);
    socket.write(frame);
  }

  // -------------------------------------------------------------------------
  // File cleanup
  // -------------------------------------------------------------------------

  private cleanupFiles(): void {
    try {
      fs.unlinkSync(socketAddress(this.socketPath));
    } catch {
      // May not exist
    }
    try {
      fs.unlinkSync(this.tokenPath);
    } catch {
      // May not exist
    }
    try {
      fs.unlinkSync(this.pidPath);
    } catch {
      // May not exist
    }
  }
}
