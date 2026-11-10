import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { AgentAdapter, createAdapter, type AgentEvent } from "../electron/orchestrator-adapters";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { Orchestrator } from "../electron/orchestrator";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";
import type { RpcRouter } from "../electron/ssh-router";
import * as settings from "../electron/orchestrator-settings";

vi.mock("../electron/orchestrator-adapters", async original => ({ ...await original<typeof import("../electron/orchestrator-adapters")>(), createAdapter: vi.fn() }));
vi.mock("../electron/ssh-native-process", () => ({ remoteAgentProcessFactory: vi.fn() }));

class NativeAdapter extends AgentAdapter {
  readonly name = "Native fixture";
  readonly binary = "server-native";
  closure = Promise.resolve();
  spawn = vi.fn(async () => {});
  iterate = vi.fn(async () => {});
  respondApproval = vi.fn(() => true);
  override cancel = vi.fn(async () => { await this.closure; this.emitExit(0); });
  notify(event: AgentEvent): void { this.emitEvent(event); }
}

describe("remote tasks through the shared native pipeline", () => {
  let directory: string;
  let orchestrator: Orchestrator;
  let adapters: NativeAdapter[];
  const sha = "a".repeat(40);
  const remoteFactory = () => { throw new Error("The native adapter owns protocol I/O"); };
  const router = {
    route: vi.fn(async () => ({ path: "/server/worktree", branch: "agent/fixture", base_ref: sha })),
    sendToRemoteAgent: vi.fn(async (_profile: string, method: string) => method === "task_apply" ? { applied: true } : { discarded: true }),
  };

  beforeEach(async () => {
    vi.clearAllMocks(); adapters = [];
    directory = await mkdtemp(path.join(tmpdir(), "crab-remote-native-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    vi.mocked(remoteAgentProcessFactory).mockReturnValue(remoteFactory);
    vi.mocked(createAdapter).mockImplementation(() => { const adapter = new NativeAdapter(); adapters.push(adapter); return adapter; });
    vi.spyOn(settings, "loadAgentConfig").mockReturnValue({ binaryPath: "/client-only/provider", extraArgs: ["--client-only"], env: { CLIENT_ONLY_CREDENTIAL: "fixture-value" } });
    vi.spyOn(CheckpointManager.prototype, "pinBase").mockResolvedValue(sha);
    vi.spyOn(CheckpointManager.prototype, "createLightweightCheckpoint").mockResolvedValue({ sha, ref: "refs/crab/checkpoints/fixture/0" });
    vi.spyOn(CheckpointManager.prototype, "snapshotHead").mockResolvedValue(sha);
    vi.spyOn(CheckpointManager.prototype, "holdCheckpoint").mockResolvedValue();
    vi.spyOn(CheckpointManager.prototype, "recordCheckpoint").mockResolvedValue({ sha, ref: "refs/crab/checkpoints/fixture/1" });
    vi.spyOn(CheckpointManager.prototype, "releaseCheckpoint").mockResolvedValue();
    vi.spyOn(CheckpointManager.prototype, "cleanup").mockResolvedValue();
    vi.spyOn(CheckpointManager.prototype, "commitTurn").mockResolvedValue(false);
    orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), rpcRouter: router as unknown as RpcRouter, maxConcurrent: 1 });
  });
  afterEach(async () => {
    for (const adapter of adapters) adapter.closure = Promise.resolve();
    await orchestrator.shutdown();
    vi.restoreAllMocks(); vi.unstubAllEnvs(); await rm(directory, { recursive: true, force: true });
  });
  const create = () => orchestrator.createTask("Native remote prompt", "claude-code", "ssh://server/repo");
  const complete = () => adapters[0].notify({ type: "done", timestamp: Date.now(), outcome: "completed", content: "Done" });

  it("uses native controls, approvals and checkpoint receipts without client configuration", async () => {
    const task = await create(); await orchestrator.startTask(task.id); complete();
    await orchestrator.iterate(task.id, "Follow up", { model: "server-model" });
    orchestrator.recordApproval(task.id, { requestId: "approval", decision: "allow_once", summary: "Owned fixture" });
    expect({ configuration: vi.mocked(createAdapter).mock.calls[0], spawn: adapters[0].spawn.mock.calls, iterate: adapters[0].iterate.mock.calls,
      approval: adapters[0].respondApproval.mock.calls, feedback: task.feedback, model: task.model,
      localConfigReads: vi.mocked(settings.loadAgentConfig).mock.calls, legacy: router.sendToRemoteAgent.mock.calls }).toEqual({
      configuration: ["claude-code", undefined, undefined, [], { env: undefined, optOutNotifications: undefined, processFactory: remoteFactory }],
      spawn: [["Native remote prompt", task.worktreePath, undefined, { model: undefined, reasoningLevel: undefined }]],
      iterate: [["Follow up", { model: "server-model" }]], approval: [["approval", "allow_once"]], feedback: ["Follow up"], model: "server-model", localConfigReads: [], legacy: [],
    });
  });

  it("admits queued remote work only after the current native turn completes", async () => {
    const first = await create(); const second = await create();
    await orchestrator.startTask(first.id);
    const pending = orchestrator.startTask(second.id);
    await vi.waitFor(() => expect(second.status).toBe("queued"));
    expect(adapters).toHaveLength(1);
    complete(); await pending;
    expect({ adapters: adapters.length, status: second.status }).toEqual({ adapters: 2, status: "running" });
  });

  it("uses the parent workspace UUID for a remote child conversation", async () => {
    const parent = await create();
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child prompt");
    await orchestrator.startTask(child.id);
    const owners = vi.mocked(remoteAgentProcessFactory).mock.calls.map(call => call[2]);
    expect({ childOwner: owners.includes(child.id), parentOwner: owners.includes(parent.id), path: child.worktreePath }).toEqual({ childOwner: false, parentOwner: true, path: parent.worktreePath });
  });

  it.each(["apply", "discard"])("waits for native closure before remote %s", async operation => {
    const task = await create(); await orchestrator.startTask(task.id); complete();
    let release!: () => void;
    adapters[0].closure = new Promise(resolve => { release = resolve; });
    const mutation = operation === "apply" ? orchestrator.applyTask(task.id) : orchestrator.discardTask(task.id);
    try {
      await vi.waitFor(() => expect(adapters[0].cancel).toHaveBeenCalled());
      expect(router.sendToRemoteAgent).not.toHaveBeenCalled();
    } finally { release(); }
    await mutation;
    expect({ methods: router.sendToRemoteAgent.mock.calls.map(call => call[1]), status: task.status }).toEqual({ methods: [`task_${operation}`], status: operation === "apply" ? "applied" : "discarded" });
  });

  it("retains a failed native owner and refuses destructive cleanup", async () => {
    const task = await create(); await orchestrator.startTask(task.id); complete();
    adapters[0].closure = Promise.reject(new Error("Unconfirmed native closure"));
    await expect(orchestrator.cancelTask(task.id)).rejects.toThrow("Unconfirmed native closure");
    await expect(orchestrator.discardTask(task.id)).rejects.toThrow("Unconfirmed native closure");
    expect(router.sendToRemoteAgent).not.toHaveBeenCalled();
  });

  it("does not construct a provider after remote checkpoint failure", async () => {
    const task = await create();
    vi.mocked(CheckpointManager.prototype.createLightweightCheckpoint).mockRejectedValue(new Error("Remote snapshot failed"));
    await expect(orchestrator.startTask(task.id)).rejects.toThrow("Remote snapshot failed");
    expect({ adapters: adapters.length, status: task.status }).toEqual({ adapters: 0, status: "failed" });
  });
});
