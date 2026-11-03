/**
 * Jira REST API v3 adapter for the Kanban board sync layer.
 *
 * Implements the TrackerAdapter interface for both Jira Cloud (OAuth 2.0 3LO)
 * and Jira Server/Data Center (Personal Access Token) authentication.
 *
 * Features:
 * - OAuth 2.0 3LO flow for Cloud instances (browser-based auth, token refresh)
 * - PAT authentication for Server/DC instances
 * - JQL-based issue fetching with 100-item pagination
 * - Workflow transitions with available-transition lookup
 * - Atlassian Document Format (ADF) comments
 * - Remote link API for PR linking
 * - Rate limiting with Retry-After header parsing (max 5 retries)
 * - Auth expiry detection (401/403 → auth:expired event)
 */

import { EventEmitter } from "events";
import * as http from "http";
import log from "electron-log";
import { shell } from "electron";
import { getPassword, setPassword } from "../../keychain";
import type { BoardFilter, JiraTrackerConfig, TrackerSource } from "../kanban-types";
import type { ExternalIssue, TrackerAdapter } from "./types";

// ── Constants ────────────────────────────────────────────────────

const OAUTH_KEYCHAIN_SERVICE = "crab-jira-oauth";
const OAUTH_KEYCHAIN_ACCOUNT = "tokens";
const PAT_KEYCHAIN_SERVICE = "crab-jira-pat";
const PAT_KEYCHAIN_ACCOUNT = "token";

const MAX_RESULTS = 100;
const MAX_RATE_LIMIT_RETRIES = 5;

/** Atlassian OAuth 2.0 endpoints for Cloud. */
const ATLASSIAN_AUTH_URL = "https://auth.atlassian.com/authorize";
const ATLASSIAN_TOKEN_URL = "https://auth.atlassian.com/oauth/token";

/** Default OAuth scopes for Jira Cloud. */
const OAUTH_SCOPES = [
  "read:jira-work",
  "write:jira-work",
  "read:jira-user",
  "offline_access",
];

/** Default local port for OAuth callback. */
const DEFAULT_CALLBACK_PORT = 9877;

// ── Types ────────────────────────────────────────────────────────

interface OAuthTokens {
  accessToken: string;
  refreshToken: string;
  expiresAt: number;
  cloudId?: string;
}

interface JiraTransition {
  id: string;
  name: string;
  to: { id: string; name: string };
}

interface JiraIssueResponse {
  key: string;
  fields: {
    summary: string;
    description: unknown;
    status: { name: string };
    assignee: { displayName: string; accountId: string } | null;
    labels: string[];
    priority: { name: string } | null;
    created?: string;
    updated: string;
    issuelinks?: JiraIssueLink[];
  };
  self: string;
}

interface JiraIssueLinkIssue {
  key?: string;
  fields?: {
    status?: { name?: string } | null;
  };
}

interface JiraIssueLink {
  type?: {
    name?: string;
    inward?: string;
    outward?: string;
  };
  inwardIssue?: JiraIssueLinkIssue;
  outwardIssue?: JiraIssueLinkIssue;
}

interface JiraSearchResponse {
  startAt: number;
  maxResults: number;
  total: number;
  issues: JiraIssueResponse[];
}

interface JiraTransitionsResponse {
  transitions: JiraTransition[];
}

// ── JiraClient ───────────────────────────────────────────────────

export class JiraClient extends EventEmitter implements TrackerAdapter {
  readonly source: TrackerSource = "jira";

  private readonly config: JiraTrackerConfig;
  private accessToken: string | null = null;
  private oauthTokens: OAuthTokens | null = null;
  private callbackServer: http.Server | null = null;

  constructor(config: JiraTrackerConfig) {
    super();
    this.config = config;
  }

  // ── Authentication ───────────────────────────────────────────

  async authenticate(): Promise<void> {
    if (this.config.authType === "pat") {
      await this.authenticateWithPAT();
    } else {
      await this.authenticateWithOAuth();
    }
  }

