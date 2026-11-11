import { describe, expect, it, vi } from "vitest";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer as createViteServer } from "vite";
import { createServer, type ServerResponse } from "node:http";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import { computeDiff } from "../electron/orchestrator-diff";
import type { AgentEvent, CodexGoalMutation } from "../electron/orchestrator-adapters";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;

describe.skipIf(!liveRoot)("native Codex goals in the task view", () => {
  it("activates a bounded goal from the UI, writes its file, projects native completion, and clears it cold", async () => {
    const cwd = await mkdtemp(path.join(liveRoot!, "native-goal-ui-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    const command = process.execPath;
    const args = [path.resolve("tests/fixtures/mcp-elicitation-server.mjs"), path.join(cwd, "mcp-proof.json"), "tool", "form"];
    await writeFile(path.join(cwd, "settings.json"), JSON.stringify({ orchestratorAgentConfigs: { codex: { extraArgs: ["-c", `mcp_servers={compatibility_fixture={command=${JSON.stringify(command)},args=${JSON.stringify(args)},env={ELECTRON_RUN_AS_NODE="1"}}}`, "-c", 'approval_policy="on-request"'] } } }));
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 150_000, maxConcurrent: 1 });
    const db = new ChatDatabase(path.join(cwd, "chat.sqlite"));
    const clients = new Set<ServerResponse>();
    const events: AgentEvent[] = [];
    const errors: string[] = [];
    const operations: Array<{ method: string; success: boolean; error?: string }> = [];
    let form: { taskId: string; requestId: string } | undefined;
    const publish = (channel: string, payload: unknown) => clients.forEach(client => client.write(`data: ${JSON.stringify({ channel, payload })}\n\n`));
    const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
    const chat = registerChatIpc({ handle: (channel, handler) => { handlers.set(channel, handler); } }, orchestrator, db, () => ({ isDestroyed: () => false, webContents: { send: publish } }));
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload.event);
      if (payload.event.approval) orchestrator.recordApproval(payload.taskId, { requestId: payload.event.approval.requestId, decision: "allow_once", summary: "Owned goal fixture" });
      const input = payload.event.inputRequest;
      if (input?.kind === "elicitation" && input.serverName === "compatibility_fixture" && input.mode === "form") {
        if (!input.fields.length) orchestrator.recordInput(payload.taskId, input.requestId, { kind: "elicitation", action: "accept", content: {} });
        else form = { taskId: payload.taskId, requestId: input.requestId };
      }
    });
    orchestrator.on("orchestrator:task-status", payload => publish("orchestrator:task-status", payload));
    orchestrator.on("orchestrator:error", payload => { errors.push(payload.message); publish("orchestrator:error", payload); });
    let taskId: string | undefined;
    let browser: Browser | undefined;
    let queuedStart: Promise<void> | undefined;
    let succeeded = false;
    const cleanup: Array<{ phase: string; at: number }> = [];
    const mark = async (phase: string) => {
      cleanup.push({ phase, at: Date.now() });
      await writeFile(path.join(liveRoot!, "native-goal-ui-cleanup.json"), JSON.stringify(cleanup, null, 2));
    };
    const api = createServer(async (request, response) => {
      response.setHeader("Access-Control-Allow-Origin", "*");
      response.setHeader("Access-Control-Allow-Headers", "Content-Type");
      response.setHeader("Access-Control-Allow-Methods", "GET,POST,OPTIONS");
      response.setHeader("Content-Type", "application/json");
      if (request.method === "OPTIONS") { response.end(); return; }
      try {
        if (request.url === "/events") {
          response.setHeader("Content-Type", "text/event-stream"); response.writeHead(200); response.write("\n");
          clients.add(response); response.once("close", () => clients.delete(response)); return;
        }
        if (!taskId) throw new Error("Owned task not created");
        const chunks: Buffer[] = [];
        for await (const chunk of request) chunks.push(Buffer.from(chunk));
        const body = JSON.parse(Buffer.concat(chunks).toString()) as { taskId?: string; mutation?: CodexGoalMutation };
        if (request.url === "/listTasks") { response.end(JSON.stringify({ success: true, tasks: [orchestrator.getTask(taskId)] })); return; }
        if (body.taskId !== taskId) throw new Error("Unexpected goal owner");
        let result: object = { success: true };
        if (request.url === "/loadChat") result = { success: true, ...chat.snapshot(taskId) };
        else if (request.url === "/iterate") {
          const handler = handlers.get("orchestrator:iterate");
          if (!handler) throw new Error("Native chat input is not registered");
          result = await handler({} as IpcMainInvokeEvent, body);
        }
        else if (request.url === "/getDiff") result = { success: true, diff: await computeDiff(orchestrator.getTask(taskId)!) };
        else if (request.url === "/getGoal") result = { success: true, goal: await orchestrator.getGoal(taskId) };
        else if (request.url === "/setGoal") {
          if (!body.mutation) throw new Error("Missing native mutation");
          result = { success: true, goal: await orchestrator.setGoal(taskId, body.mutation) };
        } else if (request.url === "/clearGoal") await orchestrator.clearGoal(taskId);
        else throw new Error("Unknown owned goal operation");
        operations.push({ method: request.url, success: true });
        response.end(JSON.stringify(result));
      } catch (cause: unknown) {
        const error = cause instanceof Error ? cause.message : String(cause);
        operations.push({ method: request.url ?? "unknown", success: false, error });
        response.end(JSON.stringify({ success: false, error }));
      }
    });
    const vite = await createViteServer({ cacheDir: `${cwd}-vite-cache`, server: { host: "127.0.0.1", port: 0, strictPort: false }, clearScreen: false });
    try {
      await vite.listen();
      const run = promisify(execFile);
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned goal fixture"], { cwd });
      const task = await orchestrator.createTask("Reply exactly GOAL_UI_READY without calling tools or modifying files.", "codex", cwd, "HEAD");
      taskId = task.id;
      await orchestrator.startTask(task.id);
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      const threadId = task.codexThreadId;
      await new Promise<void>((resolve, reject) => { api.once("error", reject); api.listen(0, "127.0.0.1", resolve); });
      const ui = vite.httpServer?.address();
      const bridge = api.address();
      if (!ui || typeof ui === "string" || !bridge || typeof bridge === "string") throw new Error("Goal UI fixture did not listen");
      browser = await chromium.launch({ channel: "chrome" });
      const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
      page.on("pageerror", error => errors.push(error.message));
      await page.goto(`http://127.0.0.1:${ui.port}/tests/fixtures/agent-workspace.html?liveWorkspace=${encodeURIComponent(`http://127.0.0.1:${bridge.port}`)}`);
      await page.waitForFunction(() => window.workspaceStreamReady === true);
      const bar = page.getByRole("region", { name: "Codex goal" });
      await bar.getByRole("button", { name: "New goal" }).click();
      const objective = "Call the compatibility_fixture confirm_values MCP tool once. Then create goal-proof.txt containing exactly native-goal without a newline, verify its content, and mark this goal complete. Do not modify other files or use subagents.";
      await bar.getByRole("textbox", { name: "Goal objective" }).fill(objective);
      // Goal accounting includes model context. Leave room to reach the held
      // form instead of stopping at the budget boundary before tool execution.
      await bar.getByRole("spinbutton", { name: "Goal token budget" }).fill("100000");
      await bar.getByRole("button", { name: "Start goal", exact: true }).click();
      await vi.waitFor(() => expect(form).toBeDefined(), { timeout: 90_000, interval: 100 });
      const other = await orchestrator.createTask("Reply exactly GOAL_SLOT_READY without calling tools or modifying files.", "codex", cwd, "HEAD");
      queuedStart = orchestrator.startTask(other.id);
      void queuedStart.catch(() => {});
      await vi.waitFor(() => expect(other.status).toBe("queued"), { timeout: 5000, interval: 100 });
      await bar.getByRole("button", { name: "Pause goal" }).click();
      await browserExpect(bar.getByRole("status", { name: "Goal status" })).toHaveText("Paused");
      expect({ goal: task.goal?.status, currentTurn: task.status, other: other.status }).toEqual({ goal: "paused", currentTurn: "running", other: "queued" });
      await bar.getByRole("button", { name: "Resume goal" }).click();
      await browserExpect(bar.getByRole("status", { name: "Goal status" })).toHaveText("Active");
      if (!form) throw new Error("Native goal form disappeared");
      expect(orchestrator.recordInput(form.taskId, form.requestId, { kind: "elicitation", action: "accept", content: { count: 0, enabled: false } })).toBe(true);
      await vi.waitFor(async () => expect(await readFile(path.join(task.worktreePath, "goal-proof.txt"), "utf8")).toBe("native-goal"), { timeout: 120_000, interval: 100 });
      await browserExpect(bar.getByRole("status", { name: "Goal status" })).toHaveText("Complete", { timeout: 90_000 });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 15_000, interval: 100 });
      await bar.screenshot({ path: path.join(liveRoot!, "native-goal-ui.png") });
      expect({ thread: task.codexThreadId, goal: task.goal?.status, objective: task.goal?.objective, budget: task.goal?.tokenBudget,
        errors }).toEqual({ thread: threadId, goal: "complete", objective, budget: 100000, errors: [] });
      expect(events.filter(event => event.type === "turn_started").length).toBeGreaterThanOrEqual(2);
      expect(task.goal?.tokensUsed).toBeGreaterThan(0);
      await queuedStart;
      await vi.waitFor(() => expect(other.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      await orchestrator.cancelTask(other.id);
      await orchestrator.cancelTask(task.id);
      await bar.getByRole("button", { name: "Clear goal" }).click();
      await browserExpect(bar.getByRole("button", { name: "New goal" })).toBeVisible();
      expect({ goal: await orchestrator.getGoal(task.id), status: task.status, failed: operations.filter(operation => !operation.success) }).toEqual({ goal: null, status: "stopped", failed: [] });
      await page.getByRole("textbox", { name: "Chat message input", exact: true }).fill("Append a newline followed by resumed to goal-proof.txt, preserving its existing content and adding nothing else. Then reply exactly NATIVE_RESUME_READY. Do not modify any other files or use subagents.");
      await page.getByRole("button", { name: "Send message", exact: true }).click();
      await vi.waitFor(async () => expect(await readFile(path.join(task.worktreePath, "goal-proof.txt"), "utf8")).toBe("native-goal\nresumed"), { timeout: 90_000, interval: 100 });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 15_000, interval: 100 });
      await browserExpect(page.getByRole("log", { name: "Chat transcript" }).getByText("NATIVE_RESUME_READY", { exact: true })).toBeVisible();
      expect({ thread: task.codexThreadId, goal: task.goal, failed: operations.filter(operation => !operation.success) }).toEqual({ thread: threadId, goal: null, failed: [] });
      await writeFile(path.join(liveRoot!, "native-goal-ui-controls.json"), JSON.stringify({ task, events, operations, errors }, null, 2));
      succeeded = true;
    } catch (cause: unknown) {
      await writeFile(path.join(liveRoot!, "native-goal-ui-failure.json"), JSON.stringify({ task: taskId ? orchestrator.getTask(taskId) : undefined, events, operations, errors, error: String(cause) }, null, 2));
      throw cause;
    } finally {
      await mark("cancel-providers");
      await Promise.allSettled(orchestrator.getAllTasks().map(task => orchestrator.cancelTask(task.id)));
      await mark("join-queued-start");
      await queuedStart?.catch(() => {});
      await mark("close-browser");
      await browser?.close();
      await mark("close-bridge");
      clients.forEach(client => client.end());
      if (api.listening) await new Promise<void>(resolve => api.close(() => resolve()));
      await mark("close-vite");
      await vite.close();
      await mark("remove-fixture");
      db.close(); vi.unstubAllEnvs();
      await rm(`${cwd}-vite-cache`, { recursive: true, force: true });
      if (succeeded) await rm(cwd, { recursive: true, force: true });
      await mark("done");
    }
  }, 300_000);
});
