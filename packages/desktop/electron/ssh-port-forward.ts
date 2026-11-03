/**
 * SSH port forwarding manager.
 *
 * Creates local TCP servers that tunnel traffic through SSH connections
 * to remote hosts. Supports auto-start on connection, auto-restart on
 * reconnection, dynamic detection from terminal output, and port
 * conflict resolution.
 */

import { EventEmitter } from "node:events";
import * as net from "node:net";
import { randomUUID } from "node:crypto";
import { shell } from "electron";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager, PortForward } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface ActiveForward {
  id: string;
  profileId: string;
  localPort: number;
  remoteHost: string;
  remotePort: number;
  label?: string;
  autoStart: boolean;
  status: "active" | "stopped" | "error";
  server: net.Server | null;
  channels: Set<ClientChannel>;
  actualLocalPort: number;
  error?: string;
}

export interface ForwardInfo {
  id: string;
  profileId: string;
  localPort: number;
  actualLocalPort: number;
  remoteHost: string;
  remotePort: number;
  label?: string;
  autoStart: boolean;
  status: "active" | "stopped" | "error";
  error?: string;
}

export interface DetectedForward {
  remotePort: number;
  label: string;
  remoteHost: string;
}

export interface PortForwardManagerEvents {
  "ssh:port-forward-detected": (payload: {
    profileId: string;
    suggestion: DetectedForward;
  }) => void;
  "ssh:port-forward-status": (payload: {
    forwardId: string;
    status: "active" | "stopped" | "error";
    actualLocalPort?: number;
    error?: string;
  }) => void;
}

// ── Detection patterns ───────────────────────────────────────────

interface DetectionPattern {
  regex: RegExp;
  label: string;
}

const DETECTION_PATTERNS: DetectionPattern[] = [
  { regex: /Running on http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)/, label: "Jupyter" },
  { regex: /TensorBoard\s+[\d.]+\s+at\s+http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)/, label: "TensorBoard" },
  { regex: /Listening on (?:port\s+)?(\d+)/, label: "Server" },
  { regex: /Running on http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)\//, label: "Flask" },
  { regex: /Uvicorn running on http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)/, label: "FastAPI" },
  { regex: /webpack.*?http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)/, label: "Webpack" },
  { regex: /Local:\s+http:\/\/(?:localhost|127\.0\.0\.1):(\d+)/, label: "Vite" },
  { regex: /Started.*?on port (\d+)/, label: "Server" },
  { regex: /http:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d+)/, label: "HTTP Server" },
];

// Maximum number of port retries on conflict
const MAX_PORT_RETRIES = 10;

// ── Manager ──────────────────────────────────────────────────────

/**
 * Manages SSH port forwards. Creates local TCP servers that tunnel
 * connections through SSH to remote hosts.
 *
 * Events:
 * - "ssh:port-forward-detected" ({ profileId, suggestion }) — URL detected in terminal output
 * - "ssh:port-forward-status" ({ forwardId, status, actualLocalPort?, error? }) — forward status change
 */
export class PortForwardManager extends EventEmitter {
  private forwards = new Map<string, ActiveForward>();
  private previouslyActive = new Map<string, Set<string>>();

  constructor(private sessionManager: SshSessionManager) {
    super();
  }

