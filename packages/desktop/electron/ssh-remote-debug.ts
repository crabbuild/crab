/**
 * Remote Debug Manager — manages remote debug sessions including
 * launching debug processes, setting up port forwarding, opening
 * debugger interfaces, and cleaning up on session end.
 *
 * Integrates with the RpcRouter for agent communication and the
 * SshSessionManager for port forwarding via SSH tunnels.
 */

import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import * as net from "node:net";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Constants ────────────────────────────────────────────────────

/** Maximum time allowed for port forwarding setup (milliseconds). */
const PORT_FORWARD_TIMEOUT_MS = 15_000;

/** Maximum number of alternative ports to try when the requested port is in use. */
const MAX_PORT_RETRIES = 10;

/** Minimum valid port number for alternative selection. */
const MIN_PORT = 1024;

/** Maximum valid port number. */
const MAX_PORT = 65535;

// ── Types ────────────────────────────────────────────────────────

export type DebugRuntime = "node" | "python" | "rust" | "cpp";

export type DebugProtocol = "chrome-devtools" | "gdbserver" | "debugpy";

export type DebugSessionStatus = "starting" | "active" | "stopping" | "terminated" | "error";

export interface DebugSessionOpts {
  runtime: DebugRuntime;
  command: string;
  cwd: string;
  port: number;
  env?: Record<string, string>;
}

export interface DebugSession {
  id: string;
  profileId: string;
  runtime: DebugRuntime;
  protocol: DebugProtocol;
  remotePid: number;
  remotePort: number;
  localPort: number;
  status: DebugSessionStatus;
  debugUrl: string;
  startedAt: number;
  error?: string;
}

export interface RemoteDebugManagerEvents {
  "debug-session-started": (session: DebugSession) => void;
  "debug-session-stopped": (payload: { sessionId: string; profileId: string }) => void;
  "debug-session-error": (payload: { sessionId: string; profileId: string; error: string }) => void;
}

// ── RemoteDebugManager ───────────────────────────────────────────

