/**
 * IPC Bridge for the Kanban Orchestration subsystem.
 *
 * Registers IPC handlers for all kanban commands (renderer → main)
 * and provides helpers to relay kanban events to the renderer (main → renderer).
 *
 * Wired to real persistence stores (BoardConfigStore, RuleStore,
 * AnalyticsStore, BoardStateStore) for end-to-end board management.
 */

import { randomUUID } from "node:crypto";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import type { BrowserWindow, IpcMain } from "electron";
import log from "electron-log";

import type {
  BoardConfig,
  KanbanCard,
  KanbanColumn,
  KanbanRunDefaults,
  KanbanTaskRun,
  AutomationRule,
  BoardAnalytics,
  CardAnalytics,
  CardRelation,
  ColumnUIState,
  DensityMode,
  RelationType,
  SavedView,
  TrackerSource,
} from "./kanban-types";
import {
  BoardConfigStore,
  RuleStore,
  AnalyticsStore,
  BoardStateStore,
} from "./kanban-persistence";
import { buildAgentContext } from "./agent-context";
import type { TaskResult } from "./task-completion";
import { TaskListenerRegistry } from "./task-listener-registry";
import { getOrchestrator } from "../main";
import { ChatDatabase } from "../orchestrator-chat-db";
import { KanbanSymphonyRuntimeManager } from "./symphony/runtime-manager";
import type { TaskStatus } from "../orchestrator";
import {
  completeAgentCardWithResult,
  isCompletedAgentTaskStatus,
  reconcileCompletedAgentCards,
  taskResultFromCompletedTask,
} from "./agent-task-reconciliation";

const execFile = promisify(execFileCb);

/**
 * Board state returned to the renderer for a single board.
 */
export interface BoardState {
  config: BoardConfig;
  cards: KanbanCard[];
  columns: KanbanColumn[];
}

/**
 * Registers all kanban IPC handlers on the given `ipcMain` instance.
 *
 * Handlers are wired to real persistence stores for board configuration,
 * rules, analytics, and card state management.
 *
 * Also returns a `sendToRenderer` helper that other kanban modules can use
 * to push events to the renderer via webContents.send().
 */
