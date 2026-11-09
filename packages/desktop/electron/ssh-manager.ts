import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Client } from "ssh2";
import type { ClientChannel, SFTPWrapper, ConnectConfig } from "ssh2";
import { getPassword } from "./keychain";
import { connectVerified, type VerifyHostKey } from "./ssh-verified-connect";
import type { AgentDeployer, DeployProgress } from "./ssh-deployer";
import { DeployError } from "./ssh-deployer";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { RpcRouter } from "./ssh-router";

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
    pendingCalls: Array<{ resolve: Function; reject: Function; method: string; params: unknown }>;
  }>();
  private pendingJumpClients: Client[] = [];
  private _suspendedAt: number | null = null;
  private agentDeps: AgentDeployDeps | null = null;

  constructor(agentDeps?: AgentDeployDeps, private readonly verifyHostKey: VerifyHostKey = async () => false) {
    super();
    this.agentDeps = agentDeps ?? null;
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
    // Idempotent: if already connected, disconnect first
    const existing = this.connections.get(profile.id);
    if (existing && existing.status !== "disconnected" && existing.status !== "error") {
      await this.disconnect(profile.id);
    }

    const client = new Client();

    const connection: SshConnection = {
      profile,
      client,
      status: "connecting",
      channels: {
        terminals: new Map<string, ClientChannel>(),
        portForwards: new Map<string, ClientChannel>(),
      },
      jumpClients: [],
    };

    this.connections.set(profile.id, connection);
    this.setStatus(profile.id, "connecting");

    const config = await this.buildConnectConfig(profile, {
      readyTimeout: profile.connectionTimeoutMs ?? 10000,
    });

    // Track jump clients for cleanup on disconnect
    if (profile.jumpHost && config.sock) {
      connection.jumpClients = this.pendingJumpClients.splice(0);
    }

    return new Promise<void>((resolve, reject) => {
      client.on("ready", () => {
        this.setStatus(profile.id, "connected");

        // Attempt agent deployment if enabled and dependencies are available
        if (profile.autoDeployAgent && this.agentDeps) {
          this.deployAgent(profile.id, client).catch(() => {
            // Errors are handled inside deployAgent — graceful degradation
          });
        }

        resolve();
      });

      client.on("error", (err: Error) => {
        this.setStatus(profile.id, "error", err.message);
        reject(err);
      });

      client.on("end", () => {
        const conn = this.connections.get(profile.id);
        if (conn && conn.status !== "disconnected" && conn.status !== "error") {
          if (profile.autoReconnect && conn.status === "connected") {
            this.attemptReconnection(profile.id);
          } else {
            this.setStatus(profile.id, "disconnected");
          }
        }
      });

      client.on("close", () => {
        const conn = this.connections.get(profile.id);
        if (conn && conn.status !== "error" && conn.status !== "disconnected") {
          if (profile.autoReconnect && conn.status === "connected") {
            this.attemptReconnection(profile.id);
          } else if (conn.status !== "reconnecting") {
            this.setStatus(profile.id, "disconnected");
          }
        }
      });

      void connectVerified(client, config, profile.host, profile.port, this.verifyHostKey).catch(error => {
        this.setStatus(profile.id, "error", error.message);
        reject(error);
      });
    });
  }

  /**
   * Gracefully disconnect: close all channels, end the SSH session,
   * and update status to "disconnected".
   *
   * Safe to call multiple times — returns early if the connection
   * doesn't exist or is already disconnected.
   */
  async disconnect(profileId: string): Promise<void> {
    const connection = this.connections.get(profileId);
    if (!connection || connection.status === "disconnected") {
      return;
    }

    // Cancel any active reconnection before disconnecting
    this.cancelReconnection(profileId);

    const { client, channels } = connection;

    // Close the agent channel
    if (channels.agent) {
      try {
        channels.agent.end();
      } catch {
        // Channel may already be closed
      }
    }

    // Close all terminal channels
    for (const [, channel] of channels.terminals) {
      try {
        channel.end();
      } catch {
        // Channel may already be closed
      }
    }

    // Close the SFTP wrapper
    if (channels.sftp) {
      try {
        channels.sftp.end();
      } catch {
        // SFTP session may already be closed
      }
    }

    // Close all port forward channels
    for (const [, channel] of channels.portForwards) {
      try {
        channel.end();
      } catch {
        // Channel may already be closed
      }
    }

    // End the SSH client connection
    try {
      client.end();
    } catch {
      // Client may already be closed
    }

    // Close all jump host clients (in reverse order for multi-hop)
    if (connection.jumpClients) {
      for (const jumpClient of [...connection.jumpClients].reverse()) {
        try {
          jumpClient.end();
        } catch {
          // Jump client may already be closed
        }
      }
      connection.jumpClients = [];
    }

    // Clear channel references
    channels.agent = undefined;
    channels.terminals.clear();
    channels.sftp = undefined;
    channels.portForwards.clear();

    this.setStatus(profileId, "disconnected");
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
      const config = await this.buildConnectConfig(profile, { readyTimeout: 5000 }, passwordOverride);
      // Capture any jump clients created during buildConnectConfig
      jumpClients = this.pendingJumpClients.splice(0);
      client = new Client();

      // Connect and wait for ready
      await new Promise<void>((resolve, reject) => {
        client!.on("ready", () => resolve());
        client!.on("error", (err: Error) => reject(err));
        void connectVerified(client!, config, profile.host, profile.port, this.verifyHostKey).catch(reject);
      });

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
   * Queue an RPC call to be replayed after reconnection succeeds.
   * Returns a Promise that resolves/rejects when the call is replayed or dropped.
   * Max 50 pending calls — oldest are dropped on overflow.
   */
  queueCall(profileId: string, method: string, params: unknown): Promise<unknown> {
    const state = this.reconnectionState.get(profileId);
    if (!state) {
      return Promise.reject(new Error("No active reconnection for this profile"));
    }

    return new Promise((resolve, reject) => {
      // Drop oldest if at capacity
      if (state.pendingCalls.length >= 50) {
        const dropped = state.pendingCalls.shift();
        if (dropped) {
          dropped.reject(new Error("Call dropped: reconnection queue overflow"));
        }
      }

      state.pendingCalls.push({ resolve, reject, method, params });
    });
  }

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

    for (const call of state.pendingCalls) {
      call.reject(new Error("Reconnection cancelled"));
    }

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

    this.reconnectionState.set(profileId, {
      timer: null,
      attempt: 0,
      startedAt: Date.now(),
      pendingCalls: [],
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
        this.reconnectionState.delete(profileId);
        return;
      }

      try {
        // Attempt to reconnect using the stored profile
        const client = new Client();
        const config = await this.buildConnectConfig(conn.profile, { readyTimeout: 10000 });

        await new Promise<void>((resolve, reject) => {
          client.on("ready", () => resolve());
          client.on("error", (err: Error) => reject(err));
          void connectVerified(client, config, conn.profile.host, conn.profile.port, this.verifyHostKey).catch(reject);
        });

        // Success: update the connection with the new client
        conn.client = client;
        conn.channels = {
          terminals: new Map(),
          portForwards: new Map(),
        };

        // Track jump clients from the new connection
        if (conn.profile.jumpHost && this.pendingJumpClients.length > 0) {
          // Close old jump clients first
          for (const oldJumpClient of [...conn.jumpClients].reverse()) {
            try {
              oldJumpClient.end();
            } catch {
              // Already closed
            }
          }
          conn.jumpClients = this.pendingJumpClients.splice(0);
        }

        // Re-attach event handlers for the new client
        client.on("end", () => {
          const c = this.connections.get(profileId);
          if (c && c.status !== "disconnected" && c.status !== "error") {
            if (conn.profile.autoReconnect && c.status === "connected") {
              this.attemptReconnection(profileId);
            } else {
              this.setStatus(profileId, "disconnected");
            }
          }
        });

        client.on("close", () => {
          const c = this.connections.get(profileId);
          if (c && c.status !== "error" && c.status !== "disconnected") {
            if (conn.profile.autoReconnect && c.status === "connected") {
              this.attemptReconnection(profileId);
            } else if (c.status !== "reconnecting") {
              this.setStatus(profileId, "disconnected");
            }
          }
        });

        client.on("error", (err: Error) => {
          this.setStatus(profileId, "error", err.message);
        });

        // Replay pending calls
        const pendingCalls = currentState.pendingCalls.splice(0);
        this.reconnectionState.delete(profileId);
        this.setStatus(profileId, "connected");

        for (const call of pendingCalls) {
          call.resolve({ method: call.method, params: call.params, replayed: true });
        }
      } catch {
        // Failure: check if 60s total has elapsed
        const elapsed = Date.now() - currentState.startedAt;
        if (elapsed >= 60000) {
          // Give up
          const pendingCalls = currentState.pendingCalls.splice(0);
          this.reconnectionState.delete(profileId);
          this.setStatus(profileId, "disconnected");
          this.emit("ssh:status-changed", { profileId, status: "disconnected", error: "Reconnection timed out after 60s" });

          for (const call of pendingCalls) {
            call.reject(new Error("Reconnection failed: timed out after 60s"));
          }
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
  private async deployAgent(profileId: string, client: Client): Promise<void> {
    const { agentDeployer, capabilityNegotiator, rpcRouter } = this.agentDeps!;

    // Forward deploy progress events from AgentDeployer to this manager
    const progressHandler = (progress: DeployProgress) => {
      this.emit("ssh:deploy-progress", { profileId, ...progress });
    };
    agentDeployer.on("ssh:deploy-progress", progressHandler);

    try {
      this.setStatus(profileId, "deploying");

      const channel = await agentDeployer.ensureAgent(client);

      // Attach the RPC channel so the router can communicate with the agent
      rpcRouter.attachChannel(profileId, channel);

      // Store the agent channel on the connection for lifecycle management
      const conn = this.connections.get(profileId);
      if (conn) {
        conn.channels.agent = channel;
      }

      // Negotiate capabilities with the remote agent
      const capabilities = await capabilityNegotiator.negotiate(profileId);

      this.setStatus(profileId, "connected");
      this.emit("ssh:agent-capabilities", { profileId, capabilities });
    } catch (err: unknown) {
      // Graceful degradation — SFTP-only mode
      const reason = err instanceof DeployError ? err.code : "UNKNOWN";
      const message = err instanceof Error ? err.message : String(err);

      this.setStatus(profileId, "connected");
      this.emit("ssh:agent-unavailable", { profileId, reason, message });
    } finally {
      agentDeployer.removeListener("ssh:deploy-progress", progressHandler);
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
  ): Promise<ClientChannel> {
    // Split comma-separated hops
    const hops = jumpHostSpec.split(",").map((s) => s.trim()).filter(Boolean);

    if (hops.length === 0) {
      throw new Error("Jump host spec is empty");
    }

    const jumpClients: Client[] = [];
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

      await new Promise<void>((resolve, reject) => {
        firstClient.on("ready", () => resolve());
        firstClient.on("error", (err: Error) => reject(err));
        void connectVerified(firstClient, firstConfig, firstHop.host, firstHop.port, this.verifyHostKey).catch(reject);
      });

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

        await new Promise<void>((resolve, reject) => {
          nextClient.on("ready", () => resolve());
          nextClient.on("error", (err: Error) => reject(err));
          void connectVerified(nextClient, nextConfig, nextHop.host, nextHop.port, this.verifyHostKey).catch(reject);
        });

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
            else resolve(stream);
          },
        );
      });

      // Store jump clients for later cleanup
      this.pendingJumpClients = jumpClients;

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
