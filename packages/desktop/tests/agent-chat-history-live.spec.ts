import { describe, expect, it, vi } from "vitest";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer as createViteServer } from "vite";
import { createServer, type ServerResponse } from "node:http";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import { computeDiff } from "../electron/orchestrator-diff";
import { readTaskMetadata } from "../electron/orchestrator-worktree";
import type { AgentEvent, AgentInputResponse } from "../electron/orchestrator-adapters";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { CodexRpcClient } from "../electron/codex-rpc";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const turnTimeoutMs = 300_000;
const turnObservationMs = turnTimeoutMs + 10_000;
const lifecycleTimeoutMs = 3 * turnTimeoutMs + 60_000;
const lifecycleCases = ["codex", "claude-code"].flatMap(agent => ["missing-workspace", "checkpoint-capture"].map(failure => ({ agent, failure })));
async function fixturePort(): Promise<number> {
  const reservation = createServer();
  await new Promise<void>((resolve, reject) => { reservation.once("error", reject); reservation.listen(0, "127.0.0.1", resolve); });
  const address = reservation.address();
  await new Promise<void>(resolve => reservation.close(() => resolve()));
  if (!address || typeof address === "string") throw new Error("Fixture port was not allocated");
  return address.port;
}

describe.skipIf(!liveRoot)("durable provider chat in the renderer", () => {
  it.each(lifecycleCases)("$agent archives, restores, and retries $failure through the task view", async ({ agent, failure }) => {
    const cwd = await mkdtemp(path.join(liveRoot!, "workspace-lifecycle-"));
    const artifact = (suffix: string) => path.join(liveRoot!, `${agent}-${failure}-workspace-lifecycle${suffix}`);
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: turnTimeoutMs });
    const db = new ChatDatabase(path.join(cwd, "chat.sqlite"));
    const clients = new Set<ServerResponse>();
    const operations = new Set<Promise<void | object>>();
    const operationResults: Array<{ operation: string; success: boolean; error?: string }> = [];
    const lifecycle: Array<{ phase: string; at: number; detail?: unknown }> = [];
    const rolloutPaths = new Map<string, boolean>();
    const observeRollouts = () => {
      for (const [rollout, previous] of rolloutPaths) {
        const exists = existsSync(rollout);
        if (exists === previous) continue;
        rolloutPaths.set(rollout, exists);
        lifecycle.push({ phase: "rollout-existence", at: Date.now(), detail: { path: rollout, exists } });
      }
    };
    const rolloutObserver = setInterval(observeRollouts, 50);
    const send = CodexRpcClient.prototype.send;
    const rpcTrace = vi.spyOn(CodexRpcClient.prototype, "send").mockImplementation(async function(this: CodexRpcClient, ...args: Parameters<typeof send>) {
      const result = await send.apply(this, args);
      if (args[0] === "thread/start" || args[0] === "thread/resume") {
        const thread = (result as { thread: { id: string; path: string | null; ephemeral: boolean } }).thread;
        lifecycle.push({ phase: args[0], at: Date.now(), detail: { id: thread.id, path: thread.path, ephemeral: thread.ephemeral } });
        if (thread.path) rolloutPaths.set(thread.path, existsSync(thread.path));
      }
      return result;
    });
    const cancel = CodexAppServerAdapter.prototype.cancel;
    const cancellation = vi.spyOn(CodexAppServerAdapter.prototype, "cancel").mockImplementation(function(this: CodexAppServerAdapter) {
      lifecycle.push({ phase: "native-close-start", at: Date.now() });
      return cancel.call(this).finally(() => lifecycle.push({ phase: "native-close-complete", at: Date.now() }));
    });
    const publish = (channel: string, payload: unknown) => {
      for (const client of clients) client.write(`data: ${JSON.stringify({ channel, payload })}\n\n`);
    };
    const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
    const ipc = { handle: (channel: string, handler: Parameters<IpcMain["handle"]>[1]) => { handlers.set(channel, handler); } };
    const chat = registerChatIpc(ipc, orchestrator, db, () => ({ isDestroyed: () => false, webContents: { send: publish } }));
    const events: AgentEvent[] = [];
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload.event);
      publish("orchestrator:event", payload);
      if (payload.event.approval) orchestrator.recordApproval(payload.taskId, { requestId: payload.event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
    });
    orchestrator.on("orchestrator:task-status", payload => publish("orchestrator:task-status", payload));
    orchestrator.on("orchestrator:files-changed", payload => publish("orchestrator:files-changed", payload));
    orchestrator.on("orchestrator:error", payload => { lifecycle.push({ phase: "error", at: Date.now(), detail: payload }); publish("orchestrator:error", payload); });
    let taskId: string | undefined;
    let browser: Browser | undefined;
    const vite = await createViteServer({ cacheDir: `${cwd}-vite-cache`, server: { host: "127.0.0.1", port: await fixturePort(), strictPort: true }, clearScreen: false });
    const api = createServer(async (request, response) => {
      response.setHeader("Access-Control-Allow-Origin", "*");
      response.setHeader("Access-Control-Allow-Headers", "Content-Type");
      response.setHeader("Access-Control-Allow-Methods", "GET,POST,OPTIONS");
      response.setHeader("Content-Type", "application/json");
      if (request.method === "OPTIONS") { response.end(); return; }
      try {
        if (request.url === "/events") {
          response.setHeader("Content-Type", "text/event-stream");
          response.writeHead(200); response.write("\n");
          clients.add(response); response.once("close", () => clients.delete(response)); return;
        }
        if (!taskId) throw new Error("Task not created");
        const chunks: Buffer[] = [];
        for await (const chunk of request) chunks.push(Buffer.from(chunk));
        const body = JSON.parse(Buffer.concat(chunks).toString()) as Record<string, unknown>;
        if (request.url === "/listTasks") { response.end(JSON.stringify({ success: true, tasks: [orchestrator.getTask(taskId)] })); return; }
        if (body.taskId !== taskId) throw new Error("Unexpected task owner");
        if (request.url === "/listCheckpoints") {
          const task = orchestrator.getTask(taskId)!;
          const checkpoints = new CheckpointManager();
          response.end(JSON.stringify({ success: true, checkpoints: await checkpoints.listCheckpoints(task), backups: await checkpoints.listRestoreBackups(task) })); return;
        }
        if (request.url === "/getGoal") { response.end(JSON.stringify({ success: true, goal: await orchestrator.getGoal(taskId) })); return; }
        if (request.url === "/getDiff") { response.end(JSON.stringify({ success: true, diff: await computeDiff(orchestrator.getTask(taskId)!) })); return; }
        if (request.url === "/loadChat" || request.url === "/iterate") {
          const handler = handlers.get(request.url === "/loadChat" ? "orchestrator:load-chat" : "orchestrator:iterate");
          if (!handler) throw new Error("Chat IPC not registered");
          response.end(JSON.stringify(await handler({} as IpcMainInvokeEvent, body))); return;
        }
        let operation: Promise<void | object>;
        lifecycle.push({ phase: request.url ?? "unknown", at: Date.now() });
        if (request.url === "/archiveTask") operation = orchestrator.archiveTask(taskId);
        else if (request.url === "/restoreTask") operation = orchestrator.restoreTask(taskId);
        else if (request.url === "/retryTask") operation = orchestrator.retryTask(taskId);
        else if (request.url === "/startReview") operation = orchestrator.startReview(taskId);
        else if (request.url === "/applyAll") operation = orchestrator.applyTask(taskId);
        else if (request.url === "/runWorkspace") operation = orchestrator.runWorkspace(taskId);
        else if (request.url === "/restoreFiles") operation = orchestrator.restoreFiles(taskId, String(body.sourceRef));
        else throw new Error("Unknown fixture operation");
        operations.add(operation);
        let result: void | object;
        try { result = await operation; } finally { operations.delete(operation); }
        const success = !result || !("success" in result) || result.success === true;
        operationResults.push({ operation: request.url, success });
        response.end(JSON.stringify({ success: true, ...(result ?? {}) }));
      } catch (error: unknown) {
        const message = error instanceof Error ? error.message : String(error);
        operationResults.push({ operation: request.url ?? "unknown", success: false, error: message });
        response.end(JSON.stringify({ success: false, error: message }));
      }
    });
    try {
      await vite.listen();
      const run = promisify(execFile);
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["config", "user.name", "Crab Fixture"], { cwd });
      await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd });
      await run("git", ["commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
      const quote = (value: string) => "'" + value.replaceAll("'", "'\\''") + "'";
      const counter = path.join(cwd, "setup-count.txt");
      const preparation = `const fs=require('node:fs');let count=0;try{count=Number(fs.readFileSync(${JSON.stringify(counter)},'utf8'))}catch{}fs.writeFileSync(${JSON.stringify(counter)},String(count+1));fs.writeFileSync('setup-proof.txt','native-saved');console.log('SETUP_READY')`;
      const setup = `${quote(process.execPath)} -e ${quote(preparation)}`;
      await writeFile(path.join(cwd, "settings.json"), JSON.stringify({ orchestratorSetupScript: setup }));
      const initialPrompt = "Read setup-proof.txt and copy its exact contents to archive-proof.txt without a newline. Then reply exactly ARCHIVE_READY. Do not modify other files or use subagents.";
      const task = await orchestrator.createTask(initialPrompt, agent, cwd, "HEAD");
      taskId = task.id;
      if (agent === "claude-code") {
        const metadata = readTaskMetadata(task.worktreePath);
        if (!metadata) throw new Error("Task ownership missing");
        const { stdout } = await run("git", ["rev-parse", "--absolute-git-dir"], { cwd: task.worktreePath });
        await writeFile(path.join(task.worktreePath, ".crab-task.json"), JSON.stringify(metadata));
        await rm(path.join(stdout.trim(), "crab-desktop-task.json"));
        await run("git", ["add", ".crab-task.json"], { cwd: task.worktreePath });
        await run("git", ["commit", "-m", "Tagged ownership layout"], { cwd: task.worktreePath });
      }
      await orchestrator.startTask(task.id);
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: turnObservationMs, interval: 100 });
      expect({ count: await readFile(counter, "utf8"), baseline: (await run("git", ["show", `refs/crab/checkpoints/${task.id}/0:setup-proof.txt`], { cwd })).stdout,
        file: await readFile(path.join(task.worktreePath, "archive-proof.txt"), "utf8") }).toEqual({ count: "1", baseline: "native-saved", file: "native-saved" });
      const session = agent === "codex" ? task.codexThreadId : task.claudeSessionId;
      const firstReply = chat.snapshot(task.id).messages.filter(message => message.kind === "text").at(-1)?.content;
      if (!firstReply || !session) throw new Error("Native completion or session identity missing");
      // Settle the native owner and its Git writers before inserting the
      // fixture regression. Restore also forces review to resume a cold CLI.
      await orchestrator.archiveTask(task.id);
      await orchestrator.restoreTask(task.id);
      const originalPath = task.worktreePath;
      // A committed regression must remain reviewable after automatic snapshots
      // and a provider restart. Uncommitted-only review silently misses it.
      const regression = "/** Returns the sum of both inputs. */\nexport function sum(a: number, b: number): number { return a - b; }\n";
      await writeFile(path.join(task.worktreePath, "sum.ts"), regression);
      await run("git", ["add", "sum.ts"], { cwd: task.worktreePath });
      await run("git", ["commit", "-m", "Owned review regression"], { cwd: task.worktreePath });
      await new Promise<void>((resolve, reject) => { api.once("error", reject); api.listen(0, "127.0.0.1", resolve); });
      const ui = vite.httpServer?.address();
      const bridge = api.address();
      if (!ui || typeof ui === "string" || !bridge || typeof bridge === "string") throw new Error("Fixture did not listen");
      browser = await chromium.launch({ channel: "chrome" });
      const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
      page.on("pageerror", error => lifecycle.push({ phase: "page-error", at: Date.now(), detail: error.message }));
      await page.context().tracing.start({ screenshots: true, snapshots: true });
      page.on("request", request => {
        if (request.url().includes(`${bridge.port}/archiveTask`)) lifecycle.push({ phase: "browser-archive-request", at: Date.now() });
      });
      await page.goto(`http://127.0.0.1:${ui.port}/tests/fixtures/agent-workspace.html?liveWorkspace=${encodeURIComponent(`http://127.0.0.1:${bridge.port}`)}`);
      await browserExpect(page.getByText(firstReply, { exact: true })).toBeVisible();
      await page.waitForFunction(() => window.workspaceStreamReady === true);
      await page.getByRole("button", { name: /Worked for/ }).first().click();
      await page.getByRole("button", { name: /^\d+ tool calls?/ }).first().click();
      await page.getByRole("button", { name: /^Workspace setup/ }).first().click();
      await browserExpect(page.getByText("SETUP_READY", { exact: true })).toBeVisible();
      await page.getByRole("tab", { name: /^Diff/ }).click();
      await page.getByRole("button", { name: "Review with AI", exact: true }).click();
      await vi.waitFor(() => expect(chat.snapshot(task.id).messages.filter(message => message.kind === "done")).toHaveLength(2), { timeout: turnObservationMs, interval: 100 });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 15_000, interval: 100 });
      const review = chat.snapshot(task.id).messages.filter(message => message.kind === "text").at(-1)?.content;
      if (!review) throw new Error("Native review output is missing");
      expect({ finding: /sum\.ts/.test(review) && /subtract|\bsum\b|\badd\b/i.test(review), file: await readFile(path.join(task.worktreePath, "sum.ts"), "utf8"),
        session: agent === "codex" ? task.codexThreadId : task.claudeSessionId, kind: task.lastInputKind }).toEqual({ finding: true, file: regression, session, kind: "review" });
      const reviewOutput = page.getByRole("log", { name: "Chat transcript" }).getByText(/sum\.ts/).last();
      await reviewOutput.scrollIntoViewIfNeeded();
      await browserExpect(reviewOutput).toBeVisible();
      const beforeRestore = { feedback: [...task.feedback], turn: task.turnNumber, session: agent === "codex" ? task.codexThreadId : task.claudeSessionId,
        messages: chat.snapshot(task.id).messages.map(message => message.id) };
      await page.getByRole("button", { name: "Saved file states", exact: true }).click();
      await page.getByRole("button", { name: "Restore files before turn 0", exact: true }).click();
      await vi.waitFor(() => expect(existsSync(path.join(task.worktreePath, "archive-proof.txt"))).toBe(false), { timeout: 20_000 });
      await browserExpect(page.getByText("Backups", { exact: true })).toBeVisible();
      await page.getByRole("button", { name: /^Restore files saved/ }).first().click();
      await vi.waitFor(async () => expect(await readFile(path.join(task.worktreePath, "archive-proof.txt"), "utf8")).toBe("native-saved"), { timeout: 20_000 });
      expect({ feedback: task.feedback, turn: task.turnNumber, session: agent === "codex" ? task.codexThreadId : task.claudeSessionId,
        messages: chat.snapshot(task.id).messages.map(message => message.id) }).toEqual(beforeRestore);
      await page.getByRole("button", { name: "Saved file states", exact: true }).click();
      // Exercise the product Run shortcut and Apply action while the native
      // conversation is still owned. Apply must retire all filesystem writers.
      const writer = `${quote(process.execPath)} ${quote(path.resolve("tests/fixtures/workspace-writer.mjs"))} ${quote(cwd)}`;
      const hook = `${quote(process.execPath)} -e ${quote("require('node:fs').writeFileSync('archive-hook.txt', 'hook-saved')")}`;
      await writeFile(path.join(cwd, "settings.json"), JSON.stringify({ orchestratorSetupScript: setup, orchestratorRunScript: writer, orchestratorArchiveScript: hook }));
      await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+R" : "Control+Shift+R");
      await vi.waitFor(() => expect(existsSync(path.join(cwd, "child.writes"))).toBe(true), { timeout: 10_000 });
      const writerPids = await Promise.all(["parent", "child"].map(async name => Number(await readFile(path.join(cwd, `${name}.pid`), "utf8"))));
      await page.getByRole("tab", { name: /^Diff/ }).click();
      await page.getByRole("button", { name: "Apply all", exact: true }).click();
      await vi.waitFor(() => expect(task.status).toBe("applied"), { timeout: 20_000 });
      expect(writerPids.map(pid => {
        try { process.kill(pid, 0); return true; } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; }
      })).toEqual([false, false]);
      await page.getByRole("button", { name: "Archive", exact: true }).click();
      await browserExpect(page.getByRole("button", { name: "Restore", exact: true })).toBeVisible({ timeout: 30_000 });
      await page.getByRole("tab", { name: "Chat", exact: true }).click();
      await browserExpect(page.getByRole("textbox", { name: "Chat message input", exact: true })).toBeDisabled();
      expect({ status: task.status, removed: !existsSync(originalPath), mainFile: await readFile(path.join(cwd, "archive-proof.txt"), "utf8") }).toEqual({ status: "archived", removed: true, mainFile: "native-saved" });
      expect((await run("git", ["ls-files", "--", ".crab-task.json"], { cwd })).stdout.trim()).toBe("");
      await page.getByRole("button", { name: "Restore", exact: true }).click();
      await browserExpect(page.getByRole("button", { name: "Restore", exact: true })).toHaveCount(0, { timeout: 30_000 });
      await browserExpect(reviewOutput).toBeVisible();
      expect(await readFile(path.join(task.worktreePath, "archive-proof.txt"), "utf8")).toBe("native-saved");
      expect(await readFile(path.join(task.worktreePath, "archive-hook.txt"), "utf8")).toBe("hook-saved");
      if (failure === "missing-workspace") {
        await rm(task.worktreePath, { recursive: true, force: true });
      } else {
        await writeFile(path.join(task.worktreePath, ".gitattributes"), "*.bin filter=checkpoint-fixture\n");
        await run("git", ["config", "filter.checkpoint-fixture.clean", "exit 17"], { cwd });
        await run("git", ["config", "filter.checkpoint-fixture.required", "true"], { cwd });
        await writeFile(path.join(task.worktreePath, "payload.bin"), "pending bytes");
      }
      await page.getByRole("textbox", { name: "Chat message input", exact: true }).fill("Append exactly -resumed to archive-proof.txt without adding a newline. Then reply exactly RESUMED_READY. Do not modify other files or use subagents.");
      await page.getByRole("button", { name: "Send message", exact: true }).click();
      await browserExpect(page.getByRole("button", { name: "Retry", exact: true })).toBeVisible({ timeout: 15_000 });
      await browserExpect(page.getByText(/Message was not sent/).first()).toBeVisible();
      expect(chat.snapshot(task.id).messages.filter(message => message.kind === "done")).toHaveLength(2);
      if (failure === "checkpoint-capture") expect(await readFile(path.join(task.worktreePath, "archive-proof.txt"), "utf8")).toBe("native-saved");
      await page.reload();
      await page.waitForFunction(() => window.workspaceStreamReady === true);
      await browserExpect(page.getByText(/Append exactly -resumed/)).toBeVisible();
      if (failure === "checkpoint-capture") {
        await rm(path.join(task.worktreePath, ".gitattributes"));
        await rm(path.join(task.worktreePath, "payload.bin"));
        await run("git", ["config", "--remove-section", "filter.checkpoint-fixture"], { cwd });
      }
      await writeFile(path.join(cwd, "settings.json"), JSON.stringify({ orchestratorSetupScript: setup, orchestratorRunScript: writer, orchestratorArchiveScript: hook }));
      await page.getByRole("button", { name: "Retry", exact: true }).click();
      await vi.waitFor(() => expect(chat.snapshot(task.id).messages.filter(message => message.kind === "done")).toHaveLength(3), { timeout: turnObservationMs, interval: 100 });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 15_000, interval: 100 });
      const snapshot = chat.snapshot(task.id);
      const reply = snapshot.messages.filter(message => message.kind === "text").at(-1)?.content;
      if (!reply) throw new Error("Resumed provider reply missing");
      const visibleReply = page.getByText(reply, { exact: true });
      await visibleReply.scrollIntoViewIfNeeded();
      await browserExpect(visibleReply).toBeInViewport();
      await browserExpect(page.getByText(/^Complete ·/)).toBeVisible();
      expect({ original: task.prompt, feedback: task.feedback.length, turn: task.turnNumber }).toEqual({
        original: initialPrompt, feedback: 2, turn: 2,
      });
      expect(await readFile(counter, "utf8")).toBe("3");
      expect({ file: await readFile(path.join(task.worktreePath, "archive-proof.txt"), "utf8"), session: agent === "codex" ? task.codexThreadId : task.claudeSessionId, outcomes: snapshot.messages.filter(message => message.kind === "done").map(message => message.outcome), restored: task.worktreePath !== originalPath }).toEqual({ file: "native-saved-resumed", session, outcomes: ["completed", "completed", "completed"], restored: true });
      await page.screenshot({ path: artifact(".png") });
      await writeFile(artifact(".json"), JSON.stringify({ task, snapshot, events, operationResults, lifecycle }, null, 2));
    } catch (error: unknown) {
      await writeFile(artifact("-failure.json"), JSON.stringify({ task: taskId && orchestrator.getTask(taskId), snapshot: taskId && chat.snapshot(taskId), events, operationResults, lifecycle }, null, 2));
      const page = browser?.contexts()[0]?.pages()[0];
      if (page) {
        await page.screenshot({ path: artifact("-failure.png") });
        await page.context().tracing.stop({ path: artifact("-failure.zip") });
      }
      throw error;
    } finally {
      const cleanup = async (phase: string) => {
        lifecycle.push({ phase, at: Date.now() });
        await writeFile(artifact("-closure.json"), JSON.stringify({ operationResults, lifecycle }, null, 2));
      };
      await cleanup("browser-close-start");
      await browser?.close();
      await cleanup("browser-close-complete");
      await Promise.allSettled([...operations]);
      await cleanup("operations-settled");
      await orchestrator.shutdown();
      await cleanup("owners-closed");
      clearInterval(rolloutObserver);
      rpcTrace.mockRestore();
      cancellation.mockRestore();
      await vite.close();
      await cleanup("vite-close-complete");
      for (const client of clients) client.end();
      await new Promise<void>(resolve => api.close(() => resolve()));
      await cleanup("bridge-close-complete");
      db.close(); vi.unstubAllEnvs();
      await rm(cwd, { recursive: true, force: true });
      await rm(`${cwd}-vite-cache`, { recursive: true, force: true });
    }
  }, lifecycleTimeoutMs);

  it.each(["codex", "claude-code"])("%s answers a child request first opened after background startup", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, "chat-history-"));
    const proof = path.join(cwd, "mcp-proof.json");
    const script = path.resolve("tests/fixtures/mcp-elicitation-server.mjs");
    const args = [script, proof, "tool", "form"];
    const command = process.execPath;
    const codexConfig = `mcp_servers={compatibility_fixture={command=${JSON.stringify(command)},args=${JSON.stringify(args)},env={ELECTRON_RUN_AS_NODE="1"}}}`;
    const claudeConfig = JSON.stringify({ mcpServers: { compatibility_fixture: { command, args, env: { ELECTRON_RUN_AS_NODE: "1" } } } });
    const extraArgs = agent === "codex" ? ["-c", codexConfig, "-c", 'approval_policy="on-request"'] : ["--mcp-config", claudeConfig, "--strict-mcp-config"];
    const settings = path.join(cwd, "settings.json");
    await writeFile(settings, JSON.stringify({ orchestratorAgentConfigs: { [agent]: { extraArgs,
      ...(agent === "codex" ? { binaryPath: path.resolve("tests/fixtures/codex-lost-ack.mjs") } : {}),
    } } }));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", settings);
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 120_000 });
    const db = new ChatDatabase(path.join(cwd, "chat.sqlite"));
    const clients = new Set<ServerResponse>();
    const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
    const ipc = { handle: (channel: string, handler: Parameters<IpcMain["handle"]>[1]) => { handlers.set(channel, handler); } };
    const chat = registerChatIpc(ipc, orchestrator, db, () => ({ isDestroyed: () => false, webContents: {
      send: (channel, update) => {
        if (channel === "orchestrator:chat-update") for (const client of clients) client.write(`data: ${JSON.stringify(update)}\n\n`);
      },
    } }));
    const events: Array<{ taskId: string; event: AgentEvent }> = [];
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload);
      if (payload.event.type === "approval_request" && payload.event.approval) {
        orchestrator.recordApproval(payload.taskId, { requestId: payload.event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
      }
    });
    let childId: string | undefined;
    let browser: Browser | undefined;
    const vite = await createViteServer({ cacheDir: `${cwd}-vite-cache`, server: { host: "127.0.0.1", port: await fixturePort(), strictPort: true }, clearScreen: false });
    const api = createServer(async (request, response) => {
      response.setHeader("Access-Control-Allow-Origin", "*");
      response.setHeader("Access-Control-Allow-Headers", "Content-Type");
      response.setHeader("Access-Control-Allow-Methods", "GET,POST,OPTIONS");
      response.setHeader("Content-Type", "application/json");
      if (request.method === "OPTIONS") { response.end(); return; }
      try {
        if (request.url === "/events") {
          response.setHeader("Content-Type", "text/event-stream");
          response.writeHead(200);
          response.write("\n");
          clients.add(response);
          response.once("close", () => clients.delete(response));
          return;
        }
        if (request.url === "/chat" && childId) {
          const load = handlers.get("orchestrator:load-chat");
          if (!load) throw new Error("Chat IPC was not registered");
          response.end(JSON.stringify(await load({} as IpcMainInvokeEvent, { taskId: childId })));
          return;
        }
        if (request.url === "/iterate" && request.method === "POST" && childId) {
          const chunks: Buffer[] = [];
          for await (const chunk of request) chunks.push(Buffer.from(chunk));
          const { feedback } = JSON.parse(Buffer.concat(chunks).toString()) as { feedback: string };
          const submit = handlers.get("orchestrator:iterate");
          if (!submit) throw new Error("Chat submission IPC was not registered");
          response.end(JSON.stringify(await submit({} as IpcMainInvokeEvent, { taskId: childId, feedback, messageId: "live-follow-up" })));
          return;
        }
        if (request.url !== "/input" || request.method !== "POST") { response.writeHead(404).end(); return; }
        const chunks: Buffer[] = [];
        for await (const chunk of request) chunks.push(Buffer.from(chunk));
        const input = JSON.parse(Buffer.concat(chunks).toString()) as { taskId: string; requestId: string; response: AgentInputResponse };
        const success = input.taskId === childId && orchestrator.recordInput(input.taskId, input.requestId, input.response);
        response.end(JSON.stringify({ success, error: success ? undefined : "Request no longer pending" }));
      } catch (error: unknown) {
        response.writeHead(500).end(JSON.stringify({ success: false, error: error instanceof Error ? error.message : String(error) }));
      }
    });
    try {
      const run = promisify(execFile);
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
      const parent = await orchestrator.createTask("Coordinate", agent, cwd, "HEAD", { worktreeMode: "primary" });
      const prompt = "Call the compatibility_fixture confirm_values MCP tool once. Then reply exactly Ready. Do not use other tools or modify files.";
      const child = orchestrator.createChildTaskForAgent(parent.id, agent, prompt, "reviewer");
      childId = child.id;
      await vite.listen();
      const uiAddress = vite.httpServer?.address();
      if (!uiAddress || typeof uiAddress === "string") throw new Error("Fixture UI did not listen");
      const fixture = `http://127.0.0.1:${uiAddress.port}/tests/fixtures/agent-requests.html`;
      browser = await chromium.launch({ channel: "chrome" });
      const page = await browser.newPage();
      // Compile the development fixture before starting the provider's own
      // MCP startup deadline. The real chat view remains unmounted.
      await page.goto(fixture);
      await browserExpect(page.getByRole("region", { name: "Durable transcript" })).toBeVisible();
      await page.goto("about:blank");
      await orchestrator.startTask(child.id);
      await vi.waitFor(() => expect(events.some(payload => payload.event.inputRequest?.kind === "elicitation" && payload.event.inputRequest.serverName === "compatibility_fixture")).toBe(true), { timeout: 90_000, interval: 100 });

      // No chat view existed while the request arrived. Its first view is the
      // production journal snapshot; the browser sends the original request ID.
      await new Promise<void>((resolve, reject) => { api.once("error", reject); api.listen(0, "127.0.0.1", resolve); });
      const apiAddress = api.address();
      if (!apiAddress || typeof apiAddress === "string") throw new Error("Fixture bridge did not listen");
      const bridge = `http://127.0.0.1:${apiAddress.port}`;
      await page.goto(`http://127.0.0.1:${uiAddress.port}/tests/fixtures/agent-requests.html?liveChat=${encodeURIComponent(bridge)}&liveTask=${child.id}`);
      const transcript = page.getByRole("region", { name: "Durable transcript" });
      await browserExpect(transcript.getByRole("button", { name: "Send form", exact: true })).toBeVisible();
      await page.reload();
      await page.waitForFunction(() => window.chatStreamReady === true);
      const first = events.find(payload => payload.event.inputRequest?.kind === "elicitation" && payload.event.inputRequest.serverName === "compatibility_fixture")?.event.inputRequest;
      // Codex's native tool consent is an empty MCP form. Answer that through
      // the same UI before the server's typed form becomes available.
      if (first?.kind === "elicitation" && first.mode === "form" && first.fields.length === 0) {
        await transcript.getByRole("button", { name: "Send form", exact: true }).click();
      }
      await browserExpect(transcript.getByLabel("count *", { exact: true })).toBeVisible({ timeout: 90_000 });
      await transcript.getByLabel("count *", { exact: true }).fill("0");
      await transcript.getByRole("button", { name: "Send form", exact: true }).click();
      await vi.waitFor(() => expect(child.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      const reply = chat.snapshot(child.id).messages.filter(message => message.kind === "text").at(-1)?.content;
      if (!reply) throw new Error("Provider completed without a visible reply");
      await writeFile(path.join(liveRoot!, `${agent}-durable-chat.json`), JSON.stringify({ snapshot: chat.snapshot(child.id), events }, null, 2));
      await browserExpect(transcript.getByRole("button", { name: /Worked for/ })).toBeVisible();
      await transcript.getByText(reply, { exact: true }).scrollIntoViewIfNeeded();
      await browserExpect(transcript.getByText(reply, { exact: true })).toBeInViewport();
      expect({
        proof: JSON.parse(await readFile(proof, "utf8")),
        parentEvents: parent.events,
        completions: chat.snapshot(child.id).messages.filter(message => message.kind === "done").map(message => message.outcome),
      }).toEqual({ proof: { action: "accept", content: { count: 0, enabled: false } }, parentEvents: [], completions: ["completed"] });
      await transcript.screenshot({ path: path.join(liveRoot!, `${agent}-durable-chat.png`) });
      if (agent === "codex") {
        await transcript.getByRole("textbox", { name: "Live follow-up" }).fill("Create lost-ack-proof.txt containing exactly fixture-accepted without a newline. Then reply Done. Do not modify other files or use subagents.");
        await transcript.getByRole("button", { name: "Send follow-up", exact: true }).click();
        const warning = transcript.getByText(/Message delivery could not be confirmed.*Check the conversation before sending it again/);
        await browserExpect(warning).toBeVisible({ timeout: 90_000 });
        expect({ content: await readFile(path.join(cwd, "lost-ack-proof.txt"), "utf8"), messages: chat.snapshot(child.id).messages.filter(message => message.id === "live-follow-up").map(message => Boolean(message.deliveryError)), status: child.status }).toEqual({ content: "fixture-accepted", messages: [true], status: "reviewing" });
        await warning.scrollIntoViewIfNeeded();
        await browserExpect(warning).toBeInViewport();
        await transcript.screenshot({ path: path.join(liveRoot!, "codex-unconfirmed-delivery.png") });
        await writeFile(path.join(liveRoot!, "codex-unconfirmed-delivery.json"), JSON.stringify({ snapshot: chat.snapshot(child.id), events }, null, 2));
      }
    } catch (error: unknown) {
      const snapshot = childId ? chat.snapshot(childId) : undefined;
      const trace = await readFile(`${proof}.trace`, "utf8").catch(() => "MCP fixture did not start");
      await writeFile(path.join(liveRoot!, `${agent}-durable-mcp.trace`), trace);
      await writeFile(path.join(liveRoot!, `${agent}-durable-failure.json`), JSON.stringify({ snapshot, events }, null, 2));
      const page = browser?.contexts()[0]?.pages()[0];
      if (page) await page.screenshot({ path: path.join(liveRoot!, `${agent}-durable-failure.png`) });
      throw error;
    } finally {
      await browser?.close();
      if (childId) await orchestrator.cancelTask(childId);
      await vite.close();
      for (const client of clients) client.end();
      await new Promise<void>(resolve => api.close(() => resolve()));
      db.close();
      vi.unstubAllEnvs();
      await rm(cwd, { recursive: true, force: true });
      await rm(`${cwd}-vite-cache`, { recursive: true, force: true });
    }
  }, 210_000);
});
