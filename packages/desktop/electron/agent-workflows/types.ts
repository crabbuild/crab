export type AgentWorkflowNodeType =
  | "agent_task"
  | "human_review"
  | "approval_gate"
  | "join"
  | "sub_workflow"
  | "if_else"
  | "loop";

export type AgentWorkflowAgentType = "codex" | "claude-code" | "custom";

export type JsonSchemaObject = Record<string, unknown>;

export interface AgentWorkflowDefinition {
  schemaVersion: 2;
  id: string;
  name: string;
  description?: string;
  tags?: string[];
  definitionVersion: number;
  revisionId: string;
  maxParallelNodes: number;
  defaultAgentType: AgentWorkflowAgentType;
  nodes: AgentWorkflowNode[];
  edges: AgentWorkflowEdge[];
  layout: Record<string, { x: number; y: number }>;
  createdAt: number;
  updatedAt: number;
}

export interface AgentWorkflowNode {
  id: string;
  type: AgentWorkflowNodeType;
  title: string;
  instructions: string;
  agentType?: AgentWorkflowAgentType;
  planFirst?: boolean;
  requireReview?: boolean;
  model?: string;
  intelligence?: string;
  timeoutMs?: number;
  retryPolicy?: { maxAttempts: number };
  outputSchemaName?: string;
  outputSchema?: JsonSchemaObject;
  requireStructuredOutput?: boolean;
  stateUpdates?: Record<string, WorkflowValueRef>;
  subWorkflowDefinitionId?: string;
  inputMapping?: Record<string, WorkflowValueRef>;
  outputMapping?: Record<string, WorkflowValueRef>;
  ifElse?: AgentWorkflowIfElseConfig;
  loop?: WorkflowLoopConfig;
  approvalGate?: AgentWorkflowApprovalGateConfig;
  humanReview?: AgentWorkflowHumanReviewConfig;
  joinConfig?: JoinConfig;
}

export type JoinStrategy = "first" | "last" | "all" | "concat";

export interface JoinConfig {
  strategy?: JoinStrategy;
  customTemplate?: string;
}

export interface AgentWorkflowApprovalGateConfig {
  approvedLabel?: string;
  rejectedLabel?: string;
  reviewTimeoutMs?: number;
  onReviewTimeout?: "approve" | "reject" | "fail";
}

export interface AgentWorkflowHumanReviewConfig {
  reviewTargetNodeId?: string;
  reviewTimeoutMs?: number;
  onReviewTimeout?: "approve" | "reject" | "fail";
}

export interface AgentWorkflowIfElseConfig {
  /** Legacy single true/false condition. Kept for schemaVersion 2 compatibility. */
  condition?: WorkflowNodeCondition;
  branches?: WorkflowIfElseBranch[];
}

export interface WorkflowIfElseBranch {
  id: string;
  label: string;
  kind: "if" | "elif" | "else";
  celExpression?: string;
  /** Legacy condition builder shape. Kept for schemaVersion 2 compatibility. */
  condition?: WorkflowConditionGroup;
}

export interface AgentWorkflowEdge {
  id: string;
  sourceNodeId: string;
  targetNodeId: string;
  condition?: WorkflowEdgeCondition;
}

export interface WorkflowEdgeCondition {
  sourceStatus: "completed" | "failed" | "cancelled" | "skipped" | "always";
  branchId?: string;
  outputPredicate?: WorkflowOutputPredicate;
}

export interface WorkflowOutputPredicate {
  path: string;
  operator:
    | "exists"
    | "not_exists"
    | "equals"
    | "not_equals"
    | "contains"
    | "not_contains"
    | "starts_with"
    | "ends_with"
    | "is_empty"
    | "is_not_empty"
    | "greater_than"
    | "less_than"
    | "in";
  value?: unknown;
}

export interface WorkflowValueRef {
  source: "literal" | "card" | "nodeSummary" | "nodeOutput" | "runInput";
  value?: unknown;
  nodeId?: string;
  path?: string;
}

export interface WorkflowNodeCondition {
  left: WorkflowValueRef;
  operator: WorkflowOutputPredicate["operator"];
  value?: unknown;
  right?: WorkflowValueRef;
}

export interface WorkflowConditionGroup {
  logic: "and" | "or";
  conditions: WorkflowNodeCondition[];
}

export interface WorkflowLoopConfig {
  maxIterations: number;
  /** Legacy continue-while condition. */
  continueCondition?: WorkflowNodeCondition;
  terminateCondition?: WorkflowConditionGroup;
  initialState?: Record<string, WorkflowValueRef>;
  stateMapping?: Record<string, WorkflowValueRef>;
}

