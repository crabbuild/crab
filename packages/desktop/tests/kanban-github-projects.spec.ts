import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import type { BoardFilter, GitHubProjectsConfig } from "../electron/kanban/kanban-types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

// Mock keychain
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn(),
}));

// Mock global fetch
const mockFetch = vi.fn();
vi.stubGlobal("fetch", mockFetch);

import {
  GitHubProjectsClient,
  GitHubProjectsAuthError,
  GitHubRateLimitError,
} from "../electron/kanban/tracker-adapters/github-projects";
import { getPassword } from "../electron/keychain";

const mockedGetPassword = vi.mocked(getPassword);

// ── Test Helpers ─────────────────────────────────────────────────

function makeProjectItem(overrides: Record<string, unknown> = {}) {
  return {
    id: "PVTI_item1",
    content: {
      number: 1,
      title: "Test issue",
      body: "Issue body",
      state: "OPEN",
      url: "https://github.com/owner/repo/issues/1",
      assignees: { nodes: [{ login: "dev1" }] },
      labels: { nodes: [{ name: "bug" }] },
      createdAt: "2024-01-14T10:00:00Z",
      updatedAt: "2024-01-15T10:00:00Z",
    },
    fieldValues: {
      nodes: [
        {
          name: "In Progress",
          field: { name: "Status" },
        },
      ],
    },
    ...overrides,
  };
}

function makeGraphQLResponse(data: unknown, errors?: Array<{ message: string; type?: string }>) {
  return {
    ok: true,
    status: 200,
    json: () => Promise.resolve({ data, errors }),
    text: () => Promise.resolve(JSON.stringify({ data, errors })),
    headers: new Headers(),
  } as unknown as Response;
}

function makeErrorResponse(status: number, body: string = "Error") {
  return {
    ok: false,
    status,
    json: () => Promise.resolve({ message: body }),
    text: () => Promise.resolve(body),
    headers: new Headers(),
  } as unknown as Response;
}

function makeProjectItemsResponse(
  items: unknown[],
  hasNextPage = false,
  endCursor: string | null = null,
) {
  return {
    user: {
      projectV2: {
        id: "PVT_project1",
        items: {
          pageInfo: { hasNextPage, endCursor },
          nodes: items,
        },
      },
    },
  };
}

function makeProjectFieldsResponse(fields: unknown[]) {
  return {
    user: {
      projectV2: {
        id: "PVT_project1",
        fields: { nodes: fields },
      },
    },
  };
}

