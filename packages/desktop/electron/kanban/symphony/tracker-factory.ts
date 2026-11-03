import type {
  BoardConfig,
  GitHubIssuesConfig,
  GitHubProjectsConfig,
  JiraTrackerConfig,
} from "../kanban-types";
import { GitHubIssuesAdapter } from "../tracker-adapters/github-issues";
import { GitHubProjectsClient } from "../tracker-adapters/github-projects";
import { JiraClient } from "../tracker-adapters/jira-client";
import type { ExternalIssue, TrackerAdapter } from "../tracker-adapters/types";
import { LinearSymphonyTracker } from "./linear-adapter";
import { SymphonyTrackerError } from "./tracker-errors";
import type { SymphonyConfig, SymphonyIssue, SymphonyTracker, SymphonyTrackerKind } from "./types";
import { normalizeIssueState } from "./types";

export function createSymphonyTracker(config: SymphonyConfig, board: BoardConfig): SymphonyTracker {
  switch (config.tracker.kind) {
    case "linear":
      return new LinearSymphonyTracker(config);
    case "jira":
      return new ExternalSymphonyTracker(
        config,
        board,
        new JiraClient(jiraConfigFromSymphony(config)),
      );
    case "github":
      return new ExternalSymphonyTracker(
        config,
        board,
        new GitHubIssuesAdapter(githubIssuesConfigFromSymphony(config)),
      );
    case "github-projects":
      return new ExternalSymphonyTracker(
        config,
        board,
        new GitHubProjectsClient(githubProjectsConfigFromSymphony(config)),
      );
    default:
      throw new SymphonyTrackerError("unsupported_tracker", `Unsupported Symphony tracker kind: ${config.tracker.kind}`, {
        tracker: config.tracker.kind,
      });
  }
}

export class ExternalSymphonyTracker implements SymphonyTracker {
  private authenticated = false;

  constructor(
    private readonly config: SymphonyConfig,
    private readonly board: BoardConfig,
    private readonly adapter: TrackerAdapter,
  ) {}

  async fetchCandidateIssues(): Promise<SymphonyIssue[]> {
    if (this.config.tracker.activeStates.length === 0) return [];
    const issues = await this.fetchNormalizedIssues();
    return filterIssuesByStates(issues, this.config.tracker.activeStates);
  }

  async fetchTerminalIssues(): Promise<SymphonyIssue[]> {
    if (this.config.tracker.terminalStates.length === 0) return [];
    const issues = await this.fetchNormalizedIssues();
    return filterIssuesByStates(issues, this.config.tracker.terminalStates);
  }

  async fetchIssueStatesByIds(issueIds: string[]): Promise<SymphonyIssue[]> {
    if (issueIds.length === 0) return [];
    const wanted = new Set(issueIds);
    const issues = await this.fetchNormalizedIssues();
    return issues.filter((issue) => wanted.has(issue.id) || wanted.has(issue.identifier));
  }

  private async fetchNormalizedIssues(): Promise<SymphonyIssue[]> {
    try {
      if (!this.authenticated) {
        await this.adapter.authenticate();
        this.authenticated = true;
      }
      const issues = await this.adapter.fetchIssues(this.board.filterCriteria ?? {});
      return issues.map((issue) => normalizeExternalIssue(issue, this.config.tracker.kind, this.config));
    } catch (err) {
      throw mapExternalTrackerError(err, this.config.tracker.kind);
    }
  }
}

export function normalizeExternalIssue(
  issue: ExternalIssue,
  kind: SymphonyTrackerKind,
  config: SymphonyConfig,
): SymphonyIssue {
  const identifier = normalizeExternalIdentifier(issue, kind, config);
  const id = kind === "github" ? identifier : String(issue.id ?? "");

  return {
    id,
    identifier,
    title: String(issue.title ?? ""),
    description: issue.description ? String(issue.description) : null,
    priority: normalizeExternalPriority(issue, kind),
    state: String(issue.status ?? "").trim(),
    branchName: null,
    url: issue.url ? String(issue.url) : null,
    labels: (issue.labels ?? []).map((label) => String(label).trim().toLowerCase()).filter(Boolean),
    blockedBy: (issue.blockedBy ?? []).map((blocker) => ({
      id: blocker.id ?? null,
      identifier: blocker.identifier ?? null,
      state: blocker.state ?? null,
    })),
    assignee: issue.assignee ?? null,
    assigneeId: null,
    createdAt: msToIso(issue.createdAt),
    updatedAt: msToIso(issue.updatedAt),
  };
}

