import { describe, expect, it } from "vitest";

import type { Task } from "../electron/orchestrator";
import type { BoardConfig, KanbanCard, KanbanColumn } from "../electron/kanban/kanban-types";
import {
  completeAgentCardWithResult,
  reconcileCompletedAgentCards,
  taskResultFromCompletedTask,
} from "../electron/kanban/agent-task-reconciliation";

const columns: KanbanColumn[] = [
  { id: "ready", name: "Ready", trackerStatusMapping: ["ready"] },
  { id: "done", name: "Done", trackerStatusMapping: ["done"] },
  { id: "blocked", name: "Blocked", trackerStatusMapping: ["blocked"] },
];

function makeBoard(): BoardConfig {
  return {
    id: "board-1",
    name: "Board",
    repoPath: "/repo",
    columns,
    trackerType: "local",
    trackerConfig: { owner: "", repo: "" },
    autonomyLevel: "moderate",
    pollIntervalMs: 60_000,
    ciPollIntervalMs: 30_000,
    maxIterations: 3,
    filterCriteria: {},
  };
}

function makeCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-1",
    boardId: "board-1",
    source: "local",
    externalId: "",
    externalUrl: "",
    title: "Say hi",
    description: "Original description",
    columnId: "ready",
    positionIndex: 0,
    priority: "none",
    labels: [],
    linkedTaskId: "task-1",
    pipelineState: "agent_running",
    iterationCount: 0,
    createdAt: 1000,
    updatedAt: 1000,
    ...overrides,
  };
}

function makeTask(overrides: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    prompt: "Say hi",
    agentType: "codex",
    status: "running",
    branch: "agent/task-1",
    worktreePath: "/tmp/task-1",
    baseRef: "HEAD",
    repoPath: "/repo",
    createdAt: 1000,
    events: [],
    feedback: [],
    todos: [],
    ...overrides,
  };
}

describe("kanban agent task reconciliation", () => {
  it("moves stale running cards to Done when the linked task is reviewing", () => {
    const board = makeBoard();
    const existingDoneCard = makeCard({
      id: "done-card",
      linkedTaskId: undefined,
      pipelineState: "done",
      columnId: "done",
      positionIndex: 0,
    });
    const runningCard = makeCard();
    const task = makeTask({
      status: "reviewing",
      exitCode: 0,
      completedAt: 2000,
      events: [{ type: "done", timestamp: 2000, content: "Hi, good morning." }],
    });

    const result = reconcileCompletedAgentCards(
      [existingDoneCard, runningCard],
      board,
      { getTask: () => task },
      { now: () => 3000 },
    );

    expect(result.changed).toBe(true);
    expect(result.finalized).toEqual([
      {
        cardId: "card-1",
        taskId: "task-1",
        pipelineState: "done",
        detail: "Agent task completed successfully",
      },
    ]);
    const updated = result.cards.find((card) => card.id === "card-1");
    expect(updated?.pipelineState).toBe("done");
    expect(updated?.columnId).toBe("done");
    expect(updated?.positionIndex).toBe(1);
    expect(updated?.updatedAt).toBe(3000);
    expect(updated?.description).toContain("Hi, good morning.");
  });

  it("moves failed linked tasks to Blocked and preserves the error summary", () => {
    const board = makeBoard();
    const card = makeCard();
    const task = makeTask({
      status: "failed",
      exitCode: 1,
      completedAt: 2000,
      events: [{ type: "error", timestamp: 2000, error: "Agent binary failed." }],
    });

    const result = reconcileCompletedAgentCards(
      [card],
      board,
      { getTask: () => task },
      { now: () => 3000 },
    );

    const updated = result.cards[0];
    expect(updated.pipelineState).toBe("blocked");
    expect(updated.columnId).toBe("blocked");
    expect(updated.description).toContain("Agent binary failed.");
  });

  it("leaves cards alone while the linked task is still running", () => {
    const board = makeBoard();
    const card = makeCard();
    const task = makeTask({ status: "running" });

    const result = reconcileCompletedAgentCards([card], board, { getTask: () => task });

    expect(result.changed).toBe(false);
    expect(result.cards[0]).toBe(card);
  });

  it("can clear a card when the linked task no longer exists", () => {
    const board = makeBoard();
    const card = makeCard();

    const result = reconcileCompletedAgentCards(
      [card],
      board,
      { getTask: () => undefined },
      { markMissingTasksBlocked: true, now: () => 3000 },
    );

    expect(result.changed).toBe(true);
    expect(result.cards[0].pipelineState).toBe("blocked");
    expect(result.cards[0].description).toContain("Linked agent task task-1 is no longer available.");
  });

  it("finalizes a card from an explicit task result when start fails", () => {
    const board = makeBoard();
    const card = makeCard();

    const result = completeAgentCardWithResult(
      [card],
      board,
      "card-1",
      "task-1",
      { exitCode: 1, summary: "Agent task failed to start." },
      "Agent task failed to start",
      { now: () => 3000 },
    );

    expect(result.finalized[0]).toMatchObject({
      cardId: "card-1",
      taskId: "task-1",
      pipelineState: "blocked",
      detail: "Agent task failed to start",
    });
    expect(result.cards[0].pipelineState).toBe("blocked");
    expect(result.cards[0].updatedAt).toBe(3000);
  });

  it("derives task results from completed task status even without exitCode", () => {
    const task = makeTask({ status: "reviewing" });

    expect(taskResultFromCompletedTask(task)).toEqual({
      exitCode: 0,
      prNumber: undefined,
      prUrl: undefined,
      summary: undefined,
    });
  });

  it("derives completion from the latest done event for long-lived Codex sessions", () => {
    const task = makeTask({
      status: "running",
      events: [
        { type: "text", timestamp: 1500, content: "Hi, good morning." },
        { type: "done", timestamp: 2000, content: "Hi, good morning." },
      ],
    });

    expect(taskResultFromCompletedTask(task)).toEqual({
      exitCode: 0,
      prNumber: undefined,
      prUrl: undefined,
      summary: "Hi, good morning.",
    });
  });

  it("treats an error followed by done as a failed long-lived Codex turn", () => {
    const task = makeTask({
      status: "running",
      events: [
        { type: "error", timestamp: 1500, error: "Turn failed." },
        { type: "done", timestamp: 2000, content: "" },
      ],
    });

    expect(taskResultFromCompletedTask(task)).toEqual({
      exitCode: 1,
      prNumber: undefined,
      prUrl: undefined,
      summary: "Turn failed.",
    });
  });
});