  private async authenticateWithPAT(): Promise<void> {
    const token = await getPassword(PAT_KEYCHAIN_SERVICE, PAT_KEYCHAIN_ACCOUNT);

    if (!token) {
      throw new JiraAuthError(
        "Jira PAT not found in keychain. Please configure a Personal Access Token in settings.",
      );
    }

    this.accessToken = token;
    log.info("[jira-client] Authenticated with PAT from keychain");
  }

  private async authenticateWithOAuth(): Promise<void> {
    // Try to load existing tokens from keychain
    const stored = await getPassword(OAUTH_KEYCHAIN_SERVICE, OAUTH_KEYCHAIN_ACCOUNT);

    if (stored) {
      let tokens: OAuthTokens;
      try {
        tokens = JSON.parse(stored) as OAuthTokens;
      } catch {
        log.warn("[jira-client] Failed to parse stored OAuth tokens, starting fresh flow");
        await this.startOAuthFlow();
        return;
      }

      this.oauthTokens = tokens;

      // Check if token is expired or about to expire (within 5 minutes)
      if (this.oauthTokens.expiresAt - Date.now() < 300_000) {
        await this.refreshOAuthToken();
      }

      this.accessToken = this.oauthTokens.accessToken;
      log.info("[jira-client] Authenticated with stored OAuth tokens");
      return;
    }

    // No valid tokens — start the full OAuth flow
    await this.startOAuthFlow();
  }

  /**
   * Start the OAuth 2.0 3LO flow:
   * 1. Open browser to Atlassian authorization URL
   * 2. Receive callback on localhost
   * 3. Exchange code for tokens
   * 4. Store tokens in keychain
   */
  private async startOAuthFlow(): Promise<void> {
    const clientId = this.config.credentialRef;
    const port = DEFAULT_CALLBACK_PORT;
    const redirectUri = `http://localhost:${port}/callback`;

    const authCode = await this.openBrowserAndWaitForCallback(clientId, redirectUri, port);
    await this.exchangeCodeForTokens(authCode, clientId, redirectUri);
  }

