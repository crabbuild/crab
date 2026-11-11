/**
 * SSH Connection Recovery — handles remote task interruption and
 * reconnection for the orchestrator.
 *
 * When an SSH connection drops while a remote task is running:
 * 1. The task is marked as "interrupted" (NOT "failed") — the remote
 *    worktree is preserved.
 * 2. On reconnection, a `task_check_orphaned` RPC is sent to the
 *    remote agent to detect if the worktree still exists and if the
 *    agent CLI is still running.
 * 3. Based on the response:
 *    - Worktree exists + agent running → re-attach to event stream
 *    - Worktree exists + agent stopped → emit "Resume or Discard" event
 *    - Worktree gone → mark task as "failed"
 *
 * Key principle: the remote worktree is NEVER automatically deleted
 * on connection loss. The user always gets to choose what to do.
 */

import { EventEmitter } from "node:events";
import type { SshSessionManager, SshConnectionStatus } from "./ssh-manager";
import type { Orchestrator, Task } from "./orchestrator";
import type { RpcRouter } from "./ssh-router";

// ── Types ────────────────────────────────────────────────────────

export interface OrphanedTaskInfo {
  taskId: string;
  worktreePath: string;
  branch: string;
  worktreeExists: boolean;
  agentRunning: boolean;
}

export interface RecoveryPrompt {
  taskId: string;
  profileId: string;
  prompt: string;
  branch: string;
  worktreePath: string;
  /** "resume" re-spawns the agent in the existing worktree. "discard" removes it. */
  options: ["resume", "discard"];
}

export interface ConnectionRecoveryEvents {
  /** Emitted when a task needs user decision after reconnection. */
  "ssh:task-recovery-prompt": (payload: RecoveryPrompt) => void;
  /** Emitted when a task is re-attached to a still-running remote agent. */
  "ssh:task-reattached": (payload: { taskId: string; profileId: string }) => void;
  /** Emitted when a task's worktree was lost and the task is marked failed. */
  "ssh:task-worktree-lost": (payload: { taskId: string; profileId: string }) => void;
}

// ── Connection Recovery Manager ──────────────────────────────────

/**
 * Monitors SSH connection lifecycle events and manages remote task
 * recovery on disconnection/reconnection.
 *
 * Events:
 * - "ssh:task-recovery-prompt" — user must choose Resume or Discard
 * - "ssh:task-reattached" — task was re-attached to running agent
 * - "ssh:task-worktree-lost" — worktree gone, task marked failed
 */
export class SshConnectionRecovery extends EventEmitter {
  private readonly sessionManager: SshSessionManager;
  private readonly orchestrator: Orchestrator;
  private readonly rpcRouter: RpcRouter;

  constructor(
    sessionManager: SshSessionManager,
    orchestrator: Orchestrator,
    rpcRouter: RpcRouter,
  ) {
    super();
    this.sessionManager = sessionManager;
    this.orchestrator = orchestrator;
    this.rpcRouter = rpcRouter;

    this.attachListeners();
  }

  /**
   * Listen for SSH connection status changes. When a connection drops,
   * interrupt running tasks. When it reconnects, check for orphaned tasks.
   */
  private attachListeners(): void {
    this.sessionManager.on("ssh:status-changed", (payload: {
      profileId: string;
      status: SshConnectionStatus;
      error?: string;
    }) => {
      if (payload.status === "disconnected" || payload.status === "error" || payload.status === "reconnecting") {
        this.handleDisconnection(payload.profileId).catch((err) => {
          console.error(
            "[ssh-connection-recovery] Error handling disconnection:",
            err instanceof Error ? err.message : String(err),
          );
        });
      }

      if (payload.status === "connected") {
        this.handleReconnection(payload.profileId).catch((err) => {
          console.error(
            "[ssh-connection-recovery] Error handling reconnection:",
            err instanceof Error ? err.message : String(err),
          );
        });
      }
    });
  }

  /**
   * When an SSH connection drops, find all running remote tasks for
   * that profile and mark them as "interrupted". The remote worktree
   * is preserved — we never auto-delete on connection loss.
   */
  async handleDisconnection(profileId: string): Promise<void> {
    const allTasks = this.orchestrator.getAllTasks();
    const runningRemoteTasks = allTasks.filter(
      (task) =>
        task.isRemote &&
        task.remoteProfileId === profileId &&
        (task.status === "running" || task.status === "iterating"),
    );

    if (runningRemoteTasks.length === 0) return;
    // Mark siblings before yielding so a fast reconnect sees the entire set.
    // Recovery may advance while persistence waits; do not publish stale state.
    for (const task of runningRemoteTasks) {
      task.status = "interrupted";
      task.completedAt = undefined;
    }
    await this.orchestrator.persistTasks();
    for (const task of runningRemoteTasks) {
      if (task.status !== "interrupted") continue;
      this.orchestrator.emit("orchestrator:task-status", { taskId: task.id, status: "interrupted" });
    }
  }

