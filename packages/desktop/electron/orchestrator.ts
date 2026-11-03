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
  AgentEvent,
  AgentType,
  createAdapter,
} from "./orchestrator-adapters";
import { CodexAppServerAdapter } from "./codex-app-server-adapter";
import { agentRegistry } from "./orchestrator-registry";
import { loadAgentConfig, migrateSettings } from "./orchestrator-settings";
import { CheckpointManager } from "./orchestrator-checkpoints";
import { createWorktree, removeWorktree, readTaskMetadata } from "./orchestrator-worktree";
import { loadNonconcurrentRun } from "./orchestrator-scripts";
import { parseRepoId } from "./ssh-router";
import type { RpcRouter } from "./ssh-router";
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
  /** Multi-agent support: additional agents in this workspace. */
  agents?: Array<{ id: string; type: string; role?: string; status?: string; childTaskId?: string }>;
  /** Current agent turn number, incremented on each iteration. */
  turnNumber?: number;
  /** True when the task targets a remote repo (worktree lives on remote). */
  isRemote?: boolean;
  /** SSH profile ID for remote tasks. */
  remoteProfileId?: string;
  /**
   * Codex session thread id from `thread.started`. When set, iterations
   * resume the existing thread via `codex exec resume <id>` so prompt
   * caches stay warm and we don't re-send the previous prompt verbatim.
   */
  codexThreadId?: string;
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
  agents?: Array<{ id: string; type: string; role?: string; status?: string; childTaskId?: string }>;
  isRemote?: boolean;
  remoteProfileId?: string;
  codexThreadId?: string;
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
const POST_TURN_GIT_TIMEOUT_MS = 10_000;

async function execPostTurnGit(
  args: string[],
  cwd: string,
  options: { env?: NodeJS.ProcessEnv } = {},
): Promise<{ stdout: string; stderr: string }> {
  return execFile("git", args, {
    cwd,
    timeout: POST_TURN_GIT_TIMEOUT_MS,
    killSignal: "SIGTERM",
    env: {
      ...process.env,
      GIT_OPTIONAL_LOCKS: "0",
      ...options.env,
    },
  });
}

function buildTaskAgentArgs(agentType: AgentType, task: Task, configuredArgs?: string[]): string[] {
  const args = [...(configuredArgs ?? [])];
  if (task.model) {
    args.push("--model", task.model);
  }
  if (task.reasoningLevel) {
    if (agentType === "claude-code") {
      args.push("-c", `thinking_effort="${task.reasoningLevel}"`);
    } else if (agentType === "codex") {
      args.push("-c", `model_reasoning_effort="${task.reasoningLevel}"`);
    }
  }
  return args;
}

// ─── Orchestrator ────────────────────────────────────────────────────────────

export class Orchestrator extends EventEmitter {
  private tasks = new Map<string, Task>();
  private adapters = new Map<string, AgentAdapter>();
  private timeouts = new Map<string, ReturnType<typeof setTimeout>>();
  private checkpoints = new CheckpointManager();
  private readonly timeoutMs: number;
  private readonly maxConcurrent: number;
  private readonly taskDb: OrchestratorTaskDatabase;
  private rpcRouter: RpcRouter | undefined;
  private remoteScriptRunner: RemoteScriptRunner | undefined;

  /** Tracks which workspace currently has the active run process. */
  private activeRunTaskId: string | null = null;
  /** When true, only one workspace run process is allowed at a time. */
  private nonconcurrentRun = false;

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
      try {
        const snapshotBase = await this.checkpoints.snapshotHead(task);
        if (snapshotBase) {
          task.baseRef = snapshotBase;
        }
      } catch {
        // Best-effort. If snapshotting fails, the resolved HEAD base still
        // gives the diff view a useful fallback without touching the index.
      }
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

    const base = baseRef ?? "HEAD";
    const worktreeMode = options.worktreeMode ?? "auto";
    let branch = `agent/${id.slice(0, 8)}`;
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
        const result = await this.rpcRouter.route(repoPath, "worktree_create", {
          repo_id: repoPath,
          branch,
          base_ref: base,
          task_id: id,
        }) as { path: string; branch?: string; base_ref?: string };

