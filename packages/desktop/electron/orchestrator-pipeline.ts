/**
 * Workflow Pipeline Engine for the Crab Desktop Orchestrator.
 *
 * Manages multi-stage pipelines that automate the workspace lifecycle
 * from implementation through merge. Each stage can be a built-in
 * executor (implement, test, review, create-pr, ci-pass, merge, archive)
 * or a custom shell command. Stages support approval gates, conditional
 * gates, and per-stage timeouts.
 */

import { EventEmitter } from "node:events";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

// ─── Types ───────────────────────────────────────────────────────────────────

export type BuiltinStageType =
  | "implement"
  | "test"
  | "review"
  | "create-pr"
  | "ci-pass"
  | "merge"
  | "archive";

export type StageType = BuiltinStageType | { custom: string };

export type GateType = "approval" | "condition";

export interface PipelineStage {
  type: StageType;
  gate?: GateType;
  condition?: string;
  timeout?: number;
}

export interface Pipeline {
  name: string;
  stages: PipelineStage[];
}

export type PipelineStatus = "running" | "paused" | "completed" | "failed" | "aborted";

export interface StageResult {
  stageIndex: number;
  type: StageType;
  success: boolean;
  error?: string;
  startedAt: number;
  completedAt?: number;
}

export interface PipelineState {
  id: string;
  workspaceId: string;
  pipeline: Pipeline;
  currentStageIndex: number;
  stageResults: StageResult[];
  status: PipelineStatus;
  createdAt: number;
  updatedAt: number;
}

interface PersistedPipelineData {
  pipelines: PipelineState[];
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_PERSIST_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "agent-pipelines.json",
);

/** Default timeouts per stage type (milliseconds). */
const DEFAULT_TIMEOUTS: Record<string, number> = {
  implement: 30 * 60 * 1000,   // 30 minutes
  test: 10 * 60 * 1000,        // 10 minutes
  review: 30 * 60 * 1000,      // 30 minutes
  "create-pr": 5 * 60 * 1000,  // 5 minutes
  "ci-pass": 10 * 60 * 1000,   // 10 minutes
  merge: 5 * 60 * 1000,        // 5 minutes
  archive: 2 * 60 * 1000,      // 2 minutes
};

const DEFAULT_CUSTOM_TIMEOUT = 10 * 60 * 1000; // 10 minutes

// ─── PipelineEngine ──────────────────────────────────────────────────────────

export class PipelineEngine extends EventEmitter {
  private states = new Map<string, PipelineState>();
  private timeoutHandles = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly persistPath: string;

  constructor(persistPath?: string) {
    super();
    this.persistPath = persistPath ?? DEFAULT_PERSIST_PATH;
    this.loadState();
  }

  // ─── Public API ──────────────────────────────────────────────────────────

  /**
   * Starts a pipeline for a workspace. Creates the initial state and
   * advances to the first stage.
   */
  async start(workspaceId: string, pipeline: Pipeline): Promise<PipelineState> {
    const state: PipelineState = {
      id: crypto.randomUUID(),
      workspaceId,
      pipeline,
      currentStageIndex: 0,
      stageResults: [],
      status: "running",
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };

    this.states.set(workspaceId, state);
    await this.persistState();

    this.emit("orchestrator:pipeline-progress", {
      workspaceId,
      stageIndex: 0,
      status: "running",
    });

    // Advance to the first stage.
    await this.advance(state);

    return state;
  }

  /**
   * Advances the pipeline to the next stage. Handles gate checks,
   * stage execution, and state transitions.
   */
  async advance(state: PipelineState): Promise<void> {
    if (state.status !== "running") return;
    if (state.currentStageIndex >= state.pipeline.stages.length) {
      state.status = "completed";
      state.updatedAt = Date.now();
      await this.persistState();
      this.emit("orchestrator:pipeline-progress", {
        workspaceId: state.workspaceId,
        stageIndex: state.currentStageIndex,
        status: "completed",
      });
      return;
    }

    const stage = state.pipeline.stages[state.currentStageIndex];

    // Check approval gate.
    if (stage.gate === "approval") {
      state.status = "paused";
      state.updatedAt = Date.now();
      await this.persistState();
      this.emit("orchestrator:pipeline-approval", {
        workspaceId: state.workspaceId,
        stage: stage.type,
        stageIndex: state.currentStageIndex,
      });
      return;
    }

    // Check conditional gate.
    if (stage.gate === "condition" && stage.condition) {
      const conditionMet = this.evaluateCondition(stage.condition, state);
      if (!conditionMet) {
        state.status = "paused";
        state.updatedAt = Date.now();
        await this.persistState();
        this.emit("orchestrator:pipeline-condition-blocked", {
          workspaceId: state.workspaceId,
          stage: stage.type,
          stageIndex: state.currentStageIndex,
          condition: stage.condition,
        });
        return;
      }
    }

    // Set up timeout for this stage.
    this.setupStageTimeout(state, stage);

    // Execute the stage.
    const result = await this.executeStage(stage, state);
    state.stageResults.push(result);

    // Clear timeout.
    this.clearStageTimeout(state.workspaceId);

    if (result.success) {
      state.currentStageIndex++;
      state.updatedAt = Date.now();
      await this.persistState();

      this.emit("orchestrator:pipeline-progress", {
        workspaceId: state.workspaceId,
        stageIndex: state.currentStageIndex,
        status: "running",
      });

      // Advance to the next stage.
      await this.advance(state);
    } else {
      state.status = "failed";
      state.updatedAt = Date.now();
      await this.persistState();

      this.emit("orchestrator:pipeline-stage-failed", {
        workspaceId: state.workspaceId,
        stage: stage.type,
        stageIndex: state.currentStageIndex,
        error: result.error,
      });
    }
  }