export class RemoteDebugManager extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private sessions = new Map<string, DebugSession>();
  private portForwardServers = new Map<string, net.Server>();
  private disconnectListeners = new Map<string, () => void>();

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * Start a remote debug session: launch the debug process on the remote,
   * set up port forwarding, and return session info with the debugger URL.
   *
   * The entire setup must complete within 15 seconds or the session is aborted.
   */
  async startSession(profileId: string, opts: DebugSessionOpts): Promise<DebugSession> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(
        `Cannot start debug session: SSH connection status is "${status}"`,
      );
    }

    const sessionId = randomUUID();
    const session: DebugSession = {
      id: sessionId,
      profileId,
      runtime: opts.runtime,
      protocol: this.resolveProtocol(opts.runtime),
      remotePid: 0,
      remotePort: opts.port,
      localPort: 0,
      status: "starting",
      debugUrl: "",
      startedAt: Date.now(),
    };

    this.sessions.set(sessionId, session);

    try {
      // Wrap the entire setup in a timeout
      const result = await this.withTimeout(
        this.setupSession(profileId, sessionId, opts),
        PORT_FORWARD_TIMEOUT_MS,
        "Debug session setup timed out (15s limit)",
      );

      return result;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      session.status = "error";
      session.error = message;

      // Clean up any partial setup
      await this.cleanupSession(sessionId);

      this.emit("debug-session-error", {
        sessionId,
        profileId,
        error: message,
      });

      throw err;
    }
  }

  /**
   * Stop a debug session: terminate the remote process, close port
   * forwarding, and clean up all resources.
   */
  async stopSession(sessionId: string): Promise<void> {
    const session = this.sessions.get(sessionId);
    if (!session) {
      return;
    }

    session.status = "stopping";

    try {
      // Tell the remote agent to stop the debug process
      if (session.remotePid > 0) {
        const status = this.sessionManager.getStatus(session.profileId);
        if (status === "connected") {
          try {
            await this.router.sendToRemoteAgent(session.profileId, "debug_stop", {
              pid: session.remotePid,
            });
          } catch {
            // Best-effort: process may already be gone
          }
        }
      }
    } finally {
      await this.cleanupSession(sessionId);
      session.status = "terminated";

      this.emit("debug-session-stopped", {
        sessionId,
        profileId: session.profileId,
      });
    }
  }

  /**
   * Get an active debug session by ID.
   */
  getSession(sessionId: string): DebugSession | undefined {
    return this.sessions.get(sessionId);
  }

  /**
   * List all debug sessions for a profile.
   */
  listSessions(profileId: string): DebugSession[] {
    const results: DebugSession[] = [];
    for (const session of this.sessions.values()) {
      if (session.profileId === profileId) {
        results.push(session);
      }
    }
    return results;
  }

  /**
   * Stop all debug sessions for a profile. Called on disconnect.
   */
  async stopAllForProfile(profileId: string): Promise<void> {
    const sessionsToStop: string[] = [];
    for (const [id, session] of this.sessions) {
      if (session.profileId === profileId && session.status !== "terminated") {
        sessionsToStop.push(id);
      }
    }

    for (const id of sessionsToStop) {
      await this.stopSession(id);
    }
  }

  /**
   * Clean up all sessions. Called during graceful shutdown.
   */
  async dispose(): Promise<void> {
    const allIds = [...this.sessions.keys()];
    for (const id of allIds) {
      await this.stopSession(id);
    }
    this.sessions.clear();
    this.removeAllListeners();
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Orchestrate the full debug session setup:
   * 1. Check/find available port on remote
   * 2. Launch debug process on remote
   * 3. Set up local port forwarding
   * 4. Build debugger URL
   */
  private async setupSession(
    profileId: string,
    sessionId: string,
    opts: DebugSessionOpts,
  ): Promise<DebugSession> {
    const session = this.sessions.get(sessionId)!;

    // Step 1: Check if the requested port is available on the remote
    let remotePort = opts.port;
    const portCheckResult = await this.router.sendToRemoteAgent(profileId, "port_check", {
      port: remotePort,
    }) as { available: boolean; alternative?: number };

    if (!portCheckResult.available) {
      // Try alternative ports
      remotePort = await this.findAvailableRemotePort(profileId, opts.port);
    }

    session.remotePort = remotePort;

    // Step 2: Launch the debug process on the remote
    const launchResult = await this.router.sendToRemoteAgent(profileId, "debug_launch", {
      runtime: opts.runtime,
      command: this.buildDebugCommand(opts, remotePort),
      cwd: opts.cwd,
      port: remotePort,
      ...(opts.env && { env: opts.env }),
    }) as { pid: number; port: number; protocol: string };

    session.remotePid = launchResult.pid;
    session.remotePort = launchResult.port;

    // Step 3: Set up local port forwarding
    const localPort = await this.setupPortForward(profileId, sessionId, launchResult.port);
    session.localPort = localPort;

    // Step 4: Build the debugger URL
    session.debugUrl = this.buildDebugUrl(session.protocol, localPort);
    session.status = "active";

    // Listen for unexpected disconnection
    this.watchForDisconnect(profileId, sessionId);

    this.emit("debug-session-started", session);

    return session;
  }

  /**
   * Find an available port on the remote by trying alternatives.
   * Tries port+1, port+2, etc. up to MAX_PORT_RETRIES.
   */
  private async findAvailableRemotePort(profileId: string, startPort: number): Promise<number> {
    for (let offset = 1; offset <= MAX_PORT_RETRIES; offset++) {
      const candidate = startPort + offset;
      if (candidate > MAX_PORT) {
        break;
      }

      const result = await this.router.sendToRemoteAgent(profileId, "port_check", {
        port: candidate,
      }) as { available: boolean };

      if (result.available) {
        return candidate;
      }
    }

    throw new Error(
      `No available debug port found in range ${startPort}–${Math.min(startPort + MAX_PORT_RETRIES, MAX_PORT)}`,
    );
  }

  /**
   * Set up a local TCP server that forwards connections through the SSH
   * tunnel to the remote debug port.
   */
  private async setupPortForward(
    profileId: string,
    sessionId: string,
    remotePort: number,
  ): Promise<number> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || conn.status !== "connected") {
      throw new Error("SSH connection lost during port forward setup");
    }

    // Try the same port locally first, then alternatives
    let localPort = remotePort;
    let server: net.Server | null = null;

    for (let attempt = 0; attempt <= MAX_PORT_RETRIES; attempt++) {
      const candidatePort = localPort + attempt;
      if (candidatePort > MAX_PORT || candidatePort < MIN_PORT) {
        continue;
      }

      try {
        server = await this.createLocalServer(profileId, candidatePort, remotePort);
        localPort = candidatePort;
        break;
      } catch (err) {
        const isAddrInUse =
          err instanceof Error &&
          ((err as NodeJS.ErrnoException).code === "EADDRINUSE" ||
            err.message.includes("EADDRINUSE"));

        if (!isAddrInUse || attempt === MAX_PORT_RETRIES) {
          throw new Error(
            `Failed to set up local port forwarding: ${err instanceof Error ? err.message : String(err)}`,
          );
        }
        // Try next port
      }
    }

    if (!server) {
      throw new Error("Failed to create local port forward server");
    }

    this.portForwardServers.set(sessionId, server);
    return localPort;
  }

  /**
   * Create a local TCP server that tunnels connections to the remote
   * debug port via the SSH connection.
   */
  private createLocalServer(
    profileId: string,
    localPort: number,
    remotePort: number,
  ): Promise<net.Server> {
    return new Promise<net.Server>((resolve, reject) => {
      const server = net.createServer((socket) => {
        const conn = this.sessionManager.getConnection(profileId);
        if (!conn || conn.status !== "connected") {
          socket.destroy();
          return;
        }

        conn.client.forwardOut(
          "127.0.0.1",
          localPort,
          "127.0.0.1",
          remotePort,
          (err, channel) => {
            if (err) {
              socket.destroy();
              return;
            }

            socket.pipe(channel);
            channel.pipe(socket);

            socket.on("error", () => channel.close());
            channel.on("error", () => socket.destroy());
            socket.on("close", () => channel.close());
            channel.on("close", () => socket.destroy());
          },
        );
      });

      server.on("error", (err) => {
        reject(err);
      });

      server.listen(localPort, "127.0.0.1", () => {
        server.removeAllListeners("error");
        server.on("error", () => {
          // Swallow errors after successful listen — handled per-connection
        });
        resolve(server);
      });
    });
  }

  /**
   * Watch for SSH disconnection and terminate the debug session if
   * the connection drops unexpectedly.
   */
  private watchForDisconnect(profileId: string, sessionId: string): void {
    const listener = () => {
      const session = this.sessions.get(sessionId);
      if (session && session.status === "active") {
        session.status = "error";
        session.error = "SSH connection lost unexpectedly";

        this.cleanupSession(sessionId);

        this.emit("debug-session-error", {
          sessionId,
          profileId,
          error: "SSH connection lost unexpectedly",
        });
      }
    };

    // Listen for status changes on the session manager
    const statusHandler = (payload: { profileId: string; status: string }) => {
      if (
        payload.profileId === profileId &&
        (payload.status === "disconnected" || payload.status === "error")
      ) {
        listener();
        this.sessionManager.removeListener("ssh:status-changed", statusHandler);
      }
    };

    this.sessionManager.on("ssh:status-changed", statusHandler);
    this.disconnectListeners.set(sessionId, () => {
      this.sessionManager.removeListener("ssh:status-changed", statusHandler);
    });
  }

  /**
   * Clean up resources for a session: close port forward server,
   * remove disconnect listener.
   */
  private async cleanupSession(sessionId: string): Promise<void> {
    // Close the port forward server
    const server = this.portForwardServers.get(sessionId);
    if (server) {
      await new Promise<void>((resolve) => {
        server.close(() => resolve());
      });
      this.portForwardServers.delete(sessionId);
    }

    // Remove disconnect listener
    const removeListener = this.disconnectListeners.get(sessionId);
    if (removeListener) {
      removeListener();
      this.disconnectListeners.delete(sessionId);
    }
  }

  /**
   * Resolve the debug protocol from the runtime type.
   */
  private resolveProtocol(runtime: DebugRuntime): DebugProtocol {
    switch (runtime) {
      case "node":
        return "chrome-devtools";
      case "python":
        return "debugpy";
      case "rust":
      case "cpp":
        return "gdbserver";
    }
  }

  /**
   * Build the debug command with appropriate flags for the runtime.
   * If the user already provided a full command with debug flags,
   * use it as-is. Otherwise, inject the appropriate debug flag.
   */
  private buildDebugCommand(opts: DebugSessionOpts, port: number): string {
    const { command, runtime } = opts;

    // If the command already contains debug flags, use it directly
    if (this.hasDebugFlags(command, runtime)) {
      return command;
    }

    // Inject debug flags based on runtime
    switch (runtime) {
      case "node":
        return command.replace(/^node\s/, `node --inspect=0.0.0.0:${port} `);
      case "python":
        return `python -m debugpy --listen 0.0.0.0:${port} --wait-for-client ${command}`;
      case "rust":
      case "cpp":
        return `gdbserver 0.0.0.0:${port} ${command}`;
    }
  }

  /**
   * Check if a command already contains debug flags for the given runtime.
   */
  private hasDebugFlags(command: string, runtime: DebugRuntime): boolean {
    switch (runtime) {
      case "node":
        return command.includes("--inspect") || command.includes("--inspect-brk");
      case "python":
        return command.includes("debugpy");
      case "rust":
      case "cpp":
        return command.includes("gdbserver");
    }
  }

  /**
   * Build the URL for the appropriate debugger interface.
   */
  private buildDebugUrl(protocol: DebugProtocol, localPort: number): string {
    switch (protocol) {
      case "chrome-devtools":
        return `devtools://devtools/bundled/js_app.html?experiments=true&v8only=true&ws=127.0.0.1:${localPort}`;
      case "debugpy":
        return `debugpy://127.0.0.1:${localPort}`;
      case "gdbserver":
        return `gdb://127.0.0.1:${localPort}`;
    }
  }

  /**
   * Wrap a promise with a timeout. Rejects with the given message
   * if the promise doesn't resolve within the specified duration.
   */
  private withTimeout<T>(promise: Promise<T>, ms: number, message: string): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(message));
      }, ms);

      promise
        .then((result) => {
          clearTimeout(timer);
          resolve(result);
        })
        .catch((err) => {
          clearTimeout(timer);
          reject(err);
        });
    });
  }
}
