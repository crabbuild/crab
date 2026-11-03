/**
 * Remote Workspace Scripts — executes setup/run/archive scripts on the
 * remote machine via the remote agent's `task_run_script` RPC.
 *
 * Workspace scripts run at different lifecycle points:
 * - **setup**: runs before the agent CLI starts (e.g., `pip install`, `conda activate`)
 * - **run**: runs alongside or after the agent (e.g., `make test`)
 * - **archive**: runs after the task completes (e.g., cleanup, save artifacts)
 *
 * For remote tasks, these scripts execute on the remote machine, not locally.
 * Script output streams back as JSON-RPC notifications (same as task events).
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import { type OrchestratorScripts } from "./orchestrator-scripts";

// ── Types ────────────────────────────────────────────────────────

export type RemoteScriptType = "setup" | "run" | "archive";

export interface RemoteScriptRequest {
  taskId: string;
  profileId: string;
  scriptType: RemoteScriptType;
  command: string;
  worktreePath: string;
  env?: Record<string, string>;
}

export interface RemoteScriptResult {
  success: boolean;
  exitCode: number;
  output: string;
  error?: string;
}

export interface RemoteScriptEvent {
  taskId: string;
  scriptType: RemoteScriptType;
  type: "stdout" | "stderr" | "exit";
  data?: string;
  exitCode?: number;
}

// ── Remote Script Runner ─────────────────────────────────────────

export class RemoteScriptRunner extends EventEmitter {
  private router: RpcRouter;

  constructor(router: RpcRouter) {
    super();
    this.router = router;
  }

  /**
   * Execute a workspace script on the remote machine via the
   * `task_run_script` RPC. The remote agent runs the command in a
   * shell within the specified worktree directory.
   *
   * Script output streams back as notifications and is also collected
   * into the returned result.
   *
   * @returns The script result with exit code and collected output.
   * @throws If the RPC call itself fails (network error, timeout).
   */
  async runScript(request: RemoteScriptRequest): Promise<RemoteScriptResult> {
    const { taskId, profileId, scriptType, command, worktreePath, env } = request;

    // Skip empty scripts.
    if (!command.trim()) {
      return { success: true, exitCode: 0, output: "" };
    }

    // Emit a start event so the UI can show script execution.
    this.emit("script:start", {
      taskId,
      scriptType,
      command,
    });

    try {
      const result = await this.router.sendToRemoteAgent(profileId, "task_run_script", {
        task_id: taskId,
        script_type: scriptType,
        command,
        worktree_path: worktreePath,
        env: env ?? {},
      }) as { exit_code: number; output: string; error?: string };

      const exitCode = typeof result.exit_code === "number" ? result.exit_code : 0;
      const success = exitCode === 0;
      const output = typeof result.output === "string" ? result.output : "";
      const error = !success
        ? (result.error ?? `Script exited with code ${exitCode}`)
        : undefined;

      const scriptResult: RemoteScriptResult = {
        success,
        exitCode,
        output,
        error,
      };

      this.emit("script:complete", {
        taskId,
        scriptType,
        result: scriptResult,
      });

      return scriptResult;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const scriptResult: RemoteScriptResult = {
        success: false,
        exitCode: -1,
        output: "",
        error: `RPC failed: ${message}`,
      };

      this.emit("script:complete", {
        taskId,
        scriptType,
        result: scriptResult,
      });

      return scriptResult;
    }
  }

  /**
   * Run the setup script for a remote task. If the setup script fails,
   * the task should NOT be started.
   *
   * @returns The script result. Caller should check `success` before
   *   proceeding with task_spawn.
   */
  async runSetupScript(
    taskId: string,
    profileId: string,
    worktreePath: string,
    command: string,
    env?: Record<string, string>,
  ): Promise<RemoteScriptResult> {
    return this.runScript({
      taskId,
      profileId,
      scriptType: "setup",
      command,
      worktreePath,
      env,
    });
  }

  /**
   * Run the run script for a remote task (executes alongside or after
   * the agent CLI).
   */
  async runRunScript(
    taskId: string,
    profileId: string,
    worktreePath: string,
    command: string,
    env?: Record<string, string>,
  ): Promise<RemoteScriptResult> {
    return this.runScript({
      taskId,
      profileId,
      scriptType: "run",
      command,
      worktreePath,
      env,
    });
  }

  /**
   * Run the archive script for a remote task (cleanup, save artifacts).
   */
  async runArchiveScript(
    taskId: string,
    profileId: string,
    worktreePath: string,
    command: string,
    env?: Record<string, string>,
  ): Promise<RemoteScriptResult> {
    return this.runScript({
      taskId,
      profileId,
      scriptType: "archive",
      command,
      worktreePath,
      env,
    });
  }

  /**
   * Handle a `task_script_output` notification from the remote agent.
   * These notifications stream script output in real-time as the script
   * runs on the remote machine.
   *
   * Expected notification params:
   * - task_id: string
   * - script_type: "setup" | "run" | "archive"
   * - stream: "stdout" | "stderr"
   * - data: string (chunk of output)
   */
  handleScriptNotification(notification: {
    profileId: string;
    method: string;
    params: unknown;
  }): void {
    if (notification.method !== "task_script_output") return;

    const params = notification.params as Record<string, unknown> | undefined;
    if (!params) return;

    const taskId = params.task_id as string | undefined;
    const scriptType = params.script_type as RemoteScriptType | undefined;
    const stream = params.stream as "stdout" | "stderr" | undefined;
    const data = params.data as string | undefined;

    if (!taskId || !scriptType || !stream || !data) return;

    const event: RemoteScriptEvent = {
      taskId,
      scriptType,
      type: stream,
      data,
    };

    this.emit("script:output", event);
  }
}

// ── Integration with Orchestrator ────────────────────────────────

/**
 * Load workspace scripts for a remote task. Reads the orchestrator
 * config from the remote repo (via the remote agent) or falls back
 * to the provided default scripts.
 *
 * For remote repos, we can't read the local filesystem, so we use
 * the remote agent's `read_text` verb to fetch the config file.
 */
export async function loadRemoteScripts(
  router: RpcRouter,
  profileId: string,
  repoId: string,
  remotePath: string,
  defaultScripts: OrchestratorScripts,
): Promise<OrchestratorScripts> {
  const configPath = `${remotePath}/crab-orchestrator.json`;

  try {
    const result = await router.sendToRemoteAgent(profileId, "read_text", {
      repo_id: repoId,
      path: configPath,
    }) as { content?: string };

    if (result.content) {
      const config = JSON.parse(result.content) as {
        scripts?: { setup?: string; run?: string; archive?: string };
      };

      return {
        setupScript: config.scripts?.setup ?? defaultScripts.setupScript,
        runScript: config.scripts?.run ?? defaultScripts.runScript,
        archiveScript: config.scripts?.archive ?? defaultScripts.archiveScript,
      };
    }
  } catch {
    // Config file doesn't exist or can't be read — use defaults.
  }

  return defaultScripts;
}

/**
 * Build workspace environment variables for a remote task.
 * These are injected into the script's environment on the remote machine.
 */
export function buildRemoteWorkspaceEnv(
  worktreePath: string,
  branch: string,
  repoPath: string,
  taskId: string,
): Record<string, string> {
  return {
    CRAB_WORKSPACE_PATH: worktreePath,
    CRAB_WORKSPACE_BRANCH: branch,
    CRAB_ROOT_PATH: repoPath,
    CRAB_TASK_ID: taskId,
    CRAB_REMOTE: "true",
  };
}
