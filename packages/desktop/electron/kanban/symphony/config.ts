import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import type { BoardConfig } from "../kanban-types";
import type {
  SymphonyConfig,
  SymphonyConfigError,
  SymphonyTrackerConfig,
  SymphonyTrackerKind,
  SymphonyWorkspaceConfig,
  WorkflowDefinition,
} from "./types";
import { loadWorkflow, SymphonyWorkflowError } from "./workflow-loader";

export interface SymphonyConfigLoadResult {
  config: SymphonyConfig | null;
  error: SymphonyConfigError | null;
  reloaded: boolean;
}

const DEFAULT_TERMINAL_STATES = ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"];
const DEFAULT_ACTIVE_STATES: Record<SymphonyTrackerKind, string[]> = {
  linear: ["Todo", "In Progress"],
  jira: ["To Do", "Todo", "In Progress"],
  github: ["open"],
  "github-projects": ["Todo", "To Do", "In Progress"],
};
const DEFAULT_TRACKER_TERMINAL_STATES: Record<SymphonyTrackerKind, string[]> = {
  linear: DEFAULT_TERMINAL_STATES,
  jira: DEFAULT_TERMINAL_STATES,
  github: ["closed"],
  "github-projects": DEFAULT_TERMINAL_STATES,
};

export class SymphonyConfigStore {
  private lastGood: SymphonyConfig | null = null;
  private lastError: SymphonyConfigError | null = null;
  private lastWorkflowPath: string | null = null;
  private lastMtimeMs: number | null = null;

  async loadInitial(board: BoardConfig): Promise<SymphonyConfigLoadResult> {
    return this.reload(board, { force: true });
  }

  async reloadIfChanged(board: BoardConfig): Promise<SymphonyConfigLoadResult> {
    return this.reload(board, { force: false });
  }

  get current(): SymphonyConfig | null {
    return this.lastGood;
  }

  get error(): SymphonyConfigError | null {
    return this.lastError;
  }

  private async reload(
    board: BoardConfig,
    options: { force: boolean },
  ): Promise<SymphonyConfigLoadResult> {
    const workflowPath = resolveWorkflowPath(board);
    if (!workflowPath) {
      const error = {
        code: "missing_repo_path",
        message: "Board repo path is required for Symphony mode.",
      };
      this.lastError = error;
      return { config: this.lastGood, error, reloaded: false };
    }

    let mtimeMs: number | null = null;
    try {
      mtimeMs = (await fs.stat(workflowPath)).mtimeMs;
    } catch {
      // Let loadWorkflow produce the canonical missing workflow error.
    }

    if (
      !options.force &&
      this.lastGood &&
      this.lastWorkflowPath === workflowPath &&
      this.lastMtimeMs === mtimeMs
    ) {
      return { config: this.lastGood, error: this.lastError, reloaded: false };
    }

    try {
      const workflow = await loadWorkflow(workflowPath);
      const config = parseSymphonyConfig(workflow, board);
      this.lastGood = config;
      this.lastError = null;
      this.lastWorkflowPath = workflowPath;
      this.lastMtimeMs = workflow.mtimeMs;
      return { config, error: null, reloaded: true };
    } catch (err) {
      const error = configErrorFromUnknown(err);
      this.lastError = error;
      return { config: this.lastGood, error, reloaded: false };
    }
  }
}

export function resolveWorkflowPath(board: BoardConfig): string | null {
  if (!board.repoPath) return null;
  const repoPath = path.resolve(expandHome(board.repoPath));
  const configuredPath = board.symphony?.workflowPath;
  const candidate = configuredPath
    ? path.resolve(repoPath, expandHome(configuredPath))
    : path.join(repoPath, "WORKFLOW.md");

  const relative = path.relative(repoPath, candidate);
  if (relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new SymphonyWorkflowError(
      "workflow_path_outside_repo",
      `Symphony workflow path must stay inside the board repo: ${candidate}`,
    );
  }

  return candidate;
}

