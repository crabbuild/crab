/**
 * Workflow types for the visual DAG editor.
 *
 * These mirror the Rust `crab/src/workflow/` structures closely
 * enough that round-tripping through the agent's JSON-RPC layer is
 * lossless. The visual editor operates on these types; serialization
 * to/from `crab.yaml` happens server-side in the agent.
 */

// ── Stage definition ────────────────────────────────────────────────

export type CmdKind = "shell" | "argv" | "shell_list";

export interface StageCmd {
  kind: CmdKind;
  /** Shell text, or a JSON string array for argv / shell_list. */
  value: string;
}

export type DepKind = "path" | "crab" | "git" | "url" | "oci" | "stage_out";

export interface StageDep {
  kind: DepKind;
  /** For path deps: the file path. For stage_out: "stage:out_path". */
  value: string;
  /** Extra metadata for structured deps. */
  meta?: Record<string, string>;
}

export type OutKind = "file" | "directory";

export interface StageOut {
  path: string;
  kind: OutKind;
  cache?: boolean;
  persist?: boolean;
  max_bytes?: number;
}

export interface StageResources {
  cpu?: number;
  gpu?: number;
  memory?: string;
}

export interface RetryPolicy {
  max_attempts?: number;
  initial_backoff?: string;
  max_backoff?: string;
  backoff_multiplier?: number;
  on_exit_codes?: number[];
  on_signals?: number[];
  on_timeout?: boolean;
}

export type EnvSpec = "inherit" | "empty" | { allowlist: string[] };

export interface WorkflowStage {
  name: string;
  cmd: StageCmd;
  deps: StageDep[];
  outs: StageOut[];
  params: string[];
  env: EnvSpec;
  retry?: RetryPolicy;
  timeout?: string;
  nondeterministic?: boolean;
  hermetic?: boolean;
  side_effects?: boolean;
  resources?: StageResources;
  frozen?: boolean;
  wdir?: string;
  condition?: ConditionSpec;
  desc?: string;
  meta?: Record<string, unknown>;
}

// ── Workflow definition ─────────────────────────────────────────────

export interface WorkflowDefaults {
  env?: EnvSpec;
  retry?: RetryPolicy;
}

export interface WorkflowDefinition {
  /** File path relative to repo root (e.g. "crab.yaml"). */
  path: string;
  params: string[];
  metrics: string[];
  plots: string[];
  defaults: WorkflowDefaults;
  stages: WorkflowStage[];
}

// ── Execution state ─────────────────────────────────────────────────

export type StageRunStatus =
  | "pending"
  | "queued"
  | "running"
  | "cached"
  | "done"
  | "failed"
  | "skipped";

export interface StageRunState {
  name: string;
  status: StageRunStatus;
  duration_ms?: number;
  error?: string;
  cached?: boolean;
  /** Output hash for cache key display. */
  output_hash?: string;
}

export interface WorkflowRunState {
  run_id: string;
  status: "idle" | "running" | "done" | "failed" | "cancelled";
  stages: StageRunState[];
  started_at?: number;
  finished_at?: number;
  error?: string;
}

// ── Visual layout (persisted per-workflow) ───────────────────────────

export interface NodePosition {
  x: number;
  y: number;
}

export interface WorkflowLayout {
  /** Stage name → position. Missing entries get auto-laid-out. */
  positions: Record<string, NodePosition>;
  /** Visual-only edges (source→target) not backed by workflow deps.
   *  Persisted so they survive editor reopens. */
  visualEdges?: Array<{ source: string; target: string }>;
}

// ── Enrichment types (templating, foreach, matrix, conditions) ───────

export interface TemplateVar {
  key: string;
  value: string;
  source: "inline" | "file";
  file_path?: string;
}

export interface ForeachConfig {
  /** List items, dict items, or complex items. */
  items: string[] | Record<string, unknown> | unknown[];
  /** Raw YAML expression (may contain ${...} references). */
  raw_expr?: string;
}

export interface MatrixConfig {
  /** Variable name → list of values. */
  variables: Record<string, string[]>;
}

export interface ConditionSpec {
  kind: "env" | "file_exists" | "expr";
  value: string;
}

/** Extended stage with all enrichment fields. */
export interface WorkflowStageEnriched extends WorkflowStage {
  foreach?: ForeachConfig;
  matrix?: MatrixConfig;
  /** If expanded from a foreach/matrix template, the source name. */
  expanded_from?: string;
  /** The expansion key (e.g., "raw_a", "resnet-imagenet"). */
  expansion_key?: string;
}

// ── Workflow status types ────────────────────────────────────────────

export type StageStatusKind =
  | "up_to_date"
  | "outdated"
  | "not_run"
  | "frozen"
  | "conditional_skip";

export interface ParamChange {
  key: string;
  old_value: string;
  new_value: string;
}

export interface StageStatusEntry {
  name: string;
  status: StageStatusKind;
  changed_deps: string[];
  changed_params: ParamChange[];
  last_run?: number;
  last_duration_ms?: number;
}

export interface WorkflowStatusReport {
  stages: StageStatusEntry[];
  overall: "all_up_to_date" | "has_outdated" | "has_not_run";
}

// ── DAG introspection types ─────────────────────────────────────────

export interface DagNode {
  name: string;
  expanded_from?: string;
  frozen: boolean;
  has_condition: boolean;
  wdir?: string;
  desc?: string;
}

export interface DagEdge {
  from: string;
  to: string;
  kind: "stage_output" | "implicit_file";
}

export interface DagInfo {
  nodes: DagNode[];
  edges: DagEdge[];
  topological_order: string[];
  parallelism_groups: string[][];
  critical_path: string[];
}

// ── Experiment types ────────────────────────────────────────────────

export type ExpStatusKind = "pending" | "running" | "done" | "failed";

export interface ExperimentEntry {
  id: string;
  status: ExpStatusKind;
  param_overrides: Record<string, string>;
  queued_at: string;
  base_commit: string;
}

// ── Template autocomplete types ─────────────────────────────────────

export type CompletionSource = "vars" | "params" | "env";

export interface TemplateCompletion {
  key: string;
  value: string;
  source: CompletionSource;
}

// ── Migration types ─────────────────────────────────────────────────

export interface MigratePreview {
  crab_yaml: string;
  warnings: string[];
  stage_count: number;
}