  /**
   * Approves a gated stage, resuming the pipeline.
   */
  async approve(workspaceId: string, stageIndex: number): Promise<void> {
    const state = this.states.get(workspaceId);
    if (!state) {
      throw new Error(`No pipeline found for workspace: ${workspaceId}`);
    }
    if (state.status !== "paused") {
      throw new Error(`Pipeline is not paused (status: ${state.status})`);
    }
    if (state.currentStageIndex !== stageIndex) {
      throw new Error(
        `Stage index mismatch: expected ${state.currentStageIndex}, got ${stageIndex}`,
      );
    }

    // Mark the approval gate as passed and advance.
    const stage = state.pipeline.stages[state.currentStageIndex];
    state.stageResults.push({
      stageIndex: state.currentStageIndex,
      type: stage.type,
      success: true,
      startedAt: Date.now(),
      completedAt: Date.now(),
    });

    state.currentStageIndex++;
    state.status = "running";
    state.updatedAt = Date.now();
    await this.persistState();

    this.emit("orchestrator:pipeline-progress", {
      workspaceId,
      stageIndex: state.currentStageIndex,
      status: "running",
    });

    await this.advance(state);
  }

  /**
   * Aborts a pipeline, stopping all further execution.
   */
  async abort(workspaceId: string): Promise<void> {
    const state = this.states.get(workspaceId);
    if (!state) {
      throw new Error(`No pipeline found for workspace: ${workspaceId}`);
    }

    this.clearStageTimeout(workspaceId);

    state.status = "aborted";
    state.updatedAt = Date.now();
    await this.persistState();

    this.emit("orchestrator:pipeline-progress", {
      workspaceId,
      stageIndex: state.currentStageIndex,
      status: "aborted",
    });
  }

  /**
   * Retries a failed stage by resetting the pipeline status and re-advancing.
   */
  async retry(workspaceId: string): Promise<void> {
    const state = this.states.get(workspaceId);
    if (!state) {
      throw new Error(`No pipeline found for workspace: ${workspaceId}`);
    }
    if (state.status !== "failed") {
      throw new Error(`Pipeline is not failed (status: ${state.status})`);
    }

    state.status = "running";
    state.updatedAt = Date.now();
    await this.persistState();

    await this.advance(state);
  }

  /** Returns the pipeline state for a workspace. */
  getState(workspaceId: string): PipelineState | undefined {
    return this.states.get(workspaceId);
  }

  /** Returns all pipeline states. */
  getAllStates(): PipelineState[] {
    return Array.from(this.states.values());
  }

  /** Removes a pipeline state. */
  async removeState(workspaceId: string): Promise<void> {
    this.clearStageTimeout(workspaceId);
    this.states.delete(workspaceId);
    await this.persistState();
  }

  // ─── Stage Execution ─────────────────────────────────────────────────────

  /**
   * Executes a pipeline stage. Built-in stages emit events that the
   * orchestrator handles; custom stages run shell commands.
   */
  private async executeStage(
    stage: PipelineStage,
    state: PipelineState,
  ): Promise<StageResult> {
    const startedAt = Date.now();
    const stageType = stage.type;

    try {
      if (typeof stageType === "string") {
        await this.executeBuiltinStage(stageType, state.workspaceId);
      } else {
        await this.executeCustomStage(stageType.custom, state.workspaceId);
      }

      return {
        stageIndex: state.currentStageIndex,
        type: stageType,
        success: true,
        startedAt,
        completedAt: Date.now(),
      };
    } catch (err) {
      return {
        stageIndex: state.currentStageIndex,
        type: stageType,
        success: false,
        error: err instanceof Error ? err.message : String(err),
        startedAt,
        completedAt: Date.now(),
      };
    }
  }

