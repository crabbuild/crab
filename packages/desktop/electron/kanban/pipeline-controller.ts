/**
 * Pipeline Controller for the Kanban subsystem.
 *
 * Manages the end-to-end lifecycle of a card from issue to merged PR.
 * State machine: idle → agent_running → pr_created → ci_pending → review →
 * merge → done | blocked.
 *
 * Integrates with the existing Orchestrator for task creation, iteration,
 * and cancellation, and with orchestrator-pr.ts for PR management.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import { Notification } from "electron";
import log from "electron-log";

import type {
  KanbanCard,
  BoardConfig,
  AutonomyLevel,
} from "./kanban-types";
import type { OrchestratorLike } from "./automation-engine";
import {
  createPR,
  mergePR,
  getGitStatus,
  pushBranch,
} from "../orchestrator-pr";

const execFile = promisify(execFileCb);

// ── Pipeline Run ─────────────────────────────────────────────────

export type PipelineRunState =
  | "idle"
  | "agent_running"
  | "pr_created"
  | "ci_pending"
  | "review"
  | "merge"
  | "done"
  | "blocked";

export interface PipelineRun {
  cardId: string;
  state: PipelineRunState;
  taskId?: string;
  branch?: string;
  prNumber?: number;
  prUrl?: string;
  iterationCount: number;
  maxIterations: number;
  startedAt: number;
  completedAt?: number;
  blockReason?: string;
  awaitingApproval: boolean;
}

// ── Interfaces for Dependencies ──────────────────────────────────

export interface BoardSyncLike {
  moveCard(cardId: string, targetColumnId: string): void;
  getCard(cardId: string): KanbanCard | undefined;
  getColumnByName(name: string): { id: string; name: string } | undefined;
}

export interface CIMonitorLike {
  watchPR(prNumber: number, cardId: string): void;
  unwatchPR(prNumber: number): void;
}

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_MAX_ITERATIONS = 3;
const PR_RETRY_DELAY_MS = 10_000;
const CI_LOG_TAIL_LINES = 200;
const APPROVAL_REMINDER_MS = 72 * 60 * 60 * 1000; // 72 hours

// ── Pipeline Controller ──────────────────────────────────────────

export class PipelineController extends EventEmitter {
  private orchestrator: OrchestratorLike;
  private boardSync: BoardSyncLike;
  private ciMonitor: CIMonitorLike;
  private config: BoardConfig;
  private runs: Map<string, PipelineRun> = new Map();
  private cancelledByUser: Set<string> = new Set();
  private reminderTimers: Map<string, ReturnType<typeof setTimeout>> = new Map();
  private activeRuleOverrides: Map<string, AutonomyLevel> = new Map();

  constructor(
    orchestrator: OrchestratorLike,
    boardSync: BoardSyncLike,
    ciMonitor: CIMonitorLike,
    config: BoardConfig,
  ) {
    super();
    this.orchestrator = orchestrator;
    this.boardSync = boardSync;
    this.ciMonitor = ciMonitor;
    this.config = config;
  }

  /**
   * Starts a pipeline run for the given card. Creates a branch, spawns
   * an agent task, and moves the card to "In Progress".
   */
  async startPipeline(card: KanbanCard): Promise<void> {
    if (this.runs.has(card.id)) {
      log.warn(`[pipeline] Pipeline already active for card ${card.id}, rejecting`);
      return;
    }

    const run: PipelineRun = {
      cardId: card.id,
      state: "idle",
      iterationCount: 0,
      maxIterations: this.config.maxIterations ?? DEFAULT_MAX_ITERATIONS,
      startedAt: Date.now(),
      awaitingApproval: false,
    };
    this.runs.set(card.id, run);

    try {
      // Create branch
      const branch = await this.createBranch(card);
      run.branch = branch;

      // Construct prompt from issue title + description
      const prompt = this.buildPrompt(card);

      // Call orchestrator to create and start the task
      const task = await this.orchestrator.createTask(
        prompt,
        this.resolveAgentType(),
        this.config.repoPath,
        undefined, // baseRef — let orchestrator resolve HEAD
      );
      run.taskId = task.id;

      // Transition state
      this.transitionState(run, "agent_running");

      // Move card to "In Progress"
      this.moveCardToColumn(card.id, "In Progress");

      log.info(`[pipeline] Started pipeline for card ${card.id}, task ${task.id}, branch ${branch}`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      log.error(`[pipeline] Failed to start pipeline for card ${card.id}: ${message}`);
      run.blockReason = message;
      this.transitionState(run, "blocked");
      this.moveCardToColumn(card.id, "Blocked");
      this.sendNotification("pipeline_blocked", card.id, `Pipeline start failed: ${message}`);
    }
  }

  /**
   * Cancels an active pipeline run. Cancels the in-flight agent task
   * and abandons the pipeline.
   */
  async cancelPipeline(cardId: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) {
      log.warn(`[pipeline] No active pipeline for card ${cardId}`);
      return;
    }

    this.cancelledByUser.add(cardId);
    this.clearApprovalReminder(cardId);

    // Cancel the orchestrator task if one is running
    if (run.taskId && (run.state === "agent_running" || run.state === "ci_pending" || run.state === "review")) {
      try {
        // The orchestrator's cancelTask is accessed via the OrchestratorLike interface.
        // If the orchestrator exposes cancelTask, call it. Otherwise, best-effort.
        const orch = this.orchestrator as unknown as { cancelTask?(id: string): Promise<void> };
        if (typeof orch.cancelTask === "function") {
          await orch.cancelTask(run.taskId);
        }
      } catch (err) {
        log.warn(`[pipeline] Failed to cancel task ${run.taskId}: ${err instanceof Error ? err.message : String(err)}`);
      }
    }

    // Unwatch PR if one exists
    if (run.prNumber) {
      this.ciMonitor.unwatchPR(run.prNumber);
    }

    run.completedAt = Date.now();
    this.runs.delete(cardId);
    this.cancelledByUser.delete(cardId);

    log.info(`[pipeline] Cancelled pipeline for card ${cardId}`);
    this.emitStatus(cardId, "idle", "Pipeline cancelled by user");
  }

  /**
   * Approves a merge that was paused for user confirmation.
   */
  async approveMerge(cardId: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run || !run.awaitingApproval) {
      log.warn(`[pipeline] No pending approval for card ${cardId}`);
      return;
    }

    run.awaitingApproval = false;
    this.clearApprovalReminder(cardId);
    await this.executeMerge(run);
  }

  /**
   * Returns the current pipeline run state for a card, if any.
   */
  getPipelineState(cardId: string): PipelineRun | undefined {
    return this.runs.get(cardId);
  }

  /**
   * Handles agent task completion. Called when the orchestrator reports
   * that the agent exited with code 0 and files were changed.
   */
  async onAgentCompleted(cardId: string, exitCode: number, worktreePath: string, baseRef: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run || run.state !== "agent_running") return;

    if (this.cancelledByUser.has(cardId)) return;

    if (exitCode !== 0) {
      run.blockReason = `Agent exited with code ${exitCode}`;
      this.transitionState(run, "blocked");
      this.moveCardToColumn(cardId, "Blocked");
      this.sendNotification("pipeline_blocked", cardId, run.blockReason);
      return;
    }

    // Check if files were actually changed
    const status = await getGitStatus(worktreePath, baseRef);
    if (status.clean && status.ahead === 0) {
      run.blockReason = "Agent completed but no files were changed";
      this.transitionState(run, "blocked");
      this.moveCardToColumn(cardId, "Blocked");
      this.sendNotification("pipeline_blocked", cardId, run.blockReason);
      return;
    }

    // Check autonomy level for PR creation
    const autonomy = this.getEffectiveAutonomy(run.cardId);
    if (autonomy === "conservative") {
      run.awaitingApproval = true;
      this.sendNotification("approval_needed", cardId, "Agent completed. Approve PR creation?");
      this.scheduleApprovalReminder(cardId);
      this.emitStatus(cardId, "agent_running", "Awaiting approval to create PR");
      return;
    }

    await this.createDraftPR(run);
  }

  /**
   * Handles CI check pass event from the CI monitor.
   */
  async onCIPassed(cardId: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) return;
    if (this.cancelledByUser.has(cardId)) return;

    this.transitionState(run, "review");
    log.info(`[pipeline] CI passed for card ${cardId}, awaiting review`);
  }

  /**
   * Handles CI check failure event from the CI monitor.
   */
  async onCIFailed(cardId: string, failedCheckName: string, logUrl?: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) return;
    if (this.cancelledByUser.has(cardId)) return;

    // Check iteration limit
    if (run.iterationCount >= run.maxIterations) {
      run.blockReason = `Exceeded max iterations (${run.maxIterations}) after CI failure: ${failedCheckName}`;
      this.transitionState(run, "blocked");
      this.moveCardToColumn(cardId, "Blocked");
      this.sendNotification(
        "pipeline_blocked",
        cardId,
        `Pipeline blocked: ${run.blockReason}`,
      );
      return;
    }

    // Extract CI log (last 200 lines)
    const ciLog = await this.extractCILog(logUrl);
    const feedback = `CI check "${failedCheckName}" failed.\n\n${ciLog}`;

    // Iterate the agent
    if (run.taskId) {
      try {
        await this.orchestrator.iterate(run.taskId, feedback);
        run.iterationCount++;
        this.transitionState(run, "ci_pending");
        log.info(`[pipeline] Iterating agent for card ${cardId} (iteration ${run.iterationCount})`);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        run.blockReason = `Failed to iterate agent: ${message}`;
        this.transitionState(run, "blocked");
        this.moveCardToColumn(cardId, "Blocked");
        this.sendNotification("pipeline_blocked", cardId, run.blockReason);
      }
    }
  }

  /**
   * Handles review feedback (changes requested) from the CI monitor.
   */
  async onReviewChangesRequested(cardId: string, comments: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) return;
    if (this.cancelledByUser.has(cardId)) return;

    // Check iteration limit
    if (run.iterationCount >= run.maxIterations) {
      run.blockReason = `Exceeded max iterations (${run.maxIterations}) after review feedback`;
      this.transitionState(run, "blocked");
      this.moveCardToColumn(cardId, "Blocked");
      this.sendNotification(
        "pipeline_blocked",
        cardId,
        `Pipeline blocked: ${run.blockReason}`,
      );
      return;
    }

    const autonomy = this.getEffectiveAutonomy(cardId);
    if (autonomy === "conservative") {
      run.awaitingApproval = true;
      this.sendNotification("approval_needed", cardId, "Review feedback received. Approve iteration?");
      this.scheduleApprovalReminder(cardId);
      this.emitStatus(cardId, run.state, "Awaiting approval to iterate on review feedback");
      return;
    }

    // Feed review comments to agent as iteration feedback
    const feedback = `PR review requested changes:\n\n${comments}\n\nPlease address the review feedback and push new commits.`;

    if (run.taskId) {
      try {
        await this.orchestrator.iterate(run.taskId, feedback);
        run.iterationCount++;
        this.transitionState(run, "ci_pending");
        log.info(`[pipeline] Iterating agent on review feedback for card ${cardId} (iteration ${run.iterationCount})`);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        run.blockReason = `Failed to iterate agent on review: ${message}`;
        this.transitionState(run, "blocked");
        this.moveCardToColumn(cardId, "Blocked");
        this.sendNotification("pipeline_blocked", cardId, run.blockReason);
      }
    }
  }

  /**
   * Handles review approved event. Proceeds to merge based on autonomy level.
   */
  async onReviewApproved(cardId: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) return;
    if (this.cancelledByUser.has(cardId)) return;

    const autonomy = this.getEffectiveAutonomy(cardId);

    if (autonomy === "aggressive") {
      // Auto-merge
      await this.executeMerge(run);
    } else {
      // moderate and conservative: pause for approval
      run.awaitingApproval = true;
      this.transitionState(run, "merge");
      this.sendNotification("approval_needed", cardId, "PR approved and CI passed. Approve merge?");
      this.scheduleApprovalReminder(cardId);
    }
  }

  /**
   * Handles manual card move by user during an active pipeline.
   * Cancels the pipeline and leaves the card where the user placed it.
   */
  async onManualCardMove(cardId: string, _targetColumnId: string): Promise<void> {
    const run = this.runs.get(cardId);
    if (!run) return;

    log.info(`[pipeline] Card ${cardId} moved manually during active pipeline, cancelling`);
    await this.cancelPipeline(cardId);
  }

  // ── Private Methods ────────────────────────────────────────────

  /**
   * Creates a git branch for the pipeline run. Uses the pattern
   * `<card-id>/<short-title-slug>`. Handles naming conflicts by
   * appending a numeric suffix and retrying once.
   */
  private async createBranch(card: KanbanCard): Promise<string> {
    const slug = this.slugify(card.title);
    const baseName = `${card.externalId}/${slug}`;

    // Try creating the branch
    const created = await this.tryCreateBranch(baseName);
    if (created) return baseName;

    // Naming conflict — append numeric suffix and retry once
    const fallbackName = `${baseName}-2`;
    const retryCreated = await this.tryCreateBranch(fallbackName);
    if (retryCreated) return fallbackName;

    // Both attempts failed — block
    throw new Error(`Branch creation failed: "${baseName}" and "${fallbackName}" both conflict with existing branches`);
  }

  private async tryCreateBranch(branchName: string): Promise<boolean> {
    try {
      await execFile("git", ["branch", branchName], { cwd: this.config.repoPath });
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Creates a URL-safe slug from a title string.
   */
  private slugify(title: string): string {
    return title
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 50);
  }

  /**
   * Builds the agent prompt from the card's issue title and description.
   */
  private buildPrompt(card: KanbanCard): string {
    const parts: string[] = [];
    parts.push(`Issue: ${card.title}`);
    if (card.description) {
      parts.push(`\nDescription:\n${card.description}`);
    }
    parts.push(`\nPlease implement the changes described above.`);
    return parts.join("\n");
  }

  /**
   * Resolves the agent type to use. Defaults to the first available
   * agent type from the board config or falls back to "claude-code".
   */
  private resolveAgentType(): string {
    return "claude-code";
  }

  /**
   * Creates a draft PR for the pipeline run. Retries once after 10s on failure.
   */
  private async createDraftPR(run: PipelineRun): Promise<void> {
    const card = this.boardSync.getCard(run.cardId);
    if (!card) {
      run.blockReason = "Card not found when creating PR";
      this.transitionState(run, "blocked");
      return;
    }

    const prResult = await this.attemptCreatePR(run, card);
    if (prResult.success && prResult.pr) {
      run.prNumber = prResult.pr.number;
      run.prUrl = prResult.pr.url;
      this.transitionState(run, "pr_created");
      this.moveCardToColumn(run.cardId, "In Review");

      // Start watching CI
      this.ciMonitor.watchPR(prResult.pr.number, run.cardId);
      this.transitionState(run, "ci_pending");

      log.info(`[pipeline] Created draft PR #${prResult.pr.number} for card ${run.cardId}`);
      return;
    }

    // Retry once after 10s
    log.warn(`[pipeline] PR creation failed for card ${run.cardId}, retrying in 10s`);
    await this.delay(PR_RETRY_DELAY_MS);

    const retryResult = await this.attemptCreatePR(run, card);
    if (retryResult.success && retryResult.pr) {
      run.prNumber = retryResult.pr.number;
      run.prUrl = retryResult.pr.url;
      this.transitionState(run, "pr_created");
      this.moveCardToColumn(run.cardId, "In Review");

      this.ciMonitor.watchPR(retryResult.pr.number, run.cardId);
      this.transitionState(run, "ci_pending");

      log.info(`[pipeline] Created draft PR #${retryResult.pr.number} for card ${run.cardId} (retry)`);
      return;
    }

    // Both attempts failed — block
    run.blockReason = `PR creation failed: ${retryResult.error ?? "unknown error"}`;
    this.transitionState(run, "blocked");
    this.moveCardToColumn(run.cardId, "Blocked");
    this.sendNotification("pipeline_blocked", run.cardId, run.blockReason);
  }

  private async attemptCreatePR(
    run: PipelineRun,
    card: KanbanCard,
  ): Promise<{ success: boolean; pr?: { number: number; url: string }; error?: string }> {
    try {
      if (!run.branch) {
        return { success: false, error: "No branch set on pipeline run" };
      }

      // Push the branch first
      const pushResult = await pushBranch(this.config.repoPath, run.branch);
      if (!pushResult.success) {
        return { success: false, error: pushResult.error ?? "Push failed" };
      }

      const result = await createPR({
        repoPath: this.config.repoPath,
        branch: run.branch,
        title: card.title,
        body: card.description || `Resolves ${card.externalId}`,
        draft: true,
      });

      if (result.success && result.pr) {
        return { success: true, pr: { number: result.pr.number, url: result.pr.url } };
      }
      return { success: false, error: result.error };
    } catch (err) {
      return { success: false, error: err instanceof Error ? err.message : String(err) };
    }
  }

  /**
   * Executes the merge flow. Merges the PR and moves the card to Done.
   */
  private async executeMerge(run: PipelineRun): Promise<void> {
    if (!run.prNumber) {
      run.blockReason = "No PR number available for merge";
      this.transitionState(run, "blocked");
      return;
    }

    this.transitionState(run, "merge");

    const result = await mergePR(this.config.repoPath, run.prNumber);
    if (result.success) {
      run.completedAt = Date.now();
      this.transitionState(run, "done");
      this.moveCardToColumn(run.cardId, "Done");

      // Unwatch the PR
      this.ciMonitor.unwatchPR(run.prNumber);

      log.info(`[pipeline] Merged PR #${run.prNumber} for card ${run.cardId}`);
      this.runs.delete(run.cardId);
    } else {
      run.blockReason = `Merge failed: ${result.error ?? "unknown error"}`;
      this.transitionState(run, "blocked");
      this.moveCardToColumn(run.cardId, "Blocked");
      this.sendNotification("pipeline_blocked", run.cardId, run.blockReason);
    }
  }

  /**
   * Extracts the last N lines of a CI log from the given URL.
   * Falls back to a generic message if extraction fails.
   */
  private async extractCILog(logUrl?: string): Promise<string> {
    if (!logUrl) {
      return "(CI log URL not available)";
    }

    try {
      // Use gh CLI to fetch the log
      const { stdout } = await execFile(
        "gh",
        ["run", "view", "--log-failed", "--repo", this.getRepoSlug()],
        { cwd: this.config.repoPath, maxBuffer: 1024 * 1024 },
      );

      const lines = stdout.split("\n");
      const tail = lines.slice(-CI_LOG_TAIL_LINES).join("\n");
      return tail || "(empty CI log)";
    } catch {
      return `(Failed to fetch CI log from ${logUrl})`;
    }
  }

  /**
   * Returns the GitHub repo slug (owner/repo) from the board config.
   */
  private getRepoSlug(): string {
    const trackerConfig = this.config.trackerConfig;
    if ("owner" in trackerConfig && "repo" in trackerConfig) {
      return `${trackerConfig.owner}/${trackerConfig.repo}`;
    }
    if ("owner" in trackerConfig) {
      return trackerConfig.owner;
    }
    return "";
  }

  /**
   * Gets the effective autonomy level, considering rule overrides.
   * Rule-level autonomyOverride takes precedence over board-level setting.
   */
  private getEffectiveAutonomy(cardId?: string): AutonomyLevel {
    if (cardId) {
      const ruleOverride = this.activeRuleOverrides.get(cardId);
      if (ruleOverride) return ruleOverride;
    }
    return this.config.autonomyLevel ?? "moderate";
  }

  /**
   * Moves a card to the named column via the board sync layer.
   */
  private moveCardToColumn(cardId: string, columnName: string): void {
    const column = this.boardSync.getColumnByName(columnName);
    if (column) {
      this.boardSync.moveCard(cardId, column.id);
    } else {
      log.warn(`[pipeline] Column "${columnName}" not found, cannot move card ${cardId}`);
    }
  }

  /**
   * Transitions the pipeline run to a new state and emits a status event.
   */
  private transitionState(run: PipelineRun, newState: PipelineRunState): void {
    const oldState = run.state;
    run.state = newState;
    this.emitStatus(run.cardId, newState, undefined, oldState);
  }

  /**
   * Emits a pipeline:status event for the given card.
   */
  private emitStatus(
    cardId: string,
    state: PipelineRunState,
    detail?: string,
    previousState?: PipelineRunState,
  ): void {
    this.emit("pipeline:status", {
      cardId,
      pipelineState: state,
      previousState,
      detail,
    });
  }

  /**
   * Sends a desktop notification via Electron's Notification API and
   * emits a pipeline:notification event for the IPC layer.
   * Notifications fire within 30s of pipeline pause (immediate dispatch).
   */
  private sendNotification(type: string, cardId: string, message: string): void {
    // Emit event for IPC relay to renderer
    this.emit("pipeline:notification", { type, cardId, message });

    // Fire Electron desktop notification
    if (Notification.isSupported()) {
      const title = type === "approval_needed"
        ? "Crab: Approval Required"
        : type === "pipeline_blocked"
          ? "Crab: Pipeline Blocked"
          : "Crab: Pipeline Update";

      const notification = new Notification({
        title,
        body: message,
        silent: false,
      });
      notification.show();
    }
  }

  /**
   * Sets a rule-level autonomy override for a card. This takes
   * precedence over the board-level autonomy setting.
   */
  setRuleOverride(cardId: string, autonomyLevel: AutonomyLevel): void {
    this.activeRuleOverrides.set(cardId, autonomyLevel);
  }

  /**
   * Clears the rule-level autonomy override for a card.
   */
  clearRuleOverride(cardId: string): void {
    this.activeRuleOverrides.delete(cardId);
  }

  /**
   * Schedules a 72-hour reminder notification if the user has not
   * responded to an approval request.
   */
  private scheduleApprovalReminder(cardId: string): void {
    // Clear any existing reminder for this card
    this.clearApprovalReminder(cardId);

    const timer = setTimeout(() => {
      const run = this.runs.get(cardId);
      if (run && run.awaitingApproval) {
        this.sendNotification(
          "approval_reminder",
          cardId,
          "Reminder: A pipeline action is still awaiting your approval (72 hours elapsed).",
        );
        log.info(`[pipeline] Sent 72-hour approval reminder for card ${cardId}`);
      }
      this.reminderTimers.delete(cardId);
    }, APPROVAL_REMINDER_MS);

    this.reminderTimers.set(cardId, timer);
  }

  /**
   * Clears the 72-hour reminder timer for a card.
   */
  private clearApprovalReminder(cardId: string): void {
    const timer = this.reminderTimers.get(cardId);
    if (timer) {
      clearTimeout(timer);
      this.reminderTimers.delete(cardId);
    }
  }

  /**
   * Cleans up all timers. Call on shutdown.
   */
  dispose(): void {
    for (const timer of this.reminderTimers.values()) {
      clearTimeout(timer);
    }
    this.reminderTimers.clear();
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