export function parseSymphonyConfig(workflow: WorkflowDefinition, board?: BoardConfig): SymphonyConfig {
  const root = normalizeObject(workflow.config);
  const tracker = normalizeObject(root.tracker);
  const polling = normalizeObject(root.polling);
  const workspace = normalizeObject(root.workspace);
  const hooks = normalizeObject(root.hooks);
  const agent = normalizeObject(root.agent);
  const codex = normalizeObject(root.codex);

  const kind = resolveTrackerKind(tracker, board);
  if (!kind) {
    throw configError(
      "missing_tracker_kind",
      "Tracker kind missing in WORKFLOW.md and no supported board tracker is configured.",
    );
  }
  const trackerConfig = parseTrackerConfig(kind, tracker, board);

  const workspaceConfig: SymphonyWorkspaceConfig = {
    root: resolvePathValue(
      asString(workspace.root),
      path.join(os.tmpdir(), "symphony_workspaces"),
      workflow.workflowDir,
    ),
  };

  return {
    workflow,
    tracker: trackerConfig,
    polling: {
      intervalMs: positiveInt(polling.interval_ms, 30_000, "polling.interval_ms"),
    },
    workspace: workspaceConfig,
    hooks: {
      afterCreate: asString(hooks.after_create) || null,
      beforeRun: asString(hooks.before_run) || null,
      afterRun: asString(hooks.after_run) || null,
      beforeRemove: asString(hooks.before_remove) || null,
      timeoutMs: positiveInt(hooks.timeout_ms, 60_000, "hooks.timeout_ms"),
    },
    agent: {
      maxConcurrentAgents: positiveInt(agent.max_concurrent_agents, 10, "agent.max_concurrent_agents"),
      maxTurns: positiveInt(agent.max_turns, 20, "agent.max_turns"),
      maxRetryBackoffMs: positiveInt(agent.max_retry_backoff_ms, 300_000, "agent.max_retry_backoff_ms"),
      maxConcurrentAgentsByState: normalizeStateLimits(agent.max_concurrent_agents_by_state),
    },
    codex: {
      command: asString(codex.command) || "codex app-server",
      approvalPolicy: codex.approval_policy,
      threadSandbox: asString(codex.thread_sandbox) || "workspace-write",
      turnSandboxPolicy: normalizeOptionalObject(codex.turn_sandbox_policy),
      turnTimeoutMs: positiveInt(codex.turn_timeout_ms, 3_600_000, "codex.turn_timeout_ms"),
      readTimeoutMs: positiveInt(codex.read_timeout_ms, 5_000, "codex.read_timeout_ms"),
      stallTimeoutMs: nonNegativeInt(codex.stall_timeout_ms, 300_000, "codex.stall_timeout_ms"),
    },
  };
}

export function validateDispatchConfig(config: SymphonyConfig): SymphonyConfigError | null {
  if (config.tracker.kind === "linear") {
    if (!config.tracker.apiKey) {
      return { code: "missing_linear_api_token", message: "Linear API token missing in WORKFLOW.md or LINEAR_API_KEY." };
    }
    if (!config.tracker.projectSlug) {
      return { code: "missing_linear_project_slug", message: "Linear project slug missing in WORKFLOW.md or board config." };
    }
  } else if (config.tracker.kind === "jira") {
    if (!config.tracker.instanceUrl) {
      return { code: "missing_jira_instance_url", message: "Jira instance URL missing in WORKFLOW.md or board config." };
    }
    if (!config.tracker.projectKey) {
      return { code: "missing_jira_project_key", message: "Jira project key missing in WORKFLOW.md or board config." };
    }
    if (!config.tracker.authType) {
      return { code: "missing_jira_auth_type", message: "Jira auth type missing in WORKFLOW.md or board config." };
    }
    if (config.tracker.authType === "oauth" && !config.tracker.credentialRef) {
      return { code: "missing_jira_oauth_credential", message: "Jira OAuth credential reference missing in WORKFLOW.md or board config." };
    }
  } else if (config.tracker.kind === "github") {
    if (!config.tracker.owner || !config.tracker.repo) {
      return { code: "missing_github_repo", message: "GitHub owner/repo missing in WORKFLOW.md or board config." };
    }
  } else if (config.tracker.kind === "github-projects") {
    if (!config.tracker.owner) {
      return { code: "missing_github_project_owner", message: "GitHub Projects owner missing in WORKFLOW.md or board config." };
    }
    if (!config.tracker.projectNumber) {
      return { code: "missing_github_project_number", message: "GitHub Projects project number missing in WORKFLOW.md or board config." };
    }
  }
  if (!config.codex.command.trim()) {
    return { code: "missing_codex_command", message: "Codex command missing in WORKFLOW.md." };
  }
  return null;
}