  private openBrowserAndWaitForCallback(
    clientId: string,
    redirectUri: string,
    port: number,
  ): Promise<string> {
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.closeCallbackServer();
        reject(new JiraAuthError("OAuth callback timed out after 5 minutes"));
      }, 300_000);

      this.callbackServer = http.createServer((req, res) => {
        const url = new URL(req.url ?? "/", `http://localhost:${port}`);

        if (url.pathname === "/callback") {
          const code = url.searchParams.get("code");
          const error = url.searchParams.get("error");

          if (error) {
            res.writeHead(400, { "Content-Type": "text/html" });
            res.end("<html><body><h1>Authentication Failed</h1><p>You can close this window.</p></body></html>");
            clearTimeout(timeout);
            this.closeCallbackServer();
            reject(new JiraAuthError(`OAuth error: ${error}`));
            return;
          }

          if (code) {
            res.writeHead(200, { "Content-Type": "text/html" });
            res.end("<html><body><h1>Authentication Successful</h1><p>You can close this window.</p></body></html>");
            clearTimeout(timeout);
            this.closeCallbackServer();
            resolve(code);
            return;
          }

          res.writeHead(400, { "Content-Type": "text/html" });
          res.end("<html><body><h1>Missing authorization code</h1></body></html>");
        } else {
          res.writeHead(404);
          res.end();
        }
      });

      this.callbackServer.listen(port, () => {
        const params = new URLSearchParams({
          audience: "api.atlassian.com",
          client_id: clientId,
          scope: OAUTH_SCOPES.join(" "),
          redirect_uri: redirectUri,
          response_type: "code",
          prompt: "consent",
        });

        const authUrl = `${ATLASSIAN_AUTH_URL}?${params.toString()}`;
        shell.openExternal(authUrl);
        log.info("[jira-client] Opened browser for OAuth authorization");
      });

      this.callbackServer.on("error", (err) => {
        clearTimeout(timeout);
        reject(new JiraAuthError(`Failed to start OAuth callback server: ${err.message}`));
      });
    });
  }

  private async exchangeCodeForTokens(
    code: string,
    clientId: string,
    redirectUri: string,
  ): Promise<void> {
    const response = await fetch(ATLASSIAN_TOKEN_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        grant_type: "authorization_code",
        client_id: clientId,
        code,
        redirect_uri: redirectUri,
      }),
    });

    if (!response.ok) {
      const body = await response.text();
      throw new JiraAuthError(`Token exchange failed: ${response.status} ${body.slice(0, 200)}`);
    }

    const data = (await response.json()) as {
      access_token: string;
      refresh_token: string;
      expires_in: number;
    };

    // Fetch the cloud ID for the instance
    const cloudId = await this.fetchCloudId(data.access_token);

    this.oauthTokens = {
      accessToken: data.access_token,
      refreshToken: data.refresh_token,
      expiresAt: Date.now() + data.expires_in * 1000,
      cloudId,
    };

    this.accessToken = data.access_token;

    // Store tokens in keychain
    await setPassword(
      OAUTH_KEYCHAIN_SERVICE,
      OAUTH_KEYCHAIN_ACCOUNT,
      JSON.stringify(this.oauthTokens),
    );

    log.info("[jira-client] OAuth tokens stored in keychain");
  }

  async refreshToken(): Promise<void> {
    await this.refreshOAuthToken();
  }

  private async refreshOAuthToken(): Promise<void> {
    if (!this.oauthTokens?.refreshToken) {
      throw new JiraAuthError("No refresh token available. Please re-authenticate.");
    }

    const clientId = this.config.credentialRef;

    const response = await fetch(ATLASSIAN_TOKEN_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        grant_type: "refresh_token",
        client_id: clientId,
        refresh_token: this.oauthTokens.refreshToken,
      }),
    });

    if (!response.ok) {
      const body = await response.text();
      log.error(`[jira-client] Token refresh failed: ${response.status}`);
      this.emit("auth:expired");
      throw new JiraAuthError(`Token refresh failed: ${response.status} ${body.slice(0, 200)}`);
    }

    const data = (await response.json()) as {
      access_token: string;
      refresh_token: string;
      expires_in: number;
    };

    this.oauthTokens = {
      ...this.oauthTokens,
      accessToken: data.access_token,
      refreshToken: data.refresh_token,
      expiresAt: Date.now() + data.expires_in * 1000,
    };

    this.accessToken = data.access_token;

    await setPassword(
      OAUTH_KEYCHAIN_SERVICE,
      OAUTH_KEYCHAIN_ACCOUNT,
      JSON.stringify(this.oauthTokens),
    );

    log.info("[jira-client] OAuth token refreshed successfully");
  }

  private async fetchCloudId(accessToken: string): Promise<string> {
    const response = await fetch("https://api.atlassian.com/oauth/token/accessible-resources", {
      headers: { Authorization: `Bearer ${accessToken}`, Accept: "application/json" },
    });

    if (!response.ok) {
      throw new JiraAuthError("Failed to fetch accessible resources for cloud ID");
    }

    const resources = (await response.json()) as Array<{ id: string; url: string }>;
    const instanceUrl = this.config.instanceUrl.replace(/\/$/, "");

    const match = resources.find((r) => r.url.replace(/\/$/, "") === instanceUrl);
    if (!match) {
      // Use the first available resource if no exact match
      if (resources.length > 0) {
        return resources[0].id;
      }
      throw new JiraAuthError("No accessible Jira Cloud resources found");
    }

    return match.id;
  }

  private closeCallbackServer(): void {
    if (this.callbackServer) {
      this.callbackServer.close();
      this.callbackServer = null;
    }
  }

  // ── API Methods ──────────────────────────────────────────────

  async fetchIssues(filter: BoardFilter): Promise<ExternalIssue[]> {
    this.ensureAuthenticated();

    const allIssues: ExternalIssue[] = [];
    let startAt = 0;
    let hasMore = true;

    const jql = this.buildJQL(filter);

    while (hasMore) {
      const params = new URLSearchParams({
        jql,
        startAt: String(startAt),
        maxResults: String(MAX_RESULTS),
        fields: "summary,description,status,assignee,labels,priority,created,updated,issuelinks",
      });

      const url = `${this.getBaseUrl()}/rest/api/3/search?${params.toString()}`;
      const response = await this.fetchWithRetry(url, { headers: this.buildHeaders() });

      if (!response.ok) {
        await this.handleErrorResponse(response);
        return allIssues;
      }

      const data = (await response.json()) as JiraSearchResponse;

      const issues = data.issues.map((issue) => this.mapToExternalIssue(issue));
      allIssues.push(...issues);

      startAt += data.issues.length;
      hasMore = startAt < data.total && data.issues.length === MAX_RESULTS;
    }

    return allIssues;
  }

  async transitionIssue(issueKey: string, targetStatus: string): Promise<void> {
    this.ensureAuthenticated();

    // Fetch available transitions for the issue
    const transitions = await this.fetchTransitions(issueKey);

    // Find the transition that matches the target status
    const transition = transitions.find(
      (t) => t.name.toLowerCase() === targetStatus.toLowerCase() ||
             t.to.name.toLowerCase() === targetStatus.toLowerCase(),
    );

    if (!transition) {
      throw new JiraTransitionError(
        `No available transition to "${targetStatus}" for issue ${issueKey}. ` +
        `Available transitions: ${transitions.map((t) => t.name).join(", ")}`,
      );
    }

    // Execute the transition
    const url = `${this.getBaseUrl()}/rest/api/3/issue/${issueKey}/transitions`;
    const response = await this.fetchWithRetry(url, {
      method: "POST",
      headers: {
        ...this.buildHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ transition: { id: transition.id } }),
    });

    if (response.status === 400 || response.status === 409) {
      const body = await response.text();
      throw new JiraTransitionError(
        `Transition rejected for ${issueKey}: ${response.status} ${body.slice(0, 200)}`,
      );
    }

    if (!response.ok) {
      await this.handleErrorResponse(response);
      return;
    }

    log.info(`[jira-client] Transitioned ${issueKey} via transition "${transition.name}"`);
  }

  async fetchTransitions(issueKey: string): Promise<JiraTransition[]> {
    this.ensureAuthenticated();

    const url = `${this.getBaseUrl()}/rest/api/3/issue/${issueKey}/transitions`;
    const response = await this.fetchWithRetry(url, { headers: this.buildHeaders() });

    if (!response.ok) {
      await this.handleErrorResponse(response);
      return [];
    }

    const data = (await response.json()) as JiraTransitionsResponse;
    return data.transitions;
  }

  async addComment(issueKey: string, body: string): Promise<void> {
    this.ensureAuthenticated();

    // Use Atlassian Document Format (ADF) for the comment body
    const adfBody = {
      version: 1,
      type: "doc",
      content: [
        {
          type: "paragraph",
          content: [
            {
              type: "text",
              text: body,
            },
          ],
        },
      ],
    };

    const url = `${this.getBaseUrl()}/rest/api/3/issue/${issueKey}/comment`;
    const response = await this.fetchWithRetry(url, {
      method: "POST",
      headers: {
        ...this.buildHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ body: adfBody }),
    });

    if (!response.ok) {
      const respBody = await response.text();
      throw new Error(
        `Failed to add comment to ${issueKey}: ${response.status} ${respBody.slice(0, 200)}`,
      );
    }

    log.info(`[jira-client] Added comment to ${issueKey}`);
  }

  async linkPr(issueKey: string, prUrl: string): Promise<void> {
    this.ensureAuthenticated();

    const url = `${this.getBaseUrl()}/rest/api/3/issue/${issueKey}/remotelink`;
    const response = await this.fetchWithRetry(url, {
      method: "POST",
      headers: {
        ...this.buildHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        object: {
          url: prUrl,
          title: `Pull Request: ${prUrl.split("/").pop() ?? "PR"}`,
          icon: {
            url16x16: "https://github.com/favicon.ico",
            title: "Pull Request",
          },
        },
      }),
    });

    if (!response.ok) {
      const respBody = await response.text();
      throw new Error(
        `Failed to link PR to ${issueKey}: ${response.status} ${respBody.slice(0, 200)}`,
      );
    }

    log.info(`[jira-client] Linked PR ${prUrl} to ${issueKey}`);
  }

  // ── Private Helpers ──────────────────────────────────────────

  private ensureAuthenticated(): void {
    if (!this.accessToken) {
      throw new Error("Not authenticated. Call authenticate() first.");
    }
  }

  private getBaseUrl(): string {
    if (this.config.authType === "oauth" && this.oauthTokens?.cloudId) {
      return `https://api.atlassian.com/ex/jira/${this.oauthTokens.cloudId}`;
    }
    return this.config.instanceUrl.replace(/\/$/, "");
  }

  private buildHeaders(): Record<string, string> {
    return {
      Authorization: `Bearer ${this.accessToken}`,
      Accept: "application/json",
      "User-Agent": "crab-desktop",
    };
  }

  private buildJQL(filter: BoardFilter): string {
    const clauses: string[] = [];

    clauses.push(`project = "${this.config.projectKey}"`);

    if (filter.labels && filter.labels.length > 0) {
      const labelList = filter.labels.map((l) => `"${l}"`).join(", ");
      clauses.push(`labels IN (${labelList})`);
    }

    if (filter.excludeLabels && filter.excludeLabels.length > 0) {
      const excludeList = filter.excludeLabels.map((l) => `"${l}"`).join(", ");
      clauses.push(`labels NOT IN (${excludeList})`);
    }

    if (filter.assignees && filter.assignees.length > 0) {
      const assigneeList = filter.assignees.map((a) => `"${a}"`).join(", ");
      clauses.push(`assignee IN (${assigneeList})`);
    }

    if (this.config.sprintId) {
      clauses.push(`sprint = ${this.config.sprintId}`);
    }

    return clauses.join(" AND ") + " ORDER BY updated DESC";
  }

  private mapToExternalIssue(issue: JiraIssueResponse): ExternalIssue {
    return {
      id: issue.key,
      identifier: issue.key,
      title: issue.fields.summary,
      description: this.extractDescription(issue.fields.description),
      status: issue.fields.status.name,
      assignee: issue.fields.assignee?.displayName ?? undefined,
      labels: issue.fields.labels,
      priority: issue.fields.priority?.name ?? "none",
      priorityValue: mapJiraPriorityToNumber(issue.fields.priority?.name),
      url: `${this.config.instanceUrl.replace(/\/$/, "")}/browse/${issue.key}`,
      ...(issue.fields.created ? { createdAt: new Date(issue.fields.created).getTime() } : {}),
      updatedAt: new Date(issue.fields.updated).getTime(),
      blockedBy: extractJiraBlockers(issue.fields.issuelinks ?? []),
    };
  }

  /**
   * Extract plain text from an ADF description or return as-is if it's a string.
   */
  private extractDescription(description: unknown): string {
    if (!description) return "";
    if (typeof description === "string") return description;

    // ADF document — extract text content recursively
    try {
      return this.extractTextFromAdf(description as Record<string, unknown>);
    } catch {
      return "";
    }
  }

  private extractTextFromAdf(node: Record<string, unknown>): string {
    if (node.type === "text" && typeof node.text === "string") {
      return node.text;
    }

    const content = node.content as Array<Record<string, unknown>> | undefined;
    if (Array.isArray(content)) {
      return content.map((child) => this.extractTextFromAdf(child)).join("");
    }

    return "";
  }

  /**
   * Fetch with rate-limit retry logic.
   * Parses Retry-After header on 429 responses and retries up to MAX_RATE_LIMIT_RETRIES times.
   */
  private async fetchWithRetry(url: string, init: RequestInit): Promise<Response> {
    let retries = 0;

    while (retries < MAX_RATE_LIMIT_RETRIES) {
      // Check if OAuth token needs refresh before making the request
      if (this.config.authType === "oauth" && this.oauthTokens) {
        if (this.oauthTokens.expiresAt - Date.now() < 60_000) {
          await this.refreshOAuthToken();
          // Update the Authorization header with the new token
          if (init.headers && typeof init.headers === "object" && !Array.isArray(init.headers)) {
            (init.headers as Record<string, string>).Authorization = `Bearer ${this.accessToken}`;
          }
        }
      }

      const response = await fetch(url, init);

      if (response.status === 429) {
        retries++;

        if (retries >= MAX_RATE_LIMIT_RETRIES) {
          throw new JiraRateLimitError(
            `Rate limit exceeded after ${MAX_RATE_LIMIT_RETRIES} retries for ${url}`,
          );
        }

        const retryAfter = response.headers.get("retry-after");
        let waitMs = 1000 * retries; // Default progressive backoff

        if (retryAfter) {
          // Retry-After can be seconds or an HTTP date
          const seconds = parseInt(retryAfter, 10);
          if (!isNaN(seconds)) {
            waitMs = seconds * 1000;
          } else {
            const date = new Date(retryAfter).getTime();
            if (!isNaN(date)) {
              waitMs = Math.max(date - Date.now(), 1000);
            }
          }
        }

        log.warn(`[jira-client] Rate limited (429). Retry ${retries}/${MAX_RATE_LIMIT_RETRIES} after ${waitMs}ms`);
        await sleep(waitMs);
        continue;
      }

      return response;
    }

    // Should not reach here, but satisfy TypeScript
    throw new JiraRateLimitError("Rate limit retry loop exhausted");
  }

  /**
   * Handle error responses (401/403 emit auth:expired, others throw).
   */
  private async handleErrorResponse(response: Response): Promise<never> {
    if (response.status === 401 || response.status === 403) {
      log.error(`[jira-client] Auth error: ${response.status}`);
      this.emit("auth:expired");
      throw new JiraAuthError(
        `Jira API returned ${response.status}: authentication failed or token expired`,
      );
    }

    const body = await response.text();
    throw new Error(`Jira API ${response.status}: ${body.slice(0, 200)}`);
  }
}

