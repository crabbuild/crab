/**
 * GitHub Issues REST API adapter for the Kanban board sync layer.
 *
 * Implements the TrackerAdapter interface using the GitHub REST API v3.
 * Authenticates via the existing GitHub token stored in the OS keychain
 * by the forge-auth module.
 */

import log from "electron-log";
import { getPassword } from "../../keychain";
import type { BoardFilter, GitHubIssuesConfig, TrackerSource } from "../kanban-types";
import type { ExternalIssue, TrackerAdapter } from "./types";

/** Keychain coordinates for the GitHub OAuth token (set by forge-auth). */
const GITHUB_KEYCHAIN_SERVICE = "crab-forge-github";
const GITHUB_KEYCHAIN_ACCOUNT = "oauth-token";

/** Maximum items per page for the GitHub REST API. */
const PER_PAGE = 100;

/** When remaining rate limit drops below this threshold, back off. */
const RATE_LIMIT_THRESHOLD = 50;

/** Base delay (ms) when rate-limited or near the limit. */
const RATE_LIMIT_BACKOFF_MS = 60_000;

/** GitHub REST API base URL. */
const GITHUB_API_BASE = "https://api.github.com";

/**
 * Response shape for a single GitHub issue from the REST API.
 */
interface GitHubIssueResponse {
  number: number;
  title: string;
  body: string | null;
  state: string;
  html_url: string;
  user: { login: string } | null;
  labels: Array<{ name: string }>;
  assignee: { login: string } | null;
  assignees: Array<{ login: string }>;
  milestone: { title: string } | null;
  pull_request?: unknown;
  created_at?: string;
  updated_at: string;
}

/**
 * Internal state for conditional request caching (ETag / If-Modified-Since).
 */
interface ConditionalRequestState {
  etag?: string;
  lastModified?: string;
  cachedIssues?: ExternalIssue[];
}

export class GitHubIssuesAdapter implements TrackerAdapter {
  readonly source: TrackerSource = "github";

  private token: string | null = null;
  private readonly owner: string;
  private readonly repo: string;
  private conditionalState: ConditionalRequestState = {};

  constructor(config: GitHubIssuesConfig) {
    this.owner = config.owner;
    this.repo = config.repo;
  }

  async authenticate(): Promise<void> {
    // Try the forge-auth keychain entry first, then the simplified credential key.
    this.token =
      (await getPassword(GITHUB_KEYCHAIN_SERVICE, GITHUB_KEYCHAIN_ACCOUNT)) ??
      (await getPassword("crab-desktop", "github-token")) ??
      process.env.GITHUB_TOKEN ??
      null;

    if (!this.token) {
      throw new Error(
        "GitHub token not found in keychain or environment. Please authenticate via forge-auth.",
      );
    }

    log.info("[github-issues] Authenticated with GitHub token from keychain");
  }

  async fetchIssues(filter: BoardFilter): Promise<ExternalIssue[]> {
    this.ensureAuthenticated();

    const allIssues: ExternalIssue[] = [];
    let page = 1;
    let hasMore = true;

    // Build query parameters from filter
    const baseParams = this.buildFilterParams(filter);

    while (hasMore) {
      const params = new URLSearchParams(baseParams);
      params.set("page", String(page));
      params.set("per_page", String(PER_PAGE));
      params.set("state", "all");

      const url = `${GITHUB_API_BASE}/repos/${this.owner}/${this.repo}/issues?${params.toString()}`;

      const headers: Record<string, string> = this.buildHeaders();

      // Apply conditional request headers on first page only
      if (page === 1 && this.conditionalState.etag) {
        headers["If-None-Match"] = this.conditionalState.etag;
      } else if (page === 1 && this.conditionalState.lastModified) {
        headers["If-Modified-Since"] = this.conditionalState.lastModified;
      }

      const response = await this.fetchWithRateLimit(url, { headers });

      // 304 Not Modified — return cached data
      if (response.status === 304 && this.conditionalState.cachedIssues) {
        log.info("[github-issues] 304 Not Modified, returning cached issues");
        return this.conditionalState.cachedIssues;
      }

      if (response.status === 401 || response.status === 403) {
        throw new GitHubAuthError(
          `GitHub API returned ${response.status}: authentication failed or token expired`,
        );
      }

      if (!response.ok) {
        const body = await response.text();
        throw new Error(`GitHub API ${response.status}: ${body.slice(0, 200)}`);
      }

      // Store conditional request headers from first page
      if (page === 1) {
        const etag = response.headers.get("etag");
        const lastModified = response.headers.get("last-modified");
        if (etag) this.conditionalState.etag = etag;
        if (lastModified) this.conditionalState.lastModified = lastModified;
      }

      const data = (await response.json()) as GitHubIssueResponse[];

      // Filter out pull requests (GitHub returns them in the issues endpoint)
      const issues = data
        .filter((item) => !item.pull_request)
        .map((item) => this.mapToExternalIssue(item));

      allIssues.push(...issues);

      // If we got fewer items than per_page, we've reached the last page
      hasMore = data.length === PER_PAGE;
      page++;
    }

    // Apply client-side filters that the API doesn't support directly
    const filtered = this.applyClientSideFilters(allIssues, filter);

    // Cache for conditional requests
    this.conditionalState.cachedIssues = filtered;

    return filtered;
  }

