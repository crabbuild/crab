import type { IpcMain } from "electron";
import type { AgentProcessFactory, OwnedAgentProcess } from "./agent-process";
import { runAgentCommand } from "./agent-process";
import { detectAgents, getCachedAgents, prewarmAgents, type DetectedAgent } from "./orchestrator-adapters";
import type { RpcRouter } from "./ssh-router";
import { parseRepoId } from "./ssh-types";
import { remoteAgentProcessFactory } from "./ssh-native-process";

/** Probes the same server environment used by native conversations. */
export class AgentDetector {
  private readonly processes = new Set<OwnedAgentProcess>();
  private closing?: Promise<void>;

  constructor(private readonly router: () => RpcRouter | undefined) {}

  async detect(repoId: string): Promise<DetectedAgent[]> {
    if (this.closing) throw new Error("Agent detection is shutting down");
    const context = parseRepoId(repoId);
    if (!context) throw new Error("Remote agent detection requires an SSH repository");
    const router = this.router();
    if (!router) throw new Error("Remote agent detection requires a connected remote agent");
    const source = remoteAgentProcessFactory(router, context.profileId);
    const factory: AgentProcessFactory = (binary, args, options) => {
      if (this.closing) throw new Error("Agent detection is shutting down");
      const owner = source(binary, args, options);
      this.processes.add(owner);
      // Failed closure retains its owner so app quit cannot report success.
      void owner.closed.then(() => this.processes.delete(owner), () => {});
      return owner;
    };
    const results = await Promise.allSettled([
      { type: "claude-code", name: "Claude Code", binary: "claude" },
      { type: "codex", name: "Codex", binary: "codex" },
    ].map(async agent => {
      // Absence is a successful empty probe. Launch/version/transport failures
      // remain errors; they must not masquerade as missing installations.
      const { stdout } = await runAgentCommand(factory, "/bin/sh", ["-c",
        'p=$(command -v "$1") || exit 0; printf "%s\\n" "$p"; exec "$p" --version',
        "crab-agent-detection", agent.binary,
      ], { cwd: context.remotePath, timeoutMs: 30_000 });
      const [path = "", ...version] = stdout.trim().split("\n");
      return { ...agent, path, version: version.join("\n").trim(), available: path !== "" };
    }));
    const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
    if (errors.length) throw new AggregateError(errors.map(result => result.reason), `Server agent detection failed: ${String(errors[0].reason)}`);
    return results.map(result => (result as PromiseFulfilledResult<DetectedAgent>).value);
  }

  shutdown(): Promise<void> {
    this.closing ??= Promise.resolve().then(async () => {
      const owners = [...this.processes];
      for (const owner of owners) owner.process.kill("SIGTERM");
      const results = await Promise.allSettled(owners.map(owner => owner.closed));
      const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Server agent detection closure is unconfirmed");
    });
    return this.closing;
  }
}

/** Registers local cached detection and uncached, profile-owned server probes. */
export function registerAgentDetectionIpc(ipcMain: IpcMain, router: () => RpcRouter | undefined): () => Promise<void> {
  const remote = new AgentDetector(router);
  ipcMain.handle("orchestrator:detect-agents", async (_event, payload?: { force?: boolean; cachedOnly?: boolean; repoId?: string }) => {
    try {
      if (payload?.repoId && parseRepoId(payload.repoId)) {
        // Local cache contents never describe a server, even on first render.
        if (payload.cachedOnly) return { success: true, agents: [], cached: false };
        return { success: true, agents: await remote.detect(payload.repoId) };
      }
      if (payload?.cachedOnly) {
        const cached = getCachedAgents();
        return { success: true, agents: cached ?? [], cached: cached !== null };
      }
      return { success: true, agents: await detectAgents({ force: payload?.force }) };
    } catch (error: unknown) {
      return { success: false, error: error instanceof Error ? error.message : String(error) };
    }
  });
  prewarmAgents();
  return () => remote.shutdown();
}
