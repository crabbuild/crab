import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Client } from "ssh2";
import type { ClientChannel, SFTPWrapper, ConnectConfig } from "ssh2";
import { getPassword } from "./keychain";
import type { AgentDeployer, DeployProgress } from "./ssh-deployer";
import { DeployError } from "./ssh-deployer";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { RpcRouter } from "./ssh-router";
import { HostKeyError, type HostKeyVerifier } from "./ssh-known-hosts";

// ── Connection status ────────────────────────────────────────────

export type SshConnectionStatus =
  | "disconnected"
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
  /** Timeout in ms for individual RPC operations (default: 30000). */
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

interface OwnedConnection extends SshConnection {
  lifetime: AbortController;
}

// ── Event types ──────────────────────────────────────────────────

export interface SshSessionManagerEvents {
  "ssh:status-changed": (payload: { profileId: string; status: SshConnectionStatus; error?: string }) => void;
  "ssh:server-info": (payload: { profileId: string; info: SshServerInfo }) => void;
  "ssh:reconnecting": (payload: { profileId: string; attempt: number; maxAttempts: number }) => void;
}

// ── Constants ────────────────────────────────────────────────────

const SSH_KEYCHAIN_SERVICE = "crab-desktop-ssh";
const RECONNECTION_DELAYS_MS = [1000, 2000, 4000, 8000, 16_000];

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
  private connections = new Map<string, OwnedConnection>();
  private reconnectionState = new Map<string, {
    timer: NodeJS.Timeout | null;
    attempt: number;
    deadline: NodeJS.Timeout;
    profile: SshProfile;
  }>();
  private _suspendedAt: number | null = null;
  private agentDeps: AgentDeployDeps | null = null;

  constructor(private readonly verifyHostKey: HostKeyVerifier) {
    super();
    this.setupPowerMonitor();
  }

  /** Wires the agent owner after constructing its router and capability negotiator. */
  setAgentDependencies(deps: AgentDeployDeps): void {
    this.agentDeps = deps;
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

  /** Connects and completes the configured agent handshake before publishing readiness. */
  async connect(profile: SshProfile): Promise<void> {
    this.clearReconnection(profile.id);
    this.agentDeps?.rpcRouter.clearQueue(profile.id);
    const connection = await this.openConnection(profile, "connecting");
    this.publishConnected(connection);
  }

  /** Cancels opening work and retires this profile's channels and queued reads. */
  async disconnect(profileId: string, error?: string): Promise<void> {
    this.clearReconnection(profileId);
    this.agentDeps?.rpcRouter.clearQueue(profileId);
    const connection = this.connections.get(profileId);
    if (!connection) return;
    const changed = connection.status !== "disconnected";
    connection.lifetime.abort(new Error("SSH connection disconnected"));
    this.clearAgent(connection);
    if (changed) this.setStatus(profileId, "disconnected", error);
    this.closeConnection(connection);
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
      const config = await this.buildConnectConfig(profile, { readyTimeout: 5000 }, jumpClients, lifetime, passwordOverride);
      client = new Client();

      await this.connectClient(client, config, profile.host, profile.port, lifetime);

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
      if (client) this.closeClient(client);
      for (const jumpClient of jumpClients.reverse()) this.closeClient(jumpClient);
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

  /** Cancels a reconnect, including a handshake or deployment already in flight. */
  cancelReconnection(profileId: string): void {
    if (this.reconnectionState.has(profileId)) void this.disconnect(profileId);
  }

  private clearReconnection(profileId: string): void {
    const state = this.reconnectionState.get(profileId);
    if (!state) return;
    clearTimeout(state.timer ?? undefined);
    clearTimeout(state.deadline);
    this.reconnectionState.delete(profileId);
  }

  private attemptReconnection(profileId: string): void {
    if (this.reconnectionState.has(profileId)) return;
    const connection = this.connections.get(profileId);
    if (!connection) return;
    const deadline = setTimeout(() => {
      void this.disconnect(profileId, "Reconnection timed out after 60s");
    }, 60_000);
    this.reconnectionState.set(profileId, { timer: null, attempt: 0, deadline, profile: connection.profile });
    connection.lifetime.abort(new Error("SSH connection lost; reconnecting"));
    this.clearAgent(connection);
    this.setStatus(profileId, "reconnecting");
    this.closeConnection(connection);
    this.scheduleReconnectionAttempt(profileId);
  }

  private scheduleReconnectionAttempt(profileId: string): void {
    const state = this.reconnectionState.get(profileId);
    if (!state) return;
    const delay = RECONNECTION_DELAYS_MS[state.attempt];
    if (delay === undefined) return;
    this.emit("ssh:reconnecting", { profileId, attempt: state.attempt + 1, maxAttempts: RECONNECTION_DELAYS_MS.length });
    state.timer = setTimeout(async () => {
      if (this.reconnectionState.get(profileId) !== state) return;
      state.timer = null;
      try {
        const connection = await this.openConnection(state.profile, "reconnecting");
        if (this.reconnectionState.get(profileId) !== state) return;
        this.clearReconnection(profileId);
        this.publishConnected(connection);
      } catch {
        if (this.reconnectionState.get(profileId) !== state) return;
        state.attempt += 1;
        this.scheduleReconnectionAttempt(profileId);
      }
    }, delay);
  }

  private async openConnection(profile: SshProfile, status: "connecting" | "reconnecting"): Promise<OwnedConnection> {
    const previous = this.connections.get(profile.id);
    if (previous) {
      if (previous.profile.host !== profile.host || previous.profile.port !== profile.port || previous.profile.username !== profile.username) {
        this.agentDeps?.rpcRouter.forgetRepositories(profile.id);
      }
      previous.lifetime.abort(new Error("SSH connection replaced"));
      this.closeConnection(previous);
    }
    const connection: OwnedConnection = {
      profile, client: new Client(), status, lifetime: new AbortController(),
      channels: { terminals: new Map(), portForwards: new Map() }, jumpClients: [],
    };
    this.connections.set(profile.id, connection);
    // A trust failure is terminal, including during rekey or a reconnect hop.
    // Retrying it would silently turn one rejection into repeated approval prompts.
    connection.lifetime.signal.addEventListener("abort", () => {
      const reason: unknown = connection.lifetime.signal.reason;
      if (!(reason instanceof HostKeyError) || this.connections.get(profile.id) !== connection) return;
      this.clearReconnection(profile.id);
      this.clearAgent(connection);
      this.agentDeps?.rpcRouter.clearQueue(profile.id);
      this.setStatus(profile.id, "error", reason.message);
      this.closeConnection(connection);
    }, { once: true });
    this.clearAgent(connection);
    this.setStatus(profile.id, status);
    const lost = (reason: Error) => {
      if (!this.isCurrent(connection)) return;
      const established = connection.status === "connected" || connection.status === "deploying";
      connection.lifetime.abort(reason);
      this.clearAgent(connection);
      if (this.reconnectionState.has(profile.id)) {
        this.setStatus(profile.id, "reconnecting");
      } else if (established && profile.autoReconnect) {
        this.attemptReconnection(profile.id);
        return;
      } else if (established) {
        this.agentDeps?.rpcRouter.clearQueue(profile.id);
        this.setStatus(profile.id, "disconnected", reason.message);
      }
      this.closeConnection(connection);
    };
    connection.client.on("error", lost);
    connection.client.on("end", () => lost(new Error("SSH connection ended")));
    connection.client.on("close", () => lost(new Error("SSH connection closed")));
    try {
      const configuration = this.buildConnectConfig(profile, { readyTimeout: profile.connectionTimeoutMs ?? 10_000 }, connection.jumpClients, connection.lifetime);
      const config = await this.owned(connection, configuration);
      this.assertCurrent(connection);
      await this.connectClient(connection.client, config, profile.host, profile.port, connection.lifetime);
      this.assertCurrent(connection);
      if (profile.autoDeployAgent) await this.attachAgent(connection);
      return connection;
    } catch (reason: unknown) {
      if (this.connections.get(profile.id) === connection && connection.status !== "disconnected" && !this.reconnectionState.has(profile.id)) {
        this.clearAgent(connection);
        this.agentDeps?.rpcRouter.clearQueue(profile.id);
        this.setStatus(profile.id, "error", reason instanceof Error ? reason.message : String(reason));
      }
      connection.lifetime.abort(reason);
      this.closeConnection(connection);
      throw reason;
    }
  }

  private isCurrent(connection: OwnedConnection): boolean {
    return this.connections.get(connection.profile.id) === connection && !connection.lifetime.signal.aborted;
  }

  private assertCurrent(connection: OwnedConnection): void {
    connection.lifetime.signal.throwIfAborted();
    if (this.connections.get(connection.profile.id) !== connection) throw new Error("SSH connection replaced");
  }

  private async owned<T>(connection: OwnedConnection, pending: Promise<T>): Promise<T> {
    const signal = connection.lifetime.signal;
    let cancel!: () => void;
    const aborted = new Promise<never>((_resolve, reject) => {
      cancel = () => reject(signal.reason);
      signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) cancel();
    });
    try { return await Promise.race([pending, aborted]); }
    finally { signal.removeEventListener("abort", cancel); }
  }

  private connectClient(client: Client, config: ConnectConfig, hostname: string, port: number, lifetime: AbortController): Promise<void> {
    const { signal } = lifetime;
    return new Promise((resolve, reject) => {
      let pending = true;
      let remaining = config.readyTimeout ?? 10_000;
      const bounded = remaining > 0;
      let started = 0;
      let timer: NodeJS.Timeout | undefined;
      const pause = () => {
        if (timer === undefined) return;
        clearTimeout(timer); timer = undefined;
        remaining -= Date.now() - started;
      };
      const resume = () => {
        if (!pending || signal.aborted || !bounded) return;
        started = Date.now();
        timer = setTimeout(() => { failed(new Error("SSH connection handshake timed out")); client.destroy(); }, Math.max(0, remaining));
      };
      const cleanup = () => {
        pending = false; pause();
        client.removeListener("ready", ready); client.removeListener("end", closed); client.removeListener("close", closed);
        signal.removeEventListener("abort", cancel);
      };
      const failed = (reason: unknown) => { cleanup(); reject(reason); };
      const ready = () => { cleanup(); resolve(); };
      const closed = () => failed(new Error("SSH connection closed before ready"));
      const cancel = () => { failed(signal.reason); client.destroy(); };
      client.once("ready", ready); client.once("end", closed); client.once("close", closed);
      // Keep an error listener until physical close, including on retired jump
      // clients. A late error must settle its owner without becoming unhandled.
      client.on("error", failed);
      client.once("close", () => client.removeListener("error", failed));
      signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) { cancel(); return; }
      resume();
      try {
        client.connect({
          ...config, readyTimeout: 0,
          // ssh2's deadline includes user interaction. Only the network handshake
          // consumes our budget; disconnect/replacement owns an unanswered prompt.
          hostVerifier: (key: Buffer, callback: (accepted: boolean) => void) => {
            pause();
            void this.verifyHostKey(hostname, port, key, signal).then(() => {
              if (signal.aborted) return;
              resume(); callback(true);
            }, (cause: unknown) => {
              if (signal.aborted) return;
              const reason = cause instanceof HostKeyError ? cause : new HostKeyError("SSH_HOST_TRUST_FAILED", "SSH host key verification failed", { cause });
              lifetime.abort(reason); callback(false);
            });
          },
        });
      } catch (reason: unknown) { failed(reason); }
    });
  }

  private clearAgent(connection: OwnedConnection): void {
    if (this.connections.get(connection.profile.id) !== connection) return;
    this.agentDeps?.rpcRouter.detachChannel(connection.profile.id);
    this.agentDeps?.capabilityNegotiator.clearCache(connection.profile.id);
  }

  private closeConnection(connection: OwnedConnection): void {
    const { channels } = connection;
    const streams = [channels.agent, ...channels.terminals.values(), ...channels.portForwards.values()];
    channels.agent = undefined; channels.terminals.clear(); channels.portForwards.clear();
    for (const stream of streams) {
      try { stream?.end(); } catch { /* Already closed. */ }
      try { stream?.destroy(); } catch { /* Already closed. */ }
    }
    try { channels.sftp?.end(); } catch { /* Already closed. */ }
    channels.sftp = undefined;
    this.closeClient(connection.client);
    for (const client of connection.jumpClients.splice(0).reverse()) {
      this.closeClient(client);
    }
  }

  private closeClient(client: Client): void {
    try { client.end(); } catch { /* Already closed. */ }
    try { client.destroy(); } catch { /* Already closed. */ }
  }

  private publishConnected(connection: OwnedConnection): void {
    this.assertCurrent(connection);
    const profileId = connection.profile.id;
    this.setStatus(profileId, "connected");
    if (!this.isCurrent(connection)) return;
    if (!connection.channels.agent) {
      this.agentDeps?.rpcRouter.clearQueue(profileId);
      return;
    }
    const capabilities = this.agentDeps?.capabilityNegotiator.getCached(profileId);
    if (capabilities) this.emit("ssh:agent-capabilities", { profileId, capabilities });
    void this.agentDeps?.rpcRouter.replayQueue(profileId);
  }

  /** Deploys and attaches the agent through the same owner used by initial connect and reconnect. */
  async deployAgent(profileId: string): Promise<void> {
    const connection = this.connections.get(profileId);
    if (!connection || connection.status !== "connected") throw new Error("SSH connection is not ready for agent deployment");
    if (connection.channels.agent) return;
    try {
      await this.attachAgent(connection);
      this.publishConnected(connection);
    } catch (reason: unknown) {
      if (this.connections.get(profileId) === connection && this.getStatus(profileId) !== "disconnected" && !this.reconnectionState.has(profileId)) {
        this.clearAgent(connection);
        this.setStatus(profileId, "error", reason instanceof Error ? reason.message : String(reason));
      }
      connection.lifetime.abort(reason);
      this.closeConnection(connection);
      throw reason;
    }
  }

  private async attachAgent(connection: OwnedConnection): Promise<void> {
    this.assertCurrent(connection);
    const deps = this.agentDeps;
    if (!deps) throw new Error("SSH agent dependencies are not configured");
    const profileId = connection.profile.id;
    const agentDeployer = deps.createDeployer();
    const progress = (value: DeployProgress) => {
      if (this.isCurrent(connection)) this.emit("ssh:deploy-progress", { profileId, ...value });
    };
    agentDeployer.on("ssh:deploy-progress", progress);
    this.setStatus(profileId, "deploying");
    try {
      const starting = agentDeployer.ensureAgent(connection.client, connection.lifetime.signal).then(channel => {
        // Deployment can finish after cancellation. It must retire its own
        // stream rather than installing it into a newer connection's slot.
        if (!this.isCurrent(connection)) { channel.destroy(); connection.lifetime.signal.throwIfAborted(); throw new Error("SSH connection replaced"); }
        return channel;
      });
      const channel = await this.owned(connection, starting);
      this.assertCurrent(connection);
      connection.channels.agent = channel;
      deps.rpcRouter.attachChannel(profileId, channel);
      const lost = () => {
        if (!this.isCurrent(connection) || connection.channels.agent !== channel) return;
        if (connection.profile.autoReconnect) this.attemptReconnection(profileId);
        else void this.disconnect(profileId);
      };
      channel.once("end", lost); channel.once("close", lost); channel.once("error", lost);
      await this.owned(connection, deps.capabilityNegotiator.negotiate(profileId));
      this.assertCurrent(connection);
      await this.owned(connection, deps.rpcRouter.restoreRepositories(profileId));
      this.assertCurrent(connection);
      this.emit("ssh:deploy-progress", { profileId, stage: "starting", percent: 100 });
    } catch (reason: unknown) {
      if (this.isCurrent(connection)) this.emit("ssh:agent-unavailable", { profileId, reason: reason instanceof DeployError ? reason.code : "UNKNOWN", message: reason instanceof Error ? reason.message : String(reason) });
      throw reason;
    } finally {
      agentDeployer.removeListener("ssh:deploy-progress", progress);
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
    options: { readyTimeout: number },
    jumpClients: Client[],
    lifetime: AbortController,
    passwordOverride?: string,
  ): Promise<ConnectConfig> {
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

    if (profile.forwardAgent) {
      config.agentForward = true;
    }

    // If a jump host is configured, establish the tunnel and use it as the socket
    if (profile.jumpHost) {
      const channel = await this.connectViaJumpHost(
        profile.jumpHost,
        profile.host,
        profile.port,
        jumpClients,
        lifetime,
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

      await this.connectClient(firstClient, firstConfig, firstHop.host, firstHop.port, lifetime);
      signal.throwIfAborted();

      // For multi-hop: chain through intermediate hosts using forwardOut
      let currentClient = firstClient;

      for (let i = 1; i < hops.length; i++) {
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

        signal.throwIfAborted();
        await this.connectClient(nextClient, nextConfig, nextHop.host, nextHop.port, lifetime);
        signal.throwIfAborted();

        currentClient = nextClient;
      }

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
      for (const client of jumpClients.reverse()) {
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