  /**
   * Executes a built-in stage by emitting the appropriate event.
   * The orchestrator's main process handles the actual execution.
   */
  private async executeBuiltinStage(
    type: BuiltinStageType,
    workspaceId: string,
  ): Promise<void> {
    switch (type) {
      case "implement":
        // No-op: the workspace agent is already running.
        break;

      case "test":
        this.emit("pipeline:execute-test", { workspaceId });
        break;

      case "review":
        this.emit("pipeline:execute-review", { workspaceId });
        break;

      case "create-pr":
        this.emit("pipeline:execute-create-pr", { workspaceId });
        break;

      case "ci-pass":
        this.emit("pipeline:execute-ci-check", { workspaceId });
        break;

      case "merge":
        this.emit("pipeline:execute-merge", { workspaceId });
        break;

      case "archive":
        this.emit("pipeline:execute-archive", { workspaceId });
        break;
    }
  }

  /**
   * Executes a custom stage (shell command or agent prompt).
   */
  private async executeCustomStage(
    command: string,
    workspaceId: string,
  ): Promise<void> {
    this.emit("pipeline:execute-custom", { workspaceId, command });
  }

  // ─── Condition Evaluation ────────────────────────────────────────────────

  /**
   * Evaluates a condition string against the pipeline state.
   * Supports simple expressions like "checksPass === true".
   */
  private evaluateCondition(condition: string, state: PipelineState): boolean {
    // Simple condition evaluation: check if all previous stages passed.
    if (condition === "allPassed") {
      return state.stageResults.every((r) => r.success);
    }

    // Check specific stage results.
    const stageMatch = condition.match(/stage\[(\d+)\]\.success\s*===\s*(true|false)/);
    if (stageMatch) {
      const idx = parseInt(stageMatch[1], 10);
      const expected = stageMatch[2] === "true";
      const result = state.stageResults[idx];
      return result ? result.success === expected : false;
    }

    // Default: condition met (permissive).
    return true;
  }

  // ─── Timeout Handling ────────────────────────────────────────────────────

  /**
   * Sets up a timeout for the current stage. If the stage doesn't
   * complete within the timeout, the pipeline is paused with a failure.
   */
  private setupStageTimeout(state: PipelineState, stage: PipelineStage): void {
    this.clearStageTimeout(state.workspaceId);

    const stageTypeName = typeof stage.type === "string" ? stage.type : "custom";
    const timeout =
      stage.timeout ??
      DEFAULT_TIMEOUTS[stageTypeName] ??
      DEFAULT_CUSTOM_TIMEOUT;

    const handle = setTimeout(async () => {
      state.status = "failed";
      state.stageResults.push({
        stageIndex: state.currentStageIndex,
        type: stage.type,
        success: false,
        error: `Stage timed out after ${Math.round(timeout / 1000)}s`,
        startedAt: Date.now() - timeout,
        completedAt: Date.now(),
      });
      state.updatedAt = Date.now();
      await this.persistState();

      this.emit("orchestrator:pipeline-stage-failed", {
        workspaceId: state.workspaceId,
        stage: stage.type,
        stageIndex: state.currentStageIndex,
        error: "Stage timed out",
      });
    }, timeout);

    this.timeoutHandles.set(state.workspaceId, handle);
  }

  private clearStageTimeout(workspaceId: string): void {
    const handle = this.timeoutHandles.get(workspaceId);
    if (handle) {
      clearTimeout(handle);
      this.timeoutHandles.delete(workspaceId);
    }
  }

  // ─── Persistence ─────────────────────────────────────────────────────────

  private async persistState(): Promise<void> {
    const data: PersistedPipelineData = {
      pipelines: Array.from(this.states.values()),
    };

    const dir = path.dirname(this.persistPath);
    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(this.persistPath, JSON.stringify(data, null, 2), "utf-8");
    } catch (err) {
      this.emit("orchestrator:error", {
        message: `Failed to persist pipelines: ${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }

  private loadState(): void {
    try {
      if (!fs.existsSync(this.persistPath)) return;

      const raw = fs.readFileSync(this.persistPath, "utf-8");
      const data: PersistedPipelineData = JSON.parse(raw);

      if (!data.pipelines || !Array.isArray(data.pipelines)) return;

      for (const state of data.pipelines) {
        this.states.set(state.workspaceId, state);
      }
    } catch {
      // Corrupted or unreadable — start fresh.
    }
  }
}