  /**
   * When an SSH connection is re-established, check for interrupted
   * tasks and determine their state on the remote machine.
   *
   * For each interrupted task:
   * - Send `task_check_orphaned` RPC to the remote agent
   * - Based on the response, either re-attach, prompt user, or mark failed
   */
  async handleReconnection(profileId: string): Promise<void> {
    const allTasks = this.orchestrator.getAllTasks();
    const interruptedTasks = allTasks.filter(
      (task) =>
        task.isRemote &&
        task.remoteProfileId === profileId &&
        task.status === "interrupted",
    );

    if (interruptedTasks.length === 0) return;

    for (const task of interruptedTasks) {
      await this.checkOrphanedTask(task);
    }
  }

  /**
   * Send a `task_check_orphaned` RPC to the remote agent to determine
   * the state of an interrupted task's worktree and agent process.
   */
  private async checkOrphanedTask(task: Task): Promise<void> {
    if (!task.remoteProfileId) return;

    try {
      const result = await this.rpcRouter.sendToRemoteAgent(
        task.remoteProfileId,
        "task_check_orphaned",
        {
          task_id: task.id,
          worktree_path: task.worktreePath,
          branch: task.branch,
        },
      ) as OrphanedTaskInfo;

      if (!result.worktreeExists) {
        // Worktree is gone — mark as failed.
        task.status = "failed";
        task.completedAt = Date.now();
        await this.orchestrator.persistTasks();
        this.orchestrator.emit("orchestrator:task-status", {
          taskId: task.id,
          status: "failed",
        });
        this.emit("ssh:task-worktree-lost", {
          taskId: task.id,
          profileId: task.remoteProfileId,
        });
        return;
      }

      if (result.agentRunning) {
        // Agent is still running — re-attach to the event stream.
        task.status = "running";
        await this.orchestrator.persistTasks();
        this.orchestrator.emit("orchestrator:task-status", {
          taskId: task.id,
          status: "running",
        });
        this.emit("ssh:task-reattached", {
          taskId: task.id,
          profileId: task.remoteProfileId,
        });
        return;
      }

      // Worktree exists but agent is not running — prompt user.
      this.emit("ssh:task-recovery-prompt", {
        taskId: task.id,
        profileId: task.remoteProfileId,
        prompt: task.prompt,
        branch: task.branch,
        worktreePath: task.worktreePath,
        options: ["resume", "discard"],
      } satisfies RecoveryPrompt);
    } catch (err) {
      // If the RPC fails (e.g., agent not started yet), keep the task
      // as interrupted — we'll retry on the next status check or the
      // user can manually trigger recovery.
      console.error(
        `[ssh-connection-recovery] Failed to check orphaned task ${task.id}:`,
        err instanceof Error ? err.message : String(err),
      );
    }
  }

  /**
   * Resume an interrupted task: re-spawn the agent CLI in the existing
   * remote worktree. Called when the user chooses "Resume".
   */
  async resumeTask(taskId: string): Promise<void> {
    const task = this.orchestrator.getTask(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }
    if (task.status !== "interrupted") {
      throw new Error(
        `Cannot resume task in status "${task.status}". Expected "interrupted".`,
      );
    }

    // Re-use the orchestrator's startTask which handles remote spawning.
    task.status = "created";
    await this.orchestrator.persistTasks();
    await this.orchestrator.startTask(taskId);
  }

  /**
   * Discard an interrupted task: remove the worktree on the remote
   * machine. Called when the user chooses "Discard".
   */
  async discardTask(taskId: string): Promise<void> {
    const task = this.orchestrator.getTask(taskId);
    if (!task) {
      throw new Error(`Task not found: ${taskId}`);
    }
    if (task.status !== "interrupted") {
      throw new Error(
        `Cannot discard task in status "${task.status}". Expected "interrupted".`,
      );
    }

    await this.orchestrator.discardTask(taskId);
  }

  /**
   * Manually trigger orphaned task detection for a profile.
   * Useful when the automatic detection on reconnection misses tasks
   * (e.g., if the agent wasn't ready when reconnection happened).
   */
  async retryOrphanedDetection(profileId: string): Promise<void> {
    await this.handleReconnection(profileId);
  }
}