export function registerKanbanIpc(
  ipcMain: IpcMain,
  getMainWindow: () => BrowserWindow | null,
) {
  // ─── Persistence Stores (singleton instances) ───────────────────────────

  const boardConfigStore = new BoardConfigStore();
  const ruleStore = new RuleStore();
  const analyticsStore = new AnalyticsStore();
  const boardStateStore = new BoardStateStore();
  const symphonyRuntimeManager = new KanbanSymphonyRuntimeManager(
    boardConfigStore,
    getMainWindow,
    getOrchestrator,
  );

  // ─── Task Listener Registry (one listener per active task) ──────────────

  const registry = new TaskListenerRegistry();

  // ─── Helper: send event to renderer ─────────────────────────────────────

  function sendToRenderer(channel: string, payload: unknown) {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send(channel, payload);
    }
  }

  async function safeSymphonyCall<T>(
    action: () => Promise<{ success: boolean; data?: T; error?: string }>,
  ): Promise<{ success: boolean; data?: T; error?: string }> {
    try {
      return await action();
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);
      log.error(`[kanban] Symphony IPC error: ${error}`);
      return { success: false, error };
    }
  }

  async function reconcileAgentCardsForBoard(
    board: BoardConfig,
    options: { notify?: boolean } = {},
  ): Promise<KanbanCard[]> {
    const allCards = await boardStateStore.getCards();
    const boardCards = allCards.filter((card) => card.boardId === board.id);
    const orchestrator = getOrchestrator();
    const reconciled = reconcileCompletedAgentCards(boardCards, board, orchestrator, {
      markMissingTasksBlocked: true,
    });

    if (!reconciled.changed) return boardCards;

    const reconciledById = new Map(reconciled.cards.map((card) => [card.id, card]));
    const nextAllCards = allCards.map((card) =>
      card.boardId === board.id && reconciledById.has(card.id)
        ? reconciledById.get(card.id)!
        : card,
    );

    await boardStateStore.saveCards(nextAllCards);

    if (options.notify) {
      sendToRenderer("kanban:board-updated", {
        boardId: board.id,
        cards: reconciled.cards,
        columns: board.columns,
      });
      for (const finalization of reconciled.finalized) {
        sendToRenderer("kanban:pipeline-status", {
          cardId: finalization.cardId,
          pipelineState: finalization.pipelineState,
          detail: finalization.detail,
        });
      }
    }

    return reconciled.cards;
  }

  function resolveRunDefaults(board: BoardConfig, card: KanbanCard, options: KanbanRunDefaults = {}): KanbanRunDefaults {
    const trackerConfig = board.trackerConfig as unknown as Record<string, unknown>;
    const agentType = options.agentType
      ?? card.runDefaults?.agentType
      ?? board.runDefaults?.agentType
      ?? trackerConfig?.agentType
      ?? "codex";
    return {
      baseRef: "HEAD",
      worktreeMode: "new",
      mode: "instant",
      openChat: true,
      ...(board.runDefaults ?? {}),
      ...(card.runDefaults ?? {}),
      ...options,
      agentType: String(agentType),
    };
  }

  async function renameTaskBranchIfRequested(
    task: { id: string; branch?: string; worktreePath?: string },
    branchName: string | undefined,
    orchestrator: { persistTasks?: () => Promise<void> },
  ): Promise<string | undefined> {
    const requested = branchName?.trim();
    if (!requested || !task.branch || !task.worktreePath || requested === task.branch) {
      return task.branch;
    }
    await execFile("git", ["branch", "-m", task.branch, requested], { cwd: task.worktreePath });
    task.branch = requested;
    await orchestrator.persistTasks?.();
    return requested;
  }

  async function runAgentForCard(
    board: BoardConfig,
    card: KanbanCard,
    options: KanbanRunDefaults & { prompt?: string } = {},
  ): Promise<{ task: { id: string; branch?: string; worktreePath?: string }; run: KanbanTaskRun } | null> {
    const orchestrator = getOrchestrator();
    if (!orchestrator) {
      log.error("[kanban] run-task: orchestrator not available");
      return null;
    }

    const resolved = resolveRunDefaults(board, card, options);
    const agentContext = buildAgentContext({
      cardTitle: card.title,
      cardDescription: card.description ?? "",
      repoPath: board.repoPath ?? "",
      labels: card.labels ?? [],
    });
    const basePrompt = options.prompt?.trim() || agentContext.prompt;
    const effectivePrompt = resolved.mode === "plan"
      ? "First, propose an implementation plan. List files to change, approach, and risks. Wait for approval before making changes.\n\nTask: " + basePrompt
      : basePrompt;

    const task = await orchestrator.createTask(
      effectivePrompt,
      resolved.agentType as "codex" | "claude-code" | "custom",
      agentContext.repoPath,
      resolved.baseRef || "HEAD",
    );

    task.kanbanCardId = card.id;
    task.title = card.title;
    if (resolved.model) (task as typeof task & { model?: string }).model = resolved.model;
    if (resolved.reasoningLevel) {
      (task as typeof task & { reasoningLevel?: string }).reasoningLevel = resolved.reasoningLevel;
    }
    if (resolved.mode === "plan") task.planFirst = true;

    let branch: string | undefined = task.branch;
    try {
      branch = await renameTaskBranchIfRequested(task, resolved.branchName, orchestrator);
    } catch (err) {
      log.warn(`[kanban] Failed to rename branch for task ${task.id}: ${err instanceof Error ? err.message : String(err)}`);
    }
    await orchestrator.persistTasks?.();

    try {
      const chatDb = new ChatDatabase();
      chatDb.insertMessage({
        id: `${task.id}-init`,
        task_id: task.id,
        role: "user",
        kind: null,
        content: effectivePrompt,
        events: null,
        timestamp: Date.now(),
      });
      chatDb.close();
    } catch (err) {
      log.warn(`[kanban] Failed to persist initial user message for task ${task.id}: ${err instanceof Error ? err.message : String(err)}`);
    }

    const run: KanbanTaskRun = {
      id: randomUUID(),
      taskId: card.id,
      boardId: board.id,
      orchestratorTaskId: task.id,
      status: "created",
      worktreePath: task.worktreePath,
      branch,
      baseRef: resolved.baseRef || "HEAD",
      agentType: resolved.agentType,
      model: resolved.model,
      reasoningLevel: resolved.reasoningLevel,
      mode: resolved.mode,
      createdAt: Date.now(),
    };
    await boardStateStore.saveTaskRun(run);
    await boardStateStore.addTaskActivity({
      taskId: card.id,
      boardId: board.id,
      type: "run",
      message: `Started ${resolved.agentType ?? "agent"} run ${task.id}`,
      toValue: { orchestratorTaskId: task.id, branch, model: resolved.model },
    });

    const cards = await boardStateStore.getCards();
    const cardIdx = cards.findIndex((candidate) => candidate.id === card.id);
    if (cardIdx >= 0) {
      cards[cardIdx] = {
        ...cards[cardIdx],
        pipelineState: "agent_running",
        linkedTaskId: task.id,
        updatedAt: Date.now(),
      };
      await boardStateStore.saveCards(cards);
    }

    let completionHandled = false;
    let lastTaskError: string | null = null;
    let onTaskEvent: (eventPayload: { taskId: string; event: Record<string, unknown> & { type: string } }) => void;
    let onTaskStatus: (statusPayload: { taskId: string; status: string }) => void;
    const cleanupTaskListeners = () => {
      registry.unregister(task.id);
      orchestrator.removeListener("orchestrator:event", onTaskEvent);
      orchestrator.removeListener("orchestrator:task-status", onTaskStatus);
    };

    const finalizeTask = async (taskResult: TaskResult, detail: string) => {
      if (completionHandled) return;
      completionHandled = true;
      cleanupTaskListeners();

      try {
        await boardStateStore.updateTaskRun(run.id, {
          status: taskResult.exitCode === 0 ? "completed" : "failed",
          completedAt: Date.now(),
          summary: taskResult.summary ?? detail,
        });
        await boardStateStore.addTaskActivity({
          taskId: card.id,
          boardId: board.id,
          type: "run",
          message: detail,
          toValue: { exitCode: taskResult.exitCode, summary: taskResult.summary },
        });

        const latestCards = await boardStateStore.getCards();
        const completed = completeAgentCardWithResult(
          latestCards,
          board,
          card.id,
          task.id,
          taskResult,
          detail,
        );
        if (!completed.changed) return;

        await boardStateStore.saveCards(completed.cards);
        sendToRenderer("kanban:board-updated", {
          boardId: board.id,
          cards: completed.cards.filter((candidate) => candidate.boardId === board.id),
          columns: board.columns,
        });
        for (const finalization of completed.finalized) {
          sendToRenderer("kanban:pipeline-status", {
            cardId: finalization.cardId,
            pipelineState: finalization.pipelineState,
            detail: finalization.detail,
          });
        }
      } catch (err) {
        completionHandled = false;
        log.error(`[kanban] Failed to process task completion: ${err instanceof Error ? err.message : String(err)}`);
      }
    };

    onTaskEvent = (eventPayload: { taskId: string; event: Record<string, unknown> & { type: string } }) => {
      if (eventPayload.taskId !== task.id) return;
      const evt = eventPayload.event;

      if (evt.type === "error") {
        const message = String(evt.error ?? evt.message ?? "Agent task failed.").slice(0, 500);
        lastTaskError = message;
        sendToRenderer("kanban:task-progress", {
          cardId: card.id,
          eventType: "progress",
          message: message.slice(0, 200),
        });
        return;
      }

      if (evt.type === "progress" || evt.type === "output") {
        const rawMessage = (evt.message as string)
          ?? (evt.data as string)
          ?? JSON.stringify(evt);
        sendToRenderer("kanban:task-progress", {
          cardId: card.id,
          eventType: evt.type as "progress" | "output",
          message: String(rawMessage).slice(0, 200),
        });
        return;
      }

      if (evt.type === "file_edit" || evt.type === "file_read") {
        const filePath = (evt.path as string) ?? "";
        const shortPath = filePath.split("/").slice(-2).join("/");
        const verb = evt.type === "file_edit" ? "Editing" : "Reading";
        sendToRenderer("kanban:task-progress", {
          cardId: card.id,
          eventType: "progress",
          message: `${verb} ${shortPath}`,
        });
        return;
      }

      if (evt.type === "command" || evt.type === "command_run") {
        const cmd = (evt.command as string) ?? "";
        sendToRenderer("kanban:task-progress", {
          cardId: card.id,
          eventType: "progress",
          message: `Running ${cmd.slice(0, 80)}`,
        });
        return;
      }

      if (evt.type === "text" && evt.content) {
        const text = String(evt.content).slice(0, 100).replace(/\n/g, " ").trim();
        if (text) {
          sendToRenderer("kanban:task-progress", {
            cardId: card.id,
            eventType: "progress",
            message: text,
          });
        }
        return;
      }

      if (evt.type === "done") {
        const doneSummary = typeof evt.content === "string" && evt.content.trim()
          ? evt.content.trim()
          : undefined;
        const taskResult: TaskResult = {
          exitCode: lastTaskError ? 1 : 0,
          summary: lastTaskError ?? doneSummary,
        };

        void finalizeTask(
          taskResult,
          taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
        );
        return;
      }

      if (evt.type === "exit" || evt.type === "complete") {
        const taskResult: TaskResult = {
          exitCode: (evt.exitCode as number) ?? 0,
          prUrl: evt.prUrl as string | undefined,
          prNumber: evt.prNumber as number | undefined,
          summary: evt.summary as string | undefined,
          modifiedFiles: evt.modifiedFiles as string[] | undefined,
          agentSummary: evt.agentSummary as string | undefined,
        };

        void finalizeTask(
          taskResult,
          taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
        );
      }
    };

    onTaskStatus = (statusPayload: { taskId: string; status: string }) => {
      if (statusPayload.taskId !== task.id) return;
      const status = statusPayload.status as TaskStatus;

      if (statusPayload.status === "queued") {
        void boardStateStore.updateTaskRun(run.id, { status: "queued" });
        sendToRenderer("kanban:pipeline-status", {
          cardId: card.id,
          pipelineState: "agent_running",
          detail: `Agent task ${task.id} queued for: ${card.title}`,
        });
        return;
      }

      if (statusPayload.status === "running") {
        void boardStateStore.updateTaskRun(run.id, { status: "running", startedAt: Date.now() });
      }

      if (!isCompletedAgentTaskStatus(status)) return;

      const latestTask = orchestrator.getTask(task.id) ?? task;
      const taskResult = taskResultFromCompletedTask(latestTask);
      if (!taskResult) return;

      void finalizeTask(
        taskResult,
        taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
      );
    };

    if (registry.register(task.id, onTaskEvent)) {
      orchestrator.on("orchestrator:event", onTaskEvent);
    }
    orchestrator.on("orchestrator:task-status", onTaskStatus);

    const latestCards = await boardStateStore.getCards();
    sendToRenderer("kanban:board-updated", {
      boardId: board.id,
      cards: latestCards.filter((candidate) => candidate.boardId === board.id),
      columns: board.columns,
    });
    sendToRenderer("kanban:pipeline-status", {
      cardId: card.id,
      pipelineState: "agent_running",
      detail: `Agent task ${task.id} spawned for: ${card.title}`,
    });

    orchestrator.startTask(task.id).catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      log.error(`[kanban] Failed to start task ${task.id}: ${message}`);
      void finalizeTask(
        { exitCode: 1, summary: message },
        "Agent task failed to start",
      );
    });

    log.info(`[kanban] Spawned agent task "${task.id}" for card "${card.id}" (${card.title})`);
    return { task, run };
  }

  // ─── Renderer → Main Handlers (ipcMain.handle) ──────────────────────────

  ipcMain.handle(
    "kanban:get-board",
    async (_evt, payload: { boardId: string }): Promise<BoardState | null> => {
      try {
        const data = await boardConfigStore.load();
        const board = data.boards.find((b) => b.id === payload.boardId);
        if (!board) return null;

        const boardCards = await reconcileAgentCardsForBoard(board);

        return {
          config: board,
          cards: boardCards,
          columns: board.columns,
        };
      } catch (err) {
        log.error(`[kanban] get-board error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:get-boards",
    async (): Promise<BoardConfig[]> => {
      try {
        const data = await boardConfigStore.load();
        return data.boards;
      } catch (err) {
        log.error(`[kanban] get-boards error: ${err instanceof Error ? err.message : String(err)}`);
        return [];
      }
    },
  );

  ipcMain.handle(
    "kanban:create-board",
    async (_evt, payload: { config: Partial<BoardConfig> }): Promise<BoardConfig | null> => {
      try {
        const config = payload.config;

        // Build a complete BoardConfig with defaults for missing fields
        const board: BoardConfig = {
          id: config.id || randomUUID(),
          name: config.name || `Board ${new Date().toLocaleDateString()}`,
          repoPath: config.repoPath || "",
          columns: config.columns || BoardConfigStore.defaultColumns(),
          trackerType: (config.trackerType as TrackerSource) || "local",
          trackerConfig: config.trackerConfig || { owner: "", repo: "" },
          autonomyLevel: config.autonomyLevel || "moderate",
          pollIntervalMs: config.pollIntervalMs || 60_000,
          ciPollIntervalMs: config.ciPollIntervalMs || 30_000,
          maxIterations: config.maxIterations || 3,
          filterCriteria: config.filterCriteria || {},
          webhookPort: config.webhookPort,
          webhookSecret: config.webhookSecret,
          densityMode: config.densityMode,
          savedViews: config.savedViews,
          columnStates: config.columnStates,
          fields: config.fields,
          customFields: config.customFields,
          runDefaults: config.runDefaults,
          symphony: config.symphony,
        };

        await boardConfigStore.saveBoard(board);
        log.info(`[kanban] Created board "${board.id}" for repo "${board.repoPath}"`);

        // Notify renderer of the new board state
        sendToRenderer("kanban:board-updated", {
          boardId: board.id,
          cards: [],
          columns: board.columns,
        });

        return board;
      } catch (err) {
        log.error(`[kanban] create-board error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:update-board",
    async (_evt, payload: { boardId: string; config: Partial<BoardConfig> }): Promise<BoardConfig | null> => {
      try {
        const data = await boardConfigStore.load();
        const idx = data.boards.findIndex((b) => b.id === payload.boardId);
        if (idx < 0) {
          log.warn(`[kanban] update-board: board "${payload.boardId}" not found`);
          return null;
        }

        // Merge the partial config into the existing board
        const existing = data.boards[idx];
        const updated: BoardConfig = {
          ...existing,
          ...payload.config,
          id: existing.id, // Never allow ID to change
        };

        data.boards[idx] = updated;
        await boardConfigStore.save(data);
        log.info(`[kanban] Updated board "${updated.id}"`);

        // Notify renderer
        const cards = await boardStateStore.getCards();
        sendToRenderer("kanban:board-updated", {
          boardId: updated.id,
          cards: cards.filter((c) => c.boardId === updated.id),
          columns: updated.columns,
        });

        return updated;
      } catch (err) {
        log.error(`[kanban] update-board error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:delete-board",
    async (_evt, payload: { boardId: string }): Promise<void> => {
      try {
        await boardConfigStore.deleteBoard(payload.boardId);
        log.info(`[kanban] Deleted board "${payload.boardId}"`);
      } catch (err) {
        log.error(`[kanban] delete-board error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:save-board-config",
    async (
      _evt,
      payload: {
        boardId: string;
        config: {
          densityMode?: DensityMode;
          savedViews?: SavedView[];
          columnStates?: ColumnUIState;
        };
      },
    ): Promise<{ success: boolean; error?: string }> => {
      try {
        const data = await boardConfigStore.load();
        const idx = data.boards.findIndex((b) => b.id === payload.boardId);
        if (idx < 0) {
          log.warn(`[kanban] save-board-config: board "${payload.boardId}" not found`);
          return { success: false, error: "Board not found" };
        }

        const existing = data.boards[idx];

        // Merge only the provided config fields into the existing board
        if (payload.config.densityMode !== undefined) {
          existing.densityMode = payload.config.densityMode;
        }
        if (payload.config.savedViews !== undefined) {
          existing.savedViews = payload.config.savedViews;
        }
        if (payload.config.columnStates !== undefined) {
          existing.columnStates = payload.config.columnStates;
        }

        data.boards[idx] = existing;
        await boardConfigStore.save(data);
        log.info(`[kanban] Saved board config for "${payload.boardId}"`);

        return { success: true };
      } catch (err) {
        log.error(`[kanban] save-board-config error: ${err instanceof Error ? err.message : String(err)}`);
        return { success: false, error: err instanceof Error ? err.message : String(err) };
      }
    },
  );

  ipcMain.handle(
    "kanban:move-card",
    async (_evt, payload: { cardId: string; targetColumnId: string; positionIndex: number }): Promise<void> => {
      try {
        const cards = await boardStateStore.getCards();
        const cardIdx = cards.findIndex((c) => c.id === payload.cardId);

        if (cardIdx >= 0) {
          const card = cards[cardIdx];
          const fromColumn = card.columnId;

          // Update card position
          cards[cardIdx] = {
            ...card,
            columnId: payload.targetColumnId,
            positionIndex: payload.positionIndex,
            updatedAt: Date.now(),
          };

          await boardStateStore.saveCards(cards);
          await boardStateStore.addTaskActivity({
            taskId: card.id,
            boardId: card.boardId ?? "",
            type: "moved",
            message: "Task moved",
            field: "columnId",
            fromValue: fromColumn,
            toValue: payload.targetColumnId,
          });
          const latestCards = await boardStateStore.getCards();

          // Emit card:moved event to renderer
          sendToRenderer("kanban:card-moved", {
            cardId: payload.cardId,
            fromColumn,
            toColumn: payload.targetColumnId,
          });

          // Emit board-updated with filtered cards for this board
          const boardId = card.boardId;
          if (boardId) {
            const data = await boardConfigStore.load();
            const board = data.boards.find((b) => b.id === boardId);
            if (board) {
              sendToRenderer("kanban:board-updated", {
                boardId: board.id,
                cards: latestCards.filter((c) => c.boardId === boardId),
                columns: board.columns,
              });
            }
          }
        }
      } catch (err) {
        log.error(`[kanban] move-card error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:create-card",
    async (_evt, payload: { boardId: string; columnId: string; title: string; description?: string }): Promise<KanbanCard | null> => {
      try {
        const cards = await boardStateStore.getCards();
        const cardsInColumn = cards.filter((c) => c.boardId === payload.boardId && c.columnId === payload.columnId);
        const now = Date.now();

        const newCard: KanbanCard = {
          id: randomUUID(),
          boardId: payload.boardId,
          source: "local",
          externalId: "",
          externalUrl: "",
          title: payload.title,
          description: payload.description ?? "",
          columnId: payload.columnId,
          positionIndex: cardsInColumn.length,
          priority: "none",
          labels: [],
          pipelineState: "idle",
          iterationCount: 0,
          createdAt: now,
          updatedAt: now,
        };

        cards.push(newCard);
        await boardStateStore.saveCards(cards);
        await boardStateStore.addTaskActivity({
          taskId: newCard.id,
          boardId: payload.boardId,
          type: "created",
          message: "Task created",
          toValue: { title: newCard.title },
          createdAt: now,
        });
        const latestCards = await boardStateStore.getCards();

        log.info(`[kanban] Created card "${newCard.id}" in column "${payload.columnId}"`);

        // Notify renderer of updated board state
        const data = await boardConfigStore.load();
        const board = data.boards.find((b) => b.id === payload.boardId);
        if (board) {
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: latestCards.filter((c) => c.boardId === payload.boardId),
            columns: board.columns,
          });
        }

        return newCard;
      } catch (err) {
        log.error(`[kanban] create-card error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:create-task",
    async (
      _evt,
      payload: {
        boardId: string;
        columnId: string;
        title: string;
        description?: string;
        updates?: Partial<KanbanCard>;
      },
    ): Promise<KanbanCard | null> => {
      try {
        const cards = await boardStateStore.getCards();
        const cardsInColumn = cards.filter((c) => c.boardId === payload.boardId && c.columnId === payload.columnId);
        const now = Date.now();
        const task: KanbanCard = {
          id: randomUUID(),
          boardId: payload.boardId,
          source: "local",
          externalId: "",
          externalUrl: "",
          title: payload.title,
          description: payload.description ?? "",
          columnId: payload.columnId,
          positionIndex: cardsInColumn.length,
          priority: "none",
          labels: [],
          pipelineState: "idle",
          iterationCount: 0,
          createdAt: now,
          updatedAt: now,
          ...(payload.updates ?? {}),
        };
        cards.push(task);
        await boardStateStore.saveCards(cards);
        await boardStateStore.addTaskActivity({
          taskId: task.id,
          boardId: payload.boardId,
          type: "created",
          message: "Task created",
          toValue: { title: task.title },
          createdAt: now,
        });
        const latestCards = await boardStateStore.getCards();

        const board = await boardConfigStore.getBoardById(payload.boardId);
        if (board) {
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: latestCards.filter((candidate) => candidate.boardId === board.id),
            columns: board.columns,
          });
        }
        return task;
      } catch (err) {
        log.error(`[kanban] create-task error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:update-card",
    async (_evt, payload: { cardId: string; updates: Partial<KanbanCard> }): Promise<KanbanCard | null> => {
      try {
        const cards = await boardStateStore.getCards();
        const cardIdx = cards.findIndex((c) => c.id === payload.cardId);

        if (cardIdx < 0) {
          log.warn(`[kanban] update-card: card "${payload.cardId}" not found`);
          return null;
        }

        // Apply partial updates onto the existing card, preserving the ID
        const existing = cards[cardIdx];
        const updated: KanbanCard = {
          ...existing,
          ...payload.updates,
          id: existing.id, // Never allow ID to change
          updatedAt: Date.now(),
        };

        cards[cardIdx] = updated;
        await boardStateStore.saveCards(cards);

        const changedFields = Object.keys(payload.updates).filter((field) => field !== "updatedAt");
        for (const field of changedFields) {
          await boardStateStore.addTaskActivity({
            taskId: existing.id,
            boardId: existing.boardId ?? "",
            type: field === "columnId" ? "moved" : "updated",
            message: field === "columnId" ? "Task moved" : `Updated ${field}`,
            field,
            fromValue: (existing as unknown as Record<string, unknown>)[field],
            toValue: (updated as unknown as Record<string, unknown>)[field],
          });
        }
        const latestCards = await boardStateStore.getCards();

        log.info(`[kanban] Updated card "${updated.id}"`);

        // Notify renderer of updated board state
        const data = await boardConfigStore.load();
        const board = existing.boardId
          ? data.boards.find((b) => b.id === existing.boardId)
          : data.boards[0];

        if (board) {
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: latestCards.filter((c) => c.boardId === board.id),
            columns: board.columns,
          });
        }

        return updated;
      } catch (err) {
        log.error(`[kanban] update-card error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:update-task",
    async (_evt, payload: { taskId: string; updates: Partial<KanbanCard> }): Promise<KanbanCard | null> => {
      try {
        const cards = await boardStateStore.getCards();
        const taskIdx = cards.findIndex((c) => c.id === payload.taskId);
        if (taskIdx < 0) return null;

        const existing = cards[taskIdx];
        const updated: KanbanCard = {
          ...existing,
          ...payload.updates,
          id: existing.id,
          updatedAt: Date.now(),
        };
        cards[taskIdx] = updated;
        await boardStateStore.saveCards(cards);

        const changedFields = Object.keys(payload.updates).filter((field) => field !== "updatedAt");
        for (const field of changedFields) {
          await boardStateStore.addTaskActivity({
            taskId: existing.id,
            boardId: existing.boardId ?? "",
            type: field === "columnId" ? "moved" : "updated",
            message: field === "columnId" ? "Task moved" : `Updated ${field}`,
            field,
            fromValue: (existing as unknown as Record<string, unknown>)[field],
            toValue: (updated as unknown as Record<string, unknown>)[field],
          });
        }
        const latestCards = await boardStateStore.getCards();

        const board = existing.boardId ? await boardConfigStore.getBoardById(existing.boardId) : undefined;
        if (board) {
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: latestCards.filter((candidate) => candidate.boardId === board.id),
            columns: board.columns,
          });
        }
        return updated;
      } catch (err) {
        log.error(`[kanban] update-task error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:delete-task",
    async (_evt, payload: { taskId: string }): Promise<void> => {
      const task = await boardStateStore.getTask(payload.taskId);
      await boardStateStore.deleteTask(payload.taskId);
      if (task?.boardId) {
        const board = await boardConfigStore.getBoardById(task.boardId);
        const cards = await boardStateStore.getCards();
        if (board) {
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: cards.filter((candidate) => candidate.boardId === board.id),
            columns: board.columns,
          });
        }
      }
    },
  );

  ipcMain.handle(
    "kanban:run-task",
    async (
      _evt,
      payload: { taskId: string; options?: KanbanRunDefaults & { prompt?: string } },
    ): Promise<{ success: boolean; task?: unknown; run?: KanbanTaskRun; error?: string }> => {
      try {
        const card = await boardStateStore.getTask(payload.taskId);
        if (!card) return { success: false, error: "Task not found" };
        const board = card.boardId ? await boardConfigStore.getBoardById(card.boardId) : undefined;
        if (!board) return { success: false, error: "Board not found" };
        const result = await runAgentForCard(board, card, payload.options ?? {});
        if (!result) return { success: false, error: "Agent could not be started" };
        return { success: true, task: result.task, run: result.run };
      } catch (err) {
        const error = err instanceof Error ? err.message : String(err);
        log.error(`[kanban] run-task error: ${error}`);
        return { success: false, error };
      }
    },
  );

  ipcMain.handle(
    "kanban:get-task-runs",
    async (_evt, payload: { taskId: string }): Promise<KanbanTaskRun[]> => {
      try {
        return await boardStateStore.getTaskRuns(payload.taskId);
      } catch (err) {
        log.error(`[kanban] get-task-runs error: ${err instanceof Error ? err.message : String(err)}`);
        return [];
      }
    },
  );

  ipcMain.handle(
    "kanban:get-task-activity",
    async (_evt, payload: { taskId: string }): Promise<unknown[]> => {
      try {
        return await boardStateStore.getTaskActivity(payload.taskId);
      } catch (err) {
        log.error(`[kanban] get-task-activity error: ${err instanceof Error ? err.message : String(err)}`);
        return [];
      }
    },
  );

  ipcMain.handle(
    "kanban:get-task-by-orchestrator-task",
    async (_evt, payload: { orchestratorTaskId: string }): Promise<KanbanCard | null> => {
      try {
        return (await boardStateStore.getTaskByOrchestratorTask(payload.orchestratorTaskId)) ?? null;
      } catch (err) {
        log.error(`[kanban] get-task-by-orchestrator-task error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:bulk-update-cards",
    async (_evt, payload: { cardIds: string[]; updates: Partial<KanbanCard> }): Promise<void> => {
      try {
        const cards = await boardStateStore.getCards();
        const idSet = new Set(payload.cardIds);
        let changed = false;

        for (let i = 0; i < cards.length; i++) {
          if (idSet.has(cards[i].id)) {
            cards[i] = {
              ...cards[i],
              ...payload.updates,
              id: cards[i].id, // Never allow ID to change
              updatedAt: Date.now(),
            };
            changed = true;
          }
        }

        if (changed) {
          await boardStateStore.saveCards(cards);

          // Determine boardId from the first matched card for the event
          const firstMatched = cards.find((c) => idSet.has(c.id));
          const boardId = firstMatched?.boardId;
          if (boardId) {
            const data = await boardConfigStore.load();
            const board = data.boards.find((b) => b.id === boardId);
            if (board) {
              sendToRenderer("kanban:board-updated", {
                boardId: board.id,
                cards: cards.filter((c) => c.boardId === boardId),
                columns: board.columns,
              });
            }
          }
        }

        log.info(`[kanban] bulk-update-cards: updated ${payload.cardIds.length} card(s)`);
      } catch (err) {
        log.error(`[kanban] bulk-update-cards error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:bulk-delete-cards",
    async (_evt, payload: { cardIds: string[] }): Promise<void> => {
      try {
        const BATCH_SIZE = 10;
        let cards = await boardStateStore.getCards();

        // Determine boardId before deletion for the event
        const firstTarget = cards.find((c) => payload.cardIds.includes(c.id));
        const boardId = firstTarget?.boardId;

        // Process deletions in batches of 10
        for (let i = 0; i < payload.cardIds.length; i += BATCH_SIZE) {
          const batch = new Set(payload.cardIds.slice(i, i + BATCH_SIZE));
          cards = cards.filter((c) => !batch.has(c.id));
        }

        await boardStateStore.saveCards(cards);

        if (boardId) {
          const data = await boardConfigStore.load();
          const board = data.boards.find((b) => b.id === boardId);
          if (board) {
            sendToRenderer("kanban:board-updated", {
              boardId: board.id,
              cards: cards.filter((c) => c.boardId === boardId),
              columns: board.columns,
            });
          }
        }

        log.info(`[kanban] bulk-delete-cards: deleted ${payload.cardIds.length} card(s)`);
      } catch (err) {
        log.error(`[kanban] bulk-delete-cards error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:force-sync",
    async (_evt, payload: { boardId: string }): Promise<void> => {
      const board = await boardConfigStore.getBoardById(payload.boardId);
      if (board?.symphony?.enabled) {
        await symphonyRuntimeManager.refresh(payload.boardId);
      }
      if (board) {
        await reconcileAgentCardsForBoard(board, { notify: true });
      }
      log.info(`[kanban] force-sync requested for board "${payload.boardId}"`);
    },
  );

  // ─── Symphony Runtime IPC ────────────────────────────────────────────────

  ipcMain.handle(
    "kanban:symphony-start",
    async (_evt, payload: { boardId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.start(payload.boardId)),
  );

  ipcMain.handle(
    "kanban:symphony-stop",
    async (_evt, payload: { boardId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.stop(payload.boardId)),
  );

  ipcMain.handle(
    "kanban:symphony-refresh",
    async (_evt, payload: { boardId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.refresh(payload.boardId)),
  );

  ipcMain.handle(
    "kanban:symphony-get-state",
    async (_evt, payload: { boardId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.getState(payload.boardId)),
  );

  ipcMain.handle(
    "kanban:symphony-get-issue",
    async (_evt, payload: { boardId: string; issueId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.getIssue(payload.boardId, payload.issueId)),
  );

  ipcMain.handle(
    "kanban:symphony-retry-issue",
    async (_evt, payload: { boardId: string; issueId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.retryIssue(payload.boardId, payload.issueId)),
  );

  ipcMain.handle(
    "kanban:symphony-cancel-issue",
    async (_evt, payload: { boardId: string; issueId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.cancelIssue(payload.boardId, payload.issueId)),
  );

  ipcMain.handle(
    "kanban:symphony-release-blocked-issue",
    async (_evt, payload: { boardId: string; issueId: string }) =>
      safeSymphonyCall(() => symphonyRuntimeManager.releaseBlockedIssue(payload.boardId, payload.issueId)),
  );

  ipcMain.handle(
    "kanban:get-rules",
    async (_evt, _payload: { boardId: string }): Promise<AutomationRule[]> => {
      try {
        return await ruleStore.getRules();
      } catch (err) {
        log.error(`[kanban] get-rules error: ${err instanceof Error ? err.message : String(err)}`);
        return [];
      }
    },
  );

  ipcMain.handle(
    "kanban:save-rule",
    async (_evt, payload: { boardId: string; rule: AutomationRule }): Promise<AutomationRule | null> => {
      try {
        const rule = {
          ...payload.rule,
          id: payload.rule.id || randomUUID(),
        };
        await ruleStore.saveRule(rule);
        log.info(`[kanban] Saved rule "${rule.id}" (${rule.name})`);
        return rule;
      } catch (err) {
        log.error(`[kanban] save-rule error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:delete-rule",
    async (_evt, payload: { boardId: string; ruleId: string }): Promise<void> => {
      try {
        await ruleStore.deleteRule(payload.ruleId);
        log.info(`[kanban] Deleted rule "${payload.ruleId}"`);
      } catch (err) {
        log.error(`[kanban] delete-rule error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:fire-rule",
    async (_evt, payload: { boardId: string; ruleId: string; cardId: string }): Promise<void> => {
      try {
        // Special case: __spawn_agent__ triggers the orchestrator directly
        if (payload.ruleId === "__spawn_agent__") {
          const cards = await boardStateStore.getCards();
          const card = cards.find((c) => c.id === payload.cardId);
          if (!card) {
            log.warn(`[kanban] fire-rule: card "${payload.cardId}" not found`);
            return;
          }

          const data = await boardConfigStore.load();
          const board = data.boards.find((b) => b.id === payload.boardId);
          if (!board) {
            log.warn(`[kanban] fire-rule: board "${payload.boardId}" not found`);
            return;
          }

          // Get the real orchestrator instance
          const orchestrator = getOrchestrator();
          if (!orchestrator) {
            log.error("[kanban] fire-rule: orchestrator not available");
            return;
          }

          // Build structured agent context from card data
          const agentContext = buildAgentContext({
            cardTitle: card.title,
            cardDescription: card.description ?? "",
            repoPath: board.repoPath ?? "",
            labels: card.labels ?? [],
          });

          // Resolve agent type from board config (default: codex)
          const agentType = (board.trackerConfig as unknown as Record<string, unknown>)?.agentType as string || "codex";

          // Create a real agent task via the orchestrator
          const task = await orchestrator.createTask(
            agentContext.prompt,
            agentType as "codex" | "claude-code" | "custom",
            agentContext.repoPath,
          );

          // Set kanbanCardId on the task for analytics tracking
          task.kanbanCardId = card.id;

          // Persist the initial prompt as a user message in the chat DB so
          // the agent view shows it when the user navigates to the task.
          try {
            const chatDb = new ChatDatabase();
            chatDb.insertMessage({
              id: `${task.id}-init`,
              task_id: task.id,
              role: "user",
              kind: null,
              content: agentContext.prompt,
              events: null,
              timestamp: Date.now(),
            });
            chatDb.close();
          } catch (err) {
            log.warn(`[kanban] Failed to persist initial user message for task ${task.id}: ${err instanceof Error ? err.message : String(err)}`);
          }

          // Update card with the linked task ID and pipeline state
          const cardIdx = cards.findIndex((c) => c.id === payload.cardId);
          cards[cardIdx] = {
            ...card,
            pipelineState: "agent_running",
            linkedTaskId: task.id,
            updatedAt: Date.now(),
          };
          await boardStateStore.saveCards(cards);

          let completionHandled = false;
          let lastTaskError: string | null = null;
          let onTaskEvent: (eventPayload: { taskId: string; event: Record<string, unknown> & { type: string } }) => void;
          let onTaskStatus: (statusPayload: { taskId: string; status: string }) => void;
          const cleanupTaskListeners = () => {
            registry.unregister(task.id);
            orchestrator.removeListener("orchestrator:event", onTaskEvent);
            orchestrator.removeListener("orchestrator:task-status", onTaskStatus);
          };

          const finalizeTask = async (taskResult: TaskResult, detail: string) => {
            if (completionHandled) return;
            completionHandled = true;
            cleanupTaskListeners();

            try {
              const latestCards = await boardStateStore.getCards();
              const completed = completeAgentCardWithResult(
                latestCards,
                board,
                payload.cardId,
                task.id,
                taskResult,
                detail,
              );
              if (!completed.changed) return;

              await boardStateStore.saveCards(completed.cards);
              sendToRenderer("kanban:board-updated", {
                boardId: board.id,
                cards: completed.cards.filter((c) => c.boardId === board.id),
                columns: board.columns,
              });
              for (const finalization of completed.finalized) {
                sendToRenderer("kanban:pipeline-status", {
                  cardId: finalization.cardId,
                  pipelineState: finalization.pipelineState,
                  detail: finalization.detail,
                });
              }
            } catch (err) {
              completionHandled = false;
              log.error(`[kanban] Failed to process task completion: ${err instanceof Error ? err.message : String(err)}`);
            }
          };

          // Listen for task events to relay progress and handle completion
          onTaskEvent = (eventPayload: { taskId: string; event: Record<string, unknown> & { type: string } }) => {
            if (eventPayload.taskId !== task.id) return;
            const evt = eventPayload.event;

            if (evt.type === "error") {
              const message = String(evt.error ?? evt.message ?? "Agent task failed.").slice(0, 500);
              lastTaskError = message;
              sendToRenderer("kanban:task-progress", {
                cardId: card.id,
                eventType: "progress",
                message: message.slice(0, 200),
              });
              return;
            }

            // Relay progress/output events to the renderer in real-time
            if (evt.type === "progress" || evt.type === "output") {
              const rawMessage = (evt.message as string)
                ?? (evt.data as string)
                ?? JSON.stringify(evt);
              const message = String(rawMessage).slice(0, 200);
              sendToRenderer("kanban:task-progress", {
                cardId: card.id,
                eventType: evt.type as "progress" | "output",
                message,
              });
              return;
            }

            // Relay meaningful agent activity as progress so the kanban
            // card can show what the agent is currently doing.
            if (evt.type === "file_edit" || evt.type === "file_read") {
              const filePath = (evt.path as string) ?? "";
              const shortPath = filePath.split("/").slice(-2).join("/");
              const verb = evt.type === "file_edit" ? "Editing" : "Reading";
              sendToRenderer("kanban:task-progress", {
                cardId: card.id,
                eventType: "progress",
                message: `${verb} ${shortPath}`,
              });
              return;
            }

            if (evt.type === "command" || evt.type === "command_run") {
              const cmd = (evt.command as string) ?? "";
              sendToRenderer("kanban:task-progress", {
                cardId: card.id,
                eventType: "progress",
                message: `Running ${cmd.slice(0, 80)}`,
              });
              return;
            }

            if (evt.type === "text" && evt.content) {
              // Only relay the first 100 chars of text as a status hint
              const text = String(evt.content).slice(0, 100).replace(/\n/g, " ").trim();
              if (text) {
                sendToRenderer("kanban:task-progress", {
                  cardId: card.id,
                  eventType: "progress",
                  message: text,
                });
              }
              return;
            }

            if (evt.type === "done") {
              const doneSummary = typeof evt.content === "string" && evt.content.trim()
                ? evt.content.trim()
                : undefined;
              const taskResult: TaskResult = {
                exitCode: lastTaskError ? 1 : 0,
                summary: lastTaskError ?? doneSummary,
              };

              void finalizeTask(
                taskResult,
                taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
              );
              return;
            }

            if (evt.type === "exit" || evt.type === "complete") {
              const taskResult: TaskResult = {
                exitCode: (evt.exitCode as number) ?? 0,
                prUrl: evt.prUrl as string | undefined,
                prNumber: evt.prNumber as number | undefined,
                summary: evt.summary as string | undefined,
                modifiedFiles: evt.modifiedFiles as string[] | undefined,
                agentSummary: evt.agentSummary as string | undefined,
              };

              void finalizeTask(
                taskResult,
                taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
              );
            }
          };

          onTaskStatus = (statusPayload: { taskId: string; status: string }) => {
            if (statusPayload.taskId !== task.id) return;
            const status = statusPayload.status as TaskStatus;

            if (statusPayload.status === "queued") {
              sendToRenderer("kanban:pipeline-status", {
                cardId: card.id,
                pipelineState: "agent_running",
                detail: `Agent task ${task.id} queued for: ${card.title}`,
              });
              return;
            }

            if (!isCompletedAgentTaskStatus(status)) return;

            const latestTask = orchestrator.getTask(task.id) ?? task;
            const taskResult = taskResultFromCompletedTask(latestTask);
            if (!taskResult) return;

            void finalizeTask(
              taskResult,
              taskResult.exitCode === 0 ? "Agent task completed successfully" : "Agent task failed",
            );
          };

          // Register via registry — skip if already registered (prevents duplicates)
          if (registry.register(task.id, onTaskEvent)) {
            orchestrator.on("orchestrator:event", onTaskEvent);
          }
          orchestrator.on("orchestrator:task-status", onTaskStatus);

          // Notify renderer of state change
          sendToRenderer("kanban:board-updated", {
            boardId: board.id,
            cards: cards.filter((c) => c.boardId === board.id),
            columns: board.columns,
          });
          sendToRenderer("kanban:pipeline-status", {
            cardId: card.id,
            pipelineState: "agent_running",
            detail: `Agent task ${task.id} spawned for: ${card.title}`,
          });

          // Start the task after listeners are registered so quick agent
          // runs cannot complete before kanban observes their status.
          orchestrator.startTask(task.id).catch((err) => {
            const message = err instanceof Error ? err.message : String(err);
            log.error(`[kanban] Failed to start task ${task.id}: ${message}`);
            void finalizeTask(
              { exitCode: 1, summary: message },
              "Agent task failed to start",
            );
          });

          log.info(`[kanban] Spawned agent task "${task.id}" for card "${card.id}" (${card.title})`);
          return;
        }

        // Normal rule firing — AutomationEngine integration
        log.info(`[kanban] fire-rule: rule="${payload.ruleId}" card="${payload.cardId}"`);
      } catch (err) {
        log.error(`[kanban] fire-rule error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:approve-merge",
    async (_evt, payload: { cardId: string }): Promise<void> => {
      // PipelineController integration would go here
      log.info(`[kanban] approve-merge: card="${payload.cardId}"`);
    },
  );

  ipcMain.handle(
    "kanban:cancel-pipeline",
    async (_evt, payload: { cardId: string }): Promise<void> => {
      // PipelineController integration would go here
      log.info(`[kanban] cancel-pipeline: card="${payload.cardId}"`);
    },
  );

  ipcMain.handle(
    "kanban:get-analytics",
    async (_evt, payload: { boardId: string }): Promise<BoardAnalytics | null> => {
      try {
        const data = await analyticsStore.load();
        if (data.analytics.length === 0) return null;

        // Compute summary from per-card analytics
        const cards = data.analytics;
        const totalCost = cards.reduce(
          (sum, c) => sum + c.tokenUsage.reduce((s, t) => s + (t.costEstimate ?? 0), 0),
          0,
        );
        const completedCards = cards.filter((c) => c.outcome === "success");
        const cycleTimesMs = completedCards
          .map((c) => c.cycleTimeMs)
          .filter((t): t is number => t != null);
        const avgCycleTimeMs = cycleTimesMs.length > 0
          ? cycleTimesMs.reduce((a, b) => a + b, 0) / cycleTimesMs.length
          : 0;

        const totalAttempted = cards.filter((c) => c.outcome !== "in_progress").length;
        const totalCompleted = completedCards.length;
        const successRate = totalAttempted > 0 ? totalCompleted / totalAttempted : 0;

        const iterationDistribution: Record<number, number> = {};
        for (const card of cards) {
          const count = card.iterationCount;
          iterationDistribution[count] = (iterationDistribution[count] ?? 0) + 1;
        }

        return {
          boardId: payload.boardId,
          cards,
          summary: {
            totalCost,
            averageCycleTimeMs: avgCycleTimeMs,
            successRate,
            totalAttempted,
            totalCompleted,
            iterationDistribution,
          },
        };
      } catch (err) {
        log.error(`[kanban] get-analytics error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:get-card-analytics",
    async (_evt, payload: { cardId: string }): Promise<CardAnalytics | null> => {
      try {
        return (await analyticsStore.getCardAnalytics(payload.cardId)) ?? null;
      } catch (err) {
        log.error(`[kanban] get-card-analytics error: ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
  );

  ipcMain.handle(
    "kanban:connect-tracker",
    async (_evt, payload: { type: TrackerSource; config: Record<string, unknown> }): Promise<{ success: boolean; error?: string }> => {
      try {
        // For now, just validate the config has required fields and return success.
        // Full tracker authentication will be wired when BoardSync is started.
        if (payload.type === "github") {
          if (!payload.config.owner || !payload.config.repo) {
            return { success: false, error: "Owner and repo are required" };
          }
        } else if (payload.type === "github-projects") {
          if (!payload.config.owner || !payload.config.projectNumber) {
            return { success: false, error: "Owner and project number are required" };
          }
        } else if (payload.type === "linear") {
          if (!payload.config.projectSlug) {
            return { success: false, error: "Linear project slug is required" };
          }
        } else if (payload.type === "jira") {
          if (!payload.config.instanceUrl || !payload.config.projectKey) {
            return { success: false, error: "Instance URL and project key are required" };
          }
        }

        log.info(`[kanban] Tracker connected: type=${payload.type}`);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        log.error(`[kanban] connect-tracker error: ${message}`);
        return { success: false, error: message };
      }
    },
  );

  // ─── Relation Helpers ─────────────────────────────────────────────────────

  /**
   * Returns the inverse relation type for bidirectional linking.
   * parent↔child, blocks↔blocked-by, relates-to↔relates-to.
   */
  function inverseRelationType(type: RelationType): RelationType {
    switch (type) {
      case "parent": return "child";
      case "child": return "parent";
      case "blocks": return "blocked-by";
      case "blocked-by": return "blocks";
      case "relates-to": return "relates-to";
    }
  }

  ipcMain.handle(
    "kanban:add-relation",
    async (_evt, payload: { sourceId: string; targetId: string; type: RelationType }): Promise<void> => {
      try {
        const cards = await boardStateStore.getCards();
        const sourceIdx = cards.findIndex((c) => c.id === payload.sourceId);
        const targetIdx = cards.findIndex((c) => c.id === payload.targetId);

        if (sourceIdx < 0) {
          log.warn(`[kanban] add-relation: source card "${payload.sourceId}" not found`);
          return;
        }
        if (targetIdx < 0) {
          log.warn(`[kanban] add-relation: target card "${payload.targetId}" not found`);
          return;
        }

        const sourceCard = cards[sourceIdx];
        const targetCard = cards[targetIdx];

        // Add relation to source card (avoid duplicates)
        const sourceRelation: CardRelation = { targetCardId: payload.targetId, type: payload.type };
        const sourceRelations = sourceCard.relations ?? [];
        const alreadyOnSource = sourceRelations.some(
          (r) => r.targetCardId === payload.targetId && r.type === payload.type,
        );
        if (!alreadyOnSource) {
          cards[sourceIdx] = {
            ...sourceCard,
            relations: [...sourceRelations, sourceRelation],
            updatedAt: Date.now(),
          };
        }

        // Add inverse relation to target card (avoid duplicates)
        const inverseType = inverseRelationType(payload.type);
        const inverseRelation: CardRelation = { targetCardId: payload.sourceId, type: inverseType };
        const targetRelations = targetCard.relations ?? [];
        const alreadyOnTarget = targetRelations.some(
          (r) => r.targetCardId === payload.sourceId && r.type === inverseType,
        );
        if (!alreadyOnTarget) {
          cards[targetIdx] = {
            ...targetCard,
            relations: [...targetRelations, inverseRelation],
            updatedAt: Date.now(),
          };
        }

        await boardStateStore.saveCards(cards);

        // Determine board for the event
        const boardId = cards[sourceIdx].boardId;
        if (boardId) {
          const data = await boardConfigStore.load();
          const board = data.boards.find((b) => b.id === boardId);
          if (board) {
            sendToRenderer("kanban:board-updated", {
              boardId: board.id,
              cards: cards.filter((c) => c.boardId === boardId),
              columns: board.columns,
            });
          }
        }

        log.info(`[kanban] Added relation: ${payload.sourceId} -[${payload.type}]-> ${payload.targetId}`);
      } catch (err) {
        log.error(`[kanban] add-relation error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  ipcMain.handle(
    "kanban:remove-relation",
    async (_evt, payload: { sourceId: string; targetId: string; type: RelationType }): Promise<void> => {
      try {
        const cards = await boardStateStore.getCards();
        const sourceIdx = cards.findIndex((c) => c.id === payload.sourceId);
        const targetIdx = cards.findIndex((c) => c.id === payload.targetId);

        if (sourceIdx < 0) {
          log.warn(`[kanban] remove-relation: source card "${payload.sourceId}" not found`);
          return;
        }
        if (targetIdx < 0) {
          log.warn(`[kanban] remove-relation: target card "${payload.targetId}" not found`);
          return;
        }

        const sourceCard = cards[sourceIdx];
        const targetCard = cards[targetIdx];

        // Remove relation from source card
        const sourceRelations = sourceCard.relations ?? [];
        cards[sourceIdx] = {
          ...sourceCard,
          relations: sourceRelations.filter(
            (r) => !(r.targetCardId === payload.targetId && r.type === payload.type),
          ),
          updatedAt: Date.now(),
        };

        // Remove inverse relation from target card
        const inverseType = inverseRelationType(payload.type);
        const targetRelations = targetCard.relations ?? [];
        cards[targetIdx] = {
          ...targetCard,
          relations: targetRelations.filter(
            (r) => !(r.targetCardId === payload.sourceId && r.type === inverseType),
          ),
          updatedAt: Date.now(),
        };

        await boardStateStore.saveCards(cards);

        // Determine board for the event
        const boardId = cards[sourceIdx].boardId;
        if (boardId) {
          const data = await boardConfigStore.load();
          const board = data.boards.find((b) => b.id === boardId);
          if (board) {
            sendToRenderer("kanban:board-updated", {
              boardId: board.id,
              cards: cards.filter((c) => c.boardId === boardId),
              columns: board.columns,
            });
          }
        }

        log.info(`[kanban] Removed relation: ${payload.sourceId} -[${payload.type}]-> ${payload.targetId}`);
      } catch (err) {
        log.error(`[kanban] remove-relation error: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
  );

  // ─── Main → Renderer Event Relay Helpers ────────────────────────────────

  function emitBoardUpdated(payload: { boardId: string; cards: KanbanCard[]; columns: KanbanColumn[] }) {
    sendToRenderer("kanban:board-updated", payload);
  }

  function emitCardMoved(payload: { cardId: string; fromColumn: string; toColumn: string }) {
    sendToRenderer("kanban:card-moved", payload);
  }

  function emitPipelineStatus(payload: { cardId: string; pipelineState: string; detail?: string }) {
    sendToRenderer("kanban:pipeline-status", payload);
  }

  function emitRuleExecuted(payload: { ruleId: string; cardId: string; status: string }) {
    sendToRenderer("kanban:rule-executed", payload);
  }

  function emitSyncConflict(payload: { cardId: string; localState: string; remoteState: string }) {
    sendToRenderer("kanban:sync-conflict", payload);
  }

  function emitAuthExpired(payload: { trackerType: string }) {
    sendToRenderer("kanban:auth-expired", payload);
  }

  function emitNotification(payload: { type: string; title: string; body: string; cardId?: string }) {
    sendToRenderer("kanban:notification", payload);
  }

  void symphonyRuntimeManager.autostartEnabledBoards().catch((err) => {
    log.warn(`[kanban] Symphony autostart failed: ${err instanceof Error ? err.message : String(err)}`);
  });

  return {
    emitBoardUpdated,
    emitCardMoved,
    emitPipelineStatus,
    emitRuleExecuted,
    emitSyncConflict,
    emitAuthExpired,
    emitNotification,
  };
}
