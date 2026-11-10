/**
 * Task Lifecycle Manager for the Crab Desktop Orchestrator.
 *
 * Manages the full lifecycle of agent tasks: create → start → running →
 * reviewing → applied/discarded/failed. Persists task metadata to disk,
 * handles timeouts, and emits events for the renderer to consume.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import {
  AgentAdapter,
  AgentSubmissionUncertainError,
  AgentEvent,
  AgentType,
  AgentTurnOptions,
  AgentInputResponse,
  CodexGoal,
  CodexGoalMutation,
  createAdapter,
} from "./orchestrator-adapters";
import { CodexAppServerAdapter } from "./codex-app-server-adapter";
import { ClaudeCodeAdapter } from "./claude-code-adapter";
import { agentRegistry } from "./orchestrator-registry";
import { loadAgentConfig, migrateSettings } from "./orchestrator-settings";
import { buildAgentModelFlags } from "./orchestrator-steering";
import { CheckpointManager } from "./orchestrator-checkpoints";
import { remoteAgentProcessFactory } from "./ssh-native-process";
import { AgentProcessClosureError, WorkspaceCommandCancelledError, type AgentProcessFactory } from "./agent-process";
import { editHunk as editWorkspaceHunk, type HunkPatch } from "./orchestrator-hunks";
import { applyAll, cherryPickFiles, computeDiff, type ApplyResult } from "./orchestrator-diff";
import { createWorktree, removeWorktree, restoreWorktree, readTaskMetadata, migrateWorktreeMetadata } from "./orchestrator-worktree";
import { WorkspaceScripts, WorkspaceScriptCancelledError, type WorkspaceScriptEvent, type WorkspaceScriptOptions, type ScriptResult } from "./orchestrator-scripts";
import { parseRepoId } from "./ssh-router";
import type { RpcRouter } from "./ssh-router";
import { sameRemoteEndpoint, type RemoteEndpoint } from "./ssh-types";
import { confirmRemoteTaskOperation } from "./ssh-task-outcome";
import {
  RemoteScriptRunner,
  loadRemoteScripts,
  buildRemoteWorkspaceEnv,
  type RemoteScriptResult,
} from "./ssh-remote-scripts";
import { OrchestratorTaskDatabase, sqlitePathForPersistPath } from "./orchestrator-task-db";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface PersistedChatUIState {
  cardCollapse?: Record<string, { collapsed: boolean; lastToggledMs: number }>;
  groupCollapse?: Record<string, { collapsed: boolean; lastToggledMs: number }>;
  turnCollapse?: Record<number, { collapsed: boolean; lastToggledMs: number }>;
  compactView?: boolean;
  timelineRail?: "compact" | "off";
  vimNavigation?: boolean;
}

export type TaskStatus =
  | "created"
  | "queued"
  | "planning"
  | "running"
  | "reviewing"
  | "iterating"
  | "applied"
  | "discarded"
  | "failed"
  | "stopped"
  | "interrupted"
  | "archived";

export type TaskWorktreeMode = "auto" | "primary" | "existing";

type WorkspaceSetupState = "pending" | "running" | "ready" | "skipped" | "failed";

type TaskInputKind = "message" | "review" | "goal";
type InputIntent = {
  id: string;
  content: string;
  kind: TaskInputKind;
  options?: AgentTurnOptions;
  goal?: CodexGoalMutation;
};

type TaskInput = InputIntent & {
  phase: "queued" | "prepared" | "unsent";
  turnNumber: number;
  sha?: string;
  outcome?: "accepted" | "unconfirmed" | "rejected";
  previousUnsent?: InputIntent;
};

type TaskCompletion = { id: string; event: AgentEvent };

export interface Task {
  id: string;
  title?: string;
  prompt: string;
  agentType: AgentType;
  status: TaskStatus;
  branch: string;
  worktreePath: string;
  /** How the task workspace was chosen. Auto worktrees are disposable. */
  worktreeMode?: TaskWorktreeMode;
  baseRef: string;
  /** The branch name that was checked out when the task was created. */
  baseBranch?: string;
  repoPath: string;
  createdAt: number;
  startedAt?: number;
  completedAt?: number;
  exitCode?: number;
  events: AgentEvent[];
  feedback: string[];
  todos: Array<{ id: string; text: string; done: boolean; source: "user" | "agent" | "comment" }>;
  /** PR metadata if a PR has been created. */
  prUrl?: string;
  prNumber?: number;
  /** Latest accepted or unconfirmed follow-up checkpoint number. */
  turnNumber?: number;
  pendingInput?: TaskInput;
  lastInputKind?: TaskInputKind;
  pendingCompletions?: TaskCompletion[];
  workspaceSetup?: WorkspaceSetupState;
  goal?: CodexGoal | null;
  /** True when the task targets a remote repo (worktree lives on remote). */
  isRemote?: boolean;
  /** SSH profile ID for remote tasks. */
  remoteProfileId?: string;
  /** Verified destination; absent on older tasks until explicit host recovery. */
  remoteEndpoint?: RemoteEndpoint;
  /** Durable native thread identity used by app-server resume. */
  codexThreadId?: string;
  claudeSessionId?: string;
  /** Per-task chat UI state (collapse/expand, compact view, etc.). */
  uiState?: PersistedChatUIState;
  /** Agent Workflow run ID associated with this task. */
  workflowRunId?: string;
  /** Agent Workflow node run ID associated with this task. */
  workflowNodeRunId?: string;
  /** If true, successful completion enters planning review instead of implementation review. */
  planFirst?: boolean;
  /** Optional per-task model override passed to the agent CLI. */
  model?: string;
  /** Optional per-task reasoning/intelligence override passed to the agent CLI. */
  reasoningLevel?: string;
  /** Sidebar pin timestamp. Pinned tasks sort before normal recents. */
  pinnedAt?: number;
  /** Parent task when this chat was spawned as a child/sub-agent. */
  parentTaskId?: string;
  /** Root task for nested sub-agent trees. */
  rootTaskId?: string;
  /** Agent row ID that spawned this child task. */
  spawnedByAgentId?: string;
  /** Role supplied when this child task was spawned. */
  spawnedRole?: string;
  /** Last meaningful activity timestamp for sidebar ordering. */
  lastActivityAt?: number;
  /** Optional dense-sidebar display label override. */
  sidebarLabel?: string;
  /** Derived child task IDs, populated before returning to the renderer. */
  childTaskIds?: string[];
  /** Optional explicit sidebar group key for future saved/grouped views. */
  sidebarGroup?: string;
}

/** Serialisable subset of Task persisted to disk (excludes events). */
export interface PersistedTask {
  id: string;
  title?: string;
  prompt: string;
  agentType: AgentType;
  status: TaskStatus;
  branch: string;
  worktreePath: string;
  worktreeMode?: TaskWorktreeMode;
  baseRef: string;
  baseBranch?: string;
  repoPath: string;
  createdAt: number;
  startedAt?: number;
  completedAt?: number;
  exitCode?: number;
  feedback: string[];
  todos: Array<{ id: string; text: string; done: boolean; source: "user" | "agent" | "comment" }>;
  prUrl?: string;
  prNumber?: number;
  turnNumber?: number;
  pendingInput?: TaskInput;
  lastInputKind?: TaskInputKind;
  pendingCompletions?: TaskCompletion[];
  workspaceSetup?: WorkspaceSetupState;
  goal?: CodexGoal | null;
  isRemote?: boolean;
  remoteProfileId?: string;
  remoteEndpoint?: RemoteEndpoint;
  codexThreadId?: string;
  claudeSessionId?: string;
  uiState?: PersistedChatUIState;
  workflowRunId?: string;
  workflowNodeRunId?: string;
  planFirst?: boolean;
  model?: string;
  reasoningLevel?: string;
  pinnedAt?: number;
  parentTaskId?: string;
  rootTaskId?: string;
  spawnedByAgentId?: string;
  spawnedRole?: string;
  lastActivityAt?: number;
  sidebarLabel?: string;
  sidebarGroup?: string;
}

function sameTaskHost(left: Task, right: Task): boolean {
  if (Boolean(left.isRemote) !== Boolean(right.isRemote)) return false;
  if (!left.isRemote) return true;
  // A legacy task without a verified destination cannot grant another task
  // a writer lease merely because its path and editable profile ID match.
  if (!left.remoteEndpoint || !right.remoteEndpoint) return left.id === right.id && left.remoteProfileId === right.remoteProfileId && left.remoteEndpoint === right.remoteEndpoint;
  return sameRemoteEndpoint(left.remoteEndpoint, right.remoteEndpoint);
}

function sameWorkspace(left: Task, right: Task): boolean {
  return left.worktreePath === right.worktreePath && sameTaskHost(left, right);
}

