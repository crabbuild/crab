import React from "react";
import { createRoot } from "react-dom/client";
import { StoreProvider } from "../../src/state";
import { SessionProvider } from "../../src/lib/session-context";
import { SettingsProvider } from "../../src/settings";
import { AgentTasksView } from "../../src/views/AgentTasksView";
import { ToastManager } from "../../src/components/ToastManager";
import "../../src/index.css";

const liveBridge = new URLSearchParams(location.search).get("liveWorkspace");
const invoke = (operation: string, body: object = {}) => fetch(`${liveBridge}/${operation}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }).then(response => response.json());
const liveListeners = new Map<string, Set<(payload: unknown) => void>>();
if (liveBridge) {
  const source = new EventSource(`${liveBridge}/events`);
  source.onopen = () => { window.workspaceStreamReady = true; };
  source.onerror = () => { window.workspaceStreamReady = false; };
  source.onmessage = event => {
    const message = JSON.parse(event.data) as { channel: string; payload: unknown };
    for (const listener of liveListeners.get(message.channel) ?? []) listener(message.payload);
  };
}
function listen<T>(channel: string, listener: (payload: T) => void): () => void {
  const listeners = liveListeners.get(channel) ?? new Set<(payload: unknown) => void>();
  const receive = (payload: unknown) => listener(payload as T);
  listeners.add(receive); liveListeners.set(channel, listeners);
  return () => { listeners.delete(receive); };
}

type Status = Parameters<Parameters<Window["crab"]["orchestrator"]["onTaskStatus"]>[0]>[0];
type Budget = Parameters<Parameters<Window["crab"]["orchestrator"]["onBudgetWarning"]>[0]>[0];
type Switch = Parameters<Parameters<Window["crab"]["orchestrator"]["onNonconcurrentStop"]>[0]>[0];
const statuses = new Set<(payload: Status) => void>();
const budgets = new Set<(payload: Budget) => void>();
const switches = new Set<(payload: Switch) => void>();
const task = { id: "workspace-task", title: "Workspace fixture", agentType: "claude-code", status: "archived", createdAt: 1, prompt: "Saved prompt", worktreePath: "/fixture/old" };
const calls: string[] = [];
const modelRequests: Array<{ cwd: string; taskId?: string }> = [];
const fixture = {
  calls, modelRequests,
  failArchive: false,
  failDiscard: false,
  status: (status: Status["status"]) => { task.status = status; statuses.forEach(listener => listener({ taskId: task.id, status })); },
  budget: () => budgets.forEach(listener => listener({ taskId: task.id, cost: 12, limit: 10, workspaceTitle: task.title })),
  switched: () => switches.forEach(listener => listener({ stoppedTaskId: task.id, startedTaskId: "another-task" })),
};
declare global { interface Window { workspaceFixture: typeof fixture; workspaceStreamReady?: boolean } }
window.workspaceFixture = fixture;
const subscribe = <T,>(listeners: Set<(payload: T) => void>, listener: (payload: T) => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };
const idle = () => () => {};
Object.defineProperty(window, "crab", { value: {
  onEvent: idle,
  updateMainSettings: async () => {},
  setDevtools: async () => {},
  agentSessions: async () => [],
  agentWorkflows: { listDefinitions: async () => ({ success: true, definitions: [] }), listRuns: async () => ({ success: true, runs: [] }) },
  orchestrator: {
    listTasks: async () => { calls.push("list"); return { success: true, tasks: [{ ...task }] }; },
    detectAgents: async () => ({ success: true, cached: true, agents: [{ type: "claude-code", name: "Claude Code", command: "claude", available: true }] }),
    loadChat: async () => ({ success: true, messages: [], revision: 0 }),
    onChatUpdate: idle, onError: idle, onEvent: idle,
    onTaskStatus: (listener: (payload: Status) => void) => subscribe(statuses, listener),
    onBudgetWarning: (listener: (payload: Budget) => void) => subscribe(budgets, listener),
    onNonconcurrentStop: (listener: (payload: Switch) => void) => subscribe(switches, listener),
    listModels: async (_agent: string, cwd: string, _requestId: string, taskId?: string) => { modelRequests.push({ cwd, taskId }); return { success: true, models: [] }; },
    cancelModelDiscovery: async () => {},
    listCheckpoints: async () => ({ success: true, checkpoints: [], backups: [] }),
    onCheckpointCreated: idle,
    onFilesChanged: idle,
    getTaskUIState: async () => ({ success: true, state: null }),
    getDiff: async () => ({ success: true, diff: { taskId: task.id, baseRef: "fixture-base", headRef: "HEAD", files: [], rawDiff: "", totalAdditions: 0, totalDeletions: 0 } }),
    getCommits: async () => ({ success: true, commits: [] }),
    getGoal: async () => ({ success: true, goal: null }),
    restoreTask: async () => { calls.push("restore"); task.worktreePath = "/fixture/restored"; fixture.status("reviewing"); return { success: true }; },
    retryTask: async () => { calls.push("retry"); fixture.status("running"); return { success: true }; },
    archiveTask: async () => {
      calls.push("archive");
      if (fixture.failArchive) return { success: false, error: "Snapshot unavailable" };
      fixture.status("archived"); return { success: true };
    },
    discard: async () => {
      calls.push("discard");
      if (fixture.failDiscard) return { success: false, error: "Workspace is locked" };
      fixture.status("discarded"); return { success: true };
    },
    ...(liveBridge ? {
      listTasks: () => invoke("listTasks"),
      loadChat: (taskId: string) => invoke("loadChat", { taskId }),
      iterate: (taskId: string, feedback: string, options?: object, messageId?: string) => invoke("iterate", { taskId, feedback, options, messageId }),
      archiveTask: (taskId: string) => invoke("archiveTask", { taskId }),
      restoreTask: (taskId: string) => invoke("restoreTask", { taskId }),
      listCheckpoints: (taskId: string) => invoke("listCheckpoints", { taskId }),
      restoreFiles: (taskId: string, sourceRef: string) => invoke("restoreFiles", { taskId, sourceRef }),
      revertFile: (taskId: string, filePath: string) => invoke("revertFile", { taskId, filePath }),
      retryTask: (taskId: string) => invoke("retryTask", { taskId }),
      startReview: (taskId: string) => invoke("startReview", { taskId }),
      applyAll: (taskId: string) => invoke("applyAll", { taskId }),
      runWorkspace: (payload: { taskId: string }) => invoke("runWorkspace", payload),
      getDiff: (taskId: string, checkpointSha?: string, checkpointFromSha?: string) => invoke("getDiff", { taskId, checkpointSha, checkpointFromSha }),
      getGoal: (taskId: string) => invoke("getGoal", { taskId }),
      setGoal: (taskId: string, mutation: import("../../electron/orchestrator-adapters").CodexGoalMutation) => invoke("setGoal", { taskId, mutation }),
      clearGoal: (taskId: string) => invoke("clearGoal", { taskId }),
      onTaskStatus: (listener: Parameters<Window["crab"]["orchestrator"]["onTaskStatus"]>[0]) => listen("orchestrator:task-status", listener),
      onChatUpdate: (listener: Parameters<Window["crab"]["orchestrator"]["onChatUpdate"]>[0]) => listen("orchestrator:chat-update", listener),
      onEvent: (listener: Parameters<Window["crab"]["orchestrator"]["onEvent"]>[0]) => listen("orchestrator:event", listener),
      onCheckpointCreated: (listener: Parameters<Window["crab"]["orchestrator"]["onCheckpointCreated"]>[0]) => listen("orchestrator:checkpoint-created", listener),
      onFilesChanged: (listener: Parameters<Window["crab"]["orchestrator"]["onFilesChanged"]>[0]) => listen("orchestrator:files-changed", listener),
      onError: (listener: Parameters<Window["crab"]["orchestrator"]["onError"]>[0]) => listen("orchestrator:error", listener),
    } : {}),
  },
} });
createRoot(document.getElementById("root")!).render(<StoreProvider><SettingsProvider><SessionProvider>
  <ToastManager />
  <AgentTasksView repo={{ repo_id: "fixture-repo", url: "fixture", is_remote: false, cache_dir: "", label: "Fixture", openedAt: 1, deferred: true }} />
</SessionProvider></SettingsProvider></StoreProvider>);
