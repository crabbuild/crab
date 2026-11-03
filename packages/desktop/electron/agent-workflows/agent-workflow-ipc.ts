import type { BrowserWindow, IpcMain } from "electron";

import type { Orchestrator } from "../orchestrator";
import type {
  AgentWorkflowDefinition,
  AgentWorkflowRetryNodeOptions,
  AgentWorkflowStartRunOptions,
  AgentWorkflowTrigger,
} from "./types";
import {
  AgentWorkflowDefinitionStore,
  AgentWorkflowRunStore,
  AgentWorkflowTriggerStore,
} from "./persistence";
import { AgentWorkflowRunner } from "./runner";
import { AgentWorkflowKanbanProjection } from "./kanban-projection";
import { validateAgentWorkflowDefinition } from "./validator";
import { AgentWorkflowTriggerScheduler } from "./trigger-scheduler";
import { AgentWorkflowWebhookServer } from "./webhook-server";

export function registerAgentWorkflowIpc(
  ipcMain: IpcMain,
  getMainWindow: () => BrowserWindow | null,
  getOrchestrator: () => Orchestrator | null,
): AgentWorkflowRunner {
  const definitionStore = new AgentWorkflowDefinitionStore();
  const runStore = new AgentWorkflowRunStore();
  const triggerStore = new AgentWorkflowTriggerStore();
  const kanbanProjection = new AgentWorkflowKanbanProjection();
  const orchestrator = getOrchestrator();

  if (!orchestrator) {
    throw new Error("Cannot register Agent Workflow IPC before Orchestrator is initialized.");
  }

  const runner = new AgentWorkflowRunner(orchestrator, runStore, definitionStore);
  const triggerScheduler = new AgentWorkflowTriggerScheduler(definitionStore, triggerStore, runStore, runner);

  runner.on("run-updated", (run) => {
    getMainWindow()?.webContents.send("agent-workflow:run-updated", run);
    kanbanProjection.upsertRunProjection(run)
      .then((projection) => {
        if (!projection) return;
        getMainWindow()?.webContents.send("kanban:board-updated", projection);
      })
      .catch((err: unknown) => {
        const message = errorMessage(err);
        console.error("[agent-workflow] kanban projection failed:", err);
        getMainWindow()?.webContents.send("agent-workflow:projection-error", {
          runId: run.id,
          message,
        });
      });
  });
  runner.on("node-event", (event) => {
    getMainWindow()?.webContents.send("agent-workflow:node-event", event);
  });
  triggerScheduler.on("trigger-event", (event) => {
    getMainWindow()?.webContents.send("agent-workflow:trigger-event", event);
  });

  const webhookServer = new AgentWorkflowWebhookServer(triggerStore, definitionStore, runner);
  webhookServer.start();

  runner.recoverActiveRuns()
    .catch((err: unknown) => {
      console.error("[agent-workflow] recovery failed:", err);
    })
    .finally(() => {
      triggerScheduler.start();
    });

  ipcMain.handle("agent-workflow:list-definitions", async (_evt, payload: { repoPath: string }) => {
    try {
      return { success: true, definitions: await definitionStore.list(payload.repoPath) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), definitions: [] };
    }
  });

  ipcMain.handle("agent-workflow:get-definition", async (_evt, payload: { repoPath: string; definitionId: string }) => {
    try {
      return { success: true, definition: await definitionStore.get(payload.repoPath, payload.definitionId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), definition: null };
    }
  });

  ipcMain.handle("agent-workflow:save-definition", async (_evt, payload: { repoPath: string; definition: AgentWorkflowDefinition }) => {
    try {
      const definition = await definitionStore.save(payload.repoPath, payload.definition);
      return { success: true, definition };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:delete-definition", async (_evt, payload: { repoPath: string; definitionId: string }) => {
    try {
      await definitionStore.delete(payload.repoPath, payload.definitionId);
      return { success: true };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:list-versions", async (_evt, payload: { definitionId: string }) => {
    try {
      return { success: true, versions: await definitionStore.listVersions(payload.definitionId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), versions: [] };
    }
  });

  ipcMain.handle("agent-workflow:get-version", async (_evt, payload: { definitionId: string; version: number }) => {
    try {
      const definition = await definitionStore.getVersion(payload.definitionId, payload.version);
      return { success: true, definition };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), definition: null };
    }
  });

  ipcMain.handle("agent-workflow:revert-to-version", async (_evt, payload: { repoPath: string; definitionId: string; version: number }) => {
    try {
      const definition = await definitionStore.getVersion(payload.definitionId, payload.version);
      if (!definition) throw new Error(`Version ${payload.version} not found.`);
      const saved = await definitionStore.save(payload.repoPath, definition);
      return { success: true, definition: saved };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:simulate-run", async (_evt, payload: { definition: AgentWorkflowDefinition; input?: Record<string, unknown> }) => {
    try {
      return { success: true, result: await runner.simulateRun(payload.definition, payload.input) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:validate-definition", async (_evt, payload: { definition: AgentWorkflowDefinition }) => {
    try {
      return validateAgentWorkflowDefinition(payload.definition);
    } catch (err: unknown) {
      return { valid: false, errors: [errorMessage(err)], warnings: [], topologicalNodeIds: [] };
    }
  });

  ipcMain.handle("agent-workflow:start-run", async (_evt, payload: {
    repoPath: string;
    definitionId: string;
    options?: AgentWorkflowStartRunOptions;
  }) => {
    try {
      const definition = await definitionStore.get(payload.repoPath, payload.definitionId);
      if (!definition) throw new Error(`Workflow definition not found: ${payload.definitionId}.`);
      const options = { ...(payload.options ?? {}) };
      let sourceTask: unknown;
      if (options.createSourceTask && !options.sourceTaskId) {
        const task = await orchestrator.createWorkflowRootTask(
          payload.repoPath,
          options.sourceTaskTitle ?? `Workflow: ${definition.name}`,
          typeof options.input?.prompt === "string" ? options.input.prompt : `Run workflow ${definition.name}`,
          definition.id,
        );
        options.sourceTaskId = task.id;
        sourceTask = task;
      }
      const run = await runner.startRun(payload.repoPath, definition, options);
      if (options.sourceTaskId) {
        sourceTask = await orchestrator.attachWorkflowRunToTask(options.sourceTaskId, run.id) ?? sourceTask;
      }
      return { success: true, run, sourceTask };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:get-run", async (_evt, payload: { runId: string }) => {
    try {
      return { success: true, run: await runner.getRun(payload.runId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), run: null };
    }
  });

  ipcMain.handle("agent-workflow:list-runs", async (_evt, payload: { repoPath?: string }) => {
    try {
      return { success: true, runs: await runner.listRuns(payload.repoPath) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), runs: [] };
    }
  });

  ipcMain.handle("agent-workflow:list-events", async (_evt, payload: { runId: string }) => {
    try {
      return { success: true, events: await runner.listEvents(payload.runId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), events: [] };
    }
  });

  ipcMain.handle("agent-workflow:cancel-run", async (_evt, payload: { runId: string }) => {
    try {
      return { success: true, run: await runner.cancelRun(payload.runId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:pause-run", async (_evt, payload: { runId: string }) => {
    try {
      return { success: true, run: await runner.pauseRun(payload.runId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:resume-run", async (_evt, payload: { runId: string }) => {
    try {
      return { success: true, run: await runner.resumeRun(payload.runId) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:approve-node", async (_evt, payload: { runId: string; nodeRunId: string; summary?: string }) => {
    try {
      return { success: true, run: await runner.approveNode(payload.runId, payload.nodeRunId, payload.summary) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:request-node-changes", async (_evt, payload: { runId: string; nodeRunId: string; feedback: string }) => {
    try {
      return { success: true, run: await runner.requestNodeChanges(payload.runId, payload.nodeRunId, payload.feedback) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:reject-node", async (_evt, payload: { runId: string; nodeRunId: string; reason: string }) => {
    try {
      return { success: true, run: await runner.rejectNode(payload.runId, payload.nodeRunId, payload.reason) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:request-human-review-changes", async (_evt, payload: { runId: string; nodeRunId: string; feedback: string }) => {
    try {
      return { success: true, run: await runner.requestHumanReviewChanges(payload.runId, payload.nodeRunId, payload.feedback) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:retry-node", async (_evt, payload: { runId: string; nodeRunId: string; options?: AgentWorkflowRetryNodeOptions }) => {
    try {
      return { success: true, run: await runner.retryNode(payload.runId, payload.nodeRunId, payload.options) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:list-triggers", async (_evt, payload: { repoPath?: string }) => {
    try {
      return { success: true, triggers: await triggerStore.list(payload.repoPath) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err), triggers: [] };
    }
  });

  ipcMain.handle("agent-workflow:save-trigger", async (_evt, payload: { trigger: AgentWorkflowTrigger }) => {
    try {
      return { success: true, trigger: await triggerStore.save(payload.trigger) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:delete-trigger", async (_evt, payload: { triggerId: string }) => {
    try {
      await triggerStore.delete(payload.triggerId);
      return { success: true };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  ipcMain.handle("agent-workflow:set-trigger-enabled", async (_evt, payload: { triggerId: string; enabled: boolean }) => {
    try {
      return { success: true, trigger: await triggerStore.setEnabled(payload.triggerId, payload.enabled) };
    } catch (err: unknown) {
      return { success: false, error: errorMessage(err) };
    }
  });

  return runner;
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
