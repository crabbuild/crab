import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Client } from "ssh2";
import type { ClientChannel, SFTPWrapper, ConnectConfig } from "ssh2";
import { getPassword } from "./keychain";
import { connectVerified, type VerifyHostKey } from "./ssh-verified-connect";
import type { AgentDeployer } from "./ssh-deployer";
import { DeployError } from "./ssh-deployer";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { RpcRouter } from "./ssh-router";

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
  agentDeployer: AgentDeployer;
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

  private deployments = new Map<string, { client: Client; controller: AbortController; promise: Promise<void> }>();
  private openings = new Map<string, { controller: AbortController; promise: Promise<void> }>();
  private closing = new Map<string, Promise<void>>();
  private sftpOpen = new WeakMap<Client, Promise<SFTPWrapper>>();
  private repositories = new Map<string, Map<string, string>>();

  constructor(private readonly verifyHostKey: VerifyHostKey = async () => false) {
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
  async connect(profile: SshProfile): Promise<void> {
    const closing = this.closing.get(profile.id);
    if (closing) await closing;
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
    const connection: SshConnection = {
      profile, client, status: "connecting",
      channels: { terminals: new Map(), portForwards: new Map() }, jumpClients: [],
    };
    const controller = new AbortController();
    const entry = { controller, promise: Promise.resolve().then(() => this.establish(connection, controller.signal)) };
    this.openings.set(profile.id, entry);
    this.connections.set(profile.id, connection);
    this.setStatus(profile.id, "connecting");
    this.watchClient(connection, client);
    try { await entry.promise; }
    finally { if (this.openings.get(profile.id) === entry) this.openings.delete(profile.id); }
  }

  private async establish(connection: SshConnection, signal: AbortSignal): Promise<void> {
    const { client, profile, jumpClients } = connection;
    let authenticated = false;
    try {
      signal.throwIfAborted();
      const config = await this.buildConnectConfig(profile, { readyTimeout: profile.connectionTimeoutMs ?? 10000, jumpClients, signal });
      signal.throwIfAborted();
      if (this.connections.get(profile.id) !== connection || connection.status !== "connecting") throw new Error("SSH connection superseded");
      await connectVerified(client, config, profile.host, profile.port, this.verifyHostKey);
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
    if (opening) await opening.promise.catch(() => {});
    // The bootstrap caller owns its error, but disconnect must join its cleanup.
    if (deployment) await deployment.promise.catch(() => {});
    this.agentDeps?.capabilityNegotiator.clearCache(profileId);
    this.agentDeps?.rpcRouter.clearQueue(profileId);
    const { client, channels } = connection;
    let failure: unknown;
    try { if (channels.agent) await this.drainAgent(channels.agent); }
    catch (error) { failure = error; }
    for (const channel of channels.terminals.values()) channel.end();
    channels.sftp?.end();
    for (const channel of channels.portForwards.values()) channel.end();
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
    let jumpClients: Client[] = [];
    try {
      const config = await this.buildConnectConfig(profile, { readyTimeout: 5000, jumpClients }, passwordOverride);
      client = new Client();

      // Connect and wait for ready
      await connectVerified(client, config, profile.host, profile.port, this.verifyHostKey);

      // Run `echo ok` and verify output
      const output = await new Promise<string>((resolve, reject) => {
        client!.exec("echo ok", (err, stream) => {
          if (err) {
            reject(err);
            return;
          }
          let data = "";
          stream.on("data", (chunk: Buffer) => {
            data += chunk.toString();
          });
          stream.on("close", () => resolve(data));
          stream.stderr.on("data", () => {
            // Ignore stderr — some shells emit login messages
          });
        });
      });

      if (!output.includes("ok")) {
        return { ok: false, error: "Remote command did not return expected output" };
      }

      return { ok: true };
    } catch (err: unknown) {
      const error = this.categorizeConnectionError(err);
      return { ok: false, error };
    } finally {
      if (client) {
        try {
          client.end();
        } catch {
          // Client may already be closed
        }
      }
      // Clean up jump clients used for the test connection
      for (const jc of jumpClients.reverse()) {
        try {
          jc.end();
        } catch {
          // Jump client may already be closed
        }
      }
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
        const config = await this.buildConnectConfig(conn.profile, { readyTimeout: 10000, jumpClients: currentState.jumpClients, signal: currentState.controller.signal });
        if (this.reconnectionState.get(profileId) !== currentState) { client.destroy(); return; }

        await connectVerified(client, config, conn.profile.host, conn.profile.port, this.verifyHostKey);

        // Cancellation revokes an unadopted transport. A late authentication
        // reply must not restart an agent after the user disconnected.
        if (this.reconnectionState.get(profileId) !== currentState || conn.status !== "reconnecting") { client.destroy(); return; }
        currentState.client = undefined;
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

  configureAgentDeployment(deps: AgentDeployDeps): void {
    if (this.agentDeps) throw new Error("SSH agent deployment already configured");
    this.agentDeps = deps;
  }

  agentCapabilities(profileId: string) {
    return this.agentDeps?.capabilityNegotiator.getCached(profileId) ?? null;
  }

  async ensureAgent(profileId: string): Promise<void> {
    const connection = this.connections.get(profileId);
    if (!connection || !["connected", "deploying"].includes(connection.status)) throw new Error("SSH connection is not ready for agent deployment");
    if (!this.agentDeps) throw new Error("SSH agent deployment is not initialized");
    const pending = this.deployments.get(profileId);
    if (pending?.client === connection.client) return pending.promise;
    if (connection.channels.agent && this.agentCapabilities(profileId)) return;
    const controller = new AbortController();
    // Queue the body after publishing the owner, before status listeners run.
    const entry = { client: connection.client, controller, promise: Promise.resolve().then(() => this.deployAgent(connection, controller.signal)) };
    this.deployments.set(profileId, entry);
    try { await entry.promise; } finally { if (this.deployments.get(profileId) === entry) this.deployments.delete(profileId); }
  }

  async openRepository(profileId: string, remotePath: string): Promise<Record<string, unknown>> {
    if (!path.posix.isAbsolute(remotePath) || remotePath.includes("\0")) throw new Error("Choose an absolute remote repository path");
    if (this.getStatus(profileId) !== "connected" || !this.agentCapabilities(profileId)) throw new Error("Deploy the SSH agent before opening a repository");
    const connection = this.connections.get(profileId)!;
    const client = connection.client;
    const repoId = `ssh://${profileId}${remotePath}`;
    const result = await this.agentDeps!.rpcRouter.sendToRemoteAgent(profileId, "open_local", { repo_id: repoId, path: remotePath });
    if (this.connections.get(profileId) !== connection || connection.client !== client || connection.status !== "connected") throw new Error("SSH repository open was interrupted");
    if (!result || typeof result !== "object" || !("repo_id" in result) || result.repo_id !== repoId) throw new Error("SSH agent returned an invalid repository handle");
    let repositories = this.repositories.get(profileId);
    if (!repositories) { repositories = new Map(); this.repositories.set(profileId, repositories); }
    repositories.set(repoId, remotePath);
    return { ...result, repo_id: repoId, url: repoId, is_remote: true };
  }

  forgetRepository(profileId: string, repoId: string): void {
    this.repositories.get(profileId)?.delete(repoId);
  }

  private async deployAgent(connection: SshConnection, signal: AbortSignal): Promise<void> {
    const { agentDeployer, capabilityNegotiator, rpcRouter } = this.agentDeps!;
    const { client, profile } = connection;
    const current = () => this.connections.get(profile.id) === connection && connection.client === client;
    let channel: ClientChannel | undefined;
    try {
      signal.throwIfAborted();
      this.setStatus(profile.id, "deploying");
      channel = await agentDeployer.ensureAgent(client, progress => {
        if (current() && connection.status === "deploying") this.emit("ssh:deploy-progress", { profileId: profile.id, ...progress });
      }, signal);
      signal.throwIfAborted();
      if (!current()) throw new Error("SSH deployment superseded");
      channel.stderr.resume();
      connection.channels.agent = channel;
      rpcRouter.attachChannel(profile.id, channel);
      channel.once("close", () => {
        if (current() && connection.channels.agent === channel) {
          connection.channels.agent = undefined;
          capabilityNegotiator.clearCache(profile.id);
          if (connection.status === "connected") this.emit("ssh:agent-unavailable", { profileId: profile.id, reason: "AGENT_EXITED", message: "SSH agent exited" });
        }
      });
      const capabilities = await capabilityNegotiator.negotiate(profile.id);
      for (const [repoId, remotePath] of this.repositories.get(profile.id) ?? []) {
        await rpcRouter.sendToRemoteAgent(profile.id, "open_local", { repo_id: repoId, path: remotePath });
        if (!this.repositories.get(profile.id)?.has(repoId)) await rpcRouter.sendToRemoteAgent(profile.id, "close", { repo_id: repoId });
      }
      await rpcRouter.replayQueue(profile.id);
      signal.throwIfAborted();
      if (!current()) throw new Error("SSH deployment superseded");
      this.setStatus(profile.id, "connected");
      this.emit("ssh:agent-capabilities", { profileId: profile.id, capabilities });
    } catch (error) {
      let failure = error;
      if (channel) {
        try { await this.drainAgent(channel); }
        catch (cleanup) { failure = new AggregateError([error, cleanup], "SSH deployment and agent cleanup failed"); }
      }
      if (current()) {
        connection.channels.agent = undefined;
        capabilityNegotiator.clearCache(profile.id);
        if (connection.status === "deploying") this.setStatus(profile.id, "connected", failure instanceof Error ? failure.message : String(failure));
        const reason = error instanceof DeployError ? error.code : "AGENT_START_FAILED";
        this.emit("ssh:agent-unavailable", { profileId: profile.id, reason, message: failure instanceof Error ? failure.message : String(failure) });
      }
      throw failure;
    }
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
    options: { readyTimeout: number; jumpClients: Client[]; signal?: AbortSignal },
    passwordOverride?: string,
  ): Promise<ConnectConfig> {
    options.signal?.throwIfAborted();
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

    options.signal?.throwIfAborted();
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
        options.signal,
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
    signal?: AbortSignal,
  ): Promise<ClientChannel> {
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

      await connectVerified(firstClient, firstConfig, firstHop.host, firstHop.port, this.verifyHostKey);

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
        await connectVerified(nextClient, nextConfig, nextHop.host, nextHop.port, this.verifyHostKey);

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
            else resolve(stream);
          },
        );
      });

      signal?.throwIfAborted();
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
