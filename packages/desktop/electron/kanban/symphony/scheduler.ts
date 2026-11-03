import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import log from "electron-log";

import type { BoardConfig } from "../kanban-types";
import { renderPrompt } from "./workflow-loader";
import { SymphonyConfigStore, validateDispatchConfig } from "./config";
import { createSymphonyTracker } from "./tracker-factory";
import { SymphonyAgentRunner, initialRunTokenTotals } from "./agent-runner";
import { SymphonyWorkspaceManager } from "./workspace-manager";
import { SymphonyKanbanProjection } from "./projection";
import type {
  SymphonyBlockedEntry,
  SymphonyConfig,
  SymphonyConfigError,
  SymphonyIssue,
  SymphonyIssueSnapshot,
  SymphonyOrchestratorLike,
  SymphonyRetryEntry,
  SymphonyRunningEntry,
  SymphonyRuntimeEvent,
  SymphonyRuntimeSnapshot,
  SymphonyTokenTotals,
  SymphonyTracker,
} from "./types";
import { normalizeIssueState } from "./types";
import { addTokenDelta, EMPTY_TOKEN_TOTALS } from "./token-accounting";

export interface SymphonySchedulerOptions {
  board: BoardConfig;
  orchestrator: SymphonyOrchestratorLike;
  configStore?: SymphonyConfigStore;
  trackerFactory?: (config: SymphonyConfig, board: BoardConfig) => SymphonyTracker;
  projection?: SymphonyKanbanProjection;
  now?: () => number;
}

interface RetryEntryInternal extends SymphonyRetryEntry {
  timer: ReturnType<typeof setTimeout> | null;
}

const CONTINUATION_RETRY_DELAY_MS = 1_000;
const FAILURE_RETRY_BASE_MS = 10_000;
const RECENT_EVENT_LIMIT = 200;

export class SymphonyScheduler extends EventEmitter {
  private board: BoardConfig;
  private readonly configStore: SymphonyConfigStore;
  private readonly trackerFactory: (config: SymphonyConfig, board: BoardConfig) => SymphonyTracker;
  private readonly projection: SymphonyKanbanProjection;
  private readonly runner: SymphonyAgentRunner;
  private readonly now: () => number;
  private pollTimer: ReturnType<typeof setTimeout> | null = null;
  private pollInProgress = false;
  private started = false;
  private status: RuntimeSnapshotStatus = "stopped";
  private lastPollAtMs: number | null = null;
  private nextPollAtMs: number | null = null;
  private configError: SymphonyConfigError | null = null;
  private running = new Map<string, SymphonyRunningEntry>();
  private retrying = new Map<string, RetryEntryInternal>();
  private blocked = new Map<string, SymphonyBlockedEntry>();
  private claimed = new Set<string>();
  private completed = new Set<string>();
  private issueCache = new Map<string, SymphonyIssue>();
  private recentEvents: SymphonyRuntimeEvent[] = [];
  private codexTotals: SymphonyTokenTotals = { ...EMPTY_TOKEN_TOTALS };
  private perTaskTokenHighWater = new Map<string, SymphonyTokenTotals>();
  private rateLimits: unknown | null = null;

  constructor(options: SymphonySchedulerOptions) {
    super();
    this.board = options.board;
    this.configStore = options.configStore ?? new SymphonyConfigStore();
    this.trackerFactory = options.trackerFactory ?? createSymphonyTracker;
    this.projection = options.projection ?? new SymphonyKanbanProjection();
    this.runner = new SymphonyAgentRunner(options.orchestrator);
    this.now = options.now ?? Date.now;

    this.runner.on("update", (update) => this.handleRunnerUpdate(update));
    this.runner.on("completed", (completion) => this.handleRunnerCompletion(completion));
  }

  async start(): Promise<SymphonyRuntimeSnapshot> {
    if (this.started) return this.snapshot();
    this.started = true;
    this.status = "idle";
    await this.configStore.loadInitial(this.board);
    this.configError = this.configStore.error;
    await this.cleanupTerminalWorkspaces();
    this.schedulePoll(0);
    this.emitState();
    return this.snapshot();
  }

