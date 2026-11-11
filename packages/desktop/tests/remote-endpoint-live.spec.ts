import { mockStorageConnections } from "./mock-storage-connections";
import { expect, it, vi } from "vitest";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import type { ClientChannel } from "ssh2";
import { SshSessionManager, type SshProfile } from "../electron/ssh-manager";
import { RpcRouter } from "../electron/ssh-router";
import { AgentDeployer } from "../electron/ssh-deployer";
import { CapabilityNegotiator } from "../electron/ssh-capabilities";
import { Orchestrator } from "../electron/orchestrator";
import { computeRemoteDiff } from "../electron/orchestrator-diff";
import { getExpandedEnv } from "../electron/env/userShellEnv";
import { terminalSshServer } from "./fixtures/ssh-terminal-server";

vi.mock("../electron/keychain", () => ({ getPassword: async () => "fixture-only" }));

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

it.skipIf(!liveRoot || process.platform === "win32")("binds warm and cold tasks to the authenticated SSH endpoint", async () => {
  const root = await realpath(await mkdtemp(path.join(liveRoot!, "remote-endpoint-")));
  const repo = path.join(root, "repo");
  const runtime = path.resolve(`resources/native/${process.platform === "darwin" ? "mac" : "linux"}-${process.arch}`);
  const servers: Awaited<ReturnType<typeof terminalSshServer>>[] = [];
  const pidFiles: string[] = [];
  const keys = new Map<number, string>();
  const manager = new SshSessionManager(async (host, port, key) => {
    if (host !== "127.0.0.1" || keys.get(port) !== createHash("sha256").update(key).digest("hex")) throw new Error("Unexpected fixture host key");
  });
  const router = new RpcRouter(manager, { request: async () => { throw new Error("Remote task cannot use local RPC"); } }, mockStorageConnections);
  const deployer = new AgentDeployer(async () => { throw new Error("The fixture uses the already built runtime"); });
  // Exercise production connection, verification, attachment and capability
  // ownership. Upload qualification has its own real-SFTP fixture.
  vi.spyOn(deployer, "ensureAgent").mockImplementation((client, signal) => new Promise<ClientChannel>((resolve, reject) => {
    signal.throwIfAborted();
    const file = path.join(root, `sidecar-${pidFiles.length}.pid`); pidFiles.push(file);
    client.exec(`printf '%s\\n' "$$" > ${quote(file)}; exec ${quote(path.join(runtime, "crab-desktop-agent"))}`, (error, stream) => error ? reject(error) : resolve(stream));
  }));
  manager.setAgentDependencies({ createDeployer: () => deployer, capabilityNegotiator: new CapabilityNegotiator(router), rpcRouter: router });
  const requests = vi.spyOn(router, "requestOnRemoteChannel");
  const persistPath = path.join(root, "tasks.sqlite");
  vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(root, "settings.json"));
  let orchestrator = new Orchestrator({ persistPath, rpcRouter: router });
  let succeeded = false;
  try {
    await mkdir(repo);
    await run("git", ["init", "--template=", "--initial-branch=main", repo]);
    for (const [key, value] of [["user.name", "Fixture"], ["user.email", "fixture@example.invalid"], ["commit.gpgsign", "false"]]) await run("git", ["config", key, value], { cwd: repo });
    await writeFile(path.join(repo, "seed.txt"), "before\n");
    await run("git", ["add", "."], { cwd: repo });
    await run("git", ["commit", "-m", "baseline"], { cwd: repo });
    for (const name of ["original", "replacement"]) {
      const home = path.join(root, name); await mkdir(home);
      const server = await terminalSshServer(home, (await getExpandedEnv()).PATH); servers.push(server);
      keys.set(server.connection.port, server.connection.fingerprint);
      Object.assign(server.env, { CRAB_BIN: path.join(runtime, "crab"), TMPDIR: home, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" });
    }
    const profile: SshProfile = { id: "fixture", label: "Owned endpoint", host: "127.0.0.1", port: servers[0].connection.port, username: "fixture",
      authMethod: { type: "password" }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect: false, autoDeployAgent: true,
      tags: [], portForwards: [], createdAt: Date.now() };
    const repoId = `ssh://fixture${repo}`;
    const open = async (port: number) => {
      await manager.connect({ ...profile, port });
      await router.sendToRemoteAgent(profile.id, "open_local", { repo_id: repoId, path: repo });
    };
    await open(servers[0].connection.port);
    const task = await orchestrator.createTask("Owned endpoint fixture", "codex", repoId);
    const original = task.remoteEndpoint;
    await writeFile(path.join(task.worktreePath, "owned-target.txt"), "original endpoint\n");
    await run("git", ["add", "."], { cwd: task.worktreePath });
    await run("git", ["commit", "-m", "owned change"], { cwd: task.worktreePath });
    const before = (await run("git", ["rev-parse", "HEAD"], { cwd: repo })).stdout;
    await open(servers[1].connection.port);
    const replacementEndpoint = router.remoteEndpoint(profile.id);
    // Both actual servers can access the same task metadata. Missing paths or
    // a backend UUID refusal must not be what prevents the unintended mutation.
    expect(await readFile(path.join(task.worktreePath, "owned-target.txt"), "utf8")).toBe("original endpoint\n");
    for (const recovery of ["warm", "cold"]) {
      if (recovery === "cold") { await orchestrator.shutdown(); orchestrator = new Orchestrator({ persistPath, rpcRouter: router }); }
      const saved = orchestrator.getTask(task.id)!;
      const count = requests.mock.calls.length;
      await expect(orchestrator.applyTask(saved.id)).rejects.toThrow("endpoint");
      await expect(orchestrator.discardTask(saved.id)).rejects.toThrow("endpoint");
      await expect(orchestrator.queryCheckpoints(saved, checkpoints => checkpoints.listCheckpoints(saved))).rejects.toThrow("endpoint");
      await expect(computeRemoteDiff(saved, router)).rejects.toThrow("endpoint");
      expect(() => orchestrator.agentProcessFactory(saved)).toThrow("endpoint");
      expect({ endpoint: saved.remoteEndpoint, sent: requests.mock.calls.length, head: (await run("git", ["rev-parse", "HEAD"], { cwd: repo })).stdout }).toEqual({ endpoint: original, sent: count, head: before });
    }
    await open(servers[0].connection.port);
    const saved = orchestrator.getTask(task.id)!;
    expect(await orchestrator.queryCheckpoints(saved, checkpoints => checkpoints.listCheckpoints(saved))).toEqual([]);
    await orchestrator.applyTask(saved.id);
    expect({ status: saved.status, file: await readFile(path.join(repo, "owned-target.txt"), "utf8") }).toEqual({ status: "applied", file: "original endpoint\n" });
    await writeFile(path.join(liveRoot!, "remote-endpoint-live.json"), JSON.stringify({ endpoint: original, replacement: replacementEndpoint, status: saved.status, requestMethods: requests.mock.calls.map(call => call[2]) }, null, 2));
    succeeded = true;
  } finally {
    try { await orchestrator.shutdown(); }
    finally {
      await manager.disconnect("fixture");
      for (const server of servers.reverse()) await server.close();
      vi.restoreAllMocks(); vi.unstubAllEnvs();
      if (succeeded) {
        for (const file of pidFiles) {
          const pid = Number((await readFile(file, "utf8")).trim());
          if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error("Fixture sidecar has no process identity");
          await vi.waitFor(() => expect(() => process.kill(pid, 0)).toThrowError(expect.objectContaining({ code: "ESRCH" })), { timeout: 10_000 });
        }
        await rm(root, { recursive: true, force: true });
      }
    }
  }
}, 90_000);
