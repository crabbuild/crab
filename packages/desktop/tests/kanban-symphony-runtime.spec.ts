import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import type { BoardConfig } from "../electron/kanban/kanban-types";
import type { ExternalIssue, TrackerAdapter } from "../electron/kanban/tracker-adapters/types";
import { normalizeLinearIssue, LinearSymphonyTracker } from "../electron/kanban/symphony/linear-adapter";
import { sortIssuesForDispatch } from "../electron/kanban/symphony/scheduler";
import { ExternalSymphonyTracker, normalizeExternalIssue } from "../electron/kanban/symphony/tracker-factory";
import { SymphonyTrackerError } from "../electron/kanban/symphony/tracker-errors";
import { sanitizeWorkspaceKey, assertPathInsideRoot } from "../electron/kanban/symphony/workspace-manager";
import { addTokenDelta, EMPTY_TOKEN_TOTALS, extractAbsoluteTokenTotals } from "../electron/kanban/symphony/token-accounting";
import type { SymphonyConfig, SymphonyIssue, WorkflowDefinition } from "../electron/kanban/symphony/types";

vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

function workflow(): WorkflowDefinition {
  return {
    workflowPath: "/repo/WORKFLOW.md",
    workflowDir: "/repo",
    loadedAtMs: 1,
    mtimeMs: 1,
    promptTemplate: "Implement {{ issue.identifier }}",
    config: {},
  };
}

function config(overrides: Partial<SymphonyConfig> = {}): SymphonyConfig {
  return {
    workflow: workflow(),
    tracker: {
      kind: "linear",
      endpoint: "https://linear.test/graphql",
      apiKey: "token",
      projectSlug: "ENG",
      assignee: null,
      activeStates: ["Todo", "In Progress"],
      terminalStates: ["Done"],
    },
    polling: { intervalMs: 30_000 },
    workspace: { root: "/tmp/symphony" },
    hooks: {
      afterCreate: null,
      beforeRun: null,
      afterRun: null,
      beforeRemove: null,
      timeoutMs: 1_000,
    },
    agent: {
      maxConcurrentAgents: 2,
      maxTurns: 20,
      maxRetryBackoffMs: 60_000,
      maxConcurrentAgentsByState: {},
    },
    codex: {
      command: "codex app-server",
      approvalPolicy: undefined,
      threadSandbox: "workspace-write",
      turnSandboxPolicy: undefined,
      turnTimeoutMs: 60_000,
      readTimeoutMs: 5_000,
      stallTimeoutMs: 30_000,
    },
    ...overrides,
  };
}

