import type { SymphonyConfig, SymphonyIssue, SymphonyIssueBlockerRef, SymphonyTracker } from "./types";
import { SymphonyTrackerError } from "./tracker-errors";

export type LinearFetch = (
  endpoint: string,
  init: { method: "POST"; headers: Record<string, string>; body: string },
) => Promise<{ ok: boolean; status: number; text(): Promise<string> }>;

interface LinearGraphQLResponse<T> {
  data?: T;
  errors?: Array<{ message?: string }>;
}

interface LinearIssueNode {
  id?: string;
  identifier?: string;
  title?: string;
  description?: string | null;
  priority?: number | null;
  branchName?: string | null;
  url?: string | null;
  createdAt?: string | null;
  updatedAt?: string | null;
  assignee?: { id?: string; name?: string; displayName?: string } | null;
  state?: { name?: string | null } | null;
  labels?: { nodes?: Array<{ name?: string | null }> } | null;
  inverseRelations?: {
    nodes?: Array<{
      type?: string | null;
      issue?: {
        id?: string | null;
        identifier?: string | null;
        state?: { name?: string | null } | null;
      } | null;
    }>;
  } | null;
  relations?: {
    nodes?: Array<{
      type?: string | null;
      relatedIssue?: {
        id?: string | null;
        identifier?: string | null;
        state?: { name?: string | null } | null;
      } | null;
    }>;
  } | null;
}

interface LinearPageInfo {
  hasNextPage?: boolean;
  endCursor?: string | null;
}

interface LinearIssuesPayload {
  issues?: {
    nodes?: LinearIssueNode[];
    pageInfo?: LinearPageInfo | null;
  };
}

const LINEAR_PAGE_SIZE = 50;
const LINEAR_RELATION_PAGE_SIZE = 50;

const ISSUE_FIELDS = `
  id
  identifier
  title
  description
  priority
  branchName
  url
  createdAt
  updatedAt
  assignee { id name displayName }
  state { name }
  labels { nodes { name } }
  inverseRelations(first: $relationFirst) {
    nodes {
      type
      issue {
        id
        identifier
        state { name }
      }
    }
  }
`;

export class LinearSymphonyTracker implements SymphonyTracker {
  constructor(
    private readonly config: SymphonyConfig,
    private readonly fetcher: LinearFetch = defaultFetch,
  ) {}

  async fetchCandidateIssues(): Promise<SymphonyIssue[]> {
    return this.fetchIssuesByStates(this.config.tracker.activeStates);
  }

  async fetchTerminalIssues(): Promise<SymphonyIssue[]> {
    return this.fetchIssuesByStates(this.config.tracker.terminalStates);
  }

  async fetchIssueStatesByIds(issueIds: string[]): Promise<SymphonyIssue[]> {
    if (issueIds.length === 0) return [];
    const query = `
      query SymphonyIssuesByIds($ids: [ID!]!, $first: Int!, $after: String, $relationFirst: Int!) {
        issues(filter: { id: { in: $ids } }, first: $first, after: $after) {
          pageInfo { hasNextPage endCursor }
          nodes {
            ${ISSUE_FIELDS}
          }
        }
      }
    `;
    return this.executePaginatedIssueQuery(query, { ids: issueIds });
  }

  private async fetchIssuesByStates(states: string[]): Promise<SymphonyIssue[]> {
    if (states.length === 0) return [];
    const assigneeFilter = this.config.tracker.assignee
      ? "assignee: { id: { eq: $assignee } }"
      : "";
    const assigneeVariable = this.config.tracker.assignee ? ", $assignee: String" : "";
    const query = `
      query SymphonyIssues($projectSlug: String!, $states: [String!]!, $first: Int!, $after: String, $relationFirst: Int!${assigneeVariable}) {
        issues(
          filter: {
            project: { slugId: { eq: $projectSlug } }
            state: { name: { in: $states } }
            ${assigneeFilter}
          }
          first: $first
          after: $after
          orderBy: updatedAt
        ) {
          pageInfo { hasNextPage endCursor }
          nodes {
            ${ISSUE_FIELDS}
          }
        }
      }
    `;
    const variables: Record<string, unknown> = {
      projectSlug: this.config.tracker.projectSlug,
      states,
      assignee: this.config.tracker.assignee,
    };

    if (!this.config.tracker.assignee) {
      delete variables.assignee;
    }

    return this.executePaginatedIssueQuery(query, variables);
  }

