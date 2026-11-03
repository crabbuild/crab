/**
 * Automation Engine for the Kanban subsystem.
 *
 * Evaluates trigger-condition-action rules and dispatches work to the
 * Orchestrator and PipelineController. Supports deduplication, sequential
 * action execution with timeouts, and cron-based scheduling.
 */

import { EventEmitter } from "node:events";
import { minimatch } from "minimatch";
import log from "electron-log";

import type {
  AutomationRule,
  RuleExecution,
  TriggerType,
  ConditionType,
  ActionType,
  KanbanCard,
} from "./kanban-types";
import { RuleStore } from "./kanban-persistence";

// ── Trigger Context ──────────────────────────────────────────────

export interface TriggerContext {
  card: KanbanCard;
  eventData?: Record<string, unknown>;
  currentUser?: string;
}

// ── Minimal Orchestrator Interface ───────────────────────────────
// TODO: Replace with actual Orchestrator import once available

export interface OrchestratorLike {
  createTask(
    prompt: string,
    agentType: string,
    repoPath: string,
    baseRef?: string,
  ): Promise<{ id: string }>;
  iterate(taskId: string, feedback: string): Promise<void>;
}

// ── Minimal PipelineController Interface ─────────────────────────
// TODO: Replace with actual PipelineController import once available

export interface PipelineControllerLike {
  createPr?(cardId: string, params: Record<string, unknown>): Promise<void>;
  runTests?(cardId: string, params: Record<string, unknown>): Promise<void>;
  requestReview?(cardId: string, params: Record<string, unknown>): Promise<void>;
  transitionIssue?(cardId: string, targetStatus: string): Promise<void>;
  addComment?(cardId: string, body: string): Promise<void>;
  mergePr?(cardId: string): Promise<void>;
}

// ── Constants ────────────────────────────────────────────────────

const MAX_RULES_PER_EVENT = 50;
const MAX_ACTIONS_PER_RULE = 20;
const ACTION_TIMEOUT_MS = 300_000; // 5 minutes
const DEDUP_WINDOW_MS = 5_000;
const DEDUP_CLEANUP_MS = 30_000;

// ── Cron Parser ──────────────────────────────────────────────────

interface CronSchedule {
  minutes: number[];
  hours: number[];
  daysOfMonth: number[];
  months: number[];
  daysOfWeek: number[];
}

function parseCronField(field: string, min: number, max: number): number[] {
  const values: number[] = [];

  for (const part of field.split(",")) {
    const trimmed = part.trim();

    if (trimmed === "*") {
      for (let i = min; i <= max; i++) values.push(i);
      continue;
    }

    const stepMatch = trimmed.match(/^(\*|\d+-\d+)\/(\d+)$/);
    if (stepMatch) {
      const step = parseInt(stepMatch[2], 10);
      let start = min;
      let end = max;
      if (stepMatch[1] !== "*") {
        const [s, e] = stepMatch[1].split("-").map(Number);
        start = s;
        end = e;
      }
      for (let i = start; i <= end; i += step) values.push(i);
      continue;
    }

    const rangeMatch = trimmed.match(/^(\d+)-(\d+)$/);
    if (rangeMatch) {
      const start = parseInt(rangeMatch[1], 10);
      const end = parseInt(rangeMatch[2], 10);
      for (let i = start; i <= end; i++) values.push(i);
      continue;
    }

    const num = parseInt(trimmed, 10);
    if (!isNaN(num) && num >= min && num <= max) {
      values.push(num);
    }
  }

  return values;
}

function parseCronExpression(expr: string): CronSchedule | null {
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5) return null;

  const minutes = parseCronField(parts[0], 0, 59);
  const hours = parseCronField(parts[1], 0, 23);
  const daysOfMonth = parseCronField(parts[2], 1, 31);
  const months = parseCronField(parts[3], 1, 12);
  const daysOfWeek = parseCronField(parts[4], 0, 6);

  if (!minutes.length || !hours.length || !daysOfMonth.length || !months.length || !daysOfWeek.length) {
    return null;
  }

  return { minutes, hours, daysOfMonth, months, daysOfWeek };
}

function cronMatchesNow(schedule: CronSchedule): boolean {
  const now = new Date();
  return (
    schedule.minutes.includes(now.getMinutes()) &&
    schedule.hours.includes(now.getHours()) &&
    schedule.daysOfMonth.includes(now.getDate()) &&
    schedule.months.includes(now.getMonth() + 1) &&
    schedule.daysOfWeek.includes(now.getDay())
  );
}

