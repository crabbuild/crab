import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";

import type { Orchestrator, Task, TaskStatus } from "../orchestrator";
import type { AgentType } from "../orchestrator-adapters";
import type {
  AgentWorkflowDefinition,
  AgentWorkflowEdgeRun,
  AgentWorkflowEvent,
  AgentWorkflowEventType,
  AgentWorkflowNode,
  AgentWorkflowNodeRun,
  AgentWorkflowRetryNodeOptions,
  AgentWorkflowRun,
  AgentWorkflowSimulationResult,
  AgentWorkflowStartRunOptions,
  NodeRunStatus,
  WorkflowConditionGroup,
  WorkflowIfElseBranch,
  WorkflowNodeCondition,
  WorkflowValueRef,
  WorkflowRunStatus,
} from "./types";
import { AgentWorkflowDefinitionStore, AgentWorkflowRunStore } from "./persistence";
import {
  evaluateWorkflowEdgeCondition,
  getDownstreamNodeIds,
  getIncomingEdges,
  getOutgoingEdges,
  getUpstreamNodeIds,
  validateAgentWorkflowDefinition,
} from "./validator";
import { evaluateWorkflowCelCondition } from "./cel";
import {
  aggregateJoinOutput,
  parseStructuredOutput,
  resolveTemplate,
  resolveValueRef,
  validateJsonSchemaSubset,
} from "./output";

type RunnerEvents = {
  "run-updated": (run: AgentWorkflowRun) => void;
  "node-event": (event: AgentWorkflowEvent) => void;
};

const ACTIVE_RUN_STATUSES = new Set<WorkflowRunStatus>(["queued", "running", "paused"]);
const TERMINAL_NODE_STATUSES = new Set<NodeRunStatus>(["completed", "failed", "cancelled", "skipped"]);
const ACTIVE_NODE_STATUSES = new Set<NodeRunStatus>(["ready", "queued", "running"]);
const ORCHESTRATOR_FAILURE_STATUSES = new Set<TaskStatus>(["failed", "discarded", "stopped", "interrupted"]);
const ORCHESTRATOR_TERMINAL_STATUSES = new Set<TaskStatus>(["applied", "discarded", "failed", "stopped", "interrupted", "archived"]);
const WATCHDOG_INTERVAL_MS = 5_000;

type TaskSyncResult = "unchanged" | "changed" | "handled";

export class AgentWorkflowRunner extends EventEmitter {
  private activeSchedules = new Set<string>();
  private pendingSchedules = new Set<string>();
  private watchdogTimer: ReturnType<typeof setInterval> | null = null;
  private nodeTimeouts = new Map<string, ReturnType<typeof setTimeout>>();
  private reviewTimeouts = new Map<string, ReturnType<typeof setTimeout>>();

  constructor(
    private readonly orchestrator: Orchestrator,
    private readonly runStore: AgentWorkflowRunStore = new AgentWorkflowRunStore(),
    private readonly definitionStore: AgentWorkflowDefinitionStore = new AgentWorkflowDefinitionStore(),
  ) {
    super();
    this.orchestrator.on("orchestrator:task-status", (payload: { taskId: string; status: TaskStatus }) => {
      this.handleTaskStatus(payload.taskId, payload.status).catch((err) => {
        this.emitWorkflowError(`Task status handling failed: ${err instanceof Error ? err.message : String(err)}`);
      });
    });
    this.watchdogTimer = setInterval(() => {
      this.reconcileActiveRuns().catch((err) => {
        this.emitWorkflowError(`Workflow reconciliation failed: ${err instanceof Error ? err.message : String(err)}`);
      });
    }, WATCHDOG_INTERVAL_MS);
    this.watchdogTimer.unref?.();
  }

  override on<K extends keyof RunnerEvents>(eventName: K, listener: RunnerEvents[K]): this {
    return super.on(eventName, listener);
  }

  async recoverActiveRuns(): Promise<void> {
    const activeRuns = (await this.runStore.list()).filter((run) => ACTIVE_RUN_STATUSES.has(run.status));
    const childRuns = activeRuns.filter((run) => run.parentRunId);
    const parentRuns = activeRuns.filter((run) => !run.parentRunId);
    for (const run of [...childRuns, ...parentRuns]) {
      await this.reconcileRun(run);
      await this.schedule(run.id);
    }
  }

  async startRun(
    repoPath: string,
    definition: AgentWorkflowDefinition,
    options: AgentWorkflowStartRunOptions = {},
  ): Promise<AgentWorkflowRun> {
    const definitionSnapshot = deepClone(options.definitionSnapshot ?? definition);
    const validation = validateAgentWorkflowDefinition(definitionSnapshot);
    if (!validation.valid) {
      throw new Error(validation.errors.join("\n"));
    }

    const now = Date.now();
    const trigger = options.trigger ?? {
      type: options.parentRunId ? "sub_workflow" as const : options.sourceTaskId ? "chat" as const : "builder" as const,
    };
    const run: AgentWorkflowRun = {
      id: randomUUID(),
      definitionId: definitionSnapshot.id,
      definitionSnapshot,
      repoPath,
      status: "queued",
      trigger,
      sourceTaskId: options.sourceTaskId,
      parentRunId: options.parentRunId,
      parentNodeRunId: options.parentNodeRunId,
      input: options.input,
      state: {},
      nodeRuns: definitionSnapshot.nodes.map((node) => ({
        id: randomUUID(),
        nodeId: node.id,
        status: "blocked",
        attempt: 1,
      })),
      edgeRuns: definitionSnapshot.edges.map((edge) => ({
        edgeId: edge.id,
        status: "pending",
      })),
      startedAt: now,
    };

    for (const nodeRun of run.nodeRuns) {
      if (getIncomingEdges(definitionSnapshot, nodeRun.nodeId).length === 0) {
        nodeRun.status = "ready";
      }
    }

    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "run_started", "Workflow run started.", undefined, {
      definitionVersion: definitionSnapshot.definitionVersion,
      revisionId: definitionSnapshot.revisionId,
      trigger,
    });
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  async getRun(runId: string): Promise<AgentWorkflowRun | null> {
    const run = await this.runStore.get(runId);
    if (!run) return null;
    if (ACTIVE_RUN_STATUSES.has(run.status)) {
      await this.reconcileRun(run);
      return this.runStore.get(runId);
    }
    return run;
  }

  async listRuns(repoPath?: string): Promise<AgentWorkflowRun[]> {
    await this.reconcileActiveRuns(repoPath);
    return this.runStore.list(repoPath);
  }

  async listEvents(runId: string): Promise<AgentWorkflowEvent[]> {
    return this.runStore.listEvents(runId);
  }

