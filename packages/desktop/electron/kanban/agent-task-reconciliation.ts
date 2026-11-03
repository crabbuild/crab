import type { Task, TaskStatus } from "../orchestrator";
import type { BoardConfig, KanbanCard, PipelineState } from "./kanban-types";
import {
  computeCompletionUpdate,
  findColumnByName,
  type CompletionUpdate,
  type TaskResult,
} from "./task-completion";

export interface AgentTaskLookup {
  getTask(taskId: string): Task | undefined;
}

export interface AgentCardFinalization {
  cardId: string;
  taskId: string;
  pipelineState: PipelineState;
  detail: string;
}

export interface AgentCardReconciliationResult {
  cards: KanbanCard[];
  changed: boolean;
  finalized: AgentCardFinalization[];
}

const AGENT_SUCCESS_STATUSES = new Set<TaskStatus>(["reviewing", "planning", "applied"]);
const AGENT_FAILURE_STATUSES = new Set<TaskStatus>(["failed", "stopped", "interrupted", "discarded"]);

/**
 * Reconciles kanban cards that are linked to orchestrator tasks.
 *
 * The orchestrator's authoritative completion signal is task status:
 * local runs move to "reviewing" or "failed" after the agent process
 * exits, while remote runs also report completion through task status.
 */
export function reconcileCompletedAgentCards(
  cards: KanbanCard[],
  board: BoardConfig,
  taskLookup: AgentTaskLookup | null | undefined,
  options: { markMissingTasksBlocked?: boolean; now?: () => number } = {},
): AgentCardReconciliationResult {
  if (!taskLookup) {
    return { cards, changed: false, finalized: [] };
  }

  const nextCards = cards.map((card) => card);
  const finalized: AgentCardFinalization[] = [];
  const now = options.now ?? Date.now;

  for (let idx = 0; idx < nextCards.length; idx += 1) {
    const card = nextCards[idx];
    if (card.pipelineState !== "agent_running" || !card.linkedTaskId) continue;

    const task = taskLookup.getTask(card.linkedTaskId);
    const result = task
      ? taskResultFromCompletedTask(task)
      : options.markMissingTasksBlocked
        ? missingTaskResult(card.linkedTaskId)
        : null;
    if (!result) continue;

    const targetColumnName = result.exitCode === 0 ? "Done" : "Blocked";
    const targetColumn = findColumnByName(board.columns, targetColumnName);
    const cardsInTargetColumn = targetColumn
      ? nextCards.filter((candidate) => candidate.id !== card.id && candidate.columnId === targetColumn.id).length
      : 0;

    const update = computeCompletionUpdate(result, card, board.columns, cardsInTargetColumn);
    const updatedCard = applyCompletionUpdate(card, update, now());
    nextCards[idx] = updatedCard;
    finalized.push({
      cardId: card.id,
      taskId: card.linkedTaskId,
      pipelineState: update.pipelineState,
      detail: completionDetail(update.pipelineState, task),
    });
  }

  return {
    cards: nextCards,
    changed: finalized.length > 0,
    finalized,
  };
}

export function completeAgentCardWithResult(
  cards: KanbanCard[],
  board: BoardConfig,
  cardId: string,
  taskId: string,
  result: TaskResult,
  detail: string,
  options: { now?: () => number } = {},
): AgentCardReconciliationResult {
  const idx = cards.findIndex((card) => card.id === cardId);
  if (idx < 0) {
    return { cards, changed: false, finalized: [] };
  }

  const nextCards = cards.map((card) => card);
  const card = nextCards[idx];
  const targetColumnName = result.exitCode === 0 ? "Done" : "Blocked";
  const targetColumn = findColumnByName(board.columns, targetColumnName);
  const cardsInTargetColumn = targetColumn
    ? nextCards.filter((candidate) => candidate.id !== card.id && candidate.columnId === targetColumn.id).length
    : 0;
  const update = computeCompletionUpdate(result, card, board.columns, cardsInTargetColumn);
  nextCards[idx] = applyCompletionUpdate(card, update, (options.now ?? Date.now)());

  return {
    cards: nextCards,
    changed: true,
    finalized: [{
      cardId,
      taskId,
      pipelineState: update.pipelineState,
      detail,
    }],
  };
}