// ── AutomationEngine ─────────────────────────────────────────────

export class AutomationEngine extends EventEmitter {
  private rules: AutomationRule[] = [];
  private executionLog: RuleExecution[] = [];
  private dedupMap = new Map<string, number>();
  private dedupCleanupTimer: ReturnType<typeof setInterval> | null = null;
  private cronTimers = new Map<string, ReturnType<typeof setInterval>>();
  private ruleStore: RuleStore;
  private orchestrator: OrchestratorLike;
  private pipelineController: PipelineControllerLike;

  constructor(
    orchestrator: OrchestratorLike,
    pipelineController: PipelineControllerLike,
    ruleStore?: RuleStore,
  ) {
    super();
    this.orchestrator = orchestrator;
    this.pipelineController = pipelineController;
    this.ruleStore = ruleStore ?? new RuleStore();

    this.dedupCleanupTimer = setInterval(() => {
      this.cleanupDedupEntries();
    }, DEDUP_CLEANUP_MS);
  }

  // ── Public API ───────────────────────────────────────────────────

  loadRules(): void {
    this.ruleStore
      .getRules()
      .then((rules) => {
        this.rules = rules;
        this.setupCronSchedulers();
      })
      .catch((err) => {
        log.error(
          `[automation-engine] Failed to load rules: ${err instanceof Error ? err.message : String(err)}`,
        );
        this.rules = [];
      });
  }

  /** Synchronous variant for when rules are already loaded. */
  loadRulesSync(rules: AutomationRule[]): void {
    this.rules = [...rules];
    this.setupCronSchedulers();
  }

  saveRules(): void {
    const data = { version: 1 as const, rules: this.rules };
    this.ruleStore.save(data).catch((err) => {
      log.error(
        `[automation-engine] Failed to save rules: ${err instanceof Error ? err.message : String(err)}`,
      );
    });
  }

  addRule(rule: AutomationRule): void {
    const idx = this.rules.findIndex((r) => r.id === rule.id);
    if (idx >= 0) {
      this.rules[idx] = rule;
    } else {
      this.rules.push(rule);
    }
    this.saveRules();
    this.setupCronSchedulers();
  }

  removeRule(ruleId: string): void {
    this.rules = this.rules.filter((r) => r.id !== ruleId);
    this.saveRules();

    // Clear any cron timer for this rule
    const timer = this.cronTimers.get(ruleId);
    if (timer) {
      clearInterval(timer);
      this.cronTimers.delete(ruleId);
    }
  }

  async fire(trigger: TriggerType, context: TriggerContext): Promise<void> {
    const cardId = context.card.id;

    // Deduplication check
    if (this.isDuplicate(cardId)) {
      return;
    }
    this.dedupMap.set(cardId, Date.now());

    // Filter matching enabled rules (max 50)
    const matchingRules = this.rules
      .filter((r) => r.enabled && r.trigger.type === trigger)
      .slice(0, MAX_RULES_PER_EVENT);

    for (const rule of matchingRules) {
      await this.evaluateRule(rule, context);
    }
  }

  getExecutionLog(): RuleExecution[] {
    return [...this.executionLog];
  }

  getRules(): AutomationRule[] {
    return [...this.rules];
  }

  /** Stops all timers. Call on shutdown. */
  dispose(): void {
    if (this.dedupCleanupTimer) {
      clearInterval(this.dedupCleanupTimer);
      this.dedupCleanupTimer = null;
    }
    for (const timer of this.cronTimers.values()) {
      clearInterval(timer);
    }
    this.cronTimers.clear();
  }

  // ── Rule Evaluation ──────────────────────────────────────────────

