import { describe, expect, it, vi } from "vitest";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { ChatDatabase } from "../electron/orchestrator-chat-db";
import { registerChatIpc, ChatJournal } from "../electron/orchestrator-chat";
import type { AgentEvent } from "../electron/orchestrator-adapters";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { applyAgentEvent, type ChatMessage } from "../electron/agent-chat-messages";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
describe.skipIf(!liveRoot)("mixed agent teams with installed providers", () => {
  it.each(["codex", "claude-code"] as const)("%s renders native sub-agents independently of the parent", async provider => {
    const cwd = await mkdtemp(path.join(liveRoot!, `${provider}-native-`));
    const run = promisify(execFile);
    await run("git", ["init", "--initial-branch=main", cwd]);
    const adapter = provider === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent(event => {
      events.push(event);
      if (event.approval) adapter.respondApproval(event.approval.requestId, "allow_once");
    });
    try {
      await adapter.spawn("Use your built-in native sub-agent tool to delegate this exact task: Create native-proof.txt containing exactly native-child-ready with no newline. Do not spawn further agents. Then reply NATIVE_CHILD_READY. Wait for that sub-agent to finish, then reply NATIVE_PARENT_READY. Do not create the file yourself. Use a regular sub-agent, not an agent team.", cwd, undefined, { mode: "instant" });
      await vi.waitFor(() => expect(events.some(event => event.type === "done")).toBe(true), { timeout: 180_000, interval: 200 });
      const messages = events.reduce((messages, event, index) => applyAgentEvent(messages, event, String(index)), [] as ChatMessage[]);
      expect({ file: await readFile(path.join(cwd, "native-proof.txt"), "utf8"), childOutput: messages.some(message => message.subagent && message.messages?.some(entry => entry.content.includes("NATIVE_CHILD_READY"))),
        parentOutput: messages.filter(message => message.kind === "text").map(message => message.content).join("").includes("NATIVE_PARENT_READY") })
        .toEqual({ file: "native-child-ready", childOutput: true, parentOutput: true });
      await writeFile(path.join(liveRoot!, `${provider}-native-transcript.json`), JSON.stringify(messages));
    } finally { await adapter.cancel(); }
  }, 210_000);

  it.each(["codex", "claude-code"] as const)("%s delegates to the other provider and reads the saved result", async provider => {
    const cwd = await mkdtemp(path.join(liveRoot!, `${provider}-team-`));
    const run = promisify(execFile);
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    const owner = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 180_000 });
    const db = new ChatDatabase(path.join(cwd, "chat.sqlite"));
    const journal = registerChatIpc({ handle: () => {} }, owner, db, () => null);
    const events: AgentEvent[] = [];
    owner.on("orchestrator:event", ({ taskId, event }: { taskId: string; event: AgentEvent }) => {
      events.push(event);
      if (event.approval) void owner.recordApproval(taskId, { requestId: event.approval.requestId, decision: "allow_once", summary: "Owned team fixture" });
    });
    try {
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned team fixture"], { cwd });
      const other = provider === "codex" ? "claude-code" : "codex";
      const prompt = `Use the crab_spawn_agent tool to delegate to provider ${other}, name Verifier, with this exact prompt: "Create team-proof.txt containing exactly mixed-team-ready without a newline in this workspace. Use only file writing tools or shell. Do not spawn agents. Then reply CHILD_RESULT_READY." Wait for the child's current turn with crab_wait_agent (repeat if still running), read its saved output with crab_read_agent, then reply TEAM_RESULT_READY. Do not create the file yourself or use native delegation tools.`;
      const task = await owner.createTask(prompt, provider, cwd, "HEAD", { worktreeMode: "primary", planFirst: false });
      await owner.startTask(task.id);
      await vi.waitFor(() => {
        if (task.status === "failed") throw new Error(events.filter(event => event.type === "error").map(event => event.error).join("\n"));
        expect(task.status).toBe("reviewing");
      }, { timeout: 180_000, interval: 200 });
      const children = owner.getAllTasks().filter(child => child.parentTaskId === task.id);
      const restored = new ChatJournal(db, id => owner.getTask(id)).snapshot(task.id).messages;
      const child = restored.find(message => message.subagent?.taskId === children[0]?.id);
      expect({ file: await readFile(path.join(cwd, "team-proof.txt"), "utf8"), providers: children.map(child => child.agentType),
        savedResult: child?.messages?.some(message => message.content.includes("CHILD_RESULT_READY")), parentResult: restored.some(message => message.content.includes("TEAM_RESULT_READY")) })
        .toEqual({ file: "mixed-team-ready", providers: [other], savedResult: true, parentResult: true });
      await owner.cancelTask(task.id);
      await owner.iterate(task.id, `Use crab_send_message to send agent_id ${children[0].id} this instruction: "Append exactly -message-delivered to team-proof.txt, with no newline. Do not change other files or spawn agents. Then reply MESSAGE_RESULT_READY." Wait for that agent with crab_wait_agent, read it with crab_read_agent, then reply TEAM_MESSAGE_READY. Do not edit the file yourself.`);
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 180_000, interval: 200 });
      expect({ file: await readFile(path.join(cwd, "team-proof.txt"), "utf8"),
        delivery: journal.snapshot(task.id).messages.some(message => message.providerEvent?.agentMessage?.delivery === "accepted"),
        reply: journal.snapshot(task.id).messages.some(message => message.content.includes("TEAM_MESSAGE_READY")) })
        .toEqual({ file: "mixed-team-ready-message-delivered", delivery: true, reply: true });
      await writeFile(path.join(liveRoot!, `${provider}-team-transcript.json`), JSON.stringify(journal.snapshot(task.id).messages));
    } finally {
      await owner.shutdown();
      db.close();
      vi.unstubAllEnvs();
    }
  }, 420_000);
});
