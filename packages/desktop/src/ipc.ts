import { OP_CANCELLED_RPC_CODE } from "../electron/rpc-transport";
import type { FileTransferActivity, FileTransferSource, ProgressTransfer } from "../electron/file-transfers";
import type { DeployProgress } from "../electron/ssh-deployer";
import type { KnownHost } from "../electron/ssh-known-hosts";
import { parseRepoId } from "../electron/ssh-types";
import type { StorageConnectionInfo, StorageConnectionInput, StorageConnectionStatus } from "../electron/storage-connection-types";
import { requestConfirmation, requestPrompt } from "./lib/application-dialogs";
export type { ProgressTransfer } from "../electron/file-transfers";
// Typed wrapper around the `window.crab` bridge exposed by
// `electron/preload.ts`. Centralising the types here keeps components
// free of `unknown` assertions and makes the agent's RPC surface
// self-documenting.

import type { JupyterRuntime, KernelExecution, KernelOutput, KernelSpec, KernelStatus, KernelTarget } from "../electron/jupyter-types";
import type { CapabilitySet } from "../electron/repo-capabilities";

export interface RepoHandle {
  repo_id: string;
  url: string;
  is_remote: boolean;
  cache_dir: string;
  native_session_id: string | null;
  /** Present when an in-flight op (merge/rebase/cherry-pick) was
   *  found on disk at open time. The renderer can use this to show
   *  a resume banner without a second RPC. */
  active_op?: {
    op_id: string;
    state: string;
    op: unknown;
    paused_reason: string | null;
  } | null;
  /** Detected forge provider for this repo (github, gitlab, bitbucket,
   *  gitea). `null` when no forge could be identified — PR features
   *  should be hidden in that case. */
  forge_provider?: "github" | "gitlab" | "bitbucket" | "gitea" | null;
}

export interface CacheUsage {
  files: number;
  directories: number;
  logical_bytes: number;
  allocated_bytes: number;
}

export interface CacheStats {
  root: string;
  root_state: "missing" | "present" | "unavailable";
  budget_bytes: number | null;
  budget_available: boolean;
  catalog: { state: "missing" | "unavailable" } | { state: "readable"; stats: { retention_budget_bytes: number | null } };
  observed: CacheUsage;
  scan_complete: boolean;
  over_budget: boolean | null;
  families: Record<string, CacheUsage & { complete: boolean; issues: number }>;
  issues: Array<{ family: string | null; path: string; kind: string; error: string }>;
  omitted_issues: number;
}

export interface CacheBudgetResult {
  usage: CacheStats;
  reclaimed_bytes: number;
  maintenance_pending: boolean;
}

export interface CacheCleanResult {
  files_removed: number;
  bytes_reclaimed: number;
  retained_entries: number;
  busy_entries: number;
  unsafe_entries: number;
  dry_run: boolean;
}

export type EntryKind = "file" | "dir" | "pointer" | "lfs" | "submodule";
export type PointerKind = "crab" | "lfs" | "git";
export type RefKind = "branch" | "tag" | "head" | "remote" | "other";

export interface DirEntry {
  path: string;
  kind: EntryKind;
  size: number | null;
  pointer_hash: string | null;
}

export interface RefEntry {
  name: string;
  target: string;
  kind: RefKind;
  upstream?: string | null;
  ahead?: number | null;
  behind?: number | null;
  is_head?: boolean;
}

export interface WorktreeEntry {
  path: string;
  head: string;
  branch: string;
  is_main: boolean;
  is_detached: boolean;
  is_locked: boolean;
  lock_reason: string | null;
  is_prunable: boolean;
}

/** UI-only metadata stored in .crab/desktop/worktrees.json. */
export interface WorktreeMeta {
  path: string;
  display_name: string | null;
  colour: string | null;
  pinned: boolean;
  hydration_policy: "all" | "on_access" | "pointers_only";
}

/** Hydrated worktree entry combining git state with UI metadata.
 *  Returned by the `worktree_list` verb after reconciliation. */
export interface HydratedWorktreeEntry {
  path: string;
  branch: string;
  head: string;
  is_main: boolean;
  locked: boolean;
  lock_reason: string | null;
  prunable: boolean;
  meta: WorktreeMeta | null;
}

/** Per-worktree status: dirty count + ahead/behind. */
export interface WorktreeStatus {
  dirty_count: number;
  ahead: number | null;
  behind: number | null;
}

/** Push estimate for a worktree (disk usage / pointer cost). */
export interface PushEstimate {
  total_bytes: number;
  xorb_count: number;
  pointer_count: number;
}

export interface FileStat {
  size: number;
  pointer_hash: string | null;
  mtime: number | null;
  rev: string | null;
  kind: PointerKind;
}

export interface CapturedFile {
  file_id: string;
  size: number;
  kind: PointerKind;
  representation: "working_file" | "git_blob" | "pointer" | "remote_content";
  content_hash: string | null;
  git_oid: string | null;
  can_materialize: boolean;
}

export interface MediaSource {
  media_id: string;
  url: string;
  size: number;
  kind: PointerKind;
}

export interface FileChunkPage {
  chunk_count: number;
  xorb_count: number | null;
  chunks: Array<{
    hash: string;
    offset: number;
    length: number;
    xorb_hash: string | null;
    xorb_chunk_index: number | null;
  }>;
  next: number | null;
}

export interface AuthStatus {
  provider: string;
  identity: string | null;
  token_expiry: string | null;
  token_expired: boolean;
  refresh: boolean;
  provider_settings?: Array<{ key: string; value: string }>;
}

export interface ManagedServiceProfile {
  authority: string;
  discovery_origin: string;
  active: boolean;
  discovered: boolean;
  service_version: string | null;
  minimum_cli_version: string | null;
}

export interface ManagedProfilesResponse {
  profiles: ManagedServiceProfile[];
  active_authority: string | null;
}

export interface ManagedOrganization {
  schema_version: number;
  id: string;
  slug: string;
  state: "active" | "suspended" | "deleting" | "deleted";
  revision: number;
}

export interface ManagedOrganizationPage {
  authority: string;
  organizations: ManagedOrganization[];
  next_cursor: string | null;
}

export interface ManagedRepository {
  schema_version: number;
  repository_id: string;
  organization_id: string;
  canonical_url: string;
  state: "provisioning" | "active" | "archived" | "soft_deleted" | "failed";
  revision: number;
  transfer_modes: string[];
  protected_push: boolean;
}

export interface ManagedRepositoryPage {
  authority: string;
  repositories: ManagedRepository[];
  next_cursor: string | null;
}

export interface ReadTextResult {
  truncated: boolean;
  size: number;
  text: string | null;
  reason?: string;
}

export interface ReadBytesResult {
  truncated: boolean;
  size: number;
  /** Base64-encoded blob body when not truncated. */
  bytes_b64: string | null;
  reason?: string;
}

export interface ParquetColumn {
  name: string;
  type: string;
  nullable: boolean;
}
export interface ParquetStats {
  column: string;
  null_count: number | null;
  distinct_count: number | null;
  compression: string;
  compressed_size: number;
  uncompressed_size: number;
}
export interface ParquetPreview {
  kind: "parquet";
  num_rows: number;
  num_row_groups: number;
  num_columns: number;
  created_by: string | null;
  columns: ParquetColumn[];
  row_group_stats: ParquetStats[];
  preview_rows: Array<Record<string, unknown>>;
  preview_row_count: number;
}

export interface SafetensorsTensor {
  name: string;
  dtype: string;
  shape: number[];
  nbytes: number;
}
export interface SafetensorsPreview {
  kind: "safetensors";
  header_bytes: number;
  tensor_count: number;
  /** Stringified BigInt. */
  total_params: string;
  /** Stringified BigInt. */
  total_bytes: string;
  dtype_histogram: Record<string, number>;
  metadata: Record<string, string> | null;
  tensors: SafetensorsTensor[];
}

export interface NpyPreview {
  kind: "npy";
  shape: number[];
  dtype: string;
  order: string;
  num_elements: string;
}
export interface NpzPreview {
  kind: "npz";
  members: Array<{
    name: string;
    shape?: number[];
    dtype?: string;
    nbytes?: number;
    error?: string;
  }>;
}

export interface ZipPreview {
  kind: "zip";
  member_count: number;
  total_compressed: string;
  total_uncompressed: string;
  members: Array<{
    name: string;
    is_dir: boolean;
    size: number;
    compressed: number;
    crc32: number;
  }>;
}

export interface FileQueryColumn {
  name: string;
  type: string;
}

export interface FileQueryResult {
  kind: "file_query";
  columns: FileQueryColumn[];
  rows: Array<Record<string, unknown>>;
  row_count: number;
  truncated: boolean;
  max_rows: number;
}

export interface RepoSummary {
  total_entries: number;
  total_files: number;
  total_dirs: number;
  total_pointers: number;
  total_lfs: number;
  total_submodules: number;
  total_bytes: string;
  /** Best-effort total commit count across all refs. 0 when unknown. */
  total_commits?: number;
  top_extensions: Array<{ ext: string; count: number; bytes: string }>;
}

export type HydrationState = "hydrated" | "pointer" | "partial" | "missing";

export interface HydrationEntry {
  path: string;
  state: HydrationState;
  on_disk: number;
  logical: number;
  kind: string;
}

export interface HydrationSummary {
  hydrated: number;
  pointer: number;
  partial: number;
  missing: number;
  on_disk_bytes: string;
  logical_bytes: string;
  total_tracked: number;
}

export interface HydrationStatusResult {
  items: HydrationEntry[];
  summary: HydrationSummary;
}

export interface PulseResult {
  commits: number;
  authors: number;
  files_changed: number;
  insertions: number;
  deletions: number;
  period_start: string; // ISO date
  period_end: string;   // ISO date
}

export interface CommitFrequencyBucket {
  period_start: string;
  period_end: string;
  count: number;
}

export interface CommitFrequencyResult {
  buckets: CommitFrequencyBucket[];
}

export interface CodeFrequencyWeek {
  week_start: string;
  additions: number;
  deletions: number;
}

export interface CodeFrequencyResult {
  weeks: CodeFrequencyWeek[];
}

export interface DedupSavingsResult {
  has_tracking: boolean;
  // Mode A: active tracking
  logical_bytes: number;
  stored_bytes: number;
  savings_pct: number;
  chunk_reuse_count: number;
  compression_ratio: number;
  // Mode B: no tracking — scan results
  large_files?: Array<{ path: string; size: number }>;
  estimated_savings_pct?: number;
  suggested_rules?: string[];
}

export interface BranchHealthEntry {
  name: string;
  last_commit_date: string;
  is_stale: boolean;
  ahead: number;
  behind: number;
}

export interface BranchHealthResult {
  total: number;
  stale: number;
  branches: BranchHealthEntry[];
}

export interface ReleaseTimelineResult {
  tags: Array<{ name: string; date: string }>;
}

export interface SizeHistoryResult {
  points: Array<{
    date: string;
    logical_bytes: number;
    pointer_bytes: number;
    git_bytes: number;
  }>;
}

export interface CommitSizeDistBucket {
  label: string;
  min: number;
  max: number;
  count: number;
}

export interface CommitSizeDistOutlier {
  sha: string;
  subject: string;
  lines_changed: number;
}

export interface CommitSizeDistResult {
  buckets: CommitSizeDistBucket[];
  outliers: CommitSizeDistOutlier[];
}

export interface ContributorGrowthMonth {
  month: string; // "2025-01"
  new_contributors: number;
  cumulative: number;
}

export interface ContributorGrowthResult {
  months: ContributorGrowthMonth[];
}

export interface AgentHealthWarning {
  level: "warn" | "error";
  code: string;
  message: string;
}

export interface AgentHealth {
  agent_version: string;
  crab_cli_version: string | null;
  cache_dir: string;
  cache_used_bytes: string;
  cache_free_bytes: string;
  staging_size_bytes: string;
  last_gc_at: string | null;
  warnings: AgentHealthWarning[];
}

export interface CommitRef {
  kind: "head" | "head_target" | "tag" | "branch" | "remote" | "other";
  name: string;
}

export interface CommitRow {
  sha: string;
  parents: string[];
  author: string;
  date: string;
  subject: string;
  /** Decoration (branches, tags, HEAD). Optional since earlier
   *  agent builds omitted this field — frontends should default to
   *  an empty array when missing. */
  refs?: CommitRef[];
  /** Number of files changed in this commit. Only present when
   *  `include_stats: true` was passed to the gitLog RPC. */
  files_changed?: number;
  /** Lines inserted. Only present with `include_stats: true`. */
  insertions?: number;
  /** Lines deleted. Only present with `include_stats: true`. */
  deletions?: number;
}

export interface CommitFileChange {
  path: string;
  /** "modified" for the common case (add/modify/delete), or
   *  "renamed" when git detected a rename/copy. */
  status: "modified" | "renamed";
  additions: number;
  deletions: number;
  /** Present only when `status === "renamed"`. */
  old_path?: string;
}

export interface CommitChanges {
  commit: {
    sha: string;
    parents: string[];
    author: string;
    date: string;
    subject: string;
  };
  is_merge: boolean;
  entries: CommitFileChange[];
}

export interface ProgressLine {
  op: string;
  job_id: string;
  stream: "stdout" | "stderr";
  line: string;
}

export interface ProgressComplete {
  op: string;
  job_id: string;
  status: number;
  outcome?: "completed" | "failed" | "cancelled";
}


export type FileTransfer = { fileId: string } | { paths: string[]; verb: "hydrate" | "dehydrate" | "restore" };

/** Incremental chunk from a streaming read handler. */
export interface ProgressPartial {
  op_id: string;
  kind: string;
  chunk: unknown;
}

/**
 * Patch anchor for hunk-level staging, unstaging, and discard.
 *
 * Mirrors the agent's `HunkAnchor` 1:1: `path` is the repo-relative
 * file path, the `old_*` / `new_*` fields echo the `@@ -x,y +a,b @@`
 * header, and `body` is the raw hunk body (the ` `, `+`, `-`
 * prefixed lines between two `@@` markers, verbatim — no re-
 * synthesis on our side). The agent uses the anchor to detect
 * stale hunks (concurrent edits between render and apply) and
 * rejects with a JSON-RPC error whose `code === 1005` and
 * `data.kind === "stale_hunk"`.
 */
export interface HunkAnchor {
  path: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  body: string;
}

