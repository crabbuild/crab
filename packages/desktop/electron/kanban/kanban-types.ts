/**
 * Shared TypeScript interfaces and types for the Kanban Orchestration subsystem.
 *
 * These types are used across the main process modules (board sync, automation
 * engine, pipeline controller, CI monitor) and the renderer for board state,
 * automation rules, and analytics.
 */

// ── Card ─────────────────────────────────────────────────────────

export type TrackerSource = "github" | "jira" | "github-projects" | "linear" | "local";

export type CardPriority = "critical" | "high" | "medium" | "low" | "none";

export type PipelineState =
  | "idle"
  | "agent_running"
  | "pr_created"
  | "ci_pending"
  | "ci_passed"
  | "ci_failed"
  | "review_pending"
  | "review_approved"
  | "review_changes_requested"
  | "merging"
  | "done"
  | "blocked";

export type AutonomyLevel = "conservative" | "moderate" | "aggressive";

/** Card layout density: controls padding, font size, and line-clamp. */
export type DensityMode = "compact" | "comfortable" | "spacious";

/** Board grouping mode: determines how cards are organized into columns. */
export type GroupingMode = "default" | "priority" | "label" | "assignee";

/** Directional relationship type between two cards. */
export type RelationType = "parent" | "child" | "blocks" | "blocked-by" | "relates-to";

export type BoardFieldType =
  | "text"
  | "number"
  | "select"
  | "multi-select"
  | "user"
  | "date"
  | "checkbox"
  | "url";

export interface BoardFieldOption {
  id: string;
  name: string;
  color?: string;
}

export interface BoardFieldDefinition {
  id: string;
  name: string;
  type: BoardFieldType;
  taskScoped?: boolean;
  boardScoped?: boolean;
  options?: BoardFieldOption[];
  required?: boolean;
  createdAt?: number;
  updatedAt?: number;
}

export type WorktreeMode = "new" | "existing";

export interface KanbanRunDefaults {
  baseRef?: string;
  branchName?: string;
  worktreeMode?: WorktreeMode;
  worktreePath?: string;
  agentType?: string;
  model?: string;
  reasoningLevel?: string;
  mode?: "plan" | "instant";
  openChat?: boolean;
}

export type TaskRunStatus =
  | "created"
  | "queued"
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

export interface KanbanTaskRun {
  id: string;
  taskId: string;
  boardId: string;
  orchestratorTaskId: string;
  status: TaskRunStatus;
  worktreePath?: string;
  branch?: string;
  baseRef?: string;
  agentType?: string;
  model?: string;
  reasoningLevel?: string;
  mode?: "plan" | "instant";
  summary?: string;
  createdAt: number;
  startedAt?: number;
  completedAt?: number;
}

export interface KanbanTaskActivity {
  id: string;
  taskId: string;
  boardId: string;
  type: "created" | "updated" | "moved" | "relation" | "run" | "system";
  message: string;
  field?: string;
  fromValue?: unknown;
  toValue?: unknown;
  createdAt: number;
}

/** A directional link from one card to another. */
export interface CardRelation {
  targetCardId: string;
  type: RelationType;
}

export interface WorkflowProjection {
  workflowDefinitionId: string;
  workflowRunId: string;
  nodeRunId: string;
  managed: true;
}

export interface SymphonyCardRuntime {
  managed: true;
  issueId: string;
  identifier: string;
  status: "unclaimed" | "running" | "retrying" | "blocked" | "completed";
  workspacePath?: string;
  taskId?: string;
  sessionId?: string | null;
  retryAttempt?: number;
  retryDueAtMs?: number;
  blockedReason?: string;
  lastEvent?: string | null;
  lastEventAt?: number | null;
  lastMessage?: string | null;
  tokens?: {
    inputTokens: number;
    outputTokens: number;
    totalTokens: number;
  };
}

/** Filter criteria applied to the board. */
export interface ActiveFilters {
  assignees: string[];
  labels: string[];
  priorities: CardPriority[];
  pipelineStates: PipelineState[];
}

/** A persisted combination of filter and grouping settings. */
export interface SavedView {
  id: string;
  name: string;
  filters: ActiveFilters;
  groupingMode: GroupingMode;
  createdAt: number;
}

/** Per-column UI state (collapsed/expanded). */
export interface ColumnUIState {
  [columnId: string]: {
    collapsed: boolean;
  };
}

export interface KanbanCard {
  id: string;
  boardId?: string;
  source: TrackerSource;
  externalId: string;
  externalUrl: string;
  title: string;
  description: string;
  columnId: string;
  positionIndex: number;
  assignee?: string;
  assigneeAvatarUrl?: string;
  priority: CardPriority;
  labels: string[];
  linkedTaskId?: string;
  linkedPrNumber?: number;
  linkedPrUrl?: string;
  pipelineState: PipelineState;
  iterationCount: number;
  createdAt: number;
  updatedAt: number;
  archivedAt?: number;
  /** Parent card ID for sub-issue hierarchy. */
  parentCardId?: string;
  /** Child card IDs (sub-issues). */
  childCardIds?: string[];
  /** Directional relations to other cards. */
  relations?: CardRelation[];
  /** Workflow projection metadata for cards managed by Agent Workflows. */
  workflowProjection?: WorkflowProjection;
  /** Symphony runtime metadata for tracker issue cards managed by Symphony mode. */
  symphonyRuntime?: SymphonyCardRuntime;
  /** Pipeline state transition history for the detail panel activity log. */
  activityLog?: Array<{
    timestamp: number;
    fromState: string;
    toState: string;
  }>;
  /** Board-defined task fields, keyed by field ID. */
  customFields?: Record<string, unknown>;
  /** Per-task agent run defaults overriding the board defaults. */
  runDefaults?: KanbanRunDefaults;
  /** Latest linked agent run for quick board/detail rendering. */
  latestRun?: KanbanTaskRun;
  /** Optional expanded run history. */
  runs?: KanbanTaskRun[];
  /** Optional expanded activity feed. */
  activity?: KanbanTaskActivity[];
}