  private async executePaginatedIssueQuery(
    query: string,
    variables: Record<string, unknown>,
  ): Promise<SymphonyIssue[]> {
    const issues: SymphonyIssue[] = [];
    let after: string | null = null;
    do {
      const page = await this.executeIssuePage(query, {
        ...variables,
        first: LINEAR_PAGE_SIZE,
        after,
        relationFirst: LINEAR_RELATION_PAGE_SIZE,
      });
      issues.push(...(page.nodes ?? []).map(normalizeLinearIssue).filter(isCompleteIssue));
      after = page.pageInfo?.hasNextPage ? page.pageInfo.endCursor ?? null : null;
    } while (after);
    return issues;
  }

  private async executeIssuePage(
    query: string,
    variables: Record<string, unknown>,
  ): Promise<NonNullable<LinearIssuesPayload["issues"]>> {
    const apiKey = this.config.tracker.apiKey;
    if (!apiKey) {
      throw new SymphonyTrackerError("not_authenticated", "Linear API key is missing.", { tracker: "linear" });
    }

    let response: Awaited<ReturnType<LinearFetch>>;
    try {
      response = await this.fetcher(this.config.tracker.endpoint ?? "https://api.linear.app/graphql", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: apiKey,
        },
        body: JSON.stringify({ query, variables }),
      });
    } catch (err) {
      throw new SymphonyTrackerError(
        "transport_error",
        `Linear request failed before a response: ${err instanceof Error ? err.message : String(err)}`,
        { tracker: "linear", cause: err },
      );
    }

    const raw = await response.text();
    if (!response.ok) {
      const code = response.status === 401 || response.status === 403
        ? "auth_error"
        : response.status === 429
          ? "rate_limit"
          : "http_error";
      throw new SymphonyTrackerError(
        code,
        `Linear request failed (${response.status}): ${raw.slice(0, 300)}`,
        { tracker: "linear", status: response.status },
      );
    }

    let parsed: LinearGraphQLResponse<LinearIssuesPayload>;
    try {
      parsed = JSON.parse(raw) as LinearGraphQLResponse<LinearIssuesPayload>;
    } catch (err) {
      throw new SymphonyTrackerError("invalid_json", "Linear returned invalid JSON.", {
        tracker: "linear",
        cause: err,
      });
    }

    if (parsed.errors?.length) {
      const message = parsed.errors.map((err) => err.message || "Unknown GraphQL error").join("; ");
      throw new SymphonyTrackerError("graphql_error", `Linear GraphQL error: ${message}`, {
        tracker: "linear",
      });
    }

    if (!parsed.data?.issues || !Array.isArray(parsed.data.issues.nodes)) {
      throw new SymphonyTrackerError("invalid_response", "Linear response missing issues.nodes.", {
        tracker: "linear",
      });
    }

    return parsed.data.issues;
  }
}

export function normalizeLinearIssue(node: LinearIssueNode): SymphonyIssue {
  return {
    id: String(node.id ?? ""),
    identifier: String(node.identifier ?? ""),
    title: String(node.title ?? ""),
    description: node.description ?? null,
    priority: typeof node.priority === "number" && node.priority > 0 ? node.priority : null,
    state: String(node.state?.name ?? ""),
    branchName: node.branchName ?? null,
    url: node.url ?? null,
    labels: (node.labels?.nodes ?? [])
      .map((label) => String(label.name ?? "").trim().toLowerCase())
      .filter(Boolean),
    blockedBy: normalizeBlockers(node),
    assignee: node.assignee?.displayName ?? node.assignee?.name ?? null,
    assigneeId: node.assignee?.id ?? null,
    createdAt: node.createdAt ?? null,
    updatedAt: node.updatedAt ?? null,
  };
}

function normalizeBlockers(node: LinearIssueNode): SymphonyIssueBlockerRef[] {
  const inverseBlockers = (node.inverseRelations?.nodes ?? [])
    .filter((relation) => String(relation.type ?? "").toLowerCase() === "blocks")
    .map((relation) => relation.issue);

  const legacyBlockers = (node.relations?.nodes ?? [])
    .filter((relation) => String(relation.type ?? "").toLowerCase() === "blocked_by")
    .map((relation) => relation.relatedIssue);

  return [...inverseBlockers, ...legacyBlockers].map((issue) => ({
    id: issue?.id ?? null,
    identifier: issue?.identifier ?? null,
    state: issue?.state?.name ?? null,
  }));
}

function isCompleteIssue(issue: SymphonyIssue): boolean {
  return Boolean(issue.id && issue.identifier && issue.title && issue.state);
}

async function defaultFetch(
  endpoint: string,
  init: { method: "POST"; headers: Record<string, string>; body: string },
): Promise<{ ok: boolean; status: number; text(): Promise<string> }> {
  const response = await fetch(endpoint, init);
  return response;
}