function normalizeExternalIdentifier(
  issue: ExternalIssue,
  kind: SymphonyTrackerKind,
  config: SymphonyConfig,
): string {
  if (issue.identifier?.trim()) return issue.identifier.trim();
  if (kind === "github") {
    const owner = config.tracker.owner ?? "unknown";
    const repo = config.tracker.repo ?? "unknown";
    return `${owner}/${repo}#${issue.id}`;
  }
  if (kind === "github-projects") {
    const owner = config.tracker.owner ?? "unknown";
    const projectNumber = config.tracker.projectNumber ?? "unknown";
    return `${owner}/project-${projectNumber}#${issue.id}`;
  }
  return String(issue.id ?? "");
}

function normalizeExternalPriority(issue: ExternalIssue, kind: SymphonyTrackerKind): number | null {
  if (typeof issue.priorityValue === "number") return issue.priorityValue > 0 ? issue.priorityValue : null;
  if (kind !== "jira") return null;
  const priority = String(issue.priority ?? "").trim().toLowerCase();
  if (!priority || priority === "none") return null;
  if (priority === "highest" || priority === "blocker" || priority === "critical") return 1;
  if (priority === "high" || priority === "major") return 2;
  if (priority === "medium" || priority === "normal") return 3;
  if (priority === "low" || priority === "minor" || priority === "lowest" || priority === "trivial") return 4;
  return null;
}

function filterIssuesByStates(issues: SymphonyIssue[], states: string[]): SymphonyIssue[] {
  const allowed = new Set(states.map(normalizeIssueState));
  return issues.filter((issue) => allowed.has(normalizeIssueState(issue.state)));
}

function msToIso(value: number | undefined): string | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  const parsed = new Date(value);
  return Number.isFinite(parsed.getTime()) ? parsed.toISOString() : null;
}

function mapExternalTrackerError(err: unknown, tracker: SymphonyTrackerKind): SymphonyTrackerError {
  if (err instanceof SymphonyTrackerError) return err;

  const message = err instanceof Error ? err.message : String(err);
  const name = err instanceof Error ? err.name : "";
  const status = statusFromMessage(message);

  if (/auth/i.test(name) || /authentication|token|expired|unauthorized/i.test(message)) {
    return new SymphonyTrackerError("auth_error", message, { tracker, status, cause: err });
  }
  if (/rate/i.test(name) || /rate limit|too many requests/i.test(message) || status === 429) {
    return new SymphonyTrackerError("rate_limit", message, { tracker, status, cause: err });
  }
  if (/not authenticated/i.test(message)) {
    return new SymphonyTrackerError("not_authenticated", message, { tracker, status, cause: err });
  }
  if (/graphql/i.test(message)) {
    return new SymphonyTrackerError("graphql_error", message, { tracker, status, cause: err });
  }
  if (status) {
    return new SymphonyTrackerError("http_error", message, { tracker, status, cause: err });
  }
  if (/invalid|missing data|missing .*field/i.test(message)) {
    return new SymphonyTrackerError("invalid_response", message, { tracker, cause: err });
  }
  return new SymphonyTrackerError("transport_error", message, { tracker, cause: err });
}

function statusFromMessage(message: string): number | undefined {
  const match = message.match(/\b([1-5][0-9]{2})\b/);
  return match ? Number(match[1]) : undefined;
}

function jiraConfigFromSymphony(config: SymphonyConfig): JiraTrackerConfig {
  return {
    instanceUrl: config.tracker.instanceUrl ?? "",
    projectKey: config.tracker.projectKey ?? "",
    boardId: config.tracker.boardId ?? undefined,
    sprintId: config.tracker.sprintId ?? undefined,
    authType: config.tracker.authType ?? "pat",
    credentialRef: config.tracker.credentialRef ?? "",
  };
}

function githubIssuesConfigFromSymphony(config: SymphonyConfig): GitHubIssuesConfig {
  return {
    owner: config.tracker.owner ?? "",
    repo: config.tracker.repo ?? "",
  };
}

function githubProjectsConfigFromSymphony(config: SymphonyConfig): GitHubProjectsConfig {
  return {
    owner: config.tracker.owner ?? "",
    projectNumber: config.tracker.projectNumber ?? 0,
    statusFieldName: config.tracker.statusFieldName ?? "Status",
  };
}
