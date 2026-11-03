import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import type { JiraTrackerConfig } from "../electron/kanban/kanban-types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

// Mock keychain
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn(),
  setPassword: vi.fn(),
}));

// Mock electron shell
vi.mock("electron", () => ({
  shell: { openExternal: vi.fn() },
}));

// Mock global fetch
const mockFetch = vi.fn();
vi.stubGlobal("fetch", mockFetch);

import {
  JiraClient,
  JiraAuthError,
  JiraTransitionError,
  JiraRateLimitError,
} from "../electron/kanban/tracker-adapters/jira-client";
import { getPassword, setPassword } from "../electron/keychain";

const mockedGetPassword = vi.mocked(getPassword);
const mockedSetPassword = vi.mocked(setPassword);

// ── Helpers ──────────────────────────────────────────────────────

function makeJiraIssue(overrides: Record<string, unknown> = {}) {
  return {
    key: "PROJ-1",
    fields: {
      summary: "Test issue",
      description: null,
      status: { name: "To Do" },
      assignee: { displayName: "Alice", accountId: "abc123" },
      labels: ["bug"],
      priority: { name: "High" },
      created: "2024-01-14T10:00:00.000Z",
      updated: "2024-01-15T10:00:00.000Z",
      issuelinks: [],
    },
    self: "https://mysite.atlassian.net/rest/api/3/issue/10001",
    ...overrides,
  };
}

function makeResponse(
  body: unknown,
  status = 200,
  headers: Record<string, string> = {},
): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers(headers),
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(typeof body === "string" ? body : JSON.stringify(body)),
  } as unknown as Response;
}

function makeCloudConfig(overrides: Partial<JiraTrackerConfig> = {}): JiraTrackerConfig {
  return {
    instanceUrl: "https://mysite.atlassian.net",
    projectKey: "PROJ",
    authType: "oauth",
    credentialRef: "test-client-id",
    ...overrides,
  };
}

function makePATConfig(overrides: Partial<JiraTrackerConfig> = {}): JiraTrackerConfig {
  return {
    instanceUrl: "https://jira.company.com",
    projectKey: "PROJ",
    authType: "pat",
    credentialRef: "crab-jira-pat",
    ...overrides,
  };
}

function makeStoredOAuthTokens(overrides: Record<string, unknown> = {}) {
  return JSON.stringify({
    accessToken: "test-access-token",
    refreshToken: "test-refresh-token",
    expiresAt: Date.now() + 3600_000, // 1 hour from now
    cloudId: "cloud-123",
    ...overrides,
  });
}

// ── Tests ────────────────────────────────────────────────────────

