import { EventEmitter } from "node:events";
import * as path from "node:path";

import type { AgentEvent, AgentType } from "../../orchestrator-adapters";
import type {
  SymphonyIssue,
  SymphonyOrchestratorLike,
  SymphonyTokenTotals,
} from "./types";
import { EMPTY_TOKEN_TOTALS, extractAbsoluteTokenTotals } from "./token-accounting";

export interface SymphonyAgentRunRequest {
  issue: SymphonyIssue;
  prompt: string;
  workspacePath: string;
  agentType?: AgentType;
  cardId?: string;
}

export interface SymphonyAgentRunStarted {
  issueId: string;
  identifier: string;
  taskId: string;
  workspacePath: string;
}

export interface SymphonyAgentRunUpdate {
  issueId: string;
  identifier: string;
  taskId: string;
  event: string;
  timestamp: number;
  message: string | null;
  sessionId: string | null;
  tokenTotals: SymphonyTokenTotals | null;
  raw: unknown;
}

export interface SymphonyAgentRunCompleted {
  issueId: string;
  identifier: string;
  taskId: string;
  exitCode: number;
  error?: string;
}

export class SymphonyAgentRunner extends EventEmitter {
  private taskEventListeners = new Map<string, (payload: { taskId: string; event: AgentEvent }) => void>();

  constructor(private readonly orchestrator: SymphonyOrchestratorLike) {
    super();
  }

  async start(request: SymphonyAgentRunRequest): Promise<SymphonyAgentRunStarted> {
    const task = await this.orchestrator.createTask(
      request.prompt,
      request.agentType ?? "codex",
      request.workspacePath,
      undefined,
    );

    task.kanbanCardId = request.cardId;
    task.symphonyIssueId = request.issue.id;

    const listener = (payload: { taskId: string; event: AgentEvent }) => {
      if (payload.taskId !== task.id) return;
      this.handleTaskEvent(request.issue, task.id, payload.event);
    };
    this.taskEventListeners.set(task.id, listener);
    this.orchestrator.on("orchestrator:event", listener);

    await this.orchestrator.startTask(task.id);

    return {
      issueId: request.issue.id,
      identifier: request.issue.identifier,
      taskId: task.id,
      workspacePath: request.workspacePath,
    };
  }

  async cancel(taskId: string): Promise<void> {
    this.removeListenerForTask(taskId);
    if (this.orchestrator.cancelTask) {
      await this.orchestrator.cancelTask(taskId);
    }
  }

  dispose(): void {
    for (const taskId of [...this.taskEventListeners.keys()]) {
      this.removeListenerForTask(taskId);
    }
  }

  private handleTaskEvent(issue: SymphonyIssue, taskId: string, event: AgentEvent): void {
    const eventType = String((event as { type?: unknown }).type ?? "event");
    const timestamp = Date.now();
    const tokenTotals = extractAbsoluteTokenTotals(event);
    const update: SymphonyAgentRunUpdate = {
      issueId: issue.id,
      identifier: issue.identifier,
      taskId,
      event: eventType,
      timestamp,
      message: summarizeEvent(event),
      sessionId: sessionIdFromEvent(event),
      tokenTotals,
      raw: event,
    };
    this.emit("update", update);

    if (isTerminalEvent(eventType)) {
      this.removeListenerForTask(taskId);
      const exitCode = eventType === "error" ? 1 : Number((event as { exitCode?: unknown }).exitCode ?? 0);
      const error = eventType === "error"
        ? String((event as { error?: unknown }).error ?? "Agent task failed.")
        : undefined;
      this.emit("completed", {
        issueId: issue.id,
        identifier: issue.identifier,
        taskId,
        exitCode,
        error,
      } satisfies SymphonyAgentRunCompleted);
    }
  }

  private removeListenerForTask(taskId: string): void {
    const listener = this.taskEventListeners.get(taskId);
    if (!listener) return;
    this.orchestrator.removeListener("orchestrator:event", listener);
    this.taskEventListeners.delete(taskId);
  }
}

export function workspaceContainsTaskPath(workspacePath: string, taskPath: string): boolean {
  const relative = path.relative(path.resolve(workspacePath), path.resolve(taskPath));
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

export function initialRunTokenTotals(): SymphonyTokenTotals {
  return { ...EMPTY_TOKEN_TOTALS };
}

function isTerminalEvent(eventType: string): boolean {
  return eventType === "exit" || eventType === "complete" || eventType === "error";
}

function summarizeEvent(event: AgentEvent): string | null {
  const obj = event as unknown as Record<string, unknown>;
  const direct =
    obj.message ??
    obj.data ??
    obj.content ??
    obj.summary ??
    obj.error;
  if (direct != null) {
    return String(direct).replace(/\s+/g, " ").trim().slice(0, 500) || null;
  }
  if (obj.path) {
    return String(obj.path);
  }
  if (obj.command) {
    return String(obj.command);
  }
  return null;
}

function sessionIdFromEvent(event: AgentEvent): string | null {
  const obj = event as unknown as Record<string, unknown>;
  const threadId = stringField(obj, "thread_id") ?? stringField(obj, "threadId");
  const turnId = stringField(obj, "turn_id") ?? stringField(obj, "turnId");
  if (threadId && turnId) return `${threadId}-${turnId}`;
  return stringField(obj, "session_id") ?? stringField(obj, "sessionId");
}

function stringField(obj: Record<string, unknown>, field: string): string | null {
  const value = obj[field];
  return typeof value === "string" && value ? value : null;
}
