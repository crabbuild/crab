import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import Sqlite from "better-sqlite3";
import { AgentAdapter, createAdapter, type AgentEvent } from "../electron/orchestrator-adapters";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { Orchestrator } from "../electron/orchestrator";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";
import { RpcRouter } from "../electron/ssh-router";
import * as settings from "../electron/orchestrator-settings";
import { AgentProcessClosureError } from "../electron/agent-process";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

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
  let unknownCheckpoint: boolean;
  let endpoint: { host: string; port: number; username: string; hostKey: string };
  let channelOwner: object;
  const sha = "a".repeat(40);
  const remoteFactory = () => { throw new Error("The native adapter owns protocol I/O"); };
  const router = {
    remoteEndpoint: vi.fn(() => ({ ...endpoint })),
    assertRemoteEndpoint: RpcRouter.prototype.assertRemoteEndpoint,
    remoteChannelOwner: vi.fn(() => channelOwner),
    requestOnRemoteChannel: vi.fn(async (profile: string, owner: object, method: string, _params: unknown) => {
      if (owner !== channelOwner) throw new Error("Remote channel changed");
      return method === "worktree_create" ? router.route() : router.sendToRemoteAgent(profile, method);
    }),
    route: vi.fn(async () => ({ path: "/server/worktree", branch: "agent/fixture", base_ref: sha })),
    sendToRemoteAgent: vi.fn(async (_profile: string, method: string) => method === "task_apply" ? { applied: true } : { discarded: true }),
  };

  beforeEach(async () => {
    vi.clearAllMocks(); adapters = []; unknownCheckpoint = false;
    endpoint = { host: "original.example.invalid", port: 22, username: "fixture", hostKey: "SHA256:original" };
    channelOwner = {};
    directory = await mkdtemp(path.join(tmpdir(), "crab-remote-native-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    vi.mocked(remoteAgentProcessFactory).mockReturnValue(remoteFactory);
    vi.mocked(createAdapter).mockImplementation(() => { const adapter = new NativeAdapter(); adapters.push(adapter); return adapter; });
    vi.spyOn(settings, "loadAgentConfig").mockReturnValue({ binaryPath: "/client-only/provider", extraArgs: ["--client-only"], env: { CLIENT_ONLY_CREDENTIAL: "fixture-value" } });
    vi.spyOn(CheckpointManager.prototype, "pinBase").mockResolvedValue(sha);
    vi.spyOn(CheckpointManager.prototype, "captureBase").mockResolvedValue(sha);
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
    if (unknownCheckpoint) await expect(orchestrator.shutdown()).rejects.toThrow("shutdown failed");
    else await orchestrator.shutdown();
    vi.restoreAllMocks(); vi.unstubAllEnvs(); await rm(directory, { recursive: true, force: true });
  });
  const create = () => orchestrator.createTask("Native remote prompt", "claude-code", "ssh://server/repo");
  const complete = () => adapters[0].notify({ type: "done", timestamp: Date.now(), outcome: "completed", content: "Done" });

  it.each(["warm", "cold"].flatMap(recovery => ["apply", "discard"].map(operation => ({ recovery, operation }))))(
    "refuses $operation on a retargeted profile after $recovery recovery", async ({ recovery, operation }) => {
      const task = await create(); await orchestrator.startTask(task.id); complete();
      await vi.waitFor(() => expect(task.status).toBe("reviewing"));
      if (recovery === "cold") {
        await orchestrator.shutdown();
        orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), rpcRouter: router as unknown as RpcRouter });
      }
      endpoint = { ...endpoint, host: "replacement.example.invalid", hostKey: "SHA256:replacement" };
      const action = operation === "apply" ? orchestrator.applyTask(task.id) : orchestrator.discardTask(task.id);
      await expect(action).rejects.toThrow(/endpoint/i);
      expect(router.sendToRemoteAgent.mock.calls).toEqual([]);
    },
  );

  it.each(["apply", "discard"])("refuses remote %s when its channel changes while writers close", async operation => {
    const task = await create(); await orchestrator.startTask(task.id); complete();
    let release!: () => void;
    adapters[0].closure = new Promise(resolve => { release = resolve; });
    const result = (operation === "apply" ? orchestrator.applyTask(task.id) : orchestrator.discardTask(task.id)).catch((error: unknown) => error);
    try {
      await vi.waitFor(() => expect(adapters[0].cancel).toHaveBeenCalled());
      channelOwner = {};
    } finally { release(); }
    const outcome = await result;
    expect({ message: outcome instanceof Error ? outcome.message : undefined, writes: router.sendToRemoteAgent.mock.calls }).toEqual({ message: expect.stringMatching(/channel/i), writes: [] });
  });

  it("does not borrow a disposable workspace owner from another endpoint at the same path", async () => {
    const original = await create();
    endpoint = { ...endpoint, host: "replacement.example.invalid", hostKey: "SHA256:replacement" };
    const borrowed = await orchestrator.createTask("Borrowed on another host", "claude-code", "ssh://server/repo", "HEAD", { worktreeMode: "existing", worktreePath: original.worktreePath });
    await orchestrator.startTask(borrowed.id);
    expect(vi.mocked(remoteAgentProcessFactory).mock.calls.at(-1)?.[2]).toBeUndefined();
  });

  it("shares a workspace owner across profile aliases of the same verified endpoint", async () => {
    const original = await create();
    const borrowed = await orchestrator.createTask("Borrowed through an alias", "claude-code", "ssh://alias/repo", "HEAD", { worktreeMode: "existing", worktreePath: original.worktreePath });
    await orchestrator.startTask(borrowed.id);
    expect(vi.mocked(remoteAgentProcessFactory).mock.calls.at(-1)?.[2]).toBe(original.id);
  });

  it("preserves an older task without inferring its missing endpoint on restart", async () => {
    const task = await create();
    task.remoteEndpoint = undefined;
    task.claudeSessionId = "saved-native-session";
    await orchestrator.shutdown();
    const oldDatabase = new Sqlite(path.join(directory, "tasks.sqlite"));
    try { oldDatabase.exec("ALTER TABLE tasks DROP COLUMN remote_endpoint_json"); }
    finally { oldDatabase.close(); }
    orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), rpcRouter: router as unknown as RpcRouter });
    const saved = orchestrator.getTask(task.id)!;
    const count = router.requestOnRemoteChannel.mock.calls.length;
    await expect(orchestrator.startTask(saved.id)).rejects.toThrow("endpoint was not recorded");
    await expect(orchestrator.applyTask(saved.id)).rejects.toThrow("endpoint was not recorded");
    await expect(orchestrator.discardTask(saved.id)).rejects.toThrow("endpoint was not recorded");
    expect({ endpoint: saved.remoteEndpoint, session: saved.claudeSessionId, path: saved.worktreePath, branch: saved.branch, sent: router.requestOnRemoteChannel.mock.calls.length }).toEqual({
      endpoint: undefined, session: "saved-native-session", path: task.worktreePath, branch: task.branch, sent: count,
    });
  });

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

  it("joins a stopped startup's checkpoint writer before reporting closure", async () => {
    const task = await create();
    let release!: () => void;
    vi.mocked(CheckpointManager.prototype.pinBase).mockImplementationOnce(() => new Promise(resolve => { release = () => resolve(sha); }));
    const startup = orchestrator.startTask(task.id);
    const cancelled = expect(startup).rejects.toThrow("cancelled");
    await vi.waitFor(() => expect(release).toBeTypeOf("function"));
    let stopped = false;
    const stopping = orchestrator.cancelTask(task.id).then(() => { stopped = true; });
    try {
      await new Promise(resolve => setImmediate(resolve));
      expect({ stopped, providers: adapters.length }).toEqual({ stopped: false, providers: 0 });
    } finally { release(); await Promise.all([stopping, cancelled]); }
  });

  it("joins post-turn Git before Stop resolves", async () => {
    const task = await create(); await orchestrator.startTask(task.id);
    let release!: () => void;
    vi.mocked(CheckpointManager.prototype.commitTurn).mockImplementationOnce(() => new Promise(resolve => { release = () => resolve(false); }));
    complete();
    await vi.waitFor(() => expect(release).toBeTypeOf("function"));
    let stopped = false;
    const stopping = orchestrator.cancelTask(task.id).then(() => { stopped = true; });
    try {
      await new Promise(resolve => setImmediate(resolve));
      expect(stopped).toBe(false);
    } finally { release(); await stopping; }
  });

  it("retains uncertain checkpoint closure and refuses later mutation", async () => {
    const task = await create(); await orchestrator.startTask(task.id);
    vi.mocked(CheckpointManager.prototype.commitTurn).mockRejectedValueOnce(new AgentProcessClosureError(new Error("Unknown Git writer")));
    const failure = vi.fn(); orchestrator.on("orchestrator:error", failure);
    complete(); await vi.waitFor(() => expect(failure).toHaveBeenCalled());
    unknownCheckpoint = true;
    await expect(orchestrator.cancelTask(task.id)).rejects.toThrow("closure");
    await expect(orchestrator.discardTask(task.id)).rejects.toThrow("closure");
    expect(() => orchestrator.createChildTaskForAgent(task.id, "codex", "Child")).toThrow("closure");
    expect(router.sendToRemoteAgent).not.toHaveBeenCalled();
    await expect(orchestrator.shutdown()).rejects.toThrow();
  });

  it("retains a lost query for quit without treating it as a workspace writer", async () => {
    const task = await create();
    vi.spyOn(CheckpointManager.prototype, "listCheckpoints").mockRejectedValueOnce(new AgentProcessClosureError(new Error("Unknown reader")));
    unknownCheckpoint = true;
    await expect(orchestrator.queryCheckpoints(task, manager => manager.listCheckpoints(task))).rejects.toBeInstanceOf(AgentProcessClosureError);
    await orchestrator.discardTask(task.id);
    expect({ status: task.status, methods: router.sendToRemoteAgent.mock.calls.map(call => call[1]) }).toEqual({ status: "discarded", methods: ["task_discard"] });
  });

  it("cancels an owned checkpoint reader before quit completes", async () => {
    const task = await create();
    let finish!: () => void;
    const closed = new Promise<void>(resolve => { finish = resolve; });
    const process = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn(() => {
      process.stdout.end(); process.stderr.end(); process.emit("close", null); finish(); return true;
    }) });
    const factory = vi.fn(() => ({ process, closed }));
    vi.mocked(remoteAgentProcessFactory).mockReturnValue(factory);
    const query = expect(orchestrator.queryCheckpoints(task, manager => manager.listCheckpoints(task))).rejects.toThrow("cancelled");
    await vi.waitFor(() => expect(factory).toHaveBeenCalled());
    await orchestrator.shutdown(); await query;
    expect(process.kill).toHaveBeenCalledOnce();
  });
});