/** JSON-RPC error code the agent uses for `StaleHunk` failures. */
export const STALE_HUNK_RPC_CODE = 1005;

/**
 * Narrow an unknown RPC failure to "is this a stale-hunk error?".
 * The `window.crab` bridge rejects with a plain `Error` whose
 * `code` field — if present — is the JSON-RPC `code`. Callers use
 * this to auto-refetch the patch instead of surfacing the raw
 * message to the user.
 */
export function isStaleHunkError(err: unknown): boolean {
  if (!err || typeof err !== "object") return false;
  const code = (err as { code?: unknown }).code;
  if (typeof code === "number" && code === STALE_HUNK_RPC_CODE) return true;
  // Some electron IPC surfaces lose the numeric code. Fall back to a
  // string probe against the attached `data.kind` or the message.
  const data = (err as { data?: { kind?: unknown } }).data;
  if (data && data.kind === "stale_hunk") return true;
  const msg = (err as { message?: unknown }).message;
  return typeof msg === "string" && msg.toLowerCase().includes("stale_hunk");
}

export type AgentEvent =
  | { method: "progress/line"; params: ProgressLine }
  | { method: "progress/complete"; params: ProgressComplete }
  | { method: "progress/transfer"; params: ProgressTransfer }
  | { method: "progress/partial"; params: ProgressPartial }
  | { method: "crab/add_progress"; params: AddProgressEvent }
  | { method: "crab/push_step"; params: PushStepEvent }
  | { method: "crab/push_progress"; params: PushProgressEvent }
  | { method: "agent/exit"; params: { code: number | null; signal: string | null } }
  | { method: string; params: unknown };

// ── Resumable operation types ──────────────────────────────────────

export type MergeStrategy = "MergeCommit" | "Squash" | "FastForwardOnly";

export type TodoActionInput =
  | "Pick"
  | "Reword"
  | "Squash"
  | "Fixup"
  | "Drop"
  | { Exec: string };

export interface TodoItemInput {
  sha: string;
  action: TodoActionInput;
  subject: string;
}

export type OpInput =
  | { kind: "Merge"; from: string; into: string; strategy: MergeStrategy; message?: string }
  | { kind: "Rebase"; onto: string; branch: string; todo?: TodoItemInput[] }
  | { kind: "CherryPick"; shas: string[]; target: string };

export interface OpResult {
  op_id: string;
  state: "done" | "paused" | "aborted";
  conflicts?: string[];
}

export interface OpStatusResult {
  op_id: string | null;
  state: string | null;
  op: OpInput | null;
  conflicts: string[];
  paused_reason: string | null;
}

// ── LSP Types (for window.crab.lsp bridge) ────────────────────────

export interface LspServerCapabilities {
  completionProvider?: unknown;
  hoverProvider?: unknown;
  definitionProvider?: unknown;
  typeDefinitionProvider?: unknown;
  referencesProvider?: unknown;
  documentSymbolProvider?: unknown;
  codeActionProvider?: unknown;
  documentFormattingProvider?: unknown;
  documentRangeFormattingProvider?: unknown;
  renameProvider?: unknown;
  signatureHelpProvider?: unknown;
  textDocumentSync?: unknown;
  [key: string]: unknown;
}

export interface LspDiscoveredServer {
  language: string;
  command: string;
  path: string;
  version?: string;
  /** Where the server was found. */
  source?: "bundled" | "installed" | "system";
}

export interface LspServerStatusInfo {
  language: string;
  state: "discovered" | "running" | "error" | "stopped" | "restarting";
  command?: string;
  error?: string;
}

export interface LspInstallProgress {
  language: string;
  phase: "resolving" | "downloading" | "extracting" | "done" | "error";
  percent?: number;
  downloaded?: number;
  total?: number;
  message?: string;
  installedPath?: string;
}

// ── Chat UI State (persisted per-task collapse/expand preferences) ───

export interface PersistedChatUIState {
  cardCollapse?: Record<string, { collapsed: boolean; lastToggledMs: number }>;
  groupCollapse?: Record<string, { collapsed: boolean; lastToggledMs: number }>;
  turnCollapse?: Record<number, { collapsed: boolean; lastToggledMs: number }>;
  compactView?: boolean;
  timelineRail?: "compact" | "off";
  vimNavigation?: boolean;
}

// ── Agent Workflow Types (renderer-side mirrors of electron/agent-workflows/types.ts) ──

export type AgentWorkflowNodeType = "agent_task" | "human_review" | "approval_gate" | "join" | "sub_workflow" | "if_else" | "loop";
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
  condition?: WorkflowNodeCondition;
  branches?: WorkflowIfElseBranch[];
}

export interface WorkflowIfElseBranch {
  id: string;
  label: string;
  kind: "if" | "elif" | "else";
  celExpression?: string;
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
  type: "builder" | "chat" | "schedule" | "sub_workflow" | "retry";
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
  sourceTaskId?: string;
  createSourceTask?: boolean;
  sourceTaskTitle?: string;
  input?: Record<string, unknown>;
  trigger?: WorkflowRunTriggerSource;
  parentRunId?: string;
  parentNodeRunId?: string;
  definitionSnapshot?: AgentWorkflowDefinition;
}

export interface WorkflowProjection {
  workflowDefinitionId: string;
  workflowRunId: string;
  nodeRunId: string;
  managed: true;
}

export interface AgentWorkflowVersionMeta {
  version: number;
  revisionId: string;
  name: string;
  timestamp: number;
}

export interface AgentWorkflowSimulationResult {
  activatedNodes: string[];
  skippedNodes: string[];
  blockedNodes: string[];
  activatedEdges: string[];
  skippedEdges: string[];
  trace: string[];
}

// ── Skill Manager Types (renderer-side mirrors of electron/skill-manager-types.ts) ──

export type SkillScope = "user" | "repo";

export interface SkillActivationPrefs {
  autoActivate: boolean;
  restrictToAgents: string[];
}

export interface SkillFrontmatter {
  name: string;
  description: string;
  license?: string;
  compatibility?: string;
  metadata?: Record<string, string>;
  'allowed-tools'?: string;
}

export interface SkillRecord {
  id: string;
  name: string;
  description: string;
  scope: SkillScope;
  repoPath: string | null;
  sourceUrl: string | null;
  installedAt: number;
  updatedAt: number;
  contentHash: string;
  enabled: boolean;
  activationPrefs: SkillActivationPrefs;
}

export interface DetectedAgent {
  id: string;
  name: string;
  configDir: string;
  installed: boolean;
}

export interface AgentSyncTarget {
  id: string;
  name: string;
  configDir: string;
  getSkillDir: (skillId: string) => string;
}

export interface SkillListItem {
  record: SkillRecord;
  frontmatter: SkillFrontmatter;
  bodyPreview: string;
  agentStatus: Record<string, "synced" | "agent_not_installed" | "error">;
}

export interface SkillDetail {
  record: SkillRecord;
  frontmatter: SkillFrontmatter;
  body: string;
  agentStatus: Record<string, "synced" | "agent_not_installed" | "error">;
}

export interface SkillOpResult<T = void> {
  success: boolean;
  data?: T;
  error?: string;
}