  stop(): SymphonyRuntimeSnapshot {
    this.started = false;
    this.status = "stopped";
    if (this.pollTimer) clearTimeout(this.pollTimer);
    this.pollTimer = null;
    this.nextPollAtMs = null;
    for (const retry of this.retrying.values()) {
      if (retry.timer) clearTimeout(retry.timer);
    }
    this.retrying.clear();
    this.runner.dispose();
    this.emitState();
    return this.snapshot();
  }

  updateBoard(board: BoardConfig): void {
    this.board = board;
  }

  async refresh(): Promise<SymphonyRuntimeSnapshot> {
    await this.runPollCycle("manual");
    return this.snapshot();
  }

  getIssue(issueIdOrIdentifier: string): SymphonyIssueSnapshot | null {
    const running = findEntry(this.running, issueIdOrIdentifier);
    const retry = findEntry(this.retrying, issueIdOrIdentifier);
    const blocked = findEntry(this.blocked, issueIdOrIdentifier);
    const issue =
      running?.issue ||
      retry?.issue ||
      blocked?.issue ||
      [...this.issueCache.values()].find(
        (candidate) => candidate.id === issueIdOrIdentifier || candidate.identifier === issueIdOrIdentifier,
      );

    if (!issue && !running && !retry && !blocked) return null;

    return {
      issueId: issue?.id ?? running?.issueId ?? retry?.issueId ?? blocked!.issueId,
      identifier: issue?.identifier ?? running?.identifier ?? retry?.identifier ?? blocked!.identifier,
      status: running ? "running" : retry ? "retrying" : blocked ? "blocked" : "unclaimed",
      running,
      retry: retry ? stripRetryTimer(retry) : undefined,
      blocked,
    };
  }

  async retryIssue(issueId: string): Promise<SymphonyRuntimeSnapshot> {
    const retry = this.retrying.get(issueId);
    if (retry?.timer) clearTimeout(retry.timer);
    this.retrying.delete(issueId);
    this.blocked.delete(issueId);
    this.claimed.delete(issueId);
    await this.handleRetry(issueId, retry?.attempt ?? 1, retry?.error ?? "manual retry");
    this.emitState();
    return this.snapshot();
  }

  async cancelIssue(issueId: string): Promise<SymphonyRuntimeSnapshot> {
    const running = this.running.get(issueId);
    if (running) {
      await this.runner.cancel(running.taskId);
      this.running.delete(issueId);
    }
    const retry = this.retrying.get(issueId);
    if (retry?.timer) clearTimeout(retry.timer);
    this.retrying.delete(issueId);
    this.claimed.delete(issueId);
    this.emitRuntimeEvent(issueId, "issue_cancelled", "Issue execution cancelled by operator.");
    await this.projectKnownIssues();
    this.emitState();
    return this.snapshot();
  }

  async releaseBlockedIssue(issueId: string): Promise<SymphonyRuntimeSnapshot> {
    this.blocked.delete(issueId);
    this.claimed.delete(issueId);
    this.emitRuntimeEvent(issueId, "blocked_released", "Blocked issue released by operator.");
    await this.projectKnownIssues();
    this.emitState();
    return this.snapshot();
  }

  snapshot(): SymphonyRuntimeSnapshot {
    return {
      boardId: this.board.id,
      status: this.status,
      started: this.started,
      workflowPath: this.configStore.current?.workflow.workflowPath ?? null,
      lastReloadAtMs: this.configStore.current?.workflow.loadedAtMs ?? null,
      lastPollAtMs: this.lastPollAtMs,
      nextPollAtMs: this.nextPollAtMs,
      configError: this.configError,
      counts: {
        running: this.running.size,
        retrying: this.retrying.size,
        blocked: this.blocked.size,
        completed: this.completed.size,
        claimed: this.claimed.size,
      },
      running: [...this.running.values()],
      retrying: [...this.retrying.values()].map(stripRetryTimer),
      blocked: [...this.blocked.values()],
      codexTotals: this.codexTotals,
      rateLimits: this.rateLimits,
      recentEvents: [...this.recentEvents],
    };
  }