  async cancelRun(runId: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const now = Date.now();

    for (const nodeRun of run.nodeRuns) {
      if (TERMINAL_NODE_STATUSES.has(nodeRun.status)) continue;
      if (nodeRun.taskId && this.shouldCancelLinkedTask(nodeRun.taskId)) {
        await this.orchestrator.cancelTask(nodeRun.taskId).catch(() => {});
      }
      if (nodeRun.childRunId) {
        await this.cancelRun(nodeRun.childRunId).catch(() => {});
      }
      nodeRun.status = "cancelled";
      nodeRun.finishedAt = now;
      await this.recordEvent(run.id, "node_cancelled", "Node cancelled.", nodeRun.id);
    }

    run.status = "cancelled";
    run.finishedAt = now;
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "run_cancelled", "Workflow run cancelled.");
    return run;
  }

  private shouldCancelLinkedTask(taskId: string): boolean {
    const task = this.orchestrator.getTask(taskId);
    return !task || !ORCHESTRATOR_TERMINAL_STATUSES.has(task.status);
  }

  async approveNode(runId: string, nodeRunId: string, summary?: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const nodeRun = this.requireNodeRun(run, nodeRunId);
    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);

    if (nodeRun.status !== "needs_review" && nodeRun.status !== "running") {
      throw new Error(`Cannot approve node in status ${nodeRun.status}.`);
    }

    const reason = (summary?.trim() || "").trim();
    const now = Date.now();

    if (node.type === "approval_gate") {
      nodeRun.status = "completed";
      nodeRun.finishedAt = now;
      nodeRun.approvalReason = reason || "Approved.";
      nodeRun.outputJson = {
        verdict: "approved",
        branch: "approved",
        reason: nodeRun.approvalReason,
      };
      nodeRun.outputSummary = `${node.title} approved.${reason ? ` Reason: ${reason}` : ""}`;
      this.applyNodeStateUpdates(run, node, nodeRun);
      await this.recordEvent(run.id, "node_completed", `${node.title} approved.`, nodeRun.id, {
        outputJson: nodeRun.outputJson,
        state: run.state,
      });
      await this.resolveOutgoingEdges(run, nodeRun);
      await this.applyReadiness(run);
      this.updateRunStatus(run);
      await this.persistAndEmit(run);
      await this.schedule(run.id);
      return (await this.runStore.get(run.id)) ?? run;
    }

    if (node.type === "human_review") {
      const feedback = reason || nodeRun.outputSummary || "Reviewed.";
      const outputSummary = reason || nodeRun.outputSummary || `${node.title} approved.`;
      // Human review can supply schema-bound data to child workflows just as
      // agent nodes do. Validate it before completing the node or its edges.
      if (!this.applyStructuredOutput(node, nodeRun, outputSummary)) {
        await this.handleNodeFailure(run, nodeRun, node, nodeRun.outputJsonError || "invalid_structured_output");
        return (await this.runStore.get(run.id)) ?? run;
      }
      if (!node.outputSchema) {
        nodeRun.outputJson = { feedback, decision: "approved" };
      }
      nodeRun.status = "completed";
      nodeRun.finishedAt = now;
      nodeRun.reviewFeedback = [...(nodeRun.reviewFeedback ?? []), reason || "Approved."];
      this.applyNodeStateUpdates(run, node, nodeRun);
      await this.recordEvent(run.id, "node_completed", `${node.title} approved.`, nodeRun.id, {
        outputJson: nodeRun.outputJson,
        state: run.state,
      });
      await this.resolveOutgoingEdges(run, nodeRun);
      await this.applyReadiness(run);
      this.updateRunStatus(run);
      await this.persistAndEmit(run);
      await this.schedule(run.id);
      return (await this.runStore.get(run.id)) ?? run;
    }

    const outputSummary = (summary?.trim() || nodeRun.outputSummary || `${node.title} approved.`).trim();
    const structuredOk = this.applyStructuredOutput(node, nodeRun, outputSummary);
    if (!structuredOk) {
      await this.handleNodeFailure(run, nodeRun, node, nodeRun.outputJsonError || "invalid_structured_output");
      return (await this.runStore.get(run.id)) ?? run;
    }

    nodeRun.status = "completed";
    nodeRun.outputSummary = outputSummary;
    nodeRun.finishedAt = now;
    this.applyNodeStateUpdates(run, node, nodeRun);
    await this.recordEvent(run.id, "node_completed", `${node.title} completed.`, nodeRun.id, {
      outputSummary: nodeRun.outputSummary,
      outputJson: nodeRun.outputJson,
      state: run.state,
    });

    await this.resolveOutgoingEdges(run, nodeRun);
    await this.applyReadiness(run);
    this.updateRunStatus(run);
    await this.persistAndEmit(run);
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  async requestNodeChanges(runId: string, nodeRunId: string, feedback: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const nodeRun = this.requireNodeRun(run, nodeRunId);
    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
    const trimmedFeedback = feedback.trim();

    if (!trimmedFeedback) throw new Error("Feedback is required.");
    nodeRun.reviewFeedback = [...(nodeRun.reviewFeedback ?? []), trimmedFeedback];

    if (!nodeRun.taskId) {
      nodeRun.status = "needs_review";
      await this.persistAndEmit(run);
      await this.recordEvent(run.id, "node_feedback", `Feedback added to ${node.title}.`, nodeRun.id, {
        feedback: trimmedFeedback,
      });
      return run;
    }

    nodeRun.status = "running";
    nodeRun.finishedAt = undefined;
    await this.orchestrator.iterate(nodeRun.taskId, trimmedFeedback);
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "node_feedback", `Changes requested for ${node.title}.`, nodeRun.id, {
      taskId: nodeRun.taskId,
      feedback: trimmedFeedback,
    });
    return run;
  }

  async rejectNode(runId: string, nodeRunId: string, reason: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const nodeRun = this.requireNodeRun(run, nodeRunId);
    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
    const trimmedReason = reason.trim() || "Rejected by user.";
    const now = Date.now();

    if (node.type === "approval_gate") {
      nodeRun.status = "completed";
      nodeRun.finishedAt = now;
      nodeRun.approvalReason = trimmedReason;
      nodeRun.outputJson = {
        verdict: "rejected",
        branch: "rejected",
        reason: trimmedReason,
      };
      nodeRun.outputSummary = `${node.title} rejected. Reason: ${trimmedReason}`;
      this.applyNodeStateUpdates(run, node, nodeRun);
      await this.recordEvent(run.id, "node_completed", `${node.title} rejected (routed to reject dock).`, nodeRun.id, {
        outputJson: nodeRun.outputJson,
        state: run.state,
      });
      await this.resolveOutgoingEdges(run, nodeRun);
      await this.applyReadiness(run);
      this.updateRunStatus(run);
      await this.persistAndEmit(run);
      await this.schedule(run.id);
      return (await this.runStore.get(run.id)) ?? run;
    }

    nodeRun.status = "failed";
    nodeRun.error = trimmedReason;
    nodeRun.finishedAt = now;
    await this.recordEvent(run.id, "node_failed", `${node.title} rejected.`, nodeRun.id, {
      reason: nodeRun.error,
    });

    await this.resolveOutgoingEdges(run, nodeRun);
    await this.applyReadiness(run);
    this.updateRunStatus(run);
    await this.persistAndEmit(run);
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  async requestHumanReviewChanges(runId: string, nodeRunId: string, feedback: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const nodeRun = this.requireNodeRun(run, nodeRunId);
    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
    const trimmedFeedback = feedback.trim();

    if (node.type !== "human_review") {
      throw new Error("requestHumanReviewChanges is only supported for human_review nodes.");
    }
    if (!trimmedFeedback) throw new Error("Feedback is required.");
    if (nodeRun.status !== "needs_review") {
      throw new Error(`Cannot request changes in status ${nodeRun.status}.`);
    }

    nodeRun.reviewFeedback = [...(nodeRun.reviewFeedback ?? []), trimmedFeedback];
    nodeRun.outputJson = { feedback: trimmedFeedback, decision: "changes_requested" };

    const targetNodeId = node.humanReview?.reviewTargetNodeId
      ?? run.definitionSnapshot.edges
          .filter((edge) => edge.targetNodeId === node.id)
          .map((edge) => edge.sourceNodeId)
          .find((sourceId) => run.definitionSnapshot.nodes.find((n) => n.id === sourceId)?.type === "agent_task");

    if (!targetNodeId) {
      throw new Error("No upstream agent_task node found to send rework feedback.");
    }

    nodeRun.status = "blocked";
    nodeRun.outputSummary = undefined;
    nodeRun.finishedAt = undefined;
    await this.recordEvent(run.id, "node_feedback", `Changes requested for ${node.title}.`, nodeRun.id, {
      feedback: trimmedFeedback,
      targetNodeId,
    });

    const targetNodeRun = this.requireNodeRun(run, targetNodeId);
    // Verify target node exists (suppresses unused var)
    void this.requireNode(run.definitionSnapshot, targetNodeId);
    targetNodeRun.status = "blocked";
    targetNodeRun.taskId = undefined;
    targetNodeRun.outputSummary = undefined;
    targetNodeRun.outputJson = undefined;
    targetNodeRun.outputJsonError = undefined;
    targetNodeRun.error = undefined;
    targetNodeRun.startedAt = undefined;
    targetNodeRun.finishedAt = undefined;
    targetNodeRun.reviewFeedback = [...(targetNodeRun.reviewFeedback ?? []), trimmedFeedback];

    for (const edgeRun of run.edgeRuns) {
      const edge = run.definitionSnapshot.edges.find((candidate) => candidate.id === edgeRun.edgeId);
      if (!edge) continue;
      if (edge.sourceNodeId === targetNodeId && edge.targetNodeId === node.id) {
        edgeRun.status = "pending";
        edgeRun.evaluatedAt = undefined;
        edgeRun.reason = undefined;
      }
    }

    if (getIncomingEdges(run.definitionSnapshot, targetNodeId).length === 0) {
      targetNodeRun.status = "ready";
    }

    await this.persistAndEmit(run);
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  async retryNode(
    runId: string,
    nodeRunId: string,
    _options: AgentWorkflowRetryNodeOptions = {},
  ): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    const nodeRun = this.requireNodeRun(run, nodeRunId);
    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);

    if (!TERMINAL_NODE_STATUSES.has(nodeRun.status)) {
      throw new Error(`Cannot retry node in status ${nodeRun.status}.`);
    }

    this.resetNodeAndDownstream(run, nodeRun.nodeId);
    const targetRun = this.requireNodeRun(run, nodeRunId);
    targetRun.attempt += 1;
    if (run.status === "failed" || run.status === "cancelled" || run.status === "completed" || run.status === "paused") {
      run.status = "running";
      run.finishedAt = undefined;
    }

    await this.applyReadiness(run);
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "node_retried", `${node.title} queued for retry.`, targetRun.id, {
      attempt: targetRun.attempt,
    });
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  async pauseRun(runId: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    if (!ACTIVE_RUN_STATUSES.has(run.status)) {
      throw new Error(`Cannot pause run in status ${run.status}.`);
    }
    run.status = "paused";
    run.userPaused = true;
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "run_paused", "Workflow run paused by user.");
    return run;
  }

  async resumeRun(runId: string): Promise<AgentWorkflowRun> {
    const run = await this.requireRun(runId);
    if (run.status !== "paused" || !run.userPaused) {
      throw new Error(`Cannot resume run in status ${run.status}.`);
    }
    run.status = "running";
    run.userPaused = false;
    run.finishedAt = undefined;
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "run_resumed", "Workflow run resumed by user.");
    await this.schedule(run.id);
    return (await this.runStore.get(run.id)) ?? run;
  }

  simulateRun(
    definition: AgentWorkflowDefinition,
    input?: Record<string, unknown>,
  ): AgentWorkflowSimulationResult {
    const nodes: AgentWorkflowSimulationResult["nodes"] = [];
    const edges: AgentWorkflowSimulationResult["edges"] = [];
    const errors: string[] = [];

    const nodeStatus = new Map<string, "pending" | "activated" | "skipped" | "blocked" | "error">();
    const edgeStatus = new Map<string, "pending" | "activated" | "not_activated" | "blocked">();

    for (const node of definition.nodes) {
      nodeStatus.set(node.id, "pending");
    }
    for (const edge of definition.edges) {
      edgeStatus.set(edge.id, "pending");
    }

    // Build a synthetic run for condition evaluation
    const syntheticRun: AgentWorkflowRun = {
      id: "sim",
      definitionId: definition.id,
      definitionSnapshot: definition,
      repoPath: "",
      status: "running",
      input,
      state: {},
      nodeRuns: definition.nodes.map((node) => ({
        id: `sim-${node.id}`,
        nodeId: node.id,
        status: "blocked" as NodeRunStatus,
        attempt: 1,
      })),
      edgeRuns: definition.edges.map((edge) => ({
        edgeId: edge.id,
        status: "pending",
      })),
      startedAt: Date.now(),
    };

    // Mark root nodes (no incoming edges) as activated
    for (const node of definition.nodes) {
      if (getIncomingEdges(definition, node.id).length === 0) {
        nodeStatus.set(node.id, "activated");
      }
    }

    // Evaluate if_else nodes: determine which branch activates
    const branchSelections = new Map<string, string>(); // ifElseNodeId → selectedBranchId
    for (const node of definition.nodes) {
      if (node.type !== "if_else") continue;
      if (nodeStatus.get(node.id) !== "activated") continue;
      const selection = this.safeSelectIfElseBranch(node, syntheticRun);
      if ("error" in selection) {
        nodeStatus.set(node.id, "error");
        errors.push(`Node ${node.title || node.id}: ${selection.error}`);
        nodes.push({
          nodeId: node.id,
          title: node.title,
          type: node.type,
          status: "error",
          reason: selection.error,
        });
        continue;
      }
      branchSelections.set(node.id, selection.branchId);
      nodeStatus.set(node.id, "activated");
      nodes.push({
        nodeId: node.id,
        title: node.title,
        type: node.type,
        status: "activated",
        branchId: selection.branchId,
        branchLabel: selection.label,
        reason: `Routed to ${selection.label || selection.branchId}: ${selection.reason}`,
      });
    }

    // Evaluate edges propagating from activated nodes
    const visited = new Set<string>();
    const queue: string[] = [];
    for (const node of definition.nodes) {
      if (nodeStatus.get(node.id) === "activated") {
        queue.push(node.id);
      }
    }

    while (queue.length > 0) {
      const sourceNodeId = queue.shift()!;
      if (visited.has(sourceNodeId)) continue;
      visited.add(sourceNodeId);

      for (const edge of getOutgoingEdges(definition, sourceNodeId)) {
        if (edgeStatus.get(edge.id) !== "pending") continue;

        const sourceStatus = nodeStatus.get(sourceNodeId);
        if (sourceStatus === "skipped" || sourceStatus === "blocked" || sourceStatus === "error") {
          edgeStatus.set(edge.id, "blocked");
          edges.push({
            edgeId: edge.id,
            sourceNodeId,
            targetNodeId: edge.targetNodeId,
            status: "blocked",
            reason: `Source node ${sourceNodeId} is ${sourceStatus}.`,
          });
          nodeStatus.set(edge.targetNodeId, "blocked");
          continue;
        }

        // Check if this edge matches the selected branch (for if_else nodes)
        const sourceNode = definition.nodes.find((n) => n.id === sourceNodeId);
        if (sourceNode?.type === "if_else" && edge.condition?.branchId) {
          const selected = branchSelections.get(sourceNodeId);
          if (selected !== edge.condition.branchId) {
            edgeStatus.set(edge.id, "not_activated");
            edges.push({
              edgeId: edge.id,
              sourceNodeId,
              targetNodeId: edge.targetNodeId,
              status: "not_activated",
              reason: `Branch ${edge.condition.branchId} not selected; ${selected} was chosen instead.`,
            });
            continue;
          }
        }

        // Check failure edges — when simulating, only success edges propagate
        if (edge.condition?.sourceStatus === "failed") {
          edgeStatus.set(edge.id, "not_activated");
          edges.push({
            edgeId: edge.id,
            sourceNodeId,
            targetNodeId: edge.targetNodeId,
            status: "not_activated",
            reason: "Failure branches are not activated during simulation; assume success.",
          });
          continue;
        }

        edgeStatus.set(edge.id, "activated");
        edges.push({
          edgeId: edge.id,
          sourceNodeId,
          targetNodeId: edge.targetNodeId,
          status: "activated",
        });
        nodeStatus.set(edge.targetNodeId, "activated");
        queue.push(edge.targetNodeId);
      }
    }

    // Build final node results
    const seenNodes = new Set(nodes.map((n) => n.nodeId));
    for (const node of definition.nodes) {
      if (seenNodes.has(node.id)) continue;
      const status = nodeStatus.get(node.id) ?? "blocked";
      if (status === "skipped" || status === "blocked") {
        nodes.push({
          nodeId: node.id,
          title: node.title,
          type: node.type,
          status,
          reason: status === "skipped" ? "No incoming edge activated." : "Upstream nodes did not activate.",
        });
      } else if (status === "activated") {
        nodes.push({
          nodeId: node.id,
          title: node.title,
          type: node.type,
          status: "activated",
        });
      }
    }

    return { nodes, edges, errors };
  }

  private safeSelectIfElseBranch(
    node: AgentWorkflowNode,
    run: AgentWorkflowRun,
  ): { branchId: string; label: string; kind: WorkflowIfElseBranch["kind"]; matched: boolean; reason: string } | { error: string } {
    try {
      return this.selectIfElseBranch(node, run);
    } catch (err: unknown) {
      return { error: err instanceof Error ? err.message : String(err) };
    }
  }

  private async schedule(runId: string): Promise<void> {
    if (this.activeSchedules.has(runId)) {
      this.pendingSchedules.add(runId);
      return;
    }
    this.activeSchedules.add(runId);

    try {
      let run = await this.runStore.get(runId);
      if (!run || !ACTIVE_RUN_STATUSES.has(run.status)) return;
      await this.reconcileRun(run);
      run = await this.runStore.get(runId);
      if (!run || !ACTIVE_RUN_STATUSES.has(run.status)) return;
      if (run.userPaused) return;

      let progressed = true;
      while (progressed) {
        progressed = await this.applyReadiness(run);
        const activeCount = this.activeExecutionCount(run);
        const capacity = Math.max(0, Math.max(1, run.definitionSnapshot.maxParallelNodes || 1) - activeCount);
        let dispatched = 0;

        for (const nodeRun of run.nodeRuns) {
          if (nodeRun.status !== "ready") continue;
          const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
          const consumesCapacity = node.type === "agent_task" || node.type === "sub_workflow" || node.type === "loop";
          if (consumesCapacity && dispatched >= capacity) continue;
          const didStart = await this.startNode(run, nodeRun, node);
          if (didStart) {
            progressed = true;
            if (consumesCapacity) dispatched += 1;
          }
        }
      }

      this.updateRunStatus(run);
      await this.persistAndEmit(run);
    } finally {
      this.activeSchedules.delete(runId);
      if (this.pendingSchedules.delete(runId)) {
        await this.schedule(runId);
      }
    }
  }

  private async startNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<boolean> {
    switch (node.type) {
      case "agent_task":
        await this.startAgentNode(run, nodeRun, node);
        return true;
      case "human_review":
      case "approval_gate":
        await this.startReviewNode(run, nodeRun, node);
        return true;
      case "join":
        await this.completeJoinNode(run, nodeRun, node);
        return true;
      case "sub_workflow":
        await this.startSubWorkflowNode(run, nodeRun, node);
        return true;
      case "if_else":
        await this.completeIfElseNode(run, nodeRun, node);
        return true;
      case "loop":
        await this.startLoopNode(run, nodeRun, node);
        return true;
    }
  }

  private async startAgentNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    const prompt = this.buildAgentPrompt(run, node, nodeRun);
    const effectivePrompt = node.planFirst
      ? `First, propose an implementation plan. List files to change, approach, and risks. Wait for approval before making changes.\n\nTask: ${prompt}`
      : prompt;
    const agentType = (node.agentType ?? run.definitionSnapshot.defaultAgentType ?? "codex") as AgentType;

    try {
      const task = await this.orchestrator.createTask(effectivePrompt, agentType, run.repoPath, "HEAD");
      task.title = node.title;
      task.workflowRunId = run.id;
      task.workflowNodeRunId = nodeRun.id;
      if (run.sourceTaskId) {
        await this.orchestrator.setTaskParent(task.id, run.sourceTaskId);
      }
      if (node.planFirst) task.planFirst = true;
      if (node.model) (task as Task & { model?: string }).model = node.model;
      if (node.intelligence) (task as Task & { reasoningLevel?: string }).reasoningLevel = node.intelligence;

      await this.orchestrator.persistTasks();

      nodeRun.taskId = task.id;
      nodeRun.status = "running";
      nodeRun.startedAt = Date.now();
      nodeRun.finishedAt = undefined;
      run.status = "running";
      await this.persistAndEmit(run);
      await this.recordEvent(run.id, "node_started", `${node.title} started.`, nodeRun.id, { taskId: task.id });

      if (node.timeoutMs && node.timeoutMs > 0) {
        this.clearNodeTimeout(nodeRun.id);
        this.nodeTimeouts.set(nodeRun.id, setTimeout(async () => {
          const latestRun = await this.runStore.get(run.id);
          if (!latestRun) return;
          const latestNodeRun = latestRun.nodeRuns.find((candidate) => candidate.id === nodeRun.id);
          if (!latestNodeRun || latestNodeRun.status !== "running") return;
          await this.handleNodeFailure(latestRun, latestNodeRun, node, "node_timeout");
        }, node.timeoutMs));
      }

      this.orchestrator.startTask(task.id).catch(async (err: unknown) => {
        const latestRun = await this.runStore.get(run.id);
        if (!latestRun) return;
        const latestNodeRun = latestRun.nodeRuns.find((candidate) => candidate.id === nodeRun.id);
        if (!latestNodeRun) return;
        await this.handleNodeFailure(latestRun, latestNodeRun, node, `dispatch_error: ${errorMessage(err)}`);
      });
    } catch (err: unknown) {
      await this.handleNodeFailure(run, nodeRun, node, `dispatch_error: ${errorMessage(err)}`);
    }
  }

  private async startReviewNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    nodeRun.status = "needs_review";
    nodeRun.startedAt = nodeRun.startedAt ?? Date.now();
    if (node.type === "human_review") {
      const upstreamAgentOutput = run.nodeRuns
        .filter((nr) => {
          const n = run.definitionSnapshot.nodes.find((nd) => nd.id === nr.nodeId);
          return n?.type === "agent_task" && run.definitionSnapshot.edges.some(
            (edge) => edge.sourceNodeId === nr.nodeId && edge.targetNodeId === node.id
          );
        })
        .map((nr) => {
          const n = run.definitionSnapshot.nodes.find((nd) => nd.id === nr.nodeId);
          return nr.outputSummary ? `${n?.title ?? nr.nodeId}: ${nr.outputSummary}` : null;
        })
        .filter(Boolean) as string[];
      const context = upstreamAgentOutput.length > 0
        ? `Upstream agent output:\n${upstreamAgentOutput.join("\n")}\n\nReview instructions:\n${node.instructions}`
        : node.instructions;
      nodeRun.outputSummary = nodeRun.outputSummary ?? context;
    }
    await this.recordEvent(run.id, "node_needs_review", `${node.title} is waiting for review.`, nodeRun.id);

    const reviewConfig = node.type === "approval_gate" ? node.approvalGate : node.type === "human_review" ? node.humanReview : undefined;
    if (reviewConfig?.reviewTimeoutMs && reviewConfig.reviewTimeoutMs > 0) {
      this.clearReviewTimeout(nodeRun.id);
      this.reviewTimeouts.set(nodeRun.id, setTimeout(async () => {
        const latestRun = await this.runStore.get(run.id);
        if (!latestRun) return;
        const latestNodeRun = latestRun.nodeRuns.find((candidate) => candidate.id === nodeRun.id);
        if (!latestNodeRun || latestNodeRun.status !== "needs_review") return;
        const action = reviewConfig.onReviewTimeout ?? "fail";
        if (action === "approve") {
          await this.approveNode(latestRun.id, latestNodeRun.id, "Auto-approved on timeout.");
        } else if (action === "reject") {
          await this.rejectNode(latestRun.id, latestNodeRun.id, "Auto-rejected on timeout.");
        } else {
          await this.handleNodeFailure(latestRun, latestNodeRun, node, "review_timeout");
        }
      }, reviewConfig.reviewTimeoutMs));
    }
  }
  private async completeJoinNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    const aggregate = aggregateJoinOutput(run.definitionSnapshot, run, node.id, node.joinConfig);
    nodeRun.status = "completed";
    nodeRun.startedAt = nodeRun.startedAt ?? Date.now();
    nodeRun.finishedAt = Date.now();
    nodeRun.outputSummary = aggregate.outputSummary;
    nodeRun.outputJson = aggregate.outputJson;
    this.applyNodeStateUpdates(run, node, nodeRun);
    await this.recordEvent(run.id, "node_completed", `${node.title} joined upstream branches.`, nodeRun.id, {
      outputJson: nodeRun.outputJson,
      state: run.state,
    });
    await this.resolveOutgoingEdges(run, nodeRun);
  }

  private async completeIfElseNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    const selection = this.selectIfElseBranch(node, run);
    if ("error" in selection) {
      await this.handleNodeFailure(run, nodeRun, node, selection.error);
      return;
    }
    const branch = selection.branchId;
    nodeRun.status = "completed";
    nodeRun.startedAt = nodeRun.startedAt ?? Date.now();
    nodeRun.finishedAt = Date.now();
    nodeRun.outputJson = {
      result: selection.matched,
      branch,
      branchLabel: selection.label,
      branchKind: selection.kind,
      reason: selection.reason,
    };
    nodeRun.outputSummary = `${node.title} routed to ${branch}.`;
    this.applyNodeStateUpdates(run, node, nodeRun);
    await this.recordEvent(run.id, "node_completed", `${node.title} selected ${selection.label || branch}.`, nodeRun.id, {
      outputJson: nodeRun.outputJson,
      state: run.state,
    });
    await this.resolveOutgoingEdges(run, nodeRun);
  }

  private selectIfElseBranch(
    node: AgentWorkflowNode,
    run: AgentWorkflowRun,
  ): { branchId: string; label: string; kind: WorkflowIfElseBranch["kind"]; matched: boolean; reason: string } | { error: string } {
    const branches = node.ifElse?.branches?.filter((branch) => branch.id?.trim());
    if (branches?.length) {
      let elseBranch: WorkflowIfElseBranch | undefined;
      for (const branch of branches) {
        if (branch.kind === "else") {
          elseBranch = branch;
          continue;
        }
        const result = branch.celExpression?.trim()
          ? evaluateWorkflowCelCondition(branch.celExpression, run, node.id)
          : this.evaluateConditionGroup(branch.condition, run);
        if (result.reason.startsWith("cel_error:") || result.reason.startsWith("cel_non_boolean:")) {
          return {
            error: `${branch.label || branch.id} CEL condition failed: ${result.reason.replace(/^cel_(?:error|non_boolean):/, "")}`,
          };
        }
        if (result.activated) {
          return {
            branchId: branch.id,
            label: branch.label || branch.id,
            kind: branch.kind,
            matched: true,
            reason: result.reason,
          };
        }
      }
      const fallback = elseBranch ?? branches[branches.length - 1];
      return {
        branchId: fallback.id,
        label: fallback.label || fallback.id,
        kind: fallback.kind,
        matched: false,
        reason: elseBranch ? "else_fallback" : "no_branch_condition_matched",
      };
    }

    const result = this.evaluateNodeCondition(node.ifElse?.condition, run);
    return {
      branchId: result.activated ? "true" : "false",
      label: result.activated ? "IF" : "ELSE",
      kind: result.activated ? "if" : "else",
      matched: result.activated,
      reason: result.reason,
    };
  }

  private async startLoopNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    if (!node.subWorkflowDefinitionId) {
      await this.handleNodeFailure(run, nodeRun, node, "Loop node is missing subWorkflowDefinitionId.");
      return;
    }
    if (!node.loop?.maxIterations || node.loop.maxIterations < 1) {
      await this.handleNodeFailure(run, nodeRun, node, "Loop node maxIterations must be at least 1.");
      return;
    }
    await this.startLoopIteration(run, nodeRun, node);
  }

  private async startLoopIteration(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    if (!node.subWorkflowDefinitionId) {
      await this.handleNodeFailure(run, nodeRun, node, "Loop node is missing subWorkflowDefinitionId.");
      return;
    }
    const childDefinition = await this.definitionStore.get(run.repoPath, node.subWorkflowDefinitionId);
    if (!childDefinition) {
      await this.handleNodeFailure(run, nodeRun, node, `Loop child workflow definition not found: ${node.subWorkflowDefinitionId}.`);
      return;
    }

    const currentOutput = this.withLoopState(node, normalizeLoopOutput(nodeRun.outputJson), run);
    const iteration = currentOutput.count + 1;
    const input = this.buildMappedInput(node, run);
    input.loop = {
      iteration,
      previous: currentOutput.last,
      iterations: currentOutput.iterations,
      state: currentOutput.state,
    };

    nodeRun.status = "running";
    nodeRun.startedAt = nodeRun.startedAt ?? Date.now();
    nodeRun.finishedAt = undefined;
    nodeRun.outputJson = currentOutput;
    nodeRun.outputSummary = `Loop ${node.title} running iteration ${iteration}.`;
    run.status = "running";
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "node_started", `${node.title} started loop iteration ${iteration}.`, nodeRun.id, {
      childDefinitionId: childDefinition.id,
      iteration,
    });

    const childRun = await this.startRun(run.repoPath, childDefinition, {
      sourceTaskId: run.sourceTaskId,
      input,
      trigger: { type: "sub_workflow" },
      parentRunId: run.id,
      parentNodeRunId: nodeRun.id,
    });

    const latestRun = await this.runStore.get(run.id);
    const latestNodeRun = latestRun?.nodeRuns.find((candidate) => candidate.id === nodeRun.id);
    if (latestRun && latestNodeRun) {
      latestNodeRun.childRunId = childRun.id;
      latestNodeRun.childRunIds = [...new Set([...(latestNodeRun.childRunIds ?? []), childRun.id])];
      Object.assign(run, latestRun);
      if (latestNodeRun.status === "running") {
        await this.persistAndEmit(latestRun);
      }
    } else {
      nodeRun.childRunId = childRun.id;
      nodeRun.childRunIds = [...new Set([...(nodeRun.childRunIds ?? []), childRun.id])];
      await this.persistAndEmit(run);
    }

    await this.recordEvent(run.id, "child_run_linked", `${node.title} linked loop iteration ${iteration}.`, nodeRun.id, {
      childRunId: childRun.id,
      childDefinitionId: childDefinition.id,
      iteration,
    });
  }

  private async startSubWorkflowNode(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
  ): Promise<void> {
    if (!node.subWorkflowDefinitionId) {
      await this.handleNodeFailure(run, nodeRun, node, "Sub-workflow node is missing subWorkflowDefinitionId.");
      return;
    }
    const childDefinition = await this.definitionStore.get(run.repoPath, node.subWorkflowDefinitionId);
    if (!childDefinition) {
      await this.handleNodeFailure(run, nodeRun, node, `Sub-workflow definition not found: ${node.subWorkflowDefinitionId}.`);
      return;
    }

    const input = this.buildMappedInput(node, run);

    nodeRun.status = "running";
    nodeRun.startedAt = Date.now();
    nodeRun.finishedAt = undefined;
    run.status = "running";
    await this.persistAndEmit(run);
    await this.recordEvent(run.id, "node_started", `${node.title} started child workflow.`, nodeRun.id, {
      childDefinitionId: childDefinition.id,
    });

    const childRun = await this.startRun(run.repoPath, childDefinition, {
      sourceTaskId: run.sourceTaskId,
      input,
      trigger: { type: "sub_workflow" },
      parentRunId: run.id,
      parentNodeRunId: nodeRun.id,
    });

    const latestRun = await this.runStore.get(run.id);
    const latestNodeRun = latestRun?.nodeRuns.find((candidate) => candidate.id === nodeRun.id);
    if (latestRun && latestNodeRun) {
      latestNodeRun.childRunId = latestNodeRun.childRunId ?? childRun.id;
      Object.assign(run, latestRun);
      if (latestNodeRun.status === "running") {
        await this.persistAndEmit(latestRun);
      }
    } else {
      nodeRun.childRunId = childRun.id;
      await this.persistAndEmit(run);
    }
    await this.recordEvent(run.id, "child_run_linked", `${node.title} started child workflow.`, nodeRun.id, {
      childRunId: childRun.id,
      childDefinitionId: childDefinition.id,
    });
  }

  private async handleTaskStatus(taskId: string, status: TaskStatus): Promise<void> {
    const run = await this.findRunByTaskId(taskId);
    if (!run) return;

    const nodeRun = run.nodeRuns.find((candidate) => candidate.taskId === taskId);
    if (!nodeRun) return;
    if (TERMINAL_NODE_STATUSES.has(nodeRun.status)) return;

    const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
    const task = this.orchestrator.getTask(taskId);
    const result = await this.syncTaskNodeStatus(run, nodeRun, node, task, status, "event");
    if (result === "changed") {
      const readinessChanged = await this.applyReadiness(run);
      this.updateRunStatus(run);
      await this.persistAndEmit(run);
      if (readinessChanged) await this.schedule(run.id);
    }
  }

  private async handleNodeFailure(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
    error: string,
  ): Promise<void> {
    this.clearNodeTimeout(nodeRun.id);
    this.clearReviewTimeout(nodeRun.id);
    const maxAttempts = node.retryPolicy?.maxAttempts ?? 1;
    if (nodeRun.attempt < maxAttempts) {
      nodeRun.attempt += 1;
      nodeRun.status = "ready";
      nodeRun.taskId = undefined;
      nodeRun.error = undefined;
      nodeRun.startedAt = undefined;
      nodeRun.finishedAt = undefined;
      await this.persistAndEmit(run);
      await this.recordEvent(run.id, "node_retried", `${node.title} retrying.`, nodeRun.id, {
        attempt: nodeRun.attempt,
      });
      await this.schedule(run.id);
      return;
    }

    nodeRun.status = "failed";
    nodeRun.error = error;
    nodeRun.finishedAt = Date.now();
    await this.recordEvent(run.id, "node_failed", `${node.title} failed.`, nodeRun.id, { error });
    await this.resolveOutgoingEdges(run, nodeRun);
    await this.applyReadiness(run);
    this.updateRunStatus(run);
    await this.persistAndEmit(run);
    await this.schedule(run.id);
  }

  private async reconcileRun(run: AgentWorkflowRun): Promise<void> {
    let changed = false;
    let handled = false;
    for (const nodeRun of run.nodeRuns) {
      if (nodeRun.taskId) {
        const task = this.orchestrator.getTask(nodeRun.taskId);
        if (!task && (nodeRun.status === "running" || nodeRun.status === "queued")) {
          const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
          await this.handleNodeFailure(run, nodeRun, node, "missing_task_after_restart");
          handled = true;
          continue;
        }
        if (task && ((task.workflowRunId !== run.id) || (task.workflowNodeRunId !== nodeRun.id))) {
          task.workflowRunId = run.id;
          task.workflowNodeRunId = nodeRun.id;
          changed = true;
        }
        if (task) {
          const node = this.requireNode(run.definitionSnapshot, nodeRun.nodeId);
          const result = await this.syncTaskNodeStatus(run, nodeRun, node, task, task.status, "reconcile");
          if (result === "handled") handled = true;
          if (result === "changed") changed = true;
        }
      }
      if (nodeRun.childRunId && nodeRun.status === "running") {
        const child = await this.runStore.get(nodeRun.childRunId);
        if (child && !ACTIVE_RUN_STATUSES.has(child.status)) {
          await this.handleChildRunTerminal(child);
          return;
        }
      }
    }
    changed = (await this.applyReadiness(run)) || changed;
    const previousStatus = run.status;
    this.updateRunStatus(run);
    if (previousStatus !== run.status) changed = true;
    if (changed) await this.orchestrator.persistTasks();
    if (changed && !handled) await this.persistAndEmit(run);
  }

  private async reconcileActiveRuns(repoPath?: string): Promise<void> {
    const activeRuns = (await this.runStore.list(repoPath)).filter((run) => ACTIVE_RUN_STATUSES.has(run.status));
    for (const run of activeRuns) {
      await this.reconcileRun(run);
    }
  }

  private async syncTaskNodeStatus(
    run: AgentWorkflowRun,
    nodeRun: AgentWorkflowNodeRun,
    node: AgentWorkflowNode,
    task: Task | undefined,
    status: TaskStatus,
    source: "event" | "reconcile",
  ): Promise<TaskSyncResult> {
    if (TERMINAL_NODE_STATUSES.has(nodeRun.status)) return "unchanged";

    if (status === "reviewing" || status === "planning") {
      const outputSummary = this.extractTaskSummary(task, node);
      nodeRun.outputSummary = outputSummary;
      const structuredOk = this.applyStructuredOutput(node, nodeRun, outputSummary);
      if (!structuredOk) {
        await this.handleNodeFailure(run, nodeRun, node, nodeRun.outputJsonError || "invalid_structured_output");
        return "handled";
      }

      if (this.shouldWaitForAgentReview(node, status)) {
        const statusChanged = nodeRun.status !== "needs_review";
        nodeRun.status = "needs_review";
        nodeRun.finishedAt = nodeRun.finishedAt ?? Date.now();
        if (statusChanged) {
          await this.recordEvent(run.id, "node_needs_review", `${node.title} is waiting for approval.`, nodeRun.id, {
            taskId: nodeRun.taskId,
            taskStatus: status,
            source,
            outputJsonError: nodeRun.outputJsonError,
          });
        }
        return statusChanged ? "changed" : "unchanged";
      }

      const statusChanged = nodeRun.status !== "completed";
      nodeRun.status = "completed";
      nodeRun.finishedAt = nodeRun.finishedAt ?? Date.now();
      this.applyNodeStateUpdates(run, node, nodeRun);
      if (statusChanged) {
        await this.recordEvent(run.id, "node_completed", `${node.title} completed.`, nodeRun.id, {
          taskId: nodeRun.taskId,
          taskStatus: status,
          source,
          outputSummary: nodeRun.outputSummary,
          outputJson: nodeRun.outputJson,
          state: run.state,
        });
      }
      return statusChanged ? "changed" : "unchanged";
    }

    if (ORCHESTRATOR_FAILURE_STATUSES.has(status)) {
      this.clearNodeTimeout(nodeRun.id);
      await this.handleNodeFailure(run, nodeRun, node, `Linked task ${nodeRun.taskId ?? task?.id ?? "(unknown)"} ended with status ${status}.`);
      return "handled";
    }

    if (nodeRun.status === "needs_review") return "unchanged";

    if (status === "running" || status === "iterating" || status === "queued") {
      const nextStatus: NodeRunStatus = status === "queued" ? "queued" : "running";
      if (nodeRun.status !== nextStatus) {
        nodeRun.status = nextStatus;
        nodeRun.finishedAt = undefined;
        return "changed";
      }
    }

    return "unchanged";
  }

  private shouldWaitForAgentReview(node: AgentWorkflowNode, status: TaskStatus): boolean {
    return status === "planning" || node.requireReview === true;
  }

  private async applyReadiness(run: AgentWorkflowRun): Promise<boolean> {
    let changed = false;
    let progressed = true;
    while (progressed) {
      progressed = false;

      for (const nodeRun of run.nodeRuns) {
        if (TERMINAL_NODE_STATUSES.has(nodeRun.status)) {
          const didResolve = await this.resolveOutgoingEdges(run, nodeRun);
          if (didResolve) {
            changed = true;
            progressed = true;
          }
        }
      }

      for (const nodeRun of run.nodeRuns) {
        if (nodeRun.status !== "blocked") continue;
        const incomingEdges = getIncomingEdges(run.definitionSnapshot, nodeRun.nodeId);
        if (incomingEdges.length === 0) {
          nodeRun.status = "ready";
          changed = true;
          progressed = true;
          await this.recordEvent(run.id, "node_ready", "Node is ready.", nodeRun.id);
          continue;
        }
        const incomingRuns = incomingEdges.map((edge) => this.requireEdgeRun(run, edge.id));
        if (!incomingRuns.every((edgeRun) => edgeRun.status !== "pending")) continue;

        if (incomingRuns.some((edgeRun) => edgeRun.status === "activated")) {
          nodeRun.status = "ready";
          changed = true;
          progressed = true;
          await this.recordEvent(run.id, "node_ready", "Node is ready.", nodeRun.id);
        } else {
          nodeRun.status = "skipped";
          nodeRun.skipReason = "condition_not_met";
          nodeRun.finishedAt = Date.now();
          changed = true;
          progressed = true;
          await this.recordEvent(run.id, "node_skipped", "Node skipped because no incoming edge activated.", nodeRun.id, {
            reason: nodeRun.skipReason,
          });
          await this.resolveOutgoingEdges(run, nodeRun);
        }
      }
    }
    return changed;
  }

  private async resolveOutgoingEdges(run: AgentWorkflowRun, sourceNodeRun: AgentWorkflowNodeRun): Promise<boolean> {
    if (!TERMINAL_NODE_STATUSES.has(sourceNodeRun.status)) return false;
    let changed = false;
    for (const edge of getOutgoingEdges(run.definitionSnapshot, sourceNodeRun.nodeId)) {
      const edgeRun = this.requireEdgeRun(run, edge.id);
      if (edgeRun.status !== "pending") continue;
      const result = evaluateWorkflowEdgeCondition(edge, sourceNodeRun);
      edgeRun.status = result.activated ? "activated" : "not_activated";
      edgeRun.evaluatedAt = Date.now();
      edgeRun.reason = result.reason;
      changed = true;
      await this.recordEvent(
        run.id,
        result.activated ? "edge_activated" : "edge_not_activated",
        `Edge ${edge.id} ${result.activated ? "activated" : "not activated"}.`,
        sourceNodeRun.id,
        { edgeId: edge.id, reason: result.reason },
      );
    }
    return changed;
  }

  private activeExecutionCount(run: AgentWorkflowRun): number {
    return run.nodeRuns.filter((nodeRun) => {
      if (nodeRun.status !== "running" && nodeRun.status !== "queued") return false;
      const node = run.definitionSnapshot.nodes.find((candidate) => candidate.id === nodeRun.nodeId);
      return node?.type === "agent_task" || node?.type === "sub_workflow" || node?.type === "loop";
    }).length;
  }

  private updateRunStatus(run: AgentWorkflowRun): void {
    if (run.status === "cancelled") return;
    if (run.userPaused) return;
    const previousStatus = run.status;

    const unfinished = run.nodeRuns.some((nodeRun) => !TERMINAL_NODE_STATUSES.has(nodeRun.status));
    const runnable = run.nodeRuns.some((nodeRun) => ACTIVE_NODE_STATUSES.has(nodeRun.status));
    const needsReview = run.nodeRuns.some((nodeRun) => nodeRun.status === "needs_review");
    const unhandledFailure = this.hasUnhandledFailure(run);

    if (!unfinished) {
      run.status = unhandledFailure ? "failed" : "completed";
      run.finishedAt = run.finishedAt ?? Date.now();
    } else if (unhandledFailure && !runnable && !needsReview) {
      run.status = "failed";
      run.finishedAt = run.finishedAt ?? Date.now();
    } else if (runnable) {
      run.status = "running";
      run.finishedAt = undefined;
    } else {
      run.status = "paused";
      run.finishedAt = undefined;
    }

    if (previousStatus !== run.status && (run.status === "completed" || run.status === "failed")) {
      const type: AgentWorkflowEventType = run.status === "completed" ? "run_completed" : "run_failed";
      this.recordEvent(run.id, type, `Workflow run ${run.status}.`).catch(() => {});
    }
  }

  private hasUnhandledFailure(run: AgentWorkflowRun): boolean {
    return run.nodeRuns.some((nodeRun) => {
      if (nodeRun.status !== "failed" && nodeRun.status !== "cancelled") return false;
      const outgoing = getOutgoingEdges(run.definitionSnapshot, nodeRun.nodeId);
      if (outgoing.length === 0) return true;
      return !outgoing.some((edge) => this.requireEdgeRun(run, edge.id).status === "activated");
    });
  }

  private buildAgentPrompt(run: AgentWorkflowRun, node: AgentWorkflowNode, nodeRun: AgentWorkflowNodeRun): string {
    const upstreamSummaries = getUpstreamNodeIds(run.definitionSnapshot, node.id)
      .map((upstreamNodeId) => {
        const upstreamNode = this.requireNode(run.definitionSnapshot, upstreamNodeId);
        const upstreamRun = run.nodeRuns.find((candidate) => candidate.nodeId === upstreamNodeId);
        if (!upstreamRun) return null;
        const taskLink = upstreamRun.taskId ? ` task=${upstreamRun.taskId}` : "";
        const structured = upstreamRun.outputJson !== undefined ? `\n  outputJson=${JSON.stringify(upstreamRun.outputJson)}` : "";
        return `- ${upstreamNode.title}${taskLink}: ${upstreamRun.outputSummary || "Completed with no summary."}${structured}`;
      })
      .filter(Boolean)
      .join("\n");

    const instructions = resolveTemplate(node.instructions, { run });
    const outputSchemaBlock = node.outputSchema
      ? `Structured output required${node.outputSchemaName ? ` (${node.outputSchemaName})` : ""}:\nReturn a final fenced json block matching this schema:\n${JSON.stringify(node.outputSchema, null, 2)}`
      : "";
    const completionInstruction = node.requireReview || node.planFirst
      ? "When complete, provide a concise summary of what changed and any risks or follow-up needed. The workflow will wait for user approval before downstream nodes run."
      : "When complete, provide a concise summary of what changed and any risks or follow-up needed. Downstream workflow nodes may run automatically after this step completes.";

    return [
      `Workflow: ${run.definitionSnapshot.name}`,
      `Definition revision: ${run.definitionSnapshot.revisionId}`,
      `Node: ${node.title}`,
      `Run ID: ${run.id}`,
      `Node Run ID: ${nodeRun.id}`,
      run.input ? `Run input:\n${JSON.stringify(run.input, null, 2)}` : "",
      upstreamSummaries ? `Activated upstream outputs:\n${upstreamSummaries}` : "",
      "Instructions:",
      instructions,
      outputSchemaBlock,
      completionInstruction,
    ].filter(Boolean).join("\n\n");
  }

  private extractTaskSummary(task: Task | undefined, node: AgentWorkflowNode): string {
    const events = task?.events ?? [];
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const event = events[i] as { type?: string; content?: unknown; message?: unknown; text?: unknown };
      const content = typeof event.content === "string" ? event.content
        : typeof event.message === "string" ? event.message
          : typeof event.text === "string" ? event.text
            : "";
      if (content.trim()) return content.trim().slice(0, 8000);
    }
    return `${node.title} is ready for review.`;
  }

  private applyStructuredOutput(node: AgentWorkflowNode, nodeRun: AgentWorkflowNodeRun, outputSummary: string): boolean {
    nodeRun.outputSummary = outputSummary;
    if (!node.outputSchema) return true;
    const result = parseStructuredOutput(outputSummary, node.outputSchema);
    nodeRun.outputJson = result.outputJson;
    nodeRun.outputJsonError = result.outputJsonError;
    return !(node.requireStructuredOutput && result.outputJsonError);
  }

  private buildMappedInput(node: AgentWorkflowNode, run: AgentWorkflowRun): Record<string, unknown> {
    const input: Record<string, unknown> = {};
    for (const [key, ref] of Object.entries(node.inputMapping ?? {})) {
      input[key] = resolveValueRef(ref, run);
    }
    return input;
  }

  private applyNodeStateUpdates(
    run: AgentWorkflowRun,
    node: AgentWorkflowNode,
    _nodeRun: AgentWorkflowNodeRun,
  ): void {
    const updates = node.stateUpdates;
    if (!updates || Object.keys(updates).length === 0) return;
    const nextState = deepClone(run.state ?? {});
    for (const [path, ref] of Object.entries(updates)) {
      setStatePath(nextState, path, resolveValueRef(ref, run));
    }
    run.state = nextState;
  }

  private withLoopState(
    node: AgentWorkflowNode,
    output: LoopOutput,
    run: AgentWorkflowRun,
  ): LoopOutput {
    if (output.count > 0 || output.state || !node.loop?.initialState) return output;
    return {
      ...output,
      state: this.resolveValueRefMap(node.loop.initialState, run),
    };
  }

  private nextLoopState(
    node: AgentWorkflowNode,
    output: LoopOutput,
    childRun: AgentWorkflowRun,
  ): Record<string, unknown> {
    const previous = output.state ?? {};
    if (!node.loop?.stateMapping || Object.keys(node.loop.stateMapping).length === 0) {
      return previous;
    }
    return {
      ...previous,
      ...this.resolveValueRefMap(node.loop.stateMapping, childRun),
    };
  }

  private resolveValueRefMap(
    mapping: Record<string, WorkflowValueRef>,
    run: AgentWorkflowRun,
  ): Record<string, unknown> {
    const result: Record<string, unknown> = {};
    for (const [key, ref] of Object.entries(mapping)) {
      result[key] = resolveValueRef(ref, run);
    }
    return result;
  }

  private async handleChildRunTerminal(childRun: AgentWorkflowRun): Promise<void> {
    if (!childRun.parentRunId || !childRun.parentNodeRunId) return;
    const parentRun = await this.runStore.get(childRun.parentRunId);
    if (!parentRun || !ACTIVE_RUN_STATUSES.has(parentRun.status)) return;
    const parentNodeRun = parentRun.nodeRuns.find((candidate) => candidate.id === childRun.parentNodeRunId);
    if (!parentNodeRun || parentNodeRun.status !== "running") return;
    const parentNode = this.requireNode(parentRun.definitionSnapshot, parentNodeRun.nodeId);

    if (parentNode.type === "loop") {
      await this.handleLoopChildTerminal(parentRun, parentNodeRun, parentNode, childRun);
      return;
    }

    parentNodeRun.childRunId = childRun.id;
    parentNodeRun.finishedAt = Date.now();
    if (childRun.status === "completed") {
      parentNodeRun.outputJson = this.mapChildOutput(parentNode, childRun);
      parentNodeRun.outputSummary = this.summarizeChildRun(childRun);
      parentNodeRun.outputJsonError = undefined;
      if (parentNode.outputSchema) {
        const errors = validateJsonSchemaSubset(parentNodeRun.outputJson, parentNode.outputSchema);
        if (errors.length > 0) {
          parentNodeRun.outputJsonError = errors.join("; ");
          if (parentNode.requireStructuredOutput) {
            await this.handleNodeFailure(parentRun, parentNodeRun, parentNode, parentNodeRun.outputJsonError);
            return;
          }
        }
      }
      parentNodeRun.status = "completed";
      this.applyNodeStateUpdates(parentRun, parentNode, parentNodeRun);
      await this.recordEvent(parentRun.id, "node_completed", `${parentNode.title} completed child workflow.`, parentNodeRun.id, {
        childRunId: childRun.id,
        outputJson: parentNodeRun.outputJson,
        outputJsonError: parentNodeRun.outputJsonError,
        state: parentRun.state,
      });
    } else {
      parentNodeRun.status = childRun.status === "cancelled" ? "cancelled" : "failed";
      parentNodeRun.error = `child_workflow_${childRun.status}`;
      await this.recordEvent(parentRun.id, "node_failed", `${parentNode.title} child workflow ended with ${childRun.status}.`, parentNodeRun.id, {
        childRunId: childRun.id,
      });
    }

    await this.resolveOutgoingEdges(parentRun, parentNodeRun);
    await this.applyReadiness(parentRun);
    this.updateRunStatus(parentRun);
    await this.persistAndEmit(parentRun);
    await this.schedule(parentRun.id);
  }

  private async handleLoopChildTerminal(
    parentRun: AgentWorkflowRun,
    parentNodeRun: AgentWorkflowNodeRun,
    parentNode: AgentWorkflowNode,
    childRun: AgentWorkflowRun,
  ): Promise<void> {
    parentNodeRun.childRunId = childRun.id;
    parentNodeRun.childRunIds = [...new Set([...(parentNodeRun.childRunIds ?? []), childRun.id])];
    parentNodeRun.finishedAt = Date.now();

    if (childRun.status !== "completed") {
      parentNodeRun.status = childRun.status === "cancelled" ? "cancelled" : "failed";
      parentNodeRun.error = `loop_child_workflow_${childRun.status}`;
      await this.recordEvent(parentRun.id, "node_failed", `${parentNode.title} loop child ended with ${childRun.status}.`, parentNodeRun.id, {
        childRunId: childRun.id,
      });
      await this.resolveOutgoingEdges(parentRun, parentNodeRun);
      await this.applyReadiness(parentRun);
      this.updateRunStatus(parentRun);
      await this.persistAndEmit(parentRun);
      await this.schedule(parentRun.id);
      return;
    }

    const currentOutput = normalizeLoopOutput(parentNodeRun.outputJson);
    const childOutput = this.defaultChildOutput(childRun);
    const childSummary = this.summarizeChildRun(childRun);
    const iteration = {
      index: currentOutput.count + 1,
      childRunId: childRun.id,
      status: childRun.status,
      summary: childSummary,
      output: childOutput,
    };
    const nextState = this.nextLoopState(parentNode, currentOutput, childRun);
    const nextOutput = {
      childRunIds: [...new Set([...currentOutput.childRunIds, childRun.id])],
      count: currentOutput.count + 1,
      iterations: [...currentOutput.iterations, iteration],
      last: iteration,
      state: nextState,
    };
    parentNodeRun.outputJson = nextOutput;
    parentNodeRun.outputSummary = `Loop ${parentNode.title} completed ${nextOutput.count} iteration${nextOutput.count === 1 ? "" : "s"}.`;
    parentNodeRun.outputJsonError = undefined;

    const maxIterations = Math.max(1, Math.min(50, parentNode.loop?.maxIterations ?? 1));
    const shouldTerminate = parentNode.loop?.terminateCondition
      ? this.evaluateConditionGroup(parentNode.loop.terminateCondition, parentRun).activated
      : false;
    const continueConditionOk = parentNode.loop?.continueCondition
      ? this.evaluateNodeCondition(parentNode.loop.continueCondition, parentRun).activated
      : true;
    const shouldContinue = nextOutput.count < maxIterations
      && !shouldTerminate
      && continueConditionOk;

    if (shouldContinue) {
      parentNodeRun.finishedAt = undefined;
      await this.persistAndEmit(parentRun);
      await this.startLoopIteration(parentRun, parentNodeRun, parentNode);
      return;
    }

    if (parentNode.outputMapping && Object.keys(parentNode.outputMapping).length > 0) {
      const mapped: Record<string, unknown> = {};
      for (const [key, ref] of Object.entries(parentNode.outputMapping)) {
        mapped[key] = resolveValueRef(ref, parentRun);
      }
      parentNodeRun.outputJson = mapped;
    }

    if (parentNode.outputSchema) {
      const errors = validateJsonSchemaSubset(parentNodeRun.outputJson, parentNode.outputSchema);
      if (errors.length > 0) {
        parentNodeRun.outputJsonError = errors.join("; ");
        if (parentNode.requireStructuredOutput) {
          await this.handleNodeFailure(parentRun, parentNodeRun, parentNode, parentNodeRun.outputJsonError);
          return;
        }
      }
    }

    parentNodeRun.status = "completed";
    this.applyNodeStateUpdates(parentRun, parentNode, parentNodeRun);
    await this.recordEvent(parentRun.id, "node_completed", `${parentNode.title} loop completed.`, parentNodeRun.id, {
      childRunId: childRun.id,
      outputJson: parentNodeRun.outputJson,
      outputJsonError: parentNodeRun.outputJsonError,
      state: parentRun.state,
    });
    await this.resolveOutgoingEdges(parentRun, parentNodeRun);
    await this.applyReadiness(parentRun);
    this.updateRunStatus(parentRun);
    await this.persistAndEmit(parentRun);
    await this.schedule(parentRun.id);
  }

  private mapChildOutput(parentNode: AgentWorkflowNode, childRun: AgentWorkflowRun): Record<string, unknown> {
    if (parentNode.outputMapping && Object.keys(parentNode.outputMapping).length > 0) {
      const mapped: Record<string, unknown> = {};
      for (const [key, ref] of Object.entries(parentNode.outputMapping)) {
        mapped[key] = resolveValueRef(ref, childRun);
      }
      return mapped;
    }
    return this.defaultChildOutput(childRun);
  }

  private defaultChildOutput(childRun: AgentWorkflowRun): Record<string, unknown> {
    return {
      childRunId: childRun.id,
      status: childRun.status,
      summariesByNodeId: Object.fromEntries(childRun.nodeRuns.map((nodeRun) => [nodeRun.nodeId, nodeRun.outputSummary ?? ""])),
      outputsByNodeId: Object.fromEntries(childRun.nodeRuns.filter((nodeRun) => nodeRun.outputJson !== undefined).map((nodeRun) => [nodeRun.nodeId, nodeRun.outputJson])),
    };
  }

  private summarizeChildRun(childRun: AgentWorkflowRun): string {
    const summaries = childRun.nodeRuns
      .map((nodeRun) => {
        const node = childRun.definitionSnapshot.nodes.find((candidate) => candidate.id === nodeRun.nodeId);
        return nodeRun.outputSummary ? `${node?.title ?? nodeRun.nodeId}: ${nodeRun.outputSummary}` : null;
      })
      .filter(Boolean);
    return [
      `Child workflow ${childRun.definitionSnapshot.name} completed.`,
      summaries.length > 0 ? summaries.join("\n") : "",
    ].filter(Boolean).join("\n").slice(0, 8000);
  }

  private evaluateConditionGroup(
    group: WorkflowConditionGroup | undefined,
    run: AgentWorkflowRun,
  ): { activated: boolean; reason: string } {
    if (!group || !Array.isArray(group.conditions) || group.conditions.length === 0) {
      return { activated: false, reason: "condition_group_missing" };
    }
    const results = group.conditions.map((condition) => this.evaluateNodeCondition(condition, run));
    const activated = group.logic === "or"
      ? results.some((result) => result.activated)
      : results.every((result) => result.activated);
    return {
      activated,
      reason: `${group.logic}_${activated ? "matched" : "not_matched"}:${results.map((result) => result.reason).join(",")}`,
    };
  }

  private evaluateNodeCondition(
    condition: WorkflowNodeCondition | undefined,
    run: AgentWorkflowRun,
  ): { activated: boolean; reason: string } {
    if (!condition) return { activated: false, reason: "condition_missing" };
    const left = resolveValueRef(condition.left, run);
    const right = condition.right ? resolveValueRef(condition.right, run) : condition.value;
    let activated = false;

    switch (condition.operator) {
      case "exists":
        activated = left !== undefined;
        break;
      case "not_exists":
        activated = left === undefined;
        break;
      case "equals":
        activated = deepEqual(left, right);
        break;
      case "not_equals":
        activated = !deepEqual(left, right);
        break;
      case "contains":
        activated = containsValue(left, right);
        break;
      case "not_contains":
        activated = !containsValue(left, right);
        break;
      case "starts_with":
        activated = typeof left === "string" && typeof right === "string" && left.startsWith(right);
        break;
      case "ends_with":
        activated = typeof left === "string" && typeof right === "string" && left.endsWith(right);
        break;
      case "is_empty":
        activated = isEmptyValue(left);
        break;
      case "is_not_empty":
        activated = !isEmptyValue(left);
        break;
      case "greater_than":
        activated = typeof left === "number" && typeof right === "number" && left > right;
        break;
      case "less_than":
        activated = typeof left === "number" && typeof right === "number" && left < right;
        break;
      case "in":
        activated = Array.isArray(right) && right.some((candidate) => deepEqual(candidate, left));
        break;
    }

    return {
      activated,
      reason: activated ? `condition_${condition.operator}_matched` : `condition_${condition.operator}_not_matched`,
    };
  }

  private resetNodeAndDownstream(run: AgentWorkflowRun, nodeId: string, stopNodeId?: string): void {
    const resetNodeIds = new Set<string>([nodeId]);
    const queue = [nodeId];
    while (queue.length > 0) {
      const current = queue.shift();
      if (!current) continue;
      if (stopNodeId && current === stopNodeId) {
        resetNodeIds.add(current);
        continue;
      }
      for (const downstream of getDownstreamNodeIds(run.definitionSnapshot, current)) {
        if (resetNodeIds.has(downstream)) continue;
        resetNodeIds.add(downstream);
        queue.push(downstream);
      }
    }

    for (const nodeRun of run.nodeRuns) {
      if (!resetNodeIds.has(nodeRun.nodeId)) continue;
      const nextAttempt = nodeRun.nodeId === nodeId ? nodeRun.attempt : 1;
      nodeRun.status = "blocked";
      nodeRun.taskId = undefined;
      nodeRun.childRunId = undefined;
      nodeRun.childRunIds = undefined;
      nodeRun.outputSummary = undefined;
      nodeRun.outputJson = undefined;
      nodeRun.outputJsonError = undefined;
      nodeRun.error = undefined;
      nodeRun.skipReason = undefined;
      nodeRun.startedAt = undefined;
      nodeRun.finishedAt = undefined;
      nodeRun.attempt = nextAttempt;
    }

    for (const edgeRun of run.edgeRuns) {
      const edge = run.definitionSnapshot.edges.find((candidate) => candidate.id === edgeRun.edgeId);
      if (!edge) continue;
      if (resetNodeIds.has(edge.sourceNodeId) || resetNodeIds.has(edge.targetNodeId)) {
        edgeRun.status = "pending";
        edgeRun.evaluatedAt = undefined;
        edgeRun.reason = undefined;
      }
    }

    const targetRun = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === nodeId);
    if (targetRun && getIncomingEdges(run.definitionSnapshot, nodeId).length === 0) {
      targetRun.status = "ready";
    }
  }

  private async findRunByTaskId(taskId: string): Promise<AgentWorkflowRun | null> {
    const task = this.orchestrator.getTask(taskId);
    if (task?.workflowRunId) {
      const run = await this.runStore.get(task.workflowRunId);
      if (run) return run;
    }

    const activeRuns = (await this.runStore.list()).filter((run) => ACTIVE_RUN_STATUSES.has(run.status));
    return activeRuns.find((run) => run.nodeRuns.some((nodeRun) => nodeRun.taskId === taskId)) ?? null;
  }

  private async requireRun(runId: string): Promise<AgentWorkflowRun> {
    const run = await this.runStore.get(runId);
    if (!run) throw new Error(`Workflow run not found: ${runId}.`);
    return run;
  }

  private requireNodeRun(run: AgentWorkflowRun, nodeRunId: string): AgentWorkflowNodeRun {
    const nodeRun = run.nodeRuns.find((candidate) => candidate.id === nodeRunId);
    if (!nodeRun) throw new Error(`Workflow node run not found: ${nodeRunId}.`);
    return nodeRun;
  }

  private requireNode(definition: AgentWorkflowDefinition, nodeId: string): AgentWorkflowNode {
    const node = definition.nodes.find((candidate) => candidate.id === nodeId);
    if (!node) throw new Error(`Workflow node not found: ${nodeId}.`);
    return node;
  }

  private requireEdgeRun(run: AgentWorkflowRun, edgeId: string): AgentWorkflowEdgeRun {
    const edgeRun = run.edgeRuns.find((candidate) => candidate.edgeId === edgeId);
    if (!edgeRun) throw new Error(`Workflow edge run not found: ${edgeId}.`);
    return edgeRun;
  }

  private async persistAndEmit(run: AgentWorkflowRun): Promise<void> {
    await this.runStore.save(run);
    this.emit("run-updated", run);
    await this.syncSourceTaskStatus(run);
    if (!ACTIVE_RUN_STATUSES.has(run.status)) {
      await this.handleChildRunTerminal(run);
    }
  }

  private async syncSourceTaskStatus(run: AgentWorkflowRun): Promise<void> {
    if (!run.sourceTaskId || run.parentRunId) return;
    const sourceTask = this.orchestrator.getTask(run.sourceTaskId);
    if (!sourceTask) return;
    await this.orchestrator.attachWorkflowRunToTask(run.sourceTaskId, run.id);
    if (sourceTask.agentType !== "workflow") return;

    const status: TaskStatus = run.status === "completed"
      ? "reviewing"
      : run.status === "failed"
        ? "failed"
        : run.status === "cancelled"
          ? "stopped"
          : "running";
    await this.orchestrator.setWorkflowTaskStatus(run.sourceTaskId, status);
  }

  private async recordEvent(
    runId: string,
    type: AgentWorkflowEventType,
    message: string,
    nodeRunId?: string,
    payload?: Record<string, unknown>,
  ): Promise<void> {
    const event: AgentWorkflowEvent = {
      id: randomUUID(),
      runId,
      nodeRunId,
      type,
      message,
      timestamp: Date.now(),
      payload,
    };
    await this.runStore.appendEvent(event);
    this.emit("node-event", event);
  }

  private emitWorkflowError(message: string): void {
    this.emit("node-event", {
      id: randomUUID(),
      runId: "unknown",
      type: "run_updated",
      message,
      timestamp: Date.now(),
    });
  }

  private clearNodeTimeout(nodeRunId: string): void {
    const timer = this.nodeTimeouts.get(nodeRunId);
    if (timer) {
      clearTimeout(timer);
      this.nodeTimeouts.delete(nodeRunId);
    }
  }

  private clearReviewTimeout(nodeRunId: string): void {
    const timer = this.reviewTimeouts.get(nodeRunId);
    if (timer) {
      clearTimeout(timer);
      this.reviewTimeouts.delete(nodeRunId);
    }
  }
}

