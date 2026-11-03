/**
 * GitHub Projects v2 GraphQL adapter for the Kanban board sync layer.
 *
 * Implements the TrackerAdapter interface using the GitHub GraphQL API to
 * read and write GitHub Projects v2 board state (columns, cards, field values).
 * Authenticates via the existing GitHub token stored in the OS keychain
 * by the forge-auth module.
 */

import { EventEmitter } from "events";
import log from "electron-log";
import { getPassword } from "../../keychain";
import type { BoardFilter, GitHubProjectsConfig, TrackerSource } from "../kanban-types";
import type { ExternalIssue, TrackerAdapter } from "./types";

/** Keychain coordinates for the GitHub OAuth token (set by forge-auth). */
const GITHUB_KEYCHAIN_SERVICE = "crab-forge-github";
const GITHUB_KEYCHAIN_ACCOUNT = "oauth-token";

/** GitHub GraphQL API endpoint. */
const GRAPHQL_ENDPOINT = "https://api.github.com/graphql";

/** Items per page for cursor-based pagination. */
const PAGE_SIZE = 100;

/** Maximum total items to fetch (5 pages × 100). */
const MAX_ITEMS = 500;

/** Per-request timeout for mutations (ms). */
const MUTATION_TIMEOUT_MS = 15_000;

/** Exponential backoff base delay (ms). */
const BACKOFF_BASE_MS = 1_000;

/** Maximum retry attempts for rate-limit errors. */
const MAX_RETRY_ATTEMPTS = 5;

/** Maximum total backoff time (ms). */
const MAX_TOTAL_BACKOFF_MS = 60_000;

// ── GraphQL Queries ──────────────────────────────────────────────

const FETCH_PROJECT_ITEMS_QUERY = `
query($owner: String!, $projectNumber: Int!, $cursor: String) {
  user(login: $owner) {
    projectV2(number: $projectNumber) {
      id
      items(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          content {
            ... on Issue {
              number
              title
              body
              state
              url
              assignees(first: 5) {
                nodes { login }
              }
              labels(first: 10) {
                nodes { name }
              }
              createdAt
              updatedAt
            }
            ... on PullRequest {
              number
              title
              body
              state
              url
              assignees(first: 5) {
                nodes { login }
              }
              labels(first: 10) {
                nodes { name }
              }
              createdAt
              updatedAt
            }
          }
          fieldValues(first: 20) {
            nodes {
              ... on ProjectV2ItemFieldSingleSelectValue {
                name
                field { ... on ProjectV2SingleSelectField { name } }
              }
            }
          }
        }
      }
    }
  }
}
`;

const FETCH_PROJECT_ITEMS_ORG_QUERY = `
query($owner: String!, $projectNumber: Int!, $cursor: String) {
  organization(login: $owner) {
    projectV2(number: $projectNumber) {
      id
      items(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          content {
            ... on Issue {
              number
              title
              body
              state
              url
              assignees(first: 5) {
                nodes { login }
              }
              labels(first: 10) {
                nodes { name }
              }
              createdAt
              updatedAt
            }
            ... on PullRequest {
              number
              title
              body
              state
              url
              assignees(first: 5) {
                nodes { login }
              }
              labels(first: 10) {
                nodes { name }
              }
              createdAt
              updatedAt
            }
          }
          fieldValues(first: 20) {
            nodes {
              ... on ProjectV2ItemFieldSingleSelectValue {
                name
                field { ... on ProjectV2SingleSelectField { name } }
              }
            }
          }
        }
      }
    }
  }
}
`;

const FETCH_PROJECT_FIELDS_QUERY = `
query($owner: String!, $projectNumber: Int!) {
  user(login: $owner) {
    projectV2(number: $projectNumber) {
      id
      fields(first: 50) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options {
              id
              name
            }
          }
        }
      }
    }
  }
}
`;

const FETCH_PROJECT_FIELDS_ORG_QUERY = `
query($owner: String!, $projectNumber: Int!) {
  organization(login: $owner) {
    projectV2(number: $projectNumber) {
      id
      fields(first: 50) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options {
              id
              name
            }
          }
        }
      }
    }
  }
}
`;

const UPDATE_ITEM_STATUS_MUTATION = `
mutation($projectId: ID!, $itemId: ID!, $fieldId: ID!, $optionId: String!) {
  updateProjectV2ItemFieldValue(
    input: {
      projectId: $projectId
      itemId: $itemId
      fieldId: $fieldId
      value: { singleSelectOptionId: $optionId }
    }
  ) {
    projectV2Item {
      id
    }
  }
}
`;