  private schedulePoll(delayMs: number): void {
    if (!this.started) return;
    if (this.pollTimer) clearTimeout(this.pollTimer);
    this.nextPollAtMs = this.now() + delayMs;
    this.pollTimer = setTimeout(() => {
      void this.runPollCycle("scheduled");
    }, delayMs);
  }

  private async runPollCycle(reason: "scheduled" | "manual"): Promise<void> {
    if (!this.started && reason === "scheduled") return;
    if (this.pollInProgress) return;

    this.pollInProgress = true;
    this.status = "polling";
    this.lastPollAtMs = this.now();
    this.nextPollAtMs = null;
    this.emitState();

    try {
      const reload = await this.configStore.reloadIfChanged(this.board);
      this.configError = reload.error;
      const config = reload.config;

      if (!config) {
        this.status = "error";
        this.emitRuntimeEvent(undefined, "config_error", reload.error?.message ?? "Invalid Symphony config.");
        return;
      }

      const tracker = this.trackerFactory(config, this.board);
      await this.reconcileRunning(config, tracker);
      await this.reconcileBlocked(config, tracker);

      const validationError = validateDispatchConfig(config);
      if (validationError) {
        this.configError = validationError;
        this.emitRuntimeEvent(undefined, "config_error", validationError.message);
        return;
      }

      const issues = await tracker.fetchCandidateIssues();
      this.cacheIssues(issues);
      await this.projectIssues(issues);

      await this.dispatchCandidates(config, tracker, issues);
    } catch (err) {
      this.status = "error";
      const message = err instanceof Error ? err.message : String(err);
      this.emitRuntimeEvent(undefined, "poll_error", message);
      log.error(`[kanban-symphony] poll failed: ${message}`);
    } finally {
      this.pollInProgress = false;
      if (this.started) {
        const config = this.configStore.current;
        this.status = this.configError ? "error" : "idle";
        this.schedulePoll(config?.polling.intervalMs ?? 30_000);
      }
      this.emitState();
    }
  }

  private async dispatchCandidates(
    config: SymphonyConfig,
    tracker: SymphonyTracker,
    issues: SymphonyIssue[],
  ): Promise<void> {
    const sorted = sortIssuesForDispatch(issues);
    for (const issue of sorted) {
      if (!this.shouldDispatch(issue, config)) continue;
      await this.dispatchIssue(config, tracker, issue, null);
    }
  }

  private async dispatchIssue(
    config: SymphonyConfig,
    _tracker: SymphonyTracker,
    issue: SymphonyIssue,
    attempt: number | null,
  ): Promise<boolean> {
    if (!this.shouldDispatch(issue, config)) return false;

    this.claimed.add(issue.id);
    this.issueCache.set(issue.id, issue);
    const workspaceManager = new SymphonyWorkspaceManager(config);

    try {
      const workspace = await workspaceManager.createForIssue(issue);
      await workspaceManager.runBeforeRun(workspace.path, issue);
      const prompt = await renderPrompt(config.workflow, { issue, attempt });
      const started = await this.runner.start({
        issue,
        prompt,
        workspacePath: workspace.path,
        cardId: symphonyCardId(config.tracker.kind, issue.id),
      });
      const running: SymphonyRunningEntry = {
        issueId: issue.id,
        identifier: issue.identifier,
        issue,
        taskId: started.taskId,
        workspacePath: workspace.path,
        startedAt: this.now(),
        attempt,
        lastEvent: "task_started",
        lastEventAt: this.now(),
        lastMessage: "Agent task started.",
        sessionId: null,
        tokenTotals: initialRunTokenTotals(),
      };
      this.running.set(issue.id, running);
      this.emitRuntimeEvent(issue.id, "task_started", `Started Symphony run for ${issue.identifier}.`, {
        taskId: started.taskId,
        workspacePath: workspace.path,
      });
      await this.projectKnownIssues();
      this.emitState();
      return true;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emitRuntimeEvent(issue.id, "dispatch_failed", message);
      this.scheduleIssueRetry(config, issue, nextAttempt(attempt), message);
      return false;
    }
  }

