import { describe, expect, it, vi } from "vitest";
import { chromium, expect as browserExpect } from "@playwright/test";
import { createServer as createViteServer } from "vite";
import { createServer, type ServerResponse } from "node:http";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { registerDiffIpc } from "../electron/orchestrator-diff";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";
import { Orchestrator } from "../electron/orchestrator";
import type { HunkPatch } from "../electron/orchestrator-hunks";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;

async function fixturePort(): Promise<number> {
  const reservation = createServer();
  await new Promise<void>(resolve => reservation.listen(0, "127.0.0.1", resolve));
  const address = reservation.address();
  await new Promise<void>(resolve => reservation.close(() => resolve()));
  if (!address || typeof address === "string") throw new Error("Fixture port was not allocated");
  return address.port;
}

describe.skipIf(!liveRoot)("hunk actions through the renderer", () => {
  it.each([... ["hunk", "snapshot"].flatMap(kind => [false, true].map(stale => ({ kind, stale }))), { kind: "diff", stale: false }])("$kind reflects real files with a conflicting edit: $stale", async ({ kind, stale }) => {
    const directory = await mkdtemp(path.join(liveRoot!, "hunk-renderer-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    const orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
    const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
    registerDiffIpc({ handle: (channel, handler) => { handlers.set(channel, handler); } } as IpcMain, orchestrator, () => null);
    const vite = await createViteServer({ cacheDir: `${directory}-vite-cache`, server: { host: "127.0.0.1", port: await fixturePort(), strictPort: true }, clearScreen: false });
    let taskId: string;
    const requests: HunkPatch[] = [];
    const clients = new Set<ServerResponse>();
    const publish = (channel: string, payload: unknown) => { for (const client of clients) client.write(`data: ${JSON.stringify({ channel, payload })}\n\n`); };
    orchestrator.on("orchestrator:task-status", payload => publish("orchestrator:task-status", payload));
    orchestrator.on("orchestrator:files-changed", payload => publish("orchestrator:files-changed", payload));
    const bridge = createServer(async (request, response) => {
      response.setHeader("Access-Control-Allow-Origin", "*");
      response.setHeader("Access-Control-Allow-Headers", "Content-Type");
      if (request.method === "OPTIONS") { response.end(); return; }
      if (request.url === "/events") {
        response.setHeader("Content-Type", "text/event-stream"); response.writeHead(200); response.write("\n");
        clients.add(response); response.once("close", () => clients.delete(response)); return;
      }
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(Buffer.from(chunk));
      try {
        const owner = orchestrator.getTask(taskId)!;
        const snapshots = new CheckpointManager();
        if (request.url === "/listTasks") { response.end(JSON.stringify({ success: true, tasks: [owner] })); return; }
        if (request.url === "/loadChat") { response.end(JSON.stringify({ success: true, messages: [], revision: 0 })); return; }
        if (request.url === "/getGoal") { response.end(JSON.stringify({ success: true, goal: null })); return; }
        if (request.url === "/getDiff") {
          const handler = handlers.get("orchestrator:get-diff");
          if (!handler) throw new Error("Diff handler not registered");
          response.end(JSON.stringify(await handler({} as IpcMainInvokeEvent, JSON.parse(Buffer.concat(chunks).toString())))); return;
        }
        if (request.url === "/snapshots" || request.url === "/listCheckpoints") {
          response.end(JSON.stringify({ success: true, checkpoints: await snapshots.listCheckpoints(owner), backups: await snapshots.listRestoreBackups(owner) })); return;
        }
        if (request.url === "/restore" || request.url === "/restoreFiles") {
          const payload = JSON.parse(Buffer.concat(chunks).toString()) as { taskId: string; sourceRef: string };
          if (payload.taskId !== taskId) throw new Error("Unexpected snapshot owner");
          await orchestrator.restoreFiles(taskId, payload.sourceRef);
          response.end(JSON.stringify({ success: true })); return;
        }
        const { patch } = JSON.parse(Buffer.concat(chunks).toString()) as { patch: HunkPatch };
        requests.push(patch);
        await orchestrator.editHunk(taskId, patch, "revert");
        response.end(JSON.stringify({ success: true }));
      } catch (error: unknown) { response.end(JSON.stringify({ success: false, error: error instanceof Error ? error.message : String(error) })); }
    });
    const browser = await chromium.launch({ channel: "chrome" });
    try {
      const run = promisify(execFile);
      await run("git", ["init", "--initial-branch=main", directory]);
      await run("git", ["config", "user.name", "Crab Fixture"], { cwd: directory });
      await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd: directory });
      await run("git", ["commit", "--allow-empty", "-m", "Owned fixture"], { cwd: directory });
      const task = await orchestrator.createTask("Owned edit", "claude-code", directory, "HEAD", { worktreeMode: kind === "hunk" ? "primary" : "auto" });
      taskId = task.id;
      if (kind !== "hunk") { task.status = "reviewing"; await orchestrator.persistTasks(); }
      const workspace = task.worktreePath;
      const secondFile = kind === "diff" ? "雪.ts" : "second.ts";
      const snapshots = new CheckpointManager();
      let diffBase = task.baseRef;
      if (kind === "diff") {
        await writeFile(path.join(workspace, "first.ts"), "before\n");
        await run("git", ["add", "first.ts"], { cwd: workspace });
        await run("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "Baseline"], { cwd: workspace });
        diffBase = (await run("git", ["rev-parse", "HEAD"], { cwd: workspace })).stdout.trim();
        task.baseRef = "refs/heads/missing-baseline";
        await orchestrator.persistTasks();
      }
      if (kind === "snapshot") {
        await writeFile(path.join(workspace, "first.ts"), "one");
        await writeFile(path.join(workspace, "second.ts"), "three");
        await snapshots.createCheckpoint(task, 0);
        await writeFile(path.join(workspace, "untracked.txt"), "uncommitted output");
        if (stale) await writeFile(path.join(workspace, ".gitignore"), "first.ts\n");
      }
      await writeFile(path.join(workspace, "first.ts"), stale ? "later user bytes" : "two");
      await writeFile(path.join(workspace, secondFile), "four");
      await vite.listen();
      await new Promise<void>(resolve => bridge.listen(0, "127.0.0.1", resolve));
      const ui = vite.httpServer?.address();
      const api = bridge.address();
      if (!ui || typeof ui === "string" || !api || typeof api === "string") throw new Error("Fixture did not listen");
      const page = await browser.newPage();
      const base = `http://127.0.0.1:${api.port}`;
      const query = kind !== "hunk" ? `liveWorkspace=${encodeURIComponent(base)}` : `liveHunk=${encodeURIComponent(base)}`;
      const fixture = kind !== "hunk" ? "agent-workspace" : "agent-requests";
      await page.goto(`http://127.0.0.1:${ui.port}/tests/fixtures/${fixture}.html?${query}`);
      if (kind === "diff") {
        await page.waitForFunction(() => window.workspaceStreamReady === true);
        await page.getByRole("tab", { name: /^Diff/ }).click();
        await browserExpect(page.getByRole("alert")).toContainText("missing-baseline");
        await browserExpect(page.getByText("No changes to display.", { exact: true })).toHaveCount(0);
        task.baseRef = diffBase;
        await page.getByRole("button", { name: "Reload changes", exact: true }).click();
        for (const file of ["first.ts", secondFile]) await browserExpect(page.getByText(file, { exact: true }).first()).toBeVisible();
      } else if (kind === "snapshot") {
        await page.waitForFunction(() => window.workspaceStreamReady === true);
        await page.getByRole("tab", { name: /^Diff/ }).click();
        await browserExpect(page.getByText("untracked.txt", { exact: true }).first()).toBeVisible();
        await page.getByRole("button", { name: "Saved file states", exact: true }).click();
        const restore = page.getByRole("button", { name: "Restore files before turn 0", exact: true });
        await restore.click();
        if (stale) {
          await browserExpect(page.getByRole("alert")).toContainText("Move ignored path");
          expect(await readFile(path.join(workspace, "first.ts"), "utf8")).toBe("later user bytes");
          await rm(path.join(workspace, ".gitignore"));
          await restore.click();
        }
        await browserExpect(page.getByText("Backups", { exact: true })).toBeVisible();
        await browserExpect(page.getByText("untracked.txt", { exact: true })).toHaveCount(0);
        expect(await Promise.all(["first.ts", "second.ts"].map(file => readFile(path.join(workspace, file), "utf8")))).toEqual(["one", "three"]);
        const recovered = page.waitForResponse(response => response.url() === `${base}/restoreFiles` && response.request().method() === "POST");
        await page.getByRole("button", { name: /^Restore files saved/ }).first().click();
        expect(await (await recovered).json()).toEqual({ success: true });
        await browserExpect(page.getByText("Files restored", { exact: true }).last()).toBeVisible();
        expect(await readFile(path.join(workspace, "untracked.txt"), "utf8")).toBe("uncommitted output");
        await browserExpect(page.getByText("untracked.txt", { exact: true }).first()).toBeVisible();
        expect(await Promise.all(["first.ts", "second.ts"].map(file => readFile(path.join(workspace, file), "utf8")))).toEqual([stale ? "later user bytes" : "two", "four"]);
      } else {
        await page.getByRole("region", { name: "Undo failure fixture" }).getByRole("button", { name: "Undo", exact: true }).click();
        await browserExpect(page.getByText(stale ? "Undo failed" : "File edits undone", { exact: true })).toBeVisible();
        if (stale) await browserExpect(page.getByText(/1 edit\(s\) were undone before the failure/)).toBeVisible();
        expect({ files: await Promise.all(["first.ts", "second.ts"].map(file => readFile(path.join(workspace, file), "utf8"))), paths: requests.map(patch => patch.filePath) })
          .toEqual({ files: [stale ? "later user bytes" : "one", "three"], paths: ["second.ts", "first.ts"] });
      }
      await page.screenshot({ path: path.join(liveRoot!, `${kind}-${stale ? "stale" : "success"}.png`) });
    } finally {
      await browser.close();
      await orchestrator.shutdown();
      await vite.close();
      for (const client of clients) client.end();
      await new Promise<void>(resolve => bridge.close(() => resolve()));
      vi.unstubAllEnvs();
      await rm(directory, { recursive: true, force: true });
      await rm(`${directory}-vite-cache`, { recursive: true, force: true });
    }
  }, 60_000);
});