// ── Types ────────────────────────────────────────────────────────

export interface ProjectFieldOption {
  id: string;
  name: string;
}

export interface ProjectField {
  id: string;
  name: string;
  options: ProjectFieldOption[];
}

interface GraphQLResponse<T = unknown> {
  data?: T;
  errors?: Array<{ message: string; type?: string }>;
}

interface ProjectItemNode {
  id: string;
  content: {
    number?: number;
    title?: string;
    body?: string | null;
    state?: string;
    url?: string;
    assignees?: { nodes: Array<{ login: string }> };
    labels?: { nodes: Array<{ name: string }> };
    createdAt?: string;
    updatedAt?: string;
  } | null;
  fieldValues: {
    nodes: Array<{
      name?: string;
      field?: { name?: string };
    }>;
  };
}

interface ProjectItemsPage {
  pageInfo: { hasNextPage: boolean; endCursor: string | null };
  nodes: ProjectItemNode[];
}

// ── Client Implementation ────────────────────────────────────────

export class GitHubProjectsClient extends EventEmitter implements TrackerAdapter {
  readonly source: TrackerSource = "github-projects";

  private token: string | null = null;
  private projectId: string | null = null;
  private statusFieldId: string | null = null;
  private readonly owner: string;
  private readonly projectNumber: number;
  private readonly statusFieldName: string;
  private isOrg: boolean | null = null;

  constructor(config: GitHubProjectsConfig) {
    super();
    this.owner = config.owner;
    this.projectNumber = config.projectNumber;
    this.statusFieldName = config.statusFieldName || "Status";
  }

  async authenticate(): Promise<void> {
    this.token =
      (await getPassword(GITHUB_KEYCHAIN_SERVICE, GITHUB_KEYCHAIN_ACCOUNT)) ??
      (await getPassword("crab-desktop", "github-token")) ??
      process.env.GITHUB_TOKEN ??
      null;

    if (!this.token) {
      this.emit("auth:expired", { trackerType: "github-projects" });
      throw new GitHubProjectsAuthError(
        "GitHub token not found in keychain or environment. Please authenticate via forge-auth.",
      );
    }

    log.info("[github-projects] Authenticated with GitHub token from keychain");
  }

  async fetchIssues(filter: BoardFilter): Promise<ExternalIssue[]> {
    this.ensureAuthenticated();

    const allItems: ProjectItemNode[] = [];
    let cursor: string | null = null;
    let pageCount = 0;
    const maxPages = Math.ceil(MAX_ITEMS / PAGE_SIZE);

    while (pageCount < maxPages) {
      const variables: Record<string, unknown> = {
        owner: this.owner,
        projectNumber: this.projectNumber,
        cursor,
      };

      const page = await this.fetchProjectItemsPage(variables);
      if (!page) break;

      allItems.push(...page.nodes);
      pageCount++;

      if (!page.pageInfo.hasNextPage || !page.pageInfo.endCursor) break;
      cursor = page.pageInfo.endCursor;
    }

    log.info(
      `[github-projects] Fetched ${allItems.length} project items across ${pageCount} page(s)`,
    );

    const issues = allItems
      .filter((item) => item.content !== null && item.content.number !== undefined)
      .map((item) => this.mapToExternalIssue(item));

    return this.applyFilters(issues, filter);
  }

  async getProjectFields(): Promise<ProjectField[]> {
    this.ensureAuthenticated();

    const variables = {
      owner: this.owner,
      projectNumber: this.projectNumber,
    };

    // Try user query first, then org
    const ownerType = await this.resolveOwnerType();
    const query = ownerType === "org"
      ? FETCH_PROJECT_FIELDS_ORG_QUERY
      : FETCH_PROJECT_FIELDS_QUERY;

    const response = await this.graphqlRequest<{
      user?: { projectV2: { id: string; fields: { nodes: ProjectField[] } } };
      organization?: { projectV2: { id: string; fields: { nodes: ProjectField[] } } };
    }>(query, variables);

    const project = response.user?.projectV2 ?? response.organization?.projectV2;
    if (!project) {
      throw new Error(
        `Project #${this.projectNumber} not found for owner "${this.owner}"`,
      );
    }

    // Cache the project ID for mutations
    this.projectId = project.id;

    // Filter to only single-select fields (those with options)
    const fields = project.fields.nodes.filter(
      (f) => f.id && f.name && f.options,
    );

    // Cache the status field ID
    const statusField = fields.find(
      (f) => f.name.toLowerCase() === this.statusFieldName.toLowerCase(),
    );
    if (statusField) {
      this.statusFieldId = statusField.id;
    }

    return fields;
  }

