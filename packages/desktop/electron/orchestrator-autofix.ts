/**
 * CI Auto-Fix Loop for the Crab Desktop Orchestrator.
 *
 * When CI fails on a workspace's branch, this module automatically sends
 * failure context to the agent for remediation, retrying up to a
 * configurable limit. If all attempts are exhausted, it emits an event
 * so the UI can notify the user.
 */

import { EventEmitter } from "node:events";

import type { Orchestrator, Task } from "./orchestrator";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface AutoFixConfig {
  enabled: boolean;
  maxAttempts: number;          // default 3
  delayBetweenAttempts: number; // ms, default 5000
  fixPromptTemplate: string;    // uses {failure_logs} placeholder
}

export interface CheckRun {
  id: string;
  name: string;
  status: "completed" | "in_progress" | "queued";
  conclusion?: "success" | "failure" | "neutral" | "cancelled";
  logs?: string;
  annotations?: Array<{ message: string; path?: string; line?: number }>;
}

interface AutoFixState {
  attempts: number;
  cancelled: boolean;
  config: AutoFixConfig;
  polling: boolean;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_CONFIG: AutoFixConfig = {
  enabled: true,
  maxAttempts: 3,
  delayBetweenAttempts: 5000,
  fixPromptTemplate:
    "CI failed. Please fix the following errors:\n\n{failure_logs}\n\nMake the minimal changes needed to pass CI.",
};

const CI_POLL_INTERVAL_MS = 15_000;   // 15 seconds
const CI_POLL_TIMEOUT_MS = 10 * 60_000; // 10 minutes

// ─── CIAutoFixer ─────────────────────────────────────────────────────────────

/**
 * Manages the CI auto-fix loop for workspaces. When CI fails, extracts
 * failure context and sends it to the agent for remediation. Retries up
 * to `maxAttempts` before giving up.
 *
 * Events emitted:
 * - `autofix:attempt` — { taskId, attempt, maxAttempts, failureContext }
 * - `autofix:exhausted` — { taskId, attempts }
 * - `autofix:success` — { taskId, attempts }
 */
export class CIAutoFixer extends EventEmitter {
  private orchestrator: Orchestrator;
  private states = new Map<string, AutoFixState>();
  private configs = new Map<string, AutoFixConfig>();

  constructor(orchestrator: Orchestrator) {
    super();
    this.orchestrator = orchestrator;
  }

  // ─── Configuration ───────────────────────────────────────────────────────

  /** Sets the auto-fix configuration for a workspace. */
  setConfig(taskId: string, config: Partial<AutoFixConfig>): void {
    const existing = this.configs.get(taskId) ?? { ...DEFAULT_CONFIG };
    this.configs.set(taskId, { ...existing, ...config });
  }

  /** Gets the auto-fix configuration for a workspace. */
  getConfig(taskId: string): AutoFixConfig {
    return this.configs.get(taskId) ?? { ...DEFAULT_CONFIG };
  }

  /** Returns whether auto-fix is enabled for a workspace. */
  isEnabled(taskId: string): boolean {
    const config = this.getConfig(taskId);
    return config.enabled;
  }

  /** Toggles auto-fix on or off for a workspace. */
  toggle(taskId: string, enabled: boolean): void {
    this.setConfig(taskId, { enabled });
    if (!enabled) {
      this.cancel(taskId);
    }
  }

  // ─── Core Loop ───────────────────────────────────────────────────────────

  /**
   * Entry point: called when a CI check run reports failure for a task.
   * Starts the auto-fix loop if enabled and not already exhausted.
   */
  async onCIFailure(task: Task, checkRun: CheckRun): Promise<void> {
    const config = this.getConfig(task.id);
    if (!config.enabled) return;

    let state = this.states.get(task.id);
    if (!state) {
      state = { attempts: 0, cancelled: false, config, polling: false };
      this.states.set(task.id, state);
    }

    // Already exhausted — don't retry.
    if (state.attempts >= config.maxAttempts) {
      return;
    }

    // Already cancelled — don't retry.
    if (state.cancelled) {
      return;
    }

    await this.attemptFix(task, checkRun, state);
  }

  /**
   * Cancels the auto-fix loop for a workspace. Stops any in-progress
   * polling and prevents further attempts.
   */
  cancel(taskId: string): void {
    const state = this.states.get(taskId);
    if (state) {
      state.cancelled = true;
      state.polling = false;
    }
  }

  /** Resets the auto-fix state for a workspace (e.g., on new PR push). */
  reset(taskId: string): void {
    this.states.delete(taskId);
  }

  /** Returns the current attempt count for a workspace. */
  getAttempts(taskId: string): number {
    return this.states.get(taskId)?.attempts ?? 0;
  }

  // ─── Internal ────────────────────────────────────────────────────────────

