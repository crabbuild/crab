import { EventEmitter } from "node:events";
import { describe, expect, it } from "vitest";

import type { Orchestrator, Task, TaskStatus } from "../electron/orchestrator";
import type { AgentWorkflowDefinition, AgentWorkflowEvent, AgentWorkflowRun } from "../electron/agent-workflows/types";
import { AgentWorkflowRunner } from "../electron/agent-workflows/runner";
import type { AgentWorkflowDefinitionStore, AgentWorkflowRunStore } from "../electron/agent-workflows/persistence";

class MemoryRunStore {
  runs = new Map<string, AgentWorkflowRun>();
  events: AgentWorkflowEvent[] = [];

  async list(repoPath?: string): Promise<AgentWorkflowRun[]> {
    const runs = Array.from(this.runs.values());
    return repoPath ? runs.filter((run) => run.repoPath === repoPath) : runs;
  }

  async get(runId: string): Promise<AgentWorkflowRun | null> {
    const run = this.runs.get(runId);
    return run ? structuredClone(run) : null;
  }

  async save(run: AgentWorkflowRun): Promise<void> {
    this.runs.set(run.id, structuredClone(run));
  }

  async appendEvent(event: AgentWorkflowEvent): Promise<void> {
    this.events.push(event);
  }

  async listEvents(runId: string): Promise<AgentWorkflowEvent[]> {
    return this.events.filter((event) => event.runId === runId);
  }
}

class MemoryDefinitionStore {
  definitions = new Map<string, AgentWorkflowDefinition>();

  async get(_repoPath: string, definitionId: string): Promise<AgentWorkflowDefinition | null> {
    const definition = this.definitions.get(definitionId);
    return definition ? structuredClone(definition) : null;
  }
}

class FakeOrchestrator extends EventEmitter {
  tasks = new Map<string, Task>();
  iterateCalls: Array<{ taskId: string; feedback: string }> = [];
  cancelCalls: string[] = [];
  private nextId = 1;

  async createTask(prompt: string, agentType: string, repoPath: string): Promise<Task> {
    const id = `task-${this.nextId++}`;
    const task = {
      id,
      prompt,
      agentType,
      status: "created",
      branch: `agent/${id}`,
      worktreePath: `/tmp/${id}`,
      baseRef: "HEAD",
      repoPath,
      createdAt: Date.now(),
      events: [],
      feedback: [],
      todos: [],
    } as Task;
    this.tasks.set(id, task);
    this.emit("orchestrator:task-status", { taskId: id, status: task.status as TaskStatus });
    return task;
  }

  async startTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error("missing task");
    task.status = "running";
    this.emit("orchestrator:task-status", { taskId, status: "running" satisfies TaskStatus });
  }

  async iterate(taskId: string, feedback: string): Promise<void> {
    this.iterateCalls.push({ taskId, feedback });
    const task = this.tasks.get(taskId);
    if (task) task.status = "running";
    this.emit("orchestrator:task-status", { taskId, status: "iterating" satisfies TaskStatus });
  }

  async cancelTask(taskId: string): Promise<void> {
    this.cancelCalls.push(taskId);
    const task = this.tasks.get(taskId);
    if (task) {
      task.status = "stopped";
      this.emit("orchestrator:task-status", { taskId, status: "stopped" satisfies TaskStatus });
    }
  }

  getTask(taskId: string): Task | undefined {
    return this.tasks.get(taskId);
  }

  async persistTasks(): Promise<void> {}

  finishTask(taskId: string, status: "reviewing" | "planning" | "failed", content = "done"): void {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error("missing task");
    task.status = status;
    task.events.push({ type: "text", content } as never);
    this.emit("orchestrator:task-status", { taskId, status });
  }

  setTaskStatusSilently(taskId: string, status: "reviewing" | "planning" | "failed", content = "done"): void {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error("missing task");
    task.status = status;
    task.events.push({ type: "text", content } as never);
  }
}

function definition(overrides: Partial<AgentWorkflowDefinition> = {}): AgentWorkflowDefinition {
  return {
    schemaVersion: 2,
    id: "wf",
    name: "Workflow",
    definitionVersion: 1,
    revisionId: "rev-1",
    maxParallelNodes: 2,
    defaultAgentType: "codex",
    nodes: [
      { id: "a", type: "agent_task", title: "A", instructions: "Do A" },
      { id: "b", type: "agent_task", title: "B", instructions: "Do B" },
    ],
    edges: [{ id: "a-b", sourceNodeId: "a", targetNodeId: "b", condition: { sourceStatus: "completed" } }],
    layout: {},
    createdAt: 1,
    updatedAt: 1,
    ...overrides,
  };
}