  /**
   * Create a local TCP server that tunnels through SSH to the remote host.
   * If the requested local port is in use, tries subsequent ports up to +10.
   *
   * Returns the forward ID (UUID).
   */
  async addLocalForward(
    profileId: string,
    localPort: number,
    remoteHost: string,
    remotePort: number,
    label?: string,
    autoStart?: boolean,
  ): Promise<string> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || conn.status !== "connected") {
      throw new Error(
        `SSH connection for profile "${profileId}" is not active. ` +
        "Connect to the server before adding port forwards.",
      );
    }

    const id = randomUUID();
    const forward: ActiveForward = {
      id,
      profileId,
      localPort,
      remoteHost,
      remotePort,
      label,
      autoStart: autoStart ?? false,
      status: "stopped",
      server: null,
      channels: new Set(),
      actualLocalPort: localPort,
    };

    this.forwards.set(id, forward);

    try {
      const actualPort = await this.startServer(forward, conn.client);
      forward.actualLocalPort = actualPort;
      forward.status = "active";

      this.trackActive(profileId, id);
      this.emitStatus(forward);

      return id;
    } catch (err: unknown) {
      forward.status = "error";
      forward.error = err instanceof Error ? err.message : String(err);
      this.emitStatus(forward);
      throw err;
    }
  }

  /**
   * Close the local TCP server and all active SSH channels for a forward.
   */
  removeForward(forwardId: string): void {
    const forward = this.forwards.get(forwardId);
    if (!forward) {
      return;
    }

    this.stopForward(forward);
    this.forwards.delete(forwardId);

    // Remove from previously active tracking
    const profileForwards = this.previouslyActive.get(forward.profileId);
    if (profileForwards) {
      profileForwards.delete(forwardId);
    }
  }

  /**
   * Return all forwards, optionally filtered by profile ID.
   */
  listForwards(profileId?: string): ForwardInfo[] {
    const results: ForwardInfo[] = [];

    for (const forward of this.forwards.values()) {
      if (profileId && forward.profileId !== profileId) {
        continue;
      }
      results.push(this.toForwardInfo(forward));
    }

    return results;
  }

  /**
   * Start all forwards marked autoStart for a profile.
   * Called after a successful SSH connection.
   */
  async autoStartForwards(profileId: string, portForwards: PortForward[]): Promise<void> {
    const autoStartForwards = portForwards.filter((pf) => pf.autoStart);

    for (const pf of autoStartForwards) {
      try {
        await this.addLocalForward(
          profileId,
          pf.localPort,
          pf.remoteHost,
          pf.remotePort,
          pf.label,
          true,
        );
      } catch {
        // Auto-start failures are non-fatal — the forward will show as "error"
      }
    }
  }

  /**
   * Re-establish all previously active port forwards after reconnection.
   */
  async restoreForwards(profileId: string): Promise<void> {
    const previousIds = this.previouslyActive.get(profileId);
    if (!previousIds || previousIds.size === 0) {
      return;
    }

    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || conn.status !== "connected") {
      return;
    }

    for (const forwardId of previousIds) {
      const forward = this.forwards.get(forwardId);
      if (!forward || forward.status === "active") {
        continue;
      }

      try {
        const actualPort = await this.startServer(forward, conn.client);
        forward.actualLocalPort = actualPort;
        forward.status = "active";
        forward.error = undefined;
        this.emitStatus(forward);
      } catch (err: unknown) {
        forward.status = "error";
        forward.error = err instanceof Error ? err.message : String(err);
        this.emitStatus(forward);
      }
    }
  }

  /**
   * Detect port forward suggestions from terminal output using regex patterns.
   * Returns a suggested forward if a URL/port pattern is detected, or null.
   */
  detectFromTerminal(output: string): DetectedForward | null {
    for (const pattern of DETECTION_PATTERNS) {
      const match = pattern.regex.exec(output);
      if (match && match[1]) {
        const port = parseInt(match[1], 10);
        if (port > 0 && port <= 65535) {
          return {
            remotePort: port,
            label: pattern.label,
            remoteHost: "127.0.0.1",
          };
        }
      }
    }
    return null;
  }

  /**
   * Emit a port-forward-detected event for a profile. Called by the
   * terminal manager when output matches a detection pattern.
   */
  emitDetection(profileId: string, suggestion: DetectedForward): void {
    this.emit("ssh:port-forward-detected", { profileId, suggestion });
  }

  /**
   * Open the forwarded URL in the system browser.
   */
  async openInBrowser(forwardId: string): Promise<void> {
    const forward = this.forwards.get(forwardId);
    if (!forward) {
      throw new Error(`Port forward not found: ${forwardId}`);
    }

    if (forward.status !== "active") {
      throw new Error(
        `Port forward "${forward.label ?? forwardId}" is not active (status: ${forward.status}).`,
      );
    }

    const url = `http://localhost:${forward.actualLocalPort}`;
    await shell.openExternal(url);
  }

  /**
   * Stop all forwards for a profile. Called on disconnect.
   */
  stopAllForProfile(profileId: string): void {
    for (const forward of this.forwards.values()) {
      if (forward.profileId === profileId && forward.status === "active") {
        this.stopForward(forward);
        forward.status = "stopped";
        this.emitStatus(forward);
      }
    }
  }

  // ── Private helpers ──────────────────────────────────────────

  /**
   * Start a local TCP server for the forward. Handles port conflicts
   * by trying subsequent ports up to MAX_PORT_RETRIES.
   */
  private async startServer(
    forward: ActiveForward,
    client: import("ssh2").Client,
  ): Promise<number> {
    let lastError: Error | null = null;

    for (let offset = 0; offset <= MAX_PORT_RETRIES; offset++) {
      const port = forward.localPort + offset;

      try {
        const server = await this.createTcpServer(forward, client, port);
        forward.server = server;
        return port;
      } catch (err: unknown) {
        if (
          err instanceof Error &&
          (err as NodeJS.ErrnoException).code === "EADDRINUSE"
        ) {
          lastError = err;
          continue;
        }
        throw err;
      }
    }

    throw new Error(
      `All ports ${forward.localPort}-${forward.localPort + MAX_PORT_RETRIES} are in use. ` +
      `Original error: ${lastError?.message ?? "unknown"}`,
    );
  }

  /**
   * Create a TCP server on the specified port that tunnels connections
   * through SSH to the remote host.
   */
  private createTcpServer(
    forward: ActiveForward,
    client: import("ssh2").Client,
    port: number,
  ): Promise<net.Server> {
    return new Promise((resolve, reject) => {
      const server = net.createServer((socket) => {
        this.handleConnection(forward, client, socket);
      });

      server.on("error", (err) => {
        reject(err);
      });

      server.listen(port, "127.0.0.1", () => {
        // Remove the error listener that was for startup only
        server.removeAllListeners("error");

        // Add a runtime error handler
        server.on("error", (err) => {
          forward.status = "error";
          forward.error = err.message;
          this.emitStatus(forward);
        });

        resolve(server);
      });
    });
  }

  /**
   * Handle an incoming TCP connection by opening an SSH tunnel channel
   * and piping data bidirectionally.
   */
  private handleConnection(
    forward: ActiveForward,
    client: import("ssh2").Client,
    socket: net.Socket,
  ): void {
    client.forwardOut(
      "127.0.0.1",
      forward.actualLocalPort,
      forward.remoteHost,
      forward.remotePort,
      (err, channel) => {
        if (err) {
          socket.destroy();
          return;
        }

        forward.channels.add(channel);

        // Pipe bidirectionally
        socket.pipe(channel);
        channel.pipe(socket);

        // Clean up on close
        const cleanup = () => {
          forward.channels.delete(channel);
          socket.unpipe(channel);
          channel.unpipe(socket);
          if (!socket.destroyed) {
            socket.destroy();
          }
          if (!channel.destroyed) {
            channel.close();
          }
        };

        socket.on("close", cleanup);
        socket.on("error", cleanup);
        channel.on("close", cleanup);
        channel.on("error", cleanup);
      },
    );
  }

  /**
   * Stop a forward: close the TCP server and all active channels.
   */
  private stopForward(forward: ActiveForward): void {
    if (forward.server) {
      try {
        forward.server.close();
      } catch {
        // Server may already be closed
      }
      forward.server = null;
    }

    for (const channel of forward.channels) {
      try {
        channel.close();
      } catch {
        // Channel may already be closed
      }
    }
    forward.channels.clear();
  }

  /**
   * Track a forward as previously active for a profile (for reconnection restore).
   */
  private trackActive(profileId: string, forwardId: string): void {
    let set = this.previouslyActive.get(profileId);
    if (!set) {
      set = new Set();
      this.previouslyActive.set(profileId, set);
    }
    set.add(forwardId);
  }

  /**
   * Emit a status change event for a forward.
   */
  private emitStatus(forward: ActiveForward): void {
    this.emit("ssh:port-forward-status", {
      forwardId: forward.id,
      status: forward.status,
      actualLocalPort: forward.actualLocalPort,
      error: forward.error,
    });
  }

  /**
   * Convert an ActiveForward to a ForwardInfo (public-facing shape).
   */
  private toForwardInfo(forward: ActiveForward): ForwardInfo {
    return {
      id: forward.id,
      profileId: forward.profileId,
      localPort: forward.localPort,
      actualLocalPort: forward.actualLocalPort,
      remoteHost: forward.remoteHost,
      remotePort: forward.remotePort,
      label: forward.label,
      autoStart: forward.autoStart,
      status: forward.status,
      error: forward.error,
    };
  }
}