declare global {
  interface Window {
    crab: {
      rpc: (method: string, params: unknown) => Promise<unknown>;
      jupyterCheck: (pythonPath?: string) => Promise<JupyterRuntime>;
      jupyterListKernels: (pythonPath: string) => Promise<KernelSpec[]>;
      jupyterInstall: (pythonPath: string) => Promise<string>;
      jupyterStart: (id: string, repoId: string, pythonPath: string, target: KernelTarget) => Promise<void>;
      jupyterExecute: (id: string, code: string, requestId: string, cell: number) => Promise<KernelExecution>;
      jupyterInterrupt: (id: string) => Promise<void>;
      jupyterRestart: (id: string) => Promise<void>;
      jupyterShutdown: (id: string) => Promise<void>;
      onJupyterOutput: (listener: (event: KernelOutput & { id: string }) => void) => () => void;
      onJupyterStatus: (listener: (event: KernelStatus) => void) => () => void;
      pickRepoDirectory: () => Promise<string | null>;
      pickCloneDestination: () => Promise<string | null>;
      pickWorktreePath: () => Promise<string | null>;
      pickJsonFile: (title?: string) => Promise<{ path: string; content: string } | null>;
      updateMainSettings: (u: Record<string, unknown>) => Promise<unknown>;
      setDevtools: (open: boolean) => Promise<void>;
      getNoTelemetryFlag: () => Promise<boolean>;
      openLogsFolder: () => Promise<{ opened: string }>;
      // File system operations
      fsReveal: (fullPath: string) => Promise<void>;
      fsCreate: {
        (fullPath: string, kind: "file" | "directory"): Promise<{ path: string; kind: string }>;
        (repoId: string, fullPath: string, kind: "file" | "directory"): Promise<{ path: string; kind: string }>;
      };
      fsRename: {
        (oldPath: string, newPath: string): Promise<{ oldPath: string; newPath: string }>;
        (repoId: string, oldPath: string, newPath: string): Promise<{ oldPath: string; newPath: string }>;
      };
      fsDelete: {
        (fullPath: string, useTrash?: boolean): Promise<{ deleted: string; trash: boolean }>;
        (repoId: string, fullPath: string, useTrash?: boolean): Promise<{ deleted: string; trash: boolean }>;
      };
      fsOpenExternal: (fullPath: string) => Promise<void>;
      fsWriteFile: (repoId: string, fullPath: string, content: string, nativeSessionId: string | null) => Promise<{ path: string; written: number }>;
      fsReadFile: {
        (fullPath: string): Promise<{ exists: boolean; content: string | null }>;
        (repoId: string, fullPath: string, maxBytes: number | undefined, nativeSessionId: string | null): Promise<{ exists: boolean; content: string | null }>;
      };
      fsReadImage: (fullPath: string) => Promise<{
        exists: boolean;
        dataUrl: string | null;
        mimeType: string | null;
        bytes: number;
        tooLarge?: boolean;
      }>;
      // File system watcher
      fsWatchStart: (repoPath: string) => Promise<{ watching: boolean; path: string }>;
      fsWatchStop: (repoPath: string) => Promise<{ watching: boolean; path: string }>;
      // Repo capabilities
      getCapabilities: (repoId: string) => Promise<CapabilitySet>;
      // Dialogs (prompt/confirm replacements for sandboxed renderer)
      // Terminal — object-based API
      termSpawn: {
        // New object-based overload
        (opts: {
          cwd: string;
          ownerPath?: string;
          sessionType?: "daemon" | "in-process";
          shell?: string;
          env?: Record<string, string>;
          startupCommand?: string;
          agentMeta?: { taskId: string; agentType: string; worktreePath: string };
        }): Promise<{ id: string; resolvedSessionType: "daemon" | "in-process" }>;
        // Legacy positional overload (kept until all call sites migrate)
        (cwd: string, shell?: string, extraEnv?: Record<string, string>, startupCommand?: string): Promise<{ id: string; resolvedSessionType: "daemon" | "in-process" }>;
      };
      termWrite: (id: string, data: string) => Promise<void>;
      termResize: (id: string, cols: number, rows: number) => Promise<void>;
      termKill: (id: string) => Promise<void>;
      termAgentCommand: (agentType: string, command: string) => Promise<{ command: string; env?: Record<string, string> }>;
      termListShells: () => Promise<string[]>;
      termPoolHint: (cwd: string) => Promise<void>;
      termSaveState: (state: { persistenceKey: string; layout: unknown; scrollbackData: Record<string, string> }) => Promise<{ success: boolean; error?: string }>;
      termRestoreState: (persistenceKey: string) => Promise<{ layout: unknown | null; scrollbackData: Record<string, string> }>;
      termNotify: (req: { title: string; body: string; icon: "success" | "error"; terminalId: string; tabIndex: number }) => Promise<void>;
      termLogPath: (sessionId: string) => Promise<{ path: string; exists: boolean; sizeBytes: number }>;
      termLogRead: (sessionId: string) => Promise<{ content: string | null; error: string | null }>;
      termLogList: () => Promise<Array<{ sessionId: string; path: string; sizeBytes: number; mtimeMs: number }>>;
      termClearSession: (sessionId: string, persistenceKey: string) => Promise<{ success: boolean }>;
      termListDaemonSessions: (repoPath: string) => Promise<Array<{
        id: string;
        daemonSessionId: string;
        state: string;
        shellState?: string;
        isAlive: boolean;
        pid: number | null;
        initialCwd?: string | null;
        cwd: string | null;
        cols: number;
        rows: number;
        createdAt: number;
        lastDataAt: number;
        agentMeta?: {
          taskId: string;
          agentType: string;
          worktreePath: string;
          activityState?: string;
          stateChangedAt?: number;
        };
      }>>;
      termAttachDaemonSession: (daemonSessionId: string, repoPath: string) => Promise<{
        id: string;
        daemonSessionId: string;
        snapshot: string | null;
        cwd: string | null;
        summary: {
          id: string;
          daemonSessionId: string;
          state: string;
          shellState?: string;
          isAlive: boolean;
          pid: number | null;
          initialCwd?: string | null;
          cwd: string | null;
          cols: number;
          rows: number;
          createdAt: number;
          lastDataAt: number;
          agentMeta?: {
            taskId: string;
            agentType: string;
            worktreePath: string;
            activityState?: string;
            stateChangedAt?: number;
          };
        };
      }>;
      termKillDaemonSession: (daemonSessionId: string, repoPath: string) => Promise<void>;
      termDetachDaemonSession: (rendererId: string) => Promise<void>;
      // Reattach to existing daemon sessions for a repo path.
      termReattachSessions: (repoPath: string) => Promise<Array<{ id: string; snapshot: string | null; cwd: string | null }>>;
      // PTY daemon diagnostics and upgrade
      daemonDiagnostics: () => Promise<unknown>;
      daemonRestart: () => Promise<{ success: boolean; killedSessionCount: number; error?: string }>;
      daemonPrepareUpgrade: (opts?: { successorPath?: string }) => Promise<{ success: boolean; error?: string }>;
      // PTY mode — resolved at startup by mode-selection.ts
      getPtyMode: () => Promise<{
        resolvedMode: "daemon" | "in-process";
        configuredMode: "auto" | "daemon" | "in-process";
        fallback: { reason: string; error?: string; timestamp: number } | null;
      }>;
      // Subscribe to live PTY mode updates pushed from the main process.
      onPtyMode: (listener: (mode: { daemonReady: boolean; breakerOpen: boolean; configuredMode: "auto" | "daemon" | "in-process" }) => void) => () => void;
      onTermLogError: (listener: (evt: { sessionId: string; error: string }) => void) => () => void;
      onTermActivateTab: (listener: (evt: { terminalId: string; tabIndex: number }) => void) => () => void;
      onTermData: (listener: (evt: { id: string; data: string }) => void) => () => void;
      onTermExit: (listener: (evt: { id: string; exitCode: number }) => void) => () => void;
      onTermSaveRequest: (listener: () => void) => () => void;
      // Agent dashboard
      agentSessions: () => Promise<Array<{
        sessionId: string;
        taskId: string;
        agentType: string;
        worktreePath: string;
        state: string;
        stateChangedAt: number;
        elapsedWorkingMs: number;
        exitCode?: number;
      }>>;
      onAgentStateChange: (listener: (evt: { sessionId: string; state: string; previousState: string; timestamp: number }) => void) => () => void;
      keychain: {
        // Full keytar-style API (service + account)
        getPassword: (service: string, account: string) => Promise<string | null>;
        setPassword: (service: string, account: string, password: string) => Promise<{ ok: boolean }>;
        deletePassword: (service: string, account: string) => Promise<boolean>;
        // Simplified single-key API (uses "crab-desktop" as service)
        set: (key: string, value: string) => Promise<{ ok: boolean }>;
        get: (key: string) => Promise<string | null>;
        delete: (key: string) => Promise<{ ok: boolean }>;
        has: (key: string) => Promise<boolean>;
      };
      onEvent: (listener: (evt: AgentEvent) => void) => () => void;
      // LSP bridge
      lsp: {
        start: (language: string, rootPath: string) => Promise<LspServerCapabilities>;
        send: (language: string, message: string) => Promise<void>;
        stop: (language: string) => Promise<void>;
        discover: () => Promise<LspDiscoveredServer[]>;
        status: () => Promise<LspServerStatusInfo[]>;
        onMessage: (listener: (payload: { language: string; message: string }) => void) => () => void;
        onStatusChange: (listener: (payload: LspServerStatusInfo) => void) => () => void;
        install: (language: string) => Promise<LspDiscoveredServer>;
        installSupported: (language: string) => Promise<boolean>;
        onInstallProgress: (listener: (payload: LspInstallProgress) => void) => () => void;
      };
      // Workspace bridge
      workspace: {
        open: (rootPath: string) => Promise<{ ok: boolean; repoCount: number }>;
        create: () => Promise<{ ok: boolean; rootPath?: string; repoCount?: number; error?: string }>;
        close: () => Promise<{ ok: boolean }>;
        refresh: () => Promise<{ ok: boolean; repoCount?: number; error?: string }>;
        updateConfig: (config: Record<string, unknown>) => Promise<{ ok: boolean; error?: string }>;
        removeRepo: (relativePath: string) => Promise<{ ok: boolean; error?: string }>;
        fetchAll: () => Promise<{ ok: boolean; results: unknown[] }>;
        pullAll: () => Promise<{ ok: boolean; results: unknown[] }>;
        pickDirectory: () => Promise<string | null>;
        onDiscovered: (listener: (payload: { rootPath: string; repos: Array<{ absolutePath: string; relativePath: string; name: string }> }) => void) => () => void;
        onStatusUpdate: (listener: (statuses: Array<{ relativePath: string; branch: string | null; dirtyCount: number; ahead: number | null; behind: number | null; error: string | null }>) => void) => () => void;
      };
      // SSH enhanced operations
      ssh: {
        onStatusChanged: (listener: (event: { profileId: string; status: string; error?: string }) => void) => () => void;
        openTerminal: (profileId: string, cols: number, rows: number, cwd?: string, resumeId?: string, ownerPath?: string) => Promise<{ terminalId: string }>;
        detachTerminal: (terminalId: string) => Promise<void>;
        listProfiles: () => Promise<import("../electron/ssh-manager").SshProfile[]>;
        saveProfile: (profile: Partial<import("../electron/ssh-manager").SshProfile> & { host: string; username: string; password?: string }) => Promise<import("../electron/ssh-manager").SshProfile>;
        connect: (profileId: string) => Promise<void>;
        disconnect: (profileId: string) => Promise<void>;
        getStatus: (profileId: string) => Promise<import("../electron/ssh-manager").SshConnectionStatus>;
        listKnownHosts: () => Promise<KnownHost[]>;
        removeKnownHost: (hostname: string, port: number, keyType: string) => Promise<boolean>;
        openAsRepo: (profileId: string, path: string) => Promise<RepoHandle>;
        // Sync operations
        syncAddPair: (config: { profileId: string; localDir: string; remoteDir: string; ignorePatterns?: string[] }) => Promise<{ syncPairId: string }>;
        syncRemovePair: (syncPairId: string) => Promise<void>;
        syncStart: (syncPairId: string) => Promise<void>;
        syncPause: (syncPairId: string) => Promise<void>;
        syncListPairs: (profileId: string) => Promise<import("../electron/ssh-types").SyncPair[]>;
        syncListConflicts: (syncPairId: string) => Promise<import("../electron/ssh-types").SyncConflict[]>;
        syncResolveConflict: (conflictId: string, resolution: "local" | "remote" | "both") => Promise<void>;
        syncValidateLocalDir: (localDir: string, profileId: string) => Promise<{ valid: boolean; error?: string }>;
        // Transfer operations
        transferHistory: (query: { page: number; pageSize: number; search?: string; sortBy?: string; sortDir?: string; profileId?: string }) => Promise<{ entries: import("../electron/ssh-types").TransferRecord[]; total: number }>;
        transferStats: (profileId: string, period: string) => Promise<import("../electron/ssh-types").TransferStats>;
        transferListScheduled: (profileId: string) => Promise<import("../electron/ssh-types").ScheduledTransfer[]>;
        transferSchedule: (params: { profileId: string; direction: string; localPath: string; remotePath: string; scheduledAt: number }) => Promise<void>;
        transferCancelSchedule: (transferId: string) => Promise<void>;
        transferCancelBatch: (ids: string[]) => Promise<{ succeeded: number; failed: number }>;
        transferRetryBatch: (ids: string[]) => Promise<string[]>;
        transferClear: (ids: string[]) => Promise<void>;
        transferGetBandwidth: () => Promise<{ actual: number; limit: number }>;
        transferSetBandwidth: (limitKBps: number) => Promise<void>;
        transferSetCompression: (enabled: boolean) => Promise<void>;
        transferSetAdaptive: (enabled: boolean) => Promise<void>;
        queueDropUpload: (params: { profileId: string; remoteDir: string; files: Array<{ localPath: string; relativePath: string }> }) => Promise<{ error?: string } | undefined>;
        testConnection: (profileData: unknown) => Promise<{ ok: boolean; error?: string }>;
        // Event listeners
        onSyncStatus: (listener: (payload: unknown) => void) => () => void;
        offSyncStatus: (listener: (payload: unknown) => void) => void;
        onSyncFileStatus: (listener: (payload: { syncPairId: string; statuses: Record<string, string> }) => void) => () => void;
        offSyncFileStatus: (listener: (payload: unknown) => void) => void;
        onSyncConflict: (listener: (payload: import("../electron/ssh-types").SyncConflict) => void) => () => void;
        offSyncConflict: (listener: (payload: unknown) => void) => void;
        onSyncProgress: (listener: (payload: unknown) => void) => () => void;
        offSyncProgress: (listener: (payload: unknown) => void) => void;
        onSyncError: (listener: (payload: unknown) => void) => () => void;
        offSyncError: (listener: (payload: unknown) => void) => void;
        onDeployProgress: (listener: (payload: DeployProgress & { profileId: string }) => void) => () => void;
        offDeployProgress: (listener: (payload: unknown) => void) => void;
        onAgentCapabilities: (listener: (payload: unknown) => void) => () => void;
        offAgentCapabilities: (listener: (payload: unknown) => void) => void;
        onAgentUnavailable: (listener: (payload: { profileId: string; reason: string; message: string }) => void) => () => void;
        offAgentUnavailable: (listener: (payload: unknown) => void) => void;
        onTransferProgress: (listener: (payload: unknown) => void) => () => void;
        onTransferStatusChanged: (listener: (payload: unknown) => void) => () => void;
        onQualityChanged: (listener: (payload: unknown) => void) => () => void;
        onStreamData: (listener: (payload: unknown) => void) => () => void;
        onStreamEnd: (listener: (payload: unknown) => void) => () => void;
        onRemoteFileChanged: (listener: (payload: unknown) => void) => () => void;
      };
      // Remote workspace operations
      remoteWorkspace: {
        discover: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) => Promise<unknown>;
        open: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) => Promise<unknown>;
        close: (profileId: string) => Promise<unknown>;
        refresh: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) => Promise<unknown>;
        onDiscovered: (listener: (payload: { profileId: string; rootPath: string; repos: Array<{ absolute_path: string; relative_path: string; name: string }> }) => void) => () => void;
        onStatusUpdate: (listener: (payload: { profileId: string; statuses: Array<{ relative_path: string; branch: string | null; dirty_count: number; ahead: number | null; behind: number | null; error: string | null }>; poll_timestamp: number }) => void) => () => void;
      };
      // VFS mount operations
      mount: {
        list: () => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
        mount: (params: { repo: string; mountpoint: string; ref?: string; readOnly?: boolean }) => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
        unmount: (mountpoint: string) => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
        status: (mountpoint: string) => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
        refresh: (mountpoint: string) => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
        switch: (mountpoint: string, ref: string) => Promise<{ ok: boolean; error?: string; errorCode?: string; data?: unknown }>;
      };
      // Auto-updater
      updater: {
        checkForUpdate: () => Promise<{ success: boolean; version?: string | null; error?: string }>;
        downloadUpdate: () => Promise<{ success: boolean; error?: string }>;
        quitAndInstall: () => Promise<void>;
        getVersion: () => Promise<string>;
        onChecking: (listener: () => void) => () => void;
        onUpdateAvailable: (listener: (info: { version: string; releaseDate: string; releaseNotes: unknown }) => void) => () => void;
        onUpdateNotAvailable: (listener: (info: { version: string }) => void) => () => void;
        onUnavailable: (listener: (info: { message: string }) => void) => () => void;
        onProgress: (listener: (progress: { percent: number; bytesPerSecond: number; transferred: number; total: number }) => void) => () => void;
        onUpdateDownloaded: (listener: (info: { version: string; releaseDate: string; releaseNotes: unknown }) => void) => () => void;
        onError: (listener: (err: { message: string }) => void) => () => void;
      };
      openUrl: (url: string) => Promise<void>;
      // Orchestrator (agent task management)
      orchestrator: {
        getDiff: (taskId: string, checkpointSha?: string, checkpointFromSha?: string) => Promise<{ success: boolean; diff?: import("../electron/orchestrator-diff").TaskDiff; error?: string }>;
        restoreFiles: (taskId: string, sourceRef: string) => Promise<{ success: boolean; error?: string }>;
        revertFile: (taskId: string, filePath: string) => Promise<{ success: boolean; error?: string }>;
        listCheckpoints: (taskId: string) => Promise<{ success: boolean; checkpoints?: import("../electron/orchestrator-checkpoints").Checkpoint[]; backups?: import("../electron/orchestrator-checkpoints").RestoreBackup[]; error?: string }>;
        onCheckpointCreated: (listener: (event: { taskId: string; turnNumber: number; ref: string; sha: string }) => void) => () => void;
        onFilesChanged: (listener: (event: { taskIds: string[] }) => void) => () => void;
        onTaskStatus: (listener: (payload: { taskId: string; status: import("../electron/orchestrator").TaskStatus }) => void) => () => void;
        onNonconcurrentStop: (listener: (payload: { stoppedTaskId: string; startedTaskId: string }) => void) => () => void;
        onBudgetWarning: (listener: (payload: { taskId: string; cost: number; limit: number; workspaceTitle?: string }) => void) => () => void;
        retryTask: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        archiveTask: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        discard: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        restoreTask: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        loadChat: (taskId: string) => Promise<{ success: boolean; messages?: import("../electron/agent-chat-messages").ChatMessage[]; revision?: number; persistenceError?: string; error?: string }>;
        appendChatNote: (taskId: string, content: string, kind?: "text" | "error") => Promise<{ success: boolean; error?: string }>;
        onEvent: (listener: (payload: { taskId: string; event: import("../electron/orchestrator-adapters").AgentEvent }) => void) => () => void;
        onChatUpdate: (listener: (update: import("../electron/agent-chat-messages").ChatUpdate) => void) => () => void;
        onError: (listener: (payload: { taskId: string | null; message: string }) => void) => () => void;
        detectAgents: (options?: { force?: boolean; cachedOnly?: boolean; repoId?: string }) => Promise<{ success: boolean; agents?: import("../electron/orchestrator-adapters").DetectedAgent[]; cached?: boolean; error?: string }>;
        listModels: (agentType: string, cwd: string, requestId: string, taskId?: string) => Promise<{ success: boolean; models?: import("../electron/orchestrator-adapters").AgentModel[]; error?: string }>;
        cancelModelDiscovery: (requestId: string) => Promise<void>;
        iterate: (taskId: string, feedback: string, options?: import("../electron/orchestrator-adapters").AgentTurnOptions, messageId?: string) => Promise<{ success: boolean; uncertain?: boolean; unsent?: boolean; error?: string }>;
        respondInput: (taskId: string, requestId: string, response: import("../electron/orchestrator-adapters").AgentInputResponse) => Promise<{ success: boolean; error?: string }>;
        respondApproval: (taskId: string, payload: { requestId: string; decision: string; summary: string }) => Promise<{ success: boolean; error?: string }>;
        setTaskUIState: (taskId: string, state: PersistedChatUIState | null) => Promise<{ success: boolean; error?: string }>;
        getTaskUIState: (taskId: string) => Promise<{ success: boolean; state: PersistedChatUIState | null; error?: string }>;
        setTaskPinned: (taskId: string, pinned: boolean) => Promise<{ success: boolean; task?: unknown; error?: string }>;
        pinTask: (taskId: string, pinned: boolean) => Promise<{ success: boolean; task?: unknown; error?: string }>;
        renameTask: (taskId: string, title: string) => Promise<{ success: boolean; task?: unknown; error?: string }>;
        setTaskParent: (taskId: string, parentTaskId?: string | null) => Promise<{ success: boolean; task?: unknown; error?: string }>;
        setTaskSidebarGroup: (taskId: string, sidebarGroup?: string | null) => Promise<{ success: boolean; task?: unknown; error?: string }>;
        // Thread Operations (Codex app-server)
        forkThread: (taskId: string) => Promise<{ success: boolean; threadId: string; error?: string }>;
        compactThread: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        startReview: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        setThreadName: (taskId: string, name: string) => Promise<{ success: boolean; error?: string }>;
        setGoal: (taskId: string, mutation: import("../electron/orchestrator-adapters").CodexGoalMutation) => Promise<{ success: boolean; goal?: import("../electron/orchestrator-adapters").CodexGoal; error?: string }>;
        getGoal: (taskId: string) => Promise<{ success: boolean; goal?: import("../electron/orchestrator-adapters").CodexGoal | null; error?: string }>;
        clearGoal: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        updateThreadMetadata: (taskId: string, metadata: Record<string, unknown>) => Promise<{ success: boolean; error?: string }>;
        shellCommand: (taskId: string, command: string, cwd?: string) => Promise<{ success: boolean; result: unknown; error?: string }>;
        cleanBackgroundTerminals: (taskId: string) => Promise<{ success: boolean; error?: string }>;
        injectItems: (taskId: string, items: Array<{ role: string; content: unknown }>) => Promise<{ success: boolean; error?: string }>;
        // Command Execution (Codex app-server)
        execCommand: (taskId: string, params: { command: string; args?: string[]; cwd?: string; env?: Record<string, string>; stdin?: string; timeoutMs?: number }) => Promise<{ success: boolean; sessionId: string; error?: string }>;
        writeCommandStdin: (taskId: string, sessionId: string, stdin: string, closeStdin?: boolean) => Promise<{ success: boolean; error?: string }>;
        resizeCommandPty: (taskId: string, sessionId: string, cols: number, rows: number) => Promise<{ success: boolean; error?: string }>;
        terminateCommand: (taskId: string, sessionId: string) => Promise<{ success: boolean; error?: string }>;
        // Feedback (Codex app-server)
        uploadFeedback: (taskId: string, params: { turnId?: string; rating?: number; comment?: string; tags?: string[] }) => Promise<{ success: boolean; error?: string }>;
        // Filesystem (Codex app-server)
        fsReadFile: (path: string, taskId?: string) => Promise<{ success: boolean; content: string; error?: string }>;
        fsWriteFile: (path: string, content: string, taskId?: string) => Promise<{ success: boolean; error?: string }>;
        fsCreateDirectory: (path: string, taskId?: string) => Promise<{ success: boolean; error?: string }>;
        fsGetMetadata: (path: string, taskId?: string) => Promise<{ success: boolean; name: string; type: string; size: number; modifiedAt: number; permissions: number; error?: string }>;
        fsReadDirectory: (path: string, taskId?: string) => Promise<{ success: boolean; entries: Array<Record<string, unknown>>; error?: string }>;
        fsRemove: (path: string, recursive?: boolean, taskId?: string) => Promise<{ success: boolean; error?: string }>;
        fsCopy: (source: string, destination: string, taskId?: string) => Promise<{ success: boolean; error?: string }>;
        fsWatch: (path: string, taskId?: string) => Promise<{ success: boolean; watchId: string; error?: string }>;
        fsUnwatch: (watchId: string, taskId?: string) => Promise<{ success: boolean; error?: string }>;
        // Event Subscriptions (Codex app-server)
        onCommandOutput: (listener: (payload: { taskId: string; sessionId: string; delta: string }) => void) => () => void;
        onCommandExited: (listener: (payload: { taskId: string; sessionId: string; exitCode: number }) => void) => () => void;
        onFsChanged: (listener: (payload: { taskId: string; watchId: string; path: string; kind: string }) => void) => () => void;
        [key: string]: unknown;
      };
      // Agent Workflow Orchestration
      agentWorkflows: {
        listDefinitions: (repoPath: string) => Promise<{ success: boolean; definitions: AgentWorkflowDefinition[]; error?: string }>;
        getDefinition: (repoPath: string, definitionId: string) => Promise<{ success: boolean; definition: AgentWorkflowDefinition | null; error?: string }>;
        saveDefinition: (repoPath: string, definition: AgentWorkflowDefinition) => Promise<{ success: boolean; definition?: AgentWorkflowDefinition; error?: string }>;
        deleteDefinition: (repoPath: string, definitionId: string) => Promise<{ success: boolean; error?: string }>;
        listVersions: (definitionId: string) => Promise<{ success: boolean; versions: AgentWorkflowVersionMeta[]; error?: string }>;
        getVersion: (definitionId: string, version: number) => Promise<{ success: boolean; definition: AgentWorkflowDefinition | null; error?: string }>;
        revertToVersion: (repoPath: string, definitionId: string, version: number) => Promise<{ success: boolean; definition?: AgentWorkflowDefinition; error?: string }>;
        simulateRun: (definition: AgentWorkflowDefinition, input?: Record<string, unknown>) => Promise<{ success: boolean; result?: AgentWorkflowSimulationResult; error?: string }>;
        validateDefinition: (definition: AgentWorkflowDefinition) => Promise<AgentWorkflowValidationResult>;
        startRun: (repoPath: string, definitionId: string, options?: AgentWorkflowStartRunOptions) => Promise<{ success: boolean; run?: AgentWorkflowRun; sourceTask?: unknown; error?: string }>;
        getRun: (runId: string) => Promise<{ success: boolean; run: AgentWorkflowRun | null; error?: string }>;
        listRuns: (repoPath?: string) => Promise<{ success: boolean; runs: AgentWorkflowRun[]; error?: string }>;
        listEvents: (runId: string) => Promise<{ success: boolean; events: AgentWorkflowEvent[]; error?: string }>;
        cancelRun: (runId: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        pauseRun: (runId: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        resumeRun: (runId: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        approveNode: (runId: string, nodeRunId: string, summary?: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        requestNodeChanges: (runId: string, nodeRunId: string, feedback: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        requestHumanReviewChanges: (runId: string, nodeRunId: string, feedback: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        rejectNode: (runId: string, nodeRunId: string, reason: string) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        retryNode: (runId: string, nodeRunId: string, options?: { useLatestDefinition?: boolean }) => Promise<{ success: boolean; run?: AgentWorkflowRun; error?: string }>;
        listTriggers: (repoPath?: string) => Promise<{ success: boolean; triggers: AgentWorkflowTrigger[]; error?: string }>;
        saveTrigger: (trigger: AgentWorkflowTrigger) => Promise<{ success: boolean; trigger?: AgentWorkflowTrigger; error?: string }>;
        deleteTrigger: (triggerId: string) => Promise<{ success: boolean; error?: string }>;
        setTriggerEnabled: (triggerId: string, enabled: boolean) => Promise<{ success: boolean; trigger?: AgentWorkflowTrigger; error?: string }>;
        onRunUpdated: (listener: (payload: AgentWorkflowRun) => void) => () => void;
        onNodeEvent: (listener: (payload: AgentWorkflowEvent) => void) => () => void;
        onTriggerEvent: (listener: (payload: AgentWorkflowEvent) => void) => () => void;
      };
      // ─── Skill Manager ──────────────────────────────────────────────────
      skills: {
        listSkills: (opts?: { scope?: string; repoPath?: string }) => Promise<SkillOpResult<SkillListItem[]>>;
        getSkill: (id: string) => Promise<SkillOpResult<SkillDetail>>;
        createSkill: (data: { name: string; description: string; body?: string; scope: string; repoPath?: string }) => Promise<SkillOpResult<SkillDetail>>;
        updateSkill: (data: { id: string; updates: { name?: string; description?: string; body?: string } }) => Promise<SkillOpResult<SkillDetail>>;
        deleteSkill: (id: string) => Promise<SkillOpResult<void>>;
        installFromUrl: (url: string, scope: string, repoPath?: string) => Promise<SkillOpResult<SkillDetail>>;
        toggleEnabled: (id: string, enabled: boolean) => Promise<SkillOpResult<void>>;
        setActivationPrefs: (id: string, prefs: SkillActivationPrefs) => Promise<SkillOpResult<void>>;
        detectAgents: () => Promise<DetectedAgent[]>;
        resyncAll: () => Promise<SkillOpResult<{ synced: number; skipped: number; errors: number }>>;
      };
      // ─── Browser ──────────────────────────────────────────────────────
      browser: {
        registerGuest: (args: { browserTabId: string; worktreeId: string; webContentsId: number }) => Promise<boolean>;
        unregisterGuest: (args: { browserTabId: string }) => Promise<boolean>;
        openDevtools: (args: { browserTabId: string }) => Promise<boolean>;
        acceptDownload: (args: { downloadId: string }) => Promise<{ ok: boolean; reason?: string }>;
        cancelDownload: (args: { downloadId: string }) => Promise<boolean>;
        setViewportOverride: (args: { browserTabId: string; override: { width: number; height: number; deviceScaleFactor: number; mobile: boolean } | null }) => Promise<boolean>;
        executeJavaScript: (args: { browserTabId: string; code: string }) => Promise<{ ok: boolean; result?: unknown; error?: string }>;
        getTabState: (args: { browserTabId: string }) => Promise<{ browserTabId: string; webContentsId: number; rendererWebContentsId: number; worktreeId: string; url: string; title: string; loading: boolean; faviconUrl: string | null; canGoBack: boolean; canGoForward: boolean } | null>;
        openUrlExternal: (args: { url: string }) => Promise<void>;
        session: {
          listProfiles: () => Promise<Array<{ id: string; scope: string; partition: string; label: string; source: Record<string, unknown> }>>;
          createProfile: (args: { scope: string; label: string }) => Promise<{ id: string; scope: string; partition: string; label: string; source: Record<string, unknown> } | null>;
          deleteProfile: (args: { profileId: string }) => Promise<boolean>;
          resolvePartition: (args: { profileId: string | null }) => Promise<string | null>;
          importCookies: (args: { profileId: string }) => Promise<{ ok: boolean; reason?: string; summary?: { totalCookies: number; imported: number; skipped: number } }>;
          clearDefaultCookies: () => Promise<boolean>;
          detectBrowsers: () => Promise<Array<{ family: string | null; label: string; profiles: { name: string; directory: string }[]; selectedProfile: string }>>;
          importFromBrowser: (args: { profileId: string; browserFamily: string; browserProfile?: string }) => Promise<{ ok: boolean; reason?: string; summary?: { totalCookies: number; imported: number; skipped: number } }>;
        };
        // ── Grab & Annotation ──────────────────────────────────────
        setGrabMode: (args: { browserPageId: string; enabled: boolean }) => Promise<{ ok: true } | { ok: false; reason: string }>;
        awaitGrabSelection: (args: { browserPageId: string; opId: string }) => Promise<unknown>;
        cancelGrab: (args: { browserPageId: string }) => Promise<boolean>;
        captureSelectionScreenshot: (args: { browserPageId: string; rect: { x: number; y: number; width: number; height: number } }) => Promise<{ ok: boolean; screenshot?: { mimeType: string; dataUrl: string; width: number; height: number }; reason?: string }>;
        extractHoverPayload: (args: { browserPageId: string }) => Promise<{ ok: boolean; payload?: unknown; reason?: string }>;
        screenshotToClipboard: (args: { browserPageId: string }) => Promise<boolean>;
        setAnnotationViewportBridge: (args: { browserPageId: string; enabled: boolean; emitViewport: boolean; markers: Array<{ id: string; index: number; rectPage: { x: number; y: number; width: number; height: number }; rectViewport: { x: number; y: number; width: number; height: number }; isFixed: boolean }>; token: string }) => Promise<boolean>;
        getAnnotations: (args: { browserPageId: string }) => Promise<unknown[]>;
        annotateSelection: (args: { browserPageId: string; id: string; comment: string; intent: string; priority: string; payload: unknown }) => Promise<boolean>;
        deleteAnnotation: (args: { browserPageId: string; annotationId: string }) => Promise<boolean>;
      };
      // Browser events (main → renderer)
      onBrowserNavigated: (listener: (payload: { browserTabId: string; url: string }) => void) => () => void;
      onBrowserPageTitleUpdated: (listener: (payload: { browserTabId: string; title: string }) => void) => () => void;
      onBrowserDidStartLoading: (listener: (payload: { browserTabId: string }) => void) => () => void;
      onBrowserDidStopLoading: (listener: (payload: { browserTabId: string }) => void) => () => void;
      onBrowserPermissionDenied: (listener: (payload: { permission: string }) => void) => () => void;
      onBrowserDownloadRequested: (listener: (payload: { downloadId: string; browserTabId: string; origin: string; filename: string; totalBytes: number | null; mimeType: string | null }) => void) => () => void;
      onBrowserDownloadProgress: (listener: (payload: { downloadId: string; browserTabId: string; receivedBytes: number; totalBytes: number }) => void) => () => void;
      onBrowserDownloadFinished: (listener: (payload: { downloadId: string; browserTabId: string; status: string; savePath: string | null; error: string | null }) => void) => () => void;
      onBrowserContextMenu: (listener: (payload: { browserTabId: string; x: number; y: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean }) => void) => () => void;
      onBrowserDomReady: (listener: (payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => void) => () => void;
      onBrowserNavigationState: (listener: (payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => void) => () => void;
      onBrowserPageFaviconUpdated: (listener: (payload: { browserTabId: string; faviconUrl: string | null }) => void) => () => void;
      onBrowserCertificateError: (listener: (payload: { browserTabId: string; url: string; error: string; certificate: any }) => void) => () => void;
      onGrabModeToggle: (listener: (browserPageId: string) => void) => () => void;
      onGrabActionShortcut: (listener: (payload: { browserPageId: string; key: string }) => void) => () => void;
      onBrowserContextMenuRequested: (listener: (payload: { browserPageId: string; x: number; y: number; screenX: number; screenY: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean }) => void) => () => void;
      openUrlExternal: (url: string) => Promise<void>;
    };
  }
}

async function request<T>(method: string, params: unknown): Promise<T> {
  try {
    return (await window.crab.rpc(method, params)) as T;
  } catch (error) {
    if (error instanceof Error) throw error;
    if (error && typeof error === "object" && "message" in error && typeof error.message === "string") {
      const failure = new Error(error.message);
      if ("code" in error) Object.assign(failure, { code: error.code });
      if ("data" in error) Object.assign(failure, { data: error.data });
      throw failure;
    }
    throw error;
  }
}

export async function call<T>(method: string, params: unknown = {}, signal?: AbortSignal): Promise<T> {
  if (!signal) return request<T>(method, params);
  signal.throwIfAborted();
  const operation = callCancellable<T>(method, params);
  const cancel = () => operation.cancel();
  signal.addEventListener("abort", cancel, { once: true });
  try {
    return await operation.promise;
  } finally {
    signal.removeEventListener("abort", cancel);
  }
}

// ── Cancellable RPC ─────────────────────────────────────────────────

/** JSON-RPC error code the agent uses for cancelled operations. */
export { OP_CANCELLED_RPC_CODE } from "../electron/rpc-transport";

/**
 * Returns true when an RPC error represents a cancelled operation.
 * The renderer should silently discard these rather than surfacing
 * them to the user.
 */
export function isCancelledError(err: unknown): boolean {
  if (!err || typeof err !== "object") return false;
  const code = (err as { code?: unknown }).code;
  if (typeof code === "number" && code === OP_CANCELLED_RPC_CODE) return true;
  const data = (err as { data?: { kind?: unknown } }).data;
  if (data && data.kind === "op_cancelled") return true;
  const msg = (err as { message?: unknown }).message;
  return typeof msg === "string" && msg.toLowerCase().includes("op_cancelled");
}

export interface CancellableCall<T> {
  promise: Promise<T>;
  cancel: () => void;
}

/** Request cancellation; the original native reply remains the terminal result. */
export function callCancellable<T>(
  method: string,
  params: unknown = {},
): CancellableCall<T> {
  const op_id = crypto.randomUUID();
  let cancelled = false;
  let settled = false;
  const enriched: Record<string, unknown> = typeof params === "object" && params !== null
    ? { ...params, op_id }
    : { op_id };
  const cancellation: { op_id: string; repo_id?: string; native_session_id?: unknown } = { op_id };
  if (typeof enriched.repo_id === "string") cancellation.repo_id = enriched.repo_id;
  if (Object.hasOwn(enriched, "native_session_id")) cancellation.native_session_id = enriched.native_session_id;
  const promise = request<T>(method, enriched).finally(() => { settled = true; });
  const cancel = () => {
    if (cancelled || settled) return;
    cancelled = true;
    // Sending the signal does not establish cancellation. The work may have
    // already completed; only its reply establishes success, failure or cancel.
    void request("rpc/cancel", cancellation).catch(() => undefined);
  };
  return { promise, cancel };
}

// ── Cancellable RPC wrappers for long-running verbs ─────────────────

export function gitLogCancellable(
  repo_id: string,
  opts: { max?: number; skip?: number; from?: string; all?: boolean; include_stats?: boolean } = {},
): CancellableCall<{ commits: CommitRow[]; skip: number; total: number | null }> {
  return callCancellable("git_log", { repo_id, ...opts });
}

export function gitGrepCancellable(
  repo_id: string,
  query: string,
  opts: {
    ignore_case?: boolean;
    regex?: boolean;
    whole_word?: boolean;
    include_patterns?: string[];
    exclude_patterns?: string[];
    max_results?: number;
  } = {},
): CancellableCall<{
  matches: Array<{ path: string; line: number; content: string }>;
  truncated: boolean;
}> {
  return callCancellable("git_grep", { repo_id, query, ...opts });
}

export function searchReplaceCancellable(
  repo_id: string,
  query: string,
  replacement: string,
  opts: {
    ignore_case?: boolean;
    regex?: boolean;
    whole_word?: boolean;
    preserve_case?: boolean;
    file_paths?: string[];
  } = {},
): CancellableCall<{
  replacements: number;
  files: string[];
}> {
  return callCancellable("search_replace", { repo_id, query, replacement, ...opts });
}

export type DiffVersion =
  | { kind: "working" | "index" | "empty" }
  | { kind: "revision"; rev: string };

export interface ChunkDiffMetrics {
  old_source: "committed" | "staged" | "worktree";
  new_source: "committed" | "staged" | "worktree";
  old_chunks: number;
  new_chunks: number;
  unchanged_chunks: number;
  removed_chunks: number;
  added_chunks: number;
  old_bytes: number;
  new_bytes: number;
  unchanged_bytes: number;
  removed_bytes: number;
  added_bytes: number;
  signed_delta_bytes: number;
  reuse_ratio: number;
  changed_byte_ranges_old: Array<[number, number]>;
  changed_byte_ranges_new: Array<[number, number]>;
}

export interface LargeFileDiff {
  kind: "large_file";
  path: string;
  status: "unchanged" | "added" | "deleted" | "modified";
  before: { kind: "crab" | "lfs"; size: number; hash: string } | null;
  after: { kind: "crab" | "lfs"; size: number; hash: string } | null;
  before_exists: boolean;
  after_exists: boolean;
  old_size: number | null;
  new_size: number | null;
  chunk_metrics: ChunkDiffMetrics | null;
  metrics_note: string;
}

export interface NotebookComparison {
  kind: "notebook";
  path: string;
  /** Only a proven absent file is null. Empty or invalid content remains an error. */
  before: string | null;
  after: string | null;
}

export type FileComparison = { kind: "git"; path: string; patch: string } | LargeFileDiff | NotebookComparison;

export function fileDiffCancellable(
  repo_id: string,
  path: string,
  opts: { native_session_id: string; before: DiffVersion; after: DiffVersion; old_path?: string },
): CancellableCall<FileComparison> {
  return callCancellable("file_diff", { repo_id, path, ...opts });
}

// Logical repository identity is distinct from each native session's lifetime.
let repoCounter = 1;
function nextRepoId(prefix: string): string {
  return `${prefix}-${Date.now()}-${repoCounter++}`;
}

/** Recognize the object-storage schemes accepted by native SDK opening. */
export function isStorageRepositoryUrl(value: string): boolean {
  return /^(crab|s3|gs|gcs|az|azure):\/\//.test(value);
}

export function nextJobId(op: string): string {
  return `${op}-${crypto.randomUUID()}`;
}

export const rpc = {
  fileTransferHistory: () => call<FileTransferActivity>("activity_file_transfers", {}),
  fileTransferSources: (jobId: string, start: number, limit: number) => call<FileTransferSource[]>("activity_file_sources", { job_id: jobId, start, limit }),
  recoverFileTransferHistory: () => call<FileTransferActivity>("activity_file_transfers_recover", {}),
  ping: () => call<{ pong: string }>("ping"),

  managedProfiles: () => call<ManagedProfilesResponse>("managed_profiles"),

  managedSelectProfile: (authority: string) =>
    call<ManagedProfilesResponse>("managed_select_profile", { authority }),

  managedLogin: (service_origin: string, job_id = nextJobId("managed-login")) =>
    call<ManagedProfilesResponse>("managed_login", { service_origin, job_id }),

  managedOrganizations: (
    service?: string,
    cursor?: string,
    limit = 50,
  ) => call<ManagedOrganizationPage>("managed_organizations", { service, cursor, limit }),

  managedRepositories: (
    service: string,
    organization: string,
    cursor?: string,
    limit = 50,
  ) => call<ManagedRepositoryPage>("managed_repositories", {
    service,
    organization,
    cursor,
    limit,
  }),

  managedCreateRepository: (service: string, repository: string) =>
    call<ManagedRepository>("managed_create_repository", { service, repository }),

  openLocal: (path: string, storage_connection_id?: string | null, repo_id = nextRepoId("local")) =>
    call<RepoHandle>("open_local", { repo_id, path, ...(storage_connection_id !== undefined ? { storage_connection_id } : {}) }),

  reopen: async (path: string, repository?: Pick<RepoHandle, "repo_id" | "is_remote" | "url">): Promise<RepoHandle> => {
    const remote = parseRepoId(path);
    if (remote) return window.crab.ssh.openAsRepo(remote.profileId, remote.remotePath);
    if (isStorageRepositoryUrl(path)) return rpc.openUrl(path, undefined, repository?.repo_id);
    return rpc.openLocal(path, undefined, repository?.repo_id);
  },

  openUrl: (url: string, storage_connection_id?: string | null, repo_id = nextRepoId("url"), signal?: AbortSignal) =>
    call<RepoHandle>("open_url", { repo_id, url, ...(storage_connection_id !== undefined ? { storage_connection_id } : {}) }, signal),

  close: (repo_id: string) => call<{ closed: string }>("close", { repo_id }),

  authStatus: (repo_id: string) => call<AuthStatus>("auth_status", { repo_id }),

  list: (repo_id: string, opts: { prefix?: string; rev?: string } = {}) =>
    call<{ entries: DirEntry[] }>("list", { repo_id, ...opts }),

  listRefs: (
    repo_id: string,
    opts: { kinds?: ("branch" | "tag" | "remote")[]; max?: number } = {},
  ) => call<{ refs: RefEntry[] }>("list_refs", { repo_id, ...opts }),

  stat: (repo_id: string, path: string, rev?: string) =>
    call<FileStat>("stat", { repo_id, path, rev }),

  openMedia: (repo_id: string, path: string, { signal, ...opts }: { native_session_id: string; rev?: string; signal?: AbortSignal }) =>
    call<MediaSource>("media_open", { repo_id, path, ...opts }, signal),

  closeMedia: (media_id: string) => call<{ closed: string }>("media_close", { media_id }),

  openFile: (repo_id: string, path: string, { signal, ...opts }: { native_session_id: string; rev?: string; signal?: AbortSignal }) =>
    call<CapturedFile>("file_open", { repo_id, path, ...opts }, signal),

  fileChunks: (repo_id: string, file_id: string, native_session_id: string, start: number, limit: number, signal?: AbortSignal) =>
    call<FileChunkPage>("file_chunks", { repo_id, file_id, native_session_id, start, limit }, signal),

  closeFile: (repo_id: string, file_id: string, native_session_id: string) => call<{ closed: string }>("file_close", { repo_id, file_id, native_session_id }),

  fileTransfer: (repo_id: string, selection: FileTransfer, op_id: string, native_session_id: string) =>
    "fileId" in selection
      ? call<{ written_bytes: number }>("file_materialize", { repo_id, file_id: selection.fileId, op_id, native_session_id })
      : call<{ written_bytes: number }>(selection.verb === "restore" ? "files_restore" : selection.verb === "dehydrate" ? "files_dehydrate" : "files_materialize", { repo_id, paths: selection.paths, op_id, native_session_id }),

  cancelOperation: (repo_id: string, op_id: string, native_session_id: string) => call<{ ok: boolean }>("rpc/cancel", { repo_id, op_id, native_session_id }),

  readText: (repo_id: string, path: string, { signal, ...opts }: { native_session_id?: string; rev?: string; max_bytes?: number; signal?: AbortSignal } = {}) =>
    call<ReadTextResult>("read_text", { repo_id, path, ...opts }, signal),

  readBytes: (repo_id: string, path: string, { signal, ...opts }: { native_session_id?: string; rev?: string; max_bytes?: number; signal?: AbortSignal } = {}) =>
    call<ReadBytesResult>("read_bytes", { repo_id, path, ...opts }, signal),

  writeText: (repo_id: string, path: string, content: string) =>
    call<{ written: boolean; size: number }>("write_text", { repo_id, path, content }),

  writeBytes: (repo_id: string, path: string, bytes_b64: string) =>
    call<{ written: boolean; size: number }>("write_bytes", { repo_id, path, bytes_b64 }),

  readRange: (repo_id: string, file_id: string, offset: number, length: number, signal?: AbortSignal) =>
    call<{ offset: number; length: number; bytes_b64: string; file_size: number }>("read_range", {
      repo_id, file_id, offset, length,
    }, signal),

  previewParquet: (
    repo_id: string,
    path: string,
    { signal, ...opts }: { native_session_id?: string; rev?: string; max_rows?: number; signal?: AbortSignal } = {},
  ) => call<ParquetPreview>("preview_parquet", { repo_id, path, ...opts }, signal),

  previewSafetensors: (repo_id: string, path: string, { signal, ...opts }: { rev?: string; signal?: AbortSignal } = {}) =>
    call<SafetensorsPreview>("preview_safetensors", { repo_id, path, ...opts }, signal),

  previewNpy: (repo_id: string, path: string, { signal, ...opts }: { rev?: string; signal?: AbortSignal } = {}) =>
    call<NpyPreview>("preview_npy", { repo_id, path, ...opts }, signal),

  previewNpz: (repo_id: string, path: string, { signal, ...opts }: { rev?: string; signal?: AbortSignal } = {}) =>
    call<NpzPreview>("preview_npz", { repo_id, path, ...opts }, signal),

  previewZip: (repo_id: string, path: string, { signal, ...opts }: { rev?: string; signal?: AbortSignal } = {}) =>
    call<ZipPreview>("preview_zip", { repo_id, path, ...opts }, signal),

  repoSummary: (repo_id: string, opts: { rev?: string } = {}) =>
    call<RepoSummary>("repo_summary", { repo_id, ...opts }),

  gitStatus: (repo_id: string) =>
    call<{ entries: Array<{ path: string; code: string }> }>("git_status_porcelain", { repo_id }),

  /** Working-tree-vs-index diff as a single unified patch. Used by
   *  the Changes view's right pane so the user sees exactly what
   *  `git apply --cached` would stage. */
  gitDiffIndexPatch: (
    repo_id: string,
    { signal, ...opts }: { native_session_id?: string; path?: string; include_untracked?: boolean; signal?: AbortSignal } = {},
  ) => call<{ patch: string }>("git_diff_index_patch", { repo_id, ...opts }, signal),

  /** Index-vs-HEAD diff as a single unified patch — the staged
   *  side of `git diff`. Paired with `gitDiffIndexPatch` to derive
   *  each file's tri-state checkbox (empty / partial / full) in the
   *  Changes view without a per-file round trip. */
  gitDiffCachedPatch: (
    repo_id: string,
    { signal, ...opts }: { native_session_id?: string; path?: string; signal?: AbortSignal } = {},
  ) => call<{ patch: string }>("git_diff_cached_patch", { repo_id, ...opts }, signal),

  /** Low-level: apply a raw unified patch. `cached` stages it,
   *  `reverse` undoes it. The hunk-level verbs below are the
   *  ergonomic wrapper for the common case. */
  gitApplyPatch: (
    repo_id: string,
    patch: string,
    opts: { cached?: boolean; reverse?: boolean } = {},
  ) =>
    call<{ applied: boolean; cached: boolean; reverse: boolean }>("git_apply_patch", {
      repo_id,
      patch,
      ...opts,
    }),

  /** Stage a set of hunks. The agent validates each anchor against
   *  the current working tree before applying; a concurrent edit
   *  surfaces as a JSON-RPC error with `code === 1005` / `data.kind
   *  === "stale_hunk"`. */
  gitStageHunks: (repo_id: string, hunks: HunkAnchor[]) =>
    call<{ staged: number }>("git_stage_hunks", { repo_id, hunks }),

  gitUnstageHunks: (repo_id: string, hunks: HunkAnchor[]) =>
    call<{ unstaged: number }>("git_unstage_hunks", { repo_id, hunks }),

  gitDiscardHunks: (repo_id: string, hunks: HunkAnchor[]) =>
    call<{ discarded: number }>("git_discard_hunks", { repo_id, hunks }),

  /** Stage files by path via `git add`. Works for untracked files
   *  where `git diff` produces no patch. */
  gitAddPaths: (repo_id: string, paths: string[]) =>
    call<{ staged: number }>("git_add_paths", { repo_id, paths }),

  /** Unstage files by path via `git reset HEAD`. Works for files
   *  staged with `git add` including previously untracked files. */
  gitResetPaths: (repo_id: string, paths: string[]) =>
    call<{ unstaged: number }>("git_reset_paths", { repo_id, paths }),

  hydrationStatus: (repo_id: string, opts: { rev?: string } = {}) =>
    call<HydrationStatusResult>("hydration_status", { repo_id, ...opts }),

  gitLog: (repo_id: string, opts: { max?: number; skip?: number; from?: string; all?: boolean; path?: string; include_stats?: boolean } = {}) =>
    call<{ commits: CommitRow[]; skip: number; total: number | null }>("git_log", { repo_id, ...opts }),

  gitBlame: (repo_id: string, path: string, opts: { rev?: string } = {}) =>
    call<{ lines: Array<{ sha: string; author: string; line: number; content: string }> }>("git_blame", { repo_id, path, ...opts }),

  gitStashList: (repo_id: string) =>
    call<{ stashes: Array<{ ref: string; message: string }> }>("git_stash_list", { repo_id }),

  gitStashApply: (repo_id: string, stash_ref: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_stash_apply", {
      repo_id,
      stash_ref,
    }),

  gitStashPop: (repo_id: string, stash_ref: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_stash_pop", {
      repo_id,
      stash_ref,
    }),

  gitStashDrop: (repo_id: string, stash_ref: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_stash_drop", {
      repo_id,
      stash_ref,
    }),

  gitStashSave: (
    repo_id: string,
    opts: { message?: string; include_untracked?: boolean } = {},
  ) =>
    call<{ status: number; stdout: string; stderr: string }>("git_stash_save", {
      repo_id,
      ...opts,
    }),

  gitStashShow: (repo_id: string, stash_ref: string) =>
    call<{ files: Array<{ status: string; path: string }> }>("git_stash_show", {
      repo_id,
      stash_ref,
    }),

  gitStashShowPatch: (repo_id: string, stash_ref: string) =>
    call<{ patch: string }>("git_stash_show_patch", {
      repo_id,
      stash_ref,
    }),

  // ---- Git worktree actions ----

  gitWorktreeList: (repo_id: string) =>
    call<{ worktrees: WorktreeEntry[] }>("git_worktree_list", { repo_id }),

  gitWorktreeAdd: (
    repo_id: string,
    opts: {
      path: string;
      branch?: string;
      create_branch?: boolean;
      start_point?: string;
      detach?: boolean;
      hydration?: "all" | "on_access" | "pointers_only";
    },
  ) =>
    call<{ created: boolean; path: string; worktree: WorktreeEntry | null }>(
      "git_worktree_add",
      { repo_id, ...opts },
    ),

  gitWorktreeRemove: (repo_id: string, path: string, opts: { force?: boolean } = {}) =>
    call<{ removed: string }>("git_worktree_remove", { repo_id, path, ...opts }),

  gitWorktreeMove: (repo_id: string, old_path: string, new_path: string) =>
    call<{ old_path: string; new_path: string }>("git_worktree_move", {
      repo_id,
      old_path,
      new_path,
    }),

  gitWorktreePrune: (repo_id: string, opts: { dry_run?: boolean; job_id?: string } = {}) =>
    call<{ dry_run: boolean; pruned_lines: string[] }>("git_worktree_prune", {
      repo_id,
      ...opts,
    }),

  gitWorktreeLock: (repo_id: string, path: string, opts: { reason?: string } = {}) =>
    call<{ locked: string; reason: string | null }>("git_worktree_lock", {
      repo_id,
      path,
      ...opts,
    }),

  gitWorktreeUnlock: (repo_id: string, path: string) =>
    call<{ unlocked: string }>("git_worktree_unlock", { repo_id, path }),

  gitWorktreeRepair: (repo_id: string, paths: string[] = []) =>
    call<{ repaired: boolean; paths: string[] }>("git_worktree_repair", { repo_id, paths }),

  // ---- Worktree manager (hydrated) ----

  /** List all worktrees with hydrated metadata from worktrees.json. */
  worktreeList: (repo_id: string) =>
    call<{ worktrees: HydratedWorktreeEntry[] }>("worktree_list", { repo_id }),

  /** Get dirty-file count + ahead/behind for a specific worktree. */
  worktreeStatus: (repo_id: string, path: string) =>
    call<WorktreeStatus>("worktree_status", { repo_id, path }),

  /** Update display name / colour / pin for a worktree. */
  worktreeSetMeta: (
    repo_id: string,
    meta: { path: string; display_name?: string | null; colour?: string | null; pinned?: boolean },
  ) =>
    call<{ ok: boolean }>("worktree_set_meta", { repo_id, ...meta }),

  /** Estimated disk usage (hydrated bytes + git objects) for a worktree. */
  worktreePushEstimate: (repo_id: string, path: string) =>
    call<PushEstimate>("worktree_push_estimate", { repo_id, path }),

  /** Repair moved or relocated worktree administrative links. */
  worktreeRepair: (repo_id: string, paths: string[] = []) =>
    call<{ repaired: boolean; paths: string[] }>("worktree_repair", { repo_id, paths }),

  /** List files changed between two refs (branches/commits). Used by
   *  the cross-worktree comparison view to enumerate files before
   *  fetching per-file diffs. Falls back to `git diff --name-only`. */
  gitDiffNameOnly: (repo_id: string, base: string, head: string) =>
    call<{ files: Array<{ path: string; status: string }> }>("git_diff_name_only", {
      repo_id,
      base,
      head,
    }),

  pointerTop: (repo_id: string, opts: { rev?: string; top_n?: number } = {}) =>
    call<{
      entries: Array<{
        path: string;
        size: number;
        kind: string;
        pointer_hash: string | null;
      }>;
    }>("pointer_top", { repo_id, ...opts }),

  gitCommitChanges: (repo_id: string, sha: string) =>
    call<CommitChanges>("git_commit_changes", { repo_id, sha }),

  resolveRef: (repo_id: string, name: string) =>
    call<{ name: string; sha: string }>("resolve_ref", { repo_id, name }),

  gitGrep: (repo_id: string, query: string, opts: {
    ignore_case?: boolean;
    regex?: boolean;
    whole_word?: boolean;
    include_patterns?: string[];
    exclude_patterns?: string[];
    max_results?: number;
  } = {}) =>
    call<{
      matches: Array<{ path: string; line: number; content: string }>;
      truncated: boolean;
    }>("git_grep", { repo_id, query, ...opts }),

  about: () =>
    call<{
      agent_version: string;
      crab_cli_version: string | null;
      os: string;
      arch: string;
    }>("about"),

  agentHealth: () =>
    call<AgentHealth>("agent_health"),

  indexerStatus: () =>
    call<IndexerStatus>("indexer_status"),

  cacheStats: (repo_id?: string) => call<CacheStats>("cache_stats", { repo_id }),
  cacheSetBudget: (repo_id: string | undefined, max_bytes: number | null) =>
    call<CacheBudgetResult>("cache_set_budget", { repo_id, max_bytes }),

  cacheClean: (repo_id?: string, dry_run = false) =>
    call<CacheCleanResult>("cache_clean", { repo_id, dry_run }),

  clone: (url: string, dest: string, jobId = nextJobId("clone"), token?: string, provider?: string, storage_connection_id?: string) =>
    call<{ status: number; dest: string }>("clone", {
      job_id: jobId,
      url,
      dest,
      ...(token ? { token } : {}),
      ...(provider ? { provider } : {}),
      ...(storage_connection_id ? { storage_connection_id } : {}),
    }),

  status: (cwd: string, jobId = nextJobId("status")) =>
    call<{ status: number }>("status", { job_id: jobId, cwd }),

  push: (cwd: string, jobId = nextJobId("push")) =>
    call<{ status: number }>("push", { job_id: jobId, cwd }),

  add: (cwd: string, paths: string[], jobId = nextJobId("add")) =>
    call<{ status: number }>("add", { job_id: jobId, cwd, paths }),

  // ---- Git write actions ----

  gitCheckout: (repo_id: string, target: string, opts: { detach?: boolean } = {}) =>
    call<{ ref_name: string; detached: boolean }>("git_checkout", { repo_id, ref_name: target, ...opts }),

  gitBranchCreate: (repo_id: string, name: string, opts: { start_point?: string; track?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_branch_create", { repo_id, name, ...opts }),

  gitBranchDelete: (repo_id: string, name: string, opts: { force?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_branch_delete", { repo_id, name, ...opts }),

  gitBranchRename: (repo_id: string, old_name: string, new_name: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_branch_rename", { repo_id, old_name, new_name }),

  gitSetUpstream: (repo_id: string, branch: string, upstream: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_set_upstream", { repo_id, branch, upstream }),

  gitTagAdd: (repo_id: string, name: string, opts: { target?: string; message?: string } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_tag_add", { repo_id, name, ...opts }),

  gitTagDelete: (repo_id: string, name: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_tag_delete", { repo_id, name }),

  gitCherryPick: (repo_id: string, sha: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_cherry_pick", { repo_id, sha }),

  gitRevert: (repo_id: string, sha: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_revert", { repo_id, sha }),

  gitMerge: (repo_id: string, branch: string, opts: { no_ff?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_merge", { repo_id, branch, ...opts }),

  gitRebase: (repo_id: string, upstream: string, opts: { onto?: string } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_rebase", { repo_id, upstream, ...opts }),

  gitReset: (repo_id: string, target: string, opts: { mode?: "soft" | "mixed" | "hard" } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_reset", { repo_id, target, ...opts }),

  gitFetch: (repo_id: string, opts: { remote?: string; prune?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_fetch", { repo_id, ...opts }),

  gitPull: (repo_id: string, opts: { rebase?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_pull", { repo_id, ...opts }),

  gitFetchBranch: (repo_id: string, branch: string, opts: { remote?: string } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_fetch_branch", { repo_id, branch, ...opts }),

  gitPullBranch: (repo_id: string, branch: string, opts: { remote?: string } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_pull_branch", { repo_id, branch, ...opts }),

  gitPushBranch: (repo_id: string, branch: string, opts: { remote?: string; set_upstream?: boolean } = {}) =>
    call<{ status: number; stdout: string; stderr: string }>("git_push_branch", { repo_id, branch, ...opts }),

  gitFetchAll: (repo_id: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_fetch_all", { repo_id }),

  gitPullMode: (repo_id: string, opts: { mode: "merge" | "rebase" | "ff-only" }) =>
    call<{ status: number; stdout: string; stderr: string }>("git_pull_mode", { repo_id, ...opts }),

  gitPushRemote: (
    repo_id: string,
    branch: string,
    opts: { force?: boolean; force_with_lease?: boolean; set_upstream?: boolean } = {},
  ) =>
    call<{ status: number; stdout: string; stderr: string }>("git_push_remote", { repo_id, branch, ...opts }),

  gitCommit: (
    repo_id: string,
    opts: {
      message: string;
      paths?: string[];
      amend?: boolean;
      sign_off?: boolean;
      sign?: boolean;
      author_date?: "keep" | "now" | string;
      co_authored_by?: string[];
    },
  ) =>
    call<{ sha: string; subject: string; amend: boolean }>(opts.paths === undefined ? "git_commit" : "git_commit_paths", {
      repo_id,
      ...opts,
    }),

  gitConfigGet: (repo_id: string, keys: string[]) =>
    call<{ values: Record<string, string | null> }>("git_config_get", { repo_id, keys }),

  gitActivity: (repo_id: string) =>
    call<{ days: Array<{ date: string; count: number }> }>("git_activity", { repo_id }),

  gitContributors: (repo_id: string, opts: { since?: string } = {}) =>
    call<{ contributors: Array<{ name: string; commits: number }> }>("git_contributors", { repo_id, ...opts }),

  gitPulse: (repo_id: string, opts: { days?: number } = {}) =>
    call<PulseResult>("git_pulse", { repo_id, ...opts }),

  gitCommitFrequency: (
    repo_id: string,
    opts: { granularity?: "day" | "week" | "month"; periods?: number } = {},
  ) =>
    call<CommitFrequencyResult>("git_commit_frequency", { repo_id, ...opts }),

  gitCodeFrequency: (
    repo_id: string,
    opts: { weeks?: number } = {},
  ) =>
    call<CodeFrequencyResult>("git_code_frequency", { repo_id, ...opts }),

  gitBranchHealth: (
    repo_id: string,
    opts: { stale_days?: number } = {},
  ) =>
    call<BranchHealthResult>("git_branch_health", { repo_id, ...opts }),

  gitReleaseTimeline: (repo_id: string) =>
    call<ReleaseTimelineResult>("git_release_timeline", { repo_id }),

  repoSizeHistory: (
    repo_id: string,
    opts: { points?: number } = {},
  ) =>
    call<SizeHistoryResult>("repo_size_history", { repo_id, ...opts }),

  gitCommitSizeDistribution: (repo_id: string) =>
    call<CommitSizeDistResult>("git_commit_size_distribution", { repo_id }),

  gitContributorGrowth: (
    repo_id: string,
    opts: { months?: number } = {},
  ) =>
    call<ContributorGrowthResult>("git_contributor_growth", { repo_id, ...opts }),

  dedupSavings: (repo_id: string) =>
    call<DedupSavingsResult>("dedup_savings", { repo_id }),

  previewSqlite: (repo_id: string, path: string, { signal, ...opts }: { native_session_id: string; rev?: string; signal?: AbortSignal }) =>
    call<{
      kind: "sqlite";
      file_size: number;
      table_count: number;
      tables: Array<{
        name: string;
        columns: Array<{ name: string; type: string; notnull: boolean; pk: boolean }>;
        row_count: number;
        preview_rows: Array<Record<string, unknown>>;
      }>;
    }>("preview_sqlite", { repo_id, path, ...opts }, signal),

  fileQuery: (
    repo_id: string,
    path: string,
    sql: string,
    opts: { native_session_id: string; rev?: string; max_rows?: number },
  ) =>
    callCancellable<FileQueryResult>("file_query", { repo_id, path, sql, ...opts }),

  previewOnnx: (repo_id: string, path: string, opts: { rev?: string } = {}) =>
    call<{
      kind: "onnx";
      file_size: number;
      ir_version: number;
      producer_name: string;
      producer_version: string;
      domain: string;
      model_version: number;
      doc_string: string;
      graph_name: string;
      node_count: number;
      inputs: string[];
      outputs: string[];
      opsets: Array<{ domain: string; version: number }>;
    }>("preview_onnx", { repo_id, path, ...opts }),

  previewHdf5: (repo_id: string, path: string, opts: { rev?: string } = {}) =>
    call<{
      kind: "hdf5";
      file_size: number;
      superblock_version: number;
      items: Array<{ kind: string; name: string }>;
      h5dump_available: boolean;
    }>("preview_hdf5", { repo_id, path, ...opts }),

  previewTorchCheckpoint: (repo_id: string, path: string, { signal, ...opts }: { rev?: string; signal?: AbortSignal } = {}) =>
    call<{
      kind: "torch_checkpoint";
      file_size: number;
      format: string;
      tensor_count: number;
      has_optimizer_state: boolean;
      zip_entries: Array<{ name: string; size: number }>;
    }>("preview_torch_checkpoint", { repo_id, path, ...opts }, signal),

  previewProtobuf: (repo_id: string, path: string, opts: { rev?: string } = {}) =>
    call<{
      kind: "protobuf";
      file_size: number;
      file_count: number;
      files: Array<{
        name: string;
        package: string;
        syntax: string;
        messages: Array<{ name: string; fields: Array<{ name: string; number: number; type: string; label: string }> }>;
        services: Array<{ name: string; methods: Array<{ name: string; input_type: string; output_type: string }> }>;
        enums: Array<{ name: string; values: string[] }>;
      }>;
    }>("preview_protobuf", { repo_id, path, ...opts }),

  previewStl: (repo_id: string, path: string, opts: { rev?: string } = {}) =>
    call<{
      kind: "stl";
      file_size: number;
      is_ascii: boolean;
      triangle_count: number;
      bounds: { min: [number, number, number]; max: [number, number, number] };
      preview_vertices: Array<[number, number, number]>;
    }>("preview_stl", { repo_id, path, ...opts }),

  gitBranchInfo: (repo_id: string, branch: string, opts: { base?: string } = {}) =>
    call<{
      branch: string;
      is_current: boolean;
      upstream: string | null;
      ahead: number;
      behind: number;
      upstream_ahead: number;
      upstream_behind: number;
      base: string;
      commits: Array<{ sha: string; author: string; date: string; subject: string }>;
      changed_files: Array<{ path: string; additions: number; deletions: number }>;
      total_additions: number;
      total_deletions: number;
      created_at: string | null;
    }>("git_branch_info", { repo_id, branch, ...opts }),

  gitRepoInfo: (repo_id: string) =>
    call<{
      remotes: Array<{ name: string; url: string; type: string }>;
      config: Array<{ key: string; value: string }>;
      hooks: string[];
      submodules: Array<{ sha: string; path: string; describe: string }>;
      head_sha: string;
      head_branch: string;
      git_dir_size: string;
      work_dir: string;
    }>("git_repo_info", { repo_id }),

  // ---- Remote management ----

  gitRemoteAdd: (repo_id: string, name: string, url: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_remote_add", { repo_id, name, url }),

  gitRemoteRemove: (repo_id: string, name: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_remote_remove", { repo_id, name }),

  gitRemoteSetUrl: (repo_id: string, name: string, url: string) =>
    call<{ status: number; stdout: string; stderr: string }>("git_remote_set_url", { repo_id, name, url }),

  // ---- Resumable operations (merge / rebase / cherry-pick) ----

  opStart: (repo_id: string, op: OpInput) =>
    call<OpResult>("op_start", { repo_id, op }),

  opContinue: (repo_id: string, op_id: string) =>
    call<OpResult>("op_continue", { repo_id, op_id }),

  opAbort: (repo_id: string, op_id: string) =>
    call<OpResult>("op_abort", { repo_id, op_id }),

  opStatus: (repo_id: string, op_id?: string) =>
    call<OpStatusResult>("op_status", { repo_id, op_id: op_id ?? null }),

  opEditTodo: (repo_id: string, op_id: string, todo: TodoItemInput[]) =>
    call<{ op_id: string; updated: boolean }>("op_edit_todo", { repo_id, op_id, todo }),

  // ---- Conflict resolution ----

  conflictList: (repo_id: string) =>
    call<{ files: Array<{ path: string; status: string }> }>("conflict_list", { repo_id }),

  conflictReadSides: (repo_id: string, path: string) =>
    call<{ base: string | null; current: string | null; incoming: string | null }>(
      "conflict_read_sides",
      { repo_id, path },
    ),

  gitApplyResolution: (repo_id: string, path: string, content: string) =>
    call<{ resolved: boolean }>("git_apply_resolution", { repo_id, path, content }),

  // ---- Experiment tracking ----

  experimentsRunsRecent: (repo_id: string, limit: number = 20) =>
    call<{ runs: ExperimentRun[] }>("experiments_runs_recent", { repo_id, limit }),

  experimentsRunsForCommit: (repo_id: string, sha: string) =>
    call<{ runs: ExperimentRun[] }>("experiments_runs_for_commit", { repo_id, sha }),

  experimentsRunDetail: (repo_id: string, run_id: string) =>
    call<ExperimentRunDetail>("experiments_run_detail", { repo_id, run_id }),

  experimentsConfigure: (repo_id: string, provider_id: string, base_url: string, token: string) =>
    call<{ ok: boolean; message?: string }>("experiments_configure", { repo_id, provider_id, base_url, token }),

  notebookStripOutputs: (repo_id: string, path: string) =>
    call<{ ok: boolean; stripped_cells: number }>("notebook_strip_outputs", { repo_id, path }),

  crabTrackAndAdd: (
    repo_id: string,
    paths: string[],
    opts: { auto_commit?: boolean; commit_message?: string } = {},
  ) =>
    call<TrackAndAddResult>("crab_track_and_add", { repo_id, paths, ...opts }),

  // ---- Structured Crab flows ----

  crabAddStructured: (
    repo_id: string,
    patterns: string[],
    opts: { dry_run?: boolean; job_id?: string } = {},
  ) =>
    call<{ status: number; job_id: string; summary: AddFlowSummary | null }>(
      "crab_add_structured",
      { repo_id, patterns, job_id: crypto.randomUUID(), ...opts },
    ),

  crabPushStructured: (
    repo_id: string,
    opts: { remote?: string; force?: boolean; job_id?: string } = {},
  ) =>
    call<{ status: number; job_id: string; summary: PushFlowSummary | null }>(
      "crab_push_structured",
      { repo_id, job_id: crypto.randomUUID(), ...opts },
    ),

  stagingStats: (repo_id: string) =>
    call<StagingStats>("staging_stats", { repo_id }),

  stagingClean: (repo_id: string, prune_abandoned = false) =>
    call<{ ok: boolean }>("staging_clean", { repo_id, prune_abandoned }),

  crabResetFiles: (repo_id: string, paths: string[]) =>
    call<{ ok: boolean; files_reset: number; output?: string }>("crab_reset_files", { repo_id, paths }),

  crabInit: (repo_id: string, url: string) =>
    call<{ ok: boolean; url?: string; config_path?: string; error?: string }>("crab_init", { repo_id, url }),

  storageConfigGet: (repo_id: string) => call<StorageConfig>("storage_config_get", { repo_id }),
  storageConfigVerify: (repo_id: string, remote?: string) => call<StorageVerifyResult>("storage_config_verify", { repo_id, remote }),

  storageConnections: (repo_id?: string) => call<StorageConnectionStatus>("storage_connections_list", { repo_id }),
  storageConnectionSave: async (connection: StorageConnectionInput) => {
    try { return await call<StorageConnectionInfo>("storage_connections_save", { connection }); }
    finally { window.dispatchEvent(new CustomEvent("crab:storage-connection-changed")); }
  },
  storageConnectionRemove: async (id: string) => {
    try { return await call<{ ok: boolean }>("storage_connections_remove", { id }); }
    finally { window.dispatchEvent(new CustomEvent("crab:storage-connection-changed")); }
  },
  storageConnectionTest: (connection: StorageConnectionInput | string, repo_id?: string) => call<StorageVerifyResult>("storage_connections_test", { ...(typeof connection === "string" ? { id: connection } : { connection }), repo_id }),
  storageConnectionBind: async (repo_id: string, id: string | null) => {
    try { return await call<{ ok: boolean }>("storage_connections_bind", { repo_id, id }); }
    finally { window.dispatchEvent(new CustomEvent("crab:storage-connection-changed", { detail: { repo_id } })); }
  },

  storageSummary: (repo_id: string) => call<StorageSummary>("storage_summary", { repo_id }),

  // ---- Forge / Pull Requests ----

  forgeListPrs: (repo_id: string) =>
    call<{ prs: PullRequestEntry[] }>("forge_list_prs", { repo_id }),

  forgePrDetail: (repo_id: string, pr_number: number) =>
    call<PullRequestDetail>("forge_pr_detail", { repo_id, pr_number }),

  forgePrComment: (repo_id: string, pr_number: number, body: string) =>
    call<{ id: string; created_at: string }>("forge_pr_comment", { repo_id, pr_number, body }),

  forgeSubmitReview: (
    repo_id: string,
    pr_number: number,
    opts: { body: string; event: "comment" | "approve" | "request_changes" },
  ) =>
    call<{ id: string; state: string }>("forge_submit_review", {
      repo_id,
      pr_number,
      ...opts,
    }),

  forgeCreatePr: (
    repo_id: string,
    opts: { title: string; body: string; head: string; base: string; draft: boolean },
  ) =>
    call<{ number: number; url: string }>("forge_create_pr", {
      repo_id,
      ...opts,
    }),

  forgeListChecks: (repo_id: string, sha: string) =>
    call<{ checks: CiCheck[] }>("forge_list_checks", { repo_id, sha }),

  forgeListIssues: (repo_id: string, query?: string) =>
    call<{ issues: ForgeIssue[] }>("forge_list_issues", { repo_id, query }),

  forgeSubmitReviewWithComments: (
    repo_id: string,
    pr_number: number,
    opts: { body: string; event: import("./review/types").Verdict; comments: import("./review/types").ForgeInlineComment[] },
  ) =>
    call<{ review_id: string }>("forge_submit_review_with_comments", {
      repo_id,
      pr_number,
      ...opts,
    }),

  forgePrThreadReply: (repo_id: string, pr_number: number, thread_id: string, body: string) =>
    call<{ id: string; created_at: string }>("forge_pr_thread_reply", {
      repo_id,
      pr_number,
      thread_id,
      body,
    }),

  forgePrThreadResolve: (repo_id: string, pr_number: number, thread_id: string, resolved: boolean) =>
    call<{ resolved: boolean }>("forge_pr_thread_resolve", {
      repo_id,
      pr_number,
      thread_id,
      resolved,
    }),

  forgePrCommentEdit: (repo_id: string, comment_id: string, body: string) =>
    call<{ updated_at: string }>("forge_pr_comment_edit", {
      repo_id,
      comment_id,
      body,
    }),

  forgePrCommentDelete: (repo_id: string, comment_id: string) =>
    call<{ deleted: boolean }>("forge_pr_comment_delete", {
      repo_id,
      comment_id,
    }),

  forgeReRequestReview: (repo_id: string, pr_number: number, reviewer: string) =>
    call<{ requested: boolean }>("forge_re_request_review", {
      repo_id,
      pr_number,
      reviewer,
    }),

  forgeApplySuggestion: (repo_id: string, pr_number: number, comment_id: string) =>
    call<{ sha: string; message: string }>("forge_apply_suggestion", {
      repo_id,
      pr_number,
      comment_id,
    }),

  forgeCurrentUser: (repo_id: string) =>
    call<{ login: string }>("forge_current_user", { repo_id }),

  // Theme management
  saveImportedTheme: (theme: { id: string; name: string; colorScheme: "dark" | "light"; colors: Record<string, string> }) =>
    call<{ saved: string }>("save_imported_theme", { theme }),

  // ---- Workflow Engine ----

  workflowList: (repo_id: string) =>
    call<{ files: string[] }>("workflow_list", { repo_id }),

  workflowGet: (repo_id: string, path: string) =>
    call<import("./lib/workflow-types").WorkflowDefinition>("workflow_get", { repo_id, path }),

  workflowSave: (repo_id: string, workflow: import("./lib/workflow-types").WorkflowDefinition) =>
    call<{ saved: boolean }>("workflow_save", { repo_id, workflow }),

  workflowCreate: (repo_id: string, path: string) =>
    call<{ created: boolean }>("workflow_create", { repo_id, path }),

  workflowDelete: (repo_id: string, path: string) =>
    call<{ deleted: boolean }>("workflow_delete", { repo_id, path }),

  workflowRun: (repo_id: string, path: string, run_id: string, stages?: string[]) =>
    call<{ run_id: string; stage_names: string[] }>("workflow_run", { repo_id, path, run_id, stages }),

  workflowCancel: (repo_id: string, run_id: string) =>
    call<{ cancelled: boolean }>("workflow_cancel", { repo_id, run_id }),

  workflowValidate: (repo_id: string, path: string) =>
    call<{ valid: boolean; errors: string[] }>("workflow_validate", { repo_id, path }),

  // ---- Workflow Builder (SDK-backed) ----

  workflowParse: (repo_id: string, yaml_content: string) =>
    call<import("./lib/workflow-types").DagInfo>("workflow_parse", { repo_id, yaml_content }),

  workflowStatus: (repo_id: string, path: string) =>
    call<import("./lib/workflow-types").WorkflowStatusReport>("workflow_status", { repo_id, path }),

  workflowDag: (repo_id: string, path: string) =>
    call<import("./lib/workflow-types").DagInfo>("workflow_dag", { repo_id, path }),

  workflowResolveTemplate: (repo_id: string, expr: string, path: string) =>
    call<{ resolved: string }>("workflow_resolve_template", { repo_id, expr, path }),

  workflowTemplateCompletions: (repo_id: string, prefix: string, path: string) =>
    call<{ completions: import("./lib/workflow-types").TemplateCompletion[] }>("workflow_template_completions", { repo_id, prefix, path }),

  workflowExpList: (repo_id: string) =>
    call<{ experiments: import("./lib/workflow-types").ExperimentEntry[] }>("workflow_exp_list", { repo_id }),

  workflowExpEnqueue: (repo_id: string, overrides: Record<string, string>[]) =>
    call<{ queued: number; ids: string[] }>("workflow_exp_enqueue", { repo_id, overrides }),

  workflowExpStart: (repo_id: string, jobs: number) =>
    call<{ started: boolean }>("workflow_exp_start", { repo_id, jobs }),

  workflowExpStop: (repo_id: string) =>
    call<{ stopped: boolean }>("workflow_exp_stop", { repo_id }),

  workflowMigratePreview: (repo_id: string, dvc_path?: string) =>
    call<import("./lib/workflow-types").MigratePreview>("workflow_migrate_preview", { repo_id, dvc_path }),

  workflowMigrateApply: (repo_id: string, output_path?: string) =>
    call<{ written: boolean; path: string }>("workflow_migrate_apply", { repo_id, output_path }),
};

/**
 * Read and parse the workspace dataset-rules override file at
 * `.crab/desktop/dataset-rules.json`. Returns `null` when the
 * file doesn't exist or can't be parsed.
 */
export async function readWorkspaceDatasetRules(
  repo_id: string,
): Promise<{ rules: Array<{ kind: string; globs: string[]; minBytes?: number; pointerOnly?: boolean }>; replace?: boolean } | null> {
  try {
    const res = await rpc.readText(repo_id, ".crab/desktop/dataset-rules.json", {
      max_bytes: 256 * 1024,
    });
    if (!res.text) return null;
    const parsed = JSON.parse(res.text);
    if (!parsed || !Array.isArray(parsed.rules)) return null;
    return parsed;
  } catch {
    // File doesn't exist or isn't readable — fall back to defaults.
    return null;
  }
}

// ── CI Check types ───────────────────────────────────────────────────

export type CiCheckStatus = "queued" | "in_progress" | "completed";

export type CiCheckConclusion =
  | "success"
  | "failure"
  | "neutral"
  | "cancelled"
  | "timed_out"
  | "action_required"
  | "skipped"
  | null;

export interface CiCheck {
  name: string;
  status: CiCheckStatus;
  conclusion: CiCheckConclusion;
  url: string | null;
  started_at: string | null;
  completed_at: string | null;
}

// ── Forge / Pull Request types ───────────────────────────────────────

export interface ForgeIssue {
  number: number;
  title: string;
  state: "open" | "closed";
}

export type PullRequestStatus =
  | "draft"
  | "open"
  | "approved"
  | "changes_requested"
  | "merged";

export interface PullRequestEntry {
  number: number;
  title: string;
  author: string;
  status: PullRequestStatus;
  source_branch: string;
  target_branch: string;
  updated_at: string;
  assignees: string[];
  is_mine: boolean;
  is_assigned_to_me: boolean;
}

// ── PR detail types ─────────────────────────────────────────────────

export type PrTimelineEventKind =
  | "comment"
  | "review"
  | "status_check"
  | "commit";

export type ReviewVerdict = "approved" | "changes_requested" | "commented";

export interface PrTimelineEvent {
  id: string;
  kind: PrTimelineEventKind;
  author: string;
  created_at: string;
  body?: string;
  /** Present when kind === "review" */
  verdict?: ReviewVerdict;
  /** Present when kind === "status_check" */
  check_name?: string;
  check_status?: "success" | "failure" | "pending";
  /** Present when kind === "commit" */
  sha?: string;
  subject?: string;
}

export interface PrChangedFile {
  path: string;
  additions: number;
  deletions: number;
  patch?: string;
  status?: string;
}

export interface PullRequestDetail {
  number: number;
  title: string;
  author: string;
  status: PullRequestStatus;
  source_branch: string;
  target_branch: string;
  created_at: string;
  updated_at: string;
  body: string;
  timeline: PrTimelineEvent[];
  changed_files: PrChangedFile[];
  mergeable: boolean | null;
  labels: string[];
  /** Inline comments keyed by file path, then line number. */
  inline_comments?: Record<string, Record<number, PrInlineComment[]>>;
  /** CI checks for the PR head commit. */
  checks?: CiCheck[];
  /** Structured inline threads (richer than flat inline_comments). */
  threads?: import("./review/types").ReviewThread[];
  /** Head SHA of the PR (for file progress tracking). */
  head_sha?: string;
  /** Current user's login (for permission checks). */
  current_user?: string;
  /** Whether the current user is the PR author. */
  is_author?: boolean;
  /** Whether the current user has write access to the repo. */
  has_write_access?: boolean;
}

export interface PrInlineComment {
  id: string;
  author: string;
  body: string;
  created_at: string;
}

// ── Crab structured flow types ────────────────────────────────────

export interface TrackAndAddResult {
  ok: boolean;
  patterns_added: string[];
  files_tracked: Array<{ path: string; status: string }>;
  commit_paths: string[];
  errors: Array<{ path: string; status?: number; stderr?: string }>;
  commit_sha: string | null;
}

export interface AddFlowSummary {
  files_staged: number;
  chunks_staged: number;
  bytes_processed: number;
  duration_ms: number;
}

export interface AddFlowFileProgress {
  path: string;
  size: number;
  chunks: number | null;
  dedup_ratio: number | null;
  bytes_done: number;
  status: "pending" | "chunking" | "done" | "error";
  error?: string;
}

export type AddFlowPhase = "chunking" | "flushing" | "indexing";

export interface AddProgressEvent {
  job_id: string;
  event: string;
  data: Record<string, unknown>;
}

export interface PushFlowSummary {
  refs_pushed: number;
  duration_ms: number;
  bytes_uploaded: number | null;
  xorbs_created: number | null;
}

export interface PushStepInfo {
  index: number;
  name: string;
  status: "pending" | "running" | "done" | "error";
  duration_ms: number | null;
}

export interface PushUploadProgress {
  bytes_uploaded: number;
  bytes_total: number | null;
  xorbs_done: number;
  xorbs_total: number | null;
  throughput_bps: number;
}

export interface PushStepEvent {
  job_id: string;
  event: string;
  data: Record<string, unknown>;
}

export interface PushProgressEvent {
  job_id: string;
  event: string;
  data: Record<string, unknown>;
}

export interface StagingStats {
  /** Number of sealed segments. */
  segments_sealed?: number;
  /** Bytes in the current (unsealed) segment. */
  current_segment_bytes?: number;
  /** Total bytes across all segments. */
  total_staged_bytes?: number;
  /** Bytes occupied by live (referenced) chunks. */
  live_bytes?: number;
  /** Bytes occupied by dead (unreferenced) chunks. */
  dead_bytes?: number;
  /** Ratio of dead bytes to total. */
  dead_ratio?: number;
  /** Total staged chunk count. */
  chunk_count?: number;
  /** Total registered file count. */
  file_count?: number;
  /** Per-file breakdown. */
  files?: StagedFileEntry[];
  [key: string]: unknown;
}

export interface StagedFileEntry {
  /** Hex-encoded Blake3 file hash. */
  file_hash: string;
  /** Original file path relative to repo root, if recorded. */
  file_path?: string | null;
  /** Original file size in bytes. */
  total_bytes: number;
  /** Committed chunks (flushed to segment). */
  committed_chunks: number;
  /** Pending chunks (not yet flushed). */
  pending_chunks: number;
  /** Total chunk count. */
  total_chunks: number;
  /** Number of distinct segments. */
  segments: number;
}

export interface StorageConfig {
  configured: boolean;
  provider: string | null;
  bucket: string | null;
  prefix: string | null;
  region: string | null;
  remote_url?: string | null;
  has_credentials: boolean;
}

export interface StorageVerifyResult {
  ok: boolean;
  latency_ms: number;
  error?: string;
}

// ── Experiment tracking types ───────────────────────────────────────

export type ExperimentRunStatus =
  | "running"
  | "completed"
  | "failed"
  | "killed"
  | "unknown";

export interface ExperimentRun {
  id: string;
  name: string;
  commit_sha: string | null;
  metrics: Record<string, number>;
  status: ExperimentRunStatus;
  start_time: string;
  url: string;
}

export interface ExperimentRunDetail {
  id: string;
  name: string;
  commit_sha: string | null;
  status: ExperimentRunStatus;
  start_time: string;
  end_time: string | null;
  url: string;
  metrics: Record<string, number>;
  params: Record<string, string>;
  tags: Record<string, string>;
}

export interface StorageSummary {
  usage: {
    cache_bytes: number;
    staging_bytes: number;
    git_dir_bytes: number;
    hydrated_bytes: number;
    pointer_bytes: number;
    local_total_bytes: number;
  };
  metadata: {
    protocol: string;
    capsule: {
      generation: number;
      visible_refs: number;
      visible_capsules: number;
      root_digest: string;
      checkpoint_present: boolean;
    };
  };
}

// ── Indexer status ───────────────────────────────────────────────────

export interface IndexerStatus {
  running: boolean;
  db_path: string | null;
  entries_cached: number;
  repos_indexed: number;
}

export const dialogs = {
  pickRepoDirectory: () => window.crab.pickRepoDirectory(),
  pickCloneDestination: () => window.crab.pickCloneDestination(),
  pickJsonFile: (title?: string) => window.crab.pickJsonFile(title),
} as const;

export const mainProcess = {
  updateSettings: (u: Record<string, unknown>) => window.crab.updateMainSettings(u),
  setDevtools: (open: boolean) => window.crab.setDevtools(open),
  openLogsFolder: () => window.crab.openLogsFolder(),
  // File system operations
  fsReveal: (fullPath: string) => window.crab.fsReveal(fullPath),
  fsCreate: ((repoIdOrPath: string, pathOrKind: string, kind?: "file" | "directory") => {
    if (kind !== undefined) {
      // Overload: fsCreate(repoId, fullPath, kind)
      return window.crab.fsCreate(repoIdOrPath, pathOrKind, kind);
    }
    // Legacy: fsCreate(fullPath, kind)
    return window.crab.fsCreate(repoIdOrPath, pathOrKind as "file" | "directory");
  }) as {
    (fullPath: string, kind: "file" | "directory"): Promise<{ path: string; kind: string }>;
    (repoId: string, fullPath: string, kind: "file" | "directory"): Promise<{ path: string; kind: string }>;
  },
  fsRename: ((repoIdOrOld: string, oldOrNew: string, newPath?: string) => {
    if (newPath !== undefined) {
      // Overload: fsRename(repoId, oldPath, newPath)
      return window.crab.fsRename(repoIdOrOld, oldOrNew, newPath);
    }
    // Legacy: fsRename(oldPath, newPath)
    return window.crab.fsRename(repoIdOrOld, oldOrNew);
  }) as {
    (oldPath: string, newPath: string): Promise<{ oldPath: string; newPath: string }>;
    (repoId: string, oldPath: string, newPath: string): Promise<{ oldPath: string; newPath: string }>;
  },
  fsDelete: ((repoIdOrPath: string, pathOrTrash?: string | boolean, useTrash?: boolean) => {
    if (typeof pathOrTrash === "string") {
      // Overload: fsDelete(repoId, fullPath, useTrash?)
      return window.crab.fsDelete(repoIdOrPath, pathOrTrash, useTrash);
    }
    // Legacy: fsDelete(fullPath, useTrash?)
    return window.crab.fsDelete(repoIdOrPath, pathOrTrash);
  }) as {
    (fullPath: string, useTrash?: boolean): Promise<{ deleted: string; trash: boolean }>;
    (repoId: string, fullPath: string, useTrash?: boolean): Promise<{ deleted: string; trash: boolean }>;
  },
  fsOpenExternal: (fullPath: string) => window.crab.fsOpenExternal(fullPath),
  fsWriteFile: (repoId: string, fullPath: string, content: string, nativeSessionId: string | null) => window.crab.fsWriteFile(repoId, fullPath, content, nativeSessionId),
  fsReadFile: (...args: [string] | [string, string, number | undefined, string | null]) => args.length === 1
    ? window.crab.fsReadFile(args[0]) : window.crab.fsReadFile(args[0], args[1], args[2], args[3]),
  fsReadImage: (fullPath: string) =>
    window.crab.fsReadImage(fullPath),
  // File system watcher
  fsWatchStart: (repoPath: string) => window.crab.fsWatchStart(repoPath),
  fsWatchStop: (repoPath: string) => window.crab.fsWatchStop(repoPath),
  // Repo capabilities
  getCapabilities: (repoId: string) => window.crab.getCapabilities(repoId),
  // Dialogs (prompt/confirm replacements for sandboxed renderer)
  dialogPrompt: requestPrompt,
  dialogConfirm: requestConfirmation,
  // PTY daemon diagnostics and upgrade
  daemonDiagnostics: () => window.crab.daemonDiagnostics(),
  daemonRestart: () => window.crab.daemonRestart(),
  daemonPrepareUpgrade: (opts?: { successorPath?: string }) =>
    window.crab.daemonPrepareUpgrade(opts),
};

export function onAgentEvent(listener: (evt: AgentEvent) => void): () => void {
  // Defensive: when the preload script fails to load (wrong path,
  // CSP, etc.), `window.crab` is undefined and the renderer would
  // otherwise blow up at module-load time. Returning a no-op
  // unsubscribe keeps React mounting so the user sees an error
  // state rather than a blank window.
  if (typeof window === "undefined" || !window.crab) {
    console.error(
      "window.crab is not available — preload script did not run. " +
        "Renderer will operate in offline/degraded mode.",
    );
    return () => undefined;
  }
  return window.crab.onEvent(listener);
}
