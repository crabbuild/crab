import { describe, expect, it, vi } from "vitest";
import { spawn, execFile, type ChildProcess } from "node:child_process";
import { generateKeyPairSync } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer as createViteServer, type ViteDevServer } from "vite";
import { promisify } from "node:util";
import { homedir } from "node:os";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Server as SshServer, utils, type Client, type ClientChannel, type ServerChannel } from "ssh2";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import { KnownHostsManager, createHostKeyVerifier } from "../electron/ssh-known-hosts";
import { SshSessionManager } from "../electron/ssh-manager";
import { AgentDeployer } from "../electron/ssh-deployer";
import { CapabilityNegotiator } from "../electron/ssh-capabilities";
import type { Task } from "../electron/orchestrator";
import { RpcRouter } from "../electron/ssh-router";
import { registerDiffIpc, type TaskDiff } from "../electron/orchestrator-diff";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const target = process.env.CARGO_TARGET_DIR;

async function fixturePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  await new Promise<void>(resolve => server.close(() => resolve()));
  if (!address || typeof address === "string") throw new Error("Fixture port unavailable");
  return address.port;
}

async function sidecarChannel(startChild: () => ChildProcess, autoReconnect: boolean) {
  const keyDirectory = await mkdtemp(path.join(liveRoot!, "ssh-key-"));
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const pem = privateKey.export({ type: "pkcs1", format: "pem" });
  const key = utils.parseKey(pem);
  if (key instanceof Error || Array.isArray(key)) throw new Error("Fixture SSH key could not be parsed");
  const keyPath = path.join(keyDirectory, "identity.pem");
  await writeFile(keyPath, pem, { mode: 0o600 });
  let remote: ServerChannel | undefined;
  const server = new SshServer({ hostKeys: [pem] }, connection => {
    connection.on("error", () => {});
    connection.on("authentication", context => {
      if (context.username !== "fixture" || context.method !== "publickey" || !context.key.data.equals(key.getPublicSSH())) { context.reject(); return; }
      if (context.signature && (!context.blob || !key.verify(context.blob, context.signature, context.hashAlgo))) { context.reject(); return; }
      context.accept();
    });
    connection.on("ready", () => connection.on("session", accept => {
      accept().on("exec", (start, reject, info) => {
        if (info.command !== "owned-sidecar") { reject(); return; }
        const child = startChild();
        if (!child.stdin || !child.stdout) { reject(); return; }
        const input = child.stdin; const stream = start(); remote = stream;
        stream.on("error", () => {});
        stream.once("close", () => input.end());
        stream.pipe(input); child.stdout.pipe(stream);
      });
    }));
  });
  // The fixture supplies an already-built sidecar rather than uploading it.
  // Connection, channel attachment, capabilities and reconnect remain real.
  class FixtureDeployer extends AgentDeployer {
    override ensureAgent(client: Client): Promise<ClientChannel> {
      return new Promise((resolve, reject) => client.exec("owned-sidecar", (error, stream) => error ? reject(error) : resolve(stream)));
    }
  }
  const hosts = new KnownHostsManager(path.join(keyDirectory, "pins.json"), path.join(keyDirectory, "user_hosts"), path.join(keyDirectory, "global_hosts"));
  const manager = new SshSessionManager(createHostKeyVerifier(hosts, async () => true));
  const router = new RpcRouter(manager, { request: async () => { throw new Error("Local fallback must not run"); } });
  const negotiator = new CapabilityNegotiator(router);
  manager.setAgentDependencies({ createDeployer: () => new FixtureDeployer(async () => { throw new Error("Fixture uses its already-built sidecar"); }), capabilityNegotiator: negotiator, rpcRouter: router });
  const close = async () => {
    await manager.disconnect("fixture");
    if (server.listening) await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(keyDirectory, { recursive: true, force: true });
  };
  try {
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("SSH fixture did not listen");
    await manager.connect({ id: "fixture", label: "Owned fixture", host: "127.0.0.1", port: address.port, username: "fixture", authMethod: { type: "key", keyPath }, forwardAgent: false, compression: false, keepAliveInterval: 15, autoReconnect, autoDeployAgent: true, tags: [], portForwards: [], createdAt: Date.now() });
    const waitForRequest = (method: string) => new Promise<void>(resolve => {
      let buffer = "";
      const data = (chunk: Buffer) => {
        const lines = (buffer + chunk.toString("utf8")).split("\n"); buffer = lines.pop() ?? "";
        if (lines.some(line => JSON.parse(line).method === method)) { remote?.removeListener("data", data); resolve(); }
      };
      remote?.on("data", data);
    });
    return { manager, router, get channel() {
      const channel = manager.getConnection("fixture")?.channels.agent;
      if (!channel) throw new Error("Fixture agent channel unavailable");
      return channel;
    }, breakChannel: () => remote?.close(), waitForRequest, close };
  } catch (error: unknown) { await close(); throw error; }
}

