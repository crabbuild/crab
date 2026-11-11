import { describe, expect, it, vi } from "vitest";
import { _electron as electron, expect as browserExpect, type ElectronApplication } from "@playwright/test";
import { createServer as createViteServer } from "vite";
import { createServer, type ServerResponse } from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { computeDiff } from "../electron/orchestrator-diff";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const bytes = (file: string) => readFile(file, "utf8").catch((error: NodeJS.ErrnoException) => {
  if (error.code === "ENOENT") return null;
  throw error;
});

describe.skipIf(!liveRoot)("selected Apply in the task view", () => {
  it.each(["live", "occupied"])("confirms the %s selection through actual Git and durable task state", async mode => {
    const directory = await mkdtemp(path.join(liveRoot!, "selected-apply-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    const orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
    const clients = new Set<ServerResponse>();
    const operations = new Set<Promise<void>>();
    const receipts: Array<{ files: string[]; success: boolean; error?: string }> = [];
    const requests: string[] = [];
    const publish = (channel: string, payload: unknown) => {
      for (const client of clients) client.write(`data: ${JSON.stringify({ channel, payload })}\n\n`);
    };
    orchestrator.on("orchestrator:task-status", payload => publish("orchestrator:task-status", payload));
    let application: ElectronApplication | undefined;
    let taskId: string | undefined;
    const api = createServer((request, response) => {
      const operation = (async () => {
        requests.push(`${request.method} ${request.url}`);
        response.setHeader("Access-Control-Allow-Origin", "*");
        response.setHeader("Access-Control-Allow-Headers", "Content-Type");
        if (request.method === "OPTIONS") { response.end(); return; }
        if (request.url === "/events") {
          response.setHeader("Content-Type", "text/event-stream"); response.writeHead(200); response.write("\n");
          clients.add(response); response.once("close", () => clients.delete(response)); return;
        }
        const chunks: Buffer[] = [];
        for await (const chunk of request) chunks.push(Buffer.from(chunk));
        const body = JSON.parse(Buffer.concat(chunks).toString()) as { taskId?: string; files?: string[] };
        const task = taskId ? orchestrator.getTask(taskId) : undefined;
        if (!task) throw new Error("Owned task unavailable");
        let value: object;
        if (request.url === "/listTasks") value = { success: true, tasks: [task] };
        else {
          if (body.taskId !== task.id) throw new Error("Unexpected task owner");
          if (request.url === "/getDiff") value = { success: true, diff: await computeDiff(task) };
          else if (request.url === "/loadChat") value = { success: true, messages: [], revision: 0 };
          else if (request.url === "/getGoal") value = { success: true, goal: null };
          else if (request.url === "/listCheckpoints") value = { success: true, checkpoints: [], backups: [] };
          else if (request.url === "/cherryPick") {
            const files = body.files;
            if (!Array.isArray(files) || !files.every(file => typeof file === "string")) throw new Error("Selected files required");
            try {
              value = await orchestrator.applyTask(task.id, files);
              receipts.push({ files, success: true });
            } catch (error: unknown) {
              const message = error instanceof Error ? error.message : String(error);
              value = { success: false, error: message }; receipts.push({ files, success: false, error: message });
            }
          } else throw new Error(`Unexpected operation: ${request.url}`);
        }
        response.end(JSON.stringify(value));
      })().catch((error: unknown) => { response.end(JSON.stringify({ success: false, error: String(error) })); });
      operations.add(operation); void operation.finally(() => operations.delete(operation));
    });
    const vite = await createViteServer({ cacheDir: path.join(directory, "vite-cache"),
      optimizeDeps: { entries: ["tests/fixtures/agent-workspace.html"] },
      server: { host: "127.0.0.1", port: 0 }, clearScreen: false });
    try {
      await run("git", ["init", "--template=", "--initial-branch=main", directory]);
      for (const [key, value] of [["user.name", "Fixture"], ["user.email", "fixture@example.invalid"], ["commit.gpgSign", "false"]]) await run("git", ["config", key, value], { cwd: directory });
      for (const [file, content] of [["old.txt", "rename original\n"], ["remove.txt", "delete original\n"], ["unchanged.txt", "before\n"]]) await writeFile(path.join(directory, file), content);
      await run("git", ["add", "old.txt", "remove.txt", "unchanged.txt"], { cwd: directory });
      await run("git", ["commit", "-m", "Baseline"], { cwd: directory });
      const task = await orchestrator.createTask("Select live files", "codex", directory);
      taskId = task.id; task.status = "reviewing"; await orchestrator.persistTasks();
      await rename(path.join(task.worktreePath, "old.txt"), path.join(task.worktreePath, "renamed.txt"));
      await rm(path.join(task.worktreePath, "remove.txt"));
      await writeFile(path.join(task.worktreePath, "pending.txt"), "pending live bytes\n");
      await writeFile(path.join(task.worktreePath, "private-source.txt"), "unselected source bytes\n");
      const indexPath = (await run("git", ["rev-parse", "--git-path", "index"], { cwd: task.worktreePath })).stdout.trim();
      const sourceIndex = await readFile(path.resolve(task.worktreePath, indexPath));
      await writeFile(path.join(directory, "unchanged.txt"), "staged user bytes\n");
      await run("git", ["add", "unchanged.txt"], { cwd: directory });
      await writeFile(path.join(directory, "unchanged.txt"), "unstaged user bytes\n");
      if (mode === "occupied") await writeFile(path.join(directory, "pending.txt"), "private target bytes\n");
      const targetIndex = await readFile(path.join(directory, ".git/index"));
      await new Promise<void>(resolve => api.listen(0, "127.0.0.1", resolve));
      await vite.listen();
      const bridge = api.address(); const renderer = vite.httpServer?.address();
      if (!bridge || typeof bridge === "string" || !renderer || typeof renderer === "string") throw new Error("Fixture servers unavailable");
      const url = `http://127.0.0.1:${renderer.port}/tests/fixtures/agent-workspace.html?liveWorkspace=${encodeURIComponent(`http://127.0.0.1:${bridge.port}`)}`;
      const entry = path.join(directory, "main.cjs");
      await writeFile(entry, `const { app, BrowserWindow } = require("electron");
        const [profile, url] = process.argv.slice(-2); app.setPath("userData", profile);
        app.whenReady().then(() => new BrowserWindow({ width: 1280, height: 900, show: false, webPreferences: { sandbox: true, contextIsolation: true } }).loadURL(url));
        app.on("window-all-closed", () => app.quit());`);
      const env = Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined));
      delete env.ELECTRON_RUN_AS_NODE;
      application = await electron.launch({ args: [entry, path.join(directory, "profile"), url], env });
      const page = await application.firstWindow();
      const errors: string[] = [];
      page.on("pageerror", error => errors.push(error.message));
      try { await page.getByRole("tab", { name: /^Diff/ }).click(); }
      catch (error: unknown) {
        await writeFile(path.join(liveRoot!, `selected-apply-${mode}-failure.json`), JSON.stringify({ url: page.url(), body: await page.locator("body").innerText(), errors, requests }, null, 2));
        throw error;
      }
      for (const file of ["pending.txt", "renamed.txt", "remove.txt"]) await page.getByRole("checkbox", { name: `Select ${file}`, exact: true }).check();
      await page.getByRole("button", { name: "Apply selected (3)", exact: true }).click();
      await browserExpect(page.getByText(mode === "live" ? "3 file(s) applied and committed" : /Cherry-pick failed: Selected target paths contain/).first()).toBeVisible();
      const current = { pending: await bytes(path.join(directory, "pending.txt")), old: await bytes(path.join(directory, "old.txt")),
        removed: await bytes(path.join(directory, "remove.txt")), renamed: await bytes(path.join(directory, "renamed.txt")), applied: orchestrator.getTask(task.id)?.status === "applied",
        sourceIndex: await readFile(path.resolve(task.worktreePath, indexPath)), staged: (await run("git", ["show", ":unchanged.txt"], { cwd: directory })).stdout,
        unstaged: await bytes(path.join(directory, "unchanged.txt")) };
      expect(current).toEqual({ pending: mode === "live" ? "pending live bytes\n" : "private target bytes\n", old: mode === "live" ? null : "rename original\n",
        removed: mode === "live" ? null : "delete original\n", renamed: mode === "live" ? "rename original\n" : null, applied: mode === "live",
        sourceIndex, staged: "staged user bytes\n", unstaged: "unstaged user bytes\n" });
      if (mode === "occupied") expect(await readFile(path.join(directory, ".git/index"))).toEqual(targetIndex);
      const reopened = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
      try { expect(reopened.getTask(task.id)?.status === "applied").toBe(mode === "live"); }
      finally { await reopened.shutdown(); }
      await writeFile(path.join(liveRoot!, `selected-apply-${mode}.json`), JSON.stringify({ receipts, applied: orchestrator.getTask(task.id)?.status === "applied", sourceIndexPreserved: true, targetUserBytesPreserved: true }));
    } finally {
      await application?.close();
      for (const client of clients) client.end();
      if (api.listening) await new Promise<void>(resolve => api.close(() => resolve()));
      await Promise.allSettled([...operations]); await vite.close(); await orchestrator.shutdown();
      vi.unstubAllEnvs(); await rm(directory, { recursive: true, force: true });
    }
  }, 90_000);
});
