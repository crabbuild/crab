import { describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import type { AgentEvent } from "../electron/orchestrator-adapters";
import { AgentWorkflowRunner } from "../electron/agent-workflows/runner";
import { AgentWorkflowRunStore } from "../electron/agent-workflows/persistence";
import type { AgentWorkflowDefinition, AgentWorkflowEvent, AgentWorkflowRun } from "../electron/agent-workflows/types";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);

// Keep workflow state isolated while exercising the real runner, task owner,
// native controls, and providers. This fixture does not qualify run persistence.
class FixtureRunStore extends AgentWorkflowRunStore {
  private runs = new Map<string, AgentWorkflowRun>();
  private events: AgentWorkflowEvent[] = [];
  override async list(): Promise<AgentWorkflowRun[]> { return structuredClone([...this.runs.values()]); }
  override async get(id: string): Promise<AgentWorkflowRun | null> { return structuredClone(this.runs.get(id) ?? null); }
  override async save(value: AgentWorkflowRun): Promise<void> { this.runs.set(value.id, structuredClone(value)); }
  override async appendEvent(value: AgentWorkflowEvent): Promise<void> { this.events.push(structuredClone(value)); }
  override async listEvents(id: string): Promise<AgentWorkflowEvent[]> { return structuredClone(this.events.filter(value => value.runId === id)); }
}

describe.skipIf(!liveRoot)("native workflow controls", () => {
  it.each(["codex", "claude-code"] as const)("%s plans through the workflow caller and implements only after native mode changes", async agent => {
    const directory = await mkdtemp(path.join(liveRoot!, "workflow-controls-"));
    const settings = path.join(directory, "settings.json");
    const planDirectory = path.join(directory, "plans");
    await writeFile(settings, JSON.stringify({ orchestratorAgentConfigs: {
      "claude-code": { extraArgs: ["--settings", JSON.stringify({ plansDirectory: planDirectory })] },
    } }));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", settings);
    const orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite"), timeoutMs: 120_000 });
    const store = new FixtureRunStore();
    const runner = new AgentWorkflowRunner(orchestrator, store);
    const events: Array<{ taskId: string; event: AgentEvent }> = [];
    let planning = true;
    let succeeded = false;
    orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
      events.push(payload);
      const { taskId, event } = payload;
      if (event.approval) {
        let decision = "allow_once";
        if (agent === "claude-code" && planning) {
          const input = JSON.parse(event.approval.detail ?? "{}") as { file_path?: string };
          const relative = input.file_path ? path.relative(planDirectory, input.file_path) : undefined;
          const writesPlan = event.approval.kind === "file" && relative !== undefined && relative !== ".."
            && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
          if (!writesPlan) decision = "deny";
        }
        void orchestrator.recordApproval(taskId, { requestId: event.approval.requestId, decision, summary: "Owned fixture" });
      }
      const input = event.inputRequest;
      if (input && input.kind !== "elicitation") orchestrator.recordInput(taskId, input.requestId, {
        kind: "questions", answers: Object.fromEntries(input.questions.map(question => [question.id, [question.options?.[0]?.label ?? "Use the specified defaults"]])),
      });
    });
    try {
      await run("git", ["init", "--initial-branch=main", directory]);
      await run("git", ["-c", "user.name=Crab Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "Owned fixture"], { cwd: directory });
      const definition: AgentWorkflowDefinition = {
        schemaVersion: 2, id: "native-controls", name: "Native controls", definitionVersion: 1, revisionId: "fixture",
        maxParallelNodes: 1, defaultAgentType: agent, edges: [], layout: {}, createdAt: 1, updatedAt: 1,
        nodes: [{ id: "implement", type: "agent_task", title: "Implement", planFirst: true,
          instructions: "Create workflow-proof.txt containing exactly workflow-native without a newline. Do this now. The specification is complete; make all remaining choices yourself. Do not use subagents." }],
      };
      const workflow = await runner.startRun(directory, definition);
      await vi.waitFor(async () => expect((await runner.getRun(workflow.id))?.nodeRuns[0].status).toBe("needs_review"), { timeout: 90_000, interval: 100 });
      const node = (await runner.getRun(workflow.id))?.nodeRuns[0];
      const task = node?.taskId ? orchestrator.getTask(node.taskId) : undefined;
      if (!task || !node) throw new Error("Workflow did not create its task");
      expect(task.status).toBe("planning");
      await expect(readFile(path.join(task.worktreePath, "workflow-proof.txt"), "utf8")).rejects.toMatchObject({ code: "ENOENT" });
      const session = agent === "codex" ? task.codexThreadId : task.claudeSessionId;
      planning = false;
      await orchestrator.iterate(task.id, "Implement now. Create workflow-proof.txt containing exactly workflow-native without a newline. Do not modify other files or use subagents.", { mode: "instant" });
      await vi.waitFor(() => expect(task.status).toBe("reviewing"), { timeout: 90_000, interval: 100 });
      await vi.waitFor(async () => expect((await runner.getRun(workflow.id))?.status).toBe("completed"), { timeout: 5000, interval: 50 });
      const completed = await runner.getRun(workflow.id);
      if (!completed) throw new Error("Completed workflow is missing");
      expect({ file: await readFile(path.join(task.worktreePath, "workflow-proof.txt"), "utf8"), session: agent === "codex" ? task.codexThreadId : task.claudeSessionId,
        workflow: completed.status, node: completed.nodeRuns[0].status }).toEqual({ file: "workflow-native", session, workflow: "completed", node: "completed" });
      await writeFile(path.join(liveRoot!, `${agent}-workflow-controls.json`), JSON.stringify({ task, workflow: completed, events }, null, 2));
      succeeded = true;
    } finally {
      await Promise.allSettled(orchestrator.getAllTasks().map(task => orchestrator.cancelTask(task.id)));
      vi.unstubAllEnvs();
      if (succeeded) await rm(directory, { recursive: true, force: true });
    }
  }, 240_000);
});
