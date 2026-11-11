import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Client } from "ssh2";
import type { ClientChannel, SFTPWrapper, ConnectConfig } from "ssh2";
import { getPassword } from "./keychain";
import { connectVerified } from "./ssh-verified-connect";
import type { AgentDeployer } from "./ssh-deployer";
import { DeployError } from "./ssh-deployer";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { RpcRouter } from "./ssh-router";
import type { RepositoryRuntime } from "./repo-capabilities";
import { parseRepoId, type RemoteEndpoint } from "./ssh-types";
import { HostKeyError, hostKeyFingerprint, type HostKeyVerifier } from "./ssh-known-hosts";

// ── Connection status ────────────────────────────────────────────

export type SshConnectionStatus =
  | "disconnected"
  | "disconnecting"
  | "connecting"
  | "deploying"
  | "connected"
  | "reconnecting"
  | "error";

// ── Authentication ───────────────────────────────────────────────

export type SshAuthMethod =
  | { type: "agent" }
  | { type: "key"; keyPath: string }
  | { type: "key_passphrase"; keyPath: string }
  | { type: "password" };

// ── Port forwarding ──────────────────────────────────────────────

export interface PortForward {
  id: string;
  profileId: string;
  direction: "local" | "remote";
  localPort: number;
  remoteHost: string;
  remotePort: number;
  label?: string;
  autoStart: boolean;
  status: "active" | "stopped" | "error";
}

// ── Profile ──────────────────────────────────────────────────────

export interface SshProfile {
  id: string;
  label: string;
  host: string;
  port: number;
  username: string;
  authMethod: SshAuthMethod;
  remotePath?: string;
  jumpHost?: string;
  forwardAgent: boolean;
  compression: boolean;
  keepAliveInterval: number;
  autoReconnect: boolean;
  autoDeployAgent: boolean;
  colour?: string;
  tags: string[];
  portForwards: PortForward[];
  createdAt: number;
  lastConnectedAt?: number;
  pinned?: boolean;
  /** Timeout in ms for the initial SSH connection handshake (default: 10000). */
  connectionTimeoutMs?: number;
  /** Explicit RPC deadline. Unset: metadata uses 30s; owned file work runs to completion/cancel. */
  operationTimeoutMs?: number;
}

// ── Server info ──────────────────────────────────────────────────

export interface SshServerInfo {
  os: string;
  arch: string;
  agentVersion: string;
  hostname: string;
  uptime?: number;
}

// ── Active connection state ──────────────────────────────────────

export interface SshConnection {
  profile: SshProfile;
  lifetime: AbortController;
  endpoint?: RemoteEndpoint;
  client: Client;
  status: SshConnectionStatus;
  error?: string;
  channels: {
    agent?: ClientChannel;
    terminals: Map<string, ClientChannel>;
    sftp?: SFTPWrapper;
    portForwards: Map<string, ClientChannel>;
  };
  serverInfo?: SshServerInfo;
  jumpClients: Client[];
  agentSessionId?: string;
}

// ── Event types ──────────────────────────────────────────────────

export interface SshSessionManagerEvents {
  "ssh:status-changed": (payload: { profileId: string; status: SshConnectionStatus; error?: string }) => void;
  "ssh:server-info": (payload: { profileId: string; info: SshServerInfo }) => void;
  "ssh:reconnecting": (payload: { profileId: string; attempt: number; maxAttempts: number }) => void;
}

// ── Constants ────────────────────────────────────────────────────

const SSH_KEYCHAIN_SERVICE = "crab-desktop-ssh";

// ── Agent deployment options ─────────────────────────────────────

export interface AgentDeployDeps {
  createDeployer: () => AgentDeployer;
  capabilityNegotiator: CapabilityNegotiator;
  rpcRouter: RpcRouter;
}

// ── Session manager ──────────────────────────────────────────────

/**
 * Manages SSH connections to remote servers. Each connection is keyed
 * by profile ID and supports multiple channels (agent RPC, terminals,
 * SFTP, port forwards) over a single TCP socket.
 *
 * Events:
 * - "ssh:status-changed" ({ profileId, status, error? })
 * - "ssh:server-info" ({ profileId, info })
 * - "ssh:reconnecting" ({ profileId, attempt, maxAttempts })
 */
export class SshSessionManager extends EventEmitter {
  private connections = new Map<string, SshConnection>();
  private reconnectionState = new Map<string, {
    timer: NodeJS.Timeout | null;
    attempt: number;
    startedAt: number;
    client?: Client;
    jumpClients: Client[];
    controller: AbortController;
    promise: Promise<void>;
    resolve: () => void;
    reject: (error: Error) => void;
  }>();
  private _suspendedAt: number | null = null;
  private agentDeps: AgentDeployDeps | null = null;

  private deployments = new Map<string, { client: Client; controller: AbortController; sessionId?: string; ready: Promise<void>; promise: Promise<void> }>();
  private openings = new Map<string, { controller: AbortController; promise: Promise<void> }>();
  private closing = new Map<string, Promise<void>>();
  private sftpOpen = new WeakMap<Client, Promise<SFTPWrapper>>();
  private repositories = new Map<string, Map<string, { path: string; handle: Record<string, unknown>; runtimeId: string | null; sessionId: string | null; error?: string }>>();
  private repositoryOpenings = new Map<string, { sessionId: string; cancelled: boolean; promise: Promise<Record<string, unknown>> }>();
  private storageChanges = new Map<string, Promise<void>>();
  private nextAgentSession = 1;
  private nextRepositorySession = 1;

  constructor(private readonly verifyHostKey: HostKeyVerifier) {
    super();
    this.setupPowerMonitor();
  }

  /**
   * Timestamp of when the system last suspended, or null if not
   * currently suspended. Useful for diagnostics and testing.
   */
  get suspendedAt(): number | null {
    return this._suspendedAt;
  }