export type KanbanTask = KanbanCard;

// ── Board Configuration ──────────────────────────────────────────

export interface KanbanColumn {
  id: string;
  name: string;
  trackerStatusMapping: string[];
  wipLimit?: number;
}

export interface BoardConfig {
  id: string;
  name: string;
  repoPath: string;
  columns: KanbanColumn[];
  trackerType: TrackerSource;
  trackerConfig: JiraTrackerConfig | GitHubProjectsConfig | GitHubIssuesConfig | LinearTrackerConfig;
  autonomyLevel: AutonomyLevel;
  pollIntervalMs: number;
  ciPollIntervalMs: number;
  maxIterations: number;
  filterCriteria: BoardFilter;
  webhookPort?: number;
  webhookSecret?: string;
  /** Active card layout density preference. */
  densityMode?: DensityMode;
  /** User-saved filter/grouping view presets (max 20). */
  savedViews?: SavedView[];
  /** Per-column collapsed/expanded state. */
  columnStates?: ColumnUIState;
  /** Board-defined fields for task and board attributes. */
  fields?: BoardFieldDefinition[];
  /** Board-level custom attribute values keyed by field ID. */
  customFields?: Record<string, unknown>;
  /** Defaults applied when a task is triggered from this board. */
  runDefaults?: KanbanRunDefaults;
  /** Symphony-style autonomous tracker execution settings. */
  symphony?: {
    enabled?: boolean;
    workflowPath?: string;
    autostart?: boolean;
    projectToKanban?: boolean;
  };
}

export interface BoardFilter {
  labels?: string[];
  assignees?: string[];
  milestones?: string[];
  excludeLabels?: string[];
}

export interface JiraTrackerConfig {
  instanceUrl: string;
  projectKey: string;
  boardId?: number;
  sprintId?: number;
  authType: "oauth" | "pat";
  credentialRef: string;
}

export interface GitHubProjectsConfig {
  projectNumber: number;
  owner: string;
  statusFieldName: string;
}

export interface GitHubIssuesConfig {
  owner: string;
  repo: string;
}

export interface LinearTrackerConfig {
  projectSlug?: string;
}

// ── Automation Rules ─────────────────────────────────────────────

export type TriggerType =
  | "card_moved"
  | "issue_assigned"
  | "label_added"
  | "pr_review_requested"
  | "ci_failed"
  | "pr_comment"
  | "schedule"
  | "manual";

export type ConditionType =
  | "label_matches"
  | "complexity_below"
  | "has_description"
  | "no_linked_pr"
  | "assignee_is_me";

export type ActionType =
  | "spawn_agent"
  | "create_pr"
  | "run_tests"
  | "request_review"
  | "transition_issue"
  | "add_comment"
  | "merge_pr"
  | "iterate_agent";

export interface AutomationRule {
  id: string;
  name: string;
  enabled: boolean;
  trigger: { type: TriggerType; params?: Record<string, unknown> };
  conditions: Array<{ type: ConditionType; params: Record<string, unknown> }>;
  actions: Array<{ type: ActionType; params: Record<string, unknown> }>;
  autonomyOverride?: AutonomyLevel;
  continueOnError: boolean;
}

export interface RuleExecution {
  id: string;
  ruleId: string;
  cardId: string;
  triggeredAt: number;
  completedAt?: number;
  status: "running" | "completed" | "failed";
  actionResults: Array<{
    actionType: ActionType;
    status: "success" | "failed" | "skipped";
    error?: string;
    durationMs: number;
  }>;
}

// ── Analytics ────────────────────────────────────────────────────

export interface CardAnalytics {
  cardId: string;
  externalId: string;
  stateTransitions: Array<{
    from: string;
    to: string;
    timestamp: number;
  }>;
  tokenUsage: Array<{
    turnNumber: number;
    inputTokens: number | null;
    outputTokens: number | null;
    costEstimate: number | null;
    timestamp: number;
  }>;
  cycleTimeMs?: number;
  iterationCount: number;
  prNumber?: number;
  prUrl?: string;
  outcome: "success" | "blocked" | "cancelled" | "in_progress";
}

export interface BoardAnalytics {
  boardId: string;
  cards: CardAnalytics[];
  summary: {
    totalCost: number;
    averageCycleTimeMs: number;
    successRate: number;
    totalAttempted: number;
    totalCompleted: number;
    iterationDistribution: Record<number, number>;
  };
}

// ── Persistence Wrappers ─────────────────────────────────────────

export interface PersistedBoardData {
  version: 1;
  boards: BoardConfig[];
}

export interface PersistedRuleData {
  version: 1;
  rules: AutomationRule[];
}

export interface PersistedAnalyticsData {
  version: 1;
  analytics: CardAnalytics[];
}

export interface PersistedBoardState {
  version: 1;
  cards: KanbanCard[];
}