function issue(overrides: Partial<SymphonyIssue> = {}): SymphonyIssue {
  return {
    id: "issue-1",
    identifier: "ENG-1",
    title: "Test issue",
    description: null,
    priority: 3,
    state: "Todo",
    branchName: null,
    url: null,
    labels: [],
    blockedBy: [],
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function board(overrides: Partial<BoardConfig> = {}): BoardConfig {
  return {
    id: "board-1",
    name: "Board",
    repoPath: "/repo",
    columns: [{ id: "todo", name: "Todo", trackerStatusMapping: ["Todo", "open"] }],
    trackerType: "linear",
    trackerConfig: { projectSlug: "ENG" },
    autonomyLevel: "moderate",
    pollIntervalMs: 60_000,
    ciPollIntervalMs: 30_000,
    maxIterations: 3,
    filterCriteria: {},
    ...overrides,
  };
}

function adapter(issues: ExternalIssue[], source: TrackerAdapter["source"] = "jira"): TrackerAdapter {
  return {
    source,
    authenticate: vi.fn().mockResolvedValue(undefined),
    fetchIssues: vi.fn().mockResolvedValue(issues),
    transitionIssue: vi.fn().mockResolvedValue(undefined),
    addComment: vi.fn().mockResolvedValue(undefined),
    linkPr: vi.fn().mockResolvedValue(undefined),
  };
}

function linearNode(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: "issue-1",
    identifier: "ENG-1",
    title: "Issue",
    description: null,
    priority: 3,
    branchName: null,
    url: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    state: { name: "Todo" },
    labels: { nodes: [] },
    inverseRelations: { nodes: [] },
    ...overrides,
  };
}

describe("kanban Symphony runtime helpers", () => {
  let tmpDir: string;

  beforeEach(async () => {
    tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "kanban-symphony-runtime-"));
  });

  afterEach(async () => {
    await fs.rm(tmpDir, { recursive: true, force: true });
  });

  it("normalizes Linear issue shape including labels, assignee, branch, and blockers", () => {
    const normalized = normalizeLinearIssue({
      id: "abc",
      identifier: "ENG-42",
      title: "Fix auth",
      description: "",
      priority: 1,
      branchName: "eng-42-fix-auth",
      url: "https://linear.app/acme/issue/ENG-42",
      assignee: { id: "user-1", displayName: "Ada" },
      state: { name: "Todo" },
      labels: { nodes: [{ name: "Bug" }, { name: " Backend " }, { name: "" }] },
      relations: {
        nodes: [
          { type: "blocked_by", relatedIssue: { id: "blocker-1", identifier: "ENG-1", state: { name: "Todo" } } },
          { type: "blocks", relatedIssue: { id: "downstream-1", identifier: "ENG-99", state: { name: "Todo" } } },
        ],
      },
    });

    expect(normalized.labels).toEqual(["bug", "backend"]);
    expect(normalized.assignee).toBe("Ada");
    expect(normalized.branchName).toBe("eng-42-fix-auth");
    expect(normalized.blockedBy).toEqual([
      { id: "blocker-1", identifier: "ENG-1", state: "Todo" },
    ]);
  });

  it("normalizes Linear blockers from inverse relation blocks", () => {
    const normalized = normalizeLinearIssue({
      id: "abc",
      identifier: "ENG-42",
      title: "Fix auth",
      state: { name: "Todo" },
      inverseRelations: {
        nodes: [
          { type: "blocks", issue: { id: "blocker-1", identifier: "ENG-1", state: { name: "In Progress" } } },
        ],
      },
    });

    expect(normalized.blockedBy).toEqual([
      { id: "blocker-1", identifier: "ENG-1", state: "In Progress" },
    ]);
  });

  it("omits the Linear assignee filter when no assignee is configured", async () => {
    const fetcher = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ data: { issues: { nodes: [] } } }),
    });
    const tracker = new LinearSymphonyTracker(config(), fetcher);

    await tracker.fetchCandidateIssues();

    const body = JSON.parse(fetcher.mock.calls[0][1].body) as { query: string; variables: Record<string, unknown> };
    expect(body.query).not.toContain("assignee:");
    expect(body.query).not.toContain("$assignee");
    expect(body.variables).not.toHaveProperty("assignee");
  });

  it("uses Linear pagination and the spec [ID!] refresh query", async () => {
    const fetcher = vi.fn()
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: async () => JSON.stringify({
          data: {
            issues: {
              pageInfo: { hasNextPage: true, endCursor: "cursor-1" },
              nodes: [linearNode({ id: "issue-1", identifier: "ENG-1" })],
            },
          },
        }),
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: async () => JSON.stringify({
          data: {
            issues: {
              pageInfo: { hasNextPage: false, endCursor: null },
              nodes: [linearNode({ id: "issue-2", identifier: "ENG-2" })],
            },
          },
        }),
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: async () => JSON.stringify({
          data: {
            issues: {
              pageInfo: { hasNextPage: false, endCursor: null },
              nodes: [linearNode({ id: "issue-1", identifier: "ENG-1" })],
            },
          },
        }),
      });

    const tracker = new LinearSymphonyTracker(config(), fetcher);
    const candidates = await tracker.fetchCandidateIssues();
    const refreshed = await tracker.fetchIssueStatesByIds(["issue-1"]);

    expect(candidates.map((candidate) => candidate.id)).toEqual(["issue-1", "issue-2"]);
    expect(refreshed.map((candidate) => candidate.id)).toEqual(["issue-1"]);

    const secondCandidateBody = JSON.parse(fetcher.mock.calls[1][1].body) as { variables: Record<string, unknown> };
    expect(secondCandidateBody.variables.after).toBe("cursor-1");

    const refreshBody = JSON.parse(fetcher.mock.calls[2][1].body) as { query: string; variables: Record<string, unknown> };
    expect(refreshBody.query).toContain("$ids: [ID!]!");
    expect(refreshBody.variables.ids).toEqual(["issue-1"]);
  });

  it("no-ops Linear empty state lists and maps typed Linear errors", async () => {
    const base = config();
    const noStateFetcher = vi.fn();
    const noStateTracker = new LinearSymphonyTracker({
      ...base,
      tracker: { ...base.tracker, activeStates: [] },
    }, noStateFetcher);

    await expect(noStateTracker.fetchCandidateIssues()).resolves.toEqual([]);
    expect(noStateFetcher).not.toHaveBeenCalled();

    const failingTracker = new LinearSymphonyTracker(base, vi.fn().mockResolvedValue({
      ok: false,
      status: 429,
      text: async () => "rate limited",
    }));

    await expect(failingTracker.fetchCandidateIssues()).rejects.toMatchObject({
      code: "rate_limit",
      status: 429,
    });
  });

  it("normalizes Jira, GitHub Issues, and GitHub Projects into SymphonyIssue", () => {
    const base = config();
    const jiraConfig: SymphonyConfig = {
      ...base,
      tracker: {
        kind: "jira",
        activeStates: ["To Do"],
        terminalStates: ["Done"],
        instanceUrl: "https://jira.example.com",
        projectKey: "OPS",
        authType: "pat",
        credentialRef: null,
      },
    };
    const githubConfig: SymphonyConfig = {
      ...base,
      tracker: {
        kind: "github",
        activeStates: ["open"],
        terminalStates: ["closed"],
        owner: "acme",
        repo: "desktop",
      },
    };
    const projectConfig: SymphonyConfig = {
      ...base,
      tracker: {
        kind: "github-projects",
        activeStates: ["Todo"],
        terminalStates: ["Done"],
        owner: "acme",
        projectNumber: 7,
        statusFieldName: "Status",
      },
    };

    expect(normalizeExternalIssue({
      id: "OPS-1",
      identifier: "OPS-1",
      title: "Fix Jira",
      description: "Body",
      status: "To Do",
      labels: ["Bug"],
      priority: "Critical",
      priorityValue: null,
      url: "https://jira.example.com/browse/OPS-1",
      createdAt: Date.parse("2026-01-01T00:00:00Z"),
      updatedAt: Date.parse("2026-01-02T00:00:00Z"),
      blockedBy: [{ id: "OPS-0", identifier: "OPS-0", state: "To Do" }],
    }, "jira", jiraConfig)).toMatchObject({
      id: "OPS-1",
      identifier: "OPS-1",
      priority: 1,
      labels: ["bug"],
      blockedBy: [{ id: "OPS-0", identifier: "OPS-0", state: "To Do" }],
    });

    expect(normalizeExternalIssue({
      id: "42",
      title: "Fix GitHub",
      description: "",
      status: "open",
      labels: ["Enhancement"],
      priority: "none",
      url: "https://github.com/acme/desktop/issues/42",
      updatedAt: Date.parse("2026-01-02T00:00:00Z"),
    }, "github", githubConfig)).toMatchObject({
      id: "acme/desktop#42",
      identifier: "acme/desktop#42",
      priority: null,
      blockedBy: [],
    });

    expect(normalizeExternalIssue({
      id: "PVTI_1",
      identifier: "acme/project-7#42",
      title: "Fix Project",
      description: "",
      status: "Todo",
      labels: [],
      priority: "none",
      url: "https://github.com/acme/desktop/issues/42",
      updatedAt: Date.parse("2026-01-02T00:00:00Z"),
    }, "github-projects", projectConfig)).toMatchObject({
      id: "PVTI_1",
      identifier: "acme/project-7#42",
      state: "Todo",
    });
  });

  it("filters external Symphony trackers, refreshes by ID, and maps adapter errors", async () => {
    const base = config();
    const jiraConfig: SymphonyConfig = {
      ...base,
      tracker: {
        kind: "jira",
        activeStates: ["To Do"],
        terminalStates: ["Done"],
        instanceUrl: "https://jira.example.com",
        projectKey: "OPS",
        authType: "pat",
        credentialRef: null,
      },
    };
    const fakeAdapter = adapter([
      {
        id: "OPS-1",
        title: "Ready",
        description: "",
        status: "To Do",
        labels: [],
        priority: "High",
        url: "https://jira.example.com/browse/OPS-1",
        updatedAt: Date.parse("2026-01-01T00:00:00Z"),
      },
      {
        id: "OPS-2",
        title: "Done",
        description: "",
        status: "Done",
        labels: [],
        priority: "Low",
        url: "https://jira.example.com/browse/OPS-2",
        updatedAt: Date.parse("2026-01-02T00:00:00Z"),
      },
    ]);
    const tracker = new ExternalSymphonyTracker(jiraConfig, board(), fakeAdapter);

    await expect(tracker.fetchIssueStatesByIds([])).resolves.toEqual([]);
    expect(fakeAdapter.fetchIssues).not.toHaveBeenCalled();

    await expect(tracker.fetchCandidateIssues()).resolves.toMatchObject([{ id: "OPS-1" }]);
    await expect(tracker.fetchTerminalIssues()).resolves.toMatchObject([{ id: "OPS-2" }]);
    await expect(tracker.fetchIssueStatesByIds(["OPS-1"])).resolves.toMatchObject([{ id: "OPS-1" }]);

    const failingAdapter = adapter([]);
    vi.mocked(failingAdapter.fetchIssues).mockRejectedValue(new Error("GitHub API 429: rate limit"));
    const failingTracker = new ExternalSymphonyTracker(jiraConfig, board(), failingAdapter);

    await expect(failingTracker.fetchCandidateIssues()).rejects.toBeInstanceOf(SymphonyTrackerError);
    await expect(failingTracker.fetchCandidateIssues()).rejects.toMatchObject({ code: "rate_limit", status: 429 });
  });

  it("sorts dispatch candidates by priority, creation time, then identifier", () => {
    const sorted = sortIssuesForDispatch([
      issue({ id: "c", identifier: "ENG-3", priority: 2, createdAt: "2026-01-03T00:00:00Z" }),
      issue({ id: "a", identifier: "ENG-1", priority: 1, createdAt: "2026-01-02T00:00:00Z" }),
      issue({ id: "b", identifier: "ENG-2", priority: 1, createdAt: "2026-01-01T00:00:00Z" }),
    ]);

    expect(sorted.map((candidate) => candidate.identifier)).toEqual(["ENG-2", "ENG-1", "ENG-3"]);
  });

  it("sanitizes workspace keys and rejects traversal or symlink escapes", async () => {
    const outside = await fs.mkdtemp(path.join(os.tmpdir(), "kanban-symphony-outside-"));
    try {
      await fs.symlink(outside, path.join(tmpDir, "escape"));

      expect(sanitizeWorkspaceKey("ENG/42 fix auth")).toBe("ENG_42_fix_auth");
      await expect(assertPathInsideRoot(tmpDir, path.join(tmpDir, "ENG-1"))).resolves.toBeUndefined();
      await expect(assertPathInsideRoot(tmpDir, path.join(tmpDir, "..", "outside"))).rejects.toThrow(
        /outside root/,
      );
      await expect(assertPathInsideRoot(tmpDir, path.join(tmpDir, "escape", "issue"))).rejects.toThrow(
        /outside root/,
      );
    } finally {
      await fs.rm(outside, { recursive: true, force: true });
    }
  });

  it("extracts absolute token totals and adds only deltas", () => {
    const first = extractAbsoluteTokenTotals({
      method: "thread/tokenUsage/updated",
      params: {
        tokenUsage: {
          total: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
        },
      },
    });
    const second = extractAbsoluteTokenTotals({
      type: "codex/event/token_count",
      info: {
        total_token_usage: {
          input_tokens: 25,
          output_tokens: 7,
          total_tokens: 32,
        },
      },
    });

    expect(first).toEqual({ inputTokens: 10, outputTokens: 5, totalTokens: 15 });
    expect(second).toEqual({ inputTokens: 25, outputTokens: 7, totalTokens: 32 });
    expect(addTokenDelta(EMPTY_TOKEN_TOTALS, first!, second!)).toEqual({
      inputTokens: 15,
      outputTokens: 2,
      totalTokens: 17,
    });
  });
});