  async updateItemStatus(itemId: string, statusOptionId: string): Promise<void> {
    this.ensureAuthenticated();

    // Ensure we have the project ID and status field ID
    if (!this.projectId || !this.statusFieldId) {
      await this.getProjectFields();
    }

    if (!this.projectId) {
      throw new Error("Could not resolve project ID. Call getProjectFields() first.");
    }
    if (!this.statusFieldId) {
      throw new Error(
        `Status field "${this.statusFieldName}" not found in project fields.`,
      );
    }

    const variables = {
      projectId: this.projectId,
      itemId,
      fieldId: this.statusFieldId,
      optionId: statusOptionId,
    };

    await this.graphqlRequest(
      UPDATE_ITEM_STATUS_MUTATION,
      variables,
      MUTATION_TIMEOUT_MS,
    );

    log.info(
      `[github-projects] Updated item ${itemId} status to option ${statusOptionId}`,
    );
  }

  /**
   * Transition an issue by mapping the target status name to a field option ID
   * and calling updateItemStatus.
   */
  async transitionIssue(issueId: string, targetStatus: string): Promise<void> {
    const fields = await this.getProjectFields();
    const statusField = fields.find(
      (f) => f.name.toLowerCase() === this.statusFieldName.toLowerCase(),
    );

    if (!statusField) {
      throw new Error(`Status field "${this.statusFieldName}" not found in project.`);
    }

    const option = statusField.options.find(
      (o) => o.name.toLowerCase() === targetStatus.toLowerCase(),
    );

    if (!option) {
      throw new Error(
        `Status option "${targetStatus}" not found. Available: ${statusField.options.map((o) => o.name).join(", ")}`,
      );
    }

    await this.updateItemStatus(issueId, option.id);
  }

  async addComment(issueId: string, _body: string): Promise<void> {
    // GitHub Projects v2 items don't have a direct comment API via GraphQL.
    // Comments are on the underlying issue/PR. This would require the REST API.
    log.warn(
      `[github-projects] addComment not directly supported for project items. Issue: ${issueId}`,
    );
  }

  async linkPr(issueId: string, _prUrl: string): Promise<void> {
    // Similar to addComment — linking is done at the issue level, not project item level.
    log.warn(
      `[github-projects] linkPr not directly supported for project items. Issue: ${issueId}`,
    );
  }

  // ── Private helpers ─────────────────────────────────────────────

  private ensureAuthenticated(): void {
    if (!this.token) {
      this.emit("auth:expired", { trackerType: "github-projects" });
      throw new GitHubProjectsAuthError("Not authenticated. Call authenticate() first.");
    }
  }

  /**
   * Resolve whether the owner is a user or organization.
   * Caches the result after the first successful resolution.
   */
  private async resolveOwnerType(): Promise<"user" | "org"> {
    if (this.isOrg !== null) {
      return this.isOrg ? "org" : "user";
    }

    // Try user first — if it fails with a null result, try org
    try {
      const response = await this.graphqlRequest<{
        user: { projectV2: unknown } | null;
      }>(
        `query($owner: String!, $projectNumber: Int!) {
          user(login: $owner) { projectV2(number: $projectNumber) { id } }
        }`,
        { owner: this.owner, projectNumber: this.projectNumber },
      );

      if (response.user?.projectV2) {
        this.isOrg = false;
        return "user";
      }
    } catch {
      // Fall through to try org
    }

    this.isOrg = true;
    return "org";
  }

  /**
   * Fetch a single page of project items, trying user then org queries.
   */
  private async fetchProjectItemsPage(
    variables: Record<string, unknown>,
  ): Promise<ProjectItemsPage | null> {
    const ownerType = await this.resolveOwnerType();
    const query = ownerType === "org"
      ? FETCH_PROJECT_ITEMS_ORG_QUERY
      : FETCH_PROJECT_ITEMS_QUERY;

    const response = await this.graphqlRequest<{
      user?: { projectV2: { id: string; items: ProjectItemsPage } };
      organization?: { projectV2: { id: string; items: ProjectItemsPage } };
    }>(query, variables);

    const project = response.user?.projectV2 ?? response.organization?.projectV2;
    if (!project) {
      throw new Error(
        `Project #${this.projectNumber} not found for owner "${this.owner}"`,
      );
    }

    // Cache project ID
    this.projectId = project.id;

    return project.items;
  }

