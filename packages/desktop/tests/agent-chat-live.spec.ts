import { describe, expect, it, vi } from "vitest";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import path from "node:path";
import { type AgentAdapter, type AgentEvent, type AgentInputResponse } from "../electron/orchestrator-adapters";
import { ClaudeCodeAdapter } from "../electron/claude-code-adapter";
import { CodexAppServerAdapter } from "../electron/codex-app-server-adapter";
import { deriveTurns } from "../src/lib/turn-derivation";
import { Orchestrator } from "../electron/orchestrator";
import { CodexRpcClient } from "../electron/codex-rpc";

// Run only in an owned qualification directory, using the installed agents'
// real authentication. No CLI or inference transport is mocked.
const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
describe.skipIf(!liveRoot)("official agent chat live qualification", () => {
  it("codex acknowledges review cancellation and closes its native process", async () => {
    const cwd = await mkdtemp(path.join(liveRoot!, "review-cancellation-"));
    const run = promisify(execFile);
    const adapter = new CodexAppServerAdapter();
    const requests = vi.spyOn(CodexRpcClient.prototype, "send");
    const events: AgentEvent[] = [];
    const closed = new Promise<void>(resolve => adapter.onExit(() => resolve()));
    adapter.onEvent(event => events.push(event));
    let succeeded = false;
    let started = false;
    try {
      await run("git", ["init", "--initial-branch=main", cwd]);
      await writeFile(path.join(cwd, "sum.ts"), "export function sum(a: number, b: number) { return a - b; }\n");
      await run("git", ["add", "sum.ts"], { cwd });
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "Owned review fixture"], { cwd });
      started = true;
      await adapter.review("Review sum.ts for arithmetic correctness. Read only this fixture. Do not modify files or ask questions.", cwd);
      await vi.waitFor(() => expect(events.some(event => event.type === "turn_started")).toBe(true), { timeout: 30_000, interval: 100 });
      await adapter.cancel();
      await closed;
      const index = requests.mock.calls.findIndex(([method]) => method === "turn/interrupt");
      if (index < 0 || requests.mock.results[index]?.type !== "return") throw new Error("Native review interrupt was not sent");
      expect(await requests.mock.results[index].value).toEqual({});
      expect({ outcome: events.filter(event => event.type === "done").at(-1)?.outcome,
        file: await readFile(path.join(cwd, "sum.ts"), "utf8") }).toEqual({ outcome: "interrupted", file: "export function sum(a: number, b: number) { return a - b; }\n" });
      await writeFile(path.join(liveRoot!, "codex-review-cancellation.json"), JSON.stringify({ events, interrupt: requests.mock.calls[index][1] }, null, 2));
      succeeded = true;
    } finally {
      await adapter.cancel();
      if (started) await closed;
      requests.mockRestore();
      if (succeeded) await rm(cwd, { recursive: true, force: true });
    }
  }, 90_000);

  it.each(["codex", "claude-code"])("%s child conversation writes through its owned task and persists its session", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, "child-chat-"));
    const run = promisify(execFile);
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 90_000 });
    let childId: string | undefined;
    const events: Array<{ taskId: string; event: AgentEvent }> = [];
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload);
      if (payload.event.type === "approval_request" && payload.event.approval) {
        orchestrator.recordApproval(payload.taskId, { requestId: payload.event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
      }
    });
    try {
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
      const parent = await orchestrator.createTask("Coordinate", agent, cwd, "HEAD", { worktreeMode: "primary" });
      const child = orchestrator.createChildTaskForAgent(parent.id, agent,
        "Write exactly child-proof to child-proof.txt in this workspace, then reply Done. Do not modify any other files or use subagents.", "reviewer");
      childId = child.id;
      await orchestrator.startTask(child.id);
      await vi.waitFor(() => {
        if (child.status === "failed") throw new Error(events.find(payload => payload.event.type === "error")?.event.error ?? "Child turn failed");
        expect(child.status).toBe("reviewing");
      }, { timeout: 90_000, interval: 100 });
      const reloaded = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite") });
      const saved = reloaded.getTask(child.id);
      expect({
        file: await readFile(path.join(cwd, "child-proof.txt"), "utf8"),
        parentStatus: parent.status, parentEvents: parent.events,
        eventOwners: [...new Set(events.map(payload => payload.taskId))],
        session: Boolean(agent === "codex" ? saved?.codexThreadId : saved?.claudeSessionId),
        status: saved?.status, mode: saved?.worktreeMode,
      }).toEqual({ file: "child-proof", parentStatus: "created", parentEvents: [], eventOwners: [child.id], session: true, status: "reviewing", mode: "existing" });
    } finally {
      if (childId) await orchestrator.cancelTask(childId);
      vi.unstubAllEnvs();
      await rm(cwd, { recursive: true, force: true });
    }
  }, 115_000);

  it.each(["codex", "claude-code"])("%s resets saved native settings through a cold task resume", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, "cold-controls-"));
    const run = promisify(execFile);
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(cwd, "settings.json"));
    const orchestrator = new Orchestrator({ persistPath: path.join(cwd, "tasks.sqlite"), timeoutMs: 120_000 });
    const catalog = agent === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    let taskId: string | undefined;
    let succeeded = false;
    orchestrator.on("orchestrator:event", ({ taskId, event }: { taskId: string; event: AgentEvent }) => {
      events.push(event);
      if (event.approval) void orchestrator.recordApproval(taskId, { requestId: event.approval.requestId, decision: "allow_once", summary: "Owned fixture" });
    });
    try {
      await run("git", ["init", "--initial-branch=main", cwd]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd });
      const models = await catalog.listModels(cwd);
      await catalog.cancel();
      const selected = models.find(model => model.isDefault && model.intelligenceLevels.length) ?? models.find(model => model.intelligenceLevels.length);
      if (!selected) throw new Error("Installed agent advertised no model with effort controls");
      const task = await orchestrator.createTask("Reply exactly COLD_READY. Do not use tools or modify files.", agent, cwd, "HEAD");
      taskId = task.id;
      task.planFirst = true;
      task.model = selected.value;
      task.reasoningLevel = selected.intelligenceLevels[0];
      await orchestrator.startTask(task.id);
      await vi.waitFor(() => expect(task.status).toBe("planning"), { timeout: 90_000, interval: 100 });
      const session = agent === "codex" ? task.codexThreadId : task.claudeSessionId;
      await orchestrator.cancelTask(task.id);
      const offset = events.length;
      await orchestrator.iterate(task.id, "Create cold-controls.txt containing exactly cold-native without a newline. Reply COLD_DONE. Do not modify other files or use subagents.", {
        mode: "instant", model: null, reasoningLevel: null,
      });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      const configuration = Object.assign({}, ...events.slice(offset).filter(event => event.type === "configuration_changed").map(event => event.configuration));
      expect({ file: await readFile(path.join(task.worktreePath, "cold-controls.txt"), "utf8"), configuration,
        model: task.model, effort: task.reasoningLevel, plan: task.planFirst, session: agent === "codex" ? task.codexThreadId : task.claudeSessionId }).toEqual({
        file: "cold-native", configuration: { mode: "instant", model: null, reasoningLevel: null }, model: undefined, effort: undefined, plan: false, session,
      });
      await writeFile(path.join(liveRoot!, `${agent}-cold-controls.json`), JSON.stringify({ task, events }, null, 2));
      succeeded = true;
    } finally {
      await catalog.cancel();
      if (taskId) await orchestrator.cancelTask(taskId);
      vi.unstubAllEnvs();
      if (succeeded) await rm(cwd, { recursive: true, force: true });
    }
  }, 240_000);

  it.each(["codex", "claude-code"])("%s discovers installed models without opening a user turn", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, "model-discovery-"));
    const adapter = agent === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent(event => events.push(event));
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    const deadline = setTimeout(() => adapter.cancel(), 30_000);
    try {
      const models = await adapter.listModels(cwd);
      expect({ found: models.length > 0, valid: models.every(model => model.value && model.label && Array.isArray(model.intelligenceLevels)), turns: events.filter(event => ["turn_started", "thread_started", "text", "tool_use", "done"].includes(event.type)) }).toEqual({ found: true, valid: true, turns: [] });
    } finally {
      clearTimeout(deadline);
      adapter.cancel();
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([exited, new Promise<void>(resolve => { timer = setTimeout(resolve, 15_000); })]);
      clearTimeout(timer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 45_000);

  it.each([["codex", "form"], ["claude-code", "form"], ["codex", "url"], ["claude-code", "url"]] as const)("%s sends a %s elicitation response to a real MCP server", async (agent, mode) => {
    const cwd = await mkdtemp(path.join(liveRoot!, "mcp-elicitation-"));
    const proof = path.join(cwd, "mcp-proof.json");
    const script = path.resolve("tests/fixtures/mcp-elicitation-server.mjs");
    const command = process.execPath;
    const args = [script, proof, agent === "claude-code" ? "tool" : "startup", mode];
    const codexConfig = `mcp_servers={compatibility_fixture={command=${JSON.stringify(command)},args=${JSON.stringify(args)},env={ELECTRON_RUN_AS_NODE="1"}}}`;
    const claudeConfig = JSON.stringify({ mcpServers: { compatibility_fixture: { command, args, env: { ELECTRON_RUN_AS_NODE: "1" } } } });
    const adapter: AgentAdapter = agent === "codex" ? new CodexAppServerAdapter(["-c", codexConfig, "-c", 'approval_policy="on-request"']) : new ClaudeCodeAdapter(["--mcp-config", claudeConfig, "--strict-mcp-config"]);
    const events: AgentEvent[] = [];
    const response: AgentInputResponse = mode === "url"
      ? { kind: "elicitation", action: "accept" }
      : { kind: "elicitation", action: "accept", content: { count: 0, enabled: false } };
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    adapter.onEvent(event => {
      events.push(event);
      if (event.type === "approval_request" && event.approval) adapter.respondApproval?.(event.approval.requestId, "allow_once");
      if (event.type === "input_request" && event.inputRequest?.kind === "elicitation") {
        adapter.respondInput?.(event.inputRequest.requestId, response);
      }
    });
    try {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => { unsubscribe(); reject(new Error("MCP turn did not complete within 90 seconds")); }, 90_000);
        const unsubscribe = adapter.onEvent(event => {
          if (event.type !== "done") return;
          clearTimeout(timer);
          unsubscribe();
          if (event.outcome === "completed") resolve();
          else reject(new Error(events.find(entry => entry.type === "error")?.error ?? "MCP turn failed"));
        });
        adapter.spawn(agent === "claude-code" ? "Call the compatibility_fixture confirm_values MCP tool once. Then reply Ready. Do not use other tools or modify files." : "Wait for MCP startup, then reply Ready. Do not run tools or modify files.", cwd);
      });
      await vi.waitFor(async () => {
        const result = JSON.parse(await readFile(proof, "utf8")) as { action?: string; content?: unknown };
        // Codex 0.163.0 core/session/handlers.rs normalizes accepted absent
        // content to {}, including URL consent; Claude preserves omission.
        const urlContent = agent === "codex" ? {} : null;
        const expectedContent = mode === "url" ? urlContent : { count: 0, enabled: false };
        expect({ action: result.action, content: result.content ?? null }).toEqual({ action: "accept", content: expectedContent });
      }, { timeout: 5_000, interval: 100 });
      expect(events.some(event => event.type === "request_resolved")).toBe(true);
      expect(events.find(event => event.type === "input_request")?.inputRequest).toMatchObject({ kind: "elicitation", mode });
    } catch (error: unknown) {
      const trace = await readFile(`${proof}.trace`, "utf8").catch(() => "MCP fixture did not start");
      throw new Error(`${error instanceof Error ? error.message : "MCP request failed"}; activity: ${events.map(event => event.type).join(",")}; fixture: ${trace}`);
    } finally {
      adapter.cancel();
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([exited, new Promise<void>(resolve => { timer = setTimeout(resolve, 15_000); })]);
      clearTimeout(timer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 115_000);

  it.each(["codex", "claude-code"])("%s preserves inference context across follow-up, process restart, and resume", async agent => {
    const cwd = await mkdtemp(path.join(liveRoot!, `${agent}-session-`));
    const resumedCwd = agent === "codex" ? await mkdtemp(path.join(liveRoot!, "codex-resumed-workspace-")) : cwd;
    const token = randomUUID();
    const rememberPrompt = `Remember this exact fixture token for later: ${token}. Reply exactly "Remembered". Do not use tools.`;
    const recallPrompt = agent === "codex"
      ? "Write the exact fixture token I asked you to remember earlier into resume-proof.txt in the current workspace, without a newline. Then reply with only that token. Do not modify other files."
      : "Reply with the exact fixture token I asked you to remember earlier. Do not use tools or include any other text.";
    const events: AgentEvent[] = [];
    let adapter = agent === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const followup = agent === "codex" ? "Reply exactly Acknowledged. Do not use tools." : "/mcp";
    const identityType = agent === "codex" ? "thread_started" : "session_started";
    const identity = (event: AgentEvent) => event.threadId ?? event.sessionId;
    let exited: Promise<void>;
    const connect = () => {
      exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
      adapter.onEvent(event => {
        events.push(event);
        if (event.type === "approval_request" && event.approval) adapter.respondApproval?.(event.approval.requestId, "allow_once");
      });
    };
    const turn = async (send: () => void | Promise<void>) => {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => { unsubscribe(); reject(new Error(`${agent} session turn timed out`)); }, 90_000);
        const unsubscribe = adapter.onEvent(event => {
          if (event.type !== "done") return;
          clearTimeout(timer);
          unsubscribe();
          const failure = events.find(entry => entry.type === "error");
          if (failure) reject(new Error(failure.error)); else resolve();
        });
        Promise.resolve(send()).catch(error => { clearTimeout(timer); unsubscribe(); reject(error); });
      });
    };
    connect();
    try {
      await turn(() => adapter.spawn(rememberPrompt, cwd, undefined, { mode: "instant" }));
      await turn(() => adapter.iterate(followup, { model: null, mode: "instant", reasoningLevel: null }));
      const sessionId = events.filter(event => event.type === identityType).map(identity)[0];
      if (!sessionId) throw new Error("Native session identity is missing");
      await adapter.cancel();
      await exited!;
      if (agent === "codex") {
        const resumed = new CodexAppServerAdapter();
        resumed.resumeThreadId = sessionId;
        adapter = resumed;
      } else {
        const resumed = new ClaudeCodeAdapter();
        resumed.resumeSessionId = sessionId;
        adapter = resumed;
      }
      connect();
      await turn(() => adapter.spawn(recallPrompt, resumedCwd));
      expect({ sessions: [...new Set(events.filter(event => event.type === identityType).map(identity))], turns: deriveTurns(events, [followup, recallPrompt]).filter(entry => entry.complete).length, recalled: events.filter(event => event.type === "done").at(-1)?.content?.trim() }).toEqual({ sessions: [sessionId], turns: 3, recalled: token });
      if (agent === "codex") {
        expect(await readFile(path.join(resumedCwd, "resume-proof.txt"), "utf8")).toBe(token);
        await expect(readFile(path.join(cwd, "resume-proof.txt"), "utf8")).rejects.toMatchObject({ code: "ENOENT" });
      }
    } finally {
      await adapter.cancel();
      await exited!;
      await rm(cwd, { recursive: true, force: true });
      if (resumedCwd !== cwd) await rm(resumedCwd, { recursive: true, force: true });
    }
  }, 290_000);

  it.each(["codex", "claude-code"])("%s plans without editing, then implements on the same session after native settings reset", async agent => {
    const cwd = await realpath(await mkdtemp(path.join(liveRoot!, `${agent}-native-controls-`)));
    const planDirectory = path.join(cwd, "plans");
    const adapter = agent === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter(["--settings", JSON.stringify({ plansDirectory: "plans" })]);
    const events: AgentEvent[] = [];
    let planning = true;
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    adapter.onEvent(event => {
      events.push(event);
      if (event.type === "approval_request" && event.approval) {
        let decision = "allow_once";
        if (agent === "claude-code" && planning) {
          const input = JSON.parse(event.approval.detail ?? "{}") as { file_path?: string };
          const relative = input.file_path ? path.relative(planDirectory, input.file_path) : undefined;
          const writesPlan = event.approval.kind === "file" && relative !== undefined && relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
          // Hold planning at its owned plan file; approving ExitPlanMode
          // would authorize implementation immediately.
          if (!writesPlan) decision = "deny";
        }
        adapter.respondApproval(event.approval.requestId, decision);
      }
      if (event.type === "input_request" && event.inputRequest && event.inputRequest.kind !== "elicitation") {
        adapter.respondInput(event.inputRequest.requestId, { kind: "questions", answers: Object.fromEntries(event.inputRequest.questions.map(question => [question.id, [question.options?.[0]?.label ?? "Use the specified defaults"]])) });
      }
    });
    const turn = async (send: () => void | Promise<void>) => new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => { unsubscribe(); reject(new Error("Native settings turn timed out")); }, 90_000);
      const unsubscribe = adapter.onEvent(event => {
        if (event.type !== "done") return;
        clearTimeout(timer);
        unsubscribe();
        if (event.outcome !== "completed") reject(new Error(events.findLast(event => event.type === "error")?.error ?? "Native settings turn failed"));
        else resolve();
      });
      Promise.resolve(send()).catch(error => { clearTimeout(timer); unsubscribe(); reject(error); });
    });
    try {
      await turn(() => adapter.spawn('Create compatibility-native.txt containing exactly "native-controls" without a newline. Do this now. The specification is complete; make all remaining choices yourself.', cwd, undefined, { mode: "plan" }));
      await expect(readFile(path.join(cwd, "compatibility-native.txt"), "utf8")).rejects.toMatchObject({ code: "ENOENT" });
      planning = false;
      await turn(() => adapter.iterate('Implement the plan now. Create compatibility-native.txt containing exactly "native-controls" without a newline. Do not change other files.', { mode: "instant", model: null, reasoningLevel: null }));
      const identities = events.filter(event => event.type === "thread_started" || event.type === "session_started").map(event => event.threadId ?? event.sessionId);
      expect({ content: await readFile(path.join(cwd, "compatibility-native.txt"), "utf8"), sessions: new Set(identities).size, complete: events.filter(event => event.type === "done" && event.outcome === "completed").length }).toEqual({ content: "native-controls", sessions: 1, complete: 2 });
    } finally {
      adapter.cancel();
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([exited, new Promise<void>(resolve => { timer = setTimeout(resolve, 15_000); })]);
      clearTimeout(timer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 200_000);

  it("claude answers an actual question before writing the selected value", async () => {
    const cwd = await mkdtemp(path.join(liveRoot!, "claude-question-"));
    const adapter = new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    const answers: Array<{ requestId: string; delivered: boolean }> = [];
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await new Promise<void>((resolve, reject) => {
        timer = setTimeout(() => reject(new Error("Question turn did not complete within 90 seconds")), 90_000);
        adapter.onEvent(event => {
          events.push(event);
          if (event.type === "approval_request" && event.approval) adapter.respondApproval(event.approval.requestId, "allow_once");
          if (event.type === "input_request" && event.inputRequest && event.inputRequest.kind !== "elicitation") {
            const request = event.inputRequest;
            const values = Object.fromEntries(request.questions.map(question => [question.id, [question.options?.find(option => option.label.toLowerCase() === "beta")?.label ?? "beta"]]));
            answers.push({ requestId: request.requestId, delivered: adapter.respondInput(request.requestId, { kind: "questions", answers: values }) });
          }
          if (event.type === "done") {
            clearTimeout(timer);
            if (event.outcome === "completed") resolve();
            else reject(new Error(events.find(entry => entry.type === "error")?.error ?? "Question turn failed"));
          }
        });
        adapter.spawn('Use AskUserQuestion to ask one question with exactly two option labels: "alpha" and "beta". Wait for the answer; do not guess. Then write question-proof.txt containing only the chosen option in lowercase, without a newline. Do nothing else.', cwd);
      });
      expect({ content: await readFile(path.join(cwd, "question-proof.txt"), "utf8"), answers: answers.length, delivered: answers.every(answer => answer.delivered), resolved: answers.every(answer => events.some(event => event.type === "request_resolved" && event.requestId === answer.requestId)) }).toEqual({ content: "beta", answers: 1, delivered: true, resolved: true });
    } finally {
      clearTimeout(timer);
      adapter.cancel();
      let exitTimer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([exited, new Promise<void>(resolve => { exitTimer = setTimeout(resolve, 15_000); })]);
      clearTimeout(exitTimer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 110_000);

  it("claude infers with discovered model/effort choices and resets both on the same session", async () => {
    const cwd = await mkdtemp(path.join(liveRoot!, "claude-model-effort-"));
    const discovery = new ClaudeCodeAdapter();
    const discoveryExited = new Promise<void>(resolve => { discovery.onExit(() => resolve()); });
    const discoveryDeadline = setTimeout(() => discovery.cancel(), 45_000);
    const adapter = new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    adapter.onEvent(event => events.push(event));
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    const turn = async (send: () => void | Promise<void>) => new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => { unsubscribe(); reject(new Error("Model/effort turn timed out")); }, 90_000);
      const unsubscribe = adapter.onEvent(event => {
        if (event.type !== "done") return;
        clearTimeout(timer);
        unsubscribe();
        if (event.outcome === "completed") resolve();
        else reject(new Error(events.findLast(event => event.type === "error")?.error ?? "Model/effort turn failed"));
      });
      Promise.resolve(send()).catch(error => { clearTimeout(timer); unsubscribe(); reject(error); });
    });
    try {
      const models = await discovery.listModels(cwd);
      clearTimeout(discoveryDeadline);
      discovery.cancel();
      await discoveryExited;
      const selected = models.find(model => model.intelligenceLevels.length > 0);
      if (!selected) throw new Error("Installed Claude advertised no model with effort controls");
      const effort = selected.intelligenceLevels[0];
      await turn(() => adapter.spawn('Reply exactly "control-one". Do not use tools.', cwd, undefined, { model: selected.value, reasoningLevel: effort }));
      await turn(() => adapter.iterate('Reply exactly "control-two". Do not use tools.', { model: null, reasoningLevel: null }));
      const settings = events.filter(event => event.type === "configuration_changed" && (event.configuration?.model !== undefined || event.configuration?.reasoningLevel !== undefined)).map(event => event.configuration);
      expect({ settings, answers: events.filter(event => event.type === "done").map(event => event.content?.trim()), sessions: new Set(events.filter(event => event.type === "session_started").map(event => event.sessionId)).size, usage: events.filter(event => event.type === "usage").length >= 2 }).toEqual({
        settings: [{ model: selected.value }, { reasoningLevel: effort }, { model: null }, { reasoningLevel: null }], answers: ["control-one", "control-two"], sessions: 1, usage: true,
      });
    } finally {
      clearTimeout(discoveryDeadline);
      discovery.cancel();
      adapter.cancel();
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([Promise.all([discoveryExited, exited]), new Promise<void>(resolve => { timer = setTimeout(resolve, 15_000); })]);
      clearTimeout(timer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 245_000);

  it.skipIf(process.platform === "win32")("claude cancels a real startup hook without opening a user turn", async () => {
    const cwd = await mkdtemp(path.join(liveRoot!, "claude-startup-cancel-"));
    const marker = path.join(cwd, "hook.pid");
    const script = path.resolve("tests/fixtures/pending-startup-hook.mjs");
    const command = [process.execPath, script, marker].map(value => "'" + value.replaceAll("'", "'\\''") + "'").join(" ");
    const settings = { hooks: { SessionStart: [{ hooks: [{ type: "command", command }] }] } };
    const adapter = new ClaudeCodeAdapter(["--settings", JSON.stringify(settings)]);
    const events: AgentEvent[] = [];
    adapter.onEvent(event => events.push(event));
    const exited = new Promise<void>(resolve => { adapter.onExit(() => resolve()); });
    let hookPid: number | undefined;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    const hookAlive = () => {
      if (!hookPid) return false;
      try { process.kill(hookPid, 0); return true; }
      catch (error: unknown) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; }
    };
    try {
      const catalog = adapter.listModels(cwd).catch(() => undefined);
      await vi.waitFor(async () => { hookPid = Number(await readFile(marker, "utf8")); expect(hookPid > 0).toBe(true); }, { timeout: 30_000 });
      adapter.cancel();
      await Promise.race([exited, new Promise<never>((_resolve, reject) => { deadline = setTimeout(() => reject(new Error("Claude did not exit after startup cancellation")), 12_000); })]);
      await catalog;
      await vi.waitFor(() => expect(hookAlive()).toBe(false), { timeout: 3_000 });
      expect(events.some(event => event.type === "turn_started" || event.type === "text" || event.type === "usage")).toBe(false);
    } finally {
      clearTimeout(deadline);
      adapter.cancel();
      if (hookAlive() && hookPid) process.kill(hookPid, "SIGTERM");
      await rm(cwd, { recursive: true, force: true });
    }
  }, 50_000);

  it.each(["codex", "claude-code"])("%s writes a file and produces a visible completed turn", async agentType => {
    const cwd = await mkdtemp(path.join(liveRoot!, `${agentType}-`));
    const adapter: AgentAdapter = agentType === "codex" ? new CodexAppServerAdapter() : new ClaudeCodeAdapter();
    const events: AgentEvent[] = [];
    let exitResolve: (() => void) | undefined;
    const exited = new Promise<void>(resolve => { exitResolve = resolve; });
    adapter.onExit(() => exitResolve?.());
    try {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`Agent did not complete within 90 seconds; activity: ${events.map(event => `${event.type}:${event.tool ?? ""}`).join(", ")}`)), 90_000);
        adapter.onEvent(event => {
          events.push(event);
          // Simulate the user approving this fixture's requested file write.
          if (event.type === "approval_request" && event.approval) {
            adapter.respondApproval?.(event.approval.requestId, "allow_once");
          }
          if (event.type === "done") {
            clearTimeout(timer);
            const failure = events.find(entry => entry.type === "error");
            if (failure) reject(new Error(failure.error));
            else resolve();
          }
        });
        adapter.spawn('Create compatibility-proof.txt containing exactly "official-agent-ok" without a newline. Then reply "File created". Do nothing else.', cwd);
      });
      expect({
        content: await readFile(path.join(cwd, "compatibility-proof.txt"), "utf8"),
        complete: deriveTurns(events, [])[0]?.complete,
        doneCount: events.filter(event => event.type === "done").length,
        streamed: events.some(event => event.type === "text"),
        usage: events.some(event => event.type === "usage"),
      }).toEqual({ content: "official-agent-ok", complete: true, doneCount: 1, streamed: true, usage: true });
      if (adapter instanceof CodexAppServerAdapter) {
        const directory = path.join(cwd, "filesystem-proof");
        const original = path.join(directory, "original.txt");
        const copy = path.join(directory, "copy.txt");
        await adapter.fsCreateDirectory(directory);
        await adapter.fsWriteFile(original, "UTF-8 🦀\n");
        await adapter.fsCopy(original, copy);
        expect({
          content: await adapter.fsReadFile(copy),
          isFile: (await adapter.fsGetMetadata(copy)).isFile,
          entries: (await adapter.fsReadDirectory(directory)).length,
        }).toEqual({ content: "UTF-8 🦀\n", isFile: true, entries: 2 });
        await adapter.fsRemove(directory, true);
        const { sessionId } = await adapter.execCommand({
          command: process.execPath,
          args: ["-e", 'process.stdout.write("command-proof")'],
          env: { ELECTRON_RUN_AS_NODE: "1" },
          cwd,
          timeoutMs: 5000,
        });
        await vi.waitFor(() => {
          expect(events.find(event => event.type === "command_exited" && event.commandSessionId === sessionId)?.exitCode).toBe(0);
        }, { timeout: 10_000 });
        expect(events.filter(event => event.type === "command_output" && event.commandSessionId === sessionId).map(event => event.content).join("")).toBe("command-proof");
      }
    } finally {
      adapter.cancel();
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([exited, new Promise<void>(resolve => { timer = setTimeout(resolve, 15_000); })]);
      clearTimeout(timer);
      await rm(cwd, { recursive: true, force: true });
    }
  }, 110_000);
});
