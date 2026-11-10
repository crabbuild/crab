/**
 * Remote Orchestrator — extends the orchestrator to support remote repos
 * by routing worktree creation and agent CLI spawning through the SSH
 * connection.
 *
 * When a task targets a remote repo (ssh:// prefix), all orchestrator
 * operations (worktree create, agent spawn, diff, apply/discard) are
 * forwarded to the remote agent over the SSH tunnel.
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import { parseRepoId } from "./ssh-types";

// ── Types ────────────────────────────────────────────────────────

export type RemoteTaskStatus =
  | "created"
  | "worktree_creating"
  | "running"
  | "reviewing"
  | "iterating"
  | "applied"
  | "discarded"
  | "failed"
  | "orphaned";

export interface RemoteAgentInfo {
  name: string;
  path: string;
  available: boolean;
}

export interface RemoteTask {
  id: string;
  repoId: string;
  profileId: string;
  prompt: string;
  agentType: string;
  status: RemoteTaskStatus;
  branch: string;
  worktreePath?: string;
  baseRef: string;
  baseBranch?: string;
  createdAt: number;
  startedAt?: number;
  completedAt?: number;
}

export interface RemoteOrchestratorEvents {
  "remote-task:status-changed": (task: RemoteTask) => void;
  "remote-task:activity": (payload: { taskId: string; event: unknown }) => void;
  "remote-task:diff-ready": (payload: { taskId: string; diff: string }) => void;
}

function branchNameFromBaseRef(baseRef: string): string | undefined {
  if (baseRef === "HEAD" || /^[0-9a-f]{7,40}$/i.test(baseRef)) {
    return undefined;
  }
  return baseRef.replace(/^refs\/heads\//, "");
}

// ── Remote Orchestrator ──────────────────────────────────────────

export class RemoteOrchestrator extends EventEmitter {
  private tasks = new Map<string, RemoteTask>();
  private router: RpcRouter;

  constructor(router: RpcRouter, _sessionManager: SshSessionManager) {
    super();
    this.router = router;
  }

  /**
   * Detect available agent CLIs on the remote server.
   * Checks for `claude` and `codex` binaries on the remote PATH.
   */
  async detectRemoteAgents(profileId: string): Promise<RemoteAgentInfo[]> {
    const agents: RemoteAgentInfo[] = [];
    const repoId = `ssh://${profileId}/`;

    for (const name of ["claude", "codex"]) {
      try {
        const result = await this.router.route(repoId, "exec_command", {
          command: `which ${name}`,
        }) as { stdout?: string; exitCode?: number };

        agents.push({
          name,
          path: result.stdout?.trim() ?? "",
          available: result.exitCode === 0,
        });
      } catch {
        agents.push({ name, path: "", available: false });
      }
    }

    return agents;
  }

  /**
   * Create a remote task: creates a worktree on the remote machine
   * and spawns the agent CLI in it.
   */
  async createTask(params: {
    repoId: string;
    prompt: string;
    agentType: string;
    baseRef?: string;
  }): Promise<RemoteTask> {
    const parsed = parseRepoId(params.repoId);
    if (!parsed) {
      throw new Error("createTask requires a remote repo (ssh:// prefix)");
    }

    const taskId = crypto.randomUUID();
    const branch = `task/${taskId.slice(0, 8)}`;
    const baseRef = params.baseRef ?? "HEAD";

    const task: RemoteTask = {
      id: taskId,
      repoId: params.repoId,
      profileId: parsed.profileId,
      prompt: params.prompt,
      agentType: params.agentType,
      status: "created",
      branch,
      baseRef,
      baseBranch: branchNameFromBaseRef(baseRef),
      createdAt: Date.now(),
    };

    this.tasks.set(taskId, task);
    this.emitStatusChange(task);

    // Create worktree on remote
    task.status = "worktree_creating";
    this.emitStatusChange(task);

    try {
      const worktreeResult = await this.router.route(params.repoId, "worktree_create", {
        repo_id: params.repoId,
        branch,
        base_ref: task.baseRef,
      }) as { path: string };

      task.worktreePath = worktreeResult.path;
    } catch (err) {
      task.status = "failed";
      this.emitStatusChange(task);
      throw err;
    }

    return task;
  }

  /**
   * Start the agent CLI on the remote machine in the task's worktree.
   * Events stream back over the SSH tunnel as JSON-RPC notifications.
   *
   * The remote agent receives the task_spawn RPC and:
   * 1. Spawns the agent CLI (claude/codex) in the worktree
   * 2. Streams events back as `task_event` notifications
   * 3. Sends `task_completed` or `task_failed` when the CLI exits
   */
  async startTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);
    if (!task.worktreePath) throw new Error("Task has no worktree");

    task.status = "running";
    task.startedAt = Date.now();
    this.emitStatusChange(task);

    try {
      await this.router.route(task.repoId, "task_spawn", {
        task_id: taskId,
        prompt: task.prompt,
        agent_type: task.agentType,
        worktree_path: task.worktreePath,
      });
    } catch (err) {
      task.status = "failed";
      this.emitStatusChange(task);
      throw err;
    }
  }

  /**
   * Compute the diff for a completed task (runs on remote).
   */
  async computeDiff(taskId: string): Promise<string> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);

    const result = await this.router.route(task.repoId, "task_compute_diff", {
      repo_id: task.repoId,
      task_id: taskId,
      branch: task.branch,
      base_ref: task.baseRef,
    }) as { diff: string };

    task.status = "reviewing";
    this.emitStatusChange(task);
    this.emit("remote-task:diff-ready", { taskId, diff: result.diff });

    return result.diff;
  }

  /**
   * Apply the task: merge the worktree branch into the base branch
   * on the remote machine.
   */
  async applyTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);

    await this.router.route(task.repoId, "task_apply", {
      repo_id: task.repoId,
      task_id: taskId,
      branch: task.branch,
      base_ref: task.baseRef,
      base_branch: task.baseBranch,
    });

    task.status = "applied";
    task.completedAt = Date.now();
    this.emitStatusChange(task);
  }

  /**
   * Discard the task: remove the worktree and branch on the remote.
   */
  async discardTask(taskId: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);

    await this.router.route(task.repoId, "task_discard", {
      repo_id: task.repoId,
      task_id: taskId,
      branch: task.branch,
      worktree_path: task.worktreePath,
    });

    task.status = "discarded";
    task.completedAt = Date.now();
    this.emitStatusChange(task);
  }

  /**
   * Send feedback to iterate on a task. Re-spawns the agent CLI
   * in the same remote worktree with additional context.
   */
  async iterateTask(taskId: string, feedback: string): Promise<void> {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`Task not found: ${taskId}`);

    task.status = "iterating";
    this.emitStatusChange(task);

    await this.router.route(task.repoId, "task_iterate", {
      repo_id: task.repoId,
      task_id: taskId,
      agent_type: task.agentType,
      feedback,
      worktree_path: task.worktreePath,
    });

    task.status = "running";
    this.emitStatusChange(task);
  }

  /**
   * Execute a workspace script on the remote machine (e.g., pip install,
   * conda activate, GPU setup).
   */
  async runScript(
    _profileId: string,
    repoId: string,
    script: string,
    cwd?: string,
  ): Promise<{ stdout: string; stderr: string; exitCode: number }> {
    const result = await this.router.route(repoId, "exec_script", {
      repo_id: repoId,
      script,
      cwd,
    }) as { stdout: string; stderr: string; exitCode: number };

    return result;
  }

  /**
   * Handle reconnection: detect orphaned tasks on the remote and
   * offer resume or discard.
   */
  async detectOrphanedTasks(profileId: string, repoId: string): Promise<RemoteTask[]> {
    try {
      const result = await this.router.route(repoId, "task_list_orphaned", {
        repo_id: repoId,
      }) as { tasks: Array<{ id: string; branch: string; worktree_path: string }> };

      const orphaned: RemoteTask[] = [];
      for (const remote of result.tasks) {
        const existing = this.tasks.get(remote.id);
        if (existing) {
          existing.status = "orphaned";
          this.emitStatusChange(existing);
          orphaned.push(existing);
        } else {
          const task: RemoteTask = {
            id: remote.id,
            repoId,
            profileId,
            prompt: "(recovered)",
            agentType: "unknown",
            status: "orphaned",
            branch: remote.branch,
            worktreePath: remote.worktree_path,
            baseRef: "HEAD",
            createdAt: Date.now(),
          };
          this.tasks.set(task.id, task);
          orphaned.push(task);
        }
      }

      return orphaned;
    } catch {
      return [];
    }
  }

  /**
   * Handle activity events from the remote agent for a running task.
   * Called when the RPC router receives a JSON-RPC notification from
   * the remote agent with task-related events.
   *
   * Notification methods:
   * - `task_event`: agent event (text, tool_use, file_edit, etc.)
   * - `task_completed`: agent CLI exited successfully (exit code 0)
   * - `task_failed`: agent CLI exited with an error
   */
  handleRemoteNotification(notification: {
    profileId: string;
    method: string;
    params: unknown;
  }): void {
    const params = notification.params as Record<string, unknown> | undefined;
    if (!params) return;

    const taskId = params.task_id as string | undefined;
    if (!taskId) return;

    const task = this.tasks.get(taskId);
    if (!task) return;

    switch (notification.method) {
      case "task_event": {
        const event = params.event as Record<string, unknown> | undefined;
        if (event) {
          this.emit("remote-task:activity", { taskId, event });
        }
        break;
      }

      case "task_completed": {
        task.status = "reviewing";
        task.completedAt = Date.now();
        this.emitStatusChange(task);
        break;
      }

      case "task_failed": {
        task.status = "failed";
        task.completedAt = Date.now();
        this.emitStatusChange(task);
        const error = typeof params.error === "string" ? params.error : "Remote task failed";
        this.emit("remote-task:activity", {
          taskId,
          event: { type: "error", timestamp: Date.now(), error },
        });
        break;
      }
    }
  }

  /**
   * Handle activity events from the remote agent for a running task.
   * @deprecated Use handleRemoteNotification instead.
   */
  handleRemoteActivity(taskId: string, event: unknown): void {
    const task = this.tasks.get(taskId);
    if (!task) return;

    this.emit("remote-task:activity", { taskId, event });
  }

  /**
   * Get a task by ID.
   */
  getTask(taskId: string): RemoteTask | undefined {
    return this.tasks.get(taskId);
  }

  /**
   * List all tasks for a given profile.
   */
  listTasks(profileId?: string): RemoteTask[] {
    const all = [...this.tasks.values()];
    if (profileId) {
      return all.filter((t) => t.profileId === profileId);
    }
    return all;
  }

  private emitStatusChange(task: RemoteTask): void {
    this.emit("remote-task:status-changed", task);
  }
}
