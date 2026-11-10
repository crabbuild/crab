import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import type { IpcMain } from "electron";

vi.mock("../electron/ssh-native-process", () => ({ remoteAgentProcessFactory: vi.fn() }));
vi.mock("../electron/orchestrator-adapters", () => ({ detectAgents: vi.fn(), getCachedAgents: vi.fn(), prewarmAgents: vi.fn(), getExpandedEnv: (env: Record<string, string>) => ({ ...process.env, ...env }) }));
import { spawnLocalAgent, type AgentProcessFactory } from "../electron/agent-process";
import { detectAgents, getCachedAgents, prewarmAgents } from "../electron/orchestrator-adapters";
import { remoteAgentProcessFactory } from "../electron/ssh-native-process";
import { AgentDetector, registerAgentDetectionIpc } from "../electron/ssh-agent-detection";
import type { RpcRouter } from "../electron/ssh-router";

describe.skipIf(process.platform === "win32")("owned server agent availability", () => {
  let root: string;
  let bin: string;
  let detector: AgentDetector;
  const router = {} as RpcRouter;
  const script = (name: string, body: string) => writeFile(path.join(bin, name), `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  const repoId = () => `ssh://selected${root}`;
  beforeEach(async () => {
    vi.clearAllMocks();
    root = await mkdtemp(path.join(tmpdir(), "crab-agent-availability-"));
    bin = path.join(root, "bin"); await mkdir(bin);
    await script("claude", "printf 'Claude fixture\\n'");
    await script("codex", "printf 'Codex fixture\\n'");
    const source: AgentProcessFactory = (binary, args, options) => spawnLocalAgent(binary, args, { cwd: options.cwd, env: { PATH: `${bin}:/usr/bin:/bin`, CRAB_PROBE_MARKER: path.join(root, "started") } });
    vi.mocked(remoteAgentProcessFactory).mockReturnValue(source);
    detector = new AgentDetector(() => router);
  });
  afterEach(async () => { await detector.shutdown().catch(() => {}); await rm(root, { recursive: true, force: true }); });

  it("probes the captured profile's PATH through owned native commands", async () => {
    expect(await detector.detect(repoId())).toEqual([
      { type: "claude-code", name: "Claude Code", binary: "claude", path: path.join(bin, "claude"), version: "Claude fixture", available: true },
      { type: "codex", name: "Codex", binary: "codex", path: path.join(bin, "codex"), version: "Codex fixture", available: true },
    ]);
    expect(remoteAgentProcessFactory).toHaveBeenCalledWith(router, "selected");
    expect(detectAgents).not.toHaveBeenCalled();
  });

  it("reports a missing binary only when command lookup confirms absence", async () => {
    await rm(path.join(bin, "claude"));
    expect((await detector.detect(repoId()))[0]).toEqual({ type: "claude-code", name: "Claude Code", binary: "claude", path: "", version: "", available: false });
  });

  it("surfaces a failed executable instead of marking it uninstalled", async () => {
    await script("claude", "printf 'Broken server CLI' >&2; exit 7");
    await expect(detector.detect(repoId())).rejects.toThrow("Broken server CLI");
  });

  it("joins an active version probe before shutdown and rejects later work", async () => {
    await script("claude", 'printf started > "$CRAB_PROBE_MARKER"; sleep 30');
    const request = expect(detector.detect(repoId())).rejects.toThrow("Server agent detection failed");
    await vi.waitFor(() => expect(existsSync(path.join(root, "started"))).toBe(true));
    await detector.shutdown(); await request;
    await expect(detector.detect(repoId())).rejects.toThrow("shutting down");
  });

  it("retains unknown process closure at the quit boundary", async () => {
    const process = Object.assign(new EventEmitter(), { stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(), kill: vi.fn(() => true) });
    vi.mocked(remoteAgentProcessFactory).mockReturnValue(() => ({ process, closed: Promise.reject(new Error("SSH channel changed")) }));
    await expect(detector.detect(repoId())).rejects.toMatchObject({ errors: [{ cause: { message: "SSH channel changed" } }, { cause: { message: "SSH channel changed" } }] });
    await expect(detector.shutdown()).rejects.toThrow("closure is unconfirmed");
    expect(process.kill).toHaveBeenCalledWith("SIGTERM");
  });

  it("refuses a disconnected server without using client detection", async () => {
    detector = new AgentDetector(() => undefined);
    await expect(detector.detect(repoId())).rejects.toThrow("requires a connected remote agent");
    expect(detectAgents).not.toHaveBeenCalled();
  });

  it("routes the IPC request to the server without sharing local cached results", async () => {
    type Handler = Parameters<IpcMain["handle"]>[1];
    const handlers = new Map<string, Handler>();
    const stop = registerAgentDetectionIpc({ handle: (name: string, handler: Handler) => handlers.set(name, handler) } as unknown as IpcMain, () => router);
    const invoke = (payload?: object) => handlers.get("orchestrator:detect-agents")!({} as Parameters<Handler>[0], payload);
    try {
      expect(await invoke({ repoId: repoId(), cachedOnly: true })).toEqual({ success: true, agents: [], cached: false });
      expect(getCachedAgents).not.toHaveBeenCalled();
      expect(remoteAgentProcessFactory).not.toHaveBeenCalled();
      expect(await invoke({ repoId: repoId() })).toMatchObject({ success: true, agents: [{ type: "claude-code", available: true }, { type: "codex", available: true }] });
      expect(detectAgents).not.toHaveBeenCalled();
      expect(prewarmAgents).toHaveBeenCalledOnce();
    } finally { await stop(); }
  });

  it("preserves the local cache and forced local probe contract", async () => {
    type Handler = Parameters<IpcMain["handle"]>[1];
    const handlers = new Map<string, Handler>();
    const stop = registerAgentDetectionIpc({ handle: (name: string, handler: Handler) => handlers.set(name, handler) } as unknown as IpcMain, () => undefined);
    const invoke = (payload: object) => handlers.get("orchestrator:detect-agents")!({} as Parameters<Handler>[0], payload);
    vi.mocked(getCachedAgents).mockReturnValue([]); vi.mocked(detectAgents).mockResolvedValue([]);
    try {
      expect(await invoke({ repoId: "/local", cachedOnly: true })).toEqual({ success: true, agents: [], cached: true });
      expect(await invoke({ force: true })).toEqual({ success: true, agents: [] });
      expect(detectAgents).toHaveBeenCalledWith({ force: true });
      expect(remoteAgentProcessFactory).not.toHaveBeenCalled();
    } finally { await stop(); }
  });
});