  private shouldDispatch(issue: SymphonyIssue, config: SymphonyConfig): boolean {
    if (!issue.id || !issue.identifier || !issue.title || !issue.state) return false;
    if (this.running.has(issue.id) || this.claimed.has(issue.id)) return false;
    if (!activeState(config, issue.state) || terminalState(config, issue.state)) return false;
    if (availableSlots(this.running, config) <= 0) return false;
    if (!stateSlotAvailable(this.running, issue.state, config)) return false;

    if (normalizeIssueState(issue.state) === "todo") {
      const terminalStates = normalizedTerminalStates(config);
      const hasOpenBlocker = issue.blockedBy.some(
        (blocker) => !terminalStates.has(normalizeIssueState(blocker.state)),
      );
      if (hasOpenBlocker) return false;
    }

    return true;
  }

  private async reconcileRunning(config: SymphonyConfig, tracker: SymphonyTracker): Promise<void> {
    await this.reconcileStalls(config);
    const ids = [...this.running.keys()];
    if (ids.length === 0) return;

    const issues = await tracker.fetchIssueStatesByIds(ids);
    this.cacheIssues(issues);
    const visible = new Set(issues.map((issue) => issue.id));

    for (const issue of issues) {
      const running = this.running.get(issue.id);
      if (!running) continue;
      if (terminalState(config, issue.state)) {
        await this.stopRunningForState(config, issue.id, true, `Issue moved to terminal state ${issue.state}.`);
      } else if (activeState(config, issue.state)) {
        this.running.set(issue.id, { ...running, issue });
      } else {
        await this.stopRunningForState(config, issue.id, false, `Issue moved to non-active state ${issue.state}.`);
      }
    }

    for (const issueId of ids) {
      if (!visible.has(issueId)) {
        await this.stopRunningForState(config, issueId, false, "Issue no longer visible to tracker query.");
      }
    }
  }

  private async reconcileBlocked(config: SymphonyConfig, tracker: SymphonyTracker): Promise<void> {
    const ids = [...this.blocked.keys()];
    if (ids.length === 0) return;
    const issues = await tracker.fetchIssueStatesByIds(ids);
    this.cacheIssues(issues);
    const visible = new Set(issues.map((issue) => issue.id));

    for (const issue of issues) {
      if (terminalState(config, issue.state)) {
        const blocked = this.blocked.get(issue.id);
        if (blocked?.identifier) {
          await new SymphonyWorkspaceManager(config).removeForIssue(blocked.identifier);
        }
        this.blocked.delete(issue.id);
        this.claimed.delete(issue.id);
      } else if (activeState(config, issue.state)) {
        const blocked = this.blocked.get(issue.id);
        if (blocked) this.blocked.set(issue.id, { ...blocked, issue });
      } else {
        this.blocked.delete(issue.id);
        this.claimed.delete(issue.id);
      }
    }

    for (const issueId of ids) {
      if (!visible.has(issueId)) {
        this.blocked.delete(issueId);
        this.claimed.delete(issueId);
      }
    }
  }

  private async reconcileStalls(config: SymphonyConfig): Promise<void> {
    if (config.codex.stallTimeoutMs <= 0) return;
    const now = this.now();
    for (const running of [...this.running.values()]) {
      const lastActivity = running.lastEventAt ?? running.startedAt;
      if (now - lastActivity <= config.codex.stallTimeoutMs) continue;

      if (inputRequiredEvent(running.lastEvent)) {
        await this.blockRunningIssue(running.issueId, `Codex requires operator input after ${now - lastActivity}ms.`);
      } else {
        await this.stopRunningForState(
          config,
          running.issueId,
          false,
          `Agent stalled for ${now - lastActivity}ms without Codex activity.`,
        );
        this.scheduleIssueRetry(config, running.issue, nextAttempt(running.attempt), "stalled without Codex activity");
      }
    }
  }

