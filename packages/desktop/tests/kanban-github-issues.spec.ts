import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import type { BoardFilter, GitHubIssuesConfig } from "../electron/kanban/kanban-types";
import type { ExternalIssue } from "../electron/kanban/tracker-adapters/types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

// Mock keychain
vi.mock("../electron/keychain", () => ({
  getPassword: vi.fn(),
  getCredential: vi.fn(),
}));

// We need to mock global fetch
const mockFetch = vi.fn();
vi.stubGlobal("fetch", mockFetch);

import { GitHubIssuesAdapter, GitHubAuthError } from "../electron/kanban/tracker-adapters/github-issues";
import { getPassword } from "../electron/keychain";

const mockedGetPassword = vi.mocked(getPassword);

function makeGitHubIssue(overrides: Record<string, unknown> = {}) {
  return {
    number: 1,
    title: "Test issue",
    body: "Issue body",
    state: "open",
    html_url: "https://github.com/owner/repo/issues/1",
    user: { login: "testuser" },
    labels: [{ name: "bug" }],
    assignee: { login: "dev1" },
    assignees: [{ login: "dev1" }],
    milestone: { title: "v1.0" },
    created_at: "2024-01-14T10:00:00Z",
    updated_at: "2024-01-15T10:00:00Z",
    ...overrides,
  };
}