function branchNameFromBaseRef(baseRef: string): string | undefined {
  if (baseRef === "HEAD" || /^[0-9a-f]{7,40}$/i.test(baseRef)) {
    return undefined;
  }
  return baseRef.replace(/^refs\/heads\//, "");
}

export interface OrchestratorOptions {
  /**
   * Path to the SQLite persistence database.
   *
   * Older callers may still pass an agent-tasks.json path; that path is
   * normalized to agent-tasks.sqlite. JSON storage is no longer read or written.
   */
  persistPath?: string;
  /** Task timeout in milliseconds. Defaults to 30 minutes. */
  timeoutMs?: number;
  /** Maximum number of concurrently running tasks. Defaults to 3. */
  maxConcurrent?: number;
  /** RPC router for routing commands to remote agents (SSH). */
  rpcRouter?: RpcRouter;
}

export interface CreateTaskOptions {
  worktreeMode?: TaskWorktreeMode;
  worktreePath?: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_PERSIST_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "agent-tasks.sqlite",
);
const DEFAULT_TIMEOUT_MS = 30 * 60 * 1000; // 30 minutes
const DEFAULT_MAX_CONCURRENT = 3;
function buildTaskAgentArgs(agentType: AgentType, task: Task, configuredArgs?: string[]): string[] {
  return [...(configuredArgs ?? []), ...(["codex", "claude-code"].includes(agentType) ? [] : buildAgentModelFlags(agentType, task))];
}

function applyTaskConfiguration(task: Task, configuration: AgentTurnOptions): void {
  if (configuration.model !== undefined) task.model = configuration.model ?? undefined;
  if (configuration.reasoningLevel !== undefined) task.reasoningLevel = configuration.reasoningLevel ?? undefined;
  if (configuration.mode !== undefined) task.planFirst = configuration.mode === "plan";
}

// ─── Orchestrator ────────────────────────────────────────────────────────────

export type TaskSubmission = { taskId: string; id: string; content: string } & (
  | { state: "pending" | "accepted" | "rejected" }
  | { state: "unconfirmed"; error: string }
);

type TaskStartup = { promise: Promise<void>; controller: AbortController; prompt: string; kind: TaskInputKind; goal?: CodexGoalMutation; configuration?: AgentTurnOptions; dispatched?: boolean };
type CheckpointJob = { task: Task; kind: "write" | "query"; controller: AbortController; promise: Promise<unknown>; failure?: AgentProcessClosureError };
type SetupResult = ScriptResult & { skipped?: boolean };
type SetupWaiter = { taskId: string; controller: AbortController; promise: Promise<SetupResult> };
type SetupJob = { id: string; owner: Task; controller: AbortController; waiters: Set<SetupWaiter>; promise: Promise<SetupResult>; output: string };
type PendingSubmission = { kind: TaskInputKind; controller: AbortController; settled: Promise<void> };

export class Orchestrator extends EventEmitter {
  private tasks = new Map<string, Task>();
  private adapters = new Map<string, AgentAdapter>();
  private retiringAdapters = new Set<AgentAdapter>();
  private startups = new Map<string, TaskStartup>();
  private admissionQueue = new Map<string, { resume: () => void; reject: (error: Error) => void }>();
  private reservedTasks = new Map<string, object>();
  private closingTasks = new Set<string>();
  private cancellations = new Map<string, Promise<void>>();
  private shutdownPromise?: Promise<void>;
  private workspaceOperations = new Set<Promise<unknown>>();
  private workspaceSetups = new Map<string, SetupJob>();
  private workspaceScripts = new WorkspaceScripts(event => this.workspaceScriptEvent(event));
  private pendingSubmissions = new Map<string, Set<PendingSubmission>>();
  private postTurnSnapshots = new Map<string, Promise<void>>();
  private inputRecovery = new Map<string, Promise<void>>();
  private goalReaders = new Map<string, Set<CodexAppServerAdapter>>();
  private goalReads = new Map<string, Promise<CodexGoal | null>>();
  private goalControls = new Map<string, { promise: Promise<unknown>; controller: AbortController; activation: boolean }>();
  private goalRevisions = new Map<string, number>();
  private goalCompletions = new Map<string, AgentEvent>();
  private completionJournalAttached = false;
  private workspaceTransitions = new Map<Task, "removed" | "restored" | "archived" | "applied" | "closed" | "updated">();
  private timeouts = new Map<string, ReturnType<typeof setTimeout>>();
  private checkpointJobs = new Set<CheckpointJob>();
  private checkpointClosing = false;
  private readonly timeoutMs: number;
  private readonly maxConcurrent: number;
  private readonly taskDb: OrchestratorTaskDatabase;
  private rpcRouter: RpcRouter | undefined;
  private remoteScriptRunner: RemoteScriptRunner | undefined;

  constructor(options?: OrchestratorOptions) {
    super();
    const persistPath = options?.persistPath ?? DEFAULT_PERSIST_PATH;
    this.taskDb = new OrchestratorTaskDatabase(sqlitePathForPersistPath(persistPath));
    this.timeoutMs = options?.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.maxConcurrent = options?.maxConcurrent ?? DEFAULT_MAX_CONCURRENT;
    this.rpcRouter = options?.rpcRouter;
    if (this.rpcRouter) {
      this.remoteScriptRunner = new RemoteScriptRunner(this.rpcRouter);
      this.wireRemoteScriptEvents();
    }
    // Run the one-shot legacy settings migration before loading any tasks
    // so per-agent binary overrides are available for every spawn that follows.
    try {
      migrateSettings();
    } catch {
      // Migration is best-effort — failures shouldn't block startup.
    }
    this.loadTasks();
  }

  // ─── Public API ──────────────────────────────────────────────────────────

  /**
   * Creates a new agent task: generates an ID, resolves the workspace,
   * persists the task, and returns the task object.
   *
   * For remote repos (repo_id starts with `ssh://`), worktree creation
   * is routed to the remote agent via the SSH router. The worktree
   * lives on the remote filesystem.
   */
  async createTask(
    prompt: string,
    agentType: AgentType,
    repoPath: string,
    baseRef?: string,
    options: CreateTaskOptions = {},
  ): Promise<Task> {
    return this.workspaceOperation(() => this.createTaskAttempt(prompt, agentType, repoPath, baseRef, options));
  }

  private async createTaskAttempt(prompt: string, agentType: AgentType, repoPath: string, baseRef: string | undefined, options: CreateTaskOptions): Promise<Task> {
    const id = crypto.randomUUID();
    const requestedMode = options.worktreeMode ?? "auto";

    // Detect if this is a remote repo.
    const remoteInfo = parseRepoId(repoPath);
    if (remoteInfo) {
      return this.createRemoteTask(id, prompt, agentType, repoPath, remoteInfo, baseRef, options);
    }

    let base = await this.resolveBaseRef(repoPath, baseRef ?? "HEAD");
    let baseBranch = await this.getCurrentBranch(repoPath);
    let worktreeMode: TaskWorktreeMode = requestedMode;
    let worktreePath: string;
    let branch: string;

    if (requestedMode === "primary") {
      if (baseRef && baseRef !== "HEAD") {
        await this.checkoutBranch(repoPath, baseRef);
        baseBranch = await this.getCurrentBranch(repoPath);
        base = await this.resolveBaseRef(repoPath, "HEAD");
      }
      worktreePath = repoPath;
      branch = baseBranch ?? baseRef ?? "primary";
    } else if (requestedMode === "existing") {
      if (!options.worktreePath) {
        throw new Error("Existing worktree mode requires a worktree path.");
      }
      worktreePath = path.resolve(options.worktreePath);
      if (!fs.existsSync(worktreePath)) {
        throw new Error(`Selected worktree path does not exist: ${worktreePath}`);
      }
      branch = await this.getCurrentBranch(worktreePath)
        ?? await this.resolveHeadRef(worktreePath)
        ?? path.basename(worktreePath);
    } else {
      worktreeMode = "auto";
      worktreePath = await createWorktree(repoPath, id, base);
      // Read the friendly name from metadata to get the actual branch name.
      const meta = readTaskMetadata(worktreePath);
      branch = meta?.originalBranch ?? `agent/${id}`;
    }

    if (worktreeMode !== "primary") {
      this.ensureContextDirectory(worktreePath);
    }

    const now = Date.now();
    const task: Task = {
      id,
      prompt,
      agentType,
      status: "created",
      branch,
      worktreePath,
      worktreeMode,
      workspaceSetup: worktreeMode === "auto" ? "pending" : undefined,
      baseRef: base,
      baseBranch,
      repoPath,
      createdAt: now,
      lastActivityAt: now,
      events: [],
      feedback: [],
      todos: [],
    };

    if (worktreeMode === "primary") {
      task.baseRef = await this.checkpointOperation(task, manager => manager.captureBase(task));
    }

    this.tasks.set(id, task);
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: id, status: task.status });
    return task;
  }

  /**
   * Creates a task for a remote repo by routing worktree creation to
   * the remote agent via the SSH router. The worktree lives on the
   * remote filesystem, not locally.
   */
  private async createRemoteTask(
    id: string,
    prompt: string,
    agentType: AgentType,
    repoPath: string,
    remoteInfo: { profileId: string; remotePath: string },
    baseRef?: string,
    options: CreateTaskOptions = {},
  ): Promise<Task> {
    if (!this.rpcRouter) {
      throw new Error(
        "Cannot create task for remote repo: SSH router not configured. " +
        "Ensure SSH remote connections are enabled.",
      );
    }

    const router = this.rpcRouter;
    const remoteEndpoint = router.remoteEndpoint(remoteInfo.profileId);
    const owner = router.remoteChannelOwner(remoteInfo.profileId);
    if (!owner) throw new Error("Remote task requires an attached SSH agent");
    let base = baseRef ?? "HEAD";
    const worktreeMode = options.worktreeMode ?? "auto";
    let branch = `agent/${id}`;
    let worktreePath: string;

    if (worktreeMode === "primary") {
      branch = "primary";
      worktreePath = remoteInfo.remotePath;
    } else if (worktreeMode === "existing") {
      if (!options.worktreePath) {
        throw new Error("Existing worktree mode requires a worktree path.");
      }
      branch = "existing";
      worktreePath = options.worktreePath;
    } else {
      // Route worktree creation to the remote agent.
      try {
        const result = await router.requestOnRemoteChannel(remoteInfo.profileId, owner, "worktree_create", {
          repo_id: repoPath,
          branch,
          base_ref: base,
          task_id: id,
        }) as { path: string; branch: string; base_ref: string };

        if (!result || typeof result.path !== "string" || !result.path.startsWith("/")
          || typeof result.branch !== "string" || !result.branch
          || typeof result.base_ref !== "string" || !/^[a-f0-9]{40}$/.test(result.base_ref)) {
          throw new Error("Remote workspace creation returned an invalid path, branch or baseline");
        }
        worktreePath = result.path;
        branch = result.branch;
        base = result.base_ref;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        throw new Error(
          `Failed to create worktree on remote (${remoteInfo.profileId}): ${message}`,
        );
      }
    }

    const now = Date.now();
    const task: Task = {
      id,
      prompt,
      agentType,
      status: "created",
      branch,
      worktreePath,
      worktreeMode,
      baseRef: base,
      baseBranch: branchNameFromBaseRef(baseRef ?? "HEAD"),
      repoPath,
      createdAt: now,
      lastActivityAt: now,
      events: [],
      feedback: [],
      todos: [],
      isRemote: true,
      remoteProfileId: remoteInfo.profileId,
      remoteEndpoint,
    };

    if (worktreeMode !== "auto") task.baseRef = await this.checkpointOperation(task, manager => manager.captureBase(task));
    this.tasks.set(id, task);
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: id, status: task.status });
    return task;
  }

  /**
   * Sets the RPC router for remote repo support. Can be called after
   * construction when the SSH subsystem initialises later.
   */
  setRpcRouter(router: RpcRouter): void {
    this.rpcRouter = router;
    this.remoteScriptRunner = new RemoteScriptRunner(router);
    this.wireRemoteScriptEvents();
  }

  /**
   * Wires up event listeners from the remote script runner to the
   * orchestrator's event system so the renderer can display script
   * output in the activity feed.
   */
  private wireRemoteScriptEvents(): void {
    if (!this.remoteScriptRunner) return;

    this.remoteScriptRunner.on("script:start", (payload) => {
      this.emit("orchestrator:event", {
        taskId: payload.taskId,
        event: {
          type: "script_start",
          timestamp: Date.now(),
          scriptType: payload.scriptType,
          command: payload.command,
        },
      });
    });

    this.remoteScriptRunner.on("script:output", (payload) => {
      this.emit("orchestrator:event", {
        taskId: payload.taskId,
        event: {
          type: "script_output",
          timestamp: Date.now(),
          scriptType: payload.scriptType,
          stream: payload.type,
          data: payload.data,
        },
      });
    });

    this.remoteScriptRunner.on("script:complete", (payload) => {
      this.emit("orchestrator:event", {
        taskId: payload.taskId,
        event: {
          type: "script_complete",
          timestamp: Date.now(),
          scriptType: payload.scriptType,
          success: payload.result.success,
          exitCode: payload.result.exitCode,
          error: payload.result.error,
        },
      });
    });
  }

  /**
   * Returns the RPC router, or undefined if not configured.
   * Used by diff computation to route remote diff requests.
   */
  getRpcRouter(): RpcRouter | undefined {
    return this.rpcRouter;
  }

  /** Captures the task's host and disposable workspace owner for native I/O. */
  agentProcessFactory(task: Task): AgentProcessFactory | undefined {
    if (!task.isRemote) return undefined;
    const host = this.remoteTaskHost(task);
    return remoteAgentProcessFactory(host.router, host.profileId, this.ownedWorkspaceTask(task)?.id);
  }

  private remoteTaskHost(task: Task): { router: RpcRouter; profileId: string; owner: object } {
    const router = this.rpcRouter;
    const profileId = task.remoteProfileId;
    if (!router || !profileId || parseRepoId(task.repoPath)?.profileId !== profileId) throw new Error("Remote task repository endpoint is unavailable");
    router.assertRemoteEndpoint(profileId, task.remoteEndpoint);
    // Capture before awaiting writer shutdown. A reconnect cannot redirect
    // an accepted Apply or Discard to a replacement agent channel.
    const owner = router.remoteChannelOwner(profileId);
    if (!owner) throw new Error("Remote task requires an attached SSH agent");
    return { router, profileId, owner };
  }

  /** Binds checkpoint I/O to the saved workspace and current SSH channel. */
  private checkpointManager(task: Task, signal: AbortSignal): CheckpointManager {
    if (!task.isRemote) return new CheckpointManager(undefined, signal);
    if (!this.rpcRouter || !task.remoteProfileId) throw new Error("Remote checkpoints require a connected remote agent");
    // Capture both factories once: reconnect cannot splice one snapshot across
    // channels, and repository refs remain accessible after worktree removal.
    const workspace = this.agentProcessFactory(task)!;
    const repository = remoteAgentProcessFactory(this.rpcRouter, task.remoteProfileId);
    return new CheckpointManager((saved, cwd) => cwd === saved.worktreePath ? workspace : repository, signal);
  }

  /** Queries refs without granting a lost reader ownership of workspace writes. */
  queryCheckpoints<T>(task: Task, operation: (manager: Pick<CheckpointManager, "listCheckpoints" | "listRestoreBackups">) => Promise<T>): Promise<T> {
    this.assertAcceptingWork();
    return this.checkpointOperation(task, operation, undefined, "query");
  }

  private async checkpointOperation<T>(task: Task, operation: (manager: CheckpointManager) => Promise<T>, signal?: AbortSignal, kind: CheckpointJob["kind"] = "write"): Promise<T> {
    if (this.checkpointClosing) throw new WorkspaceCommandCancelledError();
    signal?.throwIfAborted();
    const controller = new AbortController();
    const manager = this.checkpointManager(task, controller.signal);
    const abort = () => controller.abort(signal?.reason);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    const promise = Promise.resolve().then(() => operation(manager));
    const job: CheckpointJob = { task, kind, controller, promise };
    this.checkpointJobs.add(job);
    try { return await promise; } catch (error: unknown) {
      // An unconfirmed process can still write. Keep its failed receipt as a
      // workspace fence even after the turn/job has otherwise finished.
      if (error instanceof AgentProcessClosureError) job.failure = error;
      throw error;
    } finally {
      signal?.removeEventListener("abort", abort);
      if (!job.failure) this.checkpointJobs.delete(job);
    }
  }

  private async waitCheckpointJobs(tasks?: Task[]): Promise<void> {
    const jobs = [...this.checkpointJobs].filter(job => !tasks || job.kind === "write" && tasks.some(task => task.id === job.task.id));
    const results = await Promise.allSettled(jobs.map(job => job.promise));
    const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected" && result.reason instanceof AgentProcessClosureError);
    if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Workspace command closure is unconfirmed");
  }

  /**
   * Starts a task: spawns the agent adapter in the worktree, wires
   * event listeners, sets up timeout, and updates status to "running".
   * If the max concurrent limit is reached, queues the task instead.
   *
   * Remote tasks use the same native adapters over an owned SSH transport.
   * Provider events and input receipts share the local task pipeline.
   *
   * @param skipCheckpoint — When true, skip checkpoint creation (caller
   *   already created one, e.g. `iterate()`).
   */
  async startTask(taskId: string, { skipCheckpoint = false, prompt, kind, goal, configuration }: { skipCheckpoint?: boolean; prompt?: string; kind?: TaskInputKind; goal?: CodexGoalMutation; configuration?: AgentTurnOptions } = {}): Promise<void> {
    this.assertAcceptingWork();
    await this.recoverTaskInput(taskId);
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }
    this.assertWorkspaceAvailable(task);
    if (task.status !== "created" && task.status !== "iterating" && task.status !== "queued") {
      throw new Error(
        `Cannot start task in status "${task.status}". Expected "created", "iterating", or "queued".`,
      );
    }
    if (this.closingTasks.has(taskId)) throw new Error("Agent shutdown is still in progress");
    const existing = this.startups.get(taskId);
    if (existing) {
      if (task.status !== "queued") throw new Error("Agent startup is already in progress");
      await existing.promise;
      return;
    }
    let resolveStartup!: () => void;
    let rejectStartup!: (error: unknown) => void;
    const promise = new Promise<void>((resolve, reject) => { resolveStartup = resolve; rejectStartup = reject; });
    const startup: TaskStartup = { promise, controller: new AbortController(), prompt: prompt ?? task.feedback.at(-1) ?? task.prompt, kind: kind ?? task.lastInputKind ?? "message", goal, configuration };
    // Error listeners can synchronously request a follow-up. Publish the
    // pending startup before launching so they cannot bypass its completion.
    this.startups.set(taskId, startup);
    void this.startTaskAttempt(task, skipCheckpoint, startup).then(resolveStartup, rejectStartup);
    try { await startup.promise; } finally {
      if (this.startups.get(taskId) === startup) this.startups.delete(taskId);
    }
  }

  private async startTaskAttempt(task: Task, skipCheckpoint: boolean, startup: TaskStartup): Promise<void> {
    const taskId = task.id;
    const eventOffset = task.events.length;
    let releaseAdmission: (() => void) | undefined;
    try {
      const capabilities = agentRegistry.getCapabilities(task.agentType);
      const plan = startup.configuration?.mode !== undefined ? startup.configuration.mode === "plan" : task.planFirst;
      if (plan && !capabilities.supportsPlanMode) throw new Error(`Agent "${task.agentType}" does not support plan mode.`);
      await this.retireAdapter(taskId);
      if (!["created", "iterating", "queued"].includes(task.status)) throw new Error("Task startup was cancelled");
      releaseAdmission = await this.admitTask(task);
      await this.launchTask(task, skipCheckpoint, startup);
    } catch (error: unknown) {
      this.clearTimeout(taskId);
      const preserveQueued = this.shutdownPromise && !startup.dispatched && !task.pendingInput && startup.kind !== "goal"
        && ["created", "queued", "running", "iterating"].includes(task.status);
      if (preserveQueued) task.status = "queued";
      const cancelled = Boolean(preserveQueued) || ["stopped", "discarded", "archived", "applied"].includes(task.status);
      try {
        if (!cancelled && !task.events.slice(eventOffset).some(event => event.type === "done")) {
          task.status = "failed";
          task.completedAt = Date.now();
          task.exitCode = 1;
          const events: AgentEvent[] = [
            { type: "error", timestamp: Date.now(), error: error instanceof Error ? error.message : String(error) },
            { type: "done", timestamp: Date.now(), outcome: "failed", content: "" },
          ];
          for (const event of events) {
            task.events.push(event);
            this.publishTaskEvent(task, event);
          }
          await this.persistTasks();
          this.emit("orchestrator:task-status", { taskId, status: task.status });
        } else if (!cancelled && (task.status === "running" || task.status === "iterating")) {
          // A saved active goal may keep the last done event nonterminal. A
          // failed startup still owns no usable provider after retirement.
          task.status = "failed";
          task.completedAt = Date.now();
          task.exitCode = 1;
          await this.persistTasks();
          this.emit("orchestrator:task-status", { taskId, status: task.status });
        }
      } finally {
        // Persistence and renderer failures cannot leave a half-started CLI
        // alive. Shutdown must finish before another start can own this task.
        await this.retireAdapter(taskId);
        if (startup.kind === "goal" && (task.goal?.status === "active" || error instanceof AgentSubmissionUncertainError)) await this.pauseStoppedGoal(task);
      }
      throw error;
    } finally {
      releaseAdmission?.();
    }
  }

  private publishTaskEvent(task: Task, event: AgentEvent): void {
    const taskId = task.id;
    let completionId: string | undefined;
    let completed = false;
    if (event.type === "done" && (task.agentType === "codex" || task.agentType === "claude-code")) {
      completionId = crypto.randomUUID();
      task.pendingCompletions = [...(task.pendingCompletions ?? []), { id: completionId, event }];
      try { completed = this.completeTurnFromDoneEvent(task, taskId, event); } catch (err: unknown) {
        this.emit("orchestrator:error", {
          taskId,
          message: `Failed to complete agent turn: ${err instanceof Error ? err.message : String(err)}`,
        });
        void this.cancelTask(taskId).catch(() => {});
        return;
      }
    }
    this.emit("orchestrator:event", { taskId, event, ...(completionId ? { completionId } : {}) });
    if (completed) {
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      this.processQueue();
    }
  }

  private async launchTask(task: Task, skipCheckpoint: boolean, startup: TaskStartup): Promise<void> {
    const taskId = task.id;
    this.goalCompletions.delete(taskId);
    task.status = "running";
    task.completedAt = undefined;
    task.exitCode = undefined;
    task.startedAt = Date.now();
    this.emit("orchestrator:task-status", { taskId, status: task.status });
    this.setupTimeout(taskId);

    if (task.status !== "running") throw new Error("Task startup was cancelled");
    this.assertWorkspaceAvailable(task);
    startup.dispatched = true;
    await this.prepareWorkspace(task, startup.controller.signal);
    if (task.status !== "running") throw new Error("Task startup was cancelled");
    this.assertWorkspaceAvailable(task);

    // Capture a rollback baseline before constructing any provider owner.
    // Continuing after a Git failure would misrepresent recoverable changes.
    this.migrateWorkspaceMetadata(task);
    task.baseRef = await this.checkpointOperation(task, manager => manager.pinBase(task), startup.controller.signal);
    if (!skipCheckpoint) {
      const turnNumber = task.turnNumber ?? 0;
      const checkpoint = await this.checkpointOperation(task, manager => turnNumber === 0
        ? manager.createLightweightCheckpoint(task, turnNumber)
        : manager.createCheckpoint(task, turnNumber), startup.controller.signal);
      this.emit("orchestrator:checkpoint-created", { taskId, turnNumber, ...checkpoint });
    }
    if (task.status !== "running") throw new Error("Task startup was cancelled");
    this.assertWorkspaceAvailable(task);
    this.assertLocalWorkspace(task);

    // Load per-agent configuration (binary path, extra args, env vars)
    // from settings so users can configure each agent independently.
    if (task.isRemote && !["codex", "claude-code"].includes(task.agentType)) throw new Error("Remote conversations require Codex or Claude Code");
    const agentConfig: ReturnType<typeof loadAgentConfig> = task.isRemote ? {} : loadAgentConfig(task.agentType);
    const processFactory = this.agentProcessFactory(task);
    // Requested settings belong to this startup until native acknowledgement.
    // A shallow view also supplies custom agents' model flags without changing
    // the saved task when startup rejects a later control.
    const configuredTask = { ...task };
    if (startup.configuration) applyTaskConfiguration(configuredTask, startup.configuration);
    const adapter = createAdapter(
      task.agentType,
      agentConfig.binaryPath,
      undefined,
      buildTaskAgentArgs(task.agentType, configuredTask, agentConfig.extraArgs),
      { env: agentConfig.env, optOutNotifications: agentConfig.optOutNotifications, processFactory },
    );

    await this.persistTasks();

    // Wire event listener.
    adapter.onEvent((event: AgentEvent) => {
      if (this.adapters.get(taskId) !== adapter) return;
      if (this.retiringAdapters.has(adapter) && event.type !== "request_resolved") return;
      task.events.push(event);
      if (event.type === "goal_updated") this.updateTaskGoal(task, event.goal ?? null);
      if (event.type === "configuration_changed" && event.configuration) {
        // Claude acknowledges controls separately. Preserve accepted settings
        // even if a later control rejects the message submission.
        applyTaskConfiguration(task, event.configuration);
        if (event.turnId && event.configuration.mode && ["planning", "reviewing"].includes(task.status)) {
          // A fast Codex turn may complete before turn/start acknowledges its
          // settings. Reclassify only that completed turn, never an older one.
          const completion = [...task.events].reverse().find(entry => entry.type === "done");
          if (completion?.turnId === event.turnId && completion.outcome === "completed") {
            task.status = task.planFirst ? "planning" : "reviewing";
            this.emit("orchestrator:task-status", { taskId, status: task.status });
          }
        }
        this.persistTasks().catch(() => {});
      }
      if (event.type === "turn_started" && !["stopped", "discarded", "applied", "archived"].includes(task.status)) {
        this.goalCompletions.delete(taskId);
        task.status = "running";
        task.completedAt = undefined;
        task.exitCode = undefined;
        this.setupTimeout(taskId);
        this.persistTasks().catch(() => {});
        this.emit("orchestrator:task-status", { taskId, status: task.status });
      }
      // Capture the Codex thread id so subsequent iterations can resume.
      // We only set it once per task — the thread id is stable for the
      // life of the conversation. Re-runs that produce a new id (e.g.
      // after a resume failure) overwrite the old value.
      if (event.type === "thread_started" && event.threadId) {
        if (task.codexThreadId !== event.threadId) {
          task.codexThreadId = event.threadId;
          // Persist asynchronously; failures here are non-fatal.
          this.persistTasks().catch(() => {});
        }
      }
      if (event.type === "session_started" && event.sessionId && task.agentType === "claude-code") {
        task.claudeSessionId = event.sessionId;
        this.persistTasks().catch(() => {});
      }
      this.publishTaskEvent(task, event);
    });

    // Wire exit listener.
    adapter.onExit(async (code: number) => {
      if (this.adapters.get(taskId) !== adapter) return;
      if (this.retiringAdapters.has(adapter)) {
        this.adapters.delete(taskId);
        return;
      }
      this.clearTimeout(taskId);
      this.adapters.delete(taskId);
      if (task.status === "stopped") return;

      const alreadyCompletedFromTurn =
        (task.status === "reviewing" || task.status === "planning" || task.status === "failed")
        && Boolean(task.completedAt)
        && typeof task.exitCode === "number";
      if (alreadyCompletedFromTurn) {
        return;
      }

      task.exitCode = code;
      task.completedAt = Date.now();

      // Transition to the appropriate terminal status.
      // If the task was marked as planFirst, transition to "planning" on
      // success so the user can review the plan before approving.
      if (task.status !== "planning") {
        if (code === 0 && task.planFirst) {
          task.status = "planning";
        } else {
          task.status = code === 0 ? "reviewing" : "failed";
        }
      } else if (code !== 0) {
        task.status = "failed";
      }

      this.schedulePostTurnSnapshot(task, taskId);
      await this.persistTasks().catch(() => {});
      this.emit("orchestrator:task-status", { taskId, status: task.status });

      // Start any queued tasks now that a slot is free.
      this.processQueue();

    });

    // Spawn the agent with per-agent env vars injected (task-specific
    // secrets from Settings go in via `adapter.spawn(env)`).
    //
    // Codex resume optimisation: if we've previously captured a thread
    // id for this task, ask the adapter to resume it. This avoids
    // re-uploading the prior prompt and keeps the prompt cache warm.
    // A failed resume retains the saved identity and surfaces the error;
    // silently starting another thread would discard conversation context.
    if (adapter instanceof CodexAppServerAdapter && task.codexThreadId) {
      adapter.resumeThreadId = task.codexThreadId;
    }
    if (adapter instanceof ClaudeCodeAdapter && task.claudeSessionId) {
      adapter.resumeSessionId = task.claudeSessionId;
    }
    if (task.status !== "running") throw new Error("Task startup was cancelled");
    this.assertWorkspaceAvailable(task);
    this.adapters.set(taskId, adapter);
    const options: AgentTurnOptions = {
      model: task.model, reasoningLevel: task.reasoningLevel,
      ...(task.planFirst ? { mode: "plan" } : {}),
      ...startup.configuration,
    };
    startup.dispatched = true;
    if (startup.kind === "goal") {
      if (!(adapter instanceof CodexAppServerAdapter)) throw new Error("Goals require Codex");
      const mutation = startup.goal ?? { status: "active" };
      await adapter.startGoal(mutation, task.worktreePath, agentConfig.env);
      await this.settleGoalActivation(task, adapter);
    } else if (startup.kind === "review" && adapter.review) await adapter.review(startup.prompt, task.worktreePath, agentConfig.env, options);
    else await adapter.spawn(startup.prompt, task.worktreePath, agentConfig.env, options);

  }

  /**
   * Runs the workspace archive script on the remote machine for a task.
   * Called when a task is applied or discarded to allow cleanup.
   */
  async runRemoteArchiveScript(taskId: string): Promise<RemoteScriptResult | null> {
    const task = this.tasks.get(taskId);
    if (!task || !task.isRemote || !task.remoteProfileId) return null;
    if (!this.rpcRouter || !this.remoteScriptRunner) return null;

    const remoteInfo = parseRepoId(task.repoPath);
    if (!remoteInfo) return null;

    const defaultScripts = { setupScript: "", runScript: "", archiveScript: "" };
    const scripts = await loadRemoteScripts(
      this.rpcRouter,
      task.remoteProfileId,
      task.repoPath,
      remoteInfo.remotePath,
      defaultScripts,
    );

    if (!scripts.archiveScript.trim()) {
      return null;
    }

    const env = buildRemoteWorkspaceEnv(
      task.worktreePath,
      task.branch,
      remoteInfo.remotePath,
      task.id,
    );

    return this.remoteScriptRunner.runArchiveScript(
      task.id,
      task.remoteProfileId,
      task.worktreePath,
      scripts.archiveScript,
      env,
    );
  }

  /**
   * Returns the remote script runner instance (for handling notifications).
   */
  getRemoteScriptRunner(): RemoteScriptRunner | undefined {
    return this.remoteScriptRunner;
  }

  /**
   * Cancels a running task: sends SIGTERM to the agent process,
   * updates status to "stopped", preserves worktree for inspection.
   * Waits for the native owner on either workspace host.
   */
  async cancelTask(taskId: string): Promise<void> {
    const existing = this.cancellations.get(taskId);
    if (existing) return existing;
    let resolve!: () => void;
    let reject!: (error: unknown) => void;
    const cancellation = new Promise<void>((done, fail) => { resolve = done; reject = fail; });
    // Publish before signalling: synchronous process events can request Stop
    // again, and app quit must join that same physical closure.
    this.cancellations.set(taskId, cancellation);
    void this.cancelTaskAttempt(taskId).then(resolve, reject);
    try { await cancellation; } finally {
      if (this.cancellations.get(taskId) === cancellation) this.cancellations.delete(taskId);
    }
  }

  private async cancelTaskAttempt(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    this.clearTimeout(taskId);
    this.goalCompletions.delete(taskId);

    // Mark the user decision before signalling the process: a synchronous
    // terminal notification must not replace stopped with an interrupted turn.
    task.status = "stopped";
    task.completedAt = Date.now();
    const startup = this.startups.get(taskId);
    const submissions = [...(this.pendingSubmissions.get(taskId) ?? [])].map(input => input.settled);
    startup?.controller.abort(new WorkspaceCommandCancelledError());
    const preparation = this.setupWaiters(taskId).map(waiter => this.settleSetupWaiter(waiter));
    this.cancelPendingSubmissions(taskId);
    this.cancelQueuedAdmission(taskId);

    this.closingTasks.add(taskId);
    try {
      const adapter = this.adapters.get(taskId);
      const closeAdapter = async () => {
        if (!adapter) return;
        await adapter.cancel();
        if (this.adapters.get(taskId) === adapter) this.adapters.delete(taskId);
      };
      // Join the enclosing jobs as well as their current process. Cancellation
      // can still owe private-index cleanup or accepted-input publication.
      const results = await Promise.allSettled([closeAdapter(), ...preparation, ...submissions,
        startup?.promise.catch(() => {}), this.closeGoalReaders(taskId),
        this.goalControls.get(taskId)?.promise.catch(() => {})]);
      const checkpoints = await Promise.allSettled([this.postTurnSnapshots.get(taskId), this.inputRecovery.get(taskId), this.waitCheckpointJobs([task])]);
      const errors = [...results, ...checkpoints].filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (errors.length === 1) throw errors[0].reason;
      if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Task closure is unconfirmed");
      await this.pauseStoppedGoal(task);
    } finally {
      this.closingTasks.delete(taskId);
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      this.processQueue();
    }
  }

  /**
   * Sends feedback to a task and re-spawns the agent for iteration.
   * Follow-ups reuse the live native connection or resume its saved session
   * which re-spawns the agent CLI in the same worktree with the
   * feedback as additional context.
   */
  async iterate(taskId: string, feedback: string, options?: AgentTurnOptions, messageId: string = crypto.randomUUID()): Promise<void> {
    await this.submitInput(taskId, { id: messageId, content: feedback, kind: "message", options });
  }

  private async submitInput(taskId: string, input: InputIntent, signal?: AbortSignal): Promise<void> {
    this.assertAcceptingWork();
    const intent = { ...input, options: input.options ? { ...input.options } : undefined, goal: input.goal ? { ...input.goal } : undefined };
    const { id: messageId, content: feedback, kind } = intent;
    await this.recoverTaskInput(taskId);
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.assertWorkspaceAvailable(task);
    const pending = this.pendingSubmissions.get(taskId) ?? new Set<PendingSubmission>();
    const previousSubmissions = [...pending].map(submission => submission.settled);
    const cancellation = new AbortController();
    const abort = () => cancellation.abort(signal?.reason);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    let settle!: () => void;
    const entry = { kind, controller: cancellation, settled: new Promise<void>(resolve => { settle = resolve; }) };
    pending.add(entry);
    this.pendingSubmissions.set(taskId, pending);
    const submission = { taskId, id: messageId, content: feedback };
    try {
      try {
        // Every caller publishes intent before dispatch. The synchronous journal
        // listener can reject a send that cannot be saved, including internal work.
        this.emit("orchestrator:submission", { ...submission, state: "pending" } satisfies TaskSubmission);
        // Serialize bookkeeping as well as native writes. Otherwise a rejected
        // message can roll back counters or status belonging to a later send.
        await Promise.all(previousSubmissions);
        cancellation.signal.throwIfAborted();
        await this.iterateTask(taskId, intent, cancellation.signal);
      } catch (error: unknown) {
        if (task.pendingInput?.id === messageId && task.pendingInput.phase === "unsent" && !task.pendingInput.outcome) {
          // Preparation never reached the provider. Retain this exact input
          // for explicit Retry rather than replaying the prior accepted turn.
          try {
            await this.persistTasks();
            const message = error instanceof Error ? error.message : String(error);
            const notice = `Message was not sent. ${message}. Use Retry to send it.`;
            this.emit("orchestrator:submission", { ...submission, state: "unconfirmed",
              error: notice } satisfies TaskSubmission);
          } catch (journalError: unknown) { this.reportInputRecoveryError(task, journalError); }
          throw error;
        }
        const result: TaskSubmission = error instanceof AgentSubmissionUncertainError
          ? { ...submission, state: "unconfirmed", error: error.message }
          : { ...submission, state: "rejected" };
        try { await this.settleTaskInput(task, result); } catch (journalError: unknown) {
          this.reportInputRecoveryError(task, journalError);
        }
        throw error;
      }
      // A journal failure after native acceptance must never remove this input
      // as though the provider rejected it.
      try {
        await this.settleTaskInput(task, { ...submission, state: "accepted" });
      } catch (error: unknown) {
        throw new AgentSubmissionUncertainError(error);
      }
    } finally {
      signal?.removeEventListener("abort", abort);
      pending.delete(entry);
      if (!pending.size && this.pendingSubmissions.get(taskId) === pending) this.pendingSubmissions.delete(taskId);
      settle();
    }
  }

  private async iterateTask(taskId: string, intent: InputIntent, signal: AbortSignal): Promise<void> {
    const { content: feedback, kind, options } = intent;
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }
    this.assertWorkspaceAvailable(task);
    if (this.closingTasks.has(taskId)) throw new Error("Agent shutdown is still in progress");
    let abortStartup!: () => void;
    const aborted = new Promise<never>((_, reject) => { abortStartup = () => reject(signal.reason); });
    signal.addEventListener("abort", abortStartup, { once: true });
    try { await Promise.race([this.startups.get(taskId)?.promise, aborted]); } catch (error: unknown) {
      if (signal.aborted) throw error;
      throw new Error("Previous agent startup failed; the new message was not sent", { cause: error });
    } finally {
      signal.removeEventListener("abort", abortStartup);
    }
    signal.throwIfAborted();
    this.assertWorkspaceAvailable(task);
    if (this.closingTasks.has(taskId)) throw new Error("Agent shutdown is still in progress");
    // Workflow and programmatic submissions share the composer's lifecycle
    // boundary; they must not recreate a closed workspace through iteration.
    if (task.status === "discarded") throw new Error("Discarded conversations are read-only. Start a new chat to continue.");
    if (task.status === "archived" || task.status === "applied") throw new Error("Reopen this conversation before sending a message.");
    const previous = { status: task.status, completedAt: task.completedAt, exitCode: task.exitCode };
    const wasRunning = this.hasActiveTurn(task);
    if (kind === "review" && !["codex", "claude-code"].includes(task.agentType)) throw new Error("Review requires a Codex or Claude Code conversation");
    if (kind === "review" && ["running", "iterating"].includes(task.status)) throw new Error("Wait for the current turn to finish before starting a review");
    if (kind === "goal" && (task.isRemote || task.agentType !== "codex" || !intent.goal)) throw new Error("Goal activation requires a local Codex conversation");
    const model = options?.model !== undefined && (options.model ?? undefined) !== task.model ? options.model : undefined;
    const mode = options?.mode !== undefined && (options.mode === "plan") !== Boolean(task.planFirst) ? options.mode : undefined;
    const reasoningLevel = options?.reasoningLevel !== undefined && (options.reasoningLevel ?? undefined) !== task.reasoningLevel ? options.reasoningLevel : undefined;
    const changesSettings = model !== undefined || mode !== undefined || reasoningLevel !== undefined;
    if (changesSettings && (task.status === "running" || task.status === "iterating")) {
      throw new Error("Wait for the current turn to complete before changing agent settings");
    }
    const turnOptions = changesSettings ? { ...(model !== undefined ? { model } : {}), ...(mode !== undefined ? { mode } : {}), ...(reasoningLevel !== undefined ? { reasoningLevel } : {}) } : undefined;


    // Reject iteration for agents that don't support it.
    const capabilities = agentRegistry.getCapabilities(task.agentType);
    if (!capabilities.supportsIteration) {
      throw new Error(`Agent "${task.agentType}" does not support iteration.`);
    }
    if (mode === "plan" && !capabilities.supportsPlanMode) throw new Error(`Agent "${task.agentType}" does not support plan mode.`);

    let accepted = false;
    let sha: string | undefined;
    let releaseAdmission: (() => void) | undefined;
    const turnNumber = (task.turnNumber ?? 0) + 1;
    const previousInput = task.pendingInput;
    const previousUnsent = previousInput?.phase === "unsent"
      ? { id: previousInput.id, content: previousInput.content, kind: previousInput.kind, options: previousInput.options, goal: previousInput.goal } : undefined;
    task.pendingInput = { ...intent, phase: "queued", turnNumber, previousUnsent };
    try {
      releaseAdmission = await this.admitTask(task, signal);
      task.status = "iterating";
      task.completedAt = undefined;
      task.exitCode = undefined;
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      try {
        await this.postTurnSnapshots.get(taskId);
        await this.prepareWorkspace(task, signal);
        signal.throwIfAborted();
        this.assertWorkspaceAvailable(task);
        {
          // Capture and pin on the workspace host before native dispatch.
          // Publish only after provider acceptance.
          sha = await this.checkpointOperation(task, async manager => {
            const snapshot = await manager.snapshotHead(task);
            signal.throwIfAborted();
            this.assertWorkspaceAvailable(task);
            await manager.holdCheckpoint(task, turnNumber, snapshot);
            return snapshot;
          }, signal);
        }
      } catch (error: unknown) {
        // Failed preparation owns no native input. Retain its exact text for
        // Retry rather than restoring an idle status and replaying old work.
        if (!signal.aborted && task.pendingInput?.id === intent.id && !["stopped", "archived", "applied", "discarded"].includes(task.status)) {
          const running = wasRunning && (["running", "iterating"].includes(task.status) || task.goal?.status === "active");
          task.status = running ? previous.status : "failed";
          task.completedAt = running ? previous.completedAt : Date.now();
          task.exitCode = running ? previous.exitCode : 1;
          if (task.pendingInput) {
            task.pendingInput.phase = "unsent";
            task.pendingInput.previousUnsent = undefined;
          }
          this.emit("orchestrator:task-status", { taskId, status: task.status });
        }
        throw error;
      }
      signal.throwIfAborted();
      this.assertWorkspaceAvailable(task);
      task.pendingInput.sha = sha;
      task.pendingInput.phase = "prepared";
      // Persist the exact input and its pinned snapshot before the native
      // write. A crash from here means unconfirmed delivery, never a resend.
      await this.persistTasks();
      signal.throwIfAborted();

      // Live adapter path: if the adapter is still running and exposes
      // iterate(), send the follow-up directly over the existing connection.
      // No prompt rewriting, no re-spawn, no task.prompt mutation.
      const adapter = this.adapters.get(taskId);
      if (adapter && typeof adapter.iterate === "function") {
        task.status = "running";
        task.completedAt = undefined;
        task.exitCode = undefined;
        await this.persistTasks();
        this.emit("orchestrator:task-status", { taskId, status: task.status });

        this.setupTimeout(taskId);
        if (kind === "goal") {
          if (!(adapter instanceof CodexAppServerAdapter) || !intent.goal) throw new Error("Goals require Codex");
          await adapter.setGoal(intent.goal);
          await this.settleGoalActivation(task, adapter);
        } else if (kind === "review" && adapter.review) await adapter.review(feedback, task.worktreePath, undefined, turnOptions);
        else await adapter.iterate(feedback, turnOptions);
        accepted = true;
        if (turnOptions) applyTaskConfiguration(task, turnOptions);

        return;
      }

      // A native session owns conversation context. Keep the original task
      // identity and send only this input when starting its next process.
      await this.startTask(taskId, { skipCheckpoint: true, prompt: feedback, kind, goal: intent.goal, configuration: turnOptions });
      accepted = true;
      if (turnOptions) applyTaskConfiguration(task, turnOptions);
    } catch (error: unknown) {
      if (error instanceof AgentSubmissionUncertainError) {
        accepted = true;
        if (task.pendingInput) task.pendingInput.outcome = "unconfirmed";
      }
      if (!accepted) {
        if (task.pendingInput && task.pendingInput.phase !== "unsent") task.pendingInput.outcome = "rejected";
        if (task.status === "running" || task.status === "iterating" || task.status === "queued") {
          task.status = previous.status;
          task.completedAt = previous.completedAt;
          task.exitCode = previous.exitCode;
          if (task.status !== "running" && task.status !== "iterating") this.clearTimeout(taskId);
          this.emit("orchestrator:task-status", { taskId, status: task.status });
        }
        await this.persistTasks().catch(() => {});
      }
      throw error;
    } finally {
      if (accepted) {
        if (task.pendingInput) task.pendingInput.outcome ??= "accepted";
      }
      releaseAdmission?.();
    }
  }

  /** Recovers interrupted input bookkeeping without sending native work. */
  async recoverTaskInput(taskId: string): Promise<void> {
    this.flushCompletions(taskId);
    const existing = this.inputRecovery.get(taskId);
    if (existing) return existing;
    const task = this.tasks.get(taskId);
    if (!task?.pendingInput || this.pendingSubmissions.has(taskId)) return;
    const recovery = Promise.resolve().then(async () => {
      const input = task.pendingInput;
      if (!input) return;
      if ((input.phase === "queued" || input.phase === "unsent") && !input.outcome) {
        if (input.phase === "queued" && ["stopped", "archived", "applied", "discarded"].includes(task.status)) input.outcome = "rejected";
        else {
          // A queued follow-up can target a turn that no longer exists. Keep
          // its intent for explicit Retry instead of replaying an older prompt.
          const queued = input.phase === "queued";
          if (queued) {
            if (input.sha) await this.checkpointOperation(task, manager => manager.releaseCheckpoint(task, input.turnNumber));
            input.phase = "unsent";
            input.previousUnsent = undefined;
            if (!this.adapters.has(taskId)) {
              task.status = "interrupted";
              task.completedAt = Date.now();
              task.exitCode = 1;
            }
            try { await this.persistTasks(); } catch (error: unknown) { input.phase = "queued"; throw error; }
          }
          this.emit("orchestrator:submission", { taskId, id: input.id, content: input.content, state: "unconfirmed",
            error: "Queued message was not sent before the connection closed. Use Retry to send it." } satisfies TaskSubmission);
          if (queued) this.emit("orchestrator:task-status", { taskId, status: task.status });
          return;
        }
      }
      if (input.phase !== "prepared" && !input.outcome) throw new Error("Unsupported pending input phase");
      input.outcome ??= "unconfirmed";
      // An existing native adapter keeps its slot on either workspace host.
      // Only records restored without a process can be marked interrupted.
      if (!this.adapters.has(taskId) && ["running", "iterating", "queued"].includes(task.status)) {
        task.status = "interrupted";
        task.completedAt = Date.now();
        task.exitCode = 1;
      }
      const submission: TaskSubmission = input.outcome === "unconfirmed"
        ? { taskId, id: input.id, content: input.content, state: "unconfirmed", error: "Connection closed before message delivery was confirmed" }
        : { taskId, id: input.id, content: input.content, state: input.outcome };
      if (!await this.settleTaskInput(task, submission)) throw new Error("Input bookkeeping remains incomplete");
      this.emit("orchestrator:task-status", { taskId, status: task.status });
    }).finally(() => { if (this.inputRecovery.get(taskId) === recovery) this.inputRecovery.delete(taskId); });
    this.inputRecovery.set(taskId, recovery);
    return recovery;
  }

  private async settleTaskInput(task: Task, submission: TaskSubmission): Promise<boolean> {
    const input = task.pendingInput;
    const matches = input?.id === submission.id;
    let finalized = true;
    if (input && matches && submission.state !== "pending") {
      input.outcome = submission.state;
      try { await this.finalizeTaskInput(task); } catch (error: unknown) {
        finalized = false;
        this.reportInputRecoveryError(task, error);
      }
    }
    // Journal acknowledgement precedes removal of the recovery record. Its
    // synchronous listener must throw if that acknowledgement cannot be saved.
    // Keep durable intent when its result could not be recorded. Removing a
    // rejected row here would leave restart with an unknown input but no text.
    const recorded: TaskSubmission = finalized || submission.state !== "rejected" ? submission : {
      taskId: submission.taskId, id: submission.id, content: submission.content, state: "unconfirmed",
      error: "Input result could not be saved. Check the conversation before retrying.",
    };
    this.emit("orchestrator:submission", recorded);
    if (finalized && matches) {
      try { await this.clearTaskInput(task); } catch (error: unknown) {
        finalized = false;
        this.reportInputRecoveryError(task, error);
      }
    }
    if (submission.state !== "rejected" && task.feedback.length >= 5) this.emit("orchestrator:iteration-warning", {
      taskId: task.id, iterations: task.feedback.length, message: "Consider breaking this into smaller tasks",
    });
    return finalized;
  }

  private async finalizeTaskInput(task: Task): Promise<void> {
    const input = task.pendingInput;
    if (!input) return;
    if (input.outcome !== "rejected" && input.turnNumber > (task.turnNumber ?? 0)) {
      task.feedback.push(input.content);
      task.turnNumber = input.turnNumber;
      task.lastInputKind = task.goal?.status === "active" ? "goal" : input.kind;
    }
    // Save the result and counter together before publishing. Replaying after
    // any later failure is idempotent and uses the original pinned snapshot.
    await this.persistTasks();
    if (input.sha && input.outcome !== "rejected") {
      const sha = input.sha;
      const { ref } = await this.checkpointOperation(task, manager => manager.recordCheckpoint(task, input.turnNumber, sha));
      this.emit("orchestrator:checkpoint-created", { taskId: task.id, turnNumber: input.turnNumber, sha: input.sha, ref });
    }
  }

  private async clearTaskInput(task: Task): Promise<void> {
    const input = task.pendingInput;
    if (!input) return;
    if (input.sha) await this.checkpointOperation(task, manager => manager.releaseCheckpoint(task, input.turnNumber));
    task.pendingInput = input.outcome === "rejected" && input.previousUnsent
      ? { ...input.previousUnsent, phase: "unsent", turnNumber: (task.turnNumber ?? 0) + 1 } : undefined;
    try { await this.persistTasks(); } catch (error: unknown) { task.pendingInput = input; throw error; }
  }

  private reportInputRecoveryError(task: Task, error: unknown): void {
    this.emit("orchestrator:error", { taskId: task.id, message: `Input bookkeeping is incomplete: ${error instanceof Error ? error.message : String(error)}` });
  }

  /**
   * Records the user's response to a Codex sandbox approval request.
   *
   * Delivers the decision directly over the wire via the live adapter's
   * `respondApproval()` method. Returns `false` when the task isn't
   * known or no live adapter exists (process died, app restarted).
   */
  async recordApproval(
    taskId: string,
    decision: { requestId: string; decision: string; summary: string; note?: string },
  ): Promise<boolean> {
    if (this.shutdownPromise) return false;
    const task = this.tasks.get(taskId);
    if (!task) return false;

    // Deliver the decision directly over the wire via the live adapter.
    const adapter = this.adapters.get(taskId);
    if (adapter && typeof adapter.respondApproval === "function") {
      return adapter.respondApproval(decision.requestId, decision.decision);
    }

    // No live adapter — can't deliver.
    return false;
  }

  recordInput(taskId: string, requestId: string, response: AgentInputResponse): boolean {
    if (this.shutdownPromise) return false;
    return this.adapters.get(taskId)?.respondInput?.(requestId, response) ?? false;
  }

  /** Drains workspace writers before applying or reverting an exact text edit. */
  async editHunk(taskId: string, patch: HunkPatch, operation: "apply" | "revert"): Promise<void> {
    return this.editFiles(taskId, task => editWorkspaceHunk(task.worktreePath, patch, operation));
  }

  /** Restores a live changed file to its baseline with a retained pre-edit backup. */
  async revertFile(taskId: string, filePath: string): Promise<void> {
    let oldPath: string | undefined;
    const validate = async (task: Task) => {
      const owner = this.ownedWorkspaceTask(task);
      if (owner) {
        this.migrateWorkspaceMetadata(task);
        if (owner.status === "discarded" || readTaskMetadata(owner.worktreePath)?.taskId !== owner.id) throw new Error("This directory no longer belongs to the saved workspace. Start a new chat for it.");
      }
      // Resolve rename sources from the live comparison, never renderer input.
      // Recheck after closure: a provider can finish changing files as it exits.
      const file = (await computeDiff(task)).files.find(file => file.path === filePath);
      if (!file) throw new Error("This file no longer has changes. Reload changes before reverting it.");
      oldPath = file.status === "renamed" ? file.oldPath : undefined;
    };
    return this.editFiles(taskId, task => this.checkpointOperation(task, manager => manager.revertFile(task, filePath, oldPath)), validate);
  }

  /** Restores a conversation's files while retaining its history and native identity. */
  async restoreFiles(taskId: string, sourceRef: string): Promise<void> {
    const validate = async (task: Task) => {
      const owner = this.ownedWorkspaceTask(task);
      if (!owner || owner.status === "discarded") throw new Error("Full file restoration requires a Crab-owned worktree");
      // Validate before cancelling providers, then recheck under writer closure.
      // Keep preflight inside the operation owner so quit drains it too.
      await this.checkpointOperation(task, manager => manager.snapshotSha(task, sourceRef));
      this.migrateWorkspaceMetadata(task);
      if (readTaskMetadata(owner.worktreePath)?.taskId !== owner.id) throw new Error("This directory no longer belongs to the saved workspace. Start a new chat for it.");
    };
    return this.editFiles(taskId, task => this.checkpointOperation(task, manager => manager.restoreFiles(task, sourceRef)), validate);
  }

  private async editFiles(taskId: string, change: (task: Task) => Promise<void>, validate?: (task: Task) => Promise<void>): Promise<void> {
    return this.workspaceOperation(async () => {
      await this.recoverTaskInput(taskId);
      const task = this.tasks.get(taskId);
      if (!task) throw new Error(`Task not found: ${taskId}`);
      if (task.isRemote) throw new Error("File edits require a local workspace");
      if (["archived", "applied", "discarded"].includes(task.status)) throw new Error("Reopen this conversation before editing files");
      this.assertWorkspaceAvailable(task);
      this.assertLocalWorkspace(task);
      this.workspaceTransitions.set(task, "updated");
      try {
        await validate?.(task);
        // A borrowed or primary-folder conversation can share its files with
        // other providers and Run scripts. Closing only this chat leaves writers.
        const conversations = await this.closeWorkspaceConversations(task, true);
        await this.workspaceScripts.close(task, true);
        await validate?.(task);
        await change(task);
        for (const conversation of conversations) conversation.lastActivityAt = Date.now();
        await this.persistTasks();
        this.emit("orchestrator:files-changed", { taskIds: conversations.map(conversation => conversation.id) });
      } finally { this.workspaceTransitions.delete(task); }
    });
  }

  /** Closes workspace writers, applies changes, and records the successful result. */
  async applyTask(taskId: string, files?: string[]): Promise<ApplyResult> {
    return this.workspaceOperation(() => this.applyTaskAttempt(taskId, files));
  }

  private async applyTaskAttempt(taskId: string, files?: string[]): Promise<ApplyResult> {
    this.assertCanApply(taskId);
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.workspaceTransitions.set(task, "applied");
    let cleanupWarning: string | undefined;
    try {
      if (task.isRemote) {
        if (files) throw new Error("Selective apply requires a local workspace");
        const host = this.remoteTaskHost(task);
        await this.closeWorkspaceConversations(task);
        if (this.ownsTaskWorktree(task)) {
          try {
            const result = await host.router.requestOnRemoteChannel(host.profileId, host.owner, "task_apply", {
              repo_id: task.repoPath, task_id: taskId, branch: task.branch,
              base_ref: task.baseRef, base_branch: task.baseBranch, worktree_path: task.worktreePath,
            });
            cleanupWarning = confirmRemoteTaskOperation(result, "apply");
          } catch (error: unknown) {
            throw new Error(`Remote apply failed: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
          }
        }
      } else {
        await this.closeWorkspaceConversations(task);
        // Apply reads the whole workspace even when its directory is borrowed.
        // A peer's dev server cannot keep changing files during snapshot/merge.
        await this.workspaceScripts.close(task, true);
        if (files) await cherryPickFiles(task, files);
        else {
          const result = await applyAll(task);
          if (!result.success) return result;
        }
      }
      task.status = "applied";
      task.completedAt = Date.now();
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      if (cleanupWarning) this.emit("orchestrator:error", { taskId, message: cleanupWarning });
      return { success: true };
    } finally {
      this.workspaceTransitions.delete(task);
    }
  }

  async retryTask(taskId: string): Promise<void> {
    this.assertAcceptingWork();
    await this.recoverTaskInput(taskId);
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.assertWorkspaceAvailable(task);
    if (this.closingTasks.has(taskId) || this.startups.has(taskId) || this.pendingSubmissions.has(taskId)) throw new Error("Wait for pending input, startup, or shutdown to finish before retrying");
    if (!["failed", "interrupted", "stopped"].includes(task.status)) throw new Error("Only a stopped or failed task can be retried");
    if (!task.isRemote && !fs.existsSync(task.worktreePath)) {
      const workspace = this.ownedWorkspaceTask(task);
      if (!workspace) this.assertLocalWorkspace(task);
      else await this.workspaceOperation(() => this.restoreWorkspace(workspace));
    }
    const owner = this.setupOwner(task);
    if (!this.workspaceSetups.has(task.worktreePath) && ["failed", "running"].includes(owner.workspaceSetup ?? "")) owner.workspaceSetup = "pending";
    const input = task.pendingInput;
    if (input?.phase === "unsent") {
      const intent = { id: crypto.randomUUID(), content: input.content, kind: input.kind, options: input.options, goal: input.goal };
      try { await this.submitInput(taskId, intent); } catch (error: unknown) {
        if (error instanceof AgentSubmissionUncertainError) this.emit("orchestrator:submission", { taskId, id: input.id, content: input.content, state: "rejected" } satisfies TaskSubmission);
        throw error;
      }
      this.emit("orchestrator:submission", { taskId, id: input.id, content: input.content, state: "rejected" } satisfies TaskSubmission);
      return;
    }
    const skipCheckpoint = await this.checkpointOperation(task, manager => manager.hasCheckpoint(task, task.turnNumber ?? 0));
    task.status = "created";
    task.completedAt = undefined;
    task.exitCode = undefined;
    await this.persistTasks();
    // Retain an accepted input's baseline; preparation can fail before the
    // first checkpoint, in which case Retry must capture one before dispatch.
    await this.startTask(taskId, { skipCheckpoint });
  }

  /** Archives a conversation, saving an owned workspace before removing it. */
  async archiveTask(taskId: string): Promise<void> {
    return this.workspaceOperation(() => this.archiveTaskAttempt(taskId));
  }

  private async archiveTaskAttempt(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.assertWorkspaceAvailable(task);
    if (task.status === "archived") return;
    if (task.isRemote) throw new Error("Remote workspace archiving requires remote agent support");
    this.workspaceTransitions.set(task, "archived");
    try {
      const conversations = await this.closeWorkspaceConversations(task);
      const ownsWorktree = this.ownsTaskWorktree(task);
      if (ownsWorktree && fs.existsSync(task.worktreePath)) {
        await this.workspaceScripts.archive(task, {});
        const branch = await this.getCurrentBranch(task.worktreePath);
        if (!branch || branch === "HEAD") throw new Error("Check out a local branch before archiving this workspace");
        await this.commitTurnSnapshot(task, taskId);
        for (const conversation of conversations) conversation.branch = branch;
      }
      if (ownsWorktree) await execFile("git", ["show-ref", "--verify", `refs/heads/${task.branch}`], { cwd: task.repoPath });
      const previous = { status: task.status, completedAt: task.completedAt, lastActivityAt: task.lastActivityAt };
      task.status = "archived";
      task.completedAt = Date.now();
      task.lastActivityAt = task.completedAt;
      try {
        // Save archive intent before removal. After a crash, Restore can reuse
        // the existing tree or check out the preserved branch without resending.
        await this.persistTasks();
        if (ownsWorktree) await removeWorktree(task, { preserveBranch: true });
        if (ownsWorktree) this.workspaceScripts.release(task);
      } catch (error: unknown) {
        Object.assign(task, previous);
        try { await this.persistTasks(); } catch (saveError: unknown) {
          throw new AggregateError([error, saveError], "Archiving failed and its task metadata could not be restored");
        }
        this.emit("orchestrator:task-status", { taskId, status: task.status });
        throw error;
      }
      this.emit("orchestrator:task-status", { taskId, status: task.status });
    } finally {
      this.workspaceTransitions.delete(task);
    }
  }

  /** Reopens an archived or applied conversation without resending its previous prompt. */
  async restoreTask(taskId: string): Promise<void> {
    return this.workspaceOperation(() => this.restoreTaskAttempt(taskId));
  }

  private async restoreTaskAttempt(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    if (task.status !== "archived" && task.status !== "applied") throw new Error("Only an archived or applied task can be reopened");
    await this.restoreWorkspace(task);
  }

  private async restoreWorkspace(task: Task): Promise<void> {
    if (task.isRemote) throw new Error("Remote workspace restoration requires remote agent support");
    if (task.status === "discarded") throw new Error("This workspace was discarded. Start a new chat to continue.");
    this.assertWorkspaceAvailable(task);
    this.workspaceTransitions.set(task, "restored");
    try {
      const conversations = await this.closeWorkspaceConversations(task);
      if (this.ownsTaskWorktree(task) && !fs.existsSync(task.worktreePath)) {
        const previousPath = task.worktreePath;
        const restoredPath = await restoreWorktree(task.repoPath, task.id, task.branch, previousPath);
        this.workspaceScripts.release(task);
        task.workspaceSetup = "pending";
        // Borrowing conversations follow the recreated owner. Retaining an
        // old path would resume their CLI in a workspace that no longer exists.
        for (const conversation of conversations) if (conversation.worktreePath === previousPath) conversation.worktreePath = restoredPath;
      } else if (!fs.existsSync(task.worktreePath)) {
        throw new Error("Workspace is missing. Restore its owner before this conversation.");
      }
      if (task.status === "archived" || task.status === "applied") task.status = "reviewing";
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
    } finally {
      this.workspaceTransitions.delete(task);
    }
  }

  /** Removes a task's owned workspace while preserving its conversation. */
  async discardTask(taskId: string, options: WorkspaceScriptOptions = {}): Promise<void> {
    return this.workspaceOperation(() => this.discardTaskAttempt(taskId, options));
  }

  private async discardTaskAttempt(taskId: string, options: WorkspaceScriptOptions): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    this.assertWorkspaceAvailable(task);
    const ownsWorktree = this.ownsTaskWorktree(task);
    // Borrowed conversations also need an admission barrier while their owners
    // retire, or a late follow-up can dispatch before discarded status is saved.
    this.workspaceTransitions.set(task, ownsWorktree ? "removed" : "closed");
    let cleanupWarning: string | undefined;
    try {
      const host = task.isRemote && ownsWorktree ? this.remoteTaskHost(task) : undefined;
      await this.closeWorkspaceConversations(task);

      // Remote tasks: route discard to the remote agent when Crab created
      // the disposable worktree. Primary/existing workspaces are user-owned.
      if (host) {
        const result = await host.router.requestOnRemoteChannel(host.profileId, host.owner, "task_discard", {
          repo_id: task.repoPath,
          task_id: taskId,
          branch: task.branch,
          worktree_path: task.worktreePath,
        });
        cleanupWarning = confirmRemoteTaskOperation(result, "discard");
      } else if (!task.isRemote) {
        if (ownsWorktree) {
          await this.workspaceScripts.archive(task, options);
          cleanupWarning = await removeWorktree(task);
          this.workspaceScripts.release(task);
        }
      }
      // Failed removal must leave recovery refs intact. Once the workspace
      // is gone, ref cleanup failures are retained work, not a failed discard.
      try {
        const repositoryPath = task.isRemote ? parseRepoId(task.repoPath)?.remotePath : task.repoPath;
        const keepBase = [...this.tasks.values()].some(candidate => {
          if (candidate.id === task.id || candidate.status === "discarded" || !sameTaskHost(candidate, task)) return false;
          const candidatePath = candidate.isRemote ? parseRepoId(candidate.repoPath)?.remotePath : candidate.repoPath;
          return candidatePath === repositoryPath && candidate.baseRef === task.baseRef;
        });
        await this.checkpointOperation(task, manager => manager.cleanup(task, keepBase));
      } catch (error: unknown) {
        const warning = `Conversation discarded; snapshot cleanup needs attention: ${error instanceof Error ? error.message : String(error)}`;
        cleanupWarning = cleanupWarning ? `${cleanupWarning}\n${warning}` : warning;
      }

      task.status = "discarded";
      task.completedAt = Date.now();
      task.lastActivityAt = task.completedAt;
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      if (cleanupWarning) this.emit("orchestrator:error", { taskId, message: cleanupWarning });
    } finally {
      this.workspaceTransitions.delete(task);
    }
  }

  /** Rejects applying a borrowed or actively changing workspace. */
  assertCanApply(taskId: string): void {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.assertWorkspaceAvailable(task);
    if (["archived", "applied", "discarded"].includes(task.status)) throw new Error("Reopen this conversation before applying changes");
    const parent = task.parentTaskId ? this.tasks.get(task.parentTaskId) : undefined;
    if (parent && sameWorkspace(parent, task)) {
      throw new Error("Review and apply shared workspace changes from the parent conversation.");
    }
    const active = [...this.tasks.values()].find(candidate => sameWorkspace(candidate, task) && ["running", "iterating", "queued"].includes(candidate.status));
    if (active) throw new Error("Wait for all conversations in this workspace to finish before applying changes.");
  }

  /** Returns a task by ID, or undefined if not found. */
  getTask(taskId: string): Task | undefined {
    this.rebuildChildTaskIds();
    const task = this.tasks.get(taskId);
    if (task && this.recoverCompletedRunningTask(task)) {
      this.persistTasks().catch(() => {});
      this.emit("orchestrator:task-status", { taskId, status: task.status });
    }
    return task;
  }

  /** Returns all tasks as an array. */
  getAllTasks(): Task[] {
    let changed = false;
    for (const task of this.tasks.values()) {
      changed = this.recoverCompletedRunningTask(task) || changed;
    }
    this.rebuildChildTaskIds();
    if (changed) {
      this.persistTasks().catch(() => {});
    }
    return Array.from(this.tasks.values());
  }

  /** Removes a discarded conversation while preserving its children's metadata. */
  async removeTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) return;
    if (task.status !== "discarded") throw new Error("Discard the conversation before removing it");
    const children = [...this.tasks.values()].filter(candidate => candidate.parentTaskId === taskId);
    this.tasks.delete(taskId);
    // SQLite clears the deleted parent, but upserting stale child IDs would
    // reintroduce a dangling foreign key and roll the entire deletion back.
    for (const child of children) child.parentTaskId = undefined;
    this.rebuildChildTaskIds();
    try { await this.persistTasks(); } catch (error: unknown) {
      this.tasks.set(taskId, task);
      for (const child of children) child.parentTaskId = taskId;
      this.rebuildChildTaskIds();
      throw error;
    }
  }

  /** Pins or unpins a task for sidebar display. */
  async setTaskPinned(taskId: string, pinned: boolean): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    task.pinnedAt = pinned ? Date.now() : undefined;
    task.lastActivityAt = Date.now();
    await this.persistTasks();
    return task;
  }

  /** Renames a task for chat/sidebar display. */
  async renameTask(taskId: string, title: string): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    const normalized = title.trim();
    if (!normalized) return task;
    task.title = normalized.slice(0, 160);
    task.sidebarLabel = undefined;
    task.lastActivityAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });
    return task;
  }

  /** Assigns a task to a sidebar collection. Empty values clear the collection. */
  async setTaskSidebarGroup(taskId: string, sidebarGroup?: string | null): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    const normalized = sidebarGroup?.trim();
    task.sidebarGroup = normalized ? normalized.slice(0, 80) : undefined;
    await this.persistTasks();
    return task;
  }

  /** Creates a lightweight parent chat session for an agent workflow run. */
  async createWorkflowRootTask(repoPath: string, title: string, prompt: string, workflowDefinitionId: string): Promise<Task> {
    this.assertAcceptingWork();
    const id = `workflow-${crypto.randomUUID()}`;
    const now = Date.now();
    let baseRef = "HEAD";
    try {
      const { stdout } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoPath });
      baseRef = stdout.trim() || "HEAD";
    } catch {
      // Remote repos or non-git paths still get a durable workflow session.
    }

    const task: Task = {
      id,
      title: title.trim() || "Workflow run",
      prompt,
      agentType: "workflow",
      status: "running",
      branch: "workflow",
      worktreePath: repoPath,
      worktreeMode: "primary",
      baseRef,
      repoPath,
      createdAt: now,
      startedAt: now,
      lastActivityAt: now,
      events: [],
      feedback: [],
      todos: [],
      workflowRunId: undefined,
      workflowNodeRunId: undefined,
      sidebarGroup: "Workflows",
    };
    (task as Task & { workflowDefinitionId?: string }).workflowDefinitionId = workflowDefinitionId;
    this.tasks.set(id, task);
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: id, status: task.status });
    return task;
  }

  async attachWorkflowRunToTask(taskId: string, workflowRunId: string): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    task.workflowRunId = workflowRunId;
    await this.persistTasks();
    return task;
  }

  async setTaskParent(taskId: string, parentTaskId?: string): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    const normalizedParentId = parentTaskId?.trim() || undefined;
    if (normalizedParentId === taskId) {
      throw new Error("A task cannot be its own parent");
    }
    if (normalizedParentId && !this.tasks.has(normalizedParentId)) {
      throw new Error(`Parent task not found: ${normalizedParentId}`);
    }
    task.parentTaskId = normalizedParentId;
    task.lastActivityAt = Date.now();
    this.rebuildChildTaskIds();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });
    return task;
  }

  async setWorkflowTaskStatus(taskId: string, status: TaskStatus): Promise<Task | undefined> {
    const task = this.tasks.get(taskId);
    if (!task) return undefined;
    task.status = status;
    task.lastActivityAt = Date.now();
    if (status === "reviewing" || status === "failed" || status === "stopped") {
      task.completedAt = task.completedAt ?? Date.now();
      task.lastActivityAt = task.completedAt;
    }
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status });
    return task;
  }

  /** Creates an in-memory child task that shares its parent's worktree. */
  createChildTaskForAgent(parentTaskId: string, agentType: AgentType, prompt: string, role?: string, spawnedByAgentId?: string): Task {
    const parent = this.tasks.get(parentTaskId);
    if (!parent) {
      throw new Error(`Task not found: ${parentTaskId}`);
    }
    this.assertWorkspaceAvailable(parent);
    if (["discarded", "applied", "archived"].includes(parent.status)) throw new Error("Cannot add an agent to a closed workspace.");
    const now = Date.now();
    const child: Task = {
      id: `child-${crypto.randomUUID()}`,
      title: role ? `${role} sub-agent` : `${agentType} sub-agent`,
      prompt,
      agentType,
      status: "created",
      model: agentType === parent.agentType ? parent.model : undefined,
      reasoningLevel: agentType === parent.agentType ? parent.reasoningLevel : undefined,
      planFirst: parent.planFirst,
      branch: parent.branch,
      worktreePath: parent.worktreePath,
      // The parent owns disposal. A child may stop without deleting the
      // workspace still used by its parent and sibling conversations.
      worktreeMode: "existing",
      baseRef: parent.baseRef,
      baseBranch: parent.baseBranch,
      repoPath: parent.repoPath,
      createdAt: now,
      lastActivityAt: now,
      events: [],
      feedback: [],
      todos: [],
      parentTaskId: parent.id,
      rootTaskId: parent.rootTaskId ?? parent.id,
      spawnedByAgentId,
      spawnedRole: role,
      sidebarLabel: role ? `${role} sub-agent` : undefined,
      isRemote: parent.isRemote,
      remoteProfileId: parent.remoteProfileId,
      remoteEndpoint: parent.remoteEndpoint,
    };
    parent.lastActivityAt = now;
    this.tasks.set(child.id, child);
    this.rebuildChildTaskIds();
    return child;
  }

  /** Sets the per-task chat UI state and persists to disk. */
  async setTaskUIState(taskId: string, state: PersistedChatUIState | null): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) return;
    task.uiState = state ?? undefined;
    await this.persistTasks();
  }

  /** Returns the per-task chat UI state, or undefined if not set. */
  getTaskUIState(taskId: string): PersistedChatUIState | undefined {
    const task = this.tasks.get(taskId);
    return task?.uiState;
  }

  // ─── Thread Operations (Codex app-server) ──────────────────────────────

  private getCodexAdapter(taskId: string): CodexAppServerAdapter {
    this.assertAcceptingWork();
    const adapter = this.adapters.get(taskId);
    if (!(adapter instanceof CodexAppServerAdapter)) {
      throw new Error("No active Codex connection for this task");
    }
    return adapter;
  }

  async forkThread(taskId: string): Promise<{ threadId: string }> {
    return this.getCodexAdapter(taskId).forkThread();
  }

  async compactThread(taskId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).compactThread();
  }

  async startReview(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    // Automatic turn snapshots commit changes. Review the task's baseline,
    // including dirty and untracked files, rather than an empty working diff.
    const instructions = `Review this workspace's changes relative to Git revision ${JSON.stringify(task.baseRef)}. Include committed, staged, unstaged, and untracked changes. Identify actionable correctness, regression, security, and test coverage issues with file and line references. Inspect relevant callers and tests before concluding. Do not modify files. If there are no findings, say so.`;
    await this.submitInput(taskId, { id: crypto.randomUUID(), content: instructions, kind: "review" });
  }

  async setThreadName(taskId: string, name: string): Promise<{}> {
    return this.getCodexAdapter(taskId).setThreadName(name);
  }

  async setGoal(taskId: string, mutation: CodexGoalMutation): Promise<CodexGoal> {
    const activation = mutation.status === undefined || mutation.status === "active";
    return this.controlGoal(taskId, activation, async (task, signal) => {
      if (activation) {
        this.assertWorkspaceAvailable(task);
        if (["archived", "applied", "discarded"].includes(task.status)) throw new Error("Reopen this conversation before activating its goal");
        if (this.closingTasks.has(taskId)) throw new Error("Agent shutdown is still in progress");
        await this.submitInput(taskId, { id: crypto.randomUUID(), content: mutation.objective ?? task.goal?.objective ?? "Resume the current goal", kind: "goal", goal: mutation }, signal);
        const goal = task.goal ?? await this.getGoal(taskId);
        if (!goal) throw new Error("The native goal was cleared after activation");
        return goal;
      }
      const goal = await this.withGoalClient(task, (client, threadId) => client.setGoal(mutation, "user", threadId));
      this.updateTaskGoal(task, goal, true);
      return goal;
    });
  }

  async getGoal(taskId: string): Promise<CodexGoal | null> {
    const task = this.goalTask(taskId);
    if (!task.codexThreadId) return null;
    const existing = this.goalReads.get(taskId);
    if (existing) return existing;
    const revision = this.goalRevisions.get(taskId);
    const read = this.withGoalClient(task, (client, threadId) => client.getGoal(threadId)).then(goal => {
      if (revision === this.goalRevisions.get(taskId)) this.updateTaskGoal(task, goal, true);
      return task.goal ?? null;
    }).finally(() => { if (this.goalReads.get(taskId) === read) this.goalReads.delete(taskId); });
    this.goalReads.set(taskId, read);
    return read;
  }

  async clearGoal(taskId: string): Promise<void> {
    await this.controlGoal(taskId, false, async task => {
      if (task.codexThreadId) await this.withGoalClient(task, (client, threadId) => client.clearGoal(threadId));
      this.updateTaskGoal(task, null, true);
    });
  }

  private goalTask(taskId: string): Task {
    this.assertAcceptingWork();
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    if (task.isRemote || task.agentType !== "codex") throw new Error("Goals require a local Codex conversation");
    return task;
  }

  private async controlGoal<T>(taskId: string, activation: boolean, operation: (task: Task, signal: AbortSignal) => Promise<T>): Promise<T> {
    const task = this.goalTask(taskId);
    const previous = this.goalControls.get(taskId);
    if (!activation) {
      if (previous?.activation) previous.controller.abort(new Error("Goal activation cancelled"));
      for (const input of this.pendingSubmissions.get(taskId) ?? []) if (input.kind === "goal") input.controller.abort(new Error("Goal activation cancelled"));
    }
    const controller = new AbortController();
    const promise = (async () => {
      await previous?.promise.catch(() => {});
      controller.signal.throwIfAborted();
      return operation(task, controller.signal);
    })();
    const entry = { promise, controller, activation };
    this.goalControls.set(taskId, entry);
    try { return await promise; } finally {
      if (this.goalControls.get(taskId) === entry) this.goalControls.delete(taskId);
    }
  }

  private async withGoalClient<T>(task: Task, operation: (client: CodexAppServerAdapter, threadId: string) => Promise<T>, closing = false): Promise<T> {
    if (!closing) {
      await this.startups.get(task.id)?.promise.catch(() => {});
      this.assertWorkspaceAvailable(task);
      if (this.closingTasks.has(task.id)) throw new Error("Agent shutdown is still in progress");
    }
    const threadId = task.codexThreadId;
    if (!threadId) throw new Error("This conversation has no native Codex thread yet");
    const live = this.adapters.get(task.id);
    if (live instanceof CodexAppServerAdapter) return operation(live, threadId);
    const config = loadAgentConfig("codex");
    const client = createAdapter("codex", config.binaryPath, undefined, config.extraArgs, { env: config.env, optOutNotifications: config.optOutNotifications });
    if (!(client instanceof CodexAppServerAdapter)) throw new Error("Goals require Codex app-server");
    const readers = this.goalReaders.get(task.id) ?? new Set<CodexAppServerAdapter>();
    readers.add(client);
    this.goalReaders.set(task.id, readers);
    try {
      // Native goal/get reads durable metadata without loading the thread or
      // starting its active goal. Keep this process owned until actual close.
      await client.readGoal(threadId, fs.existsSync(task.worktreePath) ? task.worktreePath : task.repoPath, config.env);
      return await operation(client, threadId);
    } finally {
      await client.cancel();
      readers.delete(client);
      if (!readers.size && this.goalReaders.get(task.id) === readers) this.goalReaders.delete(task.id);
    }
  }

  private updateTaskGoal(task: Task, goal: CodexGoal | null, publish = false): void {
    const changed = JSON.stringify(task.goal) !== JSON.stringify(goal);
    if (changed) {
      task.goal = goal;
      this.goalRevisions.set(task.id, (this.goalRevisions.get(task.id) ?? 0) + 1);
    }
    const adapter = this.adapters.get(task.id);
    if (goal?.status === "active" && adapter && !["stopped", "archived", "applied", "discarded"].includes(task.status)) {
      task.lastInputKind = "goal";
      task.status = "running";
      task.completedAt = undefined;
      task.exitCode = undefined;
      this.setupTimeout(task.id);
      this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
    } else if (adapter instanceof CodexAppServerAdapter && !adapter.hasActiveTurn) {
      const done = this.goalCompletions.get(task.id);
      if (done) {
        this.goalCompletions.delete(task.id);
        try {
          if (this.completeTurnFromDoneEvent(task, task.id, done)) {
            this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
            this.processQueue();
          }
        }
        catch (error: unknown) { this.emit("orchestrator:error", { taskId: task.id, message: String(error) }); }
      }
    }
    if (!changed) return;
    if (publish) {
      const event: AgentEvent = { type: "goal_updated", timestamp: Date.now(), goal };
      task.events.push(event);
      this.emit("orchestrator:event", { taskId: task.id, event });
    }
    void this.persistTasks().catch(error => this.emit("orchestrator:error", { taskId: task.id, message: String(error) }));
  }

  private async settleGoalActivation(task: Task, adapter: CodexAppServerAdapter): Promise<void> {
    if (task.goal?.status === "active" || adapter.hasActiveTurn || !["running", "iterating"].includes(task.status)) return;
    // An exhausted budget can acknowledge activation while keeping the goal
    // inactive, without a new turn or notification. Do not leave a phantom run.
    task.status = "reviewing";
    task.completedAt = Date.now();
    this.clearTimeout(task.id);
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
    this.processQueue();
  }

  private async pauseStoppedGoal(task: Task): Promise<void> {
    if (task.isRemote || task.agentType !== "codex" || !task.codexThreadId
      || !(task.goal?.status === "active" || task.pendingInput?.kind === "goal" || task.lastInputKind === "goal")) return;
    const goal = await this.withGoalClient(task, async (client, threadId) => {
      const current = await client.getGoal(threadId);
      return current?.status === "active" ? client.setGoal({ status: "paused" }, "automatic", threadId) : current;
    }, true);
    // Stopping a process alone leaves its durable goal active. Pause metadata
    // without resuming the thread, so reopening cannot restart cancelled work.
    this.updateTaskGoal(task, goal, true);
  }

  async updateThreadMetadata(taskId: string, metadata: Record<string, unknown>): Promise<{}> {
    return this.getCodexAdapter(taskId).updateThreadMetadata(metadata);
  }

  async shellCommand(taskId: string, command: string, cwd?: string): Promise<{ result: unknown }> {
    return this.getCodexAdapter(taskId).shellCommand(command, cwd);
  }

  async cleanBackgroundTerminals(taskId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).cleanBackgroundTerminals();
  }

  async injectItems(taskId: string, items: Array<{ role: string; content: unknown }>): Promise<{}> {
    return this.getCodexAdapter(taskId).injectItems(items);
  }

  // ─── Command Execution (Codex app-server) ──────────────────────────────

  async execCommand(
    taskId: string,
    params: { command: string; args?: string[]; cwd?: string; env?: Record<string, string>; stdin?: string; timeoutMs?: number },
  ): Promise<{ sessionId: string }> {
    return this.getCodexAdapter(taskId).execCommand(params);
  }

  async writeCommandStdin(
    taskId: string, sessionId: string, stdin: string, closeStdin?: boolean,
  ): Promise<{}> {
    return this.getCodexAdapter(taskId).writeCommandStdin(sessionId, stdin, closeStdin);
  }

  async resizeCommandPty(
    taskId: string, sessionId: string, cols: number, rows: number,
  ): Promise<{}> {
    return this.getCodexAdapter(taskId).resizeCommandPty(sessionId, cols, rows);
  }

  async terminateCommand(taskId: string, sessionId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).terminateCommand(sessionId);
  }

  // ─── Feedback (Codex app-server) ───────────────────────────────────────

  async uploadFeedback(
    taskId: string,
    params: { turnId?: string; rating?: number; comment?: string; tags?: string[] },
  ): Promise<{}> {
    return this.getCodexAdapter(taskId).uploadFeedback(params);
  }

  // ─── Filesystem (Codex app-server) ─────────────────────────────────────

  /**
   * Returns any available Codex adapter, preferring the given task's if active.
   */
  private getAnyCodexAdapter(preferredTaskId?: string): CodexAppServerAdapter {
    this.assertAcceptingWork();
    if (preferredTaskId) {
      const preferred = this.adapters.get(preferredTaskId);
      if (preferred instanceof CodexAppServerAdapter) return preferred;
    }
    for (const adapter of this.adapters.values()) {
      if (adapter instanceof CodexAppServerAdapter) return adapter;
    }
    throw new Error("No active Codex connection available — start a Codex task first");
  }

  async fsReadFile(path: string, taskId?: string): Promise<string> {
    return this.getAnyCodexAdapter(taskId).fsReadFile(path);
  }

  async fsWriteFile(path: string, content: string, taskId?: string): Promise<{}> {
    return this.getAnyCodexAdapter(taskId).fsWriteFile(path, content);
  }

  async fsCreateDirectory(path: string, taskId?: string): Promise<{}> {
    return this.getAnyCodexAdapter(taskId).fsCreateDirectory(path);
  }

  async fsGetMetadata(path: string, taskId?: string): Promise<Record<string, unknown>> {
    return this.getAnyCodexAdapter(taskId).fsGetMetadata(path);
  }

  async fsReadDirectory(path: string, taskId?: string): Promise<Array<Record<string, unknown>>> {
    return this.getAnyCodexAdapter(taskId).fsReadDirectory(path);
  }

  async fsRemove(path: string, recursive?: boolean, taskId?: string): Promise<{}> {
    return this.getAnyCodexAdapter(taskId).fsRemove(path, recursive);
  }

  async fsCopy(source: string, destination: string, taskId?: string): Promise<{}> {
    return this.getAnyCodexAdapter(taskId).fsCopy(source, destination);
  }

  async fsWatch(path: string, taskId?: string): Promise<string> {
    return this.getAnyCodexAdapter(taskId).fsWatch(path);
  }

  async fsUnwatch(watchId: string, taskId?: string): Promise<{}> {
    return this.getAnyCodexAdapter(taskId).fsUnwatch(watchId);
  }

  /** Runs configured setup while retaining its process group for workspace closure. */
  async runSetupScript(taskId: string, options: WorkspaceScriptOptions = {}): Promise<ScriptResult & { skipped?: boolean }> {
    const task = this.scriptTask(taskId);
    const owner = this.setupOwner(task);
    if (this.workspaceSetups.has(task.worktreePath)) throw new Error("Workspace setup is already in progress");
    return this.waitForSetup(task, this.startWorkspaceSetup(owner, options));
  }

  /** Starts or replaces the run process belonging to this local workspace. */
  async runWorkspace(taskId: string, options: WorkspaceScriptOptions = {}): Promise<{ port: number; stoppedTaskId: string | null }> {
    const task = this.scriptTask(taskId);
    return this.workspaceScripts.run(task, options);
  }

  /** Stops accepted script work in this workspace and awaits its descendants. */
  async stopWorkspace(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    if (task.isRemote) throw new Error("Remote workspace scripts require remote agent support");
    const preparations = [...this.workspaceSetups.get(task.worktreePath)?.waiters ?? []];
    for (const waiter of preparations) waiter.controller.abort(new WorkspaceScriptCancelledError());
    await this.workspaceScripts.close(task, true);
    await Promise.all(preparations.map(waiter => this.settleSetupWaiter(waiter)));
  }

  private setupOwner(task: Task): Task {
    if (task.isRemote) return task;
    return this.workspaceSetups.get(task.worktreePath)?.owner
      ?? [...this.tasks.values()].find(candidate => candidate.worktreePath === task.worktreePath && !candidate.isRemote && candidate.workspaceSetup !== undefined)
      ?? task;
  }

  private async prepareWorkspace(task: Task, signal?: AbortSignal): Promise<void> {
    if (task.isRemote) return;
    this.assertLocalWorkspace(task);
    const owner = this.setupOwner(task);
    let job = this.workspaceSetups.get(task.worktreePath);
    if (!job) {
      if (!owner.workspaceSetup || ["ready", "skipped"].includes(owner.workspaceSetup)) return;
      if (owner.workspaceSetup !== "pending") throw new Error("Workspace setup failed or was interrupted. Use Retry to run setup again.");
      job = this.startWorkspaceSetup(owner, {});
    }
    const result = await this.waitForSetup(task, job, signal);
    if (!result.success) throw new Error(`Workspace setup failed (exit code ${result.exitCode}): ${result.error ?? "see setup output"}`);
  }

  private startWorkspaceSetup(owner: Task, options: WorkspaceScriptOptions): SetupJob {
    const controller = new AbortController();
    const job: SetupJob = { id: crypto.randomUUID(), owner, controller, waiters: new Set(), output: "",
      promise: Promise.resolve().then(async () => {
        owner.workspaceSetup = "running";
        this.saveTasks();
        let result: SetupResult;
        try { result = await this.workspaceScripts.setup(owner, options, controller.signal); }
        catch (error: unknown) { owner.workspaceSetup = "failed"; this.saveTasks(); throw error; }
        owner.workspaceSetup = result.success ? result.skipped ? "skipped" : "ready" : "failed";
        try { this.saveTasks(); } catch (error: unknown) { owner.workspaceSetup = "running"; throw error; }
        return result;
      }).finally(() => { if (this.workspaceSetups.get(owner.worktreePath) === job) this.workspaceSetups.delete(owner.worktreePath); }),
    };
    // Publish the shared owner before its script can emit output or fail.
    this.workspaceSetups.set(owner.worktreePath, job);
    return job;
  }

  private waitForSetup(task: Task, job: SetupJob, signal?: AbortSignal): Promise<SetupResult> {
    const controller = new AbortController();
    const cancel = () => controller.abort(signal?.reason);
    signal?.addEventListener("abort", cancel, { once: true });
    if (signal?.aborted) cancel();
    const waiter: SetupWaiter = { taskId: task.id, controller, promise: Promise.resolve().then(async () => {
      let rejectAbort!: (error: unknown) => void;
      const aborted = new Promise<never>((_resolve, reject) => { rejectAbort = reject; });
      const abort = () => rejectAbort(controller.signal.reason);
      controller.signal.addEventListener("abort", abort, { once: true });
      try {
        controller.signal.throwIfAborted();
        return await Promise.race([job.promise, aborted]);
      } finally {
        signal?.removeEventListener("abort", cancel);
        controller.signal.removeEventListener("abort", abort);
        job.waiters.delete(waiter);
        // A borrower can detach without cancelling a peer's preparation. The
        // final subscriber retains its slot until the physical script closes.
        if (!job.waiters.size && this.workspaceSetups.get(task.worktreePath) === job) {
          job.controller.abort(new WorkspaceScriptCancelledError());
          try { await job.promise; } catch (error: unknown) { if (!(error instanceof WorkspaceScriptCancelledError)) throw error; }
        }
      }
    }) };
    job.waiters.add(waiter);
    return waiter.promise;
  }

  private setupWaiters(taskId: string): SetupWaiter[] {
    return [...this.workspaceSetups.values()].flatMap(job => [...job.waiters].filter(waiter => waiter.taskId === taskId));
  }

  private async settleSetupWaiter(waiter: SetupWaiter): Promise<void> {
    try { await waiter.promise; } catch (error: unknown) {
      if (error !== waiter.controller.signal.reason && !(error instanceof WorkspaceScriptCancelledError)) throw error;
    }
  }

  private workspaceScriptEvent(event: WorkspaceScriptEvent): void {
    if (event.type !== "data" || event.kind !== "setup") { this.emit("orchestrator:workspace-script", event); return; }
    const job = [...this.workspaceSetups.values()].find(candidate => candidate.owner.id === event.taskId);
    if (!job) return;
    job.output += event.data;
    const targets = new Set([...job.waiters].map(waiter => waiter.taskId));
    for (const taskId of targets) {
      const task = this.tasks.get(taskId);
      if (!task) continue;
      const output: AgentEvent = { type: "command_run", tool: "Workspace setup", itemId: job.id, command: event.command, stdout: job.output, timestamp: Date.now() };
      task.events.push(output);
      this.publishTaskEvent(task, output);
    }
  }

  private scriptTask(taskId: string): Task {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    this.assertWorkspaceAvailable(task);
    if (task.isRemote) throw new Error("Remote workspace scripts require remote agent support");
    if (["archived", "applied", "discarded"].includes(task.status)) throw new Error("Reopen this workspace before running scripts");
    if (!fs.existsSync(task.worktreePath)) throw new Error("Workspace is missing. Restore it before running scripts.");
    return task;
  }

  // ─── Queue Management ──────────────────────────────────────────────────

  /** Counts active provider work, preparation reservations, and closing processes. */
  private getRunningTaskCount(): number {
    const active = new Set([...this.reservedTasks.keys(), ...this.closingTasks]);
    for (const task of this.tasks.values()) {
      if (this.hasActiveTurn(task)) active.add(task.id);
    }
    return active.size;
  }

  private hasActiveTurn(task: Task): boolean {
    // Coordinator rows and persisted status do not own provider work.
    // Native adapters retain both local and SSH process ownership.
    const owned = this.adapters.has(task.id);
    return owned && (task.status === "running" || task.goal?.status === "active");
  }

  private async admitTask(task: Task, signal?: AbortSignal): Promise<() => void> {
    signal?.throwIfAborted();
    this.assertWorkspaceAvailable(task);
    // A cold follow-up starts a new process inside its already admitted input.
    // Only the outer operation may release that reservation.
    if (this.reservedTasks.has(task.id)) return () => {};
    const reserve = () => {
      const token = {};
      this.reservedTasks.set(task.id, token);
      return () => {
        if (this.reservedTasks.get(task.id) !== token) return;
        this.reservedTasks.delete(task.id);
        this.processQueue();
      };
    };
    if (this.hasActiveTurn(task) || this.getRunningTaskCount() < this.maxConcurrent) return reserve();
    if (this.admissionQueue.has(task.id)) throw new Error("Task is already waiting for capacity");
    task.status = "queued";
    task.completedAt = undefined;
    task.exitCode = undefined;
    let resume!: () => void;
    let reject!: (error: Error) => void;
    let release: (() => void) | undefined;
    const ready = new Promise<void>((resolve, fail) => { resume = resolve; reject = fail; });
    void ready.catch(() => {});
    const entry = {
      resume: () => {
        if (this.admissionQueue.get(task.id) !== entry) return;
        this.admissionQueue.delete(task.id);
        release = reserve();
        resume();
      },
      reject: (error: Error) => {
        if (this.admissionQueue.get(task.id) !== entry) return;
        this.admissionQueue.delete(task.id);
        reject(error);
      },
    };
    this.admissionQueue.set(task.id, entry);
    const abort = () => entry.reject(signal?.reason instanceof Error ? signal.reason : new Error("Message cancelled before submission"));
    signal?.addEventListener("abort", abort, { once: true });
    try {
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
      this.processQueue();
      await ready;
      signal?.throwIfAborted();
      this.assertWorkspaceAvailable(task);
      return release!;
    } catch (error: unknown) {
      entry.reject(error instanceof Error ? error : new Error("Task admission failed", { cause: error }));
      release?.();
      throw error;
    } finally {
      signal?.removeEventListener("abort", abort);
    }
  }

  /** Starts queued tasks if there are free slots. */
  private processQueue(): void {
    if (this.shutdownPromise) return;
    for (const entry of this.admissionQueue.values()) {
      if (this.getRunningTaskCount() >= this.maxConcurrent) return;
      entry.resume();
    }
    // v2.0.0-alpha.1 persisted queued starts without an in-memory waiter.
    // Resume only undispatched jobs; pending inputs recover without resending.
    const queued = Array.from(this.tasks.values())
      .filter((t) => t.status === "queued" && t.agentType !== "workflow" && !t.pendingInput)
      .sort((a, b) => a.createdAt - b.createdAt);

    for (const task of queued) {
      if (this.getRunningTaskCount() >= this.maxConcurrent) break;
      if (this.startups.has(task.id) || this.reservedTasks.has(task.id) || this.pendingSubmissions.has(task.id)) continue;
      void this.startTask(task.id).catch((error: unknown) => {
        if (["stopped", "discarded", "archived", "applied"].includes(task.status)) return;
        this.emit("orchestrator:error", { taskId: task.id, message: error instanceof Error ? error.message : String(error) });
      });
    }
  }

  // ─── Timeout ─────────────────────────────────────────────────────────────

  private setupTimeout(taskId: string): void {
    this.clearTimeout(taskId);

    const timer = setTimeout(async () => {
      const task = this.tasks.get(taskId);
      // A queued follow-up may prepare while its preceding native turn still
      // owns this timer. Preparation cannot suppress that turn's timeout.
      if (!task || (task.status !== "running" && task.status !== "iterating")) return;

      this.closingTasks.add(taskId);
      const preparation = this.setupWaiters(taskId).map(waiter => this.settleSetupWaiter(waiter));
      this.cancelPendingSubmissions(taskId);
      task.status = "failed";
      task.completedAt = Date.now();
      task.lastActivityAt = task.completedAt;
      task.exitCode = 1;
      const event: AgentEvent = { type: "error", timestamp: task.completedAt, error: `Task timed out after ${this.timeoutMs}ms` };
      task.events.push(event);
      this.emit("orchestrator:event", { taskId, event });
      try {
        await Promise.all(preparation);
        const adapter = this.adapters.get(taskId);
        if (adapter) {
          await adapter.cancel();
          if (this.adapters.get(taskId) === adapter) this.adapters.delete(taskId);
        }
        await this.pauseStoppedGoal(task);
      } catch (error: unknown) {
        this.emit("orchestrator:error", { taskId, message: `Failed to pause the stopped goal: ${String(error)}` });
      } finally {
        this.closingTasks.delete(taskId);
      }

      await this.persistTasks().catch(() => {});
      this.emit("orchestrator:timeout", { taskId });
      this.emit("orchestrator:task-status", { taskId, status: task.status });

      // Start any queued tasks now that a slot is free.
      this.processQueue();
    }, this.timeoutMs);

    this.timeouts.set(taskId, timer);
  }

  private clearTimeout(taskId: string): void {
    const timer = this.timeouts.get(taskId);
    if (timer) {
      clearTimeout(timer);
      this.timeouts.delete(taskId);
    }
  }

  private async commitTurnSnapshot(task: Task, taskId: string): Promise<void> {
    if (!task.worktreePath || !task.isRemote && !fs.existsSync(task.worktreePath)) return;
    if (!this.ownsTaskWorktree(task)) return;
    // Child conversations share files, not turn boundaries. Committing while
    // another chat is working would stage that chat's unfinished edits.
    if ([...this.tasks.values()].some(candidate => candidate.id !== taskId && sameWorkspace(candidate, task)
      && ["running", "iterating"].includes(candidate.status))) return;

    const turnNum = task.pendingInput?.turnNumber ?? task.turnNumber ?? 0;
    const committed = await this.checkpointOperation(task, manager => manager.commitTurn(task, turnNum));
    if (!committed) return;
    this.emit("orchestrator:turn-committed", { taskId, turnNumber: turnNum });
  }

  // ─── Persistence ─────────────────────────────────────────────────────────

  /** Saves task metadata to disk (excludes events array). */
  async persistTasks(): Promise<void> {
    this.saveTasks();
  }

  private saveTasks(): void {
    const tasks: PersistedTask[] = Array.from(this.tasks.values()).map((task) => this.toPersistedTask(task));

    try {
      this.taskDb.replaceTasks(tasks);
    } catch (err) {
      this.emit("orchestrator:error", {
        taskId: null,
        message: `Failed to persist tasks: ${err instanceof Error ? err.message : String(err)}`,
      });
      throw err;
    }
  }

  /** Replays terminal events after the journal owner has registered. */
  connectCompletionJournal(): void {
    this.completionJournalAttached = true;
    for (const task of this.tasks.values()) {
      try { this.flushCompletions(task.id); }
      catch (error: unknown) { this.emit("orchestrator:error", { taskId: task.id, message: String(error) }); }
    }
  }

  /** Clears a terminal event only after its journal row is committed. */
  acknowledgeCompletion(taskId: string, id: string): void {
    const task = this.tasks.get(taskId);
    const previous = task?.pendingCompletions;
    if (!task || !previous?.some(completion => completion.id === id)) return;
    const remaining = previous.filter(completion => completion.id !== id);
    task.pendingCompletions = remaining.length ? remaining : undefined;
    try { this.saveTasks(); } catch (error: unknown) { task.pendingCompletions = previous; throw error; }
  }

  /** Drains earlier terminal records before a later journal write. */
  flushCompletions(taskId: string, beforeId?: string): void {
    if (!this.completionJournalAttached) return;
    const task = this.tasks.get(taskId);
    for (const completion of task?.pendingCompletions ?? []) {
      if (completion.id === beforeId) break;
      this.emit("orchestrator:event", { taskId, event: completion.event, completionId: completion.id });
      if (task?.pendingCompletions?.some(pending => pending.id === completion.id)) throw new Error("The completed turn could not be saved to chat. Resolve the storage error before continuing.");
    }
  }

  /** Loads task metadata from disk on startup. */
  private loadTasks(): void {
    try {
      const persistedTasks = this.taskDb.loadTasks();

      if (persistedTasks.length === 0) return;

      let recovered = false;
      for (const persisted of persistedTasks) {
        // Silently strip any stale `pendingApprovals` from old task records
        // so they don't leak into the in-memory Task object.
        const { pendingApprovals: _ignored, ...rest } = persisted as PersistedTask & { pendingApprovals?: unknown };
        const task: Task = {
          ...rest,
          events: [],
          todos: rest.todos ?? [],
          title: rest.title,
          prUrl: rest.prUrl,
          prNumber: rest.prNumber,
          worktreeMode: rest.worktreeMode,
          isRemote: rest.isRemote,
          remoteProfileId: rest.remoteProfileId,
          codexThreadId: rest.codexThreadId,
          claudeSessionId: rest.claudeSessionId,
          uiState: rest.uiState,
          workflowRunId: rest.workflowRunId,
          workflowNodeRunId: rest.workflowNodeRunId,
          planFirst: rest.planFirst,
          pinnedAt: rest.pinnedAt,
          parentTaskId: rest.parentTaskId,
          rootTaskId: rest.rootTaskId,
          spawnedByAgentId: rest.spawnedByAgentId,
          spawnedRole: rest.spawnedRole,
          lastActivityAt: rest.lastActivityAt,
          sidebarLabel: rest.sidebarLabel,
          sidebarGroup: rest.sidebarGroup,
        };
        recovered = this.recoverCompletedRunningTask(task) || recovered;
        // A restarted owner has no bound native connection on either host.
        // Preserve initial queued starts; interrupted work requires explicit Retry.
        if (task.workspaceSetup === "running" && ["created", "queued", "running", "iterating"].includes(task.status) || ["codex", "claude-code"].includes(task.agentType) && ["running", "iterating"].includes(task.status)) {
          task.status = "interrupted";
          task.completedAt = Date.now();
          task.exitCode = 1;
          recovered = true;
        }
        this.tasks.set(task.id, task);
      }
      this.rebuildChildTaskIds();
      for (const task of this.tasks.values()) if (task.pendingInput) {
        void this.recoverTaskInput(task.id).catch(error => this.reportInputRecoveryError(task, error));
      }
      if (recovered) {
        this.persistTasks().catch(() => {});
      }
    } catch {
      // If the database is corrupted or unreadable, start fresh.
    }
  }

  // ─── Helpers ─────────────────────────────────────────────────────────────

  private toPersistedTask(task: Task): PersistedTask {
    return {
      id: task.id,
      title: task.title,
      prompt: task.prompt,
      agentType: task.agentType,
      status: task.status,
      branch: task.branch,
      worktreePath: task.worktreePath,
      worktreeMode: task.worktreeMode,
      baseRef: task.baseRef,
      baseBranch: task.baseBranch,
      repoPath: task.repoPath,
      createdAt: task.createdAt,
      startedAt: task.startedAt,
      completedAt: task.completedAt,
      exitCode: task.exitCode,
      feedback: task.feedback,
      turnNumber: task.turnNumber,
      pendingInput: task.pendingInput,
      lastInputKind: task.lastInputKind,
      pendingCompletions: task.pendingCompletions,
      workspaceSetup: task.workspaceSetup,
      goal: task.goal,
      todos: task.todos ?? [],
      prUrl: task.prUrl,
      prNumber: task.prNumber,
      isRemote: task.isRemote,
      remoteProfileId: task.remoteProfileId,
      remoteEndpoint: task.remoteEndpoint,
      codexThreadId: task.codexThreadId,
      claudeSessionId: task.claudeSessionId,
      uiState: task.uiState,
      workflowRunId: task.workflowRunId,
      workflowNodeRunId: task.workflowNodeRunId,
      planFirst: task.planFirst,
      model: task.model,
      reasoningLevel: task.reasoningLevel,
      pinnedAt: task.pinnedAt,
      parentTaskId: task.parentTaskId,
      rootTaskId: task.rootTaskId,
      spawnedByAgentId: task.spawnedByAgentId,
      spawnedRole: task.spawnedRole,
      lastActivityAt: task.lastActivityAt,
      sidebarLabel: task.sidebarLabel,
      sidebarGroup: task.sidebarGroup,
    };
  }

  private rebuildChildTaskIds(): void {
    for (const task of this.tasks.values()) {
      task.childTaskIds = [];
      task.rootTaskId = undefined;
    }
    for (const task of this.tasks.values()) {
      if (!task.parentTaskId) continue;
      const parent = this.tasks.get(task.parentTaskId);
      if (!parent) continue;
      parent.childTaskIds = parent.childTaskIds ?? [];
      if (!parent.childTaskIds.includes(task.id)) {
        parent.childTaskIds.push(task.id);
      }
    }

    const resolveRoot = (task: Task, seen = new Set<string>()): string | undefined => {
      if (!task.parentTaskId || seen.has(task.id)) return undefined;
      seen.add(task.id);
      const parent = this.tasks.get(task.parentTaskId);
      if (!parent) return undefined;
      return resolveRoot(parent, seen) ?? parent.id;
    };
    for (const task of this.tasks.values()) {
      task.rootTaskId = resolveRoot(task);
    }
  }

  private completeTurnFromDoneEvent(
    task: Task,
    taskId: string,
    event: AgentEvent,
  ): boolean {
    if (task.status !== "running" && task.status !== "iterating") { this.saveTasks(); return false; }

    // Native goals may launch another turn immediately after done. Retain the
    // slot and defer Git writes until the goal stops to avoid racing that turn.
    if (event.outcome === "completed" && task.goal?.status === "active" && this.adapters.has(taskId)) {
      this.goalCompletions.set(taskId, event);
      task.lastActivityAt = Date.now();
      this.setupTimeout(taskId);
      this.saveTasks();
      return false;
    }

    this.goalCompletions.delete(taskId);
    this.clearTimeout(taskId);
    const successful = event.outcome === "completed";
    task.exitCode = successful ? 0 : 1;
    task.completedAt = Date.now();
    task.lastActivityAt = task.completedAt;
    // Native completion remains intact in history. A later unsent input keeps
    // Retry available once the preceding turn releases its process slot.
    const unsent = task.pendingInput?.phase === "unsent" && !task.pendingInput.outcome;
    task.status = unsent ? "failed" : event.outcome === "interrupted" ? "interrupted" : successful ? task.planFirst ? "planning" : "reviewing" : "failed";

    // The terminal event and its task outcome share this transaction. The
    // journal acknowledges the event separately; restart replays its stable ID.
    this.saveTasks();
    this.schedulePostTurnSnapshot(task, taskId);
    return true;
  }

  private schedulePostTurnSnapshot(task: Task, taskId: string): void {
    if (task.goal?.status === "active") return;
    // Publish the job before completion is visible, without delaying the UI.
    // Workspace closure must await this writer before saving/removing files.
    const previous = this.postTurnSnapshots.get(taskId) ?? Promise.resolve();
    const snapshot = previous.then(() => this.commitTurnSnapshot(task, taskId)).catch((err) => {
      // Report the failed Git save without changing the provider's completed
      // outcome. Its working files still require review and explicit closure.
      this.emit("orchestrator:error", {
        taskId,
        message: `Auto-commit after turn failed: ${err instanceof Error ? err.message : String(err)}`,
      });
      if (err instanceof AgentProcessClosureError) throw err;
    }).finally(() => {
      if (this.postTurnSnapshots.get(taskId) === snapshot) this.postTurnSnapshots.delete(taskId);
    });
    this.postTurnSnapshots.set(taskId, snapshot);
    void snapshot.catch(() => {});
  }

  private recoverCompletedRunningTask(task: Task): boolean {
    if (
      task.status !== "running"
      && task.status !== "iterating"
      && task.status !== "queued"
    ) {
      return false;
    }
    if (!task.completedAt || typeof task.exitCode !== "number") return false;

    if (task.exitCode === 0) {
      task.status = task.planFirst ? "planning" : "reviewing";
    } else {
      task.status = "failed";
    }
    return true;
  }

  private async resolveBaseRef(repoPath: string, ref: string): Promise<string> {
    try {
      const { stdout } = await execFile("git", ["rev-parse", ref], { cwd: repoPath });
      return stdout.trim();
    } catch {
      return (await this.resolveHeadRef(repoPath)) ?? "HEAD";
    }
  }

  private async resolveHeadRef(repoPath: string): Promise<string | undefined> {
    try {
      const { stdout } = await execFile("git", ["rev-parse", "HEAD"], { cwd: repoPath });
      return stdout.trim() || undefined;
    } catch {
      return undefined;
    }
  }

  private async getCurrentBranch(repoPath: string): Promise<string | undefined> {
    try {
      const { stdout } = await execFile(
        "git",
        ["rev-parse", "--abbrev-ref", "HEAD"],
        { cwd: repoPath },
      );
      const branch = stdout.trim();
      return branch && branch !== "HEAD" ? branch : undefined;
    } catch {
      return undefined;
    }
  }

  private async checkoutBranch(repoPath: string, branch: string): Promise<void> {
    const currentBranch = await this.getCurrentBranch(repoPath);
    if (currentBranch === branch) return;

    try {
      await execFile("git", ["checkout", branch], { cwd: repoPath });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      throw new Error(
        `Could not switch working directory to branch "${branch}". Commit, stash, or discard conflicting local changes first. ${message}`,
      );
    }
  }

  private ensureContextDirectory(worktreePath: string): void {
    try {
      const contextPath = path.join(worktreePath, ".context");
      fs.mkdirSync(contextPath, { recursive: true });
      fs.writeFileSync(path.join(contextPath, ".gitignore"), "*\n", "utf-8");
    } catch {
      // Best-effort.
    }
  }

  private async retireAdapter(taskId: string): Promise<void> {
    this.goalCompletions.delete(taskId);
    const previous = this.adapters.get(taskId);
    if (!previous) return;
    this.clearTimeout(taskId);
    // Keep request-resolution events while closing, but do not let the retired
    // process's outcome replace the next start or a restored conversation.
    this.retiringAdapters.add(previous);
    const alreadyClosing = this.closingTasks.has(taskId);
    this.closingTasks.add(taskId);
    try { await previous.cancel(); } finally {
      this.retiringAdapters.delete(previous);
      if (!alreadyClosing) this.closingTasks.delete(taskId);
      this.processQueue();
    }
    if (this.adapters.get(taskId) === previous) this.adapters.delete(taskId);
  }

  private cancelPendingSubmissions(taskId: string): void {
    for (const waiter of this.setupWaiters(taskId)) waiter.controller.abort(new WorkspaceScriptCancelledError());
    this.goalControls.get(taskId)?.controller.abort(new Error("Goal control cancelled"));
    for (const submission of this.pendingSubmissions.get(taskId) ?? []) submission.controller.abort(new Error("Message cancelled before submission"));
  }

  private async closeGoalReaders(taskId: string): Promise<void> {
    await Promise.all([...(this.goalReaders.get(taskId) ?? [])].map(reader => reader.cancel()));
  }

  private cancelQueuedAdmission(taskId: string): void {
    this.admissionQueue.get(taskId)?.reject(new Error("Task startup was cancelled"));
  }

  private assertWorkspaceAvailable(task: Task): void {
    this.assertAcceptingWork();
    const failed = [...this.checkpointJobs].find(job => job.kind === "write" && job.failure && sameWorkspace(job.task, task));
    if (failed) throw new Error("Workspace command closure is unconfirmed", { cause: failed.failure });
    const transition = [...this.workspaceTransitions].find(([owner]) => sameWorkspace(owner, task));
    if (transition) {
      throw new Error(`This workspace is being ${transition[1]}. Wait for the operation to finish.`);
    }
  }

  private assertAcceptingWork(): void {
    if (this.shutdownPromise) throw new Error("Agent service is shutting down");
  }

  private async workspaceOperation<T>(operation: () => Promise<T>): Promise<T> {
    this.assertAcceptingWork();
    const pending = operation();
    this.workspaceOperations.add(pending);
    try { return await pending; } finally { this.workspaceOperations.delete(pending); }
  }

  /** Stops native providers on both hosts without resending queued work. */
  shutdown(): Promise<void> {
    if (this.shutdownPromise) return this.shutdownPromise;
    // Set the admission barrier before cancellation can synchronously release
    // a slot. A second quit joins this promise instead of starting cleanup again.
    this.shutdownPromise = Promise.resolve().then(() => this.shutdownAttempt());
    return this.shutdownPromise;
  }

  private async shutdownAttempt(): Promise<void> {
    // Queries owe no durable write. Stop them while accepted mutations finish
    // through their enclosing workspace/input jobs and private-index cleanup.
    for (const job of this.checkpointJobs) if (job.kind === "query") job.controller.abort(new WorkspaceCommandCancelledError());
    const scripts = this.workspaceScripts.shutdown();
    const setupJobs = [...this.workspaceSetups.values()].map(job => job.promise.catch(error => { if (!(error instanceof WorkspaceScriptCancelledError)) throw error; }));
    const conversations = [...this.tasks.values()].filter(task => task.agentType !== "workflow");
    const startups = [...this.startups.values()].map(startup => startup.promise.catch(() => {}));
    const submissions = [...this.pendingSubmissions.values()].flatMap(pending => [...pending].map(input => input.settled));
    const active = new Set(conversations.filter(task => {
      const startup = this.startups.get(task.id);
      return this.hasActiveTurn(task) || this.pendingSubmissions.has(task.id)
        || Boolean(startup && (startup.dispatched || task.pendingInput || startup.kind === "goal"));
    }));
    for (const task of this.tasks.values()) {
      this.clearTimeout(task.id);
      this.cancelPendingSubmissions(task.id);
      this.admissionQueue.get(task.id)?.reject(new Error("Agent service is shutting down"));
    }
    const closing = conversations.map(async task => {
      if (this.cancellations.has(task.id) || active.has(task)) await this.cancelTask(task.id);
      else {
        await this.closeGoalReaders(task.id);
        await this.retireAdapter(task.id);
      }
    });
    // Closing a metadata transport rejects its read RPC normally. Await that
    // operation too, while reporting physical-close failures through `closing`.
    const reads = [...this.goalReads.values()].map(read => read.catch(() => {}));
    const workspaceOperations = [...this.workspaceOperations].map(operation => operation.catch(error => {
      // Quit cancels a running archive hook but still awaits its physical owner.
      // Its interrupted archive is not a failure to shut the application down.
      if (!(error instanceof WorkspaceScriptCancelledError)) throw error;
    }));
    // Git writers can outlive their CLI, including creation/archive already
    // accepted before the barrier. Settle every owner even if a sibling fails.
    const results = await Promise.allSettled([scripts, ...setupJobs, ...closing, ...startups, ...submissions,
      ...this.inputRecovery.values(), ...reads, ...[...this.goalControls.values()].map(control => control.promise.catch(() => {})),
      ...workspaceOperations]);
    const snapshots = await Promise.allSettled(this.postTurnSnapshots.values());
    // Accepted restore/archive/ref publication must finish; only preparation
    // receives cancellation. Fence new internal jobs after their producers close.
    this.checkpointClosing = true;
    const commands = await Promise.allSettled([this.waitCheckpointJobs()]);
    await this.persistTasks();
    const errors = [...results, ...snapshots, ...commands].filter((result): result is PromiseRejectedResult => result.status === "rejected");
    if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Agent shutdown failed");
  }

  private workspaceConversations(task: Task, entireWorkspace = false): Task[] {
    return entireWorkspace || this.ownsTaskWorktree(task)
      ? [...this.tasks.values()].filter(candidate => sameWorkspace(candidate, task))
      : [task];
  }

  private async closeWorkspaceConversations(task: Task, entireWorkspace = false): Promise<Task[]> {
    const conversations = this.workspaceConversations(task, entireWorkspace);
    await Promise.all(conversations.map(conversation => this.recoverTaskInput(conversation.id)));
    const starting = conversations.flatMap(conversation => this.startups.get(conversation.id)?.promise ?? []);
    const preparing = conversations.flatMap(conversation => this.setupWaiters(conversation.id).map(waiter => this.settleSetupWaiter(waiter)));
    const submitting = conversations.flatMap(conversation => [...(this.pendingSubmissions.get(conversation.id) ?? [])].map(submission => submission.settled));
    const active = new Set(conversations.filter(conversation => !["archived", "applied", "discarded"].includes(conversation.status)
      && (this.startups.has(conversation.id) || this.pendingSubmissions.has(conversation.id)
        || ["created", "queued", "running", "iterating"].includes(conversation.status))));
    // Withdraw every queued/input job before releasing any process slot. Wait
    // for checkpoint jobs too: closing the CLI alone cannot stop their writes.
    for (const conversation of conversations) {
      this.clearTimeout(conversation.id);
      this.cancelPendingSubmissions(conversation.id);
      this.cancelQueuedAdmission(conversation.id);
      if (active.has(conversation)) conversation.status = "stopped";
    }
    const closing = conversations.map(async conversation => {
      if (active.has(conversation)) await this.cancelTask(conversation.id);
      else {
        await this.closeGoalReaders(conversation.id);
        await this.retireAdapter(conversation.id);
      }
    });
    if (!task.isRemote) closing.push(this.workspaceScripts.close(task, this.ownsTaskWorktree(task)));
    await Promise.all([...closing, ...preparing, ...submitting, ...starting.map(startup => startup.catch(() => {}))]);
    await Promise.all(conversations.map(conversation => this.postTurnSnapshots.get(conversation.id)));
    await this.waitCheckpointJobs(conversations);
    this.migrateWorkspaceMetadata(task);
    return conversations;
  }

  private migrateWorkspaceMetadata(task: Task): void {
    if (task.isRemote) return;
    const owner = this.ownedWorkspaceTask(task);
    if (owner && fs.existsSync(owner.worktreePath)) migrateWorktreeMetadata(owner.id, owner.worktreePath);
  }

  private assertLocalWorkspace(task: Task): void {
    if (task.isRemote || fs.existsSync(task.worktreePath)) return;
    const action = this.ownedWorkspaceTask(task) ? "Use Retry to restore its saved branch." : "Restore the directory before retrying.";
    throw new Error(`Workspace path "${task.worktreePath}" does not exist. ${action}`);
  }

  private ownedWorkspaceTask(task: Task): Task | undefined {
    return this.ownsTaskWorktree(task) ? task : [...this.tasks.values()].find(candidate =>
      this.ownsTaskWorktree(candidate) && sameWorkspace(candidate, task));
  }

  private ownsTaskWorktree(task: Task): boolean {
    return !task.worktreeMode || task.worktreeMode === "auto";
  }
}
