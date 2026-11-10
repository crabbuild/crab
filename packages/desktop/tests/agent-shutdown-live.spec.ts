import { describe, expect, it } from "vitest";
import { execFile, spawn } from "node:child_process";
import { createRequire } from "node:module";
import { promisify } from "node:util";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import type { ChatMessage } from "../electron/agent-chat-messages";
import { Orchestrator } from "../electron/orchestrator";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const require = createRequire(import.meta.url);
const run = promisify(execFile);

describe.skipIf(!liveRoot || process.platform === "win32")("real Electron native provider shutdown", () => {
  it.each(["codex", "claude-code"] as const)("%s closes its processes and pending input before Electron exits", async agent => {
    const directory = await mkdtemp(path.join(liveRoot!, "electron-quit-"));
    const repo = path.join(directory, "repo");
    await mkdir(repo);
    await mkdir(path.join(directory, "electron"));
    await run("git", ["init", "--initial-branch=main"], { cwd: repo });
    await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd: repo });
    const env: NodeJS.ProcessEnv = { ...process.env, CRAB_DESKTOP_SETTINGS_PATH: path.join(directory, "settings.json") };
    delete env.ELECTRON_RUN_AS_NODE;
    const child = spawn(require("electron") as string, [path.resolve("tests/fixtures/agent-shutdown.cjs"), directory, agent, process.cwd()], { env, detached: true, stdio: ["ignore", "pipe", "pipe"] });
    let stderr = "";
    child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-8000); });
    child.stdout.resume();
    const closed = new Promise<number | null>((resolve, reject) => { child.once("error", reject); child.once("close", resolve); });
    let timer: ReturnType<typeof setTimeout> | undefined;
    let succeeded = false;
    try {
      const code = await Promise.race([closed, new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("Electron quit fixture timed out")), 240_000); })]);
      if (code !== 0) throw new Error(`Electron quit fixture exited ${code}: ${stderr}; ${await readFile(path.join(directory, "failure.json"), "utf8").catch(() => "No failure report")}`);
      const result = JSON.parse(await readFile(path.join(directory, "result.json"), "utf8")) as {
        file: string; tasks: { idle: { id: string; status: string; feedback: string[] }; active: { id: string; status: string; goal?: string }; queued: { id: string; status: string } };
        exits: string[]; quitPrevented: boolean[]; input: { success: boolean; error: string }; startup: string;
        snapshots: { idle: { messages: ChatMessage[] }; active: { messages: ChatMessage[] } };
      };
      const tasks = result.tasks;
      const restored = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
      const requests = result.snapshots.active.messages.flatMap(message => message.events ?? []).map(event => event.input as { requestId?: string; resolved?: boolean }).filter(input => input?.requestId);
      expect({ file: result.file, sent: existsSync(path.join(repo, "never-sent.txt")), statuses: [tasks.idle.status, tasks.active.status, tasks.queued.status],
        saved: [tasks.idle.id, tasks.active.id, tasks.queued.id].map(id => restored.getTask(id)?.status), exits: [...result.exits].sort(),
        prevented: result.quitPrevented, input: result.input, startup: result.startup, feedback: tasks.idle.feedback,
        pendingRow: result.snapshots.idle.messages.some(message => message.id === "quit-unsent"), resolved: requests.length > 0 && requests.every(request => request.resolved),
        goal: tasks.active.goal ?? null,
      }).toEqual({ file: "ready", sent: false, statuses: ["stopped", "stopped", "queued"], saved: ["stopped", "stopped", "queued"], exits: [tasks.idle.id, tasks.active.id].sort(),
        prevented: [true, true], input: { success: false, error: "Message cancelled before submission" }, startup: "Agent service is shutting down", feedback: [], pendingRow: false, resolved: true,
        goal: agent === "codex" ? "paused" : null,
      });
      await writeFile(path.join(liveRoot!, `${agent}-electron-quit.json`), JSON.stringify(result, null, 2));
      succeeded = true;
    } finally {
      clearTimeout(timer);
      if (child.pid) {
        try { process.kill(-child.pid, "SIGKILL"); } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; }
      }
      await closed;
      if (succeeded) await rm(directory, { recursive: true, force: true });
    }
  }, 260_000);
});
