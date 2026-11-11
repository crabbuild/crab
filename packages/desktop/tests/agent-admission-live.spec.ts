import { describe, it, expect, vi } from "vitest";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import type { IpcMain, IpcMainInvokeEvent } from "electron";
import { Orchestrator } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc } from "../electron/orchestrator-chat";
import type { AgentEvent } from "../electron/orchestrator-adapters";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);

describe.skipIf(!liveRoot)("native provider capacity admission", () => {
  it.each(["codex", "claude-code"] as const)("%s admits queued work and cancels undispatched input", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, "agent-admission-"));
    const proof = path.join(cwd, "mcp-proof.json");
    const args = [path.resolve("tests/fixtures/mcp-elicitation-server.mjs"), proof, "tool", "form"];
    const command = process.execPath;
    const extraArgs = agent === "codex"
      ? ["-c", `mcp_servers={compatibility_fixture={command=${JSON.stringify(command)},args=${JSON.stringify(args)},env={ELECTRON_RUN_AS_NODE="1"}}}`, "-c", 'approval_policy="on-request"']
      : ["--mcp-config", JSON.stringify({ mcpServers: { compatibility_fixture: { command, args, env: { ELECTRON_RUN_AS_NODE: "1" } } } }), "--strict-mcp-config"];
    const settings = path.join(cwd, "settings.json");
    await writeFile(settings, JSON.stringify({ orchestratorAgentConfigs: { [agent]: { extraArgs } } }));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", settings);
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 120_000, maxConcurrent: 1 });
    const db = new ChatDatabase(path.join(cwd, "chat.sqlite"));
    const handlers = new Map<string, Parameters<IpcMain["handle"]>[1]>();
    const chat = registerChatIpc({ handle: (channel, handler) => { handlers.set(channel, handler); } }, orchestrator, db, () => null);
    const events: Array<{ taskId: string; event: AgentEvent }> = [];
    let form: { taskId: string; requestId: string } | undefined;
    const taskIds: string[] = [];
    const submissions: Promise<unknown>[] = [];
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload);
      if (payload.event.approval) void orchestrator.recordApproval(payload.taskId, { requestId: payload.event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
      const input = payload.event.inputRequest;
      if (input?.kind !== "elicitation" || input.serverName !== "compatibility_fixture" || input.mode !== "form") return;
      if (!input.fields.length) {
        orchestrator.recordInput(payload.taskId, input.requestId, { kind: "elicitation", action: "accept", content: {} });
      } else form = { taskId: payload.taskId, requestId: input.requestId };
    });
    const answer = async () => {
      await vi.waitFor(() => expect(form).toBeDefined(), { timeout: 90_000, interval: 100 });
      if (!form) throw new Error("Native form did not arrive");
      expect(orchestrator.recordInput(form.taskId, form.requestId, { kind: "elicitation", action: "accept", content: { count: 0, enabled: false } })).toBe(true);
      form = undefined;
    };
    const send = (taskId: string, feedback: string, messageId: string) => {
      const handler = handlers.get("orchestrator:iterate");
      if (!handler) throw new Error("Chat IPC is not registered");
      const submission = Promise.resolve(handler({} as IpcMainInvokeEvent, { taskId, feedback, messageId }));
      submissions.push(submission);
      return submission;
    };
    let succeeded = false;
    try {
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
      const idle = await orchestrator.createTask("Reply exactly IDLE_READY. Do not call tools or modify files.", agent, cwd, "HEAD");
      taskIds.push(idle.id);
      await orchestrator.startTask(idle.id);
      await vi.waitFor(() => expect(idle.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      const session = agent === "codex" ? idle.codexThreadId : idle.claudeSessionId;
      const prompt = "Call the compatibility_fixture confirm_values MCP tool once. Then reply exactly BLOCKER_DONE. Do not call other tools, modify files, or use subagents.";
      const blocker = await orchestrator.createTask(prompt, agent, cwd, "HEAD");
      taskIds.push(blocker.id);
      await orchestrator.startTask(blocker.id);
      await vi.waitFor(() => expect(form).toBeDefined(), { timeout: 90_000, interval: 100 });
      let accepted = false;
      const input = send(idle.id, "Create admitted.txt containing exactly admitted without a newline. Reply ADMITTED_READY. Do not modify other files or use subagents.", "queued-input")
        .then(result => { accepted = true; return result; });
      await vi.waitFor(() => expect(idle.status).toBe("queued"), { timeout: 5000, interval: 50 });
      expect({ accepted, file: existsSync(path.join(idle.worktreePath, "admitted.txt")), pending: chat.snapshot(idle.id).messages.find(message => message.id === "queued-input")?.pendingDelivery,
        feedback: idle.feedback }).toEqual({ accepted: false, file: false, pending: true, feedback: [] });
      await answer();
      expect(await input).toEqual({ success: true });
      await vi.waitFor(() => expect(idle.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      expect({ file: await readFile(path.join(idle.worktreePath, "admitted.txt"), "utf8"), session: agent === "codex" ? idle.codexThreadId : idle.claudeSessionId }).toEqual({ file: "admitted", session });
      await orchestrator.iterate(blocker.id, prompt);
      await vi.waitFor(() => expect(form).toBeDefined(), { timeout: 90_000, interval: 100 });
      const cancelled = send(idle.id, "Create cancelled.txt. Do not modify other files.", "cancelled-input");
      await vi.waitFor(() => expect(idle.status).toBe("queued"), { timeout: 5000, interval: 50 });
      await orchestrator.cancelTask(idle.id);
      expect(await cancelled).toMatchObject({ success: false, error: "Message cancelled before submission" });
      await answer();
      await vi.waitFor(() => expect(blocker.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      expect({ status: idle.status, file: existsSync(path.join(idle.worktreePath, "cancelled.txt")), feedback: idle.feedback.length,
        cancelledRow: chat.snapshot(idle.id).messages.some(message => message.id === "cancelled-input") }).toEqual({
        status: "stopped", file: false, feedback: 1, cancelledRow: false,
      });
      await writeFile(path.join(liveRoot!, `${agent}-admission.json`), JSON.stringify({ idle, blocker, events, snapshot: chat.snapshot(idle.id) }, null, 2));
      succeeded = true;
    } catch (error: unknown) {
      await writeFile(path.join(liveRoot!, `${agent}-admission-failure.json`), JSON.stringify({ tasks: orchestrator.getAllTasks(), events, form, error: error instanceof Error ? error.message : String(error) }, null, 2));
      throw error;
    } finally {
      await Promise.allSettled(taskIds.map(id => orchestrator.cancelTask(id)));
      await Promise.allSettled(submissions);
      db.close(); vi.unstubAllEnvs();
      if (succeeded) await rm(cwd, { recursive: true, force: true });
    }
  }, 300_000);
});