  private async evaluateRule(rule: AutomationRule, context: TriggerContext): Promise<void> {
    const execution: RuleExecution = {
      id: crypto.randomUUID(),
      ruleId: rule.id,
      cardId: context.card.id,
      triggeredAt: Date.now(),
      status: "running",
      actionResults: [],
    };

    this.executionLog.push(execution);

    // Evaluate conditions — short-circuit on first false
    const conditionsPassed = this.evaluateConditions(rule.conditions, context);

    if (!conditionsPassed) {
      execution.status = "completed";
      execution.completedAt = Date.now();
      return;
    }

    // Execute actions sequentially (max 20)
    const actions = rule.actions.slice(0, MAX_ACTIONS_PER_RULE);

    for (const action of actions) {
      const startTime = Date.now();
      try {
        await this.executeActionWithTimeout(action, context);
        execution.actionResults.push({
          actionType: action.type,
          status: "success",
          durationMs: Date.now() - startTime,
        });
      } catch (err) {
        const errorMsg = err instanceof Error ? err.message : String(err);
        execution.actionResults.push({
          actionType: action.type,
          status: "failed",
          error: errorMsg,
          durationMs: Date.now() - startTime,
        });

        if (!rule.continueOnError) {
          execution.status = "failed";
          execution.completedAt = Date.now();
          this.emit("rule:failed", { ruleId: rule.id, cardId: context.card.id, error: errorMsg });
          return;
        }
      }
    }

    execution.status = "completed";
    execution.completedAt = Date.now();
    this.emit("rule:executed", { ruleId: rule.id, cardId: context.card.id });
  }

  // ── Condition Evaluators ─────────────────────────────────────────

  private evaluateConditions(
    conditions: Array<{ type: ConditionType; params: Record<string, unknown> }>,
    context: TriggerContext,
  ): boolean {
    for (const condition of conditions) {
      if (!this.evaluateCondition(condition, context)) {
        return false;
      }
    }
    return true;
  }

  private evaluateCondition(
    condition: { type: ConditionType; params: Record<string, unknown> },
    context: TriggerContext,
  ): boolean {
    switch (condition.type) {
      case "label_matches":
        return this.evalLabelMatches(condition.params, context);
      case "complexity_below":
        return this.evalComplexityBelow(condition.params, context);
      case "has_description":
        return this.evalHasDescription(context);
      case "no_linked_pr":
        return this.evalNoLinkedPr(context);
      case "assignee_is_me":
        return this.evalAssigneeIsMe(context);
      default:
        // Unknown condition type — unavailable data evaluates to false
        return false;
    }
  }

  private evalLabelMatches(params: Record<string, unknown>, context: TriggerContext): boolean {
    const pattern = params.pattern;
    if (typeof pattern !== "string") return false;

    const labels = context.card.labels;
    if (!labels || labels.length === 0) return false;

    return labels.some((label) => minimatch(label, pattern));
  }

  private evalComplexityBelow(params: Record<string, unknown>, context: TriggerContext): boolean {
    const threshold = params.threshold;
    if (typeof threshold !== "number") return false;

    // Complexity is stored in eventData or card metadata
    const complexity = context.eventData?.complexity;
    if (typeof complexity !== "number") return false;

    return complexity < threshold;
  }

  private evalHasDescription(context: TriggerContext): boolean {
    const desc = context.card.description;
    return typeof desc === "string" && desc.trim().length > 0;
  }

  private evalNoLinkedPr(context: TriggerContext): boolean {
    return context.card.linkedPrNumber == null;
  }

  private evalAssigneeIsMe(context: TriggerContext): boolean {
    const currentUser = context.currentUser;
    if (!currentUser) return false;

    const assignee = context.card.assignee;
    if (!assignee) return false;

    return assignee === currentUser;
  }

  // ── Action Executors ─────────────────────────────────────────────

  private async executeActionWithTimeout(
    action: { type: ActionType; params: Record<string, unknown> },
    context: TriggerContext,
  ): Promise<void> {
    const timeoutPromise = new Promise<never>((_, reject) => {
      setTimeout(() => reject(new Error("Action timed out after 300s")), ACTION_TIMEOUT_MS);
    });

    const actionPromise = this.executeAction(action, context);
    await Promise.race([actionPromise, timeoutPromise]);
  }

  private async executeAction(
    action: { type: ActionType; params: Record<string, unknown> },
    context: TriggerContext,
  ): Promise<void> {
    switch (action.type) {
      case "spawn_agent":
        return this.actionSpawnAgent(action.params, context);
      case "create_pr":
        return this.actionCreatePr(action.params, context);
      case "run_tests":
        return this.actionRunTests(action.params, context);
      case "request_review":
        return this.actionRequestReview(action.params, context);
      case "transition_issue":
        return this.actionTransitionIssue(action.params, context);
      case "add_comment":
        return this.actionAddComment(action.params, context);
      case "merge_pr":
        return this.actionMergePr(context);
      case "iterate_agent":
        return this.actionIterateAgent(action.params, context);
      default:
        throw new Error(`Unknown action type: ${action.type}`);
    }
  }