function resolveTrackerKind(
  tracker: Record<string, unknown>,
  board?: BoardConfig,
): SymphonyTrackerKind | null {
  const workflowKind = asString(tracker.kind);
  const kind = workflowKind || (isSupportedTrackerKind(board?.trackerType) ? board!.trackerType : null);
  if (!kind) return null;
  if (!isSupportedTrackerKind(kind)) {
    throw configError("unsupported_tracker_kind", `Unsupported tracker kind: ${kind}`);
  }
  return kind;
}

function parseTrackerConfig(
  kind: SymphonyTrackerKind,
  tracker: Record<string, unknown>,
  board?: BoardConfig,
): SymphonyTrackerConfig {
  const activeStates = asStringArray(tracker.active_states, DEFAULT_ACTIVE_STATES[kind]);
  const terminalStates = asStringArray(tracker.terminal_states, DEFAULT_TRACKER_TERMINAL_STATES[kind]);

  if (kind === "linear") {
    const boardConfig = board?.trackerType === "linear" ? board.trackerConfig : {};
    return {
      kind,
      endpoint: asString(tracker.endpoint) || "https://api.linear.app/graphql",
      apiKey: resolveSecret(asString(tracker.api_key), process.env.LINEAR_API_KEY),
      projectSlug: asString(tracker.project_slug) || asOptionalString(boardConfig, "projectSlug"),
      assignee: resolveSecret(asString(tracker.assignee), process.env.LINEAR_ASSIGNEE),
      activeStates,
      terminalStates,
    };
  }

  if (kind === "jira") {
    const boardConfig = board?.trackerType === "jira" ? board.trackerConfig : {};
    return {
      kind,
      instanceUrl: coalesceStrings(asString(tracker.instance_url), asString(tracker.instanceUrl), asOptionalString(boardConfig, "instanceUrl")),
      projectKey: coalesceStrings(asString(tracker.project_key), asString(tracker.projectKey), asOptionalString(boardConfig, "projectKey")),
      boardId: optionalPositiveInt(coalesceUnknown(tracker.board_id, tracker.boardId, asOptionalUnknown(boardConfig, "boardId")), null, "tracker.board_id"),
      sprintId: optionalPositiveInt(coalesceUnknown(tracker.sprint_id, tracker.sprintId, asOptionalUnknown(boardConfig, "sprintId")), null, "tracker.sprint_id"),
      authType: asAuthType(coalesceUnknown(tracker.auth_type, tracker.authType, asOptionalUnknown(boardConfig, "authType"))),
      credentialRef: coalesceStrings(asString(tracker.credential_ref), asString(tracker.credentialRef), asOptionalString(boardConfig, "credentialRef")),
      activeStates,
      terminalStates,
    };
  }

  if (kind === "github") {
    const boardConfig = board?.trackerType === "github" ? board.trackerConfig : {};
    return {
      kind,
      owner: coalesceStrings(asString(tracker.owner), asOptionalString(boardConfig, "owner")),
      repo: coalesceStrings(asString(tracker.repo), asOptionalString(boardConfig, "repo")),
      activeStates,
      terminalStates,
    };
  }

  const boardConfig = board?.trackerType === "github-projects" ? board.trackerConfig : {};
  return {
    kind,
    owner: coalesceStrings(asString(tracker.owner), asOptionalString(boardConfig, "owner")),
    projectNumber: optionalPositiveInt(
      coalesceUnknown(tracker.project_number, tracker.projectNumber, asOptionalUnknown(boardConfig, "projectNumber")),
      null,
      "tracker.project_number",
    ),
    statusFieldName: coalesceStrings(
      asString(tracker.status_field_name),
      asString(tracker.statusFieldName),
      asOptionalString(boardConfig, "statusFieldName"),
      "Status",
    ),
    activeStates,
    terminalStates,
  };
}