// ── Error Classes ────────────────────────────────────────────────

/**
 * Thrown when Jira returns 401/403 or OAuth flow fails.
 * The board sync layer should catch this and emit an `auth:expired` event.
 */
export class JiraAuthError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "JiraAuthError";
  }
}

/**
 * Thrown when a workflow transition is rejected (400/409) or unavailable.
 * The board sync layer should revert the card to its previous column.
 */
export class JiraTransitionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "JiraTransitionError";
  }
}

/**
 * Thrown when rate limit retries are exhausted.
 */
export class JiraRateLimitError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "JiraRateLimitError";
  }
}

export function mapJiraPriorityToNumber(priorityName: string | null | undefined): number | null {
  const normalized = String(priorityName ?? "").trim().toLowerCase();
  if (!normalized || normalized === "none") return null;
  if (normalized === "highest" || normalized === "blocker" || normalized === "critical") return 1;
  if (normalized === "high" || normalized === "major") return 2;
  if (normalized === "medium" || normalized === "normal") return 3;
  if (normalized === "low" || normalized === "minor" || normalized === "lowest" || normalized === "trivial") return 4;
  return null;
}

function extractJiraBlockers(links: JiraIssueLink[]): NonNullable<ExternalIssue["blockedBy"]> {
  const blockers: NonNullable<ExternalIssue["blockedBy"]> = [];

  for (const link of links) {
    const typeName = String(link.type?.name ?? "").toLowerCase();
    const inward = String(link.type?.inward ?? "").toLowerCase();
    const outward = String(link.type?.outward ?? "").toLowerCase();

    if (link.inwardIssue && (typeName === "blocks" || inward.includes("blocked by"))) {
      blockers.push(jiraBlockerRef(link.inwardIssue));
    }

    if (link.outwardIssue && outward.includes("blocked by")) {
      blockers.push(jiraBlockerRef(link.outwardIssue));
    }
  }

  return blockers;
}

function jiraBlockerRef(issue: JiraIssueLinkIssue): NonNullable<ExternalIssue["blockedBy"]>[number] {
  return {
    id: issue.key ?? null,
    identifier: issue.key ?? null,
    state: issue.fields?.status?.name ?? null,
  };
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
