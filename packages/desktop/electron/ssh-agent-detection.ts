/**
 * Remote Agent CLI Detection — detects available agent CLIs (claude, codex)
 * on a remote server after SSH connection is established.
 *
 * Runs detection by sending an `exec_command` RPC to the remote agent
 * (preferred since the agent is already running), or falls back to
 * running `which` commands over an SSH exec channel.
 *
 * Emits `ssh:agents-detected` when detection completes so the renderer
 * can update the connection card and task creation dialog.
 */

import { EventEmitter } from "node:events";
import type { SshSessionManager, SshConnection } from "./ssh-manager";
import type { RpcRouter } from "./ssh-router";

// ── Types ────────────────────────────────────────────────────────

export interface DetectedAgent {
  /** CLI name (e.g., "claude", "codex") */
  name: string;
  /** Absolute path on the remote machine, empty if not found */
  path: string;
  /** Whether the binary was found and is executable */
  available: boolean;
  /** Version string if detectable, undefined otherwise */
  version?: string;
}

export interface AgentDetectionResult {
  profileId: string;
  agents: DetectedAgent[];
  detectedAt: number;
}

/** Well-known agent CLIs to probe for on the remote PATH. */
const AGENT_CLIS = ["claude", "codex"] as const;

// ── Agent Detection ──────────────────────────────────────────────

export class AgentDetector extends EventEmitter {
  private router: RpcRouter | undefined;
  private sessionManager: SshSessionManager;
  private cache = new Map<string, AgentDetectionResult>();

  constructor(sessionManager: SshSessionManager, router?: RpcRouter) {
    super();
    this.sessionManager = sessionManager;
    this.router = router;
  }

  /**
   * Set or update the RPC router (may be initialized after construction).
   */
  setRouter(router: RpcRouter): void {
    this.router = router;
  }

  /**
   * Detect available agent CLIs on the remote server identified by profileId.
   *
   * Strategy:
   * 1. If the RPC router is available and the agent channel is up, send
   *    `detect_agents` RPC (preferred — no extra SSH channel needed).
   * 2. Fallback: run `which <cli>` commands over an SSH exec channel.
   *
   * Results are cached per profile and emitted via `ssh:agents-detected`.
   */
  async detect(profileId: string): Promise<AgentDetectionResult> {
    let agents: DetectedAgent[];

    if (this.router) {
      agents = await this.detectViaRpc(profileId);
    } else {
      agents = await this.detectViaExec(profileId);
    }

    const result: AgentDetectionResult = {
      profileId,
      agents,
      detectedAt: Date.now(),
    };

    this.cache.set(profileId, result);
    this.emit("ssh:agents-detected", result);
    return result;
  }

  /**
   * Get cached detection results for a profile, or undefined if not yet detected.
   */
  getCached(profileId: string): AgentDetectionResult | undefined {
    return this.cache.get(profileId);
  }

  /**
   * Clear cached results for a profile (e.g., on disconnect).
   */
  clearCache(profileId: string): void {
    this.cache.delete(profileId);
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Detect agents via the RPC router by sending a `detect_agents` call
   * to the remote agent. Falls back to individual `exec_command` calls
   * if `detect_agents` is not supported.
   */
  private async detectViaRpc(profileId: string): Promise<DetectedAgent[]> {
    const repoId = `ssh://${profileId}/`;

    try {
      // Try the dedicated detect_agents RPC first
      const result = await this.router!.route(repoId, "detect_agents", {}) as {
        agents?: Array<{ name: string; path: string; available: boolean; version?: string }>;
      };

      if (result.agents && Array.isArray(result.agents)) {
        return result.agents.map((a) => ({
          name: a.name,
          path: a.path ?? "",
          available: a.available ?? false,
          version: a.version,
        }));
      }
    } catch {
      // detect_agents RPC not supported — fall back to exec_command
    }

    // Fallback: use exec_command RPC for each CLI
    return this.detectViaExecCommand(profileId);
  }

  /**
   * Detect agents by sending individual `exec_command` RPCs for `which <cli>`.
   */
  private async detectViaExecCommand(profileId: string): Promise<DetectedAgent[]> {
    const repoId = `ssh://${profileId}/`;
    const agents: DetectedAgent[] = [];

    for (const name of AGENT_CLIS) {
      try {
        const result = await this.router!.route(repoId, "exec_command", {
          command: `which ${name}`,
        }) as { stdout?: string; exitCode?: number };

        const available = result.exitCode === 0;
        const agentPath = available ? (result.stdout?.trim() ?? "") : "";

        let version: string | undefined;
        if (available) {
          version = await this.getAgentVersion(repoId, name);
        }

        agents.push({ name, path: agentPath, available, version });
      } catch {
        agents.push({ name, path: "", available: false });
      }
    }

    return agents;
  }

  /**
   * Detect agents by running `which` commands directly over an SSH exec channel.
   * Used when no RPC router is available.
   */
  private async detectViaExec(profileId: string): Promise<DetectedAgent[]> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || conn.status !== "connected") {
      return AGENT_CLIS.map((name) => ({ name, path: "", available: false }));
    }

    const agents: DetectedAgent[] = [];

    for (const name of AGENT_CLIS) {
      try {
        const result = await this.execOnRemote(conn, `which ${name}`);
        const available = result.exitCode === 0;
        const agentPath = available ? result.stdout.trim() : "";

        let version: string | undefined;
        if (available) {
          const versionResult = await this.execOnRemote(conn, `${name} --version 2>/dev/null || true`);
          if (versionResult.exitCode === 0 && versionResult.stdout.trim()) {
            version = versionResult.stdout.trim().split("\n")[0];
          }
        }

        agents.push({ name, path: agentPath, available, version });
      } catch {
        agents.push({ name, path: "", available: false });
      }
    }

    return agents;
  }

  /**
   * Try to get the version of an agent CLI via RPC exec_command.
   */
  private async getAgentVersion(repoId: string, name: string): Promise<string | undefined> {
    try {
      const result = await this.router!.route(repoId, "exec_command", {
        command: `${name} --version 2>/dev/null || true`,
      }) as { stdout?: string; exitCode?: number };

      if (result.exitCode === 0 && result.stdout?.trim()) {
        return result.stdout.trim().split("\n")[0];
      }
    } catch {
      // Version detection is best-effort
    }
    return undefined;
  }

  /**
   * Execute a command on the remote server via the SSH exec channel.
   * Returns stdout and exit code.
   */
  private execOnRemote(
    conn: SshConnection,
    command: string,
  ): Promise<{ stdout: string; exitCode: number }> {
    return new Promise((resolve, reject) => {
      conn.client.exec(command, (err, stream) => {
        if (err) {
          reject(err);
          return;
        }

        let stdout = "";
        let exitCode = 1;

        stream.on("data", (data: Buffer) => {
          stdout += data.toString("utf8");
        });

        stream.on("close", (code: number) => {
          exitCode = code ?? 1;
          resolve({ stdout, exitCode });
        });

        stream.on("error", (streamErr: Error) => {
          reject(streamErr);
        });

        // Timeout after 5 seconds
        setTimeout(() => {
          try {
            stream.close();
          } catch {
            // Stream may already be closed
          }
          resolve({ stdout, exitCode: 1 });
        }, 5000);
      });
    });
  }
}