async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await Promise.resolve();
}

async function waitForRun(
  store: MemoryRunStore,
  runId: string,
  predicate: (run: AgentWorkflowRun) => boolean,
): Promise<AgentWorkflowRun> {
  for (let i = 0; i < 25; i += 1) {
    const run = await store.get(runId);
    if (run && predicate(run)) return run;
    await settle();
  }
  const run = await store.get(runId);
  if (!run) throw new Error("run not found");
  return run;
}

describe("AgentWorkflowRunner", () => {
  it("auto-completes agent nodes and unblocks downstream by default", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition());

    expect(orchestrator.tasks.size).toBe(1);
    const firstTaskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(firstTaskId, "reviewing", "A summary");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "a" && nodeRun.status === "completed")
      && candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "b" && nodeRun.status === "running"),
    );
    const firstNodeRun = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "a")!;
    expect(firstNodeRun.status).toBe("completed");
    expect(firstNodeRun.outputSummary).toBe("A summary");
    expect(orchestrator.tasks.size).toBe(2);
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "b")?.status).toBe("running");
  });

  it("waits for approval when an agent node requires review", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "agent_task", title: "A", instructions: "Do A", requireReview: true },
        { id: "b", type: "agent_task", title: "B", instructions: "Do B" },
      ],
    }));

    const firstTaskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(firstTaskId, "reviewing", "A summary");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "a" && nodeRun.status === "needs_review"),
    );

    const firstNodeRun = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "a")!;
    run = await runner.approveNode(run.id, firstNodeRun.id, "Approved A");

    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "a")?.status).toBe("completed");
    expect(orchestrator.tasks.size).toBe(2);
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "b")?.status).toBe("running");
  });

  it("respects maxParallelNodes for independent roots", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    await runner.startRun("/tmp/repo", definition({
      maxParallelNodes: 1,
      edges: [],
    }));

    expect(orchestrator.tasks.size).toBe(1);
  });

  it("routes review feedback to orchestrator.iterate", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "agent_task", title: "A", instructions: "Do A", requireReview: true },
        { id: "b", type: "agent_task", title: "B", instructions: "Do B" },
      ],
    }));
    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(taskId, "reviewing", "ready");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "a" && nodeRun.status === "needs_review"),
    );
    const nodeRun = run.nodeRuns.find((candidate) => candidate.nodeId === "a")!;

    await runner.requestNodeChanges(run.id, nodeRun.id, "Please adjust scope.");

    expect(orchestrator.iterateCalls).toEqual([{ taskId, feedback: "Please adjust scope." }]);
    expect((await store.get(run.id))?.nodeRuns.find((candidate) => candidate.nodeId === "a")?.status).toBe("running");
  });

  it("stops linked orchestrator tasks even when the workflow node is waiting for review", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "agent_task", title: "A", instructions: "Do A", requireReview: true },
        { id: "b", type: "agent_task", title: "B", instructions: "Do B" },
      ],
    }));
    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(taskId, "reviewing", "ready");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "a" && nodeRun.status === "needs_review"),
    );

    const stopped = await runner.cancelRun(run.id);

    expect(orchestrator.cancelCalls).toContain(taskId);
    expect(stopped.status).toBe("cancelled");
    expect(stopped.nodeRuns.find((nodeRun) => nodeRun.nodeId === "a")?.status).toBe("cancelled");
  });

  it("routes if/else nodes through true and false branches", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        {
          id: "branch",
          type: "if_else",
          title: "Choose branch",
          instructions: "",
          ifElse: {
            condition: {
              left: { source: "runInput", path: "shouldRun" },
              operator: "equals",
              value: true,
            },
          },
        },
        { id: "yes", type: "human_review", title: "Yes", instructions: "Yes path" },
        { id: "no", type: "human_review", title: "No", instructions: "No path" },
      ],
      edges: [
        {
          id: "branch-yes",
          sourceNodeId: "branch",
          targetNodeId: "yes",
          condition: { sourceStatus: "completed", outputPredicate: { path: "branch", operator: "equals", value: "true" } },
        },
        {
          id: "branch-no",
          sourceNodeId: "branch",
          targetNodeId: "no",
          condition: { sourceStatus: "completed", outputPredicate: { path: "branch", operator: "equals", value: "false" } },
        },
      ],
    }), { input: { shouldRun: true } });

    const routed = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "yes" && nodeRun.status === "needs_review"),
    );

    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "branch")?.outputJson).toMatchObject({ branch: "true" });
    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "yes")?.status).toBe("needs_review");
    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "no")?.status).toBe("skipped");
  });

  it("routes ordered if/elif/else branches by first matching condition", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        {
          id: "branch",
          type: "if_else",
          title: "Choose branch",
          instructions: "",
          ifElse: {
            branches: [
              {
                id: "if-high",
                label: "High",
                kind: "if",
                condition: { logic: "and", conditions: [{ left: { source: "runInput", path: "priority" }, operator: "equals", value: "high" }] },
              },
              {
                id: "elif-medium",
                label: "Medium",
                kind: "elif",
                condition: { logic: "and", conditions: [{ left: { source: "runInput", path: "priority" }, operator: "equals", value: "medium" }] },
              },
              { id: "else-low", label: "Low", kind: "else" },
            ],
          },
        },
        { id: "high", type: "human_review", title: "High", instructions: "High path" },
        { id: "medium", type: "human_review", title: "Medium", instructions: "Medium path" },
        { id: "low", type: "human_review", title: "Low", instructions: "Low path" },
      ],
      edges: [
        { id: "branch-high", sourceNodeId: "branch", targetNodeId: "high", condition: { sourceStatus: "completed", outputPredicate: { path: "branch", operator: "equals", value: "if-high" } } },
        { id: "branch-medium", sourceNodeId: "branch", targetNodeId: "medium", condition: { sourceStatus: "completed", outputPredicate: { path: "branch", operator: "equals", value: "elif-medium" } } },
        { id: "branch-low", sourceNodeId: "branch", targetNodeId: "low", condition: { sourceStatus: "completed", outputPredicate: { path: "branch", operator: "equals", value: "else-low" } } },
      ],
    }), { input: { priority: "medium" } });

    const routed = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "medium" && nodeRun.status === "needs_review"),
    );

    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "branch")?.outputJson).toMatchObject({
      branch: "elif-medium",
      branchKind: "elif",
    });
    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "high")?.status).toBe("skipped");
    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "medium")?.status).toBe("needs_review");
    expect(routed.nodeRuns.find((nodeRun) => nodeRun.nodeId === "low")?.status).toBe("skipped");
  });

  it("routes CEL if/elif/else branches through branch docks after state updates", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        {
          id: "classify",
          type: "agent_task",
          title: "Classify",
          instructions: "Classify customer",
          outputSchema: {
            type: "object",
            required: ["tier"],
            properties: { tier: { type: "string" } },
          },
          stateUpdates: {
            "customer.tier": { source: "nodeOutput", nodeId: "classify", path: "tier" },
          },
        },
        {
          id: "branch",
          type: "if_else",
          title: "Choose tier",
          instructions: "",
          ifElse: {
            branches: [
              { id: "gold", label: "Gold", kind: "if", celExpression: 'state.customer.tier == "gold"' },
              { id: "standard", label: "Standard", kind: "else" },
            ],
          },
        },
        { id: "gold-review", type: "human_review", title: "Gold", instructions: "Gold path" },
        { id: "standard-review", type: "human_review", title: "Standard", instructions: "Standard path" },
      ],
      edges: [
        { id: "classify-branch", sourceNodeId: "classify", targetNodeId: "branch", condition: { sourceStatus: "completed" } },
        { id: "branch-gold", sourceNodeId: "branch", targetNodeId: "gold-review", condition: { sourceStatus: "completed", branchId: "gold" } },
        { id: "branch-standard", sourceNodeId: "branch", targetNodeId: "standard-review", condition: { sourceStatus: "completed", branchId: "standard" } },
      ],
    }));

    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(taskId, "reviewing", "```json\n{\"tier\":\"gold\"}\n```");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "gold-review" && nodeRun.status === "needs_review"),
    );

    expect(run.state).toMatchObject({ customer: { tier: "gold" } });
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "branch")?.outputJson).toMatchObject({
      branch: "gold",
      branchKind: "if",
    });
    expect(run.edgeRuns.find((edgeRun) => edgeRun.edgeId === "branch-gold")?.status).toBe("activated");
    expect(run.edgeRuns.find((edgeRun) => edgeRun.edgeId === "branch-standard")?.status).toBe("not_activated");
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "standard-review")?.status).toBe("skipped");
  });

  it.each(["codex", "claude-code"] as const)("passes %s workflow plan and model settings without rewriting the prompt", async (agentType) => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);
    await runner.startRun("/tmp/repo", definition({
      nodes: [{ id: "plan", type: "agent_task", title: "Plan", instructions: "Keep these instructions", agentType,
        planFirst: true, model: "selected-model", intelligence: "high" }], edges: [],
    }));
    const task = Array.from(orchestrator.tasks.values())[0];
    expect({ promptStart: task.prompt.split("\n")[0], instructions: task.prompt.includes("Instructions:\n\nKeep these instructions"),
      plan: task.planFirst, model: task.model, effort: task.reasoningLevel }).toEqual({
      promptStart: "Workflow: Workflow", instructions: true, plan: true, model: "selected-model", effort: "high",
    });
  });

  it("reconciles a missed planning event into needs_review", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition());
    await settle();
    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.setTaskStatusSilently(taskId, "planning", "Plan summary");

    const reconciled = await runner.getRun(run.id);
    const nodeRun = reconciled?.nodeRuns.find((candidate) => candidate.nodeId === "a");

    expect(nodeRun?.status).toBe("needs_review");
    expect(nodeRun?.outputSummary).toBe("Plan summary");
    expect(reconciled?.status).toBe("paused");
  });

  it("reconciles a missing linked task into a failed node", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition());
    await settle();
    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.tasks.delete(taskId);

    const reconciled = await runner.getRun(run.id);
    const nodeRun = reconciled?.nodeRuns.find((candidate) => candidate.nodeId === "a");

    expect(nodeRun?.status).toBe("failed");
    expect(nodeRun?.error).toBe("missing_task_after_restart");
    expect(reconciled?.status).toBe("failed");
  });

  it("reconciles a missed failed task status and activates failure branches", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "agent_task", title: "A", instructions: "Do A" },
        { id: "failure", type: "human_review", title: "Failure", instructions: "Failure path" },
      ],
      edges: [
        { id: "a-failure", sourceNodeId: "a", targetNodeId: "failure", condition: { sourceStatus: "failed" } },
      ],
    }));
    await settle();
    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.setTaskStatusSilently(taskId, "failed", "boom");

    const reconciled = await runner.getRun(run.id);

    expect(reconciled?.edgeRuns.find((edgeRun) => edgeRun.edgeId === "a-failure")?.status).toBe("activated");
    expect(reconciled?.nodeRuns.find((nodeRun) => nodeRun.nodeId === "failure")?.status).toBe("needs_review");
    expect(reconciled?.status).toBe("paused");
  });

  it("starts human review nodes directly in needs_review", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    const run = await runner.startRun("/tmp/repo", definition({
      nodes: [{ id: "review", type: "human_review", title: "Review", instructions: "Check output" }],
      edges: [],
    }));

    expect(orchestrator.tasks.size).toBe(0);
    expect((await store.get(run.id))?.nodeRuns[0].status).toBe("needs_review");
    expect((await store.get(run.id))?.status).toBe("paused");
  });

  it("activates a failure branch and skips the success branch", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "agent_task", title: "A", instructions: "Do A" },
        { id: "success", type: "human_review", title: "Success", instructions: "Success path" },
        { id: "failure", type: "human_review", title: "Failure", instructions: "Failure path" },
      ],
      edges: [
        { id: "a-success", sourceNodeId: "a", targetNodeId: "success", condition: { sourceStatus: "completed" } },
        { id: "a-failure", sourceNodeId: "a", targetNodeId: "failure", condition: { sourceStatus: "failed" } },
      ],
    }));

    const taskId = Array.from(orchestrator.tasks.keys())[0];
    orchestrator.finishTask(taskId, "failed", "boom");
    run = await waitForRun(store, run.id, (candidate) =>
      candidate.edgeRuns.some((edgeRun) => edgeRun.edgeId === "a-failure" && edgeRun.status === "activated")
      && candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "failure" && nodeRun.status === "needs_review"),
    );

    expect(run.edgeRuns.find((edgeRun) => edgeRun.edgeId === "a-failure")?.status).toBe("activated");
    expect(run.edgeRuns.find((edgeRun) => edgeRun.edgeId === "a-success")?.status).toBe("not_activated");
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "failure")?.status).toBe("needs_review");
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "success")?.status).toBe("skipped");
    expect(run.status).toBe("paused");
  });

  it("completes join nodes after activated upstream branches complete", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);

    let run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "a", type: "human_review", title: "A", instructions: "Review A" },
        { id: "b", type: "human_review", title: "B", instructions: "Review B" },
        { id: "join", type: "join", title: "Join", instructions: "" },
      ],
      edges: [
        { id: "a-join", sourceNodeId: "a", targetNodeId: "join" },
        { id: "b-join", sourceNodeId: "b", targetNodeId: "join" },
      ],
    }));

    const a = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "a")!;
    const b = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "b")!;
    run = await runner.approveNode(run.id, a.id, "A ok");
    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "join")?.status).toBe("blocked");

    run = await runner.approveNode(run.id, b.id, "B ok");
    await settle();
    run = (await store.get(run.id))!;

    expect(run.nodeRuns.find((nodeRun) => nodeRun.nodeId === "join")?.status).toBe("completed");
    expect(run.status).toBe("completed");
  });

  it("rejects invalid structured human review output before unblocking downstream work", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const runner = new AgentWorkflowRunner(orchestrator as unknown as Orchestrator, store as unknown as AgentWorkflowRunStore);
    const run = await runner.startRun("/tmp/repo", definition({
      nodes: [
        { id: "review", type: "human_review", title: "Review", instructions: "Return JSON",
          outputSchema: { type: "object", required: ["result"], properties: { result: { type: "string" } } },
          requireStructuredOutput: true },
        { id: "downstream", type: "agent_task", title: "Use result", instructions: "Use result" },
      ],
      edges: [{ id: "next", sourceNodeId: "review", targetNodeId: "downstream", condition: { sourceStatus: "completed" } }],
    }));
    const review = run.nodeRuns.find(node => node.nodeId === "review")!;
    await runner.approveNode(run.id, review.id, "not valid JSON");
    expect(orchestrator.tasks.size).toBe(0);
  });

  it("maps sub-workflow output to the parent node and downstream prompt", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const definitions = new MemoryDefinitionStore();
    const child = definition({
      id: "child-wf",
      name: "Child Workflow",
      nodes: [{
        id: "child",
        type: "human_review",
        title: "Child Output",
        instructions: "Return child output",
        outputSchema: {
          type: "object",
          required: ["result"],
          properties: { result: { type: "string" } },
        },
        requireStructuredOutput: true,
      }],
      edges: [],
    });
    definitions.definitions.set(child.id, child);

    const runner = new AgentWorkflowRunner(
      orchestrator as unknown as Orchestrator,
      store as unknown as AgentWorkflowRunStore,
      definitions as unknown as AgentWorkflowDefinitionStore,
    );

    const parent = definition({
      id: "parent-wf",
      name: "Parent Workflow",
      nodes: [
        {
          id: "sub",
          type: "sub_workflow",
          title: "Run Child",
          instructions: "Run child",
          subWorkflowDefinitionId: child.id,
          outputMapping: {
            result: { source: "nodeOutput", nodeId: "child", path: "result" },
          },
          outputSchema: {
            type: "object",
            required: ["result"],
            properties: { result: { type: "string" } },
          },
          requireStructuredOutput: true,
        },
        {
          id: "downstream",
          type: "agent_task",
          title: "Use Child",
          instructions: "Value {{node.output.sub.result}}",
        },
      ],
      edges: [{ id: "sub-downstream", sourceNodeId: "sub", targetNodeId: "downstream", condition: { sourceStatus: "completed" } }],
    });

    let parentRun = await runner.startRun("/tmp/repo", parent);
    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      Boolean(candidate.nodeRuns.find((nodeRun) => nodeRun.nodeId === "sub")?.childRunId),
    );
    const childRunId = parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "sub")?.childRunId;
    expect(childRunId).toBeTruthy();

    const childRun = (await store.get(childRunId!))!;
    const childNodeRun = childRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "child")!;
    await runner.approveNode(childRun.id, childNodeRun.id, "```json\n{\"result\":\"ok\"}\n```");

    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "sub" && nodeRun.status === "completed")
      && candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "downstream" && nodeRun.status === "running"),
    );

    expect(parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "sub")?.outputJson).toEqual({ result: "ok" });
    const downstreamTask = Array.from(orchestrator.tasks.values()).find((task) => task.prompt.includes("Value ok"));
    expect(downstreamTask).toBeTruthy();
  });

  it("runs a loop node as bounded repeated child workflow executions", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const definitions = new MemoryDefinitionStore();
    const child = definition({
      id: "loop-child",
      name: "Loop Child",
      nodes: [{
        id: "child",
        type: "human_review",
        title: "Child Iteration",
        instructions: "Return child output",
      }],
      edges: [],
    });
    definitions.definitions.set(child.id, child);

    const runner = new AgentWorkflowRunner(
      orchestrator as unknown as Orchestrator,
      store as unknown as AgentWorkflowRunStore,
      definitions as unknown as AgentWorkflowDefinitionStore,
    );

    let parentRun = await runner.startRun("/tmp/repo", definition({
      id: "loop-parent",
      nodes: [
        {
          id: "loop",
          type: "loop",
          title: "Repeat Child",
          instructions: "",
          subWorkflowDefinitionId: child.id,
          loop: { maxIterations: 2 },
        },
      ],
      edges: [],
    }));

    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      Boolean(candidate.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.childRunId),
    );
    const firstChildRunId = parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.childRunId;
    const firstChildRun = (await store.get(firstChildRunId!))!;
    await runner.approveNode(firstChildRun.id, firstChildRun.nodeRuns[0].id, "first");

    parentRun = await waitForRun(store, parentRun.id, (candidate) => {
      const nodeRun = candidate.nodeRuns.find((item) => item.nodeId === "loop");
      return Boolean(nodeRun?.childRunId && nodeRun.childRunId !== firstChildRunId);
    });
    const secondChildRunId = parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.childRunId;
    const secondChildRun = (await store.get(secondChildRunId!))!;
    await runner.approveNode(secondChildRun.id, secondChildRun.nodeRuns[0].id, "second");

    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "loop" && nodeRun.status === "completed"),
    );

    expect(parentRun.status).toBe("completed");
    expect(parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.outputJson).toMatchObject({
      count: 2,
    });
  });

  it("carries loop state and terminates when the termination condition matches", async () => {
    const orchestrator = new FakeOrchestrator();
    const store = new MemoryRunStore();
    const definitions = new MemoryDefinitionStore();
    const child = definition({
      id: "loop-child-state",
      name: "Loop Child",
      nodes: [{
        id: "child",
        type: "human_review",
        title: "Child Iteration",
        instructions: "Return child output",
      }],
      edges: [],
    });
    definitions.definitions.set(child.id, child);

    const runner = new AgentWorkflowRunner(
      orchestrator as unknown as Orchestrator,
      store as unknown as AgentWorkflowRunStore,
      definitions as unknown as AgentWorkflowDefinitionStore,
    );

    let parentRun = await runner.startRun("/tmp/repo", definition({
      id: "loop-parent-state",
      nodes: [
        {
          id: "loop",
          type: "loop",
          title: "Repeat Child",
          instructions: "",
          subWorkflowDefinitionId: child.id,
          loop: {
            maxIterations: 5,
            initialState: { seed: { source: "literal", value: "draft" } },
            stateMapping: { summary: { source: "nodeSummary", nodeId: "child" } },
            terminateCondition: {
              logic: "and",
              conditions: [{ left: { source: "nodeOutput", nodeId: "loop", path: "state.summary" }, operator: "equals", value: "stop" }],
            },
          },
        },
      ],
      edges: [],
    }));

    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      Boolean(candidate.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.childRunId),
    );
    const firstChildRunId = parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.childRunId;
    const firstChildRun = (await store.get(firstChildRunId!))!;
    expect(firstChildRun.input?.loop).toMatchObject({ iteration: 1, state: { seed: "draft" } });

    await runner.approveNode(firstChildRun.id, firstChildRun.nodeRuns[0].id, "stop");

    parentRun = await waitForRun(store, parentRun.id, (candidate) =>
      candidate.nodeRuns.some((nodeRun) => nodeRun.nodeId === "loop" && nodeRun.status === "completed"),
    );

    expect(parentRun.nodeRuns.find((nodeRun) => nodeRun.nodeId === "loop")?.outputJson).toMatchObject({
      count: 1,
      state: { seed: "draft", summary: "stop" },
    });
  });
});
