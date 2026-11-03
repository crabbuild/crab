import type { EventEmitter } from "node:events";
import type { AgentType } from "../../orchestrator-adapters";
import type { BoardConfig, CardPriority, KanbanCard, KanbanColumn, TrackerSource } from "../kanban-types";

export interface SymphonyIssueBlockerRef {
  id: string | null;
  identifier: string | null;
  state: string | null;
}

export interface SymphonyIssue {
  id: string;
  identifier: string;
  title: string;
  description: string | null;
  priority: number | null;
  state: string;
  branchName: string | null;
  url: string | null;
  labels: string[];
  blockedBy: SymphonyIssueBlockerRef[];
  assignee?: string | null;
  assigneeId?: string | null;
  createdAt: string | null;
  updatedAt: string | null;
}

export interface WorkflowDefinition {
  config: Record<string, unknown>;
  promptTemplate: string;
  workflowPath: string;
  workflowDir: string;
  loadedAtMs: number;
  mtimeMs: number | null;
}

export type SymphonyTrackerKind = Extract<TrackerSource, "linear" | "jira" | "github" | "github-projects">;

export interface SymphonyTrackerConfig {
  kind: SymphonyTrackerKind;
  activeStates: string[];
  terminalStates: string[];
  endpoint?: string;
  apiKey?: string | null;
  projectSlug?: string | null;
  assignee?: string | null;
  instanceUrl?: string | null;
  projectKey?: string | null;
  boardId?: number | null;
  sprintId?: number | null;
  authType?: "oauth" | "pat" | null;
  credentialRef?: string | null;
  owner?: string | null;
  repo?: string | null;
  projectNumber?: number | null;
  statusFieldName?: string | null;
}

export interface SymphonyPollingConfig {
  intervalMs: number;
}

export interface SymphonyWorkspaceConfig {
  root: string;
}

export interface SymphonyHooksConfig {
  afterCreate: string | null;
  beforeRun: string | null;
  afterRun: string | null;
  beforeRemove: string | null;
  timeoutMs: number;
}

export interface SymphonyAgentConfig {
  maxConcurrentAgents: number;
  maxTurns: number;
  maxRetryBackoffMs: number;
  maxConcurrentAgentsByState: Record<string, number>;
}

export interface SymphonyCodexConfig {
  command: string;
  approvalPolicy?: unknown;
  threadSandbox: string;
  turnSandboxPolicy?: Record<string, unknown>;
  turnTimeoutMs: number;
  readTimeoutMs: number;
  stallTimeoutMs: number;
}

export interface SymphonyConfig {
  workflow: WorkflowDefinition;
  tracker: SymphonyTrackerConfig;
  polling: SymphonyPollingConfig;
  workspace: SymphonyWorkspaceConfig;
  hooks: SymphonyHooksConfig;
  agent: SymphonyAgentConfig;
  codex: SymphonyCodexConfig;
}

export interface SymphonyConfigError {
  code: string;
  message: string;
  path?: string;
}

export type SymphonyPollStatus = "stopped" | "idle" | "polling" | "error";
export type SymphonyIssueRuntimeStatus = "running" | "retrying" | "blocked" | "completed";

export interface SymphonyTokenTotals {
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
}

export interface SymphonyRunningEntry {
  issueId: string;
  identifier: string;
  issue: SymphonyIssue;
  taskId: string;
  workspacePath: string;
  startedAt: number;
  attempt: number | null;
  lastEvent: string | null;
  lastEventAt: number | null;
  lastMessage: string | null;
  sessionId: string | null;
  tokenTotals: SymphonyTokenTotals;
}

export interface SymphonyRetryEntry {
  issueId: string;
  identifier: string;
  issue?: SymphonyIssue;
  attempt: number;
  dueAtMs: number;
  error: string | null;
  workspacePath?: string;
}

export interface SymphonyBlockedEntry {
  issueId: string;
  identifier: string;
  issue?: SymphonyIssue;
  workspacePath?: string;
  taskId?: string;
  sessionId?: string | null;
  error: string;
  blockedAt: number;
  lastEvent: string | null;
  lastEventAt: number | null;
  lastMessage: string | null;
}

export interface SymphonyRuntimeEvent {
  id: string;
  boardId: string;
  issueId?: string;
  identifier?: string;
  taskId?: string;
  type: string;
  message: string;
  timestamp: number;
  payload?: Record<string, unknown>;
}

export interface SymphonyRuntimeSnapshot {
  boardId: string;
  status: SymphonyPollStatus;
  started: boolean;
  workflowPath: string | null;
  lastReloadAtMs: number | null;
  lastPollAtMs: number | null;
  nextPollAtMs: number | null;
  configError: SymphonyConfigError | null;
  counts: {
    running: number;
    retrying: number;
    blocked: number;
    completed: number;
    claimed: number;
  };
  running: SymphonyRunningEntry[];
  retrying: SymphonyRetryEntry[];
  blocked: SymphonyBlockedEntry[];
  codexTotals: SymphonyTokenTotals;
  rateLimits: unknown | null;
  recentEvents: SymphonyRuntimeEvent[];
}

export interface SymphonyIssueSnapshot {
  issueId: string;
  identifier: string;
  status: SymphonyIssueRuntimeStatus | "unclaimed";
  running?: SymphonyRunningEntry;
  retry?: SymphonyRetryEntry;
  blocked?: SymphonyBlockedEntry;
  card?: KanbanCard;
}

export interface SymphonyWorkspace {
  path: string;
  key: string;
  createdNow: boolean;
}

export interface SymphonyTracker {
  fetchCandidateIssues(): Promise<SymphonyIssue[]>;
  fetchIssueStatesByIds(issueIds: string[]): Promise<SymphonyIssue[]>;
  fetchTerminalIssues(): Promise<SymphonyIssue[]>;
}

export interface SymphonyOrchestratorLike extends EventEmitter {
  createTask(
    prompt: string,
    agentType: AgentType,
    repoPath: string,
    baseRef?: string,
  ): Promise<{ id: string; kanbanCardId?: string; symphonyIssueId?: string }>;
  startTask(taskId: string, options?: { skipCheckpoint?: boolean }): Promise<void>;
  cancelTask?(taskId: string): Promise<void>;
}

export interface SymphonyBoardProjectionInput {
  board: BoardConfig;
  columns: KanbanColumn[];
  trackerKind: SymphonyTrackerKind;
  issues: SymphonyIssue[];
  running: Map<string, SymphonyRunningEntry>;
  retrying: Map<string, SymphonyRetryEntry>;
  blocked: Map<string, SymphonyBlockedEntry>;
}

export interface SymphonyCardRuntime {
  managed: true;
  issueId: string;
  identifier: string;
  status: SymphonyIssueRuntimeStatus | "unclaimed";
  workspacePath?: string;
  taskId?: string;
  sessionId?: string | null;
  retryAttempt?: number;
  retryDueAtMs?: number;
  blockedReason?: string;
  lastEvent?: string | null;
  lastEventAt?: number | null;
  lastMessage?: string | null;
  tokens?: SymphonyTokenTotals;
}

export interface SymphonyBoardConfig {
  enabled?: boolean;
  workflowPath?: string;
  autostart?: boolean;
  projectToKanban?: boolean;
}

export function normalizeIssueState(state: string | null | undefined): string {
  return String(state ?? "").trim().toLowerCase();
}

export function mapIssuePriority(priority: number | null): CardPriority {
  if (priority === 1) return "critical";
  if (priority === 2) return "high";
  if (priority === 3) return "medium";
  if (priority === 4) return "low";
  return "none";
}