// The macOS fixture denies the sidecar access to the user's indexer database.
// A loopback SSH server carries the built sidecar RPC stream; deployment and
// remote CLI credentials remain outside this transport qualification.
describe.skipIf(!liveRoot || !target || process.platform !== "darwin")("built sidecar file comparisons", () => {
  it.each(["success", "baseline-retry", "channel-close", "reconnect"] as const)("preserves the real index over SSH: %s", async kind => {
    const missing = kind === "baseline-retry" || kind === "channel-close";
    const directory = await mkdtemp(path.join(liveRoot!, "sidecar-diff-"));
    const run = promisify(execFile);
    const binary = path.join(target!, "debug", "crab-desktop-agent");
    let stderr = "";
    let child: ReturnType<typeof spawn> | undefined;
    let closed: Promise<number | null> | undefined;
    const children: Array<{ process: ChildProcess; closed: Promise<number | null> }> = [];
    let browser: Browser | undefined;
    let ssh: Awaited<ReturnType<typeof sidecarChannel>> | undefined;
    let vite: ViteDevServer | undefined;
    let bridge: Server | undefined;
    const clients = new Set<ServerResponse>();
    const operations = new Set<Promise<void>>();
    try {
      await run("git", ["init", "--initial-branch=main", directory]);
      await run("git", ["config", "user.name", "Fixture"], { cwd: directory });
      await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd: directory });
      await writeFile(path.join(directory, "tracked.txt"), "before\n");
      await run("git", ["add", "."], { cwd: directory });
      await run("git", ["commit", "-m", "Baseline"], { cwd: directory });
      const index = await readFile(path.join(directory, ".git/index"));
      await writeFile(path.join(directory, "tracked.txt"), "after\n");
      await writeFile(path.join(directory, "雪.txt"), "new\n");
      const protectedPath = path.join(homedir(), ".crab", "desktop", "indexer");
      const profile = `(version 1)(allow default)(deny file-read* file-write* (subpath ${JSON.stringify(protectedPath)}))`;
      ssh = await sidecarChannel(() => {
        const process = spawn("/usr/bin/sandbox-exec", ["-p", profile, binary], { cwd: directory, stdio: ["pipe", "pipe", "pipe"] });
        child = process;
        const ending = new Promise<number | null>((resolve, reject) => { process.once("error", reject); process.once("close", resolve); });
        closed = ending; children.push({ process, closed: ending });
        process.stderr.on("data", chunk => { stderr += chunk.toString(); });
        return process;
      }, kind === "reconnect");
      const { router, manager } = ssh;
      const task: Task = {
        id: "fixture", prompt: "Read files", agentType: "codex", status: "reviewing", branch: "main",
        worktreePath: directory, repoPath: "ssh://fixture/repo", baseRef: missing ? "missing-baseline" : "HEAD",
        isRemote: true, remoteProfileId: "fixture", createdAt: Date.now(), events: [], feedback: [], todos: [],
      };
      const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
      registerDiffIpc({ handle: (name, handler) => { handlers.set(name, handler); } } as IpcMain,
        { getTask: () => task, getRpcRouter: () => router }, () => null);
      const handler = handlers.get("orchestrator:get-diff");
      if (!handler) throw new Error("Comparison IPC not registered");
      const result = await handler({} as IpcMainInvokeEvent, { taskId: task.id }) as { success: boolean; diff?: TaskDiff; error?: string };
      expect({ success: result.success, paths: result.diff?.files.map(file => file.path), error: Boolean(result.error), index: await readFile(path.join(directory, ".git/index")) })
        .toEqual({ success: !missing, paths: missing ? undefined : ["tracked.txt", "雪.txt"], error: missing, index });
      const batch = await router.batchRpc("ssh://fixture/repo", [
        { method: "ping", params: {} },
        { method: "task_compute_diff", params: { worktree_path: directory, base_ref: "HEAD" } },
        { method: "unknown_fixture_method", params: {} },
      ]);
      expect({ pong: batch[0]?.result, comparison: batch[1]?.error ?? batch[1]?.result, error: batch[2]?.error?.code })
        .toEqual({ pong: { pong: expect.any(String) }, comparison: expect.objectContaining({ diff: expect.stringContaining("+after") }), error: -32601 });
      bridge = createServer((request, response) => {
        const operation = (async () => {
          response.setHeader("Access-Control-Allow-Origin", "*");
          response.setHeader("Access-Control-Allow-Headers", "Content-Type");
          if (request.method === "OPTIONS") { response.end(); return; }
          if (request.url === "/events") {
            response.setHeader("Content-Type", "text/event-stream"); response.writeHead(200); response.write("\n");
            clients.add(response); response.once("close", () => clients.delete(response)); return;
          }
          const chunks: Buffer[] = [];
          for await (const chunk of request) chunks.push(Buffer.from(chunk));
          let value: unknown;
          if (request.url === "/getDiff") value = await handler({} as IpcMainInvokeEvent, JSON.parse(Buffer.concat(chunks).toString()));
          else if (request.url === "/listTasks") value = { success: true, tasks: [task] };
          else if (request.url === "/loadChat") value = { success: true, messages: [], revision: 0 };
          else if (request.url === "/getGoal") value = { success: true, goal: null };
          else if (request.url === "/listCheckpoints") value = { success: true, checkpoints: [], backups: [] };
          else throw new Error(`Unexpected fixture operation: ${request.url}`);
          response.end(JSON.stringify(value));
        })().catch((error: unknown) => { response.end(JSON.stringify({ success: false, error: String(error) })); });
        operations.add(operation); void operation.finally(() => operations.delete(operation));
      });
      await new Promise<void>(resolve => bridge!.listen(0, "127.0.0.1", resolve));
      const api = bridge.address();
      if (!api || typeof api === "string") throw new Error("Fixture bridge did not listen");
      vite = await createViteServer({ cacheDir: `${directory}-vite-cache`, server: { host: "127.0.0.1", port: await fixturePort(), strictPort: true }, clearScreen: false });
      await vite.listen();
      const ui = vite.httpServer?.address();
      if (!ui || typeof ui === "string") throw new Error("Fixture renderer did not listen");
      browser = await chromium.launch({ channel: "chrome" });
      const page = await browser.newPage();
      await page.goto(`http://127.0.0.1:${ui.port}/tests/fixtures/agent-workspace.html?liveWorkspace=${encodeURIComponent(`http://127.0.0.1:${api.port}`)}`);
      await page.waitForFunction(() => window.workspaceStreamReady === true);
      await page.getByRole("tab", { name: /^Diff/ }).click();
      if (missing) {
        await browserExpect(page.getByRole("alert")).toContainText("missing-baseline");
        await browserExpect(page.getByText("No changes to display.", { exact: true })).toHaveCount(0);
        if (kind === "channel-close") {
          const stopped = child;
          if (!stopped) throw new Error("Fixture child is not running");
          stopped.kill("SIGSTOP");
          await vi.waitFor(async () => expect((await run("ps", ["-o", "state=", "-p", String(stopped.pid)])).stdout).toContain("T"), { timeout: 5_000 });
          const read = ssh.waitForRequest("task_compute_diff");
          await page.getByRole("button", { name: "Reload changes", exact: true }).click();
          await read;
          ssh.breakChannel();
          await browserExpect(page.getByRole("alert")).toContainText(/channel closed|output ended/);
          await browserExpect(page.getByText("No changes to display.", { exact: true })).toHaveCount(0);
          stopped.kill("SIGCONT");
          await writeFile(path.join(liveRoot!, "sidecar-diff-ssh-closure.json"), JSON.stringify({ error: await page.getByRole("alert").textContent() }));
          // Replacing an SSH channel requires a new sidecar process; the successful
          // peer case proves normal reads. Reusing the closed stream would hide it.
          await browser.close(); browser = undefined;
          await vite.close(); vite = undefined;
          for (const client of clients) client.end();
          await new Promise<void>(resolve => bridge!.close(() => resolve())); bridge = undefined;
          await Promise.allSettled([...operations]);
          expect({ exit: await closed, index: await readFile(path.join(directory, ".git/index")) }).toEqual({ exit: 0, index });
          return;
        }
        task.baseRef = "HEAD";
        await page.getByRole("button", { name: "Reload changes", exact: true }).click();
      }
      for (const file of ["tracked.txt", "雪.txt"]) await browserExpect(page.getByText(file, { exact: true }).first()).toBeVisible();
      if (kind === "reconnect") {
        await writeFile(path.join(directory, "tracked.txt"), "reconnected\n");
        ssh.breakChannel();
        await vi.waitFor(() => expect(manager.getStatus("fixture")).toBe("reconnecting"), { timeout: 5_000 });
        for (const client of clients) client.write(`data: ${JSON.stringify({ channel: "orchestrator:files-changed", payload: { taskId: task.id } })}\n\n`);
        await browserExpect(page.getByText("reconnected", { exact: true })).toBeVisible();
        expect({ status: manager.getStatus("fixture"), processes: children.length, oldExit: await children[0].closed }).toEqual({ status: "connected", processes: 2, oldExit: 0 });
      }
      expect(await readFile(path.join(directory, ".git/index"))).toEqual(index);
      await page.screenshot({ path: path.join(liveRoot!, `sidecar-diff-ssh-${kind}.png`) });
      await writeFile(path.join(liveRoot!, `sidecar-diff-ssh-${kind}.json`), JSON.stringify({ result, stderr }, null, 2));
      if (kind === "success") {
        await router.sendToRemoteAgent("fixture", "open_local", { repo_id: "fixture", path: directory });
        await router.sendToRemoteAgent("fixture", "write_text", { repo_id: "fixture", path: "tracked.txt", content: "connected-write\n" });
        expect(await readFile(path.join(directory, "tracked.txt"), "utf8")).toBe("connected-write\n");
        const connection = manager.getConnection("fixture");
        if (!connection) throw new Error("Fixture connection unavailable");
        connection.status = "reconnecting";
        await expect(router.sendToRemoteAgent("fixture", "write_text", { repo_id: "fixture", path: "tracked.txt", content: "deferred-write\n" }))
          .rejects.toMatchObject({ code: "SSH_RECONNECTING" });
        const queued = router.sendToRemoteAgent("fixture", "task_compute_diff", { worktree_path: directory, base_ref: "HEAD" });
        connection.status = "connected";
        await router.replayQueue("fixture");
        const comparison = await queued as { diff: string };
        expect({ content: await readFile(path.join(directory, "tracked.txt"), "utf8"), diff: comparison.diff, index: await readFile(path.join(directory, ".git/index")) })
          .toEqual({ content: "connected-write\n", diff: expect.stringContaining("+connected-write"), index });
        await writeFile(path.join(liveRoot!, "sidecar-ssh-replay-admission.json"), JSON.stringify({ content: "connected-write\n", rejected: "SSH_RECONNECTING", readReplayed: true }));
      }
      await browser.close(); browser = undefined;
      await vite.close(); vite = undefined;
      for (const client of clients) client.end();
      await new Promise<void>(resolve => bridge!.close(() => resolve())); bridge = undefined;
      await Promise.allSettled([...operations]);
      ssh.channel.end();
      expect(await closed).toBe(0);
    } finally {
      child?.kill("SIGCONT");
      await browser?.close();
      await vite?.close();
      for (const client of clients) client.end();
      if (bridge) await new Promise<void>(resolve => bridge!.close(() => resolve()));
      await Promise.allSettled([...operations]);
      for (const child of children) if (child.process.exitCode === null && child.process.signalCode === null) child.process.kill("SIGKILL");
      await Promise.allSettled(children.map(child => child.closed));
      await ssh?.close();
      await rm(directory, { recursive: true, force: true });
      await rm(`${directory}-vite-cache`, { recursive: true, force: true });
    }
  }, 60_000);
});
