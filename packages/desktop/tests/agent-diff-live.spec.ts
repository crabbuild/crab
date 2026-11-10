import { describe, expect, it, vi } from "vitest";
import { spawn, execFile, type ChildProcess } from "node:child_process";
import { generateKeyPairSync } from "node:crypto";
import { once } from "node:events";
import { createServer, type Server, type ServerResponse } from "node:http";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer as createViteServer, type ViteDevServer } from "vite";
import { promisify } from "node:util";
import { homedir } from "node:os";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Client, Server as SshServer, type ClientChannel, type ServerChannel } from "ssh2";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import type { SshSessionManager } from "../electron/ssh-manager";
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

async function sidecarChannel(child: ChildProcess): Promise<{ channel: ClientChannel; breakChannel: () => void; waitForRequest: (method: string) => Promise<void>; close: () => Promise<void> }> {
  if (!child.stdout || !child.stdin) throw new Error("Sidecar pipes unavailable");
  const input = child.stdin; const output = child.stdout;
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  let remote: ServerChannel | undefined;
  const server = new SshServer({ hostKeys: [privateKey.export({ type: "pkcs1", format: "pem" })] }, connection => {
    connection.on("error", () => {});
    connection.on("authentication", context => {
      if (context.username === "fixture" && context.method === "none") context.accept();
      else context.reject();
    });
    connection.on("ready", () => connection.on("session", accept => {
      accept().on("exec", (start, reject, info) => {
        if (info.command !== "owned-sidecar") { reject(); return; }
        remote = start();
        remote.on("error", () => {});
        remote.once("close", () => input.end());
        remote.pipe(input); output.pipe(remote);
      });
    }));
  });
  const client = new Client();
  let started = false; let clientClosed = false;
  client.on("close", () => { clientClosed = true; });
  client.on("error", () => {});
  const close = async () => {
    if (started && !clientClosed) {
      const closed = new Promise<void>(resolve => client.once("close", () => resolve())); client.destroy(); await closed;
    }
    if (server.listening) await new Promise<void>(resolve => server.close(() => resolve()));
  };
  try {
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("SSH fixture did not listen");
    const ready = once(client, "ready");
    started = true;
    client.connect({ host: "127.0.0.1", port: address.port, username: "fixture", readyTimeout: 5_000 });
    await ready;
    const channel = await new Promise<ClientChannel>((resolve, reject) => client.exec("owned-sidecar", (error, stream) => error ? reject(error) : resolve(stream)));
    const waitForRequest = (method: string) => new Promise<void>(resolve => {
      let buffer = "";
      const data = (chunk: Buffer) => {
        const lines = (buffer + chunk.toString("utf8")).split("\n"); buffer = lines.pop() ?? "";
        if (lines.some(line => JSON.parse(line).method === method)) { remote?.removeListener("data", data); resolve(); }
      };
      remote?.on("data", data);
    });
    return { channel, breakChannel: () => remote?.close(), waitForRequest, close };
  } catch (error: unknown) {
    await close(); throw error;
  }
}

// The macOS fixture denies the sidecar access to the user's indexer database.
// A loopback SSH server carries the built sidecar RPC stream; deployment and
// remote CLI credentials remain outside this transport qualification.
describe.skipIf(!liveRoot || !target || process.platform !== "darwin")("built sidecar file comparisons", () => {
  it.each(["success", "baseline-retry", "channel-close"] as const)("preserves the real index over SSH: %s", async kind => {
    const missing = kind !== "success";
    const directory = await mkdtemp(path.join(liveRoot!, "sidecar-diff-"));
    const run = promisify(execFile);
    const binary = path.join(target!, "debug", "crab-desktop-agent");
    let stderr = "";
    let child: ReturnType<typeof spawn> | undefined;
    let closed: Promise<number | null> | undefined;
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
      child = spawn("/usr/bin/sandbox-exec", ["-p", profile, binary], { cwd: directory, stdio: ["pipe", "pipe", "pipe"] });
      closed = new Promise((resolve, reject) => { child!.once("error", reject); child!.once("close", resolve); });
      child.stderr?.on("data", chunk => { stderr += chunk.toString(); });
      ssh = await sidecarChannel(child);
      const { channel } = ssh;
      let status = "connected";
      const manager = { getStatus: () => status, getConnection: () => ({ channels: { agent: channel } }) } as unknown as SshSessionManager;
      const router = new RpcRouter(manager, { request: async () => { throw new Error("Local fallback must not run"); } });
      router.attachChannel("fixture", channel);
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
          child.kill("SIGSTOP");
          await vi.waitFor(async () => expect((await run("ps", ["-o", "state=", "-p", String(child!.pid)])).stdout).toContain("T"), { timeout: 5_000 });
          const read = ssh.waitForRequest("task_compute_diff");
          await page.getByRole("button", { name: "Reload changes", exact: true }).click();
          await read;
          ssh.breakChannel();
          await browserExpect(page.getByRole("alert")).toContainText(/channel closed|output ended/);
          await browserExpect(page.getByText("No changes to display.", { exact: true })).toHaveCount(0);
          child.kill("SIGCONT");
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
      expect(await readFile(path.join(directory, ".git/index"))).toEqual(index);
      await page.screenshot({ path: path.join(liveRoot!, `sidecar-diff-ssh-${kind}.png`) });
      await writeFile(path.join(liveRoot!, `sidecar-diff-ssh-${kind}.json`), JSON.stringify({ result, stderr }, null, 2));
      if (kind === "success") {
        await router.sendToRemoteAgent("fixture", "open_local", { repo_id: "fixture", path: directory });
        await router.sendToRemoteAgent("fixture", "write_text", { repo_id: "fixture", path: "tracked.txt", content: "connected-write\n" });
        expect(await readFile(path.join(directory, "tracked.txt"), "utf8")).toBe("connected-write\n");
        status = "reconnecting";
        await expect(router.sendToRemoteAgent("fixture", "write_text", { repo_id: "fixture", path: "tracked.txt", content: "deferred-write\n" }))
          .rejects.toMatchObject({ code: "SSH_RECONNECTING" });
        const queued = router.sendToRemoteAgent("fixture", "task_compute_diff", { worktree_path: directory, base_ref: "HEAD" });
        status = "connected";
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
      channel.end();
      expect(await closed).toBe(0);
    } finally {
      child?.kill("SIGCONT");
      await browser?.close();
      await vite?.close();
      for (const client of clients) client.end();
      if (bridge) await new Promise<void>(resolve => bridge!.close(() => resolve()));
      await Promise.allSettled([...operations]);
      if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      if (closed) await closed;
      await ssh?.close();
      await rm(directory, { recursive: true, force: true });
      await rm(`${directory}-vite-cache`, { recursive: true, force: true });
    }
  }, 60_000);
});
