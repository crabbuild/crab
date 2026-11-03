import { memo, useCallback, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import clsx from "clsx";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import {
  addEdge,
  Background,
  BaseEdge,
  Connection,
  Controls,
  Edge,
  EdgeLabelRenderer,
  EdgeProps,
  EdgeTypes,
  getBezierPath,
  Handle,
  MiniMap,
  Node as ReactFlowNode,
  NodeProps,
  NodeTypes,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState,
  useUpdateNodeInternals,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import {
  Asterisk,
  Award,
  Ban,
  Bot,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  CircleStop,
  CircleDot,
  ClipboardPaste,
  Clock,
  Copy,
  ExternalLink,
  FolderPlus,
  GitBranch,
  History,
  Merge,
  MessageSquare,
  Network,
  Pause,
  Play,
  Plus,
  Redo2,
  Repeat2,
  RotateCcw,
  Save,
  Search,
  SkipForward,
  Split,
  Tag,
  Trash2,
  Undo2,
  UserCheck,
  X,
  XCircle,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { JsonSchemaBuilderDialog } from "../components/workflow/JsonSchemaBuilderDialog";
import { intelligenceLabel, modelsForAgent } from "../components/orchestrator/model-catalog";
import type { IntelligenceLevel, ModelEntry } from "../components/orchestrator/model-catalog";
import type { OpenRepo } from "../state";
import type {
  AgentWorkflowAgentType,
  AgentWorkflowDefinition,
  AgentWorkflowEdge,
  AgentWorkflowEvent,
  AgentWorkflowNode,
  AgentWorkflowNodeType,
  AgentWorkflowRun,
  AgentWorkflowSimulationResult,
  AgentWorkflowTrigger,
  AgentWorkflowValidationResult,
  AgentWorkflowVersionMeta,
  JoinStrategy,
  NodeRunStatus,
  WorkflowConditionGroup,
  WorkflowEdgeCondition,
  WorkflowIfElseBranch,
  WorkflowNodeCondition,
  WorkflowOutputPredicate,
  WorkflowValueRef,
} from "../ipc";

interface AgentWorkflowBuilderViewProps {
  repo: OpenRepo;
}

type WorkflowGraphNode = ReactFlowNode<{
  workflowNode: AgentWorkflowNode;
  status?: NodeRunStatus;
  selected: boolean;
  outputSummary?: string;
}>;

type BranchKind = "if" | "elif" | "else" | "approved" | "rejected";

type WorkflowGraphEdgeData = {
  condition?: WorkflowEdgeCondition;
  selected: boolean;
  activated: boolean;
  branchLabel?: string;
  branchKind?: BranchKind;
};

type WorkflowGraphEdge = Edge<WorkflowGraphEdgeData, "workflowEdge">;

type EdgeSourceStatus = WorkflowEdgeCondition["sourceStatus"];
type CelSuggestionType = "string" | "number" | "boolean" | "enum" | "object" | "array" | "unknown";
type CelAssistantOperator =
  | "exists"
  | "not_exists"
  | "equals"
  | "not_equals"
  | "greater_equal"
  | "greater_than"
  | "less_equal"
  | "less_than"
  | "contains"
  | "is_true"
  | "is_false"
  | "is_empty"
  | "is_not_empty";

interface CelInputSuggestion {
  id: string;
  path: string;
  displayPath: string;
  fieldName: string;
  type: CelSuggestionType;
  sourceNodeId: string;
  sourceTitle: string;
  description?: string;
  enumValues?: string[];
}

const NODE_TYPE_LABELS: Record<AgentWorkflowNodeType, string> = {
  agent_task: "Agent Task",
  human_review: "Human Review",
  approval_gate: "Approval Gate",
  join: "Join",
  sub_workflow: "Sub-Workflow",
  if_else: "If / Else",
  loop: "Loop",
};

const STATUS_LABELS: Record<NodeRunStatus, string> = {
  blocked: "Blocked",
  ready: "Ready",
  queued: "Queued",
  running: "Running",
  needs_review: "Needs Review",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
  skipped: "Skipped",
};

const STATUS_CLASSES: Record<NodeRunStatus, string> = {
  blocked: "border-gx-border text-gx-muted",
  ready: "border-gx-info/60 text-gx-info",
  queued: "border-gx-info/40 text-gx-info",
  running: "border-gx-accent/70 text-gx-accent",
  needs_review: "border-gx-warn/70 text-gx-warn",
  completed: "border-gx-success/70 text-gx-success",
  failed: "border-gx-error/70 text-gx-error",
  cancelled: "border-gx-muted/50 text-gx-muted",
  skipped: "border-gx-muted/50 text-gx-muted",
};

const EDGE_SOURCE_STATUS_OPTIONS: Array<{
  value: EdgeSourceStatus;
  label: string;
  description: string;
  Icon: LucideIcon;
  className: string;
  activeClassName: string;
  badgeClassName: string;
}> = [
  {
    value: "completed",
    label: "Completed",
    description: "Route when the source node completes.",
    Icon: CheckCircle2,
    className: "text-gx-success",
    activeClassName: "border-gx-success/60 bg-gx-success/10 text-gx-success",
    badgeClassName: "border-gx-success/40 bg-gx-success/10 text-gx-success",
  },
  {
    value: "failed",
    label: "Failed",
    description: "Route when the source node fails.",
    Icon: XCircle,
    className: "text-gx-error",
    activeClassName: "border-gx-error/60 bg-gx-error/10 text-gx-error",
    badgeClassName: "border-gx-error/40 bg-gx-error/10 text-gx-error",
  },
  {
    value: "cancelled",
    label: "Cancelled",
    description: "Route when the source node is cancelled.",
    Icon: Ban,
    className: "text-gx-muted",
    activeClassName: "border-gx-muted/60 bg-gx-bg-sub text-gx-text",
    badgeClassName: "border-gx-border bg-gx-bg-sub text-gx-muted",
  },
  {
    value: "skipped",
    label: "Skipped",
    description: "Route when the source node is skipped.",
    Icon: SkipForward,
    className: "text-gx-info",
    activeClassName: "border-gx-info/60 bg-gx-info/10 text-gx-info",
    badgeClassName: "border-gx-info/40 bg-gx-info/10 text-gx-info",
  },
  {
    value: "always",
    label: "Always",
    description: "Route after the source node reaches any terminal state.",
    Icon: Asterisk,
    className: "text-gx-accent",
    activeClassName: "border-gx-accent/60 bg-gx-accent/10 text-gx-accent",
    badgeClassName: "border-gx-accent/40 bg-gx-accent/10 text-gx-accent",
  },
];

const CEL_ASSISTANT_OPERATORS: Record<CelSuggestionType, Array<{ value: CelAssistantOperator; label: string }>> = {
  string: [
    { value: "equals", label: "=" },
    { value: "not_equals", label: "!=" },
    { value: "contains", label: "contains" },
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
  ],
  number: [
    { value: "greater_equal", label: ">=" },
    { value: "greater_than", label: ">" },
    { value: "less_equal", label: "<=" },
    { value: "less_than", label: "<" },
    { value: "equals", label: "=" },
    { value: "not_equals", label: "!=" },
    { value: "exists", label: "exists" },
  ],
  boolean: [
    { value: "is_true", label: "true" },
    { value: "is_false", label: "false" },
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
  ],
  enum: [
    { value: "equals", label: "=" },
    { value: "not_equals", label: "!=" },
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
  ],
  object: [
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
  ],
  array: [
    { value: "contains", label: "contains" },
    { value: "is_not_empty", label: "not empty" },
    { value: "is_empty", label: "empty" },
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
  ],
  unknown: [
    { value: "exists", label: "exists" },
    { value: "not_exists", label: "missing" },
    { value: "equals", label: "=" },
    { value: "not_equals", label: "!=" },
  ],
};

const EDGE_OPERATOR_LABELS: Record<WorkflowOutputPredicate["operator"], string> = {
  exists: "exists",
  not_exists: "missing",
  equals: "=",
  not_equals: "!=",
  contains: "contains",
  not_contains: "not contains",
  starts_with: "starts with",
  ends_with: "ends with",
  is_empty: "empty",
  is_not_empty: "not empty",
  greater_than: ">",
  less_than: "<",
  in: "in",
};

const DEFAULT_INTELLIGENCE_OPTIONS: IntelligenceLevel[] = ["low", "medium", "high", "extra-high"];

function modelOptionsForAgent(agentType: AgentWorkflowAgentType, currentModel?: string): ModelEntry[] {
  const options = modelsForAgent(agentType);
  const model = currentModel?.trim();
  if (!model || options.some((option) => option.value === model)) return options;
  return [...options, { value: model, label: model, tier: "other" }];
}

function intelligenceOptionsForModel(
  agentType: AgentWorkflowAgentType,
  modelValue?: string,
  currentIntelligence?: string,
): string[] {
  const options = modelsForAgent(agentType);
  const selectedModel = modelValue
    ? options.find((option) => option.value === modelValue)
    : options.find((option) => option.tier === "recommended") ?? options[0];
  const levels = selectedModel?.intelligenceLevels ?? DEFAULT_INTELLIGENCE_OPTIONS;
  const intelligence = currentIntelligence?.trim();
  if (!intelligence || levels.includes(intelligence as IntelligenceLevel)) return levels;
  return [...levels, intelligence];
}

function isModelCompatibleWithAgent(agentType: AgentWorkflowAgentType, modelValue?: string): boolean {
  if (!modelValue || agentType === "custom") return true;
  const options = modelsForAgent(agentType);
  return options.length === 0 || options.some((option) => option.value === modelValue);
}

function isIntelligenceCompatibleWithModel(
  agentType: AgentWorkflowAgentType,
  modelValue: string | undefined,
  intelligence: string | undefined,
): boolean {
  if (!intelligence) return true;
  return intelligenceOptionsForModel(agentType, modelValue).includes(intelligence);
}

function formatIntelligenceLabel(level: string): string {
  return DEFAULT_INTELLIGENCE_OPTIONS.includes(level as IntelligenceLevel)
    ? intelligenceLabel(level as IntelligenceLevel)
    : level;
}

const CONDITION_OPERATOR_OPTIONS: Array<{ value: WorkflowOutputPredicate["operator"]; label: string; needsValue: boolean }> = [
  { value: "exists", label: "Exists", needsValue: false },
  { value: "not_exists", label: "Does not exist", needsValue: false },
  { value: "is_empty", label: "Is empty", needsValue: false },
  { value: "is_not_empty", label: "Is not empty", needsValue: false },
  { value: "equals", label: "Equals", needsValue: true },
  { value: "not_equals", label: "Not equals", needsValue: true },
  { value: "contains", label: "Contains", needsValue: true },
  { value: "not_contains", label: "Not contains", needsValue: true },
  { value: "starts_with", label: "Starts with", needsValue: true },
  { value: "ends_with", label: "Ends with", needsValue: true },
  { value: "greater_than", label: "Greater than", needsValue: true },
  { value: "less_than", label: "Less than", needsValue: true },
  { value: "in", label: "In", needsValue: true },
];

const LS_PROPERTIES_OPEN_KEY = "crab.agentWorkflowBuilder.propertiesOpen";
const LS_RUN_OPEN_KEY = "crab.agentWorkflowBuilder.runOpen";
const LS_SCHEDULES_OPEN_KEY = "crab.agentWorkflowBuilder.schedulesOpen";
const LS_PROPERTIES_SIZE_KEY = "crab.agentWorkflowBuilder.propertiesSize";
const LS_RUN_SIZE_KEY = "crab.agentWorkflowBuilder.runSize";
const LS_SCHEDULES_SIZE_KEY = "crab.agentWorkflowBuilder.schedulesSize";

interface WorkflowBreadcrumb {
  definitionId: string;
  nodeId?: string;
  label: string;
}

function readBooleanSetting(key: string, fallback: boolean): boolean {
  if (typeof localStorage === "undefined") return fallback;
  const value = localStorage.getItem(key);
  return value == null ? fallback : value === "1";
}

function readPanelSize(key: string, fallback: number): number {
  if (typeof localStorage === "undefined") return fallback;
  const value = Number(localStorage.getItem(key));
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function persistBooleanSetting(key: string, value: boolean): void {
  if (typeof localStorage !== "undefined") localStorage.setItem(key, value ? "1" : "0");
}

function persistPanelSize(key: string, value: number): void {
  if (typeof localStorage !== "undefined" && value >= 6) localStorage.setItem(key, String(value));
}

function createId(prefix: string): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return `${prefix}-${crypto.randomUUID()}`;
  }
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

interface HistoryEntry {
  definition: AgentWorkflowDefinition;
  selectedNodeIds: string[];
  selectedEdgeId: string | null;
  label: string;
}

function useUndoRedo() {
  const [past, setPast] = useState<HistoryEntry[]>([]);
  const [future, setFuture] = useState<HistoryEntry[]>([]);

  const push = useCallback((entry: HistoryEntry) => {
    setPast((prev) => [...prev.slice(-49), entry]);
    setFuture([]);
  }, []);

  const undo = useCallback(
    (current: HistoryEntry): HistoryEntry | null => {
      if (!past.length) return null;
      const prev = past[past.length - 1];
      setPast((p) => p.slice(0, -1));
      setFuture((f) => [...f, current]);
      return prev;
    },
    [past],
  );

  const redo = useCallback(
    (_current: HistoryEntry): HistoryEntry | null => {
      if (!future.length) return null;
      const next = future[future.length - 1];
      setFuture((f) => f.slice(0, -1));
      setPast((p) => [...p, next]);
      return next;
    },
    [future],
  );

  return { push, undo, redo, canUndo: past.length > 0, canRedo: future.length > 0 };
}

function createDefaultDefinition(): AgentWorkflowDefinition {
  const now = Date.now();
  const firstNodeId = createId("node");
  return {
    schemaVersion: 2,
    id: createId("workflow"),
    name: "New Agent Workflow",
    description: "",
    tags: [],
    definitionVersion: 1,
    revisionId: createId("revision"),
    maxParallelNodes: 2,
    defaultAgentType: "codex",
    nodes: [
      {
        id: firstNodeId,
        type: "agent_task",
        title: "Implement Task",
        instructions: "Describe the agent work to perform.",
        agentType: "codex",
        planFirst: false,
        requireReview: false,
        retryPolicy: { maxAttempts: 1 },
      },
    ],
    edges: [],
    layout: { [firstNodeId]: { x: 80, y: 80 } },
    createdAt: now,
    updatedAt: now,
  };
}

function createChildWorkflowDefinition(parent: AgentWorkflowDefinition, node: AgentWorkflowNode): AgentWorkflowDefinition {
  const child = createDefaultDefinition();
  const firstNode = child.nodes[0];
  return {
    ...child,
    name: `${node.title || "Sub-Workflow"} Child`,
    description: `Child workflow for ${parent.name}.`,
    nodes: firstNode
      ? [{
          ...firstNode,
          title: "Child Step",
          instructions: "Use the child workflow input from {{run.input.input}}, complete the work, and return a concise summary.",
        }]
      : child.nodes,
    updatedAt: Date.now(),
  };
}

function createDefaultCondition() {
  return {
    left: { source: "runInput" as const },
    operator: "exists" as const,
  };
}

function createDefaultConditionGroup(): WorkflowConditionGroup {
  return {
    logic: "and",
    conditions: [createDefaultCondition()],
  };
}

function createDefaultIfElseBranches(): WorkflowIfElseBranch[] {
  return [
    { id: "if", label: "IF", kind: "if", celExpression: "input != null" },
    { id: "else", label: "ELSE", kind: "else" },
  ];
}

interface WorkflowTemplate {
  id: string;
  name: string;
  description: string;
  create: () => AgentWorkflowDefinition;
}

function createWorkflowTemplates(): WorkflowTemplate[] {
  return [
    {
      id: "code-review",
      name: "Code Review",
      description: "Agent task → Human review → Approval gate. Review code changes before merging.",
      create: () => {
        const now = Date.now();
        const codeId = createId("node");
        const reviewId = createId("node");
        const approveId = createId("node");
        return {
          schemaVersion: 2,
          id: createId("workflow"),
          name: "Code Review Workflow",
          description: "Review code changes with human oversight and approval.",
          tags: ["code-review"],
          definitionVersion: 1,
          revisionId: createId("revision"),
          maxParallelNodes: 2,
          defaultAgentType: "codex",
          nodes: [
            { id: codeId, type: "agent_task", title: "Analyze Changes", instructions: "Review the code diff and identify issues, risks, and suggestions.", agentType: "codex", planFirst: true, requireReview: false, retryPolicy: { maxAttempts: 1 } },
            { id: reviewId, type: "human_review", title: "Human Review", instructions: "Review the agent analysis and provide feedback.", humanReview: { reviewTargetNodeId: codeId } },
            { id: approveId, type: "approval_gate", title: "Approve", instructions: "Approve or reject the changes.", approvalGate: { approvedLabel: "Merge", rejectedLabel: "Revise" } },
          ],
          edges: [
            { id: createId("edge"), sourceNodeId: codeId, targetNodeId: reviewId },
            { id: createId("edge"), sourceNodeId: reviewId, targetNodeId: approveId },
          ],
          layout: { [codeId]: { x: 80, y: 80 }, [reviewId]: { x: 320, y: 80 }, [approveId]: { x: 560, y: 80 } },
          createdAt: now, updatedAt: now,
        };
      },
    },
    {
      id: "ci-pipeline",
      name: "CI Pipeline",
      description: "Lint → Test → Build, with approval gate before deploy. Parallel lint+test.",
      create: () => {
        const now = Date.now();
        const lintId = createId("node");
        const testId = createId("node");
        const joinId = createId("node");
        const buildId = createId("node");
        const approveId = createId("node");
        return {
          schemaVersion: 2,
          id: createId("workflow"),
          name: "CI Pipeline",
          description: "Lint, test, build, and deploy with approval.",
          tags: ["ci"],
          definitionVersion: 1,
          revisionId: createId("revision"),
          maxParallelNodes: 4,
          defaultAgentType: "codex",
          nodes: [
            { id: lintId, type: "agent_task", title: "Lint", instructions: "Run the linter and fix any issues.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: testId, type: "agent_task", title: "Run Tests", instructions: "Run the test suite and fix failures.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: joinId, type: "join", title: "Merge Results", instructions: "Collect lint and test results.", joinConfig: { strategy: "concat", customTemplate: "Lint and test results:\n{{summaries}}" } },
            { id: buildId, type: "agent_task", title: "Build", instructions: "Build the project and verify artifacts.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: approveId, type: "approval_gate", title: "Deploy?", instructions: "Approve to deploy.", approvalGate: { approvedLabel: "Deploy", rejectedLabel: "Skip" } },
          ],
          edges: [
            { id: createId("edge"), sourceNodeId: lintId, targetNodeId: joinId },
            { id: createId("edge"), sourceNodeId: testId, targetNodeId: joinId },
            { id: createId("edge"), sourceNodeId: joinId, targetNodeId: buildId },
            { id: createId("edge"), sourceNodeId: buildId, targetNodeId: approveId },
          ],
          layout: { [lintId]: { x: 80, y: 30 }, [testId]: { x: 80, y: 130 }, [joinId]: { x: 320, y: 80 }, [buildId]: { x: 560, y: 80 }, [approveId]: { x: 800, y: 80 } },
          createdAt: now, updatedAt: now,
        };
      },
    },
    {
      id: "multi-agent",
      name: "Multi-Agent Brainstorm",
      description: "Two agents brainstorm in parallel, then a third synthesizes. With human approval.",
      create: () => {
        const now = Date.now();
        const ideaAId = createId("node");
        const ideaBId = createId("node");
        const joinId = createId("node");
        const synthId = createId("node");
        const reviewId = createId("node");
        return {
          schemaVersion: 2,
          id: createId("workflow"),
          name: "Multi-Agent Brainstorm",
          description: "Parallel brainstorming with synthesis and review.",
          tags: ["brainstorm"],
          definitionVersion: 1,
          revisionId: createId("revision"),
          maxParallelNodes: 4,
          defaultAgentType: "codex",
          nodes: [
            { id: ideaAId, type: "agent_task", title: "Approach A", instructions: "Brainstorm approach A: conservative, incremental solution.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: ideaBId, type: "agent_task", title: "Approach B", instructions: "Brainstorm approach B: bold, innovative solution.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: joinId, type: "join", title: "Combine Ideas", instructions: "Collect both approaches.", joinConfig: { strategy: "concat", customTemplate: "{{summaries}}" } },
            { id: synthId, type: "agent_task", title: "Synthesize", instructions: "Synthesize the best parts of both approaches into a unified plan.", agentType: "codex", retryPolicy: { maxAttempts: 1 } },
            { id: reviewId, type: "human_review", title: "Review Plan", instructions: "Review the synthesized plan.", humanReview: { reviewTargetNodeId: synthId } },
          ],
          edges: [
            { id: createId("edge"), sourceNodeId: ideaAId, targetNodeId: joinId },
            { id: createId("edge"), sourceNodeId: ideaBId, targetNodeId: joinId },
            { id: createId("edge"), sourceNodeId: joinId, targetNodeId: synthId },
            { id: createId("edge"), sourceNodeId: synthId, targetNodeId: reviewId },
          ],
          layout: { [ideaAId]: { x: 80, y: 30 }, [ideaBId]: { x: 80, y: 130 }, [joinId]: { x: 320, y: 80 }, [synthId]: { x: 560, y: 80 }, [reviewId]: { x: 800, y: 80 } },
          createdAt: now, updatedAt: now,
        };
      },
    },
  ];
}

function ensureIfElseBranches(node: AgentWorkflowNode): WorkflowIfElseBranch[] {
  if (node.ifElse?.branches?.length) return node.ifElse.branches;
  return [
    {
      id: "if",
      label: "IF",
      kind: "if",
      celExpression: node.ifElse?.condition ? undefined : "input != null",
      condition: node.ifElse?.condition
        ? { logic: "and", conditions: [node.ifElse.condition] }
        : createDefaultConditionGroup(),
    },
    { id: "else", label: "ELSE", kind: "else" },
  ];
}

function branchEdgeCondition(branchId: string): WorkflowEdgeCondition {
  return {
    sourceStatus: "completed",
    branchId,
  };
}

function approvalGateBranchId(handleId?: string | null): "approved" | "rejected" | undefined {
  return handleId === "approved" || handleId === "rejected" ? handleId : undefined;
}

function edgeSourceHandle(sourceNode: AgentWorkflowNode | undefined, condition?: WorkflowEdgeCondition): string | undefined {
  if (sourceNode?.type === "if_else") return edgeBranchId(condition);
  if (sourceNode?.type === "approval_gate") return approvalGateBranchId(edgeBranchId(condition));
  return undefined;
}

function edgeBranchId(condition?: WorkflowEdgeCondition): string | undefined {
  if (condition?.branchId) return condition.branchId;
  const predicate = condition?.outputPredicate;
  return predicate?.path === "branch" && predicate.operator === "equals" && typeof predicate.value === "string"
    ? predicate.value
    : undefined;
}

function parseJsonObjectDraft(draft: string): { value: Record<string, unknown>; error?: undefined } | { value?: undefined; error: string } {
  try {
    const parsed = draft.trim() ? JSON.parse(draft) as unknown : {};
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return { error: "Input must be a JSON object." };
    }
    return { value: parsed as Record<string, unknown> };
  } catch (err) {
    return { error: err instanceof Error ? err.message : "Invalid JSON." };
  }
}

function formatDurationMs(startedAt: number, finishedAt?: number): string {
  const end = finishedAt ?? Date.now();
  const seconds = Math.max(0, Math.round((end - startedAt) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return `${minutes}m ${rest}s`;
}

function formatInputSummary(input?: Record<string, unknown>): string {
  if (!input || Object.keys(input).length === 0) return "input {}";
  const summary = JSON.stringify(input).replace(/\s+/g, " ");
  return `input ${summary.length > 90 ? `${summary.slice(0, 89)}...` : summary}`;
}

function collectChildWorkflowDefinitionIds(definitions: AgentWorkflowDefinition[]): Set<string> {
  const childIds = new Set<string>();
  for (const definition of definitions) {
    for (const node of definition.nodes) {
      if ((node.type === "sub_workflow" || node.type === "loop") && node.subWorkflowDefinitionId) {
        childIds.add(node.subWorkflowDefinitionId);
      }
    }
  }
  return childIds;
}

function getRootWorkflowDefinitions(definitions: AgentWorkflowDefinition[]): AgentWorkflowDefinition[] {
  const childIds = collectChildWorkflowDefinitionIds(definitions);
  return definitions.filter((definition) => !childIds.has(definition.id));
}

function IconButton({
  title,
  onClick,
  disabled,
  variant = "secondary",
  className,
  children,
}: {
  title: string;
  onClick?: () => void;
  disabled?: boolean;
  variant?: "secondary" | "primary" | "danger" | "ghost";
  className?: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={clsx(
        "inline-flex h-7 w-7 items-center justify-center rounded border text-xs transition disabled:pointer-events-none disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent focus-visible:outline-offset-2",
        variant === "primary" && "border-transparent bg-gx-accent text-white hover:bg-gx-accent-hover",
        variant === "danger" && "border-transparent bg-gx-error text-white hover:bg-gx-error/90",
        variant === "secondary" && "border-gx-border bg-gx-surface text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
        variant === "ghost" && "border-transparent bg-transparent text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
        className,
      )}
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={title}
    >
      {children}
    </button>
  );
}

function CommandButton({
  title,
  label,
  onClick,
  disabled,
  variant = "secondary",
  children,
}: {
  title: string;
  label: string;
  onClick?: () => void;
  disabled?: boolean;
  variant?: "secondary" | "primary" | "danger" | "ghost";
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={clsx(
        "inline-flex h-7 items-center gap-1.5 rounded border px-2 text-xs transition disabled:pointer-events-none disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent focus-visible:outline-offset-2",
        variant === "primary" && "border-transparent bg-gx-accent text-white hover:bg-gx-accent-hover",
        variant === "danger" && "border-gx-error/30 bg-gx-error/10 text-gx-error hover:bg-gx-error/15",
        variant === "secondary" && "border-gx-border bg-gx-surface text-gx-text hover:bg-gx-surface-hover",
        variant === "ghost" && "border-transparent bg-transparent text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
      )}
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={title}
    >
      {children}
      <span>{label}</span>
    </button>
  );
}

function ResizeHandle({ direction = "horizontal" }: { direction?: "horizontal" | "vertical" }) {
  return (
    <PanelResizeHandle
      className={clsx(
        "bg-transparent transition-colors hover:bg-gx-accent/50",
        direction === "horizontal" ? "w-[3px] cursor-col-resize" : "h-[3px] cursor-row-resize",
      )}
    />
  );
}

function CollapsedPanelRail({
  title,
  icon,
  side,
  onOpen,
}: {
  title: string;
  icon: ReactNode;
  side: "left" | "right";
  onOpen: () => void;
}) {
  return (
    <Panel minSize={2.4} maxSize={2.4} defaultSize={2.4} className="min-h-0">
      <div className="flex h-full items-start justify-center border-l border-gx-border bg-gx-surface/60 py-2">
        <IconButton title={title} onClick={onOpen} variant="ghost" className="h-7 w-7">
          {icon ?? (side === "left" ? <ChevronRight size={14} /> : <ChevronLeft size={14} />)}
        </IconButton>
      </div>
    </Panel>
  );
}

function buildGraphNodes(
  definition: AgentWorkflowDefinition,
  activeRun: AgentWorkflowRun | null,
  selectedNodeIds: Set<string>,
): WorkflowGraphNode[] {
  return definition.nodes.map((node, index) => {
    const layout = definition.layout[node.id] ?? { x: 120 + index * 220, y: 100 };
    const nodeRun = activeRun?.nodeRuns.find((run) => run.nodeId === node.id);
    return {
      id: node.id,
      type: "workflowNode",
      position: layout,
      data: {
        workflowNode: node,
        status: nodeRun?.status,
        selected: selectedNodeIds.has(node.id),
        outputSummary: nodeRun?.outputSummary,
      },
    };
  });
}

function getEdgeSourceStatusMeta(status: EdgeSourceStatus = "completed") {
  return EDGE_SOURCE_STATUS_OPTIONS.find((option) => option.value === status) ?? EDGE_SOURCE_STATUS_OPTIONS[0];
}

function edgePredicateLabel(predicate?: WorkflowOutputPredicate): string | null {
  if (!predicate) return null;
  const path = (predicate.path ?? "").trim() || "output";
  const operator = EDGE_OPERATOR_LABELS[predicate.operator];
  return `${path} ${operator}`;
}

function edgeConditionLabel(condition?: WorkflowEdgeCondition): string | null {
  const branchId = edgeBranchId(condition);
  const predicate = edgePredicateLabel(condition?.outputPredicate);
  if (branchId && predicate) return `branch ${branchId} + ${predicate}`;
  if (branchId) return `branch ${branchId}`;
  return predicate;
}

function EdgeConditionPill({
  condition,
  selected = false,
  compact = false,
}: {
  condition?: WorkflowEdgeCondition;
  selected?: boolean;
  compact?: boolean;
}) {
  const meta = getEdgeSourceStatusMeta(condition?.sourceStatus ?? "completed");
  const Icon = meta.Icon;
  const predicate = edgeConditionLabel(condition);

  return (
    <span
      className={clsx(
        "nodrag nopan inline-flex max-w-[190px] items-center gap-1 rounded border shadow-sm backdrop-blur-sm",
        compact ? "px-1.5 py-0.5 text-[10px]" : "px-2 py-1 text-[11px]",
        selected ? "border-gx-accent bg-gx-accent/10 text-gx-accent" : meta.badgeClassName,
      )}
      title={predicate ? `${meta.label} if ${predicate}` : meta.description}
    >
      <Icon size={compact ? 10 : 12} className="shrink-0" />
      <span className="truncate font-medium">{meta.label}</span>
      {predicate && (
        <>
          <span className="text-gx-muted/70">if</span>
          <span className="truncate font-mono text-[10px] text-gx-muted">{predicate}</span>
        </>
      )}
    </span>
  );
}

function EdgeConditionLineBadge({
  condition,
  selected,
  activated,
  branchLabel,
}: {
  condition?: WorkflowEdgeCondition;
  selected: boolean;
  activated: boolean;
  branchLabel?: string;
}) {
  const meta = getEdgeSourceStatusMeta(condition?.sourceStatus ?? "completed");
  const Icon = meta.Icon;
  const predicate = edgeConditionLabel(condition);
  const showText = selected || Boolean(predicate) || Boolean(branchLabel);

  return (
    <span
      className={clsx(
        "nodrag nopan inline-flex h-6 items-center gap-1 rounded-full border bg-gx-bg/95 text-[10px] shadow-sm backdrop-blur-sm",
        showText ? "max-w-[180px] px-2" : "w-6 justify-center px-0",
        selected
          ? "border-gx-accent bg-gx-accent/10 text-gx-accent ring-2 ring-gx-accent/10"
          : activated
            ? "border-gx-accent/50 bg-gx-accent/10 text-gx-accent"
            : meta.badgeClassName,
      )}
      title={branchLabel ?? (predicate ? `${meta.label} if ${predicate}` : meta.description)}
      aria-label={branchLabel ?? (predicate ? `${meta.label} if ${predicate}` : meta.label)}
    >
      <Icon size={13} className="shrink-0" />
      {showText && (
        <span className="truncate font-medium">{branchLabel ?? (predicate ? `${meta.label} if ${predicate}` : meta.label)}</span>
      )}
    </span>
  );
}

const BranchEdgeColors: Record<BranchKind, string> = {
  if: "#22c55e",
  elif: "#f59e0b",
  else: "#ef4444",
  approved: "#22c55e",
  rejected: "#ef4444",
};

const WorkflowEdge = memo(function WorkflowEdge({
  id,
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
  markerEnd,
  style,
  selected,
  data,
}: EdgeProps<WorkflowGraphEdge>) {
  const [edgePath, labelX, labelY] = getBezierPath({
    sourceX,
    sourceY,
    sourcePosition,
    targetX,
    targetY,
    targetPosition,
  });
  const isSelected = Boolean(selected || data?.selected);
  const isActivated = Boolean(data?.activated);
  const branchColor = data?.branchKind ? BranchEdgeColors[data.branchKind] : undefined;

  return (
    <>
      <BaseEdge
        id={id}
        path={edgePath}
        markerEnd={markerEnd}
        interactionWidth={26}
        className={clsx(
          "transition-colors",
          !branchColor && (isSelected || isActivated ? "stroke-gx-accent" : "stroke-gx-border"),
        )}
        style={{
          ...style,
          stroke: branchColor,
          strokeWidth: isSelected ? 2 : 1.35,
        }}
      />
      <EdgeLabelRenderer>
        <div
          className="absolute"
          style={{
            transform: `translate(-50%, -50%) translate(${labelX}px, ${labelY}px)`,
            pointerEvents: "none",
          }}
        >
          <EdgeConditionLineBadge condition={data?.condition} selected={isSelected} activated={isActivated} branchLabel={data?.branchLabel} />
        </div>
      </EdgeLabelRenderer>
    </>
  );
});

function buildGraphEdges(definition: AgentWorkflowDefinition, activeRun: AgentWorkflowRun | null, selectedEdgeId: string | null): WorkflowGraphEdge[] {
  return definition.edges.map((edge) => {
    const sourceNode = definition.nodes.find((node) => node.id === edge.sourceNodeId);
    const sourceHandle = edgeSourceHandle(sourceNode, edge.condition);
    const activated = activeRun?.edgeRuns.find((edgeRun) => edgeRun.edgeId === edge.id)?.status === "activated";
    const selected = selectedEdgeId === edge.id;
    const branchInfo = deriveBranchInfo(sourceNode, edge);
    return {
      id: edge.id,
      type: "workflowEdge",
      source: edge.sourceNodeId,
      sourceHandle,
      target: edge.targetNodeId,
      animated: activated,
      data: {
        condition: edge.condition,
        selected,
        activated,
        branchLabel: branchInfo.label,
        branchKind: branchInfo.kind,
      },
    };
  });
}

function deriveBranchInfo(sourceNode: AgentWorkflowNode | undefined, edge: AgentWorkflowEdge): { label?: string; kind?: BranchKind } {
  if (!sourceNode) return {};
  if (sourceNode.type === "if_else") {
    const branchId = edgeBranchId(edge.condition);
    const branch = sourceNode.ifElse?.branches?.find((b) => b.id === branchId);
    if (branch) return { label: branch.label, kind: branch.kind };
  }
  if (sourceNode.type === "approval_gate") {
    const branchId = edgeBranchId(edge.condition);
    if (branchId === "approved") return { label: sourceNode.approvalGate?.approvedLabel ?? "Approved", kind: "approved" };
    if (branchId === "rejected") return { label: sourceNode.approvalGate?.rejectedLabel ?? "Rejected", kind: "rejected" };
  }
  return {};
}

const WorkflowNode = memo(function WorkflowNode({ data }: NodeProps<WorkflowGraphNode>) {
  const node = data.workflowNode;
  const status = data.status;
  const Icon = node.type === "agent_task"
    ? Bot
    : node.type === "human_review"
      ? UserCheck
      : node.type === "approval_gate"
        ? Award
        : node.type === "join"
          ? Merge
          : node.type === "sub_workflow"
            ? Network
            : node.type === "if_else"
              ? Split
              : Repeat2;

  if (node.type === "if_else") {
    const branches = ensureIfElseBranches(node);
    return (
      <div
        className={clsx(
          "w-[190px] rounded-[20px] border bg-gx-surface p-3 text-xs shadow-sm",
          data.selected ? "border-gx-accent ring-2 ring-gx-accent/15" : "border-gx-border",
        )}
      >
        <Handle type="target" position={Position.Left} className="!bg-gx-border" />
        <div className="mb-3 flex items-center gap-2">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gx-warn/20 text-gx-warn">
            <Split size={15} />
          </div>
          <div className="min-w-0 flex-1">
            <div className="truncate font-medium text-gx-text">{node.title || "If / Else"}</div>
            <div className="truncate text-[10px] text-gx-muted">If / Else</div>
          </div>
        </div>
        <div className="space-y-1.5">
          {branches.map((branch) => (
            <div
              key={branch.id}
              className={clsx(
                "relative flex h-8 items-center rounded-lg border px-3 pr-5",
                branch.kind === "else"
                  ? "border-gx-border bg-gx-bg-sub text-gx-muted"
                  : "border-gx-border/80 bg-gx-bg-sub text-gx-text",
              )}
              title={branch.kind === "else" ? "Fallback branch" : branch.celExpression || "Legacy condition"}
            >
              <span className="truncate text-[11px] font-medium">{branch.label || branch.kind.toUpperCase()}</span>
              <Handle
                id={branch.id}
                type="source"
                position={Position.Right}
                className="!right-[-7px] !h-3 !w-3 !border-2 !border-gx-surface !bg-gx-border"
              />
            </div>
          ))}
        </div>
        <div className="mt-3 flex items-center gap-1.5">
          <span
            className={clsx(
              "rounded border px-1.5 py-0 text-[10px]",
              status ? STATUS_CLASSES[status] : "border-gx-border text-gx-muted",
            )}
          >
            {status ? STATUS_LABELS[status] : "Draft"}
          </span>
        </div>
        {(status === "running" || status === "completed") && data.outputSummary && (
          <div className="mt-2 max-w-full truncate text-[10px] leading-relaxed text-gx-muted">
            {data.outputSummary.length > 120 ? `${data.outputSummary.slice(0, 120)}...` : data.outputSummary}
          </div>
        )}
      </div>
    );
  }

  if (node.type === "approval_gate") {
    const approvedLabel = node.approvalGate?.approvedLabel || "Approved";
    const rejectedLabel = node.approvalGate?.rejectedLabel || "Rejected";
    return (
      <div
        className={clsx(
          "w-[210px] rounded-[20px] border bg-gx-surface p-3 text-xs shadow-sm",
          data.selected ? "border-gx-accent ring-2 ring-gx-accent/15" : "border-gx-border",
        )}
      >
        <Handle type="target" position={Position.Left} className="!bg-gx-border" />
        <div className="mb-3 flex items-center gap-2">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gx-accent/20 text-gx-accent">
            <Award size={15} />
          </div>
          <div className="min-w-0 flex-1">
            <div className="truncate font-medium text-gx-text">{node.title || "Approval Gate"}</div>
            <div className="truncate text-[10px] text-gx-muted">Approval Gate</div>
          </div>
        </div>
        <div className="space-y-1.5">
          <div
            className="relative flex h-8 items-center rounded-lg border border-gx-success/30 bg-gx-success/5 px-3 pr-5"
            title="Route when approved"
          >
            <Check size={13} className="mr-2 shrink-0 text-gx-success" />
            <span className="truncate text-[11px] font-medium text-gx-text">{approvedLabel}</span>
            <Handle
              id="approved"
              type="source"
              position={Position.Right}
              className="!right-[-7px] !h-3 !w-3 !border-2 !border-gx-surface !bg-gx-success"
            />
          </div>
          <div
            className="relative flex h-8 items-center rounded-lg border border-gx-error/30 bg-gx-error/5 px-3 pr-5"
            title="Route when rejected"
          >
            <X size={13} className="mr-2 shrink-0 text-gx-error" />
            <span className="truncate text-[11px] font-medium text-gx-text">{rejectedLabel}</span>
            <Handle
              id="rejected"
              type="source"
              position={Position.Right}
              className="!right-[-7px] !h-3 !w-3 !border-2 !border-gx-surface !bg-gx-error"
            />
          </div>
        </div>
        <div className="mt-3 flex items-center gap-1.5">
          <span
            className={clsx(
              "rounded border px-1.5 py-0 text-[10px]",
              status ? STATUS_CLASSES[status] : "border-gx-border text-gx-muted",
            )}
          >
            {status ? STATUS_LABELS[status] : "Draft"}
          </span>
        </div>
        {(status === "running" || status === "completed") && data.outputSummary && (
          <div className="mt-2 max-w-full truncate text-[10px] leading-relaxed text-gx-muted">
            {data.outputSummary.length > 120 ? `${data.outputSummary.slice(0, 120)}...` : data.outputSummary}
          </div>
        )}
      </div>
    );
  }

  return (
    <div
      className={clsx(
        "w-[210px] rounded border bg-gx-surface px-3 py-2 text-xs shadow-sm",
        data.selected ? "border-gx-accent" : "border-gx-border",
      )}
    >
      <Handle type="target" position={Position.Left} className="!bg-gx-border" />
      <div className="flex items-start gap-2">
        <Icon size={15} className="mt-0.5 shrink-0 text-gx-accent" />
        <div className="min-w-0 flex-1">
          <div className="truncate font-medium text-gx-text">{node.title || NODE_TYPE_LABELS[node.type]}</div>
          <div className="mt-0.5 truncate text-[10px] text-gx-muted">{NODE_TYPE_LABELS[node.type]}</div>
        </div>
      </div>
      <div className="mt-2 flex items-center gap-1.5">
        <span
          className={clsx(
            "rounded border px-1.5 py-0 text-[10px]",
            status ? STATUS_CLASSES[status] : "border-gx-border text-gx-muted",
          )}
        >
          {status ? STATUS_LABELS[status] : "Draft"}
        </span>
        {node.agentType && (
          <span className="truncate text-[10px] text-gx-muted">{node.agentType}</span>
        )}
      </div>
      <Handle type="source" position={Position.Right} className="!bg-gx-border" />
      {(status === "running" || status === "completed") && data.outputSummary && (
        <div className="mt-2 max-w-full truncate text-[10px] leading-relaxed text-gx-muted">
          {data.outputSummary.length > 120 ? `${data.outputSummary.slice(0, 120)}...` : data.outputSummary}
        </div>
      )}
    </div>
  );
});

const nodeTypes: NodeTypes = { workflowNode: WorkflowNode };
const edgeTypes: EdgeTypes = { workflowEdge: WorkflowEdge };

export function AgentWorkflowBuilderView({ repo }: AgentWorkflowBuilderViewProps) {
  return (
    <ReactFlowProvider>
      <AgentWorkflowBuilderInner repo={repo} />
    </ReactFlowProvider>
  );
}

function AgentWorkflowBuilderInner({ repo }: AgentWorkflowBuilderViewProps) {
  const [definitions, setDefinitions] = useState<AgentWorkflowDefinition[]>([]);
  const [definition, setDefinition] = useState<AgentWorkflowDefinition | null>(null);
  const [selectedNodeIds, setSelectedNodeIds] = useState<Set<string>>(new Set());
  const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);
  const [clipboard, setClipboard] = useState<AgentWorkflowNode[] | null>(null);
  const { push, undo, redo, canUndo, canRedo } = useUndoRedo();
  const [activeRun, setActiveRun] = useState<AgentWorkflowRun | null>(null);
  const [runs, setRuns] = useState<AgentWorkflowRun[]>([]);
  const [events, setEvents] = useState<AgentWorkflowEvent[]>([]);
  const [triggers, setTriggers] = useState<AgentWorkflowTrigger[]>([]);
  const [validation, setValidation] = useState<AgentWorkflowValidationResult | null>(null);
  const [statusMessage, setStatusMessage] = useState<string>("");
  const [isSaving, setIsSaving] = useState(false);
  const [reviewTextByNodeRun, setReviewTextByNodeRun] = useState<Record<string, string>>({});
  const [navigationStack, setNavigationStack] = useState<WorkflowBreadcrumb[]>([]);
  const [propertiesOpen, setPropertiesOpen] = useState(() => readBooleanSetting(LS_PROPERTIES_OPEN_KEY, true));
  const [runPaneOpen, setRunPaneOpen] = useState(() => readBooleanSetting(LS_RUN_OPEN_KEY, true));
  const [schedulesOpen, setSchedulesOpen] = useState(() => readBooleanSetting(LS_SCHEDULES_OPEN_KEY, true));
  const [workflowQuery, setWorkflowQuery] = useState("");
  const [runInputDraft, setRunInputDraft] = useState("{}");
  const [runInputError, setRunInputError] = useState<string | null>(null);
  const [showVersionHistory, setShowVersionHistory] = useState(false);
  const [showTemplatePicker, setShowTemplatePicker] = useState(false);
  const [showSimulation, setShowSimulation] = useState(false);
  const [simulationResult, setSimulationResult] = useState<AgentWorkflowSimulationResult | null>(null);
  const [simulating, setSimulating] = useState(false);
  const [simulationInput, setSimulationInput] = useState("{}");
  const [simulationError, setSimulationError] = useState<string | null>(null);
  const templates = useMemo(() => createWorkflowTemplates(), []);
  const [versionHistory, setVersionHistory] = useState<AgentWorkflowVersionMeta[]>([]);
  const [versionHistoryLoading, setVersionHistoryLoading] = useState(false);
  const [versionHistoryError, setVersionHistoryError] = useState<string | null>(null);

  const handleLoadVersionHistory = useCallback(async () => {
    if (!definition) return;
    setVersionHistoryLoading(true);
    setVersionHistoryError(null);
    try {
      const result = await window.crab.agentWorkflows.listVersions(definition.id);
      if (result.success) {
        setVersionHistory(result.versions);
      } else {
        setVersionHistoryError(result.error ?? "Failed to load version history.");
      }
    } catch (err: unknown) {
      setVersionHistoryError(err instanceof Error ? err.message : String(err));
    } finally {
      setVersionHistoryLoading(false);
    }
  }, [definition]);

  const handleViewVersion = useCallback(async (version: number) => {
    if (!definition) return;
    try {
      const result = await window.crab.agentWorkflows.getVersion(definition.id, version);
      if (result.success && result.definition) {
        setDefinition(result.definition);
        setSelectedNodeIds(new Set());
        setSelectedEdgeId(null);
        setValidation(null);
        setShowVersionHistory(false);
        setStatusMessage(`Viewing version ${version}. Save to persist or revert again.`);
      }
    } catch (err: unknown) {
      setStatusMessage(`Error loading version: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, [definition]);

  const handleRevertToVersion = useCallback(async (version: number) => {
    if (!definition || !repo.path) return;
    try {
      const result = await window.crab.agentWorkflows.revertToVersion(repo.path, definition.id, version);
      if (result.success && result.definition) {
        setDefinition(result.definition);
        setSelectedNodeIds(new Set());
        setSelectedEdgeId(null);
        setValidation(null);
        setShowVersionHistory(false);
        setStatusMessage(`Reverted to version ${version}.`);
        const defsResult = await window.crab.agentWorkflows.listDefinitions(repo.path);
        if (defsResult.success) setDefinitions(defsResult.definitions);
      } else {
        setStatusMessage(`Failed to revert: ${result.error ?? "Unknown error"}`);
      }
    } catch (err: unknown) {
      setStatusMessage(`Error reverting: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, [definition, repo.path]);

  const handleSimulate = useCallback(async () => {
    if (!definition) return;
    setSimulating(true);
    setSimulationError(null);
    setSimulationResult(null);
    setShowSimulation(true);
    try {
      let input: Record<string, unknown> | undefined;
      try { input = JSON.parse(simulationInput); } catch { input = undefined; }
      const result = await window.crab.agentWorkflows.simulateRun(definition, input);
      if (result.success && result.result) {
        setSimulationResult(result.result);
      } else {
        setSimulationError(result.error ?? "Simulation failed.");
      }
    } catch (err: unknown) {
      setSimulationError(err instanceof Error ? err.message : String(err));
    } finally {
      setSimulating(false);
    }
  }, [definition, simulationInput]);

  const handleShowVersionHistory = useCallback(() => {
    setShowVersionHistory(true);
    handleLoadVersionHistory();
  }, [handleLoadVersionHistory]);

  const initialNodes = useMemo(() =>
    definition ? buildGraphNodes(definition, activeRun, selectedNodeIds) : [],
  [definition, activeRun, selectedNodeIds]);
  const initialEdges = useMemo(() => definition ? buildGraphEdges(definition, activeRun, selectedEdgeId) : [], [activeRun, definition, selectedEdgeId]);
  const [nodes, setNodes, onNodesChange] = useNodesState<WorkflowGraphNode>(initialNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);
  const updateNodeInternals = useUpdateNodeInternals();

  const childWorkflowDefinitionIds = useMemo(() => collectChildWorkflowDefinitionIds(definitions), [definitions]);
  const rootDefinitions = useMemo(
    () => definitions.filter((candidate) => !childWorkflowDefinitionIds.has(candidate.id)),
    [childWorkflowDefinitionIds, definitions],
  );
  const rootSelectorDefinitions = useMemo(() => {
    if (!definition || navigationStack.length > 0 || childWorkflowDefinitionIds.has(definition.id)) {
      return rootDefinitions;
    }
    if (rootDefinitions.some((candidate) => candidate.id === definition.id)) {
      return rootDefinitions;
    }
    return [definition, ...rootDefinitions];
  }, [childWorkflowDefinitionIds, definition, navigationStack.length, rootDefinitions]);
  const displayedRootDefinitions = useMemo(() => {
    const query = workflowQuery.trim().toLowerCase();
    const filtered = query
      ? rootSelectorDefinitions.filter((candidate) =>
      candidate.name.toLowerCase().includes(query)
      || (candidate.description ?? "").toLowerCase().includes(query)
      || (candidate.tags ?? []).some((tag) => tag.toLowerCase().includes(query)),
      )
      : rootSelectorDefinitions;
    if (definition && rootSelectorDefinitions.some((candidate) => candidate.id === definition.id) && !filtered.some((candidate) => candidate.id === definition.id)) {
      return [definition, ...filtered];
    }
    return filtered;
  }, [definition, rootSelectorDefinitions, workflowQuery]);
  const rootSelectorValue = useMemo(() => {
    if (navigationStack.length > 0) return navigationStack[0]?.definitionId ?? "";
    if (!definition) return "";
    if (rootSelectorDefinitions.some((candidate) => candidate.id === definition.id)) return definition.id;
    return rootSelectorDefinitions[0]?.id ?? "";
  }, [definition, navigationStack, rootSelectorDefinitions]);

  const firstSelectedNodeId = useMemo(() => {
    for (const id of selectedNodeIds) return id;
    return null;
  }, [selectedNodeIds]);
  const selectedNode = useMemo(
    () => (firstSelectedNodeId ? definition?.nodes.find((node) => node.id === firstSelectedNodeId) ?? null : null),
    [definition, firstSelectedNodeId],
  );
  const selectedEdge = useMemo(
    () => definition?.edges.find((edge) => edge.id === selectedEdgeId) ?? null,
    [definition, selectedEdgeId],
  );

  const ifElseHandleSignature = useMemo(() => (
    definition?.nodes
      .filter((node) => node.type === "if_else")
      .map((node) => `${node.id}:${ensureIfElseBranches(node).map((branch) => branch.id).join(",")}`)
      .join("|") ?? ""
  ), [definition?.nodes]);

  useEffect(() => {
    if (!definition) return;
    for (const node of definition.nodes) {
      if (node.type === "if_else") updateNodeInternals(node.id);
    }
  }, [definition, ifElseHandleSignature, updateNodeInternals]);

  const latestRun = useMemo(() => {
    if (activeRun) return activeRun;
    return runs.find((run) => run.definitionId === definition?.id) ?? null;
  }, [activeRun, definition?.id, runs]);

  const refreshDefinitions = useCallback(async () => {
    if (!repo.path) return;
    const result = await window.crab.agentWorkflows.listDefinitions(repo.path);
    if (!result.success) {
      setStatusMessage(result.error ?? "Failed to load workflows.");
      return;
    }
    setDefinitions(result.definitions);
    const rootDefinitions = getRootWorkflowDefinitions(result.definitions);
    const firstDefinition = rootDefinitions[0] ?? result.definitions[0] ?? createDefaultDefinition();
    setDefinition((current) => current ?? firstDefinition);
    const firstNodeId = firstDefinition.nodes[0]?.id;
    setSelectedNodeIds((current) => current.size === 0 && firstNodeId ? new Set([firstNodeId]) : current);
  }, [repo.path]);

  const refreshRuns = useCallback(async () => {
    if (!repo.path) return;
    const result = await window.crab.agentWorkflows.listRuns(repo.path);
    if (result.success) {
      setRuns(result.runs);
      setActiveRun((current) => {
        if (!current) return result.runs[0] ?? null;
        return result.runs.find((run) => run.id === current.id) ?? current;
      });
    }
  }, [repo.path]);

  const refreshTriggers = useCallback(async () => {
    if (!repo.path) return;
    const result = await window.crab.agentWorkflows.listTriggers(repo.path);
    if (result.success) {
      setTriggers(result.triggers);
    }
  }, [repo.path]);

  useEffect(() => {
    refreshDefinitions();
    refreshRuns();
    refreshTriggers();
  }, [refreshDefinitions, refreshRuns, refreshTriggers]);

  useEffect(() => {
    persistBooleanSetting(LS_PROPERTIES_OPEN_KEY, propertiesOpen);
  }, [propertiesOpen]);

  useEffect(() => {
    persistBooleanSetting(LS_RUN_OPEN_KEY, runPaneOpen);
  }, [runPaneOpen]);

  useEffect(() => {
    persistBooleanSetting(LS_SCHEDULES_OPEN_KEY, schedulesOpen);
  }, [schedulesOpen]);

  useEffect(() => {
    const offRun = window.crab.agentWorkflows.onRunUpdated((run) => {
      if (run.repoPath !== repo.path) return;
      setRuns((current) => [run, ...current.filter((candidate) => candidate.id !== run.id)]);
      setActiveRun((current) => current?.id === run.id || run.definitionId === definition?.id ? run : current);
    });
    const offEvent = window.crab.agentWorkflows.onNodeEvent((event) => {
      setEvents((current) => [event, ...current.filter((candidate) => candidate.id !== event.id)].slice(0, 80));
    });
    const offTrigger = window.crab.agentWorkflows.onTriggerEvent((event) => {
      setEvents((current) => [event, ...current.filter((candidate) => candidate.id !== event.id)].slice(0, 80));
      refreshTriggers();
    });
    return () => {
      offRun();
      offEvent();
      offTrigger();
    };
  }, [definition?.id, refreshTriggers, repo.path]);

  useEffect(() => {
    setNodes(initialNodes);
    setEdges(initialEdges);
  }, [initialNodes, initialEdges, setNodes, setEdges]);

  useEffect(() => {
    if (!latestRun) {
      setEvents([]);
      return;
    }
    window.crab.agentWorkflows.listEvents(latestRun.id).then((result) => {
      if (result.success) {
        setEvents(result.events.slice().reverse());
      }
    });
  }, [latestRun?.id]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement) return;
      const meta = e.ctrlKey || e.metaKey;

      // Undo: Ctrl+Z
      if (meta && e.key === "z" && !e.shiftKey) {
        e.preventDefault();
        if (!definition) return;
        const state = undo({ definition, selectedNodeIds: [...selectedNodeIds], selectedEdgeId, label: "" });
        if (state) {
          setDefinition(state.definition);
          setSelectedNodeIds(new Set(state.selectedNodeIds));
          setSelectedEdgeId(state.selectedEdgeId);
          setValidation(null);
        }
      }
      // Redo: Ctrl+Shift+Z or Ctrl+Y
      if (meta && (e.key === "Z" || e.key === "y" || (e.key === "z" && e.shiftKey))) {
        e.preventDefault();
        if (!definition) return;
        const state = redo({ definition, selectedNodeIds: [...selectedNodeIds], selectedEdgeId, label: "" });
        if (state) {
          setDefinition(state.definition);
          setSelectedNodeIds(new Set(state.selectedNodeIds));
          setSelectedEdgeId(state.selectedEdgeId);
          setValidation(null);
        }
      }
      // Copy: Ctrl+C
      if (meta && e.key === "c" && !e.shiftKey && selectedNodeIds.size > 0 && definition) {
        e.preventDefault();
        const nodes = definition.nodes.filter((n) => selectedNodeIds.has(n.id));
        setClipboard(nodes.map((n) => ({ ...n })));
      }
      // Paste: Ctrl+V
      if (meta && e.key === "v" && !e.shiftKey && clipboard && clipboard.length > 0) {
        e.preventDefault();
        handlePasteNodes();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [definition, selectedNodeIds, selectedEdgeId, undo, redo, clipboard]);

  const updateDefinition = useCallback((updater: (current: AgentWorkflowDefinition) => AgentWorkflowDefinition) => {
    setDefinition((current) => current ? updater(current) : current);
    setValidation(null);
  }, []);

  const withHistory = useCallback((label: string, mutation: () => void) => {
    if (definition) {
      push({ definition, selectedNodeIds: [...selectedNodeIds], selectedEdgeId, label });
    }
    mutation();
  }, [definition, selectedNodeIds, selectedEdgeId, push]);

  const handleSelectDefinition = useCallback((definitionId: string) => {
    const next = definitions.find((candidate) => candidate.id === definitionId);
    if (!next) return;
    setDefinition(next);
    const firstId = next.nodes[0]?.id;
    setSelectedNodeIds(firstId ? new Set([firstId]) : new Set());
    setSelectedEdgeId(null);
    setActiveRun(runs.find((run) => run.definitionId === next.id) ?? null);
    setValidation(null);
    setNavigationStack([]);
  }, [definitions, runs]);

  const handleNewWorkflow = useCallback(() => {
    setShowTemplatePicker(true);
  }, []);

  const handleCreateFromTemplate = useCallback((template: WorkflowTemplate | null) => {
    const next = template ? template.create() : createDefaultDefinition();
    setDefinition(next);
    const firstId = next.nodes[0]?.id;
    setSelectedNodeIds(firstId ? new Set([firstId]) : new Set());
    setSelectedEdgeId(null);
    setActiveRun(null);
    setValidation(null);
    setNavigationStack([]);
    setStatusMessage(template ? `Created from template: ${template.name}.` : "Created a draft workflow.");
    setShowTemplatePicker(false);
  }, []);

  const handleDuplicateWorkflow = useCallback(() => {
    if (!definition) return;
    const now = Date.now();
    const idMap = new Map(definition.nodes.map((node) => [node.id, createId("node")]));
    const duplicated: AgentWorkflowDefinition = {
      ...definition,
      id: createId("workflow"),
      name: `${definition.name} Copy`,
      definitionVersion: 1,
      revisionId: createId("revision"),
      nodes: definition.nodes.map((node) => ({ ...node, id: idMap.get(node.id) ?? createId("node") })),
      edges: definition.edges.map((edge) => ({
        ...edge,
        id: createId("edge"),
        sourceNodeId: idMap.get(edge.sourceNodeId) ?? edge.sourceNodeId,
        targetNodeId: idMap.get(edge.targetNodeId) ?? edge.targetNodeId,
      })),
      layout: Object.fromEntries(Object.entries(definition.layout).map(([nodeId, position]) => [
        idMap.get(nodeId) ?? nodeId,
        { x: position.x + 36, y: position.y + 36 },
      ])),
      createdAt: now,
      updatedAt: now,
    };
    setDefinition(duplicated);
    setDefinitions((current) => [duplicated, ...current]);
    const dupFirstId = duplicated.nodes[0]?.id;
    setSelectedNodeIds(dupFirstId ? new Set([dupFirstId]) : new Set());
    setSelectedEdgeId(null);
    setActiveRun(null);
    setValidation(null);
    setNavigationStack([]);
    setStatusMessage("Duplicated workflow as a draft.");
  }, [definition]);

  const handleDeleteWorkflow = useCallback(async () => {
    if (!repo.path || !definition) return;
    const referencing = definitions.filter((candidate) =>
      candidate.id !== definition.id
      && candidate.nodes.some((node) => node.subWorkflowDefinitionId === definition.id),
    );
    if (referencing.length > 0) {
      setStatusMessage(`Cannot delete: referenced by ${referencing.map((item) => item.name).join(", ")}.`);
      return;
    }
    if (!window.confirm(`Delete workflow "${definition.name}"? This cannot be undone.`)) return;
    const result = await window.crab.agentWorkflows.deleteDefinition(repo.path, definition.id);
    if (!result.success) {
      setStatusMessage(result.error ?? "Workflow delete failed.");
      return;
    }
    const remaining = definitions.filter((candidate) => candidate.id !== definition.id);
    const next = getRootWorkflowDefinitions(remaining)[0] ?? remaining[0] ?? createDefaultDefinition();
    setDefinitions(remaining);
    setDefinition(next);
    const nextFirstId = next.nodes[0]?.id;
    setSelectedNodeIds(nextFirstId ? new Set([nextFirstId]) : new Set());
    setSelectedEdgeId(null);
    setActiveRun(runs.find((run) => run.definitionId === next.id) ?? null);
    setValidation(null);
    setNavigationStack([]);
    setStatusMessage("Workflow deleted.");
  }, [definition, definitions, repo.path, runs]);

  const saveDefinitionDraft = useCallback(async (
    draft: AgentWorkflowDefinition | null = definition,
    options: { validateFirst?: boolean; status?: string } = {},
  ): Promise<AgentWorkflowDefinition | null> => {
    if (!repo.path || !draft) return null;
    if (options.validateFirst) {
      const validationResult = await window.crab.agentWorkflows.validateDefinition(draft);
      setValidation(validationResult);
      if (!validationResult.valid) {
        setStatusMessage("Fix validation errors before navigating.");
        return null;
      }
    }
    setIsSaving(true);
    const result = await window.crab.agentWorkflows.saveDefinition(repo.path, draft);
    setIsSaving(false);
    if (!result.success || !result.definition) {
      setStatusMessage(result.error ?? "Save failed.");
      return null;
    }
    setDefinition(result.definition);
    setDefinitions((current) => [result.definition!, ...current.filter((candidate) => candidate.id !== result.definition!.id)]);
    setStatusMessage(options.status ?? "Workflow saved.");
    return result.definition;
  }, [definition, repo.path]);

  const handleSave = useCallback(async () => {
    await saveDefinitionDraft();
  }, [saveDefinitionDraft]);

  const handleValidate = useCallback(async () => {
    if (!definition) return;
    const result = await window.crab.agentWorkflows.validateDefinition(definition);
    setValidation(result);
    setStatusMessage(result.valid ? "Workflow graph is valid." : "Workflow graph has validation errors.");
  }, [definition]);

  const handleCopyNodes = useCallback(() => {
    if (!definition || selectedNodeIds.size === 0) return;
    const nodes = definition.nodes.filter((n) => selectedNodeIds.has(n.id));
    setClipboard(nodes.map((n) => ({ ...n })));
    setStatusMessage(`Copied ${nodes.length} node${nodes.length === 1 ? "" : "s"}.`);
  }, [definition, selectedNodeIds]);

  const handlePasteNodes = useCallback(() => {
    if (!clipboard || clipboard.length === 0) return;
    withHistory("Paste nodes", () => {
      updateDefinition((current) => {
        const idMap = new Map<string, string>();
        for (const node of clipboard) {
          idMap.set(node.id, createId("node"));
        }
        const offsetX = clipboard.length * 30;
        const offsetY = clipboard.length * 30;
        const newNodes = clipboard.map((node) => ({
          ...node,
          id: idMap.get(node.id)!,
        }));
        const newLayout = { ...current.layout };
        for (const node of clipboard) {
          const oldPos = current.layout[node.id] ?? { x: 0, y: 0 };
          newLayout[idMap.get(node.id)!] = { x: oldPos.x + offsetX, y: oldPos.y + offsetY };
        }
        const newIds = new Set(newNodes.map((n) => n.id));
        setSelectedNodeIds(newIds);
        setStatusMessage(`Pasted ${newNodes.length} node${newNodes.length === 1 ? "" : "s"}.`);
        return {
          ...current,
          nodes: [...current.nodes, ...newNodes],
          layout: newLayout,
          updatedAt: Date.now(),
        };
      });
    });
  }, [clipboard, updateDefinition, withHistory]);

  const handleRun = useCallback(async () => {
    if (!repo.path || !definition) return;
    const parsedInput = parseJsonObjectDraft(runInputDraft);
    if (parsedInput.error) {
      setRunInputError(parsedInput.error);
      setStatusMessage("Fix run input JSON before starting.");
      return;
    }
    setRunInputError(null);
    const saved = await saveDefinitionDraft(definition, { status: "Saved before run." });
    if (!saved) return;
    const result = await window.crab.agentWorkflows.startRun(repo.path, saved.id, { input: parsedInput.value });
    if (!result.success || !result.run) {
      setStatusMessage(result.error ?? "Run failed to start.");
      return;
    }
    setDefinition(saved);
    setActiveRun(result.run);
    setRuns((current) => [result.run!, ...current.filter((run) => run.id !== result.run!.id)]);
    setStatusMessage("Workflow run started.");
  }, [definition, repo.path, runInputDraft, saveDefinitionDraft]);

  const handleCancelRun = useCallback(async () => {
    if (!latestRun) return;
    const result = await window.crab.agentWorkflows.cancelRun(latestRun.id);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setStatusMessage("Workflow run stopped.");
    } else {
      setStatusMessage(result.error ?? "Failed to stop run.");
    }
  }, [latestRun]);

  const handlePauseRun = useCallback(async () => {
    if (!latestRun) return;
    const result = await window.crab.agentWorkflows.pauseRun(latestRun.id);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setStatusMessage("Workflow run paused.");
    } else {
      setStatusMessage(result.error ?? "Failed to pause run.");
    }
  }, [latestRun]);

  const handleResumeRun = useCallback(async () => {
    if (!latestRun) return;
    const result = await window.crab.agentWorkflows.resumeRun(latestRun.id);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setStatusMessage("Workflow run resumed.");
    } else {
      setStatusMessage(result.error ?? "Failed to resume run.");
    }
  }, [latestRun]);

  const handleAddNode = useCallback((type: AgentWorkflowNodeType) => {
    updateDefinition((current) => {
      const id = createId("node");
      const nextNode: AgentWorkflowNode = {
        id,
        type,
        title: NODE_TYPE_LABELS[type],
        instructions: type === "join"
          ? ""
          : type === "if_else"
            ? ""
            : type === "loop"
              ? ""
          : type === "agent_task"
            ? "Describe the agent work to perform."
            : type === "sub_workflow"
              ? "Map input into the child workflow and wait for it to finish."
              : "Describe the review or approval criteria.",
        agentType: type === "agent_task" ? current.defaultAgentType : undefined,
        planFirst: false,
        retryPolicy: type === "agent_task" ? { maxAttempts: 1 } : undefined,
        inputMapping: type === "sub_workflow" || type === "loop" ? { input: { source: "runInput" } } : undefined,
        ifElse: type === "if_else" ? { branches: createDefaultIfElseBranches() } : undefined,
        loop: type === "loop" ? { maxIterations: 3 } : undefined,
        approvalGate: type === "approval_gate" ? { approvedLabel: "Approved", rejectedLabel: "Rejected" } : undefined,
        humanReview: type === "human_review" ? {} : undefined,
      };
      setSelectedNodeIds(new Set([id]));
      setSelectedEdgeId(null);
      return {
        ...current,
        nodes: [...current.nodes, nextNode],
        layout: {
          ...current.layout,
          [id]: { x: 120 + current.nodes.length * 60, y: 120 + current.nodes.length * 50 },
        },
        updatedAt: Date.now(),
      };
    });
  }, [updateDefinition]);

  const handleDeleteNode = useCallback(() => {
    if (selectedNodeIds.size === 0) return;
    withHistory("Delete nodes", () => {
      updateDefinition((current) => {
        const nextNodes = current.nodes.filter((node) => !selectedNodeIds.has(node.id));
        const nextEdges = current.edges.filter((edge) => !selectedNodeIds.has(edge.sourceNodeId) && !selectedNodeIds.has(edge.targetNodeId));
        const nextLayout = { ...current.layout };
        for (const id of selectedNodeIds) delete nextLayout[id];
        const remaining = nextNodes[0]?.id;
        setSelectedNodeIds(remaining ? new Set([remaining]) : new Set());
        return { ...current, nodes: nextNodes, edges: nextEdges, layout: nextLayout, updatedAt: Date.now() };
      });
    });
  }, [selectedNodeIds, updateDefinition, withHistory]);

  const handleNodeChange = useCallback((patch: Partial<AgentWorkflowNode>) => {
    if (!firstSelectedNodeId) return;
    withHistory("Update node", () => {
      updateDefinition((current) => ({
        ...current,
        nodes: current.nodes.map((node) => node.id === firstSelectedNodeId ? { ...node, ...patch } : node),
        updatedAt: Date.now(),
      }));
    });
  }, [firstSelectedNodeId, updateDefinition, withHistory]);

  const handleEdgeChange = useCallback((patch: Partial<AgentWorkflowEdge>) => {
    if (!selectedEdgeId) return;
    withHistory("Update edge", () => {
      updateDefinition((current) => ({
        ...current,
        edges: current.edges.map((edge) => edge.id === selectedEdgeId ? { ...edge, ...patch } : edge),
        updatedAt: Date.now(),
      }));
    });
  }, [selectedEdgeId, updateDefinition, withHistory]);

  const handleDeleteEdge = useCallback(() => {
    if (!selectedEdgeId) return;
    withHistory("Delete edge", () => {
      updateDefinition((current) => ({
        ...current,
        edges: current.edges.filter((edge) => edge.id !== selectedEdgeId),
        updatedAt: Date.now(),
      }));
      setSelectedEdgeId(null);
    });
  }, [selectedEdgeId, updateDefinition, withHistory]);

  const onConnect = useCallback((connection: Connection) => {
    if (!connection.source || !connection.target || connection.source === connection.target) return;
    withHistory("Add edge", () => {
      updateDefinition((current) => {
        const sourceNode = current.nodes.find((node) => node.id === connection.source);
        const sourceOutgoingCount = current.edges.filter((edge) => edge.sourceNodeId === connection.source).length;
        const requestedBranchId = sourceNode?.type === "if_else"
          ? connection.sourceHandle
            ?? ensureIfElseBranches(sourceNode)[sourceOutgoingCount]?.id
            ?? ensureIfElseBranches(sourceNode).find((branch) => branch.kind === "else")?.id
            ?? "else"
          : sourceNode?.type === "approval_gate"
            ? approvalGateBranchId(connection.sourceHandle) ?? (sourceOutgoingCount === 0 ? "approved" : "rejected")
            : undefined;
        const duplicate = current.edges.some((edge) =>
          edge.sourceNodeId === connection.source
          && edge.targetNodeId === connection.target
          && (
            sourceNode?.type === "if_else" || sourceNode?.type === "approval_gate"
              ? edgeBranchId(edge.condition) === requestedBranchId
              : true
          ),
        );
        if (duplicate) return current;
        const condition: WorkflowEdgeCondition = sourceNode?.type === "if_else"
          ? branchEdgeCondition(requestedBranchId ?? "else")
          : sourceNode?.type === "approval_gate"
            ? branchEdgeCondition(requestedBranchId ?? "approved")
            : { sourceStatus: "completed" };
        const nextEdge: AgentWorkflowEdge = {
          id: createId("edge"),
          sourceNodeId: connection.source!,
          targetNodeId: connection.target!,
          condition,
        };
        setSelectedEdgeId(nextEdge.id);
        setSelectedNodeIds(new Set());
        return { ...current, edges: [...current.edges, nextEdge], updatedAt: Date.now() };
      });
    });
    setEdges((current) => addEdge(connection, current));
  }, [setEdges, updateDefinition, withHistory]);

  const handleNodeDragStop = useCallback((_event: React.MouseEvent, node: WorkflowGraphNode) => {
    withHistory("Move node", () => {
      updateDefinition((current) => ({
        ...current,
        layout: { ...current.layout, [node.id]: { x: node.position.x, y: node.position.y } },
        updatedAt: Date.now(),
      }));
    });
  }, [updateDefinition, withHistory]);

  const handleApproveNode = useCallback(async (nodeRunId: string) => {
    if (!latestRun) return;
    const result = await window.crab.agentWorkflows.approveNode(latestRun.id, nodeRunId, reviewTextByNodeRun[nodeRunId]);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setReviewTextByNodeRun((current) => ({ ...current, [nodeRunId]: "" }));
    } else {
      setStatusMessage(result.error ?? "Approval failed.");
    }
  }, [latestRun, reviewTextByNodeRun]);

  const handleRequestChanges = useCallback(async (nodeRunId: string) => {
    if (!latestRun) return;
    const feedback = reviewTextByNodeRun[nodeRunId] ?? "";
    const result = await window.crab.agentWorkflows.requestNodeChanges(latestRun.id, nodeRunId, feedback);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setReviewTextByNodeRun((current) => ({ ...current, [nodeRunId]: "" }));
    } else {
      setStatusMessage(result.error ?? "Request changes failed.");
    }
  }, [latestRun, reviewTextByNodeRun]);

  const handleRejectNode = useCallback(async (nodeRunId: string) => {
    if (!latestRun) return;
    const reason = reviewTextByNodeRun[nodeRunId] ?? "";
    const result = await window.crab.agentWorkflows.rejectNode(latestRun.id, nodeRunId, reason);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setReviewTextByNodeRun((current) => ({ ...current, [nodeRunId]: "" }));
    } else {
      setStatusMessage(result.error ?? "Reject failed.");
    }
  }, [latestRun, reviewTextByNodeRun]);

  const handleRequestHumanReviewChanges = useCallback(async (nodeRunId: string) => {
    if (!latestRun) return;
    const feedback = reviewTextByNodeRun[nodeRunId] ?? "";
    const result = await window.crab.agentWorkflows.requestHumanReviewChanges(latestRun.id, nodeRunId, feedback);
    if (result.success && result.run) {
      setActiveRun(result.run);
      setReviewTextByNodeRun((current) => ({ ...current, [nodeRunId]: "" }));
    } else {
      setStatusMessage(result.error ?? "Request changes failed.");
    }
  }, [latestRun, reviewTextByNodeRun]);

  const handleRetryNode = useCallback(async (nodeRunId: string) => {
    if (!latestRun) return;
    const result = await window.crab.agentWorkflows.retryNode(latestRun.id, nodeRunId);
    if (result.success && result.run) {
      setActiveRun(result.run);
    } else {
      setStatusMessage(result.error ?? "Retry failed.");
    }
  }, [latestRun]);

  const handleOpenAgentSession = useCallback((taskId: string) => {
    localStorage.setItem("crab.orchestrator.pendingOpenTask", JSON.stringify({ taskId, repoPath: repo.path }));
    window.dispatchEvent(new CustomEvent("crab:open-agent-task", { detail: { taskId, repoPath: repo.path } }));
  }, [repo.path]);

  const handleSaveTrigger = useCallback(async (trigger: AgentWorkflowTrigger) => {
    const result = await window.crab.agentWorkflows.saveTrigger(trigger);
    if (result.success && result.trigger) {
      setTriggers((current) => [result.trigger!, ...current.filter((item) => item.id !== result.trigger!.id)]);
      setStatusMessage("Schedule saved.");
    } else {
      setStatusMessage(result.error ?? "Schedule save failed.");
    }
  }, []);

  const handleDeleteTrigger = useCallback(async (triggerId: string) => {
    const result = await window.crab.agentWorkflows.deleteTrigger(triggerId);
    if (result.success) {
      setTriggers((current) => current.filter((item) => item.id !== triggerId));
      setStatusMessage("Schedule deleted.");
    } else {
      setStatusMessage(result.error ?? "Schedule delete failed.");
    }
  }, []);

  const handleSetTriggerEnabled = useCallback(async (triggerId: string, enabled: boolean) => {
    const result = await window.crab.agentWorkflows.setTriggerEnabled(triggerId, enabled);
    if (result.success && result.trigger) {
      setTriggers((current) => current.map((item) => item.id === triggerId ? result.trigger! : item));
    } else {
      setStatusMessage(result.error ?? "Schedule update failed.");
    }
  }, []);

  const openDefinition = useCallback((next: AgentWorkflowDefinition, selectedNode?: string) => {
    setDefinition(next);
    setDefinitions((current) => [next, ...current.filter((candidate) => candidate.id !== next.id)]);
    const openNodeId = selectedNode ?? next.nodes[0]?.id;
    setSelectedNodeIds(openNodeId ? new Set([openNodeId]) : new Set());
    setSelectedEdgeId(null);
    setActiveRun(runs.find((run) => run.definitionId === next.id) ?? null);
    setValidation(null);
  }, [runs]);

  const handleOpenSubWorkflow = useCallback(async (nodeId: string) => {
    if (!repo.path || !definition) return;
    const node = definition.nodes.find((candidate) => candidate.id === nodeId);
    if (!node || (node.type !== "sub_workflow" && node.type !== "loop")) return;
    if (!node.subWorkflowDefinitionId) {
      setStatusMessage("Create a child workflow before opening it.");
      return;
    }
    const savedParent = await saveDefinitionDraft(definition, { validateFirst: true, status: "Saved parent workflow." });
    if (!savedParent) return;
    const result = await window.crab.agentWorkflows.getDefinition(repo.path, node.subWorkflowDefinitionId);
    if (!result.success || !result.definition) {
      setStatusMessage(result.error ?? "Sub-workflow definition not found.");
      return;
    }
    setNavigationStack((current) => [...current, {
      definitionId: savedParent.id,
      nodeId,
      label: savedParent.name,
    }]);
    openDefinition(result.definition);
    setStatusMessage(`Opened sub-workflow ${result.definition.name}.`);
  }, [definition, openDefinition, repo.path, saveDefinitionDraft]);

  const handleCreateSubWorkflow = useCallback(async (nodeId: string) => {
    if (!repo.path || !definition) return;
    const node = definition.nodes.find((candidate) => candidate.id === nodeId);
    if (!node || (node.type !== "sub_workflow" && node.type !== "loop")) return;
    const child = createChildWorkflowDefinition(definition, node);
    const parentWithLink: AgentWorkflowDefinition = {
      ...definition,
      nodes: definition.nodes.map((candidate) =>
        candidate.id === nodeId ? { ...candidate, subWorkflowDefinitionId: child.id } : candidate,
      ),
      updatedAt: Date.now(),
    };
    const validationResult = await window.crab.agentWorkflows.validateDefinition(parentWithLink);
    setValidation(validationResult);
    if (!validationResult.valid) {
      setStatusMessage("Fix validation errors before creating a child workflow.");
      return;
    }
    const savedChild = await window.crab.agentWorkflows.saveDefinition(repo.path, child);
    if (!savedChild.success || !savedChild.definition) {
      setStatusMessage(savedChild.error ?? "Child workflow save failed.");
      return;
    }
    const savedParent = await saveDefinitionDraft(parentWithLink, { status: "Created child workflow." });
    if (!savedParent) return;
    setDefinitions((current) => [
      savedChild.definition!,
      savedParent,
      ...current.filter((candidate) => candidate.id !== savedChild.definition!.id && candidate.id !== savedParent.id),
    ]);
    setNavigationStack((current) => [...current, {
      definitionId: savedParent.id,
      nodeId,
      label: savedParent.name,
    }]);
    openDefinition(savedChild.definition);
    setStatusMessage(`Created sub-workflow ${savedChild.definition.name}.`);
  }, [definition, openDefinition, repo.path, saveDefinitionDraft]);

  const handleNavigateBack = useCallback(async () => {
    if (!repo.path || navigationStack.length === 0) return;
    const previous = navigationStack[navigationStack.length - 1];
    const savedCurrent = await saveDefinitionDraft(definition, { validateFirst: true, status: "Saved workflow." });
    if (!savedCurrent) return;
    const result = await window.crab.agentWorkflows.getDefinition(repo.path, previous.definitionId);
    if (!result.success || !result.definition) {
      setStatusMessage(result.error ?? "Parent workflow definition not found.");
      return;
    }
    setNavigationStack((current) => current.slice(0, -1));
    openDefinition(result.definition, previous.nodeId);
    setStatusMessage(`Returned to ${result.definition.name}.`);
  }, [definition, navigationStack, openDefinition, repo.path, saveDefinitionDraft]);

  if (!repo.path) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-gx-muted">
        Agent Workflows require a local repository path.
      </div>
    );
  }

  if (!definition) {
    return (
      <div className="flex h-full items-center justify-center">
        <button type="button" className="btn-primary" onClick={handleNewWorkflow}>
          <Plus size={14} /> New Agent Workflow
        </button>
      </div>
    );
  }

  return (
    <div className="flex h-full min-w-0 flex-col bg-gx-bg text-gx-text">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-gx-border px-3">
        {navigationStack.length > 0 && (
          <IconButton title="Back to parent workflow" onClick={handleNavigateBack} variant="ghost">
            <ChevronLeft size={14} />
          </IconButton>
        )}
        <div className="flex min-w-[360px] items-center overflow-hidden rounded border border-gx-border bg-gx-bg-sub">
          <Search size={13} className="ml-2 shrink-0 text-gx-muted" />
          <input
            className="h-7 min-w-0 flex-1 bg-transparent px-2 text-xs text-gx-text placeholder:text-gx-muted focus:outline-none"
            value={workflowQuery}
            onChange={(event) => setWorkflowQuery(event.target.value)}
            placeholder="Search workflows"
            aria-label="Search workflows"
          />
          <select
            className="h-7 max-w-[210px] border-l border-gx-border bg-gx-surface px-2 text-xs text-gx-text focus:outline-none"
            value={rootSelectorValue}
            onChange={(event) => handleSelectDefinition(event.target.value)}
            title="Root workflows"
          >
            {displayedRootDefinitions.length === 0 ? (
              <option value="">No matches</option>
            ) : (
              displayedRootDefinitions.map((item) => (
                <option key={item.id} value={item.id}>{item.name}</option>
              ))
            )}
          </select>
        </div>
        <CommandButton title="New workflow" label="New" onClick={handleNewWorkflow} variant="ghost">
          <Plus size={13} />
        </CommandButton>
        <CommandButton title="Reuse this workflow as a new draft" label="Reuse" onClick={handleDuplicateWorkflow} variant="ghost">
          <Copy size={13} />
        </CommandButton>
        <CommandButton title="Delete workflow" label="Delete" onClick={handleDeleteWorkflow} variant="danger">
          <Trash2 size={13} />
        </CommandButton>
        <CommandButton title="Undo (Ctrl+Z)" label="Undo" onClick={() => {
          if (!definition) return;
          const state = undo({ definition, selectedNodeIds: [...selectedNodeIds], selectedEdgeId, label: "" });
          if (state) {
            setDefinition(state.definition);
            setSelectedNodeIds(new Set(state.selectedNodeIds));
            setSelectedEdgeId(state.selectedEdgeId);
            setValidation(null);
          }
        }} disabled={!canUndo}>
          <Undo2 size={13} />
        </CommandButton>
        <CommandButton title="Redo (Ctrl+Shift+Z)" label="Redo" onClick={() => {
          if (!definition) return;
          const state = redo({ definition, selectedNodeIds: [...selectedNodeIds], selectedEdgeId, label: "" });
          if (state) {
            setDefinition(state.definition);
            setSelectedNodeIds(new Set(state.selectedNodeIds));
            setSelectedEdgeId(state.selectedEdgeId);
            setValidation(null);
          }
        }} disabled={!canRedo}>
          <Redo2 size={13} />
        </CommandButton>
        <CommandButton title="Copy selected nodes" label="Copy" onClick={handleCopyNodes} disabled={selectedNodeIds.size === 0}>
          <Copy size={13} />
        </CommandButton>
        <CommandButton title="Paste nodes" label="Paste" onClick={handlePasteNodes} disabled={!clipboard || clipboard.length === 0}>
          <ClipboardPaste size={13} />
        </CommandButton>
        <CommandButton title="Save workflow" label="Save" onClick={handleSave} disabled={isSaving}>
          <Save size={13} />
        </CommandButton>
        <CommandButton title="Validate DAG" label="Validate" onClick={handleValidate}>
          <GitBranch size={13} />
        </CommandButton>
        <CommandButton title="Simulate run" label="Simulate" onClick={handleSimulate} disabled={!definition}>
          <SkipForward size={13} />
        </CommandButton>
        <CommandButton title="Version history" label="History" onClick={handleShowVersionHistory}>
          <History size={13} />
        </CommandButton>
        <CommandButton title="Run workflow" label="Run" onClick={handleRun} variant="primary">
          <Play size={13} />
        </CommandButton>
        {latestRun && latestRun.status === "running" && (
          <CommandButton title="Pause run" label="Pause" onClick={handlePauseRun}>
            <Pause size={13} />
          </CommandButton>
        )}
        {latestRun && latestRun.status === "paused" && (
          <CommandButton title="Resume run" label="Resume" onClick={handleResumeRun} variant="primary">
            <Play size={13} />
          </CommandButton>
        )}
        {latestRun && ["queued", "running", "paused"].includes(latestRun.status) && (
          <CommandButton title="Stop active run" label="Stop" onClick={handleCancelRun} variant="danger">
            <CircleStop size={13} />
          </CommandButton>
        )}
        {navigationStack.length > 0 && (
          <div className="min-w-0 max-w-[260px] truncate text-[11px] text-gx-muted">
            {navigationStack.map((item) => item.label).join(" / ")} / {definition.name}
          </div>
        )}
        <div className="min-w-0 flex-1 truncate text-xs text-gx-muted">
          {statusMessage || "Define DAG nodes, connect dependencies, then run with optional review gates."}
        </div>
      </div>

      <PanelGroup direction="horizontal" className="min-h-0 flex-1">
        <Panel minSize={34} className="min-h-0 min-w-0">
          <div className="flex h-full min-w-0 flex-col">
            <div className="flex h-9 shrink-0 items-center gap-2 border-b border-gx-border px-3">
              <IconButton title="Add agent task" onClick={() => handleAddNode("agent_task")}>
                <Bot size={13} />
              </IconButton>
              <IconButton title="Add human review" onClick={() => handleAddNode("human_review")}>
                <UserCheck size={13} />
              </IconButton>
              <IconButton title="Add approval gate" onClick={() => handleAddNode("approval_gate")}>
                <Award size={13} />
              </IconButton>
              <IconButton title="Add join" onClick={() => handleAddNode("join")}>
                <Merge size={13} />
              </IconButton>
              <IconButton title="Add sub-workflow" onClick={() => handleAddNode("sub_workflow")}>
                <Network size={13} />
              </IconButton>
              <IconButton title="Add if / else" onClick={() => handleAddNode("if_else")}>
                <Split size={13} />
              </IconButton>
              <IconButton title="Add loop" onClick={() => handleAddNode("loop")}>
                <Repeat2 size={13} />
              </IconButton>
              <div className="min-w-0 flex-1 truncate text-[11px] text-gx-muted">
                Add nodes, connect edges, then use the run panel for progress.
              </div>
              {!propertiesOpen && (
                <IconButton title="Show properties" onClick={() => setPropertiesOpen(true)} variant="ghost">
                  <ChevronLeft size={14} />
                </IconButton>
              )}
              {!runPaneOpen && (
                <IconButton title="Show run panel" onClick={() => setRunPaneOpen(true)} variant="ghost">
                  <ChevronLeft size={14} />
                </IconButton>
              )}
            </div>
            <div className="min-h-0 flex-1">
              <ReactFlow
                nodes={nodes}
                edges={edges}
                nodeTypes={nodeTypes}
                edgeTypes={edgeTypes}
                onNodesChange={onNodesChange}
                onEdgesChange={onEdgesChange}
                onConnect={onConnect}
                onNodeClick={(event, node) => {
                  if (event.shiftKey || event.ctrlKey || event.metaKey) {
                    setSelectedNodeIds((prev) => {
                      const next = new Set(prev);
                      if (next.has(node.id)) next.delete(node.id); else next.add(node.id);
                      return next;
                    });
                  } else {
                    setSelectedNodeIds(new Set([node.id]));
                  }
                  setSelectedEdgeId(null);
                }}
                onNodeDoubleClick={(_, node) => {
                  const workflowNode = definition.nodes.find((candidate) => candidate.id === node.id);
                  if (workflowNode?.type === "sub_workflow" || workflowNode?.type === "loop") {
                    if (workflowNode.subWorkflowDefinitionId) {
                      handleOpenSubWorkflow(node.id);
                    } else {
                      handleCreateSubWorkflow(node.id);
                    }
                  }
                }}
                onEdgeClick={(_, edge) => {
                  setSelectedEdgeId(edge.id);
                  setSelectedNodeIds(new Set());
                }}
                onPaneClick={() => {
                  setSelectedNodeIds(new Set());
                  setSelectedEdgeId(null);
                }}
                selectNodesOnDrag
                onNodeDragStop={handleNodeDragStop}
                fitView
                proOptions={{ hideAttribution: true }}
              >
                <Background color="rgba(148, 163, 184, 0.18)" gap={18} />
                <Controls />
                <MiniMap pannable zoomable nodeStrokeWidth={2} />
              </ReactFlow>
            </div>
          </div>
        </Panel>

        {propertiesOpen ? (
          <>
            <ResizeHandle />
            <Panel
              defaultSize={readPanelSize(LS_PROPERTIES_SIZE_KEY, 20)}
              minSize={16}
              maxSize={34}
              className="min-h-0"
              onResize={(size) => persistPanelSize(LS_PROPERTIES_SIZE_KEY, size)}
            >
              <div className="flex h-full min-h-0 flex-col border-l border-gx-border">
                <PropertiesPanel
                  definition={definition}
                  definitions={definitions}
                  selectedNode={selectedNode}
                  selectedEdge={selectedEdge}
                  validation={validation}
                  onDefinitionChange={updateDefinition}
                  onNodeChange={handleNodeChange}
                  onEdgeChange={handleEdgeChange}
                  onDeleteNode={handleDeleteNode}
                  onDeleteEdge={handleDeleteEdge}
                  onClose={() => setPropertiesOpen(false)}
                  onOpenSubWorkflow={handleOpenSubWorkflow}
                  onCreateSubWorkflow={handleCreateSubWorkflow}
                />
              </div>
            </Panel>
          </>
        ) : (
          <CollapsedPanelRail title="Show properties" side="right" icon={<ChevronLeft size={14} />} onOpen={() => setPropertiesOpen(true)} />
        )}

        {runPaneOpen ? (
          <>
            <ResizeHandle />
            <Panel
              defaultSize={readPanelSize(LS_RUN_SIZE_KEY, 24)}
              minSize={20}
              maxSize={42}
              className="min-h-0"
              onResize={(size) => persistPanelSize(LS_RUN_SIZE_KEY, size)}
            >
              <div className="flex h-full min-h-0 flex-col border-l border-gx-border">
                {schedulesOpen ? (
                  <PanelGroup direction="vertical" className="min-h-0 flex-1">
                    <Panel
                      defaultSize={readPanelSize(LS_SCHEDULES_SIZE_KEY, 28)}
                      minSize={14}
                      maxSize={48}
                      onResize={(size) => persistPanelSize(LS_SCHEDULES_SIZE_KEY, size)}
                    >
                      <TriggerPanel
                        repoPath={repo.path}
                        definition={definition}
                        triggers={triggers.filter((trigger) => trigger.workflowDefinitionId === definition.id)}
                        onSaveTrigger={handleSaveTrigger}
                        onDeleteTrigger={handleDeleteTrigger}
                        onSetTriggerEnabled={handleSetTriggerEnabled}
                        onClose={() => setSchedulesOpen(false)}
                      />
                    </Panel>
                    <ResizeHandle direction="vertical" />
                    <Panel minSize={32} className="min-h-0">
                      <RunPanel
                        run={latestRun}
                        runs={runs.filter((run) => run.definitionId === definition.id)}
                        definition={definition}
                        events={events}
                        activeRunId={latestRun?.id ?? null}
                        onSelectRun={(run) => setActiveRun(run)}
                        runInputDraft={runInputDraft}
                        runInputError={runInputError}
                        onRunInputDraftChange={setRunInputDraft}
                        onRun={handleRun}
                        reviewTextByNodeRun={reviewTextByNodeRun}
                        onReviewTextChange={setReviewTextByNodeRun}
                        onApproveNode={handleApproveNode}
                        onRequestChanges={handleRequestChanges}
                        onRejectNode={handleRejectNode}
                        onRetryNode={handleRetryNode}
                        onRequestHumanReviewChanges={handleRequestHumanReviewChanges}
                        onClosePanel={() => setRunPaneOpen(false)}
                        onToggleSchedules={() => setSchedulesOpen((open) => !open)}
                        schedulesOpen={schedulesOpen}
                        onOpenSubWorkflow={handleOpenSubWorkflow}
                        onCreateSubWorkflow={handleCreateSubWorkflow}
                        onOpenAgentSession={handleOpenAgentSession}
                      />
                    </Panel>
                  </PanelGroup>
                ) : (
                  <>
                    <div className="flex h-8 shrink-0 items-center gap-2 border-b border-gx-border px-3">
                      <Clock size={13} className="text-gx-muted" />
                      <span className="flex-1 text-xs font-medium">Schedules</span>
                      <IconButton title="Show schedules" onClick={() => setSchedulesOpen(true)} variant="ghost" className="h-6 w-6">
                        <ChevronDown size={13} />
                      </IconButton>
                    </div>
                    <RunPanel
                      run={latestRun}
                      runs={runs.filter((run) => run.definitionId === definition.id)}
                      definition={definition}
                      events={events}
                      activeRunId={latestRun?.id ?? null}
                      onSelectRun={(run) => setActiveRun(run)}
                      runInputDraft={runInputDraft}
                      runInputError={runInputError}
                      onRunInputDraftChange={setRunInputDraft}
                      onRun={handleRun}
                      reviewTextByNodeRun={reviewTextByNodeRun}
                      onReviewTextChange={setReviewTextByNodeRun}
                      onApproveNode={handleApproveNode}
                      onRequestChanges={handleRequestChanges}
                      onRejectNode={handleRejectNode}
                      onRetryNode={handleRetryNode}
                      onRequestHumanReviewChanges={handleRequestHumanReviewChanges}
                      onClosePanel={() => setRunPaneOpen(false)}
                      onToggleSchedules={() => setSchedulesOpen((open) => !open)}
                      schedulesOpen={schedulesOpen}
                      onOpenSubWorkflow={handleOpenSubWorkflow}
                      onCreateSubWorkflow={handleCreateSubWorkflow}
                      onOpenAgentSession={handleOpenAgentSession}
                    />
                  </>
                )}
              </div>
            </Panel>
          </>
        ) : (
          <CollapsedPanelRail title="Show run panel" side="right" icon={<ChevronLeft size={14} />} onOpen={() => setRunPaneOpen(true)} />
        )}
      </PanelGroup>
      {showSimulation && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40" onClick={() => setShowSimulation(false)}>
          <div className="w-full max-w-xl rounded-lg border border-gx-border bg-gx-bg p-4 shadow-xl" onClick={(event) => event.stopPropagation()}>
            <div className="mb-3 flex items-center justify-between">
              <h3 className="text-sm font-semibold text-gx-text">Dry Run Simulation</h3>
              <IconButton title="Close" onClick={() => setShowSimulation(false)}>
                <X size={14} />
              </IconButton>
            </div>
            {!simulationResult && !simulationError && (
              <>
                <label className="mb-2 block space-y-1 text-[11px] text-gx-muted">
                  <span>Simulation input (JSON)</span>
                  <textarea
                    className="input-field w-full resize-none font-mono text-xs"
                    rows={4}
                    value={simulationInput}
                    onChange={(event) => setSimulationInput(event.target.value)}
                    placeholder='{"prompt": "test"}'
                  />
                </label>
                <button type="button" className="btn-primary w-full text-xs" onClick={handleSimulate} disabled={simulating}>
                  {simulating ? "Simulating..." : "Run Simulation"}
                </button>
              </>
            )}
            {simulating && (
              <div className="py-8 text-center text-xs text-gx-muted">Simulating workflow...</div>
            )}
            {simulationError && (
              <div className="space-y-2">
                <div className="rounded border border-gx-error/30 bg-gx-error/5 px-3 py-2 text-xs text-gx-error">{simulationError}</div>
                <button type="button" className="btn-primary w-full text-xs" onClick={() => { setSimulationError(null); setSimulationResult(null); }}>
                  Try Again
                </button>
              </div>
            )}
            {simulationResult && !simulating && (
              <div className="space-y-3">
                <div className="grid grid-cols-2 gap-2">
                  <div className="rounded border border-gx-success/30 bg-gx-success/5 px-3 py-2 text-center">
                    <div className="text-lg font-bold text-gx-success">{simulationResult.activatedNodes.length}</div>
                    <div className="text-[10px] text-gx-muted">Activated</div>
                  </div>
                  <div className="rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-center">
                    <div className="text-lg font-bold text-gx-text">{simulationResult.skippedNodes.length}</div>
                    <div className="text-[10px] text-gx-muted">Skipped</div>
                  </div>
                </div>
                {simulationResult.blockedNodes.length > 0 && (
                  <div className="rounded border border-gx-error/30 bg-gx-error/5 px-3 py-2 text-center">
                    <div className="text-lg font-bold text-gx-error">{simulationResult.blockedNodes.length}</div>
                    <div className="text-[10px] text-gx-muted">Blocked</div>
                  </div>
                )}
                <div className="max-h-60 space-y-1 overflow-auto rounded border border-gx-border p-2">
                  {simulationResult.trace.map((line, index) => (
                    <div key={index} className="text-[10px] leading-relaxed text-gx-muted">{line}</div>
                  ))}
                </div>
                <div className="flex gap-2">
                  <button type="button" className="btn-primary flex-1 text-xs" onClick={() => { setSimulationError(null); setSimulationResult(null); }}>
                    Simulate Again
                  </button>
                  <button type="button" className="flex-1 rounded border border-gx-border px-3 py-2 text-xs text-gx-muted hover:bg-gx-bg-sub" onClick={() => setShowSimulation(false)}>
                    Close
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>
      )}
      {showTemplatePicker && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40" onClick={() => setShowTemplatePicker(false)}>
          <div className="w-full max-w-lg rounded-lg border border-gx-border bg-gx-bg p-4 shadow-xl" onClick={(event) => event.stopPropagation()}>
            <div className="mb-3 flex items-center justify-between">
              <h3 className="text-sm font-semibold text-gx-text">New Workflow</h3>
              <IconButton title="Close" onClick={() => setShowTemplatePicker(false)}>
                <X size={14} />
              </IconButton>
            </div>
            <div className="mb-3 grid gap-2">
              {templates.map((template) => (
                <button
                  key={template.id}
                  type="button"
                  className="flex items-start gap-3 rounded border border-gx-border p-3 text-left hover:border-gx-accent/60 hover:bg-gx-accent/5"
                  onClick={() => handleCreateFromTemplate(template)}
                >
                  <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-gx-accent/10 text-gx-accent">
                    <Network size={15} />
                  </div>
                  <div className="min-w-0">
                    <div className="text-xs font-medium text-gx-text">{template.name}</div>
                    <div className="mt-0.5 text-[11px] text-gx-muted">{template.description}</div>
                  </div>
                </button>
              ))}
            </div>
            <button
              type="button"
              className="w-full rounded border border-gx-border px-3 py-2 text-xs text-gx-muted hover:bg-gx-bg-sub"
              onClick={() => handleCreateFromTemplate(null)}
            >
              Start from scratch
            </button>
          </div>
        </div>
      )}
      {showVersionHistory && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40" onClick={() => setShowVersionHistory(false)}>
          <div className="w-full max-w-md rounded-lg border border-gx-border bg-gx-bg p-4 shadow-xl" onClick={(event) => event.stopPropagation()}>
            <div className="mb-3 flex items-center justify-between">
              <h3 className="text-sm font-semibold text-gx-text">Version History</h3>
              <IconButton title="Close" onClick={() => setShowVersionHistory(false)}>
                <X size={14} />
              </IconButton>
            </div>
            {versionHistoryLoading && (
              <div className="py-8 text-center text-xs text-gx-muted">Loading versions...</div>
            )}
            {versionHistoryError && (
              <div className="py-4 text-center text-xs text-gx-error">{versionHistoryError}</div>
            )}
            {!versionHistoryLoading && !versionHistoryError && versionHistory.length === 0 && (
              <div className="py-8 text-center text-xs text-gx-muted">No version history yet. Save the workflow to create versions.</div>
            )}
            {!versionHistoryLoading && versionHistory.length > 0 && (
              <div className="max-h-80 space-y-1 overflow-auto">
                {versionHistory.map((meta) => (
                  <div key={meta.version} className="flex items-center justify-between rounded border border-gx-border px-3 py-2">
                    <div className="min-w-0">
                      <div className="truncate text-xs text-gx-text">v{meta.version} — {meta.name}</div>
                      <div className="text-[10px] text-gx-muted">{new Date(meta.timestamp).toLocaleString()}</div>
                    </div>
                    <div className="flex shrink-0 items-center gap-1">
                      <button
                        type="button"
                        className="rounded px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/10"
                        onClick={() => handleViewVersion(meta.version)}
                      >
                        View
                      </button>
                      <button
                        type="button"
                        className="rounded px-2 py-1 text-[10px] text-gx-warn hover:bg-gx-warn/10"
                        onClick={() => handleRevertToVersion(meta.version)}
                      >
                        Revert
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function PropertiesPanel({
  definition,
  definitions,
  selectedNode,
  selectedEdge,
  validation,
  onDefinitionChange,
  onNodeChange,
  onEdgeChange,
  onDeleteNode,
  onDeleteEdge,
  onClose,
  onOpenSubWorkflow,
  onCreateSubWorkflow,
}: {
  definition: AgentWorkflowDefinition;
  definitions: AgentWorkflowDefinition[];
  selectedNode: AgentWorkflowNode | null;
  selectedEdge: AgentWorkflowEdge | null;
  validation: AgentWorkflowValidationResult | null;
  onDefinitionChange: (updater: (current: AgentWorkflowDefinition) => AgentWorkflowDefinition) => void;
  onNodeChange: (patch: Partial<AgentWorkflowNode>) => void;
  onEdgeChange: (patch: Partial<AgentWorkflowEdge>) => void;
  onDeleteNode: () => void;
  onDeleteEdge: () => void;
  onClose: () => void;
  onOpenSubWorkflow: (nodeId: string) => void;
  onCreateSubWorkflow: (nodeId: string) => void;
}) {
  const [schemaBuilderOpen, setSchemaBuilderOpen] = useState(false);
  const ifElseInputSuggestions = useMemo(
    () => selectedNode?.type === "if_else" ? buildIfElseInputSuggestions(definition, selectedNode) : [],
    [definition, selectedNode],
  );
  const childWorkflowOptions = useMemo(
    () => definitions.filter((item) => item.id !== definition.id),
    [definition.id, definitions],
  );
  const activeAgentType = selectedNode?.type === "agent_task"
    ? selectedNode.agentType ?? definition.defaultAgentType
    : definition.defaultAgentType;
  const modelOptions = useMemo(() => {
    return modelOptionsForAgent(activeAgentType, selectedNode?.model);
  }, [activeAgentType, selectedNode?.model]);
  const intelligenceOptions = useMemo(() => {
    return intelligenceOptionsForModel(activeAgentType, selectedNode?.model, selectedNode?.intelligence);
  }, [activeAgentType, selectedNode?.intelligence, selectedNode?.model]);
  const handleAgentTypeChange = useCallback((agentTypeValue: string) => {
    const nextAgentType = (agentTypeValue || undefined) as AgentWorkflowAgentType | undefined;
    const resolvedAgentType = nextAgentType ?? definition.defaultAgentType;
    const currentModel = selectedNode?.model?.trim();
    const currentIntelligence = selectedNode?.intelligence?.trim();
    const patch: Partial<AgentWorkflowNode> = { agentType: nextAgentType };
    if (currentModel && !isModelCompatibleWithAgent(resolvedAgentType, currentModel)) {
      patch.model = undefined;
      patch.intelligence = undefined;
    } else if (currentIntelligence && !isIntelligenceCompatibleWithModel(resolvedAgentType, currentModel, currentIntelligence)) {
      patch.intelligence = undefined;
    }
    onNodeChange(patch);
  }, [definition.defaultAgentType, onNodeChange, selectedNode?.intelligence, selectedNode?.model]);
  const handleModelChange = useCallback((modelValue: string) => {
    const nextModel = modelValue || undefined;
    const currentIntelligence = selectedNode?.intelligence?.trim();
    const patch: Partial<AgentWorkflowNode> = { model: nextModel };
    if (currentIntelligence && !isIntelligenceCompatibleWithModel(activeAgentType, nextModel, currentIntelligence)) {
      patch.intelligence = undefined;
    }
    onNodeChange(patch);
  }, [activeAgentType, onNodeChange, selectedNode?.intelligence]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-9 items-center gap-2 border-b border-gx-border px-3 text-xs font-medium">
        <span className="flex-1">Properties</span>
        <IconButton title="Hide properties" onClick={onClose} variant="ghost" className="h-6 w-6">
          <ChevronRight size={13} />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 space-y-4 overflow-auto p-3">
        <section className="space-y-2">
          <div className="flex items-center gap-2 text-xs font-medium text-gx-text">
            <GitBranch size={13} className="text-gx-accent" />
            Workflow Metadata
          </div>
          <label className="block text-[11px] text-gx-muted">Name</label>
          <input
            className="input-field w-full text-xs font-medium"
            value={definition.name}
            onChange={(event) => onDefinitionChange((current) => ({ ...current, name: event.target.value, updatedAt: Date.now() }))}
          />
          <label className="block text-[11px] text-gx-muted">Description</label>
          <textarea
            className="input-field min-h-16 w-full resize-none text-xs"
            value={definition.description ?? ""}
            onChange={(event) => onDefinitionChange((current) => ({ ...current, description: event.target.value, updatedAt: Date.now() }))}
          />
          <label className="block text-[11px] text-gx-muted">Tags</label>
          <div className="flex items-center gap-1.5">
            <Tag size={13} className="shrink-0 text-gx-muted" />
            <input
              className="input-field min-w-0 flex-1 text-xs"
              value={(definition.tags ?? []).join(", ")}
              placeholder="release, refactor, qa"
              onChange={(event) => onDefinitionChange((current) => ({
                ...current,
                tags: event.target.value.split(",").map((tag) => tag.trim()).filter(Boolean),
                updatedAt: Date.now(),
              }))}
            />
          </div>
          <div className="grid grid-cols-2 gap-2">
            <label className="space-y-1 text-[11px] text-gx-muted">
              <span>Max parallel</span>
              <input
                type="number"
                min={1}
                className="input-field w-full text-xs"
                value={definition.maxParallelNodes}
                onChange={(event) => onDefinitionChange((current) => ({
                  ...current,
                  maxParallelNodes: Math.max(1, Number(event.target.value) || 1),
                  updatedAt: Date.now(),
                }))}
              />
            </label>
            <label className="space-y-1 text-[11px] text-gx-muted">
              <span>Default agent</span>
              <select
                className="input-field w-full text-xs"
                value={definition.defaultAgentType}
                onChange={(event) => onDefinitionChange((current) => ({
                  ...current,
                  defaultAgentType: event.target.value as AgentWorkflowAgentType,
                  updatedAt: Date.now(),
                }))}
              >
                <option value="codex">Codex</option>
                <option value="claude-code">Claude Code</option>
                <option value="custom">Custom</option>
              </select>
            </label>
          </div>
        </section>

        {selectedNode && (
          <section className="space-y-2 border-t border-gx-border pt-3">
            <div className="flex items-center gap-2">
              <div className="text-xs font-medium">Selected Node</div>
              <div className="flex-1" />
              <button type="button" className="btn-icon h-6 w-6 text-gx-error" onClick={onDeleteNode} title="Delete node">
                <Trash2 size={13} />
              </button>
            </div>
            <label className="block text-[11px] text-gx-muted">Title</label>
            <input className="input-field w-full text-xs" value={selectedNode.title} onChange={(event) => onNodeChange({ title: event.target.value })} />
            <label className="block text-[11px] text-gx-muted">Type</label>
            <select
              className="input-field w-full text-xs"
              value={selectedNode.type}
              onChange={(event) => {
                const type = event.target.value as AgentWorkflowNodeType;
                onNodeChange({
                  type,
                  ifElse: type === "if_else" ? selectedNode.ifElse ?? { branches: createDefaultIfElseBranches() } : selectedNode.ifElse,
                  loop: type === "loop" ? selectedNode.loop ?? { maxIterations: 3 } : selectedNode.loop,
                  inputMapping: type === "sub_workflow" || type === "loop"
                    ? selectedNode.inputMapping ?? { input: { source: "runInput" } }
                    : selectedNode.inputMapping,
                });
              }}
            >
              <option value="agent_task">Agent Task</option>
              <option value="human_review">Human Review</option>
              <option value="approval_gate">Approval Gate</option>
              <option value="join">Join</option>
              <option value="sub_workflow">Sub-Workflow</option>
              <option value="if_else">If / Else</option>
              <option value="loop">Loop</option>
            </select>
            <label className="block text-[11px] text-gx-muted">Instructions</label>
            <textarea
              className="input-field min-h-32 w-full resize-none text-xs"
              value={selectedNode.instructions}
              onChange={(event) => onNodeChange({ instructions: event.target.value })}
            />
            {selectedNode.type === "agent_task" && (
              <div className="space-y-2">
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Agent</span>
                    <select
                      className="input-field w-full text-xs"
                      value={selectedNode.agentType ?? ""}
                      onChange={(event) => handleAgentTypeChange(event.target.value)}
                    >
                      <option value="">Default</option>
                      <option value="codex">Codex</option>
                      <option value="claude-code">Claude Code</option>
                      <option value="custom">Custom</option>
                    </select>
                  </label>
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Retries</span>
                    <input
                      type="number"
                      min={1}
                      className="input-field w-full text-xs"
                      value={selectedNode.retryPolicy?.maxAttempts ?? 1}
                      onChange={(event) => onNodeChange({ retryPolicy: { maxAttempts: Math.max(1, Number(event.target.value) || 1) } })}
                    />
                  </label>
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Timeout (ms)</span>
                    <input
                      type="number"
                      min={0}
                      className="input-field w-full text-xs"
                      value={selectedNode.timeoutMs ?? ""}
                      placeholder="No timeout"
                      onChange={(event) => {
                        const val = event.target.value;
                        onNodeChange({ timeoutMs: val ? Math.max(0, Number(val) || 0) : undefined });
                      }}
                    />
                  </label>
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <ToggleCard
                    label="Plan first"
                    description="Pause for an implementation plan before edits."
                    checked={selectedNode.planFirst ?? false}
                    onChange={(planFirst) => onNodeChange({ planFirst })}
                  />
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Model</span>
                    <select
                      className="input-field w-full text-xs"
                      value={selectedNode.model?.trim() ?? ""}
                      onChange={(event) => handleModelChange(event.target.value)}
                    >
                      <option value="">Agent default</option>
                      {modelOptions.map((model) => (
                        <option key={model.value} value={model.value} title={model.description}>
                          {model.label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Intelligence</span>
                    <select
                      className="input-field w-full text-xs"
                      value={selectedNode.intelligence?.trim() ?? ""}
                      onChange={(event) => onNodeChange({ intelligence: event.target.value || undefined })}
                    >
                      <option value="">Agent default</option>
                      {intelligenceOptions.map((level) => (
                        <option key={level} value={level}>
                          {formatIntelligenceLabel(level)}
                        </option>
                      ))}
                    </select>
                  </label>
                </div>
              </div>
            )}
            {selectedNode.type === "if_else" && (
              <div className="space-y-2 rounded border border-gx-border p-2">
                <div className="text-[11px] font-medium text-gx-text">Branches</div>
                <IfElseBranchEditor
                  branches={ensureIfElseBranches(selectedNode)}
                  inputSuggestions={ifElseInputSuggestions}
                  onChange={(branches) => {
                    const validBranchIds = new Set(branches.map((branch) => branch.id));
                    onDefinitionChange((current) => ({
                      ...current,
                      nodes: current.nodes.map((node) => node.id === selectedNode.id ? { ...node, ifElse: { branches } } : node),
                      edges: current.edges.filter((edge) =>
                        edge.sourceNodeId !== selectedNode.id
                        || !edgeBranchId(edge.condition)
                        || validBranchIds.has(edgeBranchId(edge.condition)!),
                      ),
                      updatedAt: Date.now(),
                    }));
                  }}
                />
              </div>
            )}
            {selectedNode.type === "join" && (
              <div className="space-y-2 border-t border-gx-border pt-3">
                <label className="block space-y-1 text-[11px] text-gx-muted">
                  <span>Merge strategy</span>
                  <select
                    className="input-field w-full text-xs"
                    value={selectedNode.joinConfig?.strategy ?? "all"}
                    onChange={(event) => {
                      const strategy = event.target.value as JoinStrategy | "all";
                      onNodeChange({
                        joinConfig: { ...selectedNode.joinConfig, strategy: strategy === "all" ? undefined : strategy },
                      });
                    }}
                  >
                    <option value="all">All (collect outputs)</option>
                    <option value="first">First (first activated input)</option>
                    <option value="last">Last (last activated input)</option>
                    <option value="concat">Concat (merge with template)</option>
                  </select>
                </label>
                {(selectedNode.joinConfig?.strategy === "concat") && (
                  <label className="block space-y-1 text-[11px] text-gx-muted">
                    <span>Merge template</span>
                    <textarea
                      className="input-field w-full resize-none text-xs"
                      rows={3}
                      value={selectedNode.joinConfig?.customTemplate ?? "{{summaries}}"}
                      placeholder="{{summaries}}"
                      onChange={(event) => {
                        const val = event.target.value;
                        onNodeChange({
                          joinConfig: { ...selectedNode.joinConfig, customTemplate: val || undefined },
                        });
                      }}
                    />
                  </label>
                )}
              </div>
            )}
            {(selectedNode.type === "sub_workflow" || selectedNode.type === "loop") && (
              <div className="space-y-2">
                <label className="block space-y-1 text-[11px] text-gx-muted">
                  <span>Reusable workflow</span>
                  <select
                    className="input-field w-full text-xs"
                    value={selectedNode.subWorkflowDefinitionId ?? ""}
                    onChange={(event) => onNodeChange({ subWorkflowDefinitionId: event.target.value || undefined })}
                  >
                    <option value="">Select or create a reusable workflow</option>
                    {selectedNode.subWorkflowDefinitionId && !childWorkflowOptions.some((item) => item.id === selectedNode.subWorkflowDefinitionId) && (
                      <option value={selectedNode.subWorkflowDefinitionId}>Unknown workflow ({selectedNode.subWorkflowDefinitionId})</option>
                    )}
                    {childWorkflowOptions.map((item) => (
                      <option key={item.id} value={item.id}>{item.name}</option>
                    ))}
                  </select>
                </label>
                <div className="flex items-center gap-1.5 rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5">
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[11px] text-gx-text">
                      {childWorkflowOptions.find((item) => item.id === selectedNode.subWorkflowDefinitionId)?.name
                        ?? (selectedNode.subWorkflowDefinitionId ? "Linked reusable workflow" : "No reusable workflow selected")}
                    </div>
                    <div className="truncate font-mono text-[10px] text-gx-muted">
                      {selectedNode.subWorkflowDefinitionId || "Select an existing workflow or create one for nested steps."}
                    </div>
                  </div>
                  {selectedNode.subWorkflowDefinitionId ? (
                    <IconButton title="Open reusable workflow" onClick={() => onOpenSubWorkflow(selectedNode.id)} variant="ghost">
                      <ExternalLink size={13} />
                    </IconButton>
                  ) : (
                    <IconButton title="Create reusable workflow" onClick={() => onCreateSubWorkflow(selectedNode.id)} variant="ghost">
                      <FolderPlus size={13} />
                    </IconButton>
                  )}
                </div>
                {selectedNode.type === "loop" && (
                  <div className="space-y-2 rounded border border-gx-border p-2">
                    <LoopConfigEditor
                      loop={selectedNode.loop ?? { maxIterations: 3 }}
                      onChange={(loop) => onNodeChange({ loop })}
                    />
                  </div>
                )}
                <ValueRefMapEditor
                  label="Child input mapping"
                  value={selectedNode.inputMapping ?? {}}
                  emptyLabel="No child inputs mapped. The child receives an empty input object."
                  newKeyPrefix="input"
                  onChange={(inputMapping) => onNodeChange({ inputMapping })}
                />
                <ValueRefMapEditor
                  label="Parent output mapping"
                  value={selectedNode.outputMapping ?? {}}
                  emptyLabel="No parent output mapping. The parent receives the child run summary."
                  newKeyPrefix="output"
                  onChange={(outputMapping) => onNodeChange({ outputMapping })}
                />
              </div>
            )}
            {selectedNode.type === "approval_gate" && (
              <div className="space-y-2 border-t border-gx-border pt-3">
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Approved label</span>
                    <input
                      className="input-field w-full text-xs"
                      value={selectedNode.approvalGate?.approvedLabel ?? "Approved"}
                      onChange={(event) => onNodeChange({
                        approvalGate: { ...selectedNode.approvalGate, approvedLabel: event.target.value },
                      })}
                    />
                  </label>
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Rejected label</span>
                    <input
                      className="input-field w-full text-xs"
                      value={selectedNode.approvalGate?.rejectedLabel ?? "Rejected"}
                      onChange={(event) => onNodeChange({
                        approvalGate: { ...selectedNode.approvalGate, rejectedLabel: event.target.value },
                      })}
                    />
                  </label>
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Review timeout (ms)</span>
                    <input
                      type="number"
                      min={0}
                      className="input-field w-full text-xs"
                      value={selectedNode.approvalGate?.reviewTimeoutMs ?? ""}
                      placeholder="No timeout"
                      onChange={(event) => {
                        const val = event.target.value;
                        const num = val ? Math.max(0, Number(val) || 0) : undefined;
                        onNodeChange({
                          approvalGate: { ...selectedNode.approvalGate, reviewTimeoutMs: num },
                        });
                      }}
                    />
                  </label>
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>On timeout</span>
                    <select
                      className="input-field w-full text-xs"
                      value={selectedNode.approvalGate?.onReviewTimeout ?? "fail"}
                      onChange={(event) => {
                        const val = event.target.value || undefined;
                        onNodeChange({
                          approvalGate: { ...selectedNode.approvalGate, onReviewTimeout: val as "approve" | "reject" | "fail" | undefined },
                        });
                      }}
                    >
                      <option value="approve">Auto-approve</option>
                      <option value="reject">Auto-reject</option>
                      <option value="fail">Fail</option>
                    </select>
                  </label>
                </div>
              </div>
            )}
            {selectedNode.type === "human_review" && (
              <div className="space-y-2 border-t border-gx-border pt-3">
                <label className="block space-y-1 text-[11px] text-gx-muted">
                  <span>Review target agent</span>
                  <select
                    className="input-field w-full text-xs"
                    value={selectedNode.humanReview?.reviewTargetNodeId ?? ""}
                    onChange={(event) => onNodeChange({
                      humanReview: { ...selectedNode.humanReview, reviewTargetNodeId: event.target.value || undefined },
                    })}
                  >
                    <option value="">Auto-detect upstream agent</option>
                    {definition?.nodes
                      .filter((n) => n.type === "agent_task" && n.id !== selectedNode.id)
                      .map((n) => (
                        <option key={n.id} value={n.id}>{n.title}</option>
                      ))}
                  </select>
                </label>
                <div className="grid grid-cols-2 gap-2">
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>Review timeout (ms)</span>
                    <input
                      type="number"
                      min={0}
                      className="input-field w-full text-xs"
                      value={selectedNode.humanReview?.reviewTimeoutMs ?? ""}
                      placeholder="No timeout"
                      onChange={(event) => {
                        const val = event.target.value;
                        const num = val ? Math.max(0, Number(val) || 0) : undefined;
                        onNodeChange({
                          humanReview: { ...selectedNode.humanReview, reviewTimeoutMs: num },
                        });
                      }}
                    />
                  </label>
                  <label className="space-y-1 text-[11px] text-gx-muted">
                    <span>On timeout</span>
                    <select
                      className="input-field w-full text-xs"
                      value={selectedNode.humanReview?.onReviewTimeout ?? "fail"}
                      onChange={(event) => {
                        const val = event.target.value || undefined;
                        onNodeChange({
                          humanReview: { ...selectedNode.humanReview, onReviewTimeout: val as "approve" | "reject" | "fail" | undefined },
                        });
                      }}
                    >
                      <option value="approve">Auto-approve</option>
                      <option value="reject">Auto-reject</option>
                      <option value="fail">Fail</option>
                    </select>
                  </label>
                </div>
              </div>
            )}
            <div className="space-y-2 border-t border-gx-border pt-3">
              <ValueRefMapEditor
                label="State updates"
                value={selectedNode.stateUpdates ?? {}}
                emptyLabel="No workflow state updates."
                newKeyPrefix="state"
                onChange={(stateUpdates) => onNodeChange({ stateUpdates })}
              />
            </div>
            <div className="space-y-2 border-t border-gx-border pt-3">
              <label className="flex items-center gap-2 text-xs text-gx-text">
                <input
                  type="checkbox"
                  checked={selectedNode.requireStructuredOutput ?? false}
                  onChange={(event) => onNodeChange({ requireStructuredOutput: event.target.checked })}
                />
                Require structured output
              </label>
              <div className="space-y-1 text-[11px] text-gx-muted">
                <span>Output schema</span>
                <button
                  type="button"
                  className="flex w-full items-center justify-between rounded border border-gx-border bg-gx-bg-sub px-2 py-2 text-left text-xs text-gx-text hover:border-gx-accent/60"
                  onClick={() => setSchemaBuilderOpen(true)}
                >
                  <span className="truncate">{selectedNode.outputSchemaName || schemaTitle(selectedNode.outputSchema) || "schema"}</span>
                  <span className="font-mono text-[10px] text-gx-muted">
                    {selectedNode.outputSchema ? `${Object.keys((selectedNode.outputSchema.properties as Record<string, unknown> | undefined) ?? {}).length} fields` : "{}"}
                  </span>
                </button>
              </div>
              <JsonSchemaBuilderDialog
                open={schemaBuilderOpen}
                schema={selectedNode.outputSchema}
                schemaName={selectedNode.outputSchemaName}
                onClose={() => setSchemaBuilderOpen(false)}
                onApply={(outputSchema, outputSchemaName) => onNodeChange({ outputSchema, outputSchemaName })}
              />
            </div>
          </section>
        )}

        {selectedEdge && (
          <section className="space-y-2 border-t border-gx-border pt-3">
            <div className="flex items-center gap-2">
              <div className="text-xs font-medium">Selected Edge</div>
              <div className="flex-1" />
              <button type="button" className="btn-icon h-6 w-6 text-gx-error" onClick={onDeleteEdge} title="Delete edge">
                <Trash2 size={13} />
              </button>
            </div>
            <EdgeConditionEditor
              edge={selectedEdge}
              onChange={(condition) => onEdgeChange({ condition })}
            />
          </section>
        )}

        {validation && (
          <section className="space-y-2 border-t border-gx-border pt-3 text-xs">
            <div className={validation.valid ? "text-gx-success" : "text-gx-error"}>
              {validation.valid ? "Valid DAG" : "Validation errors"}
            </div>
            {validation.errors.map((error) => (
              <div key={error} className="text-gx-error">{error}</div>
            ))}
            {validation.warnings.map((warning) => (
              <div key={warning} className="text-gx-warn">{warning}</div>
            ))}
          </section>
        )}
      </div>
    </div>
  );
}

function ToggleCard({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      className={clsx(
        "min-h-[58px] rounded border px-2 py-2 text-left transition focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent focus-visible:outline-offset-1",
        checked
          ? "border-gx-accent/60 bg-gx-accent/10 text-gx-text"
          : "border-gx-border bg-gx-bg-sub text-gx-muted hover:border-gx-accent/50 hover:text-gx-text",
      )}
      onClick={() => onChange(!checked)}
    >
      <span className="flex items-center justify-between gap-2 text-xs font-medium">
        <span className="truncate">{label}</span>
        <span
          className={clsx(
            "h-3 w-6 rounded-full border transition",
            checked ? "border-gx-accent bg-gx-accent/60" : "border-gx-border bg-gx-surface",
          )}
        >
          <span
            className={clsx(
              "block h-2.5 w-2.5 rounded-full bg-gx-text transition-transform",
              checked && "translate-x-3",
            )}
          />
        </span>
      </span>
      <span className="mt-1 block text-[10px] leading-4 text-gx-muted">{description}</span>
    </button>
  );
}

function schemaTitle(schema: Record<string, unknown> | undefined): string | undefined {
  return typeof schema?.title === "string" ? schema.title : undefined;
}

function buildIfElseInputSuggestions(
  definition: AgentWorkflowDefinition,
  node: AgentWorkflowNode,
): CelInputSuggestion[] {
  const incomingEdges = definition.edges.filter((edge) => edge.targetNodeId === node.id);
  const keyedInput = incomingEdges.length > 1;
  const suggestions: CelInputSuggestion[] = [];

  for (const edge of incomingEdges) {
    const sourceNode = definition.nodes.find((candidate) => candidate.id === edge.sourceNodeId);
    if (!sourceNode?.outputSchema) continue;
    const rootPath = keyedInput ? celPath("input", [sourceNode.id]) : "input";
    const displayPrefix = keyedInput ? `${sourceNode.title || sourceNode.id}.` : "";
    collectSchemaSuggestions({
      schema: sourceNode.outputSchema,
      rootPath,
      displayPrefix,
      sourceNode,
      suggestions,
      segments: [],
    });
  }

  return suggestions;
}

function collectSchemaSuggestions({
  schema,
  rootPath,
  displayPrefix,
  sourceNode,
  suggestions,
  segments,
}: {
  schema: Record<string, unknown>;
  rootPath: string;
  displayPrefix: string;
  sourceNode: AgentWorkflowNode;
  suggestions: CelInputSuggestion[];
  segments: string[];
}): void {
  const properties = schemaProperties(schema);
  if (!properties) return;
  for (const [name, propertySchema] of Object.entries(properties)) {
    if (!isSchemaObject(propertySchema)) continue;
    const nextSegments = [...segments, name];
    const type = schemaSuggestionType(propertySchema);
    const path = celPath(rootPath, nextSegments);
    const displayPath = `${displayPrefix}${nextSegments.join(".")}`;
    suggestions.push({
      id: `${sourceNode.id}:${nextSegments.join(".")}`,
      path,
      displayPath,
      fieldName: name,
      type,
      sourceNodeId: sourceNode.id,
      sourceTitle: sourceNode.title || sourceNode.id,
      description: typeof propertySchema.description === "string" ? propertySchema.description : undefined,
      enumValues: Array.isArray(propertySchema.enum)
        ? propertySchema.enum.filter((value): value is string => typeof value === "string")
        : undefined,
    });
    if (type === "object") {
      collectSchemaSuggestions({
        schema: propertySchema,
        rootPath,
        displayPrefix,
        sourceNode,
        suggestions,
        segments: nextSegments,
      });
    }
  }
}

function schemaProperties(schema: Record<string, unknown>): Record<string, unknown> | null {
  return isSchemaObject(schema.properties) ? schema.properties : null;
}

function schemaSuggestionType(schema: Record<string, unknown>): CelSuggestionType {
  if (Array.isArray(schema.enum) && schema.enum.length > 0) return "enum";
  if (schema.type === "string") return "string";
  if (schema.type === "number" || schema.type === "integer") return "number";
  if (schema.type === "boolean") return "boolean";
  if (schema.type === "object") return "object";
  if (schema.type === "array") return "array";
  return "unknown";
}

function isSchemaObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function celPath(root: string, segments: string[]): string {
  return segments.reduce((path, segment) => (
    /^[A-Za-z_][A-Za-z0-9_]*$/.test(segment)
      ? `${path}.${segment}`
      : `${path}[${JSON.stringify(segment)}]`
  ), root);
}

function IfElseBranchEditor({
  branches,
  inputSuggestions,
  onChange,
}: {
  branches: WorkflowIfElseBranch[];
  inputSuggestions: CelInputSuggestion[];
  onChange: (branches: WorkflowIfElseBranch[]) => void;
}) {
  const addElif = () => {
    const elseIndex = branches.findIndex((branch) => branch.kind === "else");
    const label = `ELIF ${branches.filter((branch) => branch.kind === "elif").length + 1}`;
    const next: WorkflowIfElseBranch = {
      id: createId("elif"),
      label,
      kind: "elif",
      celExpression: defaultCelExpression(inputSuggestions, label),
    };
    const insertAt = elseIndex >= 0 ? elseIndex : branches.length;
    onChange([...branches.slice(0, insertAt), next, ...branches.slice(insertAt)]);
  };
  const updateBranchExpression = (branchId: string, celExpression: string) => {
    onChange(branches.map((item) => item.id === branchId ? {
      ...item,
      celExpression,
      condition: undefined,
    } : item));
  };

  return (
    <div className="space-y-2">
      {branches.map((branch) => (
        <div key={branch.id} className="space-y-2 rounded border border-gx-border bg-gx-bg-sub/40 p-2">
          <div className="flex items-center gap-2">
            <span className={clsx(
              "rounded border px-1.5 py-0 text-[10px] font-medium",
              branch.kind === "else" ? "border-gx-muted/40 text-gx-muted" : "border-gx-accent/40 text-gx-accent",
            )}>
              {branch.kind.toUpperCase()}
            </span>
            <input
              className="input-field h-6 min-w-0 flex-1 text-[11px]"
              value={branch.label}
              onChange={(event) => onChange(branches.map((item) => item.id === branch.id ? { ...item, label: event.target.value } : item))}
            />
            {branch.kind === "elif" && (
              <IconButton
                title="Delete branch"
                variant="ghost"
                className="h-6 w-6 text-gx-error"
                onClick={() => onChange(branches.filter((item) => item.id !== branch.id))}
              >
                <Trash2 size={12} />
              </IconButton>
            )}
          </div>
          {branch.kind !== "else" ? (
            <>
              <CelConditionAssistant
                suggestions={inputSuggestions}
                expression={branch.celExpression ?? legacyConditionExpression(branch.condition)}
                branchLabel={branch.label}
                onExpressionChange={(celExpression) => updateBranchExpression(branch.id, celExpression)}
              />
              <label className="block space-y-1 text-[11px] text-gx-muted">
                <span>CEL expression</span>
                <textarea
                  className="input-field min-h-16 w-full resize-y font-mono text-[11px]"
                  value={branch.celExpression ?? legacyConditionExpression(branch.condition)}
                  onChange={(event) => updateBranchExpression(branch.id, event.target.value)}
                  placeholder="input.score >= 0.8"
                />
              </label>
            </>
          ) : (
            <div className="text-[10px] text-gx-muted">Fallback branch when no earlier branch matches.</div>
          )}
          <div className="font-mono text-[10px] text-gx-muted">route handle: {branch.id}</div>
        </div>
      ))}
      <button type="button" className="text-xs text-gx-accent hover:underline" onClick={addElif}>
        + Add branch
      </button>
    </div>
  );
}

function CelConditionAssistant({
  suggestions,
  expression,
  branchLabel,
  onExpressionChange,
}: {
  suggestions: CelInputSuggestion[];
  expression: string;
  branchLabel: string;
  onExpressionChange: (expression: string) => void;
}) {
  const expressionSuggestion = useMemo(
    () => suggestions.find((suggestion) => expression.includes(suggestion.path)),
    [expression, suggestions],
  );
  const initialSuggestion = expressionSuggestion ?? selectDefaultCelSuggestion(suggestions, branchLabel);
  const [selectedId, setSelectedId] = useState(initialSuggestion?.id ?? "");
  const selectedSuggestion = suggestions.find((suggestion) => suggestion.id === selectedId) ?? initialSuggestion ?? null;
  const [operator, setOperator] = useState<CelAssistantOperator>(() =>
    selectedSuggestion ? defaultOperatorForSuggestion(selectedSuggestion, branchLabel) : "exists",
  );
  const [valueDraft, setValueDraft] = useState(() =>
    selectedSuggestion ? defaultValueForSuggestion(selectedSuggestion, branchLabel) : "",
  );

  useEffect(() => {
    const next = expressionSuggestion ?? selectDefaultCelSuggestion(suggestions, branchLabel);
    if (!next) {
      setSelectedId("");
      return;
    }
    setSelectedId((current) => current && suggestions.some((suggestion) => suggestion.id === current) ? current : next.id);
  }, [branchLabel, expressionSuggestion, suggestions]);

  useEffect(() => {
    if (!selectedSuggestion) return;
    const options = CEL_ASSISTANT_OPERATORS[selectedSuggestion.type];
    if (!options.some((option) => option.value === operator)) {
      setOperator(defaultOperatorForSuggestion(selectedSuggestion, branchLabel));
      setValueDraft(defaultValueForSuggestion(selectedSuggestion, branchLabel));
    }
  }, [branchLabel, operator, selectedSuggestion]);

  if (suggestions.length === 0) {
    return (
      <div className="border-t border-gx-border/70 pt-2 text-[10px] text-gx-muted">
        Connect an upstream node with an output schema to build this condition visually, or write CEL directly below.
      </div>
    );
  }

  if (!selectedSuggestion) return null;
  const operatorOptions = CEL_ASSISTANT_OPERATORS[selectedSuggestion.type];
  const nextExpression = buildCelExpression(selectedSuggestion, operator, valueDraft);

  const chooseSuggestion = (suggestion: CelInputSuggestion, apply = false) => {
    const nextOperator = defaultOperatorForSuggestion(suggestion, branchLabel);
    const nextValue = defaultValueForSuggestion(suggestion, branchLabel);
    setSelectedId(suggestion.id);
    setOperator(nextOperator);
    setValueDraft(nextValue);
    if (apply) onExpressionChange(buildCelExpression(suggestion, nextOperator, nextValue));
  };

  return (
    <div className="space-y-2 border-t border-gx-border/70 pt-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[11px] text-gx-muted">Condition builder</span>
        <span className="truncate text-[10px] text-gx-muted">{selectedSuggestion.sourceTitle}</span>
      </div>
      <div className="flex flex-wrap gap-1">
        {suggestions.slice(0, 8).map((suggestion) => (
          <button
            key={suggestion.id}
            type="button"
            title={suggestion.description || suggestion.path}
            className={clsx(
              "max-w-full rounded border px-1.5 py-0.5 text-[10px] transition",
              selectedSuggestion.id === suggestion.id
                ? "border-gx-accent/60 bg-gx-accent/10 text-gx-accent"
                : "border-gx-border bg-gx-bg-sub text-gx-muted hover:border-gx-accent/50 hover:text-gx-text",
            )}
            onClick={() => chooseSuggestion(suggestion, true)}
          >
            <span className="font-mono">{suggestion.displayPath}</span>
            <span className="ml-1 uppercase opacity-70">{suggestion.type}</span>
          </button>
        ))}
      </div>
      <div className="space-y-1.5">
        <select
          className="input-field h-7 w-full font-mono text-[11px]"
          value={selectedSuggestion.id}
          onChange={(event) => {
            const suggestion = suggestions.find((item) => item.id === event.target.value);
            if (suggestion) chooseSuggestion(suggestion);
          }}
        >
          {suggestions.map((suggestion) => (
            <option key={suggestion.id} value={suggestion.id}>
              {suggestion.displayPath} ({suggestion.type})
            </option>
          ))}
        </select>
        <div className="grid grid-cols-[92px_1fr_52px] gap-1.5">
          <select
            className="input-field h-7 min-w-0 text-[11px]"
            value={operator}
            onChange={(event) => setOperator(event.target.value as CelAssistantOperator)}
          >
            {operatorOptions.map((option) => (
              <option key={option.value} value={option.value}>{option.label}</option>
            ))}
          </select>
          <CelConditionValueInput
            suggestion={selectedSuggestion}
            operator={operator}
            value={valueDraft}
            onChange={setValueDraft}
          />
          <button
            type="button"
            className="btn h-7 px-2 text-[11px]"
            onClick={() => onExpressionChange(nextExpression)}
          >
            Apply
          </button>
        </div>
        <div className="truncate font-mono text-[10px] text-gx-muted" title={nextExpression}>{nextExpression}</div>
      </div>
    </div>
  );
}

function CelConditionValueInput({
  suggestion,
  operator,
  value,
  onChange,
}: {
  suggestion: CelInputSuggestion;
  operator: CelAssistantOperator;
  value: string;
  onChange: (value: string) => void;
}) {
  if (!operatorNeedsValue(operator)) {
    return <div className="h-7 rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[11px] text-gx-muted">-</div>;
  }
  if (suggestion.type === "enum" && suggestion.enumValues?.length) {
    return (
      <select className="input-field h-7 min-w-0 text-[11px]" value={value} onChange={(event) => onChange(event.target.value)}>
        {suggestion.enumValues.map((item) => (
          <option key={item} value={item}>{item}</option>
        ))}
      </select>
    );
  }
  if (suggestion.type === "number") {
    return (
      <input
        className="input-field h-7 min-w-0 font-mono text-[11px]"
        inputMode="decimal"
        value={value}
        onChange={(event) => onChange(event.target.value)}
      />
    );
  }
  return (
    <input
      className="input-field h-7 min-w-0 text-[11px]"
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  );
}

function selectDefaultCelSuggestion(
  suggestions: CelInputSuggestion[],
  branchLabel: string,
): CelInputSuggestion | null {
  if (suggestions.length === 0) return null;
  const token = branchToken(branchLabel);
  if (token) {
    const enumMatch = suggestions.find((suggestion) =>
      suggestion.enumValues?.some((value) => normalizedText(value) === token),
    );
    if (enumMatch) return enumMatch;
    const nameMatch = suggestions.find((suggestion) =>
      normalizedText(suggestion.fieldName) === token || normalizedText(suggestion.displayPath).endsWith(token),
    );
    if (nameMatch) return nameMatch;
  }
  return suggestions.find((suggestion) => suggestion.type === "enum")
    ?? suggestions.find((suggestion) => suggestion.type === "boolean")
    ?? suggestions.find((suggestion) => suggestion.type === "number")
    ?? suggestions[0];
}

function defaultCelExpression(suggestions: CelInputSuggestion[], branchLabel: string): string {
  const suggestion = selectDefaultCelSuggestion(suggestions, branchLabel);
  if (!suggestion) return "input != null";
  const operator = defaultOperatorForSuggestion(suggestion, branchLabel);
  return buildCelExpression(suggestion, operator, defaultValueForSuggestion(suggestion, branchLabel));
}

function defaultOperatorForSuggestion(suggestion: CelInputSuggestion, branchLabel: string): CelAssistantOperator {
  if (suggestion.type === "boolean") {
    return /\b(no|false|fail|failed|reject|rejected)\b/i.test(branchLabel) ? "is_false" : "is_true";
  }
  if (suggestion.type === "number") return "greater_equal";
  if (suggestion.type === "array") return "is_not_empty";
  if (suggestion.type === "object") return "exists";
  return "equals";
}

function defaultValueForSuggestion(suggestion: CelInputSuggestion, branchLabel: string): string {
  const token = branchToken(branchLabel);
  if (suggestion.type === "enum" && suggestion.enumValues?.length) {
    return suggestion.enumValues.find((value) => normalizedText(value) === token) ?? suggestion.enumValues[0];
  }
  if (suggestion.type === "number") {
    return /score|confidence|probability|rating/i.test(suggestion.fieldName) ? "0.8" : "1";
  }
  if (suggestion.type === "string") {
    return token || "value";
  }
  if (suggestion.type === "array") {
    return "value";
  }
  return "";
}

function buildCelExpression(
  suggestion: CelInputSuggestion,
  operator: CelAssistantOperator,
  valueDraft: string,
): string {
  const path = suggestion.path;
  switch (operator) {
    case "exists":
      return `${path} != null`;
    case "not_exists":
      return `${path} == null`;
    case "is_true":
      return `${path} == true`;
    case "is_false":
      return `${path} == false`;
    case "is_empty":
      return `size(${path}) == 0`;
    case "is_not_empty":
      return `size(${path}) > 0`;
    case "contains":
      return suggestion.type === "array"
        ? `${celLiteral(valueDraft, "string")} in ${path}`
        : `${path}.contains(${celLiteral(valueDraft, "string")})`;
    case "equals":
      return `${path} == ${celLiteral(valueDraft, suggestion.type)}`;
    case "not_equals":
      return `${path} != ${celLiteral(valueDraft, suggestion.type)}`;
    case "greater_equal":
      return `${path} >= ${celLiteral(valueDraft, "number")}`;
    case "greater_than":
      return `${path} > ${celLiteral(valueDraft, "number")}`;
    case "less_equal":
      return `${path} <= ${celLiteral(valueDraft, "number")}`;
    case "less_than":
      return `${path} < ${celLiteral(valueDraft, "number")}`;
  }
}

function operatorNeedsValue(operator: CelAssistantOperator): boolean {
  return !["exists", "not_exists", "is_true", "is_false", "is_empty", "is_not_empty"].includes(operator);
}

function celLiteral(value: string, type: CelSuggestionType): string {
  if (type === "number") {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? String(parsed) : "0";
  }
  if (type === "boolean") return value === "false" ? "false" : "true";
  return JSON.stringify(value);
}

function branchToken(label: string): string {
  const normalized = normalizedText(label)
    .replace(/^if$/, "")
    .replace(/^elif\s*\d*$/, "")
    .replace(/^case\s*/, "")
    .trim();
  return normalized;
}

function normalizedText(value: string): string {
  return value.toLowerCase().replace(/[_-]+/g, " ").replace(/\s+/g, " ").trim();
}

function legacyConditionExpression(condition: WorkflowConditionGroup | undefined): string {
  if (!condition) return "input != null";
  const joiner = condition.logic === "or" ? " || " : " && ";
  return condition.conditions.map((item) => {
    const left = valueRefCelPath(item.left);
    switch (item.operator) {
      case "exists":
        return `${left} != null`;
      case "not_exists":
        return `${left} == null`;
      case "equals":
        return `${left} == ${JSON.stringify(item.value)}`;
      case "not_equals":
        return `${left} != ${JSON.stringify(item.value)}`;
      case "greater_than":
        return `${left} > ${JSON.stringify(item.value)}`;
      case "less_than":
        return `${left} < ${JSON.stringify(item.value)}`;
      case "contains":
        return `${left}.contains(${JSON.stringify(item.value)})`;
      case "not_contains":
        return `!${left}.contains(${JSON.stringify(item.value)})`;
      case "starts_with":
        return `${left}.startsWith(${JSON.stringify(item.value)})`;
      case "ends_with":
        return `${left}.endsWith(${JSON.stringify(item.value)})`;
      case "is_empty":
        return `size(${left}) == 0`;
      case "is_not_empty":
        return `size(${left}) > 0`;
      case "in":
        return `${JSON.stringify(item.value ?? [])}.exists(value, value == ${left})`;
    }
  }).join(joiner);
}

function valueRefCelPath(ref: WorkflowValueRef): string {
  if (ref.source === "runInput") return ref.path ? `workflow.input.${ref.path}` : "workflow.input";
  if (ref.source === "nodeOutput") return ref.nodeId ? `node[${JSON.stringify(ref.nodeId)}].output${ref.path ? `.${ref.path}` : ""}` : "input";
  if (ref.source === "nodeSummary") return ref.nodeId ? `node[${JSON.stringify(ref.nodeId)}].summary` : "input";
  if (ref.source === "literal") return JSON.stringify(ref.value);
  return "input";
}

function LoopConfigEditor({
  loop,
  onChange,
}: {
  loop: NonNullable<AgentWorkflowNode["loop"]>;
  onChange: (loop: NonNullable<AgentWorkflowNode["loop"]>) => void;
}) {
  return (
    <div className="space-y-3">
      <label className="space-y-1 text-[11px] text-gx-muted">
        <span>Maximum loop count</span>
        <input
          type="number"
          min={1}
          max={50}
          className="input-field w-full text-xs"
          value={loop.maxIterations ?? 3}
          onChange={(event) => onChange({
            ...loop,
            maxIterations: Math.max(1, Math.min(50, Number(event.target.value) || 1)),
          })}
        />
      </label>
      <div className="space-y-2">
        <label className="flex items-center gap-2 text-xs text-gx-text">
          <input
            type="checkbox"
            checked={Boolean(loop.terminateCondition)}
            onChange={(event) => onChange({
              ...loop,
              terminateCondition: event.target.checked ? createDefaultConditionGroup() : undefined,
            })}
          />
          Terminate when a condition matches
        </label>
        {loop.terminateCondition && (
          <ConditionGroupEditor
            group={loop.terminateCondition}
            onChange={(terminateCondition) => onChange({ ...loop, terminateCondition })}
          />
        )}
      </div>
      <ValueRefMapEditor
        label="Initial loop state"
        value={loop.initialState ?? {}}
        emptyLabel="No initial loop state."
        newKeyPrefix="initial"
        onChange={(initialState) => onChange({ ...loop, initialState })}
      />
      <ValueRefMapEditor
        label="State update mapping"
        value={loop.stateMapping ?? {}}
        emptyLabel="No per-iteration state mapping."
        newKeyPrefix="loop_state"
        onChange={(stateMapping) => onChange({ ...loop, stateMapping })}
      />
    </div>
  );
}

function ValueRefMapEditor({
  label,
  value,
  emptyLabel = "No mappings configured.",
  newKeyPrefix = "value",
  onChange,
}: {
  label: string;
  value: Record<string, WorkflowValueRef>;
  emptyLabel?: string;
  newKeyPrefix?: string;
  onChange: (value: Record<string, WorkflowValueRef> | undefined) => void;
}) {
  const entries = Object.entries(value);
  const commit = (next: Record<string, WorkflowValueRef>) => {
    onChange(Object.keys(next).length > 0 ? next : undefined);
  };
  const updateKey = (oldKey: string, newKey: string) => {
    const trimmed = newKey.trim();
    if (!trimmed || trimmed === oldKey) return;
    const next = { ...value };
    const current = next[oldKey];
    delete next[oldKey];
    next[trimmed] = current;
    commit(next);
  };
  const updateValue = (key: string, ref: WorkflowValueRef) => {
    commit({ ...value, [key]: ref });
  };
  const addValue = () => {
    let index = entries.length + 1;
    let key = `${newKeyPrefix}_${index}`;
    while (value[key]) {
      index += 1;
      key = `${newKeyPrefix}_${index}`;
    }
    commit({ ...value, [key]: { source: "runInput", path: key } });
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[11px] text-gx-muted">{label}</span>
        <button type="button" className="text-[11px] text-gx-accent hover:underline" onClick={addValue}>
          + Add
        </button>
      </div>
      {entries.length === 0 ? (
        <div className="rounded border border-dashed border-gx-border p-2 text-[10px] text-gx-muted">{emptyLabel}</div>
      ) : entries.map(([key, ref]) => (
        <ValueRefMapRow
          key={key}
          name={key}
          refValue={ref}
          onRename={updateKey}
          onChange={updateValue}
          onDelete={() => {
            const next = { ...value };
            delete next[key];
            commit(next);
          }}
        />
      ))}
    </div>
  );
}

function ValueRefMapRow({
  name,
  refValue,
  onRename,
  onChange,
  onDelete,
}: {
  name: string;
  refValue: WorkflowValueRef;
  onRename: (oldKey: string, newKey: string) => void;
  onChange: (key: string, ref: WorkflowValueRef) => void;
  onDelete: () => void;
}) {
  const [nameDraft, setNameDraft] = useState(name);

  useEffect(() => {
    setNameDraft(name);
  }, [name]);

  return (
    <div className="grid grid-cols-[82px_1fr_auto] items-start gap-1.5">
      <input
        className="input-field h-7 min-w-0 font-mono text-[11px]"
        value={nameDraft}
        onChange={(event) => setNameDraft(event.target.value)}
        onBlur={() => onRename(name, nameDraft)}
      />
      <ValueRefPicker value={refValue} onChange={(nextRef) => onChange(name, nextRef)} />
      <IconButton
        title="Remove state variable"
        variant="ghost"
        className="h-7 w-7 text-gx-error"
        onClick={onDelete}
      >
        <Trash2 size={12} />
      </IconButton>
    </div>
  );
}

function ConditionGroupEditor({
  group,
  onChange,
}: {
  group: WorkflowConditionGroup;
  onChange: (group: WorkflowConditionGroup) => void;
}) {
  const updateCondition = (index: number, condition: WorkflowNodeCondition) => {
    onChange({
      ...group,
      conditions: group.conditions.length === 0
        ? [condition]
        : group.conditions.map((item, itemIndex) => itemIndex === index ? condition : item),
    });
  };

  return (
    <div className="space-y-2">
      <div className="flex overflow-hidden rounded border border-gx-border">
        {(["and", "or"] as const).map((logic) => (
          <button
            key={logic}
            type="button"
            className={clsx(
              "flex-1 px-2 py-1 text-[11px] uppercase transition",
              group.logic === logic ? "bg-gx-accent/15 text-gx-accent" : "bg-gx-bg-sub text-gx-muted hover:text-gx-text",
            )}
            onClick={() => onChange({ ...group, logic })}
          >
            {logic}
          </button>
        ))}
      </div>
      {(group.conditions.length > 0 ? group.conditions : [createDefaultCondition()]).map((condition, index) => (
        <ConditionRowEditor
          key={index}
          condition={condition}
          onChange={(next) => updateCondition(index, next)}
          onDelete={group.conditions.length > 1 ? () => onChange({ ...group, conditions: group.conditions.filter((_, itemIndex) => itemIndex !== index) }) : undefined}
        />
      ))}
      <button
        type="button"
        className="text-[11px] text-gx-accent hover:underline"
        onClick={() => onChange({ ...group, conditions: [...group.conditions, createDefaultCondition()] })}
      >
        + Add condition
      </button>
    </div>
  );
}

function ConditionRowEditor({
  condition,
  onChange,
  onDelete,
}: {
  condition: WorkflowNodeCondition;
  onChange: (condition: WorkflowNodeCondition) => void;
  onDelete?: () => void;
}) {
  const [valueDraft, setValueDraft] = useState(() => condition.value === undefined ? "" : JSON.stringify(condition.value));

  useEffect(() => {
    setValueDraft(condition.value === undefined ? "" : JSON.stringify(condition.value));
  }, [condition.value]);

  const option = CONDITION_OPERATOR_OPTIONS.find((item) => item.value === condition.operator) ?? CONDITION_OPERATOR_OPTIONS[0];

  return (
    <div className="space-y-1.5 rounded border border-gx-border/70 bg-gx-surface/50 p-2">
      <div className="flex items-center gap-1.5">
        <ValueRefPicker value={condition.left} onChange={(left) => onChange({ ...condition, left })} />
        {onDelete && (
          <IconButton title="Delete condition" onClick={onDelete} variant="ghost" className="h-6 w-6 text-gx-error">
            <Trash2 size={12} />
          </IconButton>
        )}
      </div>
      <select
        className="input-field w-full text-xs"
        value={condition.operator}
        onChange={(event) => onChange({ ...condition, operator: event.target.value as WorkflowOutputPredicate["operator"] })}
      >
        {CONDITION_OPERATOR_OPTIONS.map((item) => (
          <option key={item.value} value={item.value}>{item.label}</option>
        ))}
      </select>
      {option.needsValue && (
        <input
          className="input-field w-full font-mono text-[11px]"
          value={valueDraft}
          placeholder={condition.operator === "in" ? "[\"open\", \"ready\"]" : "\"ready\""}
          onChange={(event) => {
            const next = event.target.value;
            setValueDraft(next);
            try {
              onChange({ ...condition, value: next.trim() ? JSON.parse(next) as unknown : undefined, right: undefined });
            } catch {
              onChange({ ...condition, value: next, right: undefined });
            }
          }}
        />
      )}
    </div>
  );
}

function ValueRefPicker({
  value,
  onChange,
}: {
  value: WorkflowValueRef;
  onChange: (value: WorkflowValueRef) => void;
}) {
  return (
    <div className="grid min-w-0 flex-1 grid-cols-[110px_1fr] gap-1.5">
      <select
        className="input-field h-7 text-xs"
        value={value.source}
        onChange={(event) => onChange({ source: event.target.value as WorkflowValueRef["source"] })}
      >
        <option value="runInput">Run input</option>
        <option value="nodeOutput">Node output</option>
        <option value="nodeSummary">Node summary</option>
        <option value="literal">Literal</option>
      </select>
      {value.source === "literal" ? (
        <input
          className="input-field h-7 font-mono text-[11px]"
          value={value.value === undefined ? "" : JSON.stringify(value.value)}
          placeholder="JSON value"
          onChange={(event) => {
            const next = event.target.value;
            try {
              onChange({ source: "literal", value: next.trim() ? JSON.parse(next) as unknown : undefined });
            } catch {
              onChange({ source: "literal", value: next });
            }
          }}
        />
      ) : (
        <input
          className="input-field h-7 font-mono text-[11px]"
          value={value.source === "nodeOutput" || value.source === "nodeSummary"
            ? [value.nodeId, value.path].filter(Boolean).join(".")
            : value.path ?? ""}
          placeholder={value.source === "runInput" ? "payload.status" : "nodeId.output.path"}
          onChange={(event) => {
            const raw = event.target.value.trim();
            if (value.source === "nodeOutput" || value.source === "nodeSummary") {
              const [nodeId, ...pathParts] = raw.split(".").filter(Boolean);
              onChange({ source: value.source, nodeId, path: pathParts.join(".") || undefined });
            } else {
              onChange({ source: value.source, path: raw || undefined });
            }
          }}
        />
      )}
    </div>
  );
}

function EdgeConditionEditor({
  edge,
  onChange,
}: {
  edge: AgentWorkflowEdge;
  onChange: (condition: WorkflowEdgeCondition) => void;
}) {
  const condition = edge.condition ?? { sourceStatus: "completed" as const };
  const predicate = condition.outputPredicate;
  const selectedStatus = condition.sourceStatus ?? "completed";
  const [predicateValueDraft, setPredicateValueDraft] = useState(() =>
    predicate?.value === undefined ? "" : JSON.stringify(predicate.value),
  );

  useEffect(() => {
    setPredicateValueDraft(predicate?.value === undefined ? "" : JSON.stringify(predicate.value));
  }, [predicate?.value]);

  const updatePredicate = (patch: Partial<WorkflowOutputPredicate>) => {
    onChange({
      ...condition,
      outputPredicate: {
        path: predicate?.path ?? "",
        operator: predicate?.operator ?? "exists",
        value: predicate?.value,
        ...patch,
      },
    });
  };

  return (
    <div className="space-y-2 text-xs">
      <div className="space-y-1.5">
        <div className="flex items-center justify-between gap-2 text-[11px] text-gx-muted">
          <span>Source status</span>
          <EdgeConditionPill condition={condition} compact />
        </div>
        <div className="grid grid-cols-2 gap-1" role="radiogroup" aria-label="Source status">
          {EDGE_SOURCE_STATUS_OPTIONS.map(({ value, label, description, Icon, className, activeClassName }) => {
            const active = selectedStatus === value;
            return (
              <button
                key={value}
                type="button"
                role="radio"
                aria-checked={active}
                title={description}
                className={clsx(
                  "flex h-8 min-w-0 items-center gap-1.5 rounded border px-2 text-left text-[11px] transition focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent focus-visible:outline-offset-1",
                  active
                    ? activeClassName
                    : "border-gx-border bg-gx-bg-sub text-gx-muted hover:border-gx-accent/50 hover:bg-gx-surface-hover hover:text-gx-text",
                )}
                onClick={() => onChange({ ...condition, sourceStatus: value })}
              >
                <Icon size={13} className={clsx("shrink-0", active ? undefined : className)} />
                <span className="truncate font-medium">{label}</span>
              </button>
            );
          })}
        </div>
      </div>
      {condition.branchId && (
        <label className="space-y-1 text-[11px] text-gx-muted">
          <span>Branch route</span>
          <input
            className="input-field w-full font-mono text-[11px]"
            value={condition.branchId}
            onChange={(event) => onChange({ ...condition, branchId: event.target.value || undefined })}
          />
        </label>
      )}
      <label className="flex items-center gap-2 text-xs text-gx-text">
        <input
          type="checkbox"
          checked={Boolean(predicate)}
          onChange={(event) => onChange({
            ...condition,
            outputPredicate: event.target.checked ? { path: "", operator: "exists" } : undefined,
          })}
        />
        Add structured-output predicate
      </label>
      {predicate && (
        <div className="space-y-2 rounded border border-gx-border p-2">
          <label className="space-y-1 text-[11px] text-gx-muted">
            <span>Output path</span>
            <input className="input-field w-full text-xs" value={predicate.path} onChange={(event) => updatePredicate({ path: event.target.value })} />
          </label>
          <label className="space-y-1 text-[11px] text-gx-muted">
            <span>Operator</span>
            <select className="input-field w-full text-xs" value={predicate.operator} onChange={(event) => updatePredicate({ operator: event.target.value as WorkflowOutputPredicate["operator"] })}>
              {CONDITION_OPERATOR_OPTIONS.map((item) => (
                <option key={item.value} value={item.value}>{item.label}</option>
              ))}
            </select>
          </label>
          {(CONDITION_OPERATOR_OPTIONS.find((item) => item.value === predicate.operator)?.needsValue ?? true) && (
            <label className="space-y-1 text-[11px] text-gx-muted">
              <span>Value JSON</span>
              <input
                className="input-field w-full font-mono text-[11px]"
                value={predicateValueDraft}
                onChange={(event) => {
                  const next = event.target.value;
                  setPredicateValueDraft(next);
                  try {
                    updatePredicate({ value: next.trim() ? JSON.parse(next) as unknown : undefined });
                  } catch {
                    updatePredicate({ value: next });
                  }
                }}
              />
            </label>
          )}
        </div>
      )}
    </div>
  );
}

function TriggerPanel({
  repoPath,
  definition,
  triggers,
  onSaveTrigger,
  onDeleteTrigger,
  onSetTriggerEnabled,
  onClose,
}: {
  repoPath: string;
  definition: AgentWorkflowDefinition;
  triggers: AgentWorkflowTrigger[];
  onSaveTrigger: (trigger: AgentWorkflowTrigger) => void;
  onDeleteTrigger: (triggerId: string) => void;
  onSetTriggerEnabled: (triggerId: string, enabled: boolean) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("Daily workflow");
  const [rrule, setRrule] = useState("FREQ=DAILY;INTERVAL=1");
  const [kind, setKind] = useState<"schedule" | "webhook">("schedule");

  const handleAdd = () => {
    const now = Date.now();
    const token = kind === "webhook" ? createId("wh").replace(/-/g, "").slice(0, 16) : undefined;
    onSaveTrigger({
      id: createId("trigger"),
      repoPath,
      workflowDefinitionId: definition.id,
      name: name.trim() || `${definition.name} ${kind}`,
      kind,
      rrule: kind === "schedule" ? rrule.trim() : "",
      webhookToken: token,
      enabled: true,
      createdAt: now,
      updatedAt: now,
    });
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 items-center gap-2 border-b border-gx-border px-3 text-xs font-medium">
        <Clock size={13} />
        <span className="flex-1">Triggers</span>
        <IconButton title="Hide triggers" onClick={onClose} variant="ghost" className="h-6 w-6">
          <ChevronUp size={13} />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 space-y-2 overflow-auto p-3">
        <div className="flex gap-1">
          <button
            type="button"
            className={clsx("flex-1 rounded px-2 py-1 text-[11px]", kind === "schedule" ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:bg-gx-bg-sub")}
            onClick={() => setKind("schedule")}
          >
            Schedule
          </button>
          <button
            type="button"
            className={clsx("flex-1 rounded px-2 py-1 text-[11px]", kind === "webhook" ? "bg-gx-accent/20 text-gx-accent" : "text-gx-muted hover:bg-gx-bg-sub")}
            onClick={() => setKind("webhook")}
          >
            Webhook
          </button>
        </div>
        <div className="grid grid-cols-[1fr_1fr_auto] gap-2">
          <input className="input-field h-7 text-xs" value={name} onChange={(event) => setName(event.target.value)} placeholder="Trigger name" />
          {kind === "schedule" ? (
            <input className="input-field h-7 font-mono text-[11px]" value={rrule} onChange={(event) => setRrule(event.target.value)} placeholder="RRULE" />
          ) : (
            <div className="flex h-7 items-center text-[10px] text-gx-muted">Token auto-generated</div>
          )}
          <IconButton title="Add trigger" onClick={handleAdd}>
            <Plus size={13} />
          </IconButton>
        </div>
        <div className="max-h-36 space-y-1 overflow-auto">
          {triggers.length === 0 ? (
            <div className="text-[11px] text-gx-muted">No triggers for this workflow.</div>
          ) : triggers.map((trigger) => (
            <div key={trigger.id} className="flex items-center gap-2 rounded border border-gx-border px-2 py-1 text-[11px]">
              <input
                type="checkbox"
                checked={trigger.enabled}
                onChange={(event) => onSetTriggerEnabled(trigger.id, event.target.checked)}
                title="Enable trigger"
              />
              <div className="min-w-0 flex-1">
                <div className="truncate text-gx-text">
                  {trigger.name}
                  <span className={clsx("ml-1 rounded px-1 text-[9px]", trigger.kind === "webhook" ? "bg-gx-accent/10 text-gx-accent" : "bg-gx-bg-sub text-gx-muted")}>
                    {trigger.kind}
                  </span>
                </div>
                <div className="truncate font-mono text-[10px] text-gx-muted">
                  {trigger.kind === "webhook" && trigger.webhookToken
                    ? `POST http://127.0.0.1:17789/${trigger.webhookToken}`
                    : trigger.rrule}
                </div>
              </div>
              {trigger.lastRunId && <span className="font-mono text-[10px] text-gx-muted">{trigger.lastRunId.slice(0, 8)}</span>}
              {trigger.kind === "webhook" && trigger.webhookToken && (
                <button
                  type="button"
                  className="btn-icon h-5 w-5 text-gx-accent"
                  title="Copy webhook URL"
                  onClick={() => {
                    navigator.clipboard.writeText(`http://127.0.0.1:17789/${trigger.webhookToken}`).catch(() => {});
                  }}
                >
                  <Copy size={11} />
                </button>
              )}
              <button type="button" className="btn-icon h-5 w-5 text-gx-error" title="Delete trigger" onClick={() => onDeleteTrigger(trigger.id)}>
                <Trash2 size={12} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function RunPanel({
  run,
  runs,
  definition,
  events,
  activeRunId,
  onSelectRun,
  runInputDraft,
  runInputError,
  onRunInputDraftChange,
  onRun,
  reviewTextByNodeRun,
  onReviewTextChange,
  onApproveNode,
  onRequestChanges,
  onRejectNode,
  onRetryNode,
  onRequestHumanReviewChanges,
  onClosePanel,
  onToggleSchedules,
  schedulesOpen,
  onOpenSubWorkflow,
  onCreateSubWorkflow,
  onOpenAgentSession,
}: {
  run: AgentWorkflowRun | null;
  runs: AgentWorkflowRun[];
  definition: AgentWorkflowDefinition;
  events: AgentWorkflowEvent[];
  activeRunId: string | null;
  onSelectRun: (run: AgentWorkflowRun) => void;
  runInputDraft: string;
  runInputError: string | null;
  onRunInputDraftChange: (value: string) => void;
  onRun: () => void;
  reviewTextByNodeRun: Record<string, string>;
  onReviewTextChange: React.Dispatch<React.SetStateAction<Record<string, string>>>;
  onApproveNode: (nodeRunId: string) => void;
  onRequestChanges: (nodeRunId: string) => void;
  onRejectNode: (nodeRunId: string) => void;
  onRetryNode: (nodeRunId: string) => void;
  onRequestHumanReviewChanges: (nodeRunId: string) => void;
  onClosePanel: () => void;
  onToggleSchedules: () => void;
  schedulesOpen: boolean;
  onOpenSubWorkflow: (nodeId: string) => void;
  onCreateSubWorkflow: (nodeId: string) => void;
  onOpenAgentSession: (taskId: string) => void;
}) {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-2 border-b border-gx-border px-3 text-xs font-medium">
        <History size={13} />
        Execution
        {run && <span className="ml-auto rounded border border-gx-border px-1.5 py-0 text-[10px] text-gx-muted">{run.status}</span>}
        <IconButton title={schedulesOpen ? "Hide schedules" : "Show schedules"} onClick={onToggleSchedules} variant="ghost" className="h-6 w-6">
          {schedulesOpen ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
        </IconButton>
        <IconButton title="Hide run panel" onClick={onClosePanel} variant="ghost" className="h-6 w-6">
          <ChevronRight size={13} />
        </IconButton>
      </div>
      <div className="border-b border-gx-border p-3">
        <div className="mb-1.5 flex items-center justify-between gap-2">
          <div className="text-[11px] font-medium text-gx-text">Run input</div>
          <CommandButton title="Run workflow with this input" label="Run" onClick={onRun} variant="primary">
            <Play size={12} />
          </CommandButton>
        </div>
        <textarea
          className={clsx("input-field min-h-20 w-full resize-y font-mono text-[11px]", runInputError && "border-gx-error")}
          value={runInputDraft}
          onChange={(event) => onRunInputDraftChange(event.target.value)}
        />
        {runInputError && <div className="mt-1 text-[10px] text-gx-error">{runInputError}</div>}
      </div>
      {runs.length > 0 && (
        <div className="max-h-36 space-y-1 overflow-auto border-b border-gx-border p-2">
          {runs.map((candidate) => (
            <button
              key={candidate.id}
              type="button"
              className={clsx(
                "grid w-full grid-cols-[1fr_auto] gap-2 rounded border px-2 py-1.5 text-left text-[11px] transition",
                activeRunId === candidate.id
                  ? "border-gx-accent/60 bg-gx-accent/10"
                  : "border-gx-border bg-gx-surface hover:bg-gx-surface-hover",
              )}
              onClick={() => onSelectRun(candidate)}
            >
              <span className="min-w-0">
                <span className="block truncate text-gx-text">
                  {new Date(candidate.startedAt).toLocaleString()} · {candidate.trigger?.type ?? "builder"}
                </span>
                <span className="block truncate font-mono text-[10px] text-gx-muted">
                  {candidate.id.slice(0, 8)} · rev {candidate.definitionSnapshot.definitionVersion} · {formatDurationMs(candidate.startedAt, candidate.finishedAt)}
                </span>
                <span className="block truncate font-mono text-[10px] text-gx-muted">
                  {formatInputSummary(candidate.input)}
                </span>
              </span>
              <span className={clsx(
                "rounded border px-1.5 py-0 text-[10px]",
                candidate.status === "completed" ? "border-gx-success/50 text-gx-success"
                  : candidate.status === "failed" ? "border-gx-error/50 text-gx-error"
                    : "border-gx-border text-gx-muted",
              )}>
                {candidate.status}
              </span>
            </button>
          ))}
        </div>
      )}
      {!run ? (
        <div className="p-3 text-xs text-gx-muted">No run selected.</div>
      ) : (
        <div className="min-h-0 flex-1 overflow-auto">
          <div className="space-y-2 p-3">
            {run.nodeRuns.map((nodeRun) => {
              const node = definition.nodes.find((candidate) => candidate.id === nodeRun.nodeId);
              if (!node) return null;
              return (
                <div key={nodeRun.id} className="rounded border border-gx-border bg-gx-surface px-3 py-2">
                  <div className="flex items-start gap-2">
                    <RunStatusIcon status={nodeRun.status} />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-xs font-medium">{node.title}</div>
                      <div className="mt-0.5 flex items-center gap-2 text-[10px] text-gx-muted">
                        <span>{NODE_TYPE_LABELS[node.type]}</span>
                        <span>Attempt {nodeRun.attempt}</span>
                        {nodeRun.taskId && <span className="font-mono">task {nodeRun.taskId.slice(0, 8)}</span>}
                        {nodeRun.childRunId && <span className="font-mono">child {nodeRun.childRunId.slice(0, 8)}</span>}
                        {node.type === "loop" && nodeRun.childRunIds && <span>{nodeRun.childRunIds.length} iterations</span>}
                      </div>
                    </div>
                    <span className={clsx("rounded border px-1.5 py-0 text-[10px]", STATUS_CLASSES[nodeRun.status])}>
                      {STATUS_LABELS[nodeRun.status]}
                    </span>
                    {nodeRun.taskId && (
                      <IconButton title="Open agent session" onClick={() => onOpenAgentSession(nodeRun.taskId!)} variant="ghost" className="h-6 w-6">
                        <MessageSquare size={12} />
                      </IconButton>
                    )}
                    {(node.type === "sub_workflow" || node.type === "loop") && (
                      node.subWorkflowDefinitionId ? (
                        <IconButton title="Open child workflow" onClick={() => onOpenSubWorkflow(node.id)} variant="ghost" className="h-6 w-6">
                          <ExternalLink size={12} />
                        </IconButton>
                      ) : (
                        <IconButton title="Create child workflow" onClick={() => onCreateSubWorkflow(node.id)} variant="ghost" className="h-6 w-6">
                          <FolderPlus size={12} />
                        </IconButton>
                      )
                    )}
                  </div>
                  {nodeRun.outputSummary && (
                    <div className="mt-2 whitespace-pre-wrap text-[11px] leading-relaxed text-gx-muted">
                      {nodeRun.outputSummary}
                    </div>
                  )}
                  {nodeRun.outputJson !== undefined && (
                    <pre className="mt-2 max-h-32 overflow-auto rounded border border-gx-border bg-gx-bg-sub/60 p-2 font-mono text-[10px] leading-relaxed text-gx-muted">
                      {JSON.stringify(nodeRun.outputJson, null, 2)}
                    </pre>
                  )}
                  {node.type === "loop" && Array.isArray((nodeRun.outputJson as { iterations?: unknown[] } | undefined)?.iterations) && (
                    <div className="mt-2 grid grid-cols-2 gap-1.5">
                      {((nodeRun.outputJson as { iterations: Array<Record<string, unknown>> }).iterations).map((iteration) => (
                        <div key={String(iteration.childRunId ?? iteration.index)} className="rounded border border-gx-border px-2 py-1 text-[10px]">
                          <div className="truncate text-gx-text">Iteration {String(iteration.index ?? "?")}</div>
                          <div className="truncate text-gx-muted">{String(iteration.childRunId ?? "")}</div>
                        </div>
                      ))}
                    </div>
                  )}
                  {nodeRun.outputJsonError && (
                    <div className="mt-2 text-[11px] text-gx-warn">{nodeRun.outputJsonError}</div>
                  )}
                  {nodeRun.error && (
                    <div className="mt-2 text-[11px] text-gx-error">{nodeRun.error}</div>
                  )}
                  {nodeRun.status === "needs_review" && (
                    <div className="mt-2 space-y-2">
                      <textarea
                        className="input-field min-h-16 w-full resize-none text-xs"
                        placeholder={node.type === "approval_gate" ? "Approval reason" : node.type === "human_review" ? "Review feedback" : "Approval summary or feedback"}
                        value={reviewTextByNodeRun[nodeRun.id] ?? ""}
                        onChange={(event) => onReviewTextChange((current) => ({ ...current, [nodeRun.id]: event.target.value }))}
                      />
                      <div className="flex items-center gap-1.5">
                        <IconButton title="Approve node" onClick={() => onApproveNode(nodeRun.id)} variant="ghost" className="text-gx-success">
                          <Check size={13} />
                        </IconButton>
                        {node.type === "human_review" && (
                          <IconButton title="Request changes" onClick={() => onRequestHumanReviewChanges(nodeRun.id)} variant="ghost" className="text-gx-warn">
                            <RotateCcw size={13} />
                          </IconButton>
                        )}
                        {node.type === "agent_task" && (
                          <IconButton title="Request changes" onClick={() => onRequestChanges(nodeRun.id)} variant="ghost" className="text-gx-warn">
                            <RotateCcw size={13} />
                          </IconButton>
                        )}
                        {node.type !== "human_review" && (
                          <IconButton title="Reject node" onClick={() => onRejectNode(nodeRun.id)} variant="ghost" className="text-gx-error">
                            <X size={13} />
                          </IconButton>
                        )}
                      </div>
                    </div>
                  )}
                  {(nodeRun.status === "failed" || nodeRun.status === "cancelled") && (
                    <IconButton title="Retry node" onClick={() => onRetryNode(nodeRun.id)} className="mt-2">
                      <RotateCcw size={13} />
                    </IconButton>
                  )}
                </div>
              );
            })}
          </div>

          <div className="border-t border-gx-border p-3">
            {run.edgeRuns.length > 0 && (
              <div className="mb-3">
                <div className="mb-2 text-xs font-medium">Edges</div>
                <div className="grid grid-cols-2 gap-1.5">
                  {run.edgeRuns.map((edgeRun) => {
                    const edge = run.definitionSnapshot.edges.find((candidate) => candidate.id === edgeRun.edgeId);
                    return (
                      <div key={edgeRun.edgeId} className="rounded border border-gx-border px-2 py-1 text-[10px]">
                        <div className="truncate text-gx-text">{edge?.sourceNodeId} → {edge?.targetNodeId}</div>
                        <div className="truncate text-gx-muted">{edgeRun.status}{edgeRun.reason ? ` · ${edgeRun.reason}` : ""}</div>
                      </div>
                    );
                  })}
                </div>
              </div>
            )}
            <div className="mb-2 text-xs font-medium">Event Timeline</div>
            <div className="space-y-1.5">
              {events.length === 0 ? (
                <div className="text-xs text-gx-muted">No events yet.</div>
              ) : events.map((event) => (
                <div key={event.id} className="flex gap-2 text-[11px]">
                  <Clock size={12} className="mt-0.5 shrink-0 text-gx-muted" />
                  <div className="min-w-0">
                    <div className="truncate text-gx-text">{event.message}</div>
                    <div className="text-[10px] text-gx-muted">{new Date(event.timestamp).toLocaleTimeString()}</div>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function RunStatusIcon({ status }: { status: NodeRunStatus }) {
  if (status === "completed") return <Check size={14} className="mt-0.5 shrink-0 text-gx-success" />;
  if (status === "failed") return <X size={14} className="mt-0.5 shrink-0 text-gx-error" />;
  if (status === "needs_review") return <Pause size={14} className="mt-0.5 shrink-0 text-gx-warn" />;
  if (status === "running" || status === "queued" || status === "ready") return <Play size={14} className="mt-0.5 shrink-0 text-gx-accent" />;
  return <CircleDot size={14} className="mt-0.5 shrink-0 text-gx-muted" />;
}