function deepClone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function setStatePath(state: Record<string, unknown>, path: string, value: unknown): void {
  const parts = path.split(".").filter(Boolean);
  let current = state;
  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index];
    if (part === "__proto__" || part === "prototype" || part === "constructor") return;
    if (index === parts.length - 1) {
      current[part] = value === undefined ? null : deepClone(value);
      return;
    }
    const child = current[part];
    if (!child || typeof child !== "object" || Array.isArray(child)) {
      current[part] = {};
    }
    current = current[part] as Record<string, unknown>;
  }
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

interface LoopOutput {
  childRunIds: string[];
  count: number;
  iterations: Array<Record<string, unknown>>;
  last?: Record<string, unknown>;
  state?: Record<string, unknown>;
}

function normalizeLoopOutput(value: unknown): LoopOutput {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return { childRunIds: [], count: 0, iterations: [] };
  }
  const record = value as Record<string, unknown>;
  return {
    childRunIds: Array.isArray(record.childRunIds) ? record.childRunIds.filter((item): item is string => typeof item === "string") : [],
    count: Number.isInteger(record.count) && (record.count as number) >= 0 ? record.count as number : 0,
    iterations: Array.isArray(record.iterations)
      ? record.iterations.filter((item): item is Record<string, unknown> => Boolean(item) && typeof item === "object" && !Array.isArray(item))
      : [],
    last: record.last && typeof record.last === "object" && !Array.isArray(record.last)
      ? record.last as Record<string, unknown>
      : undefined,
    state: record.state && typeof record.state === "object" && !Array.isArray(record.state)
      ? record.state as Record<string, unknown>
      : undefined,
  };
}

function containsValue(value: unknown, expected: unknown): boolean {
  if (typeof value === "string") return typeof expected === "string" && value.includes(expected);
  if (Array.isArray(value)) return value.some((item) => deepEqual(item, expected));
  if (value && typeof value === "object" && !Array.isArray(value)) {
    return typeof expected === "string" && Object.prototype.hasOwnProperty.call(value, expected);
  }
  return false;
}

function isEmptyValue(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.trim().length === 0;
  if (Array.isArray(value)) return value.length === 0;
  if (value && typeof value === "object" && !Array.isArray(value)) {
    return Object.keys(value as Record<string, unknown>).length === 0;
  }
  return false;
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== typeof b) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((item, index) => deepEqual(item, b[index]));
  }
  if (a && b && typeof a === "object" && typeof b === "object" && !Array.isArray(a) && !Array.isArray(b)) {
    const aRecord = a as Record<string, unknown>;
    const bRecord = b as Record<string, unknown>;
    const aKeys = Object.keys(aRecord);
    const bKeys = Object.keys(bRecord);
    return aKeys.length === bKeys.length && aKeys.every((key) => deepEqual(aRecord[key], bRecord[key]));
  }
  return false;
}