  private async stopRunningForState(
    config: SymphonyConfig,
    issueId: string,
    cleanupWorkspace: boolean,
    reason: string,
  ): Promise<void> {
    const running = this.running.get(issueId);
    if (!running) {
      this.claimed.delete(issueId);
      return;
    }

    await this.runner.cancel(running.taskId);
    this.running.delete(issueId);
    this.perTaskTokenHighWater.delete(running.taskId);
    this.claimed.delete(issueId);
    this.retrying.delete(issueId);
    if (cleanupWorkspace) {
      await new SymphonyWorkspaceManager(config).removeForIssue(running.identifier);
    }
    this.emitRuntimeEvent(issueId, "run_stopped", reason);
  }

  private async blockRunningIssue(issueId: string, error: string): Promise<void> {
    const running = this.running.get(issueId);
    if (!running) return;
    await this.runner.cancel(running.taskId);
    this.running.delete(issueId);
    this.blocked.set(issueId, {
      issueId,
      identifier: running.identifier,
      issue: running.issue,
      workspacePath: running.workspacePath,
      taskId: running.taskId,
      sessionId: running.sessionId,
      error,
      blockedAt: this.now(),
      lastEvent: running.lastEvent,
      lastEventAt: running.lastEventAt,
      lastMessage: running.lastMessage,
    });
    this.claimed.add(issueId);
    this.emitRuntimeEvent(issueId, "issue_blocked", error);
    await this.projectKnownIssues();
    this.emitState();
  }