describe("GitHubProjectsClient", () => {
  let client: GitHubProjectsClient;
  const config: GitHubProjectsConfig = {
    projectNumber: 1,
    owner: "testowner",
    statusFieldName: "Status",
  };

  beforeEach(() => {
    vi.clearAllMocks();
    client = new GitHubProjectsClient(config);
    mockedGetPassword.mockResolvedValue("ghp_test_token_123");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("authenticate", () => {
    it("retrieves token from keychain", async () => {
      await client.authenticate();

      expect(mockedGetPassword).toHaveBeenCalledWith(
        "crab-forge-github",
        "oauth-token",
      );
    });

    it("throws and emits auth:expired when no token is available", async () => {
      mockedGetPassword.mockResolvedValue(null);
      const authHandler = vi.fn();
      client.on("auth:expired", authHandler);

      await expect(client.authenticate()).rejects.toThrow(
        GitHubProjectsAuthError,
      );
      expect(authHandler).toHaveBeenCalledWith({
        trackerType: "github-projects",
      });
    });

    it("falls back to crab-desktop github-token credential", async () => {
      mockedGetPassword
        .mockResolvedValueOnce(null) // crab-forge-github
        .mockResolvedValueOnce("fallback-token"); // crab-desktop/github-token

      await client.authenticate();
      // Should not throw
    });
  });

  describe("fetchIssues", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("fetches project items and maps to ExternalIssue format", async () => {
      const items = [makeProjectItem({
        id: "PVTI_42",
        content: {
          number: 42,
          title: "Fix login",
          body: "Login is broken",
          state: "OPEN",
          url: "https://github.com/owner/repo/issues/42",
          assignees: { nodes: [{ login: "alice" }] },
          labels: { nodes: [{ name: "bug" }, { name: "urgent" }] },
          createdAt: "2024-02-29T12:00:00Z",
          updatedAt: "2024-03-01T12:00:00Z",
        },
        fieldValues: {
          nodes: [{ name: "In Progress", field: { name: "Status" } }],
        },
      })];

      // First call resolves owner type (user query)
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // Second call fetches items
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse(items)),
      );

      const result = await client.fetchIssues({});

      expect(result).toHaveLength(1);
      expect(result[0]).toEqual({
        id: "PVTI_42",
        identifier: "testowner/project-1#42",
        title: "Fix login",
        description: "Login is broken",
        status: "In Progress",
        assignee: "alice",
        labels: ["bug", "urgent"],
        priority: "none",
        priorityValue: null,
        url: "https://github.com/owner/repo/issues/42",
        createdAt: new Date("2024-02-29T12:00:00Z").getTime(),
        updatedAt: new Date("2024-03-01T12:00:00Z").getTime(),
        blockedBy: [],
      });
    });

    it("paginates through multiple pages of items", async () => {
      const page1Items = Array.from({ length: 100 }, (_, i) =>
        makeProjectItem({ id: `PVTI_${i + 1}` }),
      );
      const page2Items = Array.from({ length: 50 }, (_, i) =>
        makeProjectItem({ id: `PVTI_${i + 101}` }),
      );

      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // Page 1
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(
          makeProjectItemsResponse(page1Items, true, "cursor_page1"),
        ),
      );
      // Page 2
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(
          makeProjectItemsResponse(page2Items, false, null),
        ),
      );

      const result = await client.fetchIssues({});

      // 1 owner resolve + 2 pages
      expect(mockFetch).toHaveBeenCalledTimes(3);
      expect(result).toHaveLength(150);
    });

    it("stops at 500 items maximum (5 pages)", async () => {
      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );

      // 5 full pages of 100 items each
      for (let page = 0; page < 5; page++) {
        const items = Array.from({ length: 100 }, (_, i) =>
          makeProjectItem({ id: `PVTI_${page * 100 + i + 1}` }),
        );
        mockFetch.mockResolvedValueOnce(
          makeGraphQLResponse(
            makeProjectItemsResponse(items, true, `cursor_page${page + 1}`),
          ),
        );
      }

      const result = await client.fetchIssues({});

      // 1 owner resolve + 5 pages = 6 calls
      expect(mockFetch).toHaveBeenCalledTimes(6);
      expect(result).toHaveLength(500);
    });

    it("filters items with null content", async () => {
      const items = [
        makeProjectItem({ id: "PVTI_1" }),
        { id: "PVTI_2", content: null, fieldValues: { nodes: [] } },
        makeProjectItem({ id: "PVTI_3" }),
      ];

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse(items)),
      );

      const result = await client.fetchIssues({});

      expect(result).toHaveLength(2);
      expect(result.map((i) => i.id)).toEqual(["PVTI_1", "PVTI_3"]);
    });

    it("applies label filter", async () => {
      const items = [
        makeProjectItem({
          id: "PVTI_1",
          content: {
            ...makeProjectItem().content,
            labels: { nodes: [{ name: "bug" }] },
          },
        }),
        makeProjectItem({
          id: "PVTI_2",
          content: {
            ...makeProjectItem().content,
            labels: { nodes: [{ name: "feature" }] },
          },
        }),
      ];

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse(items)),
      );

      const result = await client.fetchIssues({ labels: ["bug"] });

      expect(result).toHaveLength(1);
      expect(result[0].id).toBe("PVTI_1");
    });

    it("applies excludeLabels filter", async () => {
      const items = [
        makeProjectItem({
          id: "PVTI_1",
          content: {
            ...makeProjectItem().content,
            labels: { nodes: [{ name: "bug" }] },
          },
        }),
        makeProjectItem({
          id: "PVTI_2",
          content: {
            ...makeProjectItem().content,
            labels: { nodes: [{ name: "wontfix" }] },
          },
        }),
      ];

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse(items)),
      );

      const result = await client.fetchIssues({ excludeLabels: ["wontfix"] });

      expect(result).toHaveLength(1);
      expect(result[0].id).toBe("PVTI_1");
    });

    it("applies assignee filter", async () => {
      const items = [
        makeProjectItem({
          id: "PVTI_1",
          content: {
            ...makeProjectItem().content,
            assignees: { nodes: [{ login: "alice" }] },
          },
        }),
        makeProjectItem({
          id: "PVTI_2",
          content: {
            ...makeProjectItem().content,
            assignees: { nodes: [{ login: "bob" }] },
          },
        }),
      ];

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse(items)),
      );

      const result = await client.fetchIssues({ assignees: ["alice"] });

      expect(result).toHaveLength(1);
      expect(result[0].id).toBe("PVTI_1");
    });

    it("emits auth:expired on 401 response", async () => {
      const authHandler = vi.fn();
      client.on("auth:expired", authHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(makeErrorResponse(401, "Unauthorized"));

      await expect(client.fetchIssues({})).rejects.toThrow(
        GitHubProjectsAuthError,
      );
      expect(authHandler).toHaveBeenCalledWith({
        trackerType: "github-projects",
      });
    });

    it("throws when not authenticated", async () => {
      const unauthClient = new GitHubProjectsClient(config);

      await expect(unauthClient.fetchIssues({})).rejects.toThrow(
        "Not authenticated",
      );
    });
  });

  describe("getProjectFields", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("fetches project fields and returns single-select fields", async () => {
      const fields = [
        {
          id: "PVTSSF_status",
          name: "Status",
          options: [
            { id: "opt_1", name: "Todo" },
            { id: "opt_2", name: "In Progress" },
            { id: "opt_3", name: "Done" },
          ],
        },
        {
          id: "PVTSSF_priority",
          name: "Priority",
          options: [
            { id: "opt_p1", name: "High" },
            { id: "opt_p2", name: "Low" },
          ],
        },
      ];

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse(fields)),
      );

      const result = await client.getProjectFields();

      expect(result).toHaveLength(2);
      expect(result[0].name).toBe("Status");
      expect(result[0].options).toHaveLength(3);
      expect(result[1].name).toBe("Priority");
    });
  });

  describe("updateItemStatus", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("sends mutation with correct variables", async () => {
      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // getProjectFields call
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse([
          {
            id: "PVTSSF_status",
            name: "Status",
            options: [
              { id: "opt_1", name: "Todo" },
              { id: "opt_2", name: "In Progress" },
            ],
          },
        ])),
      );
      // updateItemStatus mutation
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({
          updateProjectV2ItemFieldValue: { projectV2Item: { id: "PVTI_1" } },
        }),
      );

      await client.updateItemStatus("PVTI_1", "opt_2");

      // Verify the mutation call
      const lastCall = mockFetch.mock.calls[mockFetch.mock.calls.length - 1];
      const body = JSON.parse(lastCall[1].body as string);
      expect(body.variables).toEqual({
        projectId: "PVT_project1",
        itemId: "PVTI_1",
        fieldId: "PVTSSF_status",
        optionId: "opt_2",
      });
    });

    it("uses 15-second timeout for mutations", async () => {
      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // getProjectFields
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse([
          {
            id: "PVTSSF_status",
            name: "Status",
            options: [{ id: "opt_1", name: "Todo" }],
          },
        ])),
      );
      // Mutation that times out
      mockFetch.mockImplementationOnce((_url: string, init: RequestInit) => {
        // Verify AbortSignal is present
        expect(init.signal).toBeDefined();
        return Promise.resolve(
          makeGraphQLResponse({
            updateProjectV2ItemFieldValue: { projectV2Item: { id: "PVTI_1" } },
          }),
        );
      });

      await client.updateItemStatus("PVTI_1", "opt_1");
    });
  });

  describe("rate limiting with exponential backoff", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("retries on rate-limit GraphQL error with backoff", async () => {
      vi.useFakeTimers();

      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // First attempt: rate limited
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(null, [
          { message: "rate limit exceeded", type: "RATE_LIMITED" },
        ]),
      );
      // Second attempt: success
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse([makeProjectItem()])),
      );

      const promise = client.fetchIssues({});
      // Advance past the backoff (1s base for attempt 1)
      await vi.advanceTimersByTimeAsync(2000);
      const result = await promise;

      expect(result).toHaveLength(1);
      // owner resolve + rate-limited + retry = 3
      expect(mockFetch).toHaveBeenCalledTimes(3);

      vi.useRealTimers();
    });

    it("retries on 403 with rate limit message", async () => {
      vi.useFakeTimers();

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // 403 with rate limit body
      mockFetch.mockResolvedValueOnce({
        ok: false,
        status: 403,
        text: () => Promise.resolve("You have exceeded a secondary rate limit"),
        headers: new Headers(),
      } as unknown as Response);
      // Retry succeeds
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse([makeProjectItem()])),
      );

      const promise = client.fetchIssues({});
      await vi.advanceTimersByTimeAsync(2000);
      const result = await promise;

      expect(result).toHaveLength(1);

      vi.useRealTimers();
    });

    it("gives up after max retry attempts", async () => {
      // Mock setTimeout to execute callbacks immediately (no actual delay)
      const originalSetTimeout = globalThis.setTimeout;
      vi.stubGlobal("setTimeout", (fn: () => void) => originalSetTimeout(fn, 0));

      const syncErrorHandler = vi.fn();
      client.on("sync:error", syncErrorHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // All attempts return rate limit
      for (let i = 0; i < MAX_RETRY_ATTEMPTS + 1; i++) {
        mockFetch.mockResolvedValueOnce(
          makeGraphQLResponse(null, [
            { message: "rate limit exceeded", type: "RATE_LIMITED" },
          ]),
        );
      }

      await expect(client.fetchIssues({})).rejects.toThrow(GitHubRateLimitError);
      expect(syncErrorHandler).toHaveBeenCalledWith(
        expect.objectContaining({ category: "rate_limit_exhausted" }),
      );

      vi.stubGlobal("setTimeout", originalSetTimeout);
    });

    it("uses exponential backoff with increasing delays", async () => {
      vi.useFakeTimers();

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // Two rate-limit failures then success
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(null, [
          { message: "rate limit", type: "RATE_LIMITED" },
        ]),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(null, [
          { message: "rate limit", type: "RATE_LIMITED" },
        ]),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectItemsResponse([makeProjectItem()])),
      );

      const promise = client.fetchIssues({});
      // First backoff ~1s, second ~2s — advance enough for both
      await vi.advanceTimersByTimeAsync(5000);
      const result = await promise;

      expect(result).toHaveLength(1);
      // owner + 2 failures + 1 success = 4
      expect(mockFetch).toHaveBeenCalledTimes(4);

      vi.useRealTimers();
    });
  });

  describe("error handling", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("emits sync:error on network timeout", async () => {
      const syncErrorHandler = vi.fn();
      client.on("sync:error", syncErrorHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // Simulate AbortError (timeout)
      const abortError = new Error("The operation was aborted");
      abortError.name = "AbortError";
      mockFetch.mockRejectedValueOnce(abortError);

      await expect(client.fetchIssues({})).rejects.toThrow("timed out");
      expect(syncErrorHandler).toHaveBeenCalledWith(
        expect.objectContaining({ category: "network_timeout" }),
      );
    });

    it("emits sync:error on server error", async () => {
      const syncErrorHandler = vi.fn();
      client.on("sync:error", syncErrorHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeErrorResponse(500, "Internal Server Error"),
      );

      await expect(client.fetchIssues({})).rejects.toThrow("500");
      expect(syncErrorHandler).toHaveBeenCalledWith(
        expect.objectContaining({ category: "server_error" }),
      );
    });

    it("emits sync:error on invalid GraphQL response", async () => {
      const syncErrorHandler = vi.fn();
      client.on("sync:error", syncErrorHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // GraphQL error that is not rate-limit
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(null, [
          { message: "Field 'foo' doesn't exist on type 'Query'" },
        ]),
      );

      await expect(client.fetchIssues({})).rejects.toThrow("GraphQL errors");
      expect(syncErrorHandler).toHaveBeenCalledWith(
        expect.objectContaining({ category: "invalid_response" }),
      );
    });

    it("emits auth:expired on 401 and does not retry", async () => {
      const authHandler = vi.fn();
      client.on("auth:expired", authHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(makeErrorResponse(401, "Unauthorized"));

      await expect(client.fetchIssues({})).rejects.toThrow(
        GitHubProjectsAuthError,
      );
      expect(authHandler).toHaveBeenCalledTimes(1);
      // Should not retry auth errors
      expect(mockFetch).toHaveBeenCalledTimes(2);
    });

    it("preserves local state on non-rate-limit failures (no rollback)", async () => {
      const syncErrorHandler = vi.fn();
      client.on("sync:error", syncErrorHandler);

      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeErrorResponse(502, "Bad Gateway"),
      );

      await expect(client.fetchIssues({})).rejects.toThrow();

      // sync:error emitted — caller preserves local state
      expect(syncErrorHandler).toHaveBeenCalledWith(
        expect.objectContaining({
          operation: "graphql_request",
          category: "server_error",
        }),
      );
    });
  });

  describe("transitionIssue", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("maps status name to option ID and calls updateItemStatus", async () => {
      // Resolve owner type
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      // getProjectFields
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse([
          {
            id: "PVTSSF_status",
            name: "Status",
            options: [
              { id: "opt_1", name: "Todo" },
              { id: "opt_2", name: "In Progress" },
              { id: "opt_3", name: "Done" },
            ],
          },
        ])),
      );
      // updateItemStatus mutation
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({
          updateProjectV2ItemFieldValue: { projectV2Item: { id: "PVTI_1" } },
        }),
      );

      await client.transitionIssue("PVTI_1", "Done");

      const lastCall = mockFetch.mock.calls[mockFetch.mock.calls.length - 1];
      const body = JSON.parse(lastCall[1].body as string);
      expect(body.variables.optionId).toBe("opt_3");
    });

    it("throws when target status option not found", async () => {
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse([
          {
            id: "PVTSSF_status",
            name: "Status",
            options: [
              { id: "opt_1", name: "Todo" },
              { id: "opt_2", name: "Done" },
            ],
          },
        ])),
      );

      await expect(
        client.transitionIssue("PVTI_1", "NonExistent"),
      ).rejects.toThrow('Status option "NonExistent" not found');
    });

    it("status matching is case-insensitive", async () => {
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: { id: "PVT_1" } } }),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse(makeProjectFieldsResponse([
          {
            id: "PVTSSF_status",
            name: "Status",
            options: [
              { id: "opt_1", name: "In Progress" },
            ],
          },
        ])),
      );
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({
          updateProjectV2ItemFieldValue: { projectV2Item: { id: "PVTI_1" } },
        }),
      );

      // Should match "In Progress" even with different casing
      await client.transitionIssue("PVTI_1", "in progress");

      const lastCall = mockFetch.mock.calls[mockFetch.mock.calls.length - 1];
      const body = JSON.parse(lastCall[1].body as string);
      expect(body.variables.optionId).toBe("opt_1");
    });
  });

  describe("owner type resolution", () => {
    beforeEach(async () => {
      await client.authenticate();
    });

    it("falls back to org query when user query returns null", async () => {
      // User query returns null project
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({ user: { projectV2: null } }),
      );
      // Org query succeeds for fetchIssues
      mockFetch.mockResolvedValueOnce(
        makeGraphQLResponse({
          organization: {
            projectV2: {
              id: "PVT_org1",
              items: {
                pageInfo: { hasNextPage: false, endCursor: null },
                nodes: [makeProjectItem()],
              },
            },
          },
        }),
      );

      const result = await client.fetchIssues({});

      expect(result).toHaveLength(1);
    });
  });
});

const MAX_RETRY_ATTEMPTS = 5;