export type WorkflowRunStatus =
  | "queued"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export type NodeRunStatus =
  | "blocked"
  | "ready"
  | "queued"
  | "running"
  | "needs_review"
  | "completed"
  | "failed"
  | "cancelled"
  | "skipped";

export type EdgeRunStatus = "pending" | "activated" | "not_activated";

export interface AgentWorkflowRun {
  id: string;
  definitionId: string;
  definitionSnapshot: AgentWorkflowDefinition;
  repoPath: string;
  status: WorkflowRunStatus;
  trigger?: WorkflowRunTriggerSource;
  sourceBoardId?: string;
  sourceCardId?: string;
  sourceTaskId?: string;
  parentRunId?: string;
  parentNodeRunId?: string;
  input?: Record<string, unknown>;
  state?: Record<string, unknown>;
  nodeRuns: AgentWorkflowNodeRun[];
  edgeRuns: AgentWorkflowEdgeRun[];
  startedAt: number;
  finishedAt?: number;
  userPaused?: boolean;
}

export interface AgentWorkflowNodeRun {
  id: string;
  nodeId: string;
  status: NodeRunStatus;
  taskId?: string;
  childRunId?: string;
  childRunIds?: string[];
  attempt: number;
  outputSummary?: string;
  outputJson?: unknown;
  outputJsonError?: string;
  reviewFeedback?: string[];
  /** Human-typed reason on approval gate approve/reject actions. */
  approvalReason?: string;
  error?: string;
  skipReason?: string;
  startedAt?: number;
  finishedAt?: number;
}

export interface AgentWorkflowEdgeRun {
  edgeId: string;
  status: EdgeRunStatus;
  evaluatedAt?: number;
  reason?: string;
}

export interface WorkflowRunTriggerSource {
  type: "builder" | "chat" | "kanban" | "schedule" | "sub_workflow" | "retry" | "webhook";
  triggerId?: string;
  scheduledAt?: number;
}

export interface AgentWorkflowTrigger {
  id: string;
  repoPath: string;
  workflowDefinitionId: string;
  name: string;
  kind: "schedule" | "webhook";
  rrule: string;
  enabled: boolean;
  input?: Record<string, unknown>;
  webhookToken?: string;
  lastScheduledAt?: number;
  lastRunId?: string;
  createdAt: number;
  updatedAt: number;
}

export type AgentWorkflowEventType =
  | "run_started"
  | "run_updated"
  | "run_completed"
  | "run_failed"
  | "run_cancelled"
  | "run_paused"
  | "run_resumed"
  | "node_ready"
  | "node_started"
  | "node_needs_review"
  | "node_completed"
  | "node_failed"
  | "node_cancelled"
  | "node_feedback"
  | "node_retried"
  | "node_skipped"
  | "edge_activated"
  | "edge_not_activated"
  | "child_run_linked"
  | "trigger_fired"
  | "trigger_skipped"
  | "trigger_failed"
  | "projection_error";

export interface AgentWorkflowEvent {
  id: string;
  runId: string;
  nodeRunId?: string;
  type: AgentWorkflowEventType;
  message: string;
  timestamp: number;
  payload?: Record<string, unknown>;
}

export interface AgentWorkflowValidationResult {
  valid: boolean;
  errors: string[];
  warnings: string[];
  topologicalNodeIds: string[];
  cyclePath?: string[];
}

export interface AgentWorkflowStartRunOptions {
  sourceBoardId?: string;
  sourceCardId?: string;
  sourceTaskId?: string;
  createSourceTask?: boolean;
  sourceTaskTitle?: string;
  projectToKanban?: boolean;
  input?: Record<string, unknown>;
  trigger?: WorkflowRunTriggerSource;
  parentRunId?: string;
  parentNodeRunId?: string;
  definitionSnapshot?: AgentWorkflowDefinition;
}

export interface AgentWorkflowRetryNodeOptions {
  useLatestDefinition?: boolean;
}

export interface AgentWorkflowVersionMeta {
  version: number;
  revisionId: string;
  name: string;
  timestamp: number;
}

export interface AgentWorkflowSimulationNodeResult {
  nodeId: string;
  title: string;
  type: AgentWorkflowNodeType;
  status: "activated" | "skipped" | "blocked" | "error";
  reason?: string;
  branchId?: string;
  branchLabel?: string;
}

export interface AgentWorkflowSimulationEdgeResult {
  edgeId: string;
  sourceNodeId: string;
  targetNodeId: string;
  status: "activated" | "not_activated" | "blocked";
  reason?: string;
}

export interface AgentWorkflowSimulationResult {
  nodes: AgentWorkflowSimulationNodeResult[];
  edges: AgentWorkflowSimulationEdgeResult[];
  errors: string[];
}
