import { describe, it, expect, vi } from "vitest";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { createRequire } from "node:module";
import { Orchestrator } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const require = createRequire(import.meta.url);
const cases = ["codex", "claude-code"].flatMap(agent => ["delivery", "publication", "queued", "terminal", "terminal-ack"].map(phase => ({ agent, phase })));

describe.skipIf(!liveRoot || process.platform === "win32")("native input owner crash recovery", () => {
  it.each(cases)("$agent recovers a $phase crash without resending accepted work", async ({ agent, phase }) => {
    const directory = await mkdtemp(path.join(liveRoot!, "input-recovery-"));
    const repo = path.join(directory, "repo");
    await mkdir(repo);
    await run("git", ["init", "--initial-branch=main"], { cwd: repo });
    await run("git", ["config", "user.name", "Crab Fixture"], { cwd: repo });
    await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd: repo });
    await run("git", ["commit", "--allow-empty", "-m", "Owned fixture"], { cwd: repo });
    const settings = path.join(directory, "settings.json");
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", settings);
    const child = spawn(process.execPath, [require.resolve("vite-node/cli"), path.resolve("tests/fixtures/pending-input-crash.ts"), directory, agent, phase], {
      detached: true, stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, CRAB_DESKTOP_SETTINGS_PATH: settings },
    });
    let stderr = "";
    child.stderr.on("data", chunk => { stderr = (stderr + String(chunk)).slice(-8000); });
    child.stdout.resume();
    const closed = new Promise<void>(resolve => child.once("close", () => resolve()));
    let killTimer: ReturnType<typeof setTimeout> | undefined;
    let orchestrator: Orchestrator | undefined;
    let db: ChatDatabase | undefined;
    let taskId: string | undefined;
    let succeeded = false;
    let stopped = false;
    const terminateGroup = () => {
      if (!child.pid) return;
      try { process.kill(-child.pid, "SIGTERM"); } catch (error: unknown) {
        if ((error as NodeJS.ErrnoException).code === "ESRCH") return;
        throw error;
      }
      killTimer ??= setTimeout(() => {
        try { process.kill(-child.pid!, "SIGKILL"); } catch (error: unknown) {
          if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
        }
      }, 2000);
    };
    const stopGroup = async () => {
      if (stopped) return;
      if (!child.pid) { await closed; stopped = true; return; }
      terminateGroup();
      await closed;
      await vi.waitFor(() => {
        let exists = true;
        try { process.kill(-child.pid!, 0); } catch (error: unknown) {
          if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
          exists = false;
        }
        expect(exists).toBe(false);
      }, { timeout: 10_000, interval: 100 });
      if (killTimer) clearTimeout(killTimer);
      stopped = true;
    };
    try {
      const code = await new Promise<number | null>((resolve, reject) => {
        child.once("error", reject);
        child.once("exit", resolve);
      });
      if (code !== 86) throw new Error(`Fixture exited ${code}: ${stderr}`);
      await stopGroup();
      const crash = JSON.parse(await readFile(path.join(directory, "crash-state.json"), "utf8")) as {
        taskId: string; blockerId?: string; input?: { sha?: string }; file: string;
        completions?: Array<{ id: string; event: { outcome: string } }>;
      };
      taskId = crash.taskId;
      orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), timeoutMs: 120_000, maxConcurrent: 1 });
      db = new ChatDatabase(path.join(directory, "chat.sqlite"));
      const chat = registerChatIpc({ handle() {} }, orchestrator, db, () => null);
      orchestrator.on("orchestrator:event", ({ taskId, event }) => {
        if (event.approval) void orchestrator!.recordApproval(taskId, { requestId: event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
      });
      await orchestrator.recoverTaskInput(taskId);
      await orchestrator.recoverTaskInput(taskId);
      const task = orchestrator.getTask(taskId);
      if (!task) throw new Error("Recovered task is missing");
      const checkpoint = (await new CheckpointManager().listCheckpoints(task)).find(candidate => candidate.turnNumber === 1);
      const { stdout: pins } = await run("git", ["for-each-ref", "--format=%(refname)", `refs/crab/checkpoint-intents/${task.id}/`], { cwd: repo });
      const input = chat.snapshot(taskId).messages.find(message => message.id === "crash-input");
      if (phase === "queued") {
        if (!crash.blockerId) throw new Error("Queued fixture has no blocker");
        await orchestrator.cancelTask(crash.blockerId);
        expect({ atCrash: crash.file, current: await readFile(path.join(task.worktreePath, "crash-proof.txt"), "utf8"), status: task.status,
          phase: task.pendingInput?.phase, number: task.turnNumber, feedback: task.feedback, checkpoint, notice: input?.deliveryError, pins: pins.trim() }).toEqual({
          atCrash: "before", current: "before", status: "interrupted", phase: "unsent", number: undefined, feedback: [], checkpoint: undefined,
          notice: "Queued message was not sent before the connection closed. Use Retry to send it.", pins: "",
        });
        const session = agent === "codex" ? task.codexThreadId : task.claudeSessionId;
        await orchestrator.retryTask(task.id);
        await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
        expect({ file: await readFile(path.join(task.worktreePath, "crash-proof.txt"), "utf8"), feedback: task.feedback.length,
          session: agent === "codex" ? task.codexThreadId : task.claudeSessionId }).toEqual({ file: "before-native", feedback: 1, session });
      } else {
        if (!checkpoint) throw new Error("Recovered checkpoint is missing");
        const pinnedInput = phase === "delivery" || phase === "publication";
        const { stdout } = await run("git", ["show", `${checkpoint.ref}:crash-proof.txt`], { cwd: repo });
        expect({ atCrash: crash.file, current: await readFile(path.join(task.worktreePath, "crash-proof.txt"), "utf8"), before: stdout,
          ...(pinnedInput ? { sha: checkpoint.sha } : {}), feedback: task.feedback.length, number: task.turnNumber, pending: task.pendingInput, pins: pins.trim(),
          input: input?.content, uncertain: Boolean(input?.deliveryError) }).toEqual({
          atCrash: "before-native", current: "before-native", before: "before", ...(pinnedInput ? { sha: crash.input?.sha } : {}),
          feedback: 1, number: 1, pending: undefined, pins: "",
          input: "Append exactly -native to crash-proof.txt without a newline. Reply FOLLOWUP_READY. Do not modify other files or use subagents.", uncertain: phase === "delivery",
        });
        if (phase === "terminal" || phase === "terminal-ack") {
          const completion = crash.completions?.[0];
          if (!completion) throw new Error("Native completion was not staged at the crash");
          expect({ outcome: completion.event.outcome, status: task.status, pending: task.pendingCompletions,
            terminalRows: chat.snapshot(taskId).messages.filter(message => message.kind === "done").length,
            receiptRows: db.loadMessages(taskId).filter(message => message.id === completion.id).length,
          }).toEqual({ outcome: "completed", status: "reviewing", pending: undefined, terminalRows: 2, receiptRows: 1 });
        }
      }
      await writeFile(path.join(liveRoot!, `${agent}-${phase}-input-recovery.json`), JSON.stringify({ task, crash, checkpoint, snapshot: chat.snapshot(taskId) }, null, 2));
      succeeded = true;
    } finally {
      await stopGroup();
      if (orchestrator && taskId) await orchestrator.cancelTask(taskId);
      db?.close();
      vi.unstubAllEnvs();
      if (succeeded) await rm(directory, { recursive: true, force: true });
    }
  }, 240_000);
});