export function taskResultFromCompletedTask(task: Task): TaskResult | null {
  if (typeof task.exitCode === "number" && task.completedAt) {
    return {
      exitCode: task.exitCode,
      prUrl: task.prUrl,
      prNumber: task.prNumber,
      summary: task.exitCode === 0 ? latestDoneSummary(task) : latestErrorSummary(task),
    };
  }

  const latestTurnResult = taskResultFromLatestDoneEvent(task);
  if (latestTurnResult) {
    return {
      ...latestTurnResult,
      prUrl: task.prUrl,
      prNumber: task.prNumber,
    };
  }

  if (AGENT_SUCCESS_STATUSES.has(task.status)) {
    return {
      exitCode: 0,
      prUrl: task.prUrl,
      prNumber: task.prNumber,
      summary: latestDoneSummary(task),
    };
  }

  if (AGENT_FAILURE_STATUSES.has(task.status)) {
    return {
      exitCode: typeof task.exitCode === "number" ? task.exitCode : 1,
      prUrl: task.prUrl,
      prNumber: task.prNumber,
      summary: latestErrorSummary(task),
    };
  }

  return null;
}

export function isCompletedAgentTaskStatus(status: TaskStatus): boolean {
  return AGENT_SUCCESS_STATUSES.has(status) || AGENT_FAILURE_STATUSES.has(status);
}

export function applyCompletionUpdate(
  card: KanbanCard,
  update: CompletionUpdate,
  updatedAt: number,
): KanbanCard {
  const updatedCard: KanbanCard = {
    ...card,
    pipelineState: update.pipelineState,
    updatedAt,
  };

  if (update.linkedPrUrl) {
    updatedCard.linkedPrUrl = update.linkedPrUrl;
  }
  if (update.linkedPrNumber !== undefined) {
    updatedCard.linkedPrNumber = update.linkedPrNumber;
  }
  if (update.targetColumnId) {
    updatedCard.columnId = update.targetColumnId;
    updatedCard.positionIndex = update.positionIndex ?? 0;
  }
  if (update.descriptionAppend) {
    updatedCard.description = (updatedCard.description ?? "") + update.descriptionAppend;
  }

  return updatedCard;
}

function missingTaskResult(taskId: string): TaskResult {
  return {
    exitCode: 1,
    summary: `Linked agent task ${taskId} is no longer available.`,
  };
}

function completionDetail(pipelineState: PipelineState, task: Task | undefined): string {
  if (!task) return "Linked agent task is no longer available";
  if (pipelineState === "done") return "Agent task completed successfully";
  return task.status === "discarded"
    ? "Agent task was discarded"
    : "Agent task failed or stopped";
}

function latestDoneSummary(task: Task): string | undefined {
  for (let idx = task.events.length - 1; idx >= 0; idx -= 1) {
    const event = task.events[idx];
    if (event.type === "done" && typeof event.content === "string") {
      const content = event.content.trim();
      if (content) return content;
    }
  }
  return undefined;
}

function latestErrorSummary(task: Task): string | undefined {
  for (let idx = task.events.length - 1; idx >= 0; idx -= 1) {
    const event = task.events[idx];
    if (event.type === "error" && typeof event.error === "string") {
      const error = event.error.trim();
      if (error) return error;
    }
  }
  return undefined;
}

function taskResultFromLatestDoneEvent(task: Task): TaskResult | null {
  for (let idx = task.events.length - 1; idx >= 0; idx -= 1) {
    const event = task.events[idx];
    if (event.type === "usage") continue;
    if (event.type !== "done") return null;

    const previous = previousNonUsageEvent(task, idx - 1);
    if (previous?.type === "error") {
      const error = typeof previous.error === "string" ? previous.error.trim() : "";
      return { exitCode: 1, summary: error || undefined };
    }

    const content = typeof event.content === "string" ? event.content.trim() : "";
    return { exitCode: 0, summary: content || undefined };
  }

  return null;
}

function previousNonUsageEvent(task: Task, startIndex: number): Task["events"][number] | null {
  for (let idx = startIndex; idx >= 0; idx -= 1) {
    const event = task.events[idx];
    if (event.type !== "usage") return event;
  }
  return null;
}