  private async cleanupTerminalWorkspaces(): Promise<void> {
    const config = this.configStore.current;
    if (!config) return;
    const validation = validateDispatchConfig(config);
    if (validation) return;

    try {
      const tracker = this.trackerFactory(config, this.board);
      const terminalIssues = await tracker.fetchTerminalIssues();
      const workspaceManager = new SymphonyWorkspaceManager(config);
      await Promise.all(terminalIssues.map((issue) => workspaceManager.removeForIssue(issue.identifier)));
    } catch (err) {
      log.warn(`[kanban-symphony] terminal cleanup failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  private handleRunnerUpdate(update: {
    issueId: string;
    identifier: string;
    taskId: string;
    event: string;
    timestamp: number;
    message: string | null;
    sessionId: string | null;
    tokenTotals: SymphonyTokenTotals | null;
    raw: unknown;
  }): void {
    const running = this.running.get(update.issueId);
    if (!running) return;

    if (update.tokenTotals) {
      const previous = this.perTaskTokenHighWater.get(update.taskId) ?? EMPTY_TOKEN_TOTALS;
      this.codexTotals = addTokenDelta(this.codexTotals, previous, update.tokenTotals);
      this.perTaskTokenHighWater.set(update.taskId, update.tokenTotals);
    }

    const nextRunning = {
      ...running,
      lastEvent: update.event,
      lastEventAt: update.timestamp,
      lastMessage: update.message,
      sessionId: update.sessionId ?? running.sessionId,
      tokenTotals: update.tokenTotals ?? running.tokenTotals,
    };
    this.running.set(update.issueId, nextRunning);
    if (inputRequiredEvent(update.event)) {
      void this.blockRunningIssue(update.issueId, update.message ?? "Codex requires operator input.");
      return;
    }

    this.emitRuntimeEvent(update.issueId, update.event, update.message ?? update.event, {
      taskId: update.taskId,
    });
    void this.projectKnownIssues();
    this.emitState();
  }

  private handleRunnerCompletion(completion: {
    issueId: string;
    identifier: string;
    taskId: string;
    exitCode: number;
    error?: string;
  }): void {
    const running = this.running.get(completion.issueId);
    if (!running) return;

    this.running.delete(completion.issueId);
    this.perTaskTokenHighWater.delete(completion.taskId);

    const config = this.configStore.current;
    if (!config) {
      this.claimed.delete(completion.issueId);
      return;
    }

    void new SymphonyWorkspaceManager(config)
      .runAfterRun(running.workspacePath, running.issue)
      .finally(() => {
        if (completion.exitCode === 0) {
          this.completed.add(completion.issueId);
          this.emitRuntimeEvent(completion.issueId, "task_completed", "Agent task completed; scheduling continuation check.");
          this.scheduleIssueRetry(config, running.issue, 1, null, CONTINUATION_RETRY_DELAY_MS);
        } else {
          const error = completion.error ?? `Agent exited with code ${completion.exitCode}.`;
          this.emitRuntimeEvent(completion.issueId, "task_failed", error);
          this.scheduleIssueRetry(config, running.issue, nextAttempt(running.attempt), error);
        }
        void this.projectKnownIssues();
        this.emitState();
      });
  }

  private scheduleIssueRetry(
    config: SymphonyConfig,
    issue: SymphonyIssue,
    attempt: number,
    error: string | null,
    fixedDelayMs?: number,
  ): void {
    const existing = this.retrying.get(issue.id);
    if (existing?.timer) clearTimeout(existing.timer);

    const delay = fixedDelayMs ?? Math.min(
      FAILURE_RETRY_BASE_MS * Math.pow(2, Math.max(0, attempt - 1)),
      config.agent.maxRetryBackoffMs,
    );
    const dueAtMs = this.now() + delay;
    const timer = setTimeout(() => {
      void this.handleRetry(issue.id, attempt, error);
    }, delay);

    this.claimed.add(issue.id);
    this.retrying.set(issue.id, {
      issueId: issue.id,
      identifier: issue.identifier,
      issue,
      attempt,
      dueAtMs,
      error,
      workspacePath: this.running.get(issue.id)?.workspacePath,
      timer,
    });
    this.emitRuntimeEvent(issue.id, "retry_scheduled", error ?? "Continuation check scheduled.", {
      attempt,
      dueAtMs,
    });
  }

  private async handleRetry(issueId: string, attempt: number, error: string | null): Promise<void> {
    const retry = this.retrying.get(issueId);
    if (retry?.timer) clearTimeout(retry.timer);
    this.retrying.delete(issueId);

    const config = this.configStore.current;
    if (!config) {
      this.claimed.delete(issueId);
      return;
    }

    try {
      const tracker = this.trackerFactory(config, this.board);
      const candidates = await tracker.fetchCandidateIssues();
      this.cacheIssues(candidates);
      const issue = candidates.find((candidate) => candidate.id === issueId);
      if (!issue) {
        this.claimed.delete(issueId);
        this.emitRuntimeEvent(issueId, "retry_released", "Issue is no longer an active candidate.");
        return;
      }

      this.claimed.delete(issueId);
      if (!(await this.dispatchIssue(config, tracker, issue, attempt))) {
        this.scheduleIssueRetry(config, issue, attempt, error ?? "no available orchestrator slots");
      }
    } catch (err) {
      const issue = retry?.issue ?? this.issueCache.get(issueId);
      if (issue) {
        this.scheduleIssueRetry(config, issue, attempt, err instanceof Error ? err.message : String(err));
      } else {
        this.claimed.delete(issueId);
      }
    }
  }

  private cacheIssues(issues: SymphonyIssue[]): void {
    for (const issue of issues) {
      this.issueCache.set(issue.id, issue);
    }
  }

  private async projectIssues(issues: SymphonyIssue[]): Promise<void> {
    if (this.board.symphony?.projectToKanban === false) return;
    await this.projection.upsertIssues({
      board: this.board,
      columns: this.board.columns,
      trackerKind: this.configStore.current?.tracker.kind ?? "linear",
      issues,
      running: this.running,
      retrying: new Map([...this.retrying].map(([key, value]) => [key, stripRetryTimer(value)])),
      blocked: this.blocked,
    }).then((projection) => {
      this.emit("board-updated", projection);
    }).catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      this.emitRuntimeEvent(undefined, "projection_error", message);
    });
  }

  private async projectKnownIssues(): Promise<void> {
    const issues = [...new Set([
      ...[...this.running.values()].map((entry) => entry.issue),
      ...[...this.retrying.values()].map((entry) => entry.issue).filter(Boolean) as SymphonyIssue[],
      ...[...this.blocked.values()].map((entry) => entry.issue).filter(Boolean) as SymphonyIssue[],
      ...this.issueCache.values(),
    ])];
    await this.projectIssues(issues);
  }

  private emitRuntimeEvent(
    issueId: string | undefined,
    type: string,
    message: string,
    payload?: Record<string, unknown>,
  ): void {
    const issue = issueId ? this.issueCache.get(issueId) : undefined;
    const event: SymphonyRuntimeEvent = {
      id: randomUUID(),
      boardId: this.board.id,
      issueId,
      identifier: issue?.identifier,
      type,
      message,
      timestamp: this.now(),
      payload,
    };
    this.recentEvents.push(event);
    if (this.recentEvents.length > RECENT_EVENT_LIMIT) {
      this.recentEvents.splice(0, this.recentEvents.length - RECENT_EVENT_LIMIT);
    }
    this.emit("event", event);
  }

  private emitState(): void {
    this.emit("state", this.snapshot());
  }
}

type RuntimeSnapshotStatus = "stopped" | "idle" | "polling" | "error";

export function sortIssuesForDispatch(issues: SymphonyIssue[]): SymphonyIssue[] {
  return [...issues].sort((a, b) => {
    const priorityA = a.priority ?? Number.MAX_SAFE_INTEGER;
    const priorityB = b.priority ?? Number.MAX_SAFE_INTEGER;
    if (priorityA !== priorityB) return priorityA - priorityB;

    const createdA = a.createdAt ? Date.parse(a.createdAt) : Number.MAX_SAFE_INTEGER;
    const createdB = b.createdAt ? Date.parse(b.createdAt) : Number.MAX_SAFE_INTEGER;
    if (createdA !== createdB) return createdA - createdB;

    return a.identifier.localeCompare(b.identifier);
  });
}

function availableSlots(running: Map<string, SymphonyRunningEntry>, config: SymphonyConfig): number {
  return Math.max(config.agent.maxConcurrentAgents - running.size, 0);
}

function stateSlotAvailable(
  running: Map<string, SymphonyRunningEntry>,
  state: string,
  config: SymphonyConfig,
): boolean {
  const normalized = normalizeIssueState(state);
  const limit = config.agent.maxConcurrentAgentsByState[normalized] ?? config.agent.maxConcurrentAgents;
  const count = [...running.values()].filter((entry) => normalizeIssueState(entry.issue.state) === normalized).length;
  return count < limit;
}

function activeState(config: SymphonyConfig, state: string): boolean {
  const normalized = normalizeIssueState(state);
  return config.tracker.activeStates.some((candidate) => normalizeIssueState(candidate) === normalized);
}

function terminalState(config: SymphonyConfig, state: string): boolean {
  return normalizedTerminalStates(config).has(normalizeIssueState(state));
}

function normalizedTerminalStates(config: SymphonyConfig): Set<string> {
  return new Set(config.tracker.terminalStates.map(normalizeIssueState));
}

function nextAttempt(attempt: number | null): number {
  return Math.max(1, (attempt ?? 0) + 1);
}

function inputRequiredEvent(event: string | null): boolean {
  return event === "turn_input_required" ||
    event === "approval_required" ||
    event === "input_required" ||
    event === "needs_input";
}

function stripRetryTimer(entry: RetryEntryInternal): SymphonyRetryEntry {
  return {
    issueId: entry.issueId,
    identifier: entry.identifier,
    issue: entry.issue,
    attempt: entry.attempt,
    dueAtMs: entry.dueAtMs,
    error: entry.error,
    workspacePath: entry.workspacePath,
  };
}

function findEntry<T extends { issueId: string; identifier: string }>(
  map: Map<string, T>,
  issueIdOrIdentifier: string,
): T | undefined {
  return map.get(issueIdOrIdentifier) ||
    [...map.values()].find((entry) => entry.identifier === issueIdOrIdentifier);
}

function symphonyCardId(kind: SymphonyConfig["tracker"]["kind"], issueId: string): string {
  return `symphony-${kind}-${issueId}`;
}
