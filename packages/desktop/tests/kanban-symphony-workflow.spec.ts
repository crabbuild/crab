import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import type { BoardConfig } from "../electron/kanban/kanban-types";
import { parseSymphonyConfig, resolveWorkflowPath, SymphonyConfigStore } from "../electron/kanban/symphony/config";
import { parseWorkflowContent, renderPrompt, SymphonyWorkflowError } from "../electron/kanban/symphony/workflow-loader";
import type { SymphonyIssue, WorkflowDefinition } from "../electron/kanban/symphony/types";

vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

function workflow(overrides: Partial<WorkflowDefinition> = {}): WorkflowDefinition {
  return {
    workflowPath: "/repo/WORKFLOW.md",
    workflowDir: "/repo",
    loadedAtMs: 1,
    mtimeMs: 1,
    promptTemplate: "Implement {{ issue.identifier }}: {{ issue.title }}",
    config: {
      tracker: {
        kind: "linear",
        api_key: "$SYMPHONY_TEST_LINEAR_TOKEN",
        project_slug: "ENG",
      },
      workspace: {
        root: "./.symphony-workspaces",
      },
    },
    ...overrides,
  };
}

function issue(overrides: Partial<SymphonyIssue> = {}): SymphonyIssue {
  return {
    id: "issue-1",
    identifier: "ENG-1",
    title: "Ship runtime",
    description: null,
    priority: 1,
    state: "Todo",
    branchName: "eng-1-ship-runtime",
    url: "https://linear.app/acme/issue/ENG-1",
    labels: ["backend"],
    blockedBy: [],
    assignee: "Ada",
    assigneeId: "user-1",
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-02T00:00:00Z",
    ...overrides,
  };
}

function board(repoPath: string, workflowPath?: string): BoardConfig {
  return {
    id: "board-1",
    name: "Board",
    repoPath,
    columns: [{ id: "todo", name: "Todo", trackerStatusMapping: ["Todo"] }],
    trackerType: "linear",
    trackerConfig: { projectSlug: "ENG" },
    autonomyLevel: "moderate",
    pollIntervalMs: 60_000,
    ciPollIntervalMs: 30_000,
    maxIterations: 3,
    filterCriteria: {},
    symphony: { enabled: true, workflowPath },
  };
}

describe("kanban Symphony workflow/config", () => {
  const originalEnv = process.env.SYMPHONY_TEST_LINEAR_TOKEN;
  let tmpDir: string;

  beforeEach(async () => {
    tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "kanban-symphony-workflow-"));
    process.env.SYMPHONY_TEST_LINEAR_TOKEN = "linear-token";
  });

  afterEach(async () => {
    if (originalEnv === undefined) {
      delete process.env.SYMPHONY_TEST_LINEAR_TOKEN;
    } else {
      process.env.SYMPHONY_TEST_LINEAR_TOKEN = originalEnv;
    }
    await fs.rm(tmpDir, { recursive: true, force: true });
  });

  it("parses YAML front matter and renders prompts with strict Liquid variables", async () => {
    const parsed = parseWorkflowContent(`---
tracker:
  kind: linear
  project_slug: ENG
---
Implement {{ issue.identifier }} for {{ issue.title }}.`);

    expect(parsed.config).toEqual({
      tracker: { kind: "linear", project_slug: "ENG" },
    });
    await expect(renderPrompt(parsed, { issue: issue(), attempt: 2 })).resolves.toContain("ENG-1");
    await expect(
      renderPrompt({ promptTemplate: "Missing {{ issue.not_real }}" }, { issue: issue(), attempt: null }),
    ).rejects.toMatchObject({ code: "template_render_error" });
  });

  it("resolves env secrets, defaults, and validates dispatch requirements", () => {
    const config = parseSymphonyConfig(workflow({
      config: {
        tracker: {
          kind: "linear",
          api_key: "$SYMPHONY_TEST_LINEAR_TOKEN",
          project_slug: "ENG",
          active_states: ["Todo"],
        },
        polling: { interval_ms: 15_000 },
        agent: { max_concurrent_agents: 2 },
        workspace: { root: "./workspaces" },
      },
    }));

    expect(config.tracker.apiKey).toBe("linear-token");
    expect(config.tracker.activeStates).toEqual(["Todo"]);
    expect(config.polling.intervalMs).toBe(15_000);
    expect(config.agent.maxConcurrentAgents).toBe(2);
    expect(config.workspace.root).toBe(path.normalize("/repo/workspaces"));
  });

  it("merges non-Linear tracker config from WORKFLOW.md with board fallback", () => {
    const jiraBoard: BoardConfig = {
      ...board("/repo"),
      trackerType: "jira",
      trackerConfig: {
        instanceUrl: "https://jira.example.com",
        projectKey: "OPS",
        authType: "oauth",
        credentialRef: "client-id",
      },
    };

    const config = parseSymphonyConfig(workflow({
      config: {
        tracker: {
          kind: "jira",
          active_states: ["Selected for Development"],
        },
      },
    }), jiraBoard);

    expect(config.tracker.kind).toBe("jira");
    expect(config.tracker.instanceUrl).toBe("https://jira.example.com");
    expect(config.tracker.projectKey).toBe("OPS");
    expect(config.tracker.authType).toBe("oauth");
    expect(config.tracker.activeStates).toEqual(["Selected for Development"]);
  });

  it("falls back to the board tracker kind when WORKFLOW.md omits tracker.kind", () => {
    const githubBoard: BoardConfig = {
      ...board("/repo"),
      trackerType: "github",
      trackerConfig: { owner: "acme", repo: "desktop" },
    };

    const config = parseSymphonyConfig(workflow({
      config: {
        tracker: {},
      },
    }), githubBoard);

    expect(config.tracker.kind).toBe("github");
    expect(config.tracker.owner).toBe("acme");
    expect(config.tracker.repo).toBe("desktop");
    expect(config.tracker.activeStates).toEqual(["open"]);
    expect(config.tracker.terminalStates).toEqual(["closed"]);
  });

  it("keeps explicit workflow paths contained within the repo", () => {
    expect(resolveWorkflowPath(board("/repo/project", "ops/WORKFLOW.md"))).toBe(
      path.resolve("/repo/project/ops/WORKFLOW.md"),
    );
    expect(() => resolveWorkflowPath(board("/repo/project", "../WORKFLOW.md"))).toThrow(SymphonyWorkflowError);
  });

  it("retains the last known good config when a workflow reload becomes invalid", async () => {
    const workflowPath = path.join(tmpDir, "WORKFLOW.md");
    await fs.writeFile(workflowPath, `---
tracker:
  kind: linear
  api_key: "$SYMPHONY_TEST_LINEAR_TOKEN"
  project_slug: ENG
---
Implement {{ issue.identifier }}.
`);

    const store = new SymphonyConfigStore();
    const initial = await store.loadInitial(board(tmpDir));
    expect(initial.config?.tracker.projectSlug).toBe("ENG");
    expect(initial.error).toBeNull();

    await fs.writeFile(workflowPath, `---
tracker:
  kind: unsupported
---
bad
`);
    const reload = await store.loadInitial(board(tmpDir));
    expect(reload.config?.tracker.projectSlug).toBe("ENG");
    expect(reload.error?.code).toBe("unsupported_tracker_kind");
  });
});