  /**
   * Execute a GraphQL request with exponential backoff on rate-limit errors.
   */
  private async graphqlRequest<T>(
    query: string,
    variables: Record<string, unknown>,
    timeoutMs?: number,
  ): Promise<T> {
    let attempt = 0;
    let totalBackoffMs = 0;

    while (true) {
      try {
        const result = await this.executeGraphQL<T>(query, variables, timeoutMs);
        return result;
      } catch (error) {
        if (error instanceof GitHubProjectsAuthError) {
          throw error;
        }

        if (error instanceof GitHubRateLimitError) {
          attempt++;
          if (attempt >= MAX_RETRY_ATTEMPTS || totalBackoffMs >= MAX_TOTAL_BACKOFF_MS) {
            this.emit("sync:error", {
              operation: "graphql_request",
              category: "rate_limit_exhausted",
              message: `Rate limit retry exhausted after ${attempt} attempts`,
            });
            throw error;
          }

          const backoffMs = this.calculateBackoff(attempt);
          if (totalBackoffMs + backoffMs > MAX_TOTAL_BACKOFF_MS) {
            this.emit("sync:error", {
              operation: "graphql_request",
              category: "rate_limit_exhausted",
              message: `Rate limit total backoff would exceed ${MAX_TOTAL_BACKOFF_MS}ms`,
            });
            throw error;
          }

          log.warn(
            `[github-projects] Rate limited, retrying in ${backoffMs}ms (attempt ${attempt}/${MAX_RETRY_ATTEMPTS})`,
          );
          await sleep(backoffMs);
          totalBackoffMs += backoffMs;
          continue;
        }

        // Non-rate-limit errors: emit sync:error and rethrow
        const errorMessage = error instanceof Error ? error.message : String(error);
        this.emit("sync:error", {
          operation: "graphql_request",
          category: this.categorizeError(error),
          message: errorMessage,
        });
        throw error;
      }
    }
  }

