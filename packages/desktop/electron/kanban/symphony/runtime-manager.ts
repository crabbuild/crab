import type { BrowserWindow } from "electron";

import { BoardConfigStore } from "../kanban-persistence";
import type {
  SymphonyIssueSnapshot,
  SymphonyOrchestratorLike,
  SymphonyRuntimeEvent,
  SymphonyRuntimeSnapshot,
} from "./types";
import { SymphonyScheduler } from "./scheduler";

export interface SymphonyIpcResult<T> {
  success: boolean;
  data?: T;
  error?: string;
}

export class KanbanSymphonyRuntimeManager {
  private schedulers = new Map<string, SymphonyScheduler>();

  constructor(
    private readonly boardConfigStore: BoardConfigStore,
    private readonly getMainWindow: () => BrowserWindow | null,
    private readonly getOrchestrator: () => SymphonyOrchestratorLike | null,
  ) {}

  async start(boardId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const schedulerResult = await this.getOrCreateScheduler(boardId);
    if (!schedulerResult.success || !schedulerResult.data) {
      return { success: false, error: schedulerResult.error ?? "Unable to create Symphony scheduler." };
    }
    const snapshot = await schedulerResult.data.start();
    return { success: true, data: snapshot };
  }

  async stop(boardId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const scheduler = this.schedulers.get(boardId);
    if (!scheduler) return { success: true, data: emptyStoppedSnapshot(boardId) };
    const snapshot = scheduler.stop();
    return { success: true, data: snapshot };
  }

  async refresh(boardId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const schedulerResult = await this.getOrCreateScheduler(boardId);
    if (!schedulerResult.success || !schedulerResult.data) {
      return { success: false, error: schedulerResult.error ?? "Unable to create Symphony scheduler." };
    }
    return { success: true, data: await schedulerResult.data.refresh() };
  }

  async getState(boardId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const scheduler = this.schedulers.get(boardId);
    if (scheduler) return { success: true, data: scheduler.snapshot() };
    return { success: true, data: emptyStoppedSnapshot(boardId) };
  }

  async getIssue(boardId: string, issueId: string): Promise<SymphonyIpcResult<SymphonyIssueSnapshot | null>> {
    const scheduler = this.schedulers.get(boardId);
    return { success: true, data: scheduler?.getIssue(issueId) ?? null };
  }

  async retryIssue(boardId: string, issueId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const schedulerResult = await this.getOrCreateScheduler(boardId);
    if (!schedulerResult.success || !schedulerResult.data) {
      return { success: false, error: schedulerResult.error ?? "Unable to create Symphony scheduler." };
    }
    return { success: true, data: await schedulerResult.data.retryIssue(issueId) };
  }

  async cancelIssue(boardId: string, issueId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const scheduler = this.schedulers.get(boardId);
    if (!scheduler) return { success: false, error: "Symphony runtime is not started for this board." };
    return { success: true, data: await scheduler.cancelIssue(issueId) };
  }

  async releaseBlockedIssue(boardId: string, issueId: string): Promise<SymphonyIpcResult<SymphonyRuntimeSnapshot>> {
    const scheduler = this.schedulers.get(boardId);
    if (!scheduler) return { success: false, error: "Symphony runtime is not started for this board." };
    return { success: true, data: await scheduler.releaseBlockedIssue(issueId) };
  }

  async autostartEnabledBoards(): Promise<void> {
    const data = await this.boardConfigStore.load();
    await Promise.all(
      data.boards
        .filter((board) => board.symphony?.enabled && board.symphony?.autostart)
        .map((board) => this.start(board.id)),
    );
  }

  private async getOrCreateScheduler(boardId: string): Promise<SymphonyIpcResult<SymphonyScheduler>> {
    const existing = this.schedulers.get(boardId);
    if (existing) {
      const board = await this.boardConfigStore.getBoardById(boardId);
      if (board) existing.updateBoard(board);
      return { success: true, data: existing };
    }

    const board = await this.boardConfigStore.getBoardById(boardId);
    if (!board) return { success: false, error: "Board not found." };
    if (!board.symphony?.enabled) {
      return { success: false, error: "Symphony mode is not enabled for this board." };
    }

    const orchestrator = this.getOrchestrator();
    if (!orchestrator) return { success: false, error: "Orchestrator is not available." };

    const scheduler = new SymphonyScheduler({ board, orchestrator });
    scheduler.on("state", (snapshot: SymphonyRuntimeSnapshot) => {
      this.send("kanban:symphony-state", snapshot);
    });
    scheduler.on("event", (event: SymphonyRuntimeEvent) => {
      this.send("kanban:symphony-event", event);
      if (event.issueId) {
        this.send("kanban:symphony-issue", {
          boardId,
          issue: scheduler.getIssue(event.issueId),
        });
      }
      if (event.type === "config_error") {
        this.send("kanban:symphony-config-error", {
          boardId: event.boardId,
          message: event.message,
          timestamp: event.timestamp,
          workflowPath: scheduler.snapshot().workflowPath,
        });
      }
    });
    scheduler.on("board-updated", (projection) => {
      this.send("kanban:board-updated", projection);
    });
    this.schedulers.set(boardId, scheduler);
    return { success: true, data: scheduler };
  }

  private send(channel: string, payload: unknown): void {
    const win = this.getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send(channel, payload);
    }
  }
}

function emptyStoppedSnapshot(boardId: string): SymphonyRuntimeSnapshot {
  return {
    boardId,
    status: "stopped",
    started: false,
    workflowPath: null,
    lastReloadAtMs: null,
    lastPollAtMs: null,
    nextPollAtMs: null,
    configError: null,
    counts: { running: 0, retrying: 0, blocked: 0, completed: 0, claimed: 0 },
    running: [],
    retrying: [],
    blocked: [],
    codexTotals: { inputTokens: 0, outputTokens: 0, totalTokens: 0 },
    rateLimits: null,
    recentEvents: [],
  };
}