  private async actionSpawnAgent(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    const prompt = typeof params.prompt === "string"
      ? params.prompt
      : `Work on: ${context.card.title}\n\n${context.card.description}`;
    const agentType = typeof params.agentType === "string" ? params.agentType : "claude-code";
    const repoPath = typeof params.repoPath === "string" ? params.repoPath : "";

    await this.orchestrator.createTask(prompt, agentType, repoPath);
  }

  private async actionCreatePr(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    if (this.pipelineController.createPr) {
      await this.pipelineController.createPr(context.card.id, params);
    }
  }

  private async actionRunTests(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    if (this.pipelineController.runTests) {
      await this.pipelineController.runTests(context.card.id, params);
    }
  }

  private async actionRequestReview(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    if (this.pipelineController.requestReview) {
      await this.pipelineController.requestReview(context.card.id, params);
    }
  }

  private async actionTransitionIssue(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    if (this.pipelineController.transitionIssue) {
      const targetStatus = typeof params.targetStatus === "string" ? params.targetStatus : "";
      await this.pipelineController.transitionIssue(context.card.id, targetStatus);
    }
  }

  private async actionAddComment(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    if (this.pipelineController.addComment) {
      const body = typeof params.body === "string" ? params.body : "";
      await this.pipelineController.addComment(context.card.id, body);
    }
  }

  private async actionMergePr(context: TriggerContext): Promise<void> {
    if (this.pipelineController.mergePr) {
      await this.pipelineController.mergePr(context.card.id);
    }
  }

  private async actionIterateAgent(params: Record<string, unknown>, context: TriggerContext): Promise<void> {
    const taskId = context.card.linkedTaskId;
    if (!taskId) {
      throw new Error("No linked task to iterate on");
    }
    const feedback = typeof params.feedback === "string" ? params.feedback : "";
    await this.orchestrator.iterate(taskId, feedback);
  }

  // ── Deduplication ────────────────────────────────────────────────

  private isDuplicate(cardId: string): boolean {
    const lastTimestamp = this.dedupMap.get(cardId);
    if (lastTimestamp == null) return false;
    return Date.now() - lastTimestamp < DEDUP_WINDOW_MS;
  }

  private cleanupDedupEntries(): void {
    const now = Date.now();
    for (const [cardId, timestamp] of this.dedupMap) {
      if (now - timestamp > DEDUP_CLEANUP_MS) {
        this.dedupMap.delete(cardId);
      }
    }
  }

  // ── Cron Scheduling ──────────────────────────────────────────────

  private setupCronSchedulers(): void {
    // Clear existing cron timers
    for (const timer of this.cronTimers.values()) {
      clearInterval(timer);
    }
    this.cronTimers.clear();

    // Set up timers for schedule-triggered rules
    for (const rule of this.rules) {
      if (rule.trigger.type !== "schedule" || !rule.enabled) continue;

      const cronExpr = rule.trigger.params?.cron;
      if (typeof cronExpr !== "string") continue;

      const schedule = parseCronExpression(cronExpr);
      if (!schedule) {
        log.warn(`[automation-engine] Invalid cron expression for rule "${rule.name}": ${cronExpr}`);
        continue;
      }

      // Check every 60 seconds if the cron matches
      const timer = setInterval(() => {
        if (cronMatchesNow(schedule)) {
          // Fire with a synthetic context for schedule triggers
          const syntheticCard: KanbanCard = {
            id: `schedule-${rule.id}`,
            source: "local",
            externalId: "",
            externalUrl: "",
            title: `Scheduled: ${rule.name}`,
            description: "",
            columnId: "",
            positionIndex: 0,
            priority: "none",
            labels: [],
            pipelineState: "idle",
            iterationCount: 0,
            createdAt: Date.now(),
            updatedAt: Date.now(),
          };

          this.fire("schedule", { card: syntheticCard }).catch((err) => {
            log.error(
              `[automation-engine] Scheduled rule "${rule.name}" failed: ${err instanceof Error ? err.message : String(err)}`,
            );
          });
        }
      }, 60_000);

      this.cronTimers.set(rule.id, timer);
    }
  }
}

// Re-export for convenience
export { parseCronExpression, cronMatchesNow, parseCronField };
export type { CronSchedule };