  /**
   * Listen for Electron `powerMonitor` suspend/resume events.
   * On suspend, record the timestamp. On resume, check all active
   * connections for health and trigger reconnection for stale ones.
   *
   * Safe to call in non-Electron environments (e.g., tests) — uses
   * try/catch around the electron import.
   */
  private setupPowerMonitor(): void {
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const { powerMonitor } = require("electron") as typeof import("electron");

      powerMonitor.on("suspend", () => {
        this._suspendedAt = Date.now();
      });

      powerMonitor.on("resume", () => {
        this._suspendedAt = null;
        this.checkAllConnectionsHealth();
      });
    } catch {
      // Not running in Electron (e.g., unit tests) — skip power monitor setup
    }
  }

  /**
   * Check health of all currently connected profiles. For any that
   * fail the health check, trigger reconnection.
   */
  private async checkAllConnectionsHealth(): Promise<void> {
    const connectedProfiles: string[] = [];
    for (const [profileId, conn] of this.connections) {
      if (conn.status === "connected") {
        connectedProfiles.push(profileId);
      }
    }

    for (const profileId of connectedProfiles) {
      const healthy = await this.checkConnectionHealth(profileId);
      if (!healthy) {
        this.attemptReconnection(profileId);
      }
    }
  }

  /**
   * Proactively check whether a connection is still responsive by
   * running a quick command (`echo 1`) with a short timeout (3s).
   *
   * Returns `true` if the connection responds, `false` if it is
   * unresponsive, disconnected, or times out.
   */
  async checkConnectionHealth(profileId: string): Promise<boolean> {
    const conn = this.connections.get(profileId);
    if (!conn || conn.status !== "connected") {
      return false;
    }

    const { client } = conn;

    return new Promise<boolean>((resolve) => {
      const timeout = setTimeout(() => {
        resolve(false);
      }, 3000);

      try {
        client.exec("echo 1", (err, stream) => {
          if (err) {
            clearTimeout(timeout);
            resolve(false);
            return;
          }

          let received = false;
          stream.on("data", () => {
            received = true;
          });
          stream.on("close", () => {
            clearTimeout(timeout);
            resolve(received);
          });
          stream.on("error", () => {
            clearTimeout(timeout);
            resolve(false);
          });
        });
      } catch {
        clearTimeout(timeout);
        resolve(false);
      }
    });
  }

  /**
   * Establish an SSH connection for the given profile. Authenticates,
   * verifies host key, and transitions through connecting → connected.
   */
  async connect(requested: SshProfile): Promise<void> {
    const profile = structuredClone(requested);
    const closing = this.closing.get(profile.id);
    if (closing) await closing;
    const previous = this.connections.get(profile.id);
    if (previous && (previous.profile.host.toLowerCase() !== profile.host.toLowerCase() || previous.profile.port !== profile.port || previous.profile.username !== profile.username)) {
      // An edited profile selects another authority. Drain the old transport,
      // then require explicit repository opens instead of replaying its paths.
      await this.disconnect(profile.id);
      const repositories = this.repositories.get(profile.id);
      this.repositories.delete(profile.id);
      for (const repoId of repositories?.keys() ?? []) this.publishRepositoryRuntime(profile.id, repoId);
    }
    const pending = this.openings.get(profile.id);
    if (pending) return pending.promise;
    const reconnecting = this.reconnectionState.get(profile.id);
    if (reconnecting) return reconnecting.promise;
    const existing = this.connections.get(profile.id);
    if (existing?.status === "connected") return;
    if (existing?.status === "deploying") return this.ensureAgent(profile.id);
    if (existing && existing.status !== "disconnected") {
      await this.disconnect(profile.id);
      return this.connect(profile);
    }
    const client = new Client();
    const controller = new AbortController();
    const connection: SshConnection = {
      profile, client, lifetime: controller, status: "connecting",
      channels: { terminals: new Map(), portForwards: new Map() }, jumpClients: [],
    };
    const entry = { controller, promise: Promise.resolve().then(() => this.establish(connection, controller.signal)) };
    this.openings.set(profile.id, entry);
    this.connections.set(profile.id, connection);
    this.setStatus(profile.id, "connecting");
    this.watchTrust(connection);
    this.watchClient(connection, client);
    try { await entry.promise; }
    finally { if (this.openings.get(profile.id) === entry) this.openings.delete(profile.id); }
  }

  private async establish(connection: SshConnection, signal: AbortSignal): Promise<void> {
    const { client, profile, jumpClients } = connection;
    let authenticated = false;
    try {
      signal.throwIfAborted();
      const config = await this.buildConnectConfig(profile, { readyTimeout: profile.connectionTimeoutMs ?? 10000, jumpClients, lifetime: connection.lifetime });
      signal.throwIfAborted();
      if (this.connections.get(profile.id) !== connection || connection.status !== "connecting") throw new Error("SSH connection superseded");
      await connectVerified(client, config, profile.host, profile.port, connection.lifetime, this.verifyHostKey, key => {
        connection.endpoint = { host: profile.host.toLowerCase(), port: profile.port, username: profile.username, hostKey: hostKeyFingerprint(key) };
      });
      authenticated = true;
      signal.throwIfAborted();
      if (connection.status !== "connecting") throw new Error("SSH connection superseded");
      if (profile.autoDeployAgent || this.repositories.get(profile.id)?.size) {
        this.setStatus(profile.id, "deploying");
        await this.ensureAgent(profile.id);
      } else this.setStatus(profile.id, "connected");
    } catch (error) {
      if (this.connections.get(profile.id) === connection && connection.status === "connecting") {
        this.setStatus(profile.id, "error", error instanceof Error ? error.message : String(error));
      }
      if (!authenticated) {
        client.destroy();
        for (const jump of jumpClients.splice(0).reverse()) jump.destroy();
      }
      throw error;
    }
  }

  private watchTrust(connection: SshConnection): void {
    connection.lifetime.signal.addEventListener("abort", () => {
      const reason: unknown = connection.lifetime.signal.reason;
      if (!(reason instanceof HostKeyError) || this.connections.get(connection.profile.id) !== connection) return;
      this.cancelReconnection(connection.profile.id);
      this.agentDeps?.capabilityNegotiator.clearCache(connection.profile.id);
      this.agentDeps?.rpcRouter.clearQueue(connection.profile.id);
      this.setStatus(connection.profile.id, "error", reason.message);
    }, { once: true });
  }

  private watchClient(connection: SshConnection, client: Client): void {
    const current = () => this.connections.get(connection.profile.id) === connection && connection.client === client;
    const ended = (error?: Error) => {
      if (!current() || ["disconnected", "disconnecting", "error", "reconnecting"].includes(connection.status)) return;
      this.agentDeps?.capabilityNegotiator.clearCache(connection.profile.id);
      if (connection.profile.autoReconnect && ["connected", "deploying"].includes(connection.status)) this.attemptReconnection(connection.profile.id);
      else this.setStatus(connection.profile.id, error ? "error" : "disconnected", error?.message);
    };
    client.on("error", ended);
    client.on("end", () => ended());
    client.on("close", () => ended());
  }

  /**
   * Gracefully disconnect: close all channels, end the SSH session,
   * and update status to "disconnected".
   *
   * Safe to call multiple times — returns early if the connection
   * doesn't exist or is already disconnected.
   */
  async disconnect(profileId: string): Promise<void> {
    const pending = this.closing.get(profileId);
    if (pending) return pending;
    const connection = this.connections.get(profileId);
    if (!connection || connection.status === "disconnected") return;
    // Install the close owner before emitting status; observers may re-enter.
    const closing = Promise.resolve().then(() => this.closeConnection(connection));
    this.closing.set(profileId, closing);
    try { await closing; } finally { if (this.closing.get(profileId) === closing) this.closing.delete(profileId); }
  }

  private async closeConnection(connection: SshConnection): Promise<void> {
    const profileId = connection.profile.id;
    this.cancelReconnection(profileId);
    this.setStatus(profileId, "disconnecting");
    const opening = this.openings.get(profileId);
    opening?.controller.abort(new Error("SSH connection closed during authentication"));
    const deployment = this.deployments.get(profileId);
    deployment?.controller.abort(new Error("SSH connection closed during deployment"));
    // Authentication can be destroyed immediately. Deployment/native work needs
    // the transport to acknowledge cancellation and remove its temporary state.
    if (opening && !deployment && !connection.channels.agent) {
      connection.client.destroy();
      for (const jump of [...connection.jumpClients].reverse()) jump.destroy();
    }
    const { client, channels } = connection;
    // EOF must precede joining replay: the native owner needs that signal to
    // cancel and drain an open that is still holding bootstrap's completion.
    // Authentication/bootstrap callers retain their original terminal error.
    const [native] = await Promise.allSettled([
      channels.agent ? this.drainAgent(channels.agent) : Promise.resolve(),
      opening?.promise,
      deployment?.promise,
    ]);
    this.agentDeps?.capabilityNegotiator.clearCache(profileId);
    this.agentDeps?.rpcRouter.clearQueue(profileId);
    const failure: unknown = native.status === "rejected" ? native.reason : undefined;
    for (const channel of channels.terminals.values()) channel.end();
    channels.sftp?.end();
    for (const channel of channels.portForwards.values()) channel.end();
    connection.lifetime.abort(new Error("SSH connection disconnected"));
    client.end();
    // Opening SFTP owns no native work; close the transport first so a pending
    // subsystem request can fail instead of blocking disconnect indefinitely.
    await this.sftpOpen.get(client)?.catch(() => {});
    for (const jump of [...connection.jumpClients].reverse()) jump.end();
    connection.jumpClients = [];
    channels.agent = undefined; channels.terminals.clear(); channels.sftp = undefined; channels.portForwards.clear();
    this.setStatus(profileId, "disconnected", failure instanceof Error ? failure.message : undefined);
    if (failure) throw failure;
  }

  private drainAgent(channel: ClientChannel): Promise<void> {
    if (channel.closed || channel.destroyed) return Promise.resolve();
    return new Promise((resolve, reject) => {
      let failure: Error | undefined;
      const failed = (error: Error) => { failure = error; };
      channel.on("error", failed);
      channel.once("close", (code?: number) => {
        channel.removeListener("error", failed);
        if (failure) reject(failure);
        else if (code !== 0) reject(new Error(`SSH agent closed without a successful exit status (${code})`));
        else resolve();
      });
      channel.end();
    });
  }

  /**
   * Test connectivity to a host without persisting the connection.
   * Connects, runs `echo ok`, disconnects, and returns success/failure
   * with a detailed error message on failure.
   *
   * Uses a shorter timeout (5s) and does NOT modify `this.connections`.
   */
  async testConnection(profile: SshProfile, passwordOverride?: string): Promise<{ ok: boolean; error?: string }> {
    let client: Client | undefined;
    const jumpClients: Client[] = [];
    const lifetime = new AbortController();
    try {
      const config = await this.buildConnectConfig(profile, { readyTimeout: 5000, jumpClients, lifetime }, passwordOverride);
      client = new Client();

      await connectVerified(client, config, profile.host, profile.port, lifetime, this.verifyHostKey);

      const testingClient = client;
      const output = await new Promise<string>((resolve, reject) => {
        let settled = false;
        let channel: ClientChannel | undefined;
        let data = "";
        let received = 0;
        const cleanup = () => {
          settled = true; clearTimeout(timer);
          testingClient.removeListener("close", disconnected);
          testingClient.removeListener("error", failed);
          channel?.removeListener("data", collect);
          channel?.removeListener("close", completed);
          channel?.stderr.removeListener("data", drain);
        };
        const failed = (reason: unknown) => {
          if (settled) return;
          cleanup(); reject(reason); channel?.destroy();
        };
        const disconnected = () => failed(new Error("SSH connection closed during connection test"));
        const completed = (code?: number | null, signal?: string) => {
          if (code !== undefined && code !== 0) {
            failed(new Error(signal ? `Remote connection test ended with signal ${signal}` : `Remote connection test exited with status ${code}`));
            return;
          }
          cleanup(); resolve(data);
        };
        const collect = (chunk: Buffer) => {
          received += chunk.length;
          if (received > 64 * 1024) { failed(new Error("Remote connection test output exceeded 64 KiB")); return; }
          data += chunk.toString();
        };
        const drain = () => {};
        // Authentication alone does not prove an exec channel will open or close.
        // Bound the whole command, including its callback, and retire late channels.
        const timer = setTimeout(() => failed(new Error("Remote connection test command timed out")), 5000);
        testingClient.once("close", disconnected); testingClient.once("error", failed);
        try {
          testingClient.exec("echo ok", (error, stream) => {
            if (error) { failed(error); return; }
            // Consume errors until physical close even if the test already ended.
            stream.on("error", failed);
            stream.once("close", () => stream.removeListener("error", failed));
            if (settled) { stream.destroy(); return; }
            channel = stream;
            stream.on("data", collect); stream.once("close", completed); stream.stderr.on("data", drain);
          });
        } catch (reason: unknown) { failed(reason); }
      });

      if (!output.split(/\r?\n/).includes("ok")) {
        return { ok: false, error: "Remote command did not return expected output" };
      }

      return { ok: true };
    } catch (err: unknown) {
      const error = this.categorizeConnectionError(err);
      return { ok: false, error };
    } finally {
      lifetime.abort(new Error("SSH connection test finished"));
      if (client) client.destroy();
      for (const jumpClient of jumpClients.reverse()) jumpClient.destroy();
    }
  }

  /**
   * Retrieve the active connection for a profile, or undefined if
   * not connected.
   */
  getConnection(profileId: string): SshConnection | undefined {
    return this.connections.get(profileId);
  }

  /**
   * Get the current connection status for a profile.
   */
  getStatus(profileId: string): SshConnectionStatus {
    return this.connections.get(profileId)?.status ?? "disconnected";
  }

  /**
   * Return all active connections as an array.
   */
  getAllConnections(): SshConnection[] {
    return Array.from(this.connections.values());
  }

  // ── Reconnection ───────────────────────────────────────────

  /**
   * Cancel an active reconnection attempt. Rejects all pending calls
   * and clears the reconnection state.
   */
  cancelReconnection(profileId: string): void {
    const state = this.reconnectionState.get(profileId);
    if (!state) {
      return;
    }

    if (state.timer) {
      clearTimeout(state.timer);
    }

    this.agentDeps?.rpcRouter.clearQueue(profileId);

    state.controller.abort(new Error("SSH reconnection cancelled"));
    state.client?.destroy();
    for (const jump of state.jumpClients.splice(0).reverse()) jump.destroy();
    state.reject(new Error("SSH reconnection cancelled"));
    this.reconnectionState.delete(profileId);
  }

  /**
   * Attempt to reconnect to a remote server with exponential backoff.
   * Delays: 1s, 2s, 4s, 8s, 16s, 30s (capped). Gives up after 60s total.
   */
  private attemptReconnection(profileId: string): void {
    // Don't start a new reconnection if one is already in progress
    if (this.reconnectionState.has(profileId)) {
      // An adopted retry transport can fail during native bootstrap. Revoke
      // that deployment before its error path can report the dead SSH as ready.
      this.setStatus(profileId, "reconnecting");
      this.deployments.get(profileId)?.controller.abort(new Error("SSH connection lost during deployment"));
      return;
    }

    const conn = this.connections.get(profileId);
    if (!conn) {
      return;
    }

    this.deployments.get(profileId)?.controller.abort(new Error("SSH connection lost during deployment"));
    let resolve!: () => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<void>((done, failed) => { resolve = done; reject = failed; });
    // Background retries report through status events; repository opens can
    // join the same outcome without creating a second authenticated transport.
    void promise.catch(() => {});
    this.reconnectionState.set(profileId, {
      timer: null,
      attempt: 0,
      startedAt: Date.now(),
      jumpClients: [], controller: new AbortController(), promise, resolve, reject,
    });

    this.setStatus(profileId, "reconnecting");
    this.scheduleReconnectionAttempt(profileId);
  }

  private scheduleReconnectionAttempt(profileId: string): void {
    const state = this.reconnectionState.get(profileId);
    if (!state) {
      return;
    }

    const delay = Math.min(1000 * Math.pow(2, state.attempt), 30000);

    this.emit("ssh:reconnecting", {
      profileId,
      attempt: state.attempt + 1,
      maxAttempts: Math.ceil(60000 / delay),
    });

    state.timer = setTimeout(async () => {
      const currentState = this.reconnectionState.get(profileId);
      if (!currentState) {
        return; // Reconnection was cancelled
      }

      const conn = this.connections.get(profileId);
      if (!conn) {
        this.cancelReconnection(profileId);
        return;
      }

      try {
        // Attempt to reconnect using the stored profile
        const client = new Client();
        currentState.client = client;
        const config = await this.buildConnectConfig(conn.profile, { readyTimeout: 10000, jumpClients: currentState.jumpClients, lifetime: currentState.controller });
        if (this.reconnectionState.get(profileId) !== currentState) { client.destroy(); return; }

        let endpoint: RemoteEndpoint | undefined;
        await connectVerified(client, config, conn.profile.host, conn.profile.port, currentState.controller, this.verifyHostKey, key => {
          const { host, port, username } = conn.profile;
          endpoint = { host: host.toLowerCase(), port, username, hostKey: hostKeyFingerprint(key) };
        });

        // Cancellation revokes an unadopted transport. A late authentication
        // reply must not restart an agent after the user disconnected.
        if (this.reconnectionState.get(profileId) !== currentState || conn.status !== "reconnecting") { client.destroy(); return; }
        currentState.client = undefined;
        conn.lifetime = currentState.controller;
        conn.endpoint = endpoint;
        this.watchTrust(conn);
        conn.client = client;
        conn.channels = {
          terminals: new Map(),
          portForwards: new Map(),
        };

        // Track jump clients from the new connection
        if (conn.profile.jumpHost) {
          // Close old jump clients first
          for (const oldJumpClient of [...conn.jumpClients].reverse()) {
            try {
              oldJumpClient.end();
            } catch {
              // Already closed
            }
          }
          conn.jumpClients = currentState.jumpClients.splice(0);
        }

        this.watchClient(conn, client);
        if (conn.profile.autoDeployAgent || this.repositories.get(profileId)?.size) {
          this.setStatus(profileId, "deploying");
          await this.ensureAgent(profileId);
        } else this.setStatus(profileId, "connected");
        if (this.reconnectionState.get(profileId) !== currentState || conn.client !== client || this.getStatus(profileId) !== "connected") throw new Error("SSH reconnection superseded");
        currentState.resolve();
        this.reconnectionState.delete(profileId);

      } catch (error) {
        currentState.client?.destroy();
        currentState.client = undefined;
        for (const jump of currentState.jumpClients.splice(0).reverse()) jump.destroy();
        if (this.reconnectionState.get(profileId) !== currentState) return;
        if (conn.status === "connected" || conn.status === "disconnecting" || conn.status === "disconnected") {
          currentState.reject(error instanceof Error ? error : new Error("SSH reconnection failed", { cause: error }));
          this.reconnectionState.delete(profileId);
          this.agentDeps?.rpcRouter.clearQueue(profileId);
          return;
        }
        if (error instanceof HostKeyError) {
          this.cancelReconnection(profileId);
          this.setStatus(profileId, "error", error.message);
          return;
        }
        // Failure: check if 60s total has elapsed
        const elapsed = Date.now() - currentState.startedAt;
        if (elapsed >= 60000) {
          // Give up
          this.reconnectionState.delete(profileId);
          currentState.reject(new Error("Reconnection timed out after 60s", { cause: error }));
          this.setStatus(profileId, "disconnected");
          this.emit("ssh:status-changed", { profileId, status: "disconnected", error: "Reconnection timed out after 60s" });

          this.agentDeps?.rpcRouter.clearQueue(profileId);
        } else {
          // Schedule next attempt
          currentState.attempt += 1;
          this.scheduleReconnectionAttempt(profileId);
        }
      }
    }, delay);
  }

  // ── Agent deployment ────────────────────────────────────────

  /**
   * Deploy the remote agent after SSH connection is established.
   * On success, attaches the RPC channel and broadcasts capabilities.
   * On failure, broadcasts agent-unavailable for graceful degradation.
   *
   * Deploy progress events are forwarded to the renderer via the
   * "ssh:deploy-progress" event on this manager instance.
   */
  async getSftp(profileId: string): Promise<SFTPWrapper> {
    const connection = this.connections.get(profileId);
    if (!connection || connection.status !== "connected") throw new Error("SSH connection is not ready");
    const client = connection.client;
    if (connection.channels.sftp) return connection.channels.sftp;
    const pending = this.sftpOpen.get(client);
    if (pending) return pending;
    const opening = new Promise<SFTPWrapper>((resolve, reject) => client.sftp((error, stream) => {
      if (error) { reject(error); return; }
      if (this.connections.get(profileId) !== connection || connection.client !== client || connection.status !== "connected") {
        stream.end(); reject(new Error("SSH connection closed while opening SFTP")); return;
      }
      connection.channels.sftp = stream;
      stream.once("close", () => { if (connection.channels.sftp === stream) connection.channels.sftp = undefined; });
      resolve(stream);
    }));
    this.sftpOpen.set(client, opening);
    try { return await opening; } finally { this.sftpOpen.delete(client); }
  }

  setAgentDependencies(deps: AgentDeployDeps): void {
    if (this.agentDeps) throw new Error("SSH agent deployment already configured");
    this.agentDeps = deps;
  }

  agentCapabilities(profileId: string) {
    return this.agentDeps?.capabilityNegotiator.getCached(profileId) ?? null;
  }

  agentSession(profileId: string): string | null {
    const connection = this.connections.get(profileId);
    return connection?.status === "connected" && connection.channels.agent && this.agentCapabilities(profileId)
      ? connection.agentSessionId ?? null : null;
  }

  repositoryRuntime(profileId: string, repoId: string): RepositoryRuntime {
    const sessionId = this.agentSession(profileId);
    const repository = this.repositories.get(profileId)?.get(repoId);
    return { repoId, sessionId: sessionId && repository?.runtimeId === sessionId ? repository.sessionId : null, error: sessionId ? repository?.error : undefined };
  }

  repositoryRuntimes(profileId: string): RepositoryRuntime[] {
    return [...this.repositories.get(profileId)?.keys() ?? []].map(repoId => this.repositoryRuntime(profileId, repoId));
  }

  repositoryWork(repoId: string): Promise<unknown> | undefined {
    return this.storageChanges.get(repoId) ?? this.repositoryOpenings.get(repoId)?.promise;
  }

  private publishRepositoryRuntime(profileId: string, repoId: string): void {
    this.emit("ssh:repository-runtime", { profileId, ...this.repositoryRuntime(profileId, repoId) });
  }

  async ensureAgent(profileId: string): Promise<void> {
    const connection = this.connections.get(profileId);
    if (!connection || !["connected", "deploying"].includes(connection.status)) throw new Error("SSH connection is not ready for agent deployment");
    if (!this.agentDeps) throw new Error("SSH agent deployment is not initialized");
    const pending = this.deployments.get(profileId);
    if (pending?.client === connection.client && (!pending.sessionId || pending.sessionId === connection.agentSessionId)) return pending.ready;
    if (connection.channels.agent && this.agentCapabilities(profileId)) return;
    const controller = new AbortController();
    let markReady!: () => void;
    let rejectReady!: (error: unknown) => void;
    const ready = new Promise<void>((resolve, reject) => { markReady = resolve; rejectReady = reject; });
    // Queue the body after publishing the owner, before status listeners run.
    const entry = {
      client: connection.client, controller, ready,
      sessionId: undefined as string | undefined,
      promise: Promise.resolve().then(() => this.deployAgent(connection, controller.signal, runtimeReady)),
    };
    const runtimeReady = (sessionId: string) => { entry.sessionId = sessionId; markReady(); };
    this.deployments.set(profileId, entry);
    const finished = () => { if (this.deployments.get(profileId) === entry) this.deployments.delete(profileId); };
    void entry.promise.then(finished, error => { rejectReady(error); finished(); });
    return ready;
  }

  async openRepository(profileId: string, remotePath: string): Promise<Record<string, unknown>> {
    if (!path.posix.isAbsolute(remotePath) || remotePath.includes("\0")) throw new Error("Choose an absolute remote repository path");
    const repoId = `ssh://${profileId}${remotePath}`;
    const changing = this.storageChanges.get(repoId);
    if (changing) {
      const desired = this.repositories.get(profileId)?.get(repoId);
      await changing.catch(() => {});
      if (desired && this.repositories.get(profileId)?.get(repoId) !== desired) throw new Error("SSH repository closed during opening");
    }
    const pending = this.repositoryOpenings.get(repoId);
    if (pending && this.repositories.get(profileId)?.get(repoId)?.error) {
      // An explicit retry owns a fresh attempt after failed-open cleanup. Joining
      // the old result would return the same failure without trying the repair.
      await pending.promise.catch(() => {});
      if (pending.cancelled) throw new Error("SSH repository closed during opening");
    }
    if (this.getStatus(profileId) !== "connected" || !this.agentCapabilities(profileId)) throw new Error("Deploy the SSH agent before opening a repository");
    const sessionId = this.agentSession(profileId);
    if (!sessionId) throw new Error("Deploy the SSH agent before opening a repository");
    return this.openRepositorySession(this.connections.get(profileId)!, remotePath, sessionId);
  }

  private openRepositorySession(connection: SshConnection, remotePath: string, sessionId: string): Promise<Record<string, unknown>> {
    const profileId = connection.profile.id;
    const client = connection.client;
    const repoId = `ssh://${profileId}${remotePath}`;
    const pending = this.repositoryOpenings.get(repoId);
    if (pending?.sessionId === sessionId) return pending.promise;
    const existing = this.repositories.get(profileId)?.get(repoId);
    // Bootstrap already reopened tracked repositories. Opening again removes
    // the native owner and invalidates readers captured by its ready event.
    if (existing?.runtimeId === sessionId) return Promise.resolve({ ...existing.handle, repo_id: repoId, url: repoId, is_remote: true, native_session_id: existing.sessionId });
    let repositories = this.repositories.get(profileId);
    if (!repositories) { repositories = new Map(); this.repositories.set(profileId, repositories); }
    const repository: NonNullable<typeof existing> = existing ?? { path: remotePath, handle: {}, runtimeId: null, sessionId: null };
    repositories.set(repoId, repository);
    const current = () => this.connections.get(profileId) === connection && connection.client === client && connection.agentSessionId === sessionId && !!connection.channels.agent;
    const entry = { sessionId, cancelled: false, promise: Promise.resolve().then(async () => {
      repository.runtimeId = null; repository.sessionId = null; repository.error = undefined;
      this.publishRepositoryRuntime(profileId, repoId);
      try {
        const result = await this.agentDeps!.rpcRouter.sendToRemoteAgent(profileId, "open_local", { repo_id: repoId, path: remotePath });
        if (!current()) throw new Error("SSH repository open was interrupted");
        if (!result || typeof result !== "object" || !("repo_id" in result) || result.repo_id !== repoId) throw new Error("SSH agent returned an invalid repository handle");
        if (entry.cancelled) {
          await this.agentDeps!.rpcRouter.sendToRemoteAgent(profileId, "close", { repo_id: repoId });
          throw new Error("SSH repository closed during opening");
        }
        repository.handle = result as Record<string, unknown>;
        repository.runtimeId = sessionId;
        // Rebinding one repository keeps the SSH process alive. Give its readers
        // a new identity so old handles cannot reach that replacement owner.
        repository.sessionId = `${sessionId}:repository:${this.nextRepositorySession++}`;
        this.publishRepositoryRuntime(profileId, repoId);
        return { ...result, repo_id: repoId, url: repoId, is_remote: true, native_session_id: repository.sessionId };
      } catch (error) {
        if (current() && this.repositories.get(profileId)?.get(repoId) === repository) {
          repository.error = error instanceof Error ? error.message : String(error);
          this.publishRepositoryRuntime(profileId, repoId);
          // Credential preparation can fail before open_local reaches native.
          // Drain the previous owner so its captured identity cannot survive.
          let failure = error;
          try { await this.agentDeps!.rpcRouter.retireRepository(profileId, repoId); }
          catch (cleanup) { failure = new AggregateError([error, cleanup], "SSH repository opening and native cleanup failed"); }
          if (failure !== error) {
            repository.error = failure instanceof Error ? failure.message : String(failure);
            this.publishRepositoryRuntime(profileId, repoId);
          }
          throw failure;
        }
        throw error;
      }
    }) };
    this.repositoryOpenings.set(repoId, entry);
    const finished = () => { if (this.repositoryOpenings.get(repoId) === entry) this.repositoryOpenings.delete(repoId); };
    void entry.promise.then(finished, finished);
    return entry.promise;
  }

  async storageIdentityChanged(keys: string[], state: "changed" | "removed"): Promise<void> {
    const changes = keys.flatMap(repoId => {
      const remote = parseRepoId(repoId);
      if (!remote) return [];
      // Native close and replacement share admission. A late removal must not
      // close the new identity selected while its old reader is still draining.
      const previous = this.storageChanges.get(repoId) ?? Promise.resolve();
      const work = previous.catch(() => {}).then(async () => {
        await this.repositoryOpenings.get(repoId)?.promise.catch(() => {});
        const connection = this.connections.get(remote.profileId);
        const repository = this.repositories.get(remote.profileId)?.get(repoId);
        if (!connection || !repository) return;
        repository.runtimeId = null; repository.sessionId = null;
        repository.error = state === "removed" ? "This repository's saved connection was removed. Select another connection in Storage settings." : undefined;
        this.publishRepositoryRuntime(remote.profileId, repoId);
        const runtime = this.agentSession(remote.profileId);
        if (!runtime) return;
        if (state === "changed") { await this.openRepositorySession(connection, repository.path, runtime); return; }
        try { await this.agentDeps!.rpcRouter.retireRepository(remote.profileId, repoId); }
        catch (error) {
          repository.error = error instanceof Error ? error.message : String(error);
          this.publishRepositoryRuntime(remote.profileId, repoId);
          throw error;
        }
      });
      this.storageChanges.set(repoId, work);
      const finished = () => { if (this.storageChanges.get(repoId) === work) this.storageChanges.delete(repoId); };
      void work.then(finished, finished);
      return [work];
    });
    const outcomes = await Promise.allSettled(changes);
    const failures = outcomes.flatMap(outcome => outcome.status === "rejected" ? [outcome.reason] : []);
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) throw new AggregateError(failures, "Storage identity changed but some SSH repositories could not recover");
  }

  forgetRepository(profileId: string, repoId: string): void {
    const pending = this.repositoryOpenings.get(repoId);
    if (pending) pending.cancelled = true;
    this.repositories.get(profileId)?.delete(repoId);
    this.publishRepositoryRuntime(profileId, repoId);
  }

  private async deployAgent(connection: SshConnection, signal: AbortSignal, markReady: (sessionId: string) => void): Promise<void> {
    const { createDeployer, capabilityNegotiator, rpcRouter } = this.agentDeps!;
    const agentDeployer = createDeployer();
    const { client, profile } = connection;
    const current = () => this.connections.get(profile.id) === connection && connection.client === client;
    let channel: ClientChannel | undefined;
    const progress = (value: import("./ssh-deployer").DeployProgress) => {
      if (current() && connection.status === "deploying") this.emit("ssh:deploy-progress", { profileId: profile.id, ...value });
    };
    agentDeployer.on("ssh:deploy-progress", progress);
    try {
      signal.throwIfAborted();
      this.setStatus(profile.id, "deploying");
      channel = await agentDeployer.ensureAgent(client, signal);
      signal.throwIfAborted();
      if (!current()) throw new Error("SSH deployment superseded");
      channel.stderr.resume();
      connection.channels.agent = channel;
      const sessionId = `ssh:${profile.id}:${this.nextAgentSession++}`;
      connection.agentSessionId = sessionId;
      rpcRouter.attachChannel(profile.id, channel);
      channel.once("close", () => {
        if (current() && connection.channels.agent === channel) {
          connection.channels.agent = undefined;
          connection.agentSessionId = undefined;
          capabilityNegotiator.clearCache(profile.id);
          if (connection.status === "connected") this.emit("ssh:agent-unavailable", { profileId: profile.id, reason: "AGENT_EXITED", message: "SSH agent exited" });
        }
      });
      const capabilities = await capabilityNegotiator.negotiate(profile.id);
      signal.throwIfAborted();
      if (!current() || connection.channels.agent !== channel) throw new Error("SSH deployment superseded");
      // Process readiness is independent of repository recovery. The deployment
      // owner still joins every replay on disconnect; each open owns its result.
      const openings = [...this.repositories.get(profile.id)?.values() ?? []].map(repository => this.openRepositorySession(connection, repository.path, sessionId));
      this.setStatus(profile.id, "connected");
      this.emit("ssh:agent-capabilities", { profileId: profile.id, capabilities });
      this.emit("ssh:deploy-progress", { profileId: profile.id, stage: "starting", percent: 100 });
      markReady(sessionId);
      // A repository failure belongs to that repository. Keep the negotiated
      // runtime alive so other repositories and an explicit retry can use it.
      await Promise.all([Promise.allSettled(openings), rpcRouter.replayQueue(profile.id)]);
      signal.throwIfAborted();
      if (!current() || connection.channels.agent !== channel) throw new Error("SSH deployment superseded");
    } catch (error) {
      let failure = error;
      if (channel) {
        try { await this.drainAgent(channel); }
        catch (cleanup) { failure = new AggregateError([error, cleanup], "SSH deployment and agent cleanup failed"); }
      }
      if (current() && (!connection.channels.agent || connection.channels.agent === channel)) {
        connection.channels.agent = undefined;
        connection.agentSessionId = undefined;
        capabilityNegotiator.clearCache(profile.id);
        if (connection.status === "deploying") this.setStatus(profile.id, "connected", failure instanceof Error ? failure.message : String(failure));
        const reason = error instanceof DeployError ? error.code : "AGENT_START_FAILED";
        this.emit("ssh:agent-unavailable", { profileId: profile.id, reason, message: failure instanceof Error ? failure.message : String(failure) });
      }
      throw failure;
    } finally { agentDeployer.removeListener("ssh:deploy-progress", progress); }
  }

  // ── Internal helpers ─────────────────────────────────────────

  /**
   * Build an SSH ConnectConfig for the given profile. Resolves auth
   * credentials (agent socket, key file, keychain password/passphrase)
   * and applies profile settings (compression, agent forwarding).
   *
   * Shared by `connect` and `testConnection` to avoid duplication.
   */
  private async buildConnectConfig(
    profile: SshProfile,
    options: { readyTimeout: number; jumpClients: Client[]; lifetime: AbortController },
    passwordOverride?: string,
  ): Promise<ConnectConfig> {
    options.lifetime.signal.throwIfAborted();
    const config: ConnectConfig = {
      host: profile.host,
      port: profile.port,
      username: profile.username,
      keepaliveInterval: profile.keepAliveInterval * 1000,
      keepaliveCountMax: 3,
      readyTimeout: options.readyTimeout,
      ...(profile.compression && {
        algorithms: { compress: ["zlib@openssh.com", "zlib", "none"] },
      }),
    };

    switch (profile.authMethod.type) {
      case "agent": {
        const agentSocket = this.resolveAgentSocket();
        if (!agentSocket) {
          throw new Error(
            "No SSH agent socket found. Ensure ssh-agent is running and SSH_AUTH_SOCK is set, " +
            "or configure a 1Password SSH agent or macOS Keychain agent.",
          );
        }
        config.agent = agentSocket;
        break;
      }
      case "key":
        config.privateKey = this.readPrivateKey(profile.authMethod.keyPath);
        break;
      case "key_passphrase": {
        config.privateKey = this.readPrivateKey(profile.authMethod.keyPath);
        const passphrase = await this.getPassphraseFromKeychain(profile.id);
        if (passphrase) {
          config.passphrase = passphrase;
        }
        break;
      }
      case "password": {
        const password = passwordOverride ?? await getPassword(SSH_KEYCHAIN_SERVICE, profile.id);
        console.log(`[buildConnectConfig] password auth: hasOverride=${!!passwordOverride}, keychainResult=${password ? 'found' : 'null'}, profileId=${profile.id}`);
        if (!password) {
          throw new Error(
            `No password found in the OS keychain for profile "${profile.label}" (${profile.host}). ` +
            `Store your password using the keychain service "${SSH_KEYCHAIN_SERVICE}" with account "${profile.id}".`,
          );
        }
        config.password = password;
        // Explicitly restrict to password auth only — prevents ssh2 from
        // trying the SSH agent even when SSH_AUTH_SOCK is set.
        config.authHandler = ["password"];
        break;
      }
    }

    options.lifetime.signal.throwIfAborted();
    if (profile.forwardAgent) {
      config.agentForward = true;
    }

    // If a jump host is configured, establish the tunnel and use it as the socket
    if (profile.jumpHost) {
      const channel = await this.connectViaJumpHost(
        profile.jumpHost,
        profile.host,
        profile.port,
        options.jumpClients,
        options.lifetime,
      );
      config.sock = channel;
      delete config.host;
      delete config.port;
    }

    return config;
  }

  /**
   * Parse a jump host spec string into its components.
   * Format: `[user@]host[:port]`
   */
  private parseJumpHostSpec(spec: string): { user?: string; host: string; port: number } {
    let user: string | undefined;
    let host: string;
    let port = 22;

    let remainder = spec.trim();

    // Extract user if present
    const atIndex = remainder.indexOf("@");
    if (atIndex !== -1) {
      user = remainder.slice(0, atIndex);
      remainder = remainder.slice(atIndex + 1);
    }

    // Extract port if present (handle IPv6 bracket notation)
    if (remainder.startsWith("[")) {
      // IPv6: [host]:port
      const closeBracket = remainder.indexOf("]");
      if (closeBracket !== -1) {
        host = remainder.slice(1, closeBracket);
        const afterBracket = remainder.slice(closeBracket + 1);
        if (afterBracket.startsWith(":")) {
          port = parseInt(afterBracket.slice(1), 10) || 22;
        }
      } else {
        host = remainder;
      }
    } else {
      // IPv4 or hostname: host:port
      const colonIndex = remainder.lastIndexOf(":");
      if (colonIndex !== -1) {
        const possiblePort = parseInt(remainder.slice(colonIndex + 1), 10);
        if (!isNaN(possiblePort) && possiblePort > 0 && possiblePort <= 65535) {
          host = remainder.slice(0, colonIndex);
          port = possiblePort;
        } else {
          host = remainder;
        }
      } else {
        host = remainder;
      }
    }

    return { user, host, port };
  }

  /**
   * Connect through one or more jump hosts (ProxyJump) to reach the target.
   * Supports multi-hop chains specified as comma-separated jump host specs.
   *
   * Each hop uses SSH agent auth by default (simplest for bastion hosts).
   * Returns a Duplex stream (ClientChannel) to use as the `sock` option
   * for the final SSH connection.
   */
  private async connectViaJumpHost(
    jumpHostSpec: string,
    targetHost: string,
    targetPort: number,
    jumpClients: Client[],
    lifetime: AbortController,
  ): Promise<ClientChannel> {
    const { signal } = lifetime;
    // Split comma-separated hops
    const hops = jumpHostSpec.split(",").map((s) => s.trim()).filter(Boolean);

    if (hops.length === 0) {
      throw new Error("Jump host spec is empty");
    }

    const agentSocket = this.resolveAgentSocket();

    try {
      signal?.throwIfAborted();
      // Connect to the first jump host directly
      const firstHop = this.parseJumpHostSpec(hops[0]);
      const firstClient = new Client();
      jumpClients.push(firstClient);

      const firstConfig: ConnectConfig = {
        host: firstHop.host,
        port: firstHop.port,
        username: firstHop.user ?? os.userInfo().username,
        readyTimeout: 10000,
        ...(agentSocket ? { agent: agentSocket } : {}),
      };

      await connectVerified(firstClient, firstConfig, firstHop.host, firstHop.port, lifetime, this.verifyHostKey);

      // For multi-hop: chain through intermediate hosts using forwardOut
      let currentClient = firstClient;

      for (let i = 1; i < hops.length; i++) {
        signal?.throwIfAborted();
        const nextHop = this.parseJumpHostSpec(hops[i]);

        // Open a TCP tunnel from the current hop to the next hop
        const channel = await new Promise<ClientChannel>((resolve, reject) => {
          currentClient.forwardOut(
            "127.0.0.1",
            0,
            nextHop.host,
            nextHop.port,
            (err, stream) => {
              if (err) reject(err);
              else if (signal.aborted) { stream.destroy(); reject(signal.reason); }
            else resolve(stream);
            },
          );
        });

        // Connect a new SSH client through the tunnel
        const nextClient = new Client();
        jumpClients.push(nextClient);

        const nextConfig: ConnectConfig = {
          sock: channel,
          username: nextHop.user ?? os.userInfo().username,
          readyTimeout: 10000,
          ...(agentSocket ? { agent: agentSocket } : {}),
        };

        signal?.throwIfAborted();
        await connectVerified(nextClient, nextConfig, nextHop.host, nextHop.port, lifetime, this.verifyHostKey);

        currentClient = nextClient;
      }

      signal?.throwIfAborted();
      // Open a TCP tunnel from the last hop to the final target
      const targetChannel = await new Promise<ClientChannel>((resolve, reject) => {
        currentClient.forwardOut(
          "127.0.0.1",
          0,
          targetHost,
          targetPort,
          (err, stream) => {
            if (err) reject(err);
            else if (signal.aborted) { stream.destroy(); reject(signal.reason); }
            else resolve(stream);
          },
        );
      });

      if (signal.aborted) { targetChannel.destroy(); signal.throwIfAborted(); }
      return targetChannel;
    } catch (err) {
      // Clean up any clients that were connected before the failure
      for (const client of [...jumpClients].reverse()) {
        try {
          client.end();
        } catch {
          // Client may already be closed
        }
      }
      throw err;
    }
  }

  /**
   * Categorize an SSH connection error into a user-friendly message.
   * Maps common error codes to descriptive messages.
   */
  private categorizeConnectionError(err: unknown): string {
    if (!(err instanceof Error)) {
      return String(err);
    }

    const message = err.message.toLowerCase();
    const code = (err as NodeJS.ErrnoException).code?.toLowerCase() ?? "";

    // Authentication failures
    if (
      message.includes("authentication") ||
      message.includes("auth") ||
      message.includes("permission denied") ||
      code === "err_generic_client" ||
      message.includes("all configured authentication methods failed")
    ) {
      return "Authentication failed";
    }

    // Host unreachable
    if (code === "econnrefused" || code === "ehostunreach" || message.includes("econnrefused") || message.includes("ehostunreach")) {
      return "Host unreachable";
    }

    // Timeout
    if (
      code === "etimedout" ||
      message.includes("timed out") ||
      message.includes("timeout") ||
      message.includes("etimedout")
    ) {
      return "Connection timed out";
    }

    // DNS resolution failure
    if (code === "enotfound" || message.includes("enotfound") || message.includes("getaddrinfo")) {
      return "DNS resolution failed";
    }

    // Fall back to the original error message
    return err.message;
  }

  /**
   * Read a private key file from disk. Resolves `~` to the user's home
   * directory and validates the file exists before reading.
   *
   * Throws a descriptive error if the file doesn't exist or isn't readable.
   */
  private readPrivateKey(keyPath: string): Buffer {
    const resolvedPath = keyPath.startsWith("~")
      ? path.join(os.homedir(), keyPath.slice(1))
      : keyPath;

    if (!fs.existsSync(resolvedPath)) {
      throw new Error(
        `SSH private key not found at "${resolvedPath}". ` +
        "Verify the key path in your connection profile and ensure the file exists.",
      );
    }

    try {
      return fs.readFileSync(resolvedPath);
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      throw new Error(
        `Failed to read SSH private key at "${resolvedPath}": ${message}. ` +
        "Check file permissions (should be 600) and ensure the file is readable.",
      );
    }
  }

  /**
   * Retrieve the passphrase for a profile's private key from the OS keychain.
   *
   * Returns null if no passphrase is stored for this profile.
   */
  private async getPassphraseFromKeychain(profileId: string): Promise<string | null> {
    return getPassword(SSH_KEYCHAIN_SERVICE, profileId);
  }

  /**
   * Resolve the SSH agent socket path. Checks standard `SSH_AUTH_SOCK`,
   * 1Password SSH agent, and macOS Keychain agent (the default on macOS).
   * Returns the first valid socket path found, or undefined if none available.
   */
  private resolveAgentSocket(): string | undefined {
    // Standard SSH agent socket (works on all platforms)
    const envSocket = process.env.SSH_AUTH_SOCK;
    if (envSocket && fs.existsSync(envSocket)) {
      return envSocket;
    }

    if (process.platform === "darwin") {
      // 1Password SSH agent (well-known path on macOS)
      const onePasswordSocket = path.join(
        os.homedir(),
        "Library",
        "Group Containers",
        "2BUA8C4S2C.com.1password",
        "t",
        "agent.sock",
      );
      if (fs.existsSync(onePasswordSocket)) {
        return onePasswordSocket;
      }

      // macOS Keychain agent — the default SSH_AUTH_SOCK on macOS typically
      // points to /private/tmp/com.apple.launchd.XXXXX/Listeners. If
      // SSH_AUTH_SOCK was set but the file didn't exist (checked above),
      // fall through. If it was unset, there's no socket to use.
      if (envSocket) {
        // The env var was set but the socket file doesn't exist — return
        // the path anyway as the socket may appear shortly (launchd lazy start).
        return envSocket;
      }
    }

    // Non-macOS: if SSH_AUTH_SOCK is set but the file doesn't exist,
    // still return it — the agent may use an abstract socket (Linux).
    if (envSocket) {
      return envSocket;
    }

    return undefined;
  }

  /**
   * Update the status of a connection and emit the status-changed event.
   */
  protected setStatus(profileId: string, status: SshConnectionStatus, error?: string): void {
    const conn = this.connections.get(profileId);
    if (conn) {
      conn.status = status;
      conn.error = error;
    }
    this.emit("ssh:status-changed", { profileId, status, error });
  }
}