  private async attemptFix(
    task: Task,
    checkRun: CheckRun,
    state: AutoFixState,
  ): Promise<void> {
    if (state.cancelled) return;

    const config = state.config;
    state.attempts++;

    // Extract failure context from the check run.
    const failureContext = this.extractFailureContext(checkRun);

    // Build the fix prompt.
    const fixPrompt = this.buildFixPrompt(config.fixPromptTemplate, failureContext);

    // Emit attempt event.
    this.emit("autofix:attempt", {
      taskId: task.id,
      attempt: state.attempts,
      maxAttempts: config.maxAttempts,
      failureContext,
    });

    // Send fix prompt to the agent via orchestrator.iterate().
    try {
      await this.orchestrator.iterate(task.id, fixPrompt);
    } catch {
      // If iteration fails (e.g., task in wrong state), stop the loop.
      return;
    }

    // Wait for the configured delay before polling CI.
    if (config.delayBetweenAttempts > 0) {
      await this.delay(config.delayBetweenAttempts);
    }

    if (state.cancelled) return;

    // Poll for new CI status.
    state.polling = true;
    const result = await this.pollCIStatus(task, state);
    state.polling = false;

    if (state.cancelled) return;

    if (result === "success") {
      this.emit("autofix:success", {
        taskId: task.id,
        attempts: state.attempts,
      });
      return;
    }

    // CI still failing — check if we've exhausted attempts.
    if (state.attempts >= config.maxAttempts) {
      this.emit("autofix:exhausted", {
        taskId: task.id,
        attempts: state.attempts,
      });
      return;
    }

    // Retry with the latest check run data if available.
    if (result === "failure" && !state.cancelled) {
      // Small delay before next attempt.
      await this.delay(config.delayBetweenAttempts);
      if (!state.cancelled) {
        await this.attemptFix(task, checkRun, state);
      }
    }
  }

  /**
   * Extracts relevant failure context from a check run: error messages,
   * failing test names, stack traces, and annotations.
   */
  extractFailureContext(checkRun: CheckRun): string {
    const parts: string[] = [];

    parts.push(`Check: ${checkRun.name}`);
    parts.push(`Status: ${checkRun.status}, Conclusion: ${checkRun.conclusion ?? "unknown"}`);

    if (checkRun.annotations && checkRun.annotations.length > 0) {
      parts.push("\nAnnotations:");
      for (const annotation of checkRun.annotations) {
        const location = annotation.path
          ? `  ${annotation.path}${annotation.line ? `:${annotation.line}` : ""}`
          : "";
        parts.push(`${location}: ${annotation.message}`);
      }
    }

    if (checkRun.logs) {
      // Extract the most relevant portion of logs (last 3000 chars to
      // capture error output without overwhelming the prompt).
      const maxLogLength = 3000;
      const logs = checkRun.logs.length > maxLogLength
        ? checkRun.logs.slice(-maxLogLength)
        : checkRun.logs;
      parts.push("\nFailure logs:");
      parts.push(logs);
    }

    return parts.join("\n");
  }

  /**
   * Builds the fix prompt by substituting `{failure_logs}` in the template.
   */
  buildFixPrompt(template: string, failureContext: string): string {
    return template.replace("{failure_logs}", failureContext);
  }

  /**
   * Polls for CI check status after the agent pushes a fix.
   * Returns "success", "failure", or "timeout".
   */
  private async pollCIStatus(
    task: Task,
    state: AutoFixState,
  ): Promise<"success" | "failure" | "timeout"> {
    const startTime = Date.now();

    while (Date.now() - startTime < CI_POLL_TIMEOUT_MS) {
      if (state.cancelled) return "timeout";

      await this.delay(CI_POLL_INTERVAL_MS);

      if (state.cancelled) return "timeout";

      // Attempt to get the latest check run status via the forge API.
      try {
        const checkStatus = await this.fetchLatestCheckStatus(task);
        if (checkStatus === "success") return "success";
        if (checkStatus === "failure") return "failure";
        // "pending" — continue polling.
      } catch {
        // Network error or API failure — continue polling.
      }
    }

    return "timeout";
  }

  /**
   * Fetches the latest CI check status for a task's branch.
   * Returns "success", "failure", or "pending".
   *
   * Uses `gh` CLI to query check run status. Falls back to "pending"
   * if the command fails.
   */
  private async fetchLatestCheckStatus(
    task: Task,
  ): Promise<"success" | "failure" | "pending"> {
    try {
      const { execFile: execFileCb } = await import("node:child_process");
      const { promisify } = await import("node:util");
      const execFile = promisify(execFileCb);

      const { stdout } = await execFile(
        "gh",
        ["pr", "checks", "--json", "state", "--jq", ".[].state"],
        { cwd: task.repoPath, timeout: 10_000 },
      );

      const states = stdout.trim().split("\n").filter(Boolean);
      if (states.length === 0) return "pending";

      // If any check is still pending/in_progress, overall is pending.
      if (states.some((s) => s === "PENDING" || s === "IN_PROGRESS")) {
        return "pending";
      }

      // If all checks passed, success.
      if (states.every((s) => s === "SUCCESS" || s === "NEUTRAL" || s === "SKIPPED")) {
        return "success";
      }

      // Otherwise, at least one failure.
      return "failure";
    } catch {
      return "pending";
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