function isSupportedTrackerKind(value: unknown): value is SymphonyTrackerKind {
  return value === "linear" || value === "jira" || value === "github" || value === "github-projects";
}

function coalesceUnknown(...values: unknown[]): unknown {
  return values.find((value) => value !== undefined && value !== null && value !== "");
}

function coalesceStrings(...values: Array<string | null | undefined>): string | null {
  for (const value of values) {
    if (typeof value !== "string") continue;
    const trimmed = value.trim();
    if (trimmed) return trimmed;
  }
  return null;
}

function asOptionalString(value: unknown, key: string): string | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  return coalesceStrings((value as Record<string, unknown>)[key] as string | null | undefined);
}

function asOptionalUnknown(value: unknown, key: string): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  return (value as Record<string, unknown>)[key];
}

function asAuthType(value: unknown): "oauth" | "pat" | null {
  if (value == null || value === "") return null;
  const raw = String(value).trim().toLowerCase();
  if (raw === "oauth" || raw === "pat") return raw;
  throw configError("invalid_workflow_config", "tracker.auth_type must be either oauth or pat.");
}

function optionalPositiveInt(value: unknown, fallback: number | null, field: string): number | null {
  if (value == null || value === "") return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw configError("invalid_workflow_config", `${field} must be a positive integer.`);
  }
  return parsed;
}

function normalizeObject(value: unknown): Record<string, unknown> {
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    return value as Record<string, unknown>;
  }
  return {};
}

function normalizeOptionalObject(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asStringArray(value: unknown, fallback: string[]): string[] {
  if (!Array.isArray(value)) return [...fallback];
  const strings = value.map((item) => String(item).trim()).filter(Boolean);
  return strings.length > 0 ? strings : [...fallback];
}

function positiveInt(value: unknown, fallback: number, field: string): number {
  if (value == null || value === "") return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw configError("invalid_workflow_config", `${field} must be a positive integer.`);
  }
  return parsed;
}

function nonNegativeInt(value: unknown, fallback: number, field: string): number {
  if (value == null || value === "") return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw configError("invalid_workflow_config", `${field} must be a non-negative integer.`);
  }
  return parsed;
}

function normalizeStateLimits(value: unknown): Record<string, number> {
  const obj = normalizeObject(value);
  const result: Record<string, number> = {};
  for (const [key, raw] of Object.entries(obj)) {
    const limit = Number(raw);
    const normalizedState = key.trim().toLowerCase();
    if (!normalizedState || !Number.isInteger(limit) || limit <= 0) continue;
    result[normalizedState] = limit;
  }
  return result;
}

function resolveSecret(value: string | null, fallback: string | undefined): string | null {
  if (!value) return fallback && fallback !== "" ? fallback : null;
  const envName = envReferenceName(value);
  if (!envName) return value;
  const resolved = process.env[envName];
  return resolved && resolved !== "" ? resolved : null;
}

function resolvePathValue(value: string | null, fallback: string, workflowDir: string): string {
  const raw = value && value.trim() ? value.trim() : fallback;
  const envName = envReferenceName(raw);
  const envResolved = envName ? process.env[envName] : raw;
  const selected = envResolved && envResolved !== "" ? envResolved : fallback;
  const homeExpanded = expandHome(selected);
  const absolute = path.isAbsolute(homeExpanded)
    ? homeExpanded
    : path.resolve(workflowDir, homeExpanded);
  return path.normalize(absolute);
}

function envReferenceName(value: string): string | null {
  if (!value.startsWith("$")) return null;
  const name = value.slice(1);
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : null;
}

function expandHome(value: string): string {
  if (value === "~") return os.homedir();
  if (value.startsWith("~/")) return path.join(os.homedir(), value.slice(2));
  return value;
}

function configError(code: string, message: string): SymphonyWorkflowError {
  return new SymphonyWorkflowError(code, message);
}

function configErrorFromUnknown(err: unknown): SymphonyConfigError {
  if (err instanceof SymphonyWorkflowError) {
    return {
      code: err.code,
      message: err.message,
    };
  }
  return {
    code: "invalid_workflow_config",
    message: err instanceof Error ? err.message : String(err),
  };
}