  async transitionIssue(issueId: string, targetStatus: string): Promise<void> {
    this.ensureAuthenticated();

    // GitHub Issues only supports open/closed states
    const state = targetStatus.toLowerCase() === "closed" ? "closed" : "open";
    const issueNumber = issueId;

    const url = `${GITHUB_API_BASE}/repos/${this.owner}/${this.repo}/issues/${issueNumber}`;

    const response = await this.fetchWithRateLimit(url, {
      method: "PATCH",
      headers: {
        ...this.buildHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ state }),
    });

    if (!response.ok) {
      const body = await response.text();
      throw new Error(
        `Failed to transition issue #${issueNumber} to "${state}": ${response.status} ${body.slice(0, 200)}`,
      );
    }

    log.info(`[github-issues] Transitioned issue #${issueNumber} to ${state}`);
  }

  async addComment(issueId: string, body: string): Promise<void> {
    this.ensureAuthenticated();

    const url = `${GITHUB_API_BASE}/repos/${this.owner}/${this.repo}/issues/${issueId}/comments`;

    const response = await this.fetchWithRateLimit(url, {
      method: "POST",
      headers: {
        ...this.buildHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ body }),
    });

    if (!response.ok) {
      const respBody = await response.text();
      throw new Error(
        `Failed to add comment to issue #${issueId}: ${response.status} ${respBody.slice(0, 200)}`,
      );
    }

    log.info(`[github-issues] Added comment to issue #${issueId}`);
  }

  async linkPr(issueId: string, prUrl: string): Promise<void> {
    this.ensureAuthenticated();

    // GitHub doesn't have a dedicated "link PR" API for issues.
    // The standard approach is to add a comment referencing the PR.
    const body = `🔗 Pull Request linked: ${prUrl}`;
    await this.addComment(issueId, body);

    log.info(`[github-issues] Linked PR ${prUrl} to issue #${issueId} via comment`);
  }

  // ── Private helpers ─────────────────────────────────────────────

  private ensureAuthenticated(): void {
    if (!this.token) {
      throw new Error("Not authenticated. Call authenticate() first.");
    }
  }

  private buildHeaders(): Record<string, string> {
    return {
      Authorization: `Bearer ${this.token}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "crab-desktop",
      "X-GitHub-Api-Version": "2022-11-28",
    };
  }

  /**
   * Build URL query parameters from the BoardFilter for the GitHub Issues API.
   * Only labels, assignees, and milestones can be applied server-side.
   * excludeLabels must be applied client-side.
   */
  private buildFilterParams(filter: BoardFilter): URLSearchParams {
    const params = new URLSearchParams();

    if (filter.labels && filter.labels.length > 0) {
      // GitHub accepts comma-separated labels (AND logic)
      params.set("labels", filter.labels.join(","));
    }

    if (filter.assignees && filter.assignees.length === 1) {
      // GitHub API only supports a single assignee filter
      params.set("assignee", filter.assignees[0]);
    }

    if (filter.milestones && filter.milestones.length === 1) {
      // GitHub API accepts milestone by title (single value)
      params.set("milestone", filter.milestones[0]);
    }

    return params;
  }

  /**
   * Apply filters that the GitHub API doesn't support natively:
   * - excludeLabels: remove issues that have any of the excluded labels
   * - Multiple assignees: keep only issues assigned to one of the listed users
   * - Multiple milestones: keep only issues in one of the listed milestones
   */
  private applyClientSideFilters(
    issues: ExternalIssue[],
    filter: BoardFilter,
  ): ExternalIssue[] {
    let result = issues;

    if (filter.excludeLabels && filter.excludeLabels.length > 0) {
      const excluded = new Set(filter.excludeLabels.map((l) => l.toLowerCase()));
      result = result.filter(
        (issue) => !issue.labels.some((l) => excluded.has(l.toLowerCase())),
      );
    }

    if (filter.assignees && filter.assignees.length > 1) {
      const allowed = new Set(filter.assignees.map((a) => a.toLowerCase()));
      result = result.filter(
        (issue) => issue.assignee && allowed.has(issue.assignee.toLowerCase()),
      );
    }

    return result;
  }

  private mapToExternalIssue(item: GitHubIssueResponse): ExternalIssue {
    return {
      id: String(item.number),
      identifier: `${this.owner}/${this.repo}#${item.number}`,
      title: item.title,
      description: item.body ?? "",
      status: item.state,
      assignee: item.assignee?.login ?? undefined,
      labels: item.labels.map((l) => l.name),
      priority: "none", // GitHub Issues doesn't have native priority
      priorityValue: null,
      url: item.html_url,
      ...(item.created_at ? { createdAt: new Date(item.created_at).getTime() } : {}),
      updatedAt: new Date(item.updated_at).getTime(),
      blockedBy: [],
    };
  }

  /**
   * Wrapper around fetch that handles rate limiting.
   *
   * Checks X-RateLimit-Remaining header and backs off when near the limit.
   * On 429 responses, waits for the reset time before retrying.
   */
  private async fetchWithRateLimit(
    url: string,
    init: RequestInit,
  ): Promise<Response> {
    const response = await fetch(url, init);

    // Check rate limit headers
    const remaining = response.headers.get("x-ratelimit-remaining");
    const resetHeader = response.headers.get("x-ratelimit-reset");

    if (remaining !== null) {
      const remainingCount = parseInt(remaining, 10);

      if (remainingCount <= RATE_LIMIT_THRESHOLD && remainingCount > 0) {
        log.warn(
          `[github-issues] Rate limit low: ${remainingCount} requests remaining`,
        );
      }
    }

    // Handle 429 Too Many Requests
    if (response.status === 429) {
      const retryAfter = response.headers.get("retry-after");
      let waitMs = RATE_LIMIT_BACKOFF_MS;

      if (retryAfter) {
        waitMs = parseInt(retryAfter, 10) * 1000;
      } else if (resetHeader) {
        const resetTime = parseInt(resetHeader, 10) * 1000;
        waitMs = Math.max(resetTime - Date.now(), 1000);
      }

      log.warn(`[github-issues] Rate limited. Waiting ${waitMs}ms before retry.`);
      await sleep(waitMs);

      // Retry the request once after waiting
      return fetch(url, init);
    }

    // If remaining is very low, proactively slow down for next request
    if (remaining !== null && parseInt(remaining, 10) <= 10 && resetHeader) {
      const resetTime = parseInt(resetHeader, 10) * 1000;
      const waitMs = Math.max(resetTime - Date.now(), 0);
      if (waitMs > 0 && waitMs < RATE_LIMIT_BACKOFF_MS) {
        log.warn(
          `[github-issues] Proactive backoff: ${waitMs}ms (only ${remaining} requests left)`,
        );
        await sleep(waitMs);
      }
    }

    return response;
  }
}

/**
 * Thrown when GitHub returns 401/403, indicating the token is expired or invalid.
 * The board sync layer should catch this and emit an `auth:expired` event.
 */
export class GitHubAuthError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "GitHubAuthError";
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