function makeResponse(
  body: unknown,
  status = 200,
  headers: Record<string, string> = {},
): Response {
  const defaultHeaders: Record<string, string> = {
    "x-ratelimit-remaining": "4999",
    "x-ratelimit-reset": String(Math.floor(Date.now() / 1000) + 3600),
    ...headers,
  };

  return {
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers(defaultHeaders),
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

describe("GitHubIssuesAdapter", () => {
  let adapter: GitHubIssuesAdapter;
  const config: GitHubIssuesConfig = { owner: "testowner", repo: "testrepo" };

  beforeEach(() => {
    vi.clearAllMocks();
    adapter = new GitHubIssuesAdapter(config);
    mockedGetPassword.mockResolvedValue("ghp_test_token_123");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("authenticate", () => {
    it("retrieves token from keychain", async () => {
      await adapter.authenticate();

      expect(mockedGetPassword).toHaveBeenCalledWith(
        "crab-forge-github",
        "oauth-token",
      );
    });

    it("throws when no token is available", async () => {
      mockedGetPassword.mockResolvedValue(null);

      await expect(adapter.authenticate()).rejects.toThrow(
        "GitHub token not found",
      );
    });

    it("falls back to crab-desktop github-token credential", async () => {
      mockedGetPassword
        .mockResolvedValueOnce(null) // crab-forge-github
        .mockResolvedValueOnce("fallback-token"); // crab-desktop/github-token

      await adapter.authenticate();
      // Should not throw
    });
  });

  describe("fetchIssues", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("fetches issues from the correct endpoint", async () => {
      const issues = [makeGitHubIssue()];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({});

      expect(mockFetch).toHaveBeenCalledTimes(1);
      const calledUrl = mockFetch.mock.calls[0][0] as string;
      expect(calledUrl).toContain("/repos/testowner/testrepo/issues");
      expect(calledUrl).toContain("per_page=100");
      expect(calledUrl).toContain("state=all");
    });

    it("maps GitHub issue to ExternalIssue format", async () => {
      const issues = [makeGitHubIssue({
        number: 42,
        title: "Fix login",
        body: "Login is broken",
        state: "open",
        html_url: "https://github.com/owner/repo/issues/42",
        assignee: { login: "alice" },
        labels: [{ name: "bug" }, { name: "urgent" }],
        created_at: "2024-02-29T12:00:00Z",
        updated_at: "2024-03-01T12:00:00Z",
      })];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({});

      expect(result).toHaveLength(1);
      expect(result[0]).toEqual({
        id: "42",
        identifier: "testowner/testrepo#42",
        title: "Fix login",
        description: "Login is broken",
        status: "open",
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

    it("filters out pull requests from results", async () => {
      const issues = [
        makeGitHubIssue({ number: 1 }),
        makeGitHubIssue({ number: 2, pull_request: { url: "..." } }),
        makeGitHubIssue({ number: 3 }),
      ];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({});

      expect(result).toHaveLength(2);
      expect(result.map((i) => i.id)).toEqual(["1", "3"]);
    });

    it("paginates when a page returns exactly 100 items", async () => {
      // First page: 100 items
      const page1 = Array.from({ length: 100 }, (_, i) =>
        makeGitHubIssue({ number: i + 1 }),
      );
      // Second page: 50 items (less than 100, so pagination stops)
      const page2 = Array.from({ length: 50 }, (_, i) =>
        makeGitHubIssue({ number: i + 101 }),
      );

      mockFetch
        .mockResolvedValueOnce(makeResponse(page1))
        .mockResolvedValueOnce(makeResponse(page2));

      const result = await adapter.fetchIssues({});

      expect(mockFetch).toHaveBeenCalledTimes(2);
      expect(result).toHaveLength(150);

      // Verify page parameters
      const url1 = mockFetch.mock.calls[0][0] as string;
      const url2 = mockFetch.mock.calls[1][0] as string;
      expect(url1).toContain("page=1");
      expect(url2).toContain("page=2");
    });

    it("stops pagination when a page returns fewer than 100 items", async () => {
      const page1 = Array.from({ length: 30 }, (_, i) =>
        makeGitHubIssue({ number: i + 1 }),
      );
      mockFetch.mockResolvedValueOnce(makeResponse(page1));

      const result = await adapter.fetchIssues({});

      expect(mockFetch).toHaveBeenCalledTimes(1);
      expect(result).toHaveLength(30);
    });

    it("applies label filter as query parameter", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse([]));

      await adapter.fetchIssues({ labels: ["bug", "enhancement"] });

      const calledUrl = mockFetch.mock.calls[0][0] as string;
      expect(calledUrl).toContain("labels=bug%2Cenhancement");
    });

    it("applies single assignee filter as query parameter", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse([]));

      await adapter.fetchIssues({ assignees: ["alice"] });

      const calledUrl = mockFetch.mock.calls[0][0] as string;
      expect(calledUrl).toContain("assignee=alice");
    });

    it("applies milestone filter as query parameter", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse([]));

      await adapter.fetchIssues({ milestones: ["v2.0"] });

      const calledUrl = mockFetch.mock.calls[0][0] as string;
      expect(calledUrl).toContain("milestone=v2.0");
    });

    it("excludes issues with excludeLabels (client-side filter)", async () => {
      const issues = [
        makeGitHubIssue({ number: 1, labels: [{ name: "bug" }] }),
        makeGitHubIssue({ number: 2, labels: [{ name: "wontfix" }] }),
        makeGitHubIssue({ number: 3, labels: [{ name: "feature" }] }),
      ];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({ excludeLabels: ["wontfix"] });

      expect(result).toHaveLength(2);
      expect(result.map((i) => i.id)).toEqual(["1", "3"]);
    });

    it("excludeLabels is case-insensitive", async () => {
      const issues = [
        makeGitHubIssue({ number: 1, labels: [{ name: "WontFix" }] }),
        makeGitHubIssue({ number: 2, labels: [{ name: "bug" }] }),
      ];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({ excludeLabels: ["wontfix"] });

      expect(result).toHaveLength(1);
      expect(result[0].id).toBe("2");
    });

    it("filters multiple assignees client-side", async () => {
      const issues = [
        makeGitHubIssue({ number: 1, assignee: { login: "alice" } }),
        makeGitHubIssue({ number: 2, assignee: { login: "bob" } }),
        makeGitHubIssue({ number: 3, assignee: { login: "charlie" } }),
      ];
      mockFetch.mockResolvedValueOnce(makeResponse(issues));

      const result = await adapter.fetchIssues({
        assignees: ["alice", "bob"],
      });

      expect(result).toHaveLength(2);
      expect(result.map((i) => i.id)).toEqual(["1", "2"]);
    });

    it("throws GitHubAuthError on 401 response", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({}, 401));

      await expect(adapter.fetchIssues({})).rejects.toThrow(GitHubAuthError);
    });

    it("throws GitHubAuthError on 403 response", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({}, 403));

      await expect(adapter.fetchIssues({})).rejects.toThrow(GitHubAuthError);
    });

    it("throws on non-OK response", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ message: "Not Found" }, 404),
      );

      await expect(adapter.fetchIssues({})).rejects.toThrow("GitHub API 404");
    });

    it("throws when not authenticated", async () => {
      const unauthAdapter = new GitHubIssuesAdapter(config);

      await expect(unauthAdapter.fetchIssues({})).rejects.toThrow(
        "Not authenticated",
      );
    });
  });

  describe("ETag / conditional requests", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("stores ETag from response and sends If-None-Match on next request", async () => {
      const issues = [makeGitHubIssue()];
      mockFetch.mockResolvedValueOnce(
        makeResponse(issues, 200, { etag: '"abc123"' }),
      );

      await adapter.fetchIssues({});

      // Second request should include If-None-Match
      mockFetch.mockResolvedValueOnce(makeResponse(null, 304));
      const result = await adapter.fetchIssues({});

      const secondCallHeaders = mockFetch.mock.calls[1][1]?.headers as Record<string, string>;
      expect(secondCallHeaders["If-None-Match"]).toBe('"abc123"');
      // Should return cached issues
      expect(result).toHaveLength(1);
    });

    it("stores Last-Modified and sends If-Modified-Since", async () => {
      const issues = [makeGitHubIssue()];
      mockFetch.mockResolvedValueOnce(
        makeResponse(issues, 200, { "last-modified": "Wed, 15 Jan 2024 10:00:00 GMT" }),
      );

      await adapter.fetchIssues({});

      // Second request should include If-Modified-Since
      mockFetch.mockResolvedValueOnce(makeResponse(null, 304));
      const result = await adapter.fetchIssues({});

      const secondCallHeaders = mockFetch.mock.calls[1][1]?.headers as Record<string, string>;
      expect(secondCallHeaders["If-Modified-Since"]).toBe(
        "Wed, 15 Jan 2024 10:00:00 GMT",
      );
      expect(result).toHaveLength(1);
    });
  });

  describe("rate limiting", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("retries on 429 response after waiting", async () => {
      // First call returns 429 with retry-after
      mockFetch
        .mockResolvedValueOnce(
          makeResponse({ message: "rate limited" }, 429, {
            "retry-after": "1",
            "x-ratelimit-remaining": "0",
          }),
        )
        // Retry returns success
        .mockResolvedValueOnce(makeResponse([makeGitHubIssue()]));

      // Use fake timers to avoid waiting
      vi.useFakeTimers();
      const promise = adapter.fetchIssues({});
      await vi.advanceTimersByTimeAsync(1000);
      const result = await promise;
      vi.useRealTimers();

      expect(mockFetch).toHaveBeenCalledTimes(2);
      expect(result).toHaveLength(1);
    });

    it("uses x-ratelimit-reset when retry-after is not present", async () => {
      const resetTime = Math.floor(Date.now() / 1000) + 2; // 2 seconds from now
      mockFetch
        .mockResolvedValueOnce(
          makeResponse({ message: "rate limited" }, 429, {
            "x-ratelimit-remaining": "0",
            "x-ratelimit-reset": String(resetTime),
          }),
        )
        .mockResolvedValueOnce(makeResponse([makeGitHubIssue()]));

      vi.useFakeTimers();
      const promise = adapter.fetchIssues({});
      await vi.advanceTimersByTimeAsync(3000);
      const result = await promise;
      vi.useRealTimers();

      expect(mockFetch).toHaveBeenCalledTimes(2);
      expect(result).toHaveLength(1);
    });
  });

  describe("transitionIssue", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("sends PATCH to close an issue", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ state: "closed" }));

      await adapter.transitionIssue("42", "closed");

      expect(mockFetch).toHaveBeenCalledTimes(1);
      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toBe(
        "https://api.github.com/repos/testowner/testrepo/issues/42",
      );
      expect(opts.method).toBe("PATCH");
      expect(JSON.parse(opts.body as string)).toEqual({ state: "closed" });
    });

    it("sends PATCH to reopen an issue", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ state: "open" }));

      await adapter.transitionIssue("42", "open");

      const [, opts] = mockFetch.mock.calls[0];
      expect(JSON.parse(opts.body as string)).toEqual({ state: "open" });
    });

    it("defaults to open for unknown status", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ state: "open" }));

      await adapter.transitionIssue("42", "in_progress");

      const [, opts] = mockFetch.mock.calls[0];
      expect(JSON.parse(opts.body as string)).toEqual({ state: "open" });
    });

    it("throws on failure", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ message: "Validation Failed" }, 422),
      );

      await expect(adapter.transitionIssue("42", "closed")).rejects.toThrow(
        "Failed to transition issue #42",
      );
    });
  });

  describe("addComment", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("posts a comment to the correct endpoint", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ id: 1 }, 201));

      await adapter.addComment("42", "CI passed!");

      expect(mockFetch).toHaveBeenCalledTimes(1);
      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toBe(
        "https://api.github.com/repos/testowner/testrepo/issues/42/comments",
      );
      expect(opts.method).toBe("POST");
      expect(JSON.parse(opts.body as string)).toEqual({ body: "CI passed!" });
    });

    it("throws on failure", async () => {
      mockFetch.mockResolvedValueOnce(
        makeResponse({ message: "Not Found" }, 404),
      );

      await expect(adapter.addComment("999", "test")).rejects.toThrow(
        "Failed to add comment to issue #999",
      );
    });
  });

  describe("linkPr", () => {
    beforeEach(async () => {
      await adapter.authenticate();
    });

    it("adds a comment with the PR URL", async () => {
      mockFetch.mockResolvedValueOnce(makeResponse({ id: 1 }, 201));

      await adapter.linkPr("42", "https://github.com/owner/repo/pull/10");

      const [url, opts] = mockFetch.mock.calls[0];
      expect(url).toContain("/issues/42/comments");
      const body = JSON.parse(opts.body as string).body as string;
      expect(body).toContain("https://github.com/owner/repo/pull/10");
      expect(body).toContain("Pull Request linked");
    });
  });
});