  /**
   * Execute a single GraphQL request against the GitHub API.
   */
  private async executeGraphQL<T>(
    query: string,
    variables: Record<string, unknown>,
    timeoutMs?: number,
  ): Promise<T> {
    const controller = timeoutMs ? new AbortController() : undefined;
    const timeout = timeoutMs && controller
      ? setTimeout(() => controller.abort(), timeoutMs)
      : undefined;

    try {
      const response = await fetch(GRAPHQL_ENDPOINT, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.token}`,
          "Content-Type": "application/json",
          "User-Agent": "crab-desktop",
        },
        body: JSON.stringify({ query, variables }),
        signal: controller?.signal,
      });

      if (response.status === 401) {
        this.emit("auth:expired", { trackerType: "github-projects" });
        throw new GitHubProjectsAuthError(
          "GitHub GraphQL API returned 401: token expired or invalid",
        );
      }

      if (response.status === 403) {
        const body = await response.text();
        // Check if it's a rate limit (secondary rate limit returns 403)
        if (body.includes("rate limit") || body.includes("abuse")) {
          throw new GitHubRateLimitError(
            `GitHub secondary rate limit hit: ${body.slice(0, 200)}`,
          );
        }
        this.emit("auth:expired", { trackerType: "github-projects" });
        throw new GitHubProjectsAuthError(
          `GitHub GraphQL API returned 403: ${body.slice(0, 200)}`,
        );
      }

      if (!response.ok) {
        const body = await response.text();
        throw new Error(
          `GitHub GraphQL API ${response.status}: ${body.slice(0, 200)}`,
        );
      }

      const json = (await response.json()) as GraphQLResponse<T>;

      // Check for GraphQL-level errors
      if (json.errors && json.errors.length > 0) {
        const rateLimitError = json.errors.find(
          (e) => e.type === "RATE_LIMITED" || e.message.toLowerCase().includes("rate limit"),
        );
        if (rateLimitError) {
          throw new GitHubRateLimitError(rateLimitError.message);
        }

        throw new Error(
          `GraphQL errors: ${json.errors.map((e) => e.message).join("; ")}`,
        );
      }

      if (!json.data) {
        throw new Error("GraphQL response missing data field");
      }

      return json.data;
    } catch (error) {
      if (error instanceof GitHubProjectsAuthError || error instanceof GitHubRateLimitError) {
        throw error;
      }

      // Handle abort (timeout)
      if (error instanceof Error && error.name === "AbortError") {
        throw new Error(
          `GitHub GraphQL request timed out after ${timeoutMs}ms`,
        );
      }

      // Handle network errors
      if (error instanceof TypeError && error.message.includes("fetch")) {
        throw new Error(`Network error communicating with GitHub: ${error.message}`);
      }

      throw error;
    } finally {
      if (timeout) clearTimeout(timeout);
    }
  }

  /**
   * Calculate exponential backoff with jitter.
   * Base: 1s, doubling per attempt, with random jitter ±25%.
   */
  private calculateBackoff(attempt: number): number {
    const baseDelay = BACKOFF_BASE_MS * Math.pow(2, attempt - 1);
    const jitter = baseDelay * 0.25 * (Math.random() * 2 - 1); // ±25%
    return Math.min(Math.round(baseDelay + jitter), MAX_TOTAL_BACKOFF_MS);
  }

  /**
   * Categorize an error for the sync:error event.
   */
  private categorizeError(error: unknown): string {
    if (error instanceof Error) {
      if (error.message.includes("timed out") || error.name === "AbortError") {
        return "network_timeout";
      }
      if (error.message.includes("Network error") || error.message.includes("fetch")) {
        return "network_error";
      }
      if (error.message.includes("GraphQL errors")) {
        return "invalid_response";
      }
    }
    return "server_error";
  }

  /**
   * Map a project item node to the normalized ExternalIssue format.
   */
  private mapToExternalIssue(item: ProjectItemNode): ExternalIssue {
    const content = item.content!;
    const statusValue = this.extractStatusField(item);

    return {
      id: item.id,
      identifier: `${this.owner}/project-${this.projectNumber}#${content.number ?? item.id}`,
      title: content.title ?? "",
      description: content.body ?? "",
      status: statusValue ?? "unknown",
      assignee: content.assignees?.nodes[0]?.login ?? undefined,
      labels: content.labels?.nodes.map((l) => l.name) ?? [],
      priority: "none",
      priorityValue: null,
      url: content.url ?? "",
      ...(content.createdAt ? { createdAt: new Date(content.createdAt).getTime() } : {}),
      updatedAt: content.updatedAt ? new Date(content.updatedAt).getTime() : Date.now(),
      blockedBy: [],
    };
  }

  /**
   * Extract the status field value from a project item's field values.
   */
  private extractStatusField(item: ProjectItemNode): string | null {
    for (const fieldValue of item.fieldValues.nodes) {
      if (
        fieldValue.field?.name?.toLowerCase() === this.statusFieldName.toLowerCase() &&
        fieldValue.name
      ) {
        return fieldValue.name;
      }
    }
    return null;
  }

  /**
   * Apply board filter criteria to fetched issues.
   */
  private applyFilters(issues: ExternalIssue[], filter: BoardFilter): ExternalIssue[] {
    let result = issues;

    if (filter.labels && filter.labels.length > 0) {
      const required = new Set(filter.labels.map((l) => l.toLowerCase()));
      result = result.filter((issue) =>
        issue.labels.some((l) => required.has(l.toLowerCase())),
      );
    }

    if (filter.assignees && filter.assignees.length > 0) {
      const allowed = new Set(filter.assignees.map((a) => a.toLowerCase()));
      result = result.filter(
        (issue) => issue.assignee && allowed.has(issue.assignee.toLowerCase()),
      );
    }

    if (filter.excludeLabels && filter.excludeLabels.length > 0) {
      const excluded = new Set(filter.excludeLabels.map((l) => l.toLowerCase()));
      result = result.filter(
        (issue) => !issue.labels.some((l) => excluded.has(l.toLowerCase())),
      );
    }

    return result;
  }
}

/**
 * Thrown when GitHub returns 401/403, indicating the token is expired or invalid.
 */
export class GitHubProjectsAuthError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "GitHubProjectsAuthError";
  }
}

/**
 * Thrown when GitHub returns a rate-limit error (429, secondary 403, or GraphQL RATE_LIMITED).
 */
export class GitHubRateLimitError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "GitHubRateLimitError";
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