describe("JiraClient", () => {
  afterEach(() => {
    vi.clearAllMocks();
    vi.restoreAllMocks();
  });

  describe("PAT authentication", () => {
    let client: JiraClient;

    beforeEach(() => {
      client = new JiraClient(makePATConfig());
    });

    it("retrieves PAT from keychain", async () => {
      mockedGetPassword.mockResolvedValue("my-pat-token");

      await client.authenticate();

      expect(mockedGetPassword).toHaveBeenCalledWith("crab-jira-pat", "token");
    });

    it("throws JiraAuthError when PAT is not found", async () => {
      mockedGetPassword.mockResolvedValue(null);

      await expect(client.authenticate()).rejects.toThrow(JiraAuthError);
      await expect(client.authenticate()).rejects.toThrow("PAT not found");
    });

    it("uses PAT as Bearer token in requests", async () => {
      mockedGetPassword.mockResolvedValue("my-pat-token");
      await client.authenticate();

      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({});

      const [, opts] = mockFetch.mock.calls[0];
      expect(opts.headers.Authorization).toBe("Bearer my-pat-token");
    });
  });

  describe("OAuth authentication", () => {
    let client: JiraClient;

    beforeEach(() => {
      client = new JiraClient(makeCloudConfig());
    });

    it("loads stored OAuth tokens from keychain", async () => {
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());

      await client.authenticate();

      expect(mockedGetPassword).toHaveBeenCalledWith("crab-jira-oauth", "tokens");
    });

    it("refreshes token when stored token is near expiry", async () => {
      // Token expires in 2 minutes (less than 5-minute threshold)
      mockedGetPassword.mockResolvedValue(
        makeStoredOAuthTokens({ expiresAt: Date.now() + 120_000 }),
      );

      // Mock the refresh token endpoint
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          access_token: "new-access-token",
          refresh_token: "new-refresh-token",
          expires_in: 3600,
        }),
      );

      await client.authenticate();

      // Should have called the token endpoint for refresh
      expect(mockFetch).toHaveBeenCalledTimes(1);
      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toBe("https://auth.atlassian.com/oauth/token");
      const body = JSON.parse(opts.body as string);
      expect(body.grant_type).toBe("refresh_token");
      expect(body.refresh_token).toBe("test-refresh-token");
    });

    it("stores refreshed tokens in keychain", async () => {
      mockedGetPassword.mockResolvedValue(
        makeStoredOAuthTokens({ expiresAt: Date.now() + 120_000 }),
      );

      mockFetch.mockResolvedValueOnce(
        makeResponse({
          access_token: "new-access-token",
          refresh_token: "new-refresh-token",
          expires_in: 3600,
        }),
      );

      await client.authenticate();

      expect(mockedSetPassword).toHaveBeenCalledWith(
        "crab-jira-oauth",
        "tokens",
        expect.stringContaining("new-access-token"),
      );
    });

    it("emits auth:expired when refresh fails", async () => {
      mockedGetPassword.mockResolvedValueOnce(
        makeStoredOAuthTokens({ expiresAt: Date.now() + 120_000 }),
      );

      mockFetch.mockResolvedValueOnce(makeResponse("Unauthorized", 401));

      const authExpiredHandler = vi.fn();
      client.on("auth:expired", authExpiredHandler);

      await expect(client.authenticate()).rejects.toThrow(JiraAuthError);
      expect(authExpiredHandler).toHaveBeenCalled();
    });
  });

  describe("fetchIssues", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("fetches issues using JQL with project filter", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({});

      const [url] = mockFetch.mock.calls[0];
      expect(url).toContain("/rest/api/3/search");
      expect(url).toContain("project");
      expect(url).toContain("PROJ");
    });

    it("uses Cloud API base URL with cloudId for OAuth", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({});

      const [url] = mockFetch.mock.calls[0];
      expect(url as string).toContain("https://api.atlassian.com/ex/jira/cloud-123");
    });

    it("maps Jira issue to ExternalIssue format", async () => {
      const issue = makeJiraIssue({
        key: "PROJ-42",
        fields: {
          summary: "Fix login bug",
          description: { type: "doc", version: 1, content: [{ type: "paragraph", content: [{ type: "text", text: "Login is broken" }] }] },
          status: { name: "In Progress" },
          assignee: { displayName: "Bob", accountId: "xyz" },
          labels: ["bug", "urgent"],
          priority: { name: "Critical" },
          created: "2024-02-29T12:00:00.000Z",
          updated: "2024-03-01T12:00:00.000Z",
          issuelinks: [
            {
              type: { name: "Blocks", inward: "is blocked by", outward: "blocks" },
              inwardIssue: { key: "PROJ-1", fields: { status: { name: "In Progress" } } },
            },
          ],
        },
      });

      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 1, issues: [issue] }),
      );

      const result = await client.fetchIssues({});

      expect(result).toHaveLength(1);
      expect(result[0]).toEqual({
        id: "PROJ-42",
        identifier: "PROJ-42",
        title: "Fix login bug",
        description: "Login is broken",
        status: "In Progress",
        assignee: "Bob",
        labels: ["bug", "urgent"],
        priority: "Critical",
        priorityValue: 1,
        url: "https://mysite.atlassian.net/browse/PROJ-42",
        createdAt: new Date("2024-02-29T12:00:00.000Z").getTime(),
        updatedAt: new Date("2024-03-01T12:00:00.000Z").getTime(),
        blockedBy: [{ id: "PROJ-1", identifier: "PROJ-1", state: "In Progress" }],
      });
    });

    it("paginates with startAt when total exceeds maxResults", async () => {
      // First page: 100 items, total 150
      const page1Issues = Array.from({ length: 100 }, (_, i) =>
        makeJiraIssue({ key: `PROJ-${i + 1}` }),
      );
      // Second page: 50 items
      const page2Issues = Array.from({ length: 50 }, (_, i) =>
        makeJiraIssue({ key: `PROJ-${i + 101}` }),
      );

      mockFetch
        .mockResolvedValueOnce(
          makeResponse({ startAt: 0, maxResults: 100, total: 150, issues: page1Issues }),
        )
        .mockResolvedValueOnce(
          makeResponse({ startAt: 100, maxResults: 100, total: 150, issues: page2Issues }),
        );

      const result = await client.fetchIssues({});

      expect(mockFetch).toHaveBeenCalledTimes(2);
      expect(result).toHaveLength(150);

      // Verify pagination parameters
      const url1 = mockFetch.mock.calls[0][0] as string;
      const url2 = mockFetch.mock.calls[1][0] as string;
      expect(url1).toContain("startAt=0");
      expect(url2).toContain("startAt=100");
    });

    it("stops pagination when fewer than maxResults returned", async () => {
      const issues = Array.from({ length: 30 }, (_, i) =>
        makeJiraIssue({ key: `PROJ-${i + 1}` }),
      );

      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 30, issues }),
      );

      const result = await client.fetchIssues({});

      expect(mockFetch).toHaveBeenCalledTimes(1);
      expect(result).toHaveLength(30);
    });

    it("includes label filter in JQL", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({ labels: ["bug", "feature"] });

      const [url] = mockFetch.mock.calls[0];
      const decodedUrl = decodeURIComponent(url as string).replace(/\+/g, " ");
      expect(decodedUrl).toContain('labels IN ("bug", "feature")');
    });

    it("includes excludeLabels filter in JQL", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({ excludeLabels: ["wontfix"] });

      const [url] = mockFetch.mock.calls[0];
      const decodedUrl = decodeURIComponent(url as string).replace(/\+/g, " ");
      expect(decodedUrl).toContain('labels NOT IN ("wontfix")');
    });

    it("includes assignee filter in JQL", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({ assignees: ["alice", "bob"] });

      const [url] = mockFetch.mock.calls[0];
      const decodedUrl = decodeURIComponent(url as string).replace(/\+/g, " ");
      expect(decodedUrl).toContain('assignee IN ("alice", "bob")');
    });

    it("throws JiraAuthError on 401 response", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse("Unauthorized", 401));

      const authExpiredHandler = vi.fn();
      client.on("auth:expired", authExpiredHandler);

      await expect(client.fetchIssues({})).rejects.toThrow(JiraAuthError);
      expect(authExpiredHandler).toHaveBeenCalled();
    });

    it("throws JiraAuthError on 403 response", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse("Forbidden", 403));

      const authExpiredHandler = vi.fn();
      client.on("auth:expired", authExpiredHandler);

      await expect(client.fetchIssues({})).rejects.toThrow(JiraAuthError);
      expect(authExpiredHandler).toHaveBeenCalled();
    });

    it("throws when not authenticated", async () => {
      const unauthClient = new JiraClient(makeCloudConfig());

      await expect(unauthClient.fetchIssues({})).rejects.toThrow(
        "Not authenticated",
      );
    });

    it("handles null description gracefully", async () => {
      const issue = makeJiraIssue({
        fields: {
          summary: "No description",
          description: null,
          status: { name: "Open" },
          assignee: null,
          labels: [],
          priority: null,
          updated: "2024-01-01T00:00:00.000Z",
        },
      });

      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 1, issues: [issue] }),
      );

      const result = await client.fetchIssues({});

      expect(result[0].description).toBe("");
      expect(result[0].assignee).toBeUndefined();
      expect(result[0].priority).toBe("none");
      expect(result[0].priorityValue).toBeNull();
    });
  });

  describe("transitionIssue", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("fetches available transitions and executes matching one", async () => {
      // First call: fetch transitions
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "11", name: "Start Progress", to: { id: "3", name: "In Progress" } },
            { id: "21", name: "Done", to: { id: "5", name: "Done" } },
          ],
        }),
      );
      // Second call: execute transition
      mockFetch.mockResolvedValueOnce(makeResponse(null, 204));

      await client.transitionIssue("PROJ-42", "In Progress");

      expect(mockFetch).toHaveBeenCalledTimes(2);

      // Verify transition execution
      const [url, opts] = mockFetch.mock.calls[1];
      expect(url).toContain("/rest/api/3/issue/PROJ-42/transitions");
      expect(opts.method).toBe("POST");
      expect(JSON.parse(opts.body as string)).toEqual({
        transition: { id: "11" },
      });
    });

    it("matches transition by name (case-insensitive)", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "31", name: "DONE", to: { id: "5", name: "Done" } },
          ],
        }),
      );
      mockFetch.mockResolvedValueOnce(makeResponse(null, 204));

      await client.transitionIssue("PROJ-1", "done");

      const [, opts] = mockFetch.mock.calls[1];
      expect(JSON.parse(opts.body as string)).toEqual({
        transition: { id: "31" },
      });
    });

    it("matches transition by target status name", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "41", name: "Move to Review", to: { id: "4", name: "In Review" } },
          ],
        }),
      );
      mockFetch.mockResolvedValueOnce(makeResponse(null, 204));

      await client.transitionIssue("PROJ-1", "In Review");

      const [, opts] = mockFetch.mock.calls[1];
      expect(JSON.parse(opts.body as string)).toEqual({
        transition: { id: "41" },
      });
    });

    it("throws JiraTransitionError when no matching transition found", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "11", name: "Start", to: { id: "3", name: "In Progress" } },
          ],
        }),
      );

      await expect(
        client.transitionIssue("PROJ-1", "Nonexistent Status"),
      ).rejects.toThrow(JiraTransitionError);
    });

    it("throws JiraTransitionError on 400 response", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "11", name: "Done", to: { id: "5", name: "Done" } },
          ],
        }),
      );
      mockFetch.mockResolvedValueOnce(
        makeResponse({ errorMessages: ["Transition not valid"] }, 400),
      );

      await expect(
        client.transitionIssue("PROJ-1", "Done"),
      ).rejects.toThrow(JiraTransitionError);
    });

    it("throws JiraTransitionError on 409 response", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({
          transitions: [
            { id: "11", name: "Done", to: { id: "5", name: "Done" } },
          ],
        }),
      );
      mockFetch.mockResolvedValueOnce(
        makeResponse({ errorMessages: ["Conflict"] }, 409),
      );

      await expect(
        client.transitionIssue("PROJ-1", "Done"),
      ).rejects.toThrow(JiraTransitionError);
    });
  });

  describe("addComment", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("posts comment in ADF format", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ id: "10001" }, 201));

      await client.addComment("PROJ-42", "CI passed successfully!");

      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toContain("/rest/api/3/issue/PROJ-42/comment");
      expect(opts.method).toBe("POST");

      const body = JSON.parse(opts.body as string);
      expect(body.body).toEqual({
        version: 1,
        type: "doc",
        content: [
          {
            type: "paragraph",
            content: [
              { type: "text", text: "CI passed successfully!" },
            ],
          },
        ],
      });
    });

    it("throws on failure", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ errorMessages: ["Issue not found"] }, 404),
      );

      await expect(
        client.addComment("PROJ-999", "test"),
      ).rejects.toThrow("Failed to add comment to PROJ-999");
    });
  });

  describe("linkPr", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("creates a remote link via the remotelink API", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ id: 10000 }, 201));

      await client.linkPr("PROJ-42", "https://github.com/owner/repo/pull/10");

      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toContain("/rest/api/3/issue/PROJ-42/remotelink");
      expect(opts.method).toBe("POST");

      const body = JSON.parse(opts.body as string);
      expect(body.object.url).toBe("https://github.com/owner/repo/pull/10");
      expect(body.object.title).toContain("Pull Request");
    });

    it("throws on failure", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ errorMessages: ["Permission denied"] }, 403),
      );

      await expect(
        client.linkPr("PROJ-42", "https://github.com/owner/repo/pull/10"),
      ).rejects.toThrow("Failed to link PR to PROJ-42");
    });
  });

  describe("rate limiting", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("retries on 429 with Retry-After header", async () => {
      vi.useFakeTimers();

      // First call: 429 with Retry-After
      mockFetch
        .mockResolvedValueOnce(
          makeResponse("Rate limited", 429, { "retry-after": "1" }),
        )
        // Retry: success
        .mockResolvedValueOnce(
          makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
        );

      const promise = client.fetchIssues({});
      await vi.advanceTimersByTimeAsync(1000);
      const result = await promise;

      vi.useRealTimers();

      expect(mockFetch).toHaveBeenCalledTimes(2);
      expect(result).toHaveLength(0);
    });

    it("retries up to 5 times then throws JiraRateLimitError", async () => {
      vi.useFakeTimers();

      // All calls return 429
      for (let i = 0; i < 5; i++) {
        mockFetch.mockResolvedValueOnce(
          makeResponse("Rate limited", 429, { "retry-after": "1" }),
        );
      }

      const promise = client.fetchIssues({}).catch((err) => {
        expect(err).toBeInstanceOf(JiraRateLimitError);
        return err;
      });

      // Advance time for each retry (5 retries × 1s each)
      await vi.advanceTimersByTimeAsync(6000);
      await promise;

      vi.useRealTimers();
    });

    it("parses Retry-After as seconds", async () => {
      vi.useFakeTimers();

      mockFetch
        .mockResolvedValueOnce(
          makeResponse("Rate limited", 429, { "retry-after": "3" }),
        )
        .mockResolvedValueOnce(
          makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
        );

      const promise = client.fetchIssues({});

      // Should wait 3 seconds
      await vi.advanceTimersByTimeAsync(3000);
      await promise;

      vi.useRealTimers();

      expect(mockFetch).toHaveBeenCalledTimes(2);
    });

    it("uses progressive backoff when Retry-After is missing", async () => {
      vi.useFakeTimers();

      mockFetch
        .mockResolvedValueOnce(makeResponse("Rate limited", 429))
        .mockResolvedValueOnce(
          makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
        );

      const promise = client.fetchIssues({});

      // First retry uses 1000ms (1 * 1000)
      await vi.advanceTimersByTimeAsync(1000);
      await promise;

      vi.useRealTimers();

      expect(mockFetch).toHaveBeenCalledTimes(2);
    });
  });

  describe("auth:expired event", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makeCloudConfig());
      mockedGetPassword.mockResolvedValue(makeStoredOAuthTokens());
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("emits auth:expired on 401 during fetchIssues", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse("Unauthorized", 401));

      const handler = vi.fn();
      client.on("auth:expired", handler);

      await expect(client.fetchIssues({})).rejects.toThrow(JiraAuthError);
      expect(handler).toHaveBeenCalledTimes(1);
    });

    it("emits auth:expired on 403 during fetchIssues", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse("Forbidden", 403));

      const handler = vi.fn();
      client.on("auth:expired", handler);

      await expect(client.fetchIssues({})).rejects.toThrow(JiraAuthError);
      expect(handler).toHaveBeenCalledTimes(1);
    });

    it("emits auth:expired on 401 during transitionIssue", async () => {
      // Fetch transitions returns 401
      mockFetch.mockResolvedValueOnce(makeResponse("Unauthorized", 401));

      const handler = vi.fn();
      client.on("auth:expired", handler);

      await expect(
        client.transitionIssue("PROJ-1", "Done"),
      ).rejects.toThrow(JiraAuthError);
      expect(handler).toHaveBeenCalledTimes(1);
    });
  });

  describe("PAT with Server/DC base URL", () => {
    let client: JiraClient;

    beforeEach(async () => {
      client = new JiraClient(makePATConfig());
      mockedGetPassword.mockResolvedValue("server-pat-token");
      await client.authenticate();
      mockFetch.mockClear();
    });

    it("uses instance URL directly for Server/DC", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ startAt: 0, maxResults: 100, total: 0, issues: [] }),
      );

      await client.fetchIssues({});

      const [url] = mockFetch.mock.calls[0];
      expect(url as string).toContain("https://jira.company.com/rest/api/3/search");
    });
  });
});