        worktreePath = result.path;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        throw new Error(
          `Failed to create worktree on remote (${remoteInfo.profileId}): ${message}`,
        );
      }
    }

    // Resolve the actual base ref from the remote response if available.
    const resolvedBranch = branch;

    const now = Date.now();
    const task: Task = {
      id,
      prompt,
      agentType,
      status: "created",
      branch: resolvedBranch,
      worktreePath,
      worktreeMode,
      baseRef: base,
      baseBranch: branchNameFromBaseRef(base),
      repoPath,
      createdAt: now,
      lastActivityAt: now,
      events: [],
      feedback: [],
      todos: [],
      isRemote: true,
      remoteProfileId: remoteInfo.profileId,
    };

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

  /**
   * Starts a task: spawns the agent adapter in the worktree, wires
   * event listeners, sets up timeout, and updates status to "running".
   * If the max concurrent limit is reached, queues the task instead.
   *
   * For remote tasks (task.isRemote === true), sends a `task_spawn`
   * RPC to the remote agent instead of spawning a local process.
   * Events stream back as JSON-RPC notifications over the SSH channel.
   *
   * @param skipCheckpoint — When true, skip checkpoint creation (caller
   *   already created one, e.g. `iterate()`).
   */
  async startTask(taskId: string, { skipCheckpoint = false } = {}): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }
    if (task.status !== "created" && task.status !== "iterating" && task.status !== "queued") {
      throw new Error(
        `Cannot start task in status "${task.status}". Expected "created", "iterating", or "queued".`,
      );
    }

    // Enforce max concurrent limit.
    const runningCount = this.getRunningTaskCount();
    if (task.status !== "queued" && runningCount >= this.maxConcurrent) {
      task.status = "queued";
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });
      return;
    }

    // Remote tasks are spawned on the remote machine via RPC.
    if (task.isRemote) {
      await this.startRemoteTask(task, { skipCheckpoint });
      return;
    }

    // Resolve agent type — fall back to the default if the persisted type
    // is no longer registered (e.g. a user removed a manifest they used
    // to have). Without the fallback, createAdapter() throws and the task
    // hangs in "created" forever.
    let resolvedAgentType = task.agentType;
    if (!agentRegistry.has(resolvedAgentType)) {
      const registered = agentRegistry.listRegistered();
      const fallback = registered.find(r => r.type === "claude-code")?.type
        ?? registered[0]?.type;
      if (!fallback) {
        throw new Error(
          `Unknown agent type "${task.agentType}" and no registered agents available.`,
        );
      }
      this.emit("orchestrator:warning", {
        taskId,
        message: `Agent type "${task.agentType}" is not registered; falling back to "${fallback}".`,
      });
      resolvedAgentType = fallback;
      task.agentType = fallback;
      await this.persistTasks();
    }

    // Load per-agent configuration (binary path, extra args, env vars)
    // from settings so users can configure each agent independently.
    const agentConfig = loadAgentConfig(resolvedAgentType);
    const adapter = createAdapter(
      resolvedAgentType,
      agentConfig.binaryPath,
      undefined,
      buildTaskAgentArgs(resolvedAgentType, task, agentConfig.extraArgs),
      { env: agentConfig.env, optOutNotifications: agentConfig.optOutNotifications },
    );

    task.status = "running";
    task.startedAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });

    // Wire event listener.
    adapter.onEvent((event: AgentEvent) => {
      const previousEvent = task.events[task.events.length - 1];
      task.events.push(event);
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
      // Detect Codex resume failures so we can recover by starting a
      // fresh thread on the next iteration. Codex surfaces this as a
      // `turn.failed` whose error mentions the thread/session id.
      if (
        event.type === "error"
        && task.agentType === "codex"
        && task.codexThreadId
        && typeof event.error === "string"
        && /thread|session|resume/i.test(event.error)
        && /(not\s+found|expired|invalid|unknown)/i.test(event.error)
      ) {
        this.emit("orchestrator:warning", {
          taskId,
          message: `Codex thread "${task.codexThreadId}" could not be resumed; subsequent iterations will start a fresh session.`,
        });
        task.codexThreadId = undefined;
        this.persistTasks().catch(() => {});
      }
      this.emit("orchestrator:event", { taskId, event });
      if (event.type === "done" && task.agentType === "codex") {
        this.completeCodexTurnFromDoneEvent(task, taskId, previousEvent).catch((err) => {
          this.emit("orchestrator:error", {
            taskId,
            message: `Failed to complete Codex turn: ${err instanceof Error ? err.message : String(err)}`,
          });
        });
      }
    });

    // Wire exit listener.
    adapter.onExit(async (code: number) => {
      this.clearTimeout(taskId);
      this.adapters.delete(taskId);

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

      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });

      // Start any queued tasks now that a slot is free.
      await this.processQueue();

      this.schedulePostTurnSnapshot(task, taskId);
    });

    // Create the pre-turn checkpoint before spawning, but only if
    // the caller hasn't already created one (e.g. iterate() creates its own
    // checkpoint before calling startTask to avoid double-creation and
    // SHA inconsistency between the UI event and the git ref).
    //
    // For the first turn (turnNumber === 0), skip the full snapshot — the
    // base ref already captures the pre-agent state, so a heavyweight
    // `git add -A` is unnecessary.
    let checkpointPromise: Promise<void> | null = null;
    if (!skipCheckpoint) {
      const turnNum = task.turnNumber ?? 0;
      if (turnNum === 0) {
        // First turn: use the base ref directly as the checkpoint without
        // running git add -A. This saves 1-3s on large repos.
        checkpointPromise = this.checkpoints.createLightweightCheckpoint(task, turnNum)
          .then(({ ref, sha }) => {
            this.emit("orchestrator:checkpoint-created", { taskId, turnNumber: turnNum, ref, sha });
          })
          .catch((err) => {
            this.emit("orchestrator:warning", {
              taskId,
              message: `Checkpoint creation failed: ${err instanceof Error ? err.message : String(err)}`,
            });
          });
      } else {
        checkpointPromise = this.checkpoints.createCheckpoint(task, turnNum)
          .then(({ ref, sha }) => {
            this.emit("orchestrator:checkpoint-created", { taskId, turnNumber: turnNum, ref, sha });
          })
          .catch((err) => {
            this.emit("orchestrator:warning", {
              taskId,
              message: `Checkpoint creation failed: ${err instanceof Error ? err.message : String(err)}`,
            });
          });
      }
    }

    if (checkpointPromise) await checkpointPromise;

    // Verify the worktree path exists before spawning. If it was created
    // with an older version (in /tmp) and has been cleaned up, or if
    // creation failed silently, we must not spawn the agent in the wrong
    // directory.
    if (!fs.existsSync(task.worktreePath)) {
      if (!this.ownsTaskWorktree(task)) {
        throw new Error(`Workspace path "${task.worktreePath}" does not exist.`);
      }
      // Attempt to recreate the worktree if it's missing.
      try {
        const recreatedPath = await createWorktree(task.repoPath, task.id, task.baseRef);
        task.worktreePath = recreatedPath;
        // Update the branch name from the new worktree metadata.
        const meta = readTaskMetadata(recreatedPath);
        if (meta?.originalBranch) {
          task.branch = meta.originalBranch;
        }
        await this.persistTasks();
        this.emit("orchestrator:warning", {
          taskId,
          message: `Worktree was missing — recreated at ${recreatedPath}`,
        });
      } catch (err) {
        throw new Error(
          `Worktree path "${task.worktreePath}" does not exist and could not be recreated: ${err instanceof Error ? err.message : String(err)}`,
        );
      }
    }

    // Spawn the agent with per-agent env vars injected (task-specific
    // secrets from Settings go in via `adapter.spawn(env)`).
    //
    // Codex resume optimisation: if we've previously captured a thread
    // id for this task, ask the adapter to resume it. This avoids
    // re-uploading the prior prompt and keeps the prompt cache warm.
    // The adapter will fall back to a fresh start if the resume fails
    // (we re-spawn without the hint when we observe a `turn.failed`
    // event referencing thread/session resolution).
    if (adapter instanceof CodexAppServerAdapter && task.codexThreadId) {
      adapter.resumeThreadId = task.codexThreadId;
    }
    adapter.spawn(task.prompt, task.worktreePath, agentConfig.env);
    this.adapters.set(taskId, adapter);

    // Set up timeout.
    this.setupTimeout(taskId);
  }

  /**
   * Starts a remote task by sending a `task_spawn` RPC to the remote
   * agent. The remote agent spawns the agent CLI (claude/codex) in the
   * remote worktree and streams events back as JSON-RPC notifications.
   *
   * Before spawning, runs the workspace setup script on the remote
   * machine (e.g., `pip install`, `conda activate`, GPU setup). If the
   * setup script fails, the task is marked as failed and not started.
   *
   * The orchestrator treats remote events identically to local events —
   * the renderer doesn't know the difference.
   */
  private async startRemoteTask(task: Task, { skipCheckpoint = false } = {}): Promise<void> {
    if (!this.rpcRouter) {
      throw new Error(
        "Cannot start remote task: SSH router not configured. " +
        "Ensure SSH remote connections are enabled.",
      );
    }

    if (!task.remoteProfileId) {
      throw new Error(
        `Cannot start remote task "${task.id}": missing remoteProfileId.`,
      );
    }

    // Create a checkpoint before spawning (same as local tasks).
    if (!skipCheckpoint) {
      try {
        const { ref, sha } = await this.checkpoints.createCheckpoint(task, task.turnNumber ?? 0);
        this.emit("orchestrator:checkpoint-created", { taskId: task.id, turnNumber: task.turnNumber ?? 0, ref, sha });
      } catch (err) {
        this.emit("orchestrator:warning", {
          taskId: task.id,
          message: `Checkpoint creation failed: ${err instanceof Error ? err.message : String(err)}`,
        });
      }
    }

    // Run setup script on the remote machine before spawning the agent.
    // If the setup script fails, the task is marked as failed.
    if (this.remoteScriptRunner) {
      const setupResult = await this.runRemoteSetupScript(task);
      if (setupResult && !setupResult.success) {
        task.status = "failed";
        task.completedAt = Date.now();
        await this.persistTasks();
        this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });
        this.emit("orchestrator:event", {
          taskId: task.id,
          event: {
            type: "error",
            timestamp: Date.now(),
            error: `Setup script failed (exit code ${setupResult.exitCode}): ${setupResult.error ?? setupResult.output}`,
          } satisfies AgentEvent,
        });
        throw new Error(
          `Remote setup script failed (exit code ${setupResult.exitCode}): ${setupResult.error ?? "see output"}`,
        );
      }
    }

    task.status = "running";
    task.startedAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });

    // Set up timeout (same as local tasks).
    this.setupTimeout(task.id);

    // Send the task_spawn RPC to the remote agent. The remote agent
    // spawns the CLI and streams events back as notifications.
    try {
      await this.rpcRouter.sendToRemoteAgent(task.remoteProfileId, "task_spawn", {
        task_id: task.id,
        prompt: task.prompt,
        agent_type: task.agentType,
        model: task.model,
        reasoning_level: task.reasoningLevel,
        worktree_path: task.worktreePath,
      });
    } catch (err) {
      this.clearTimeout(task.id);
      task.status = "failed";
      task.completedAt = Date.now();
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });

      const message = err instanceof Error ? err.message : String(err);
      this.emit("orchestrator:event", {
        taskId: task.id,
        event: {
          type: "error",
          timestamp: Date.now(),
          error: `Remote task spawn failed: ${message}`,
        } satisfies AgentEvent,
      });
      throw new Error(`Failed to spawn remote task: ${message}`);
    }
  }

  /**
   * Runs the workspace setup script on the remote machine for a task.
   * Loads the script configuration from the remote repo's
   * `crab-orchestrator.json` file, then executes it via the
   * `task_run_script` RPC.
   *
   * Returns null if no setup script is configured, or the script result.
   */
  private async runRemoteSetupScript(task: Task): Promise<RemoteScriptResult | null> {
    if (!this.rpcRouter || !this.remoteScriptRunner || !task.remoteProfileId) {
      return null;
    }

    // Load scripts config from the remote repo.
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

    if (!scripts.setupScript.trim()) {
      return null;
    }

    // Build environment variables for the script.
    const env = buildRemoteWorkspaceEnv(
      task.worktreePath,
      task.branch,
      remoteInfo.remotePath,
      task.id,
    );

    return this.remoteScriptRunner.runSetupScript(
      task.id,
      task.remoteProfileId,
      task.worktreePath,
      scripts.setupScript,
      env,
    );
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
   * Handles a JSON-RPC notification from the remote agent for a running
   * task. Maps remote event notifications to the orchestrator's event
   * system so the renderer sees them identically to local task events.
   *
   * Expected notification methods:
   * - `task_event`: an agent event (progress, tool use, text, etc.)
   * - `task_completed`: the remote agent CLI exited successfully
   * - `task_failed`: the remote agent CLI exited with an error
   *
   * Call this from the RPC router's notification listener.
   */
  async handleRemoteTaskNotification(notification: {
    profileId: string;
    method: string;
    params: unknown;
  }): Promise<void> {
    const params = notification.params as Record<string, unknown> | undefined;
    if (!params) return;

    const taskId = params.task_id as string | undefined;
    if (!taskId) return;

    const task = this.tasks.get(taskId);
    if (!task || !task.isRemote) return;

    switch (notification.method) {
      case "task_event": {
        // Forward the event to the renderer via the existing event system.
        const event = params.event as AgentEvent | undefined;
        if (event) {
          // Ensure timestamp is present.
          if (!event.timestamp) {
            event.timestamp = Date.now();
          }
          task.events.push(event);
          this.emit("orchestrator:event", { taskId, event });
        }
        break;
      }

      case "task_completed": {
        this.clearTimeout(taskId);
        const exitCode = typeof params.exit_code === "number" ? params.exit_code : 0;
        task.exitCode = exitCode;
        task.completedAt = Date.now();
        task.status = exitCode === 0 ? "reviewing" : "failed";
        await this.persistTasks();
        this.emit("orchestrator:task-status", { taskId, status: task.status });

        // Start any queued tasks now that a slot is free.
        await this.processQueue();
        break;
      }

      case "task_failed": {
        this.clearTimeout(taskId);
        const errorMsg = typeof params.error === "string" ? params.error : "Remote task failed";
        task.exitCode = typeof params.exit_code === "number" ? params.exit_code : 1;
        task.completedAt = Date.now();
        task.status = "failed";

        task.events.push({
          type: "error",
          timestamp: Date.now(),
          error: errorMsg,
        });
        this.emit("orchestrator:event", {
          taskId,
          event: { type: "error", timestamp: Date.now(), error: errorMsg },
        });

        await this.persistTasks();
        this.emit("orchestrator:task-status", { taskId, status: task.status });

        // Start any queued tasks now that a slot is free.
        await this.processQueue();
        break;
      }

      case "task_script_output": {
        // Forward script output notifications to the remote script runner
        // which emits events for the UI to display.
        if (this.remoteScriptRunner) {
          this.remoteScriptRunner.handleScriptNotification(notification);
        }
        break;
      }
    }
  }

  /**
   * Cancels a running task: sends SIGTERM to the agent process,
   * updates status to "stopped", preserves worktree for inspection.
   * For remote tasks, sends a `task_cancel` RPC to the remote agent.
   */
  async cancelTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    this.clearTimeout(taskId);

    if (task.isRemote && task.remoteProfileId && this.rpcRouter && this.ownsTaskWorktree(task)) {
      // Send cancel RPC to the remote agent (best-effort).
      try {
        await this.rpcRouter.sendToRemoteAgent(task.remoteProfileId, "task_cancel", {
          task_id: taskId,
        });
      } catch {
        // Best-effort — the remote may already be stopped.
      }
    } else {
      const adapter = this.adapters.get(taskId);
      if (adapter) {
        adapter.cancel();
        this.adapters.delete(taskId);
      }
    }

    task.status = "stopped";
    task.completedAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });

    // Start any queued tasks now that a slot is free.
    await this.processQueue();
  }

  /**
   * Sends feedback to a task and re-spawns the agent for iteration.
   * For remote tasks, sends a `task_iterate` RPC to the remote agent
   * which re-spawns the agent CLI in the same worktree with the
   * feedback as additional context.
   */
  async iterate(taskId: string, feedback: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    // Reject iteration for agents that don't support it.
    const capabilities = agentRegistry.getCapabilities(task.agentType);
    if (!capabilities.supportsIteration) {
      throw new Error(`Agent "${task.agentType}" does not support iteration.`);
    }

    task.feedback.push(feedback);

    // Emit a warning after 5 iterations.
    if (task.feedback.length >= 5) {
      this.emit("orchestrator:iteration-warning", {
        taskId,
        iterations: task.feedback.length,
        message: "Consider breaking this into smaller tasks",
      });
    }

    // Remote iteration reports its persisted iterating/running states; the
    // local spawn hint only covers adapter startup on this machine.
    if (!task.isRemote) {
      this.emit("orchestrator:task-status", { taskId, status: "spawning" });
    }

    // Complete the pre-turn snapshot before dispatching any new agent work;
    // otherwise a fast agent can overwrite the state that revert must restore.
    task.turnNumber = (task.turnNumber ?? 0) + 1;
    const checkpointPromise = this.checkpoints.createCheckpoint(task, task.turnNumber)
      .then(({ ref, sha }) => {
        this.emit("orchestrator:checkpoint-created", { taskId, turnNumber: task.turnNumber!, ref, sha });
      })
      .catch((err) => {
        this.emit("orchestrator:warning", {
          taskId,
          message: `Checkpoint creation failed: ${err instanceof Error ? err.message : String(err)}`,
        });
      });

    task.status = "iterating";
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });

    await checkpointPromise;

    // Remote tasks: send task_iterate RPC to the remote agent which
    // re-spawns the agent CLI in the same worktree with feedback.
    if (task.isRemote) {
      await this.iterateRemoteTask(task, feedback);
      return;
    }

    // Live adapter path: if the adapter is still running and exposes
    // iterate(), send the follow-up directly over the existing connection.
    // No prompt rewriting, no re-spawn, no task.prompt mutation.
    const adapter = this.adapters.get(taskId);
    if (adapter && typeof adapter.iterate === "function") {
      task.status = "running";
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId, status: task.status });

      adapter.iterate(feedback);

      return;
    }

    // Fallback: no live adapter (process died, app restarted, or adapter
    // doesn't support iterate). Build the next-turn prompt and re-spawn.
    //
    // Codex resume path: when we already have a thread id, we send only
    // the feedback as the prompt — the running session already has the
    // earlier turns in context. This is much cheaper than rebuilding a
    // "Previous task: …\n\nFeedback: …" wrapper, and the prompt cache
    // stays warm.
    //
    // Other agents (and Codex on its first iteration, before we've
    // observed `thread.started`) fall back to the explicit combined prompt.
    const isCodexResume = task.agentType === "codex" && Boolean(task.codexThreadId);
    if (isCodexResume) {
      task.prompt = feedback;
    } else {
      task.prompt = `Previous task: ${task.prompt}\n\nFeedback: ${feedback}\n\nPlease address the feedback.`;
    }

    await this.startTask(taskId, { skipCheckpoint: true });
  }

  /**
   * Sends a `task_iterate` RPC to the remote agent, which re-spawns
   * the agent CLI in the same remote worktree with the feedback as
   * additional context. Events stream back the same way as the initial
   * spawn (via `task_event` / `task_completed` / `task_failed` notifications).
   */
  private async iterateRemoteTask(task: Task, feedback: string): Promise<void> {
    if (!this.rpcRouter) {
      throw new Error(
        "Cannot iterate remote task: SSH router not configured. " +
        "Ensure SSH remote connections are enabled.",
      );
    }

    if (!task.remoteProfileId) {
      throw new Error(
        `Cannot iterate remote task "${task.id}": missing remoteProfileId.`,
      );
    }

    // Transition to running — the remote agent is re-spawning the CLI.
    task.status = "running";
    task.startedAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });

    // Set up timeout (same as initial spawn).
    this.setupTimeout(task.id);

    try {
      await this.rpcRouter.sendToRemoteAgent(task.remoteProfileId, "task_iterate", {
        task_id: task.id,
        feedback,
        agent_type: task.agentType,
        worktree_path: task.worktreePath,
      });
    } catch (err) {
      this.clearTimeout(task.id);
      task.status = "failed";
      task.completedAt = Date.now();
      await this.persistTasks();
      this.emit("orchestrator:task-status", { taskId: task.id, status: task.status });

      const message = err instanceof Error ? err.message : String(err);
      this.emit("orchestrator:event", {
        taskId: task.id,
        event: {
          type: "error",
          timestamp: Date.now(),
          error: `Remote task iteration failed: ${message}`,
        } satisfies AgentEvent,
      });
      throw new Error(`Failed to iterate remote task: ${message}`);
    }
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
    decision: { requestId: string; decision: "allow" | "allow_once" | "deny"; summary: string; note?: string },
  ): Promise<boolean> {
    const task = this.tasks.get(taskId);
    if (!task) return false;

    // Deliver the decision directly over the wire via the live adapter.
    const adapter = this.adapters.get(taskId);
    if (adapter && typeof adapter.respondApproval === "function") {
      adapter.respondApproval(decision.requestId, decision.decision);
      return true;
    }

    // No live adapter — can't deliver.
    return false;
  }

  /**
   * Marks a task as applied. Worktree cleanup is handled by the diff module.
   * For remote tasks, sends a `task_apply` RPC to the remote agent which
   * merges the branch and cleans up the worktree on the remote machine.
   */
  async applyTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    // Remote tasks: route apply to the remote agent.
    const ownsWorktree = this.ownsTaskWorktree(task);

    if (task.isRemote && task.remoteProfileId && this.rpcRouter && ownsWorktree) {
      try {
        await this.rpcRouter.sendToRemoteAgent(task.remoteProfileId, "task_apply", {
          repo_id: task.repoPath,
          task_id: taskId,
          branch: task.branch,
          base_ref: task.baseRef,
          base_branch: task.baseBranch,
          worktree_path: task.worktreePath,
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        throw new Error(`Remote apply failed: ${message}`);
      }
    }

    task.status = "applied";
    task.completedAt = Date.now();
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });
  }

  /**
   * Discards a task: removes worktree and branch, updates status.
   * For remote tasks, sends a `task_discard` RPC to the remote agent
   * which removes the worktree and deletes the branch on the remote machine.
   */
  async discardTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }

    this.clearTimeout(taskId);

    const adapter = this.adapters.get(taskId);
    if (adapter) {
      adapter.cancel();
      this.adapters.delete(taskId);
    }

    const ownsWorktree = this.ownsTaskWorktree(task);

    // Remote tasks: route discard to the remote agent when Crab created
    // the disposable worktree. Primary/existing workspaces are user-owned.
    if (task.isRemote && task.remoteProfileId && this.rpcRouter && ownsWorktree) {
      try {
        await this.rpcRouter.sendToRemoteAgent(task.remoteProfileId, "task_discard", {
          repo_id: task.repoPath,
          task_id: taskId,
          branch: task.branch,
          worktree_path: task.worktreePath,
        });
      } catch {
        // Best-effort — the remote may already be cleaned up or disconnected.
      }
    } else if (!task.isRemote) {
      // Local tasks: clean up locally.
      // Delete checkpoint refs before removing the worktree (needs repoPath).
      try {
        await this.checkpoints.cleanup(task);
      } catch {
        // Best-effort — refs may already be gone.
      }

      if (ownsWorktree) {
        try {
          await removeWorktree(task.repoPath, task.id);
        } catch {
          // Best-effort cleanup — worktree may already be gone.
        }
      }
    }

    task.status = "discarded";
    task.completedAt = Date.now();
    task.lastActivityAt = task.completedAt;
    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });
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

  /** Removes a task from the in-memory map and persists. */
  async removeTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (task) {
      try {
        await this.checkpoints.cleanup(task);
      } catch {
        // Best-effort — refs may already be gone.
      }
    }
    this.tasks.delete(taskId);
    this.rebuildChildTaskIds();
    await this.persistTasks();
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
    const now = Date.now();
    const child: Task = {
      id: `child-${crypto.randomUUID()}`,
      title: role ? `${role} sub-agent` : `${agentType} sub-agent`,
      prompt,
      agentType,
      status: "running",
      branch: parent.branch,
      worktreePath: parent.worktreePath,
      worktreeMode: parent.worktreeMode,
      baseRef: parent.baseRef,
      baseBranch: parent.baseBranch,
      repoPath: parent.repoPath,
      createdAt: now,
      startedAt: now,
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
    const adapter = this.adapters.get(taskId);
    if (!(adapter instanceof CodexAppServerAdapter)) {
      throw new Error("No active Codex connection for this task");
    }
    return adapter;
  }

  async forkThread(taskId: string): Promise<{ threadId: string }> {
    return this.getCodexAdapter(taskId).forkThread();
  }

  async rollbackThread(taskId: string, n: number): Promise<{ threadId: string }> {
    return this.getCodexAdapter(taskId).rollbackThread(n);
  }

  async compactThread(taskId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).compactThread();
  }

  async startReview(taskId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).startReview();
  }

  async setThreadName(taskId: string, name: string): Promise<{}> {
    return this.getCodexAdapter(taskId).setThreadName(name);
  }

  async setGoal(taskId: string, goal: string): Promise<{}> {
    return this.getCodexAdapter(taskId).setGoal(goal);
  }

  async getGoal(taskId: string): Promise<{ goal: string; tokenBudget?: number }> {
    return this.getCodexAdapter(taskId).getGoal();
  }

  async clearGoal(taskId: string): Promise<{}> {
    return this.getCodexAdapter(taskId).clearGoal();
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

  // ─── Nonconcurrent Run Mode ────────────────────────────────────────────

  /**
   * Loads the nonconcurrent run setting from the repository config.
   * Call this when the repo path is known (e.g., when starting a run).
   */
  loadNonconcurrentRunSetting(repoPath: string): void {
    this.nonconcurrentRun = loadNonconcurrentRun(repoPath);
  }

  /** Returns whether nonconcurrent run mode is enabled. */
  isNonconcurrentRunEnabled(): boolean {
    return this.nonconcurrentRun;
  }

  /** Returns the task ID of the currently active run, or null. */
  getActiveRunTaskId(): string | null {
    return this.activeRunTaskId;
  }

  /** Sets the active run task ID after a run process is started. */
  setActiveRunTaskId(taskId: string): void {
    this.activeRunTaskId = taskId;
  }

  /** Clears the active run task ID (when a run completes or is cancelled). */
  clearActiveRunTaskId(taskId: string): void {
    if (this.activeRunTaskId === taskId) {
      this.activeRunTaskId = null;
    }
  }

  /**
   * Stops the active run process if nonconcurrent mode is enabled and
   * a different workspace is starting a run. Sends SIGHUP, waits 200ms,
   * then SIGKILL if still running.
   *
   * Returns the stopped task ID if a stop occurred, or null otherwise.
   */
  async stopActiveRunIfNeeded(
    newTaskId: string,
    runningProcesses: Map<string, import("node:child_process").ChildProcess>,
  ): Promise<string | null> {
    if (!this.nonconcurrentRun) return null;
    if (!this.activeRunTaskId) return null;
    if (this.activeRunTaskId === newTaskId) return null;

    const stoppedTaskId = this.activeRunTaskId;
    const proc = runningProcesses.get(stoppedTaskId);

    if (proc && !proc.killed) {
      proc.kill("SIGHUP");
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          if (!proc.killed) {
            proc.kill("SIGKILL");
          }
          resolve();
        }, 200);

        proc.on("close", () => {
          clearTimeout(timer);
          resolve();
        });
      });

      runningProcesses.delete(stoppedTaskId);
    }

    this.activeRunTaskId = null;
    this.emit("orchestrator:nonconcurrent-stop", {
      stoppedTaskId,
      startedTaskId: newTaskId,
    });

    return stoppedTaskId;
  }

  // ─── Queue Management ──────────────────────────────────────────────────

  /** Returns the number of tasks currently in "running" status. */
  private getRunningTaskCount(): number {
    let count = 0;
    for (const task of this.tasks.values()) {
      if (task.status === "running") count++;
    }
    return count;
  }

  /** Starts queued tasks if there are free slots. */
  private async processQueue(): Promise<void> {
    const queued = Array.from(this.tasks.values())
      .filter((t) => t.status === "queued")
      .sort((a, b) => a.createdAt - b.createdAt);

    for (const task of queued) {
      if (this.getRunningTaskCount() >= this.maxConcurrent) break;
      await this.startTask(task.id);
    }
  }

  // ─── Timeout ─────────────────────────────────────────────────────────────

  private setupTimeout(taskId: string): void {
    this.clearTimeout(taskId);

    const timer = setTimeout(async () => {
      const task = this.tasks.get(taskId);
      if (!task || task.status !== "running") return;

      const adapter = this.adapters.get(taskId);
      if (adapter) {
        adapter.cancel();
        this.adapters.delete(taskId);
      }

      task.status = "failed";
      task.completedAt = Date.now();
      task.lastActivityAt = task.completedAt;
      await this.persistTasks();
      this.emit("orchestrator:timeout", { taskId });
      this.emit("orchestrator:task-status", { taskId, status: task.status });

      // Start any queued tasks now that a slot is free.
      await this.processQueue();
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
    if (!task.worktreePath || !fs.existsSync(task.worktreePath)) return;
    if (!this.ownsTaskWorktree(task)) return;

    const turnNum = task.turnNumber ?? 0;
    await execPostTurnGit(["add", "-A"], task.worktreePath);

    try {
      await execPostTurnGit(["diff", "--cached", "--quiet"], task.worktreePath);
      return;
    } catch (err) {
      // Exit code 1 means there are staged changes. Timeout or other git
      // failures are handled by the outer best-effort caller.
      if ((err as { code?: unknown }).code !== 1) throw err;
    }

    const commitMsg = `agent checkpoint: turn ${turnNum}`;
    await execPostTurnGit(["commit", "-m", commitMsg, "--allow-empty-message"], task.worktreePath, {
      env: {
        GIT_AUTHOR_NAME: "crab-agent",
        GIT_AUTHOR_EMAIL: "agent@crab.local",
        GIT_COMMITTER_NAME: "crab-agent",
        GIT_COMMITTER_EMAIL: "agent@crab.local",
      },
    });
    this.emit("orchestrator:turn-committed", { taskId, turnNumber: turnNum });
  }

  // ─── Persistence ─────────────────────────────────────────────────────────

  /** Saves task metadata to disk (excludes events array). */
  async persistTasks(): Promise<void> {
    const tasks: PersistedTask[] = Array.from(this.tasks.values()).map((task) => this.toPersistedTask(task));

    try {
      this.taskDb.replaceTasks(tasks);
    } catch (err) {
      this.emit("orchestrator:error", {
        taskId: null,
        message: `Failed to persist tasks: ${err instanceof Error ? err.message : String(err)}`,
      });
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
          agents: rest.agents,
          worktreeMode: rest.worktreeMode,
          isRemote: rest.isRemote,
          remoteProfileId: rest.remoteProfileId,
          codexThreadId: rest.codexThreadId,
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
        this.tasks.set(task.id, task);
      }
      this.rebuildChildTaskIds();
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
      todos: task.todos ?? [],
      prUrl: task.prUrl,
      prNumber: task.prNumber,
      agents: task.agents,
      isRemote: task.isRemote,
      remoteProfileId: task.remoteProfileId,
      codexThreadId: task.codexThreadId,
      uiState: task.uiState,
      workflowRunId: task.workflowRunId,
      workflowNodeRunId: task.workflowNodeRunId,
      planFirst: task.planFirst,
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

  private async completeCodexTurnFromDoneEvent(
    task: Task,
    taskId: string,
    previousEvent?: AgentEvent,
  ): Promise<void> {
    if (task.status !== "running" && task.status !== "iterating") return;

    this.clearTimeout(taskId);
    const failed = previousEvent?.type === "error";
    task.exitCode = failed ? 1 : 0;
    task.completedAt = Date.now();
    task.lastActivityAt = task.completedAt;
    task.status = failed ? "failed" : task.planFirst ? "planning" : "reviewing";

    await this.persistTasks();
    this.emit("orchestrator:task-status", { taskId, status: task.status });

    // Start any queued tasks now that a Codex app-server turn has finished.
    await this.processQueue();
    this.schedulePostTurnSnapshot(task, taskId);
  }

  private schedulePostTurnSnapshot(task: Task, taskId: string): void {
    // Auto-commit all changes after the user-visible lifecycle transition.
    // This is best-effort timeline support; it must not keep a completed
    // agent turn stuck in "running" if git/filter-process work blocks.
    this.commitTurnSnapshot(task, taskId).catch((err) => {
      this.emit("orchestrator:warning", {
        taskId,
        message: `Auto-commit after turn failed: ${err instanceof Error ? err.message : String(err)}`,
      });
    });
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

  private ownsTaskWorktree(task: Task): boolean {
    return !task.worktreeMode || task.worktreeMode === "auto";
  }
}
