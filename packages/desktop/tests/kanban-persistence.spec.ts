import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";

import {
  atomicWrite,
  atomicRead,
  BoardConfigStore,
  RuleStore,
  AnalyticsStore,
  BoardStateStore,
} from "../electron/kanban/kanban-persistence";

import type {
  PersistedBoardData,
  PersistedRuleData,
  PersistedAnalyticsData,
  PersistedBoardState,
  BoardConfig,
  AutomationRule,
  CardAnalytics,
  KanbanCard,
  KanbanTaskRun,
} from "../electron/kanban/kanban-types";

// Mock electron-log to avoid requiring Electron in tests
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

describe("kanban-persistence", () => {
  let tmpDir: string;

  beforeEach(async () => {
    tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "kanban-persist-test-"));
  });

  afterEach(async () => {
    await fs.rm(tmpDir, { recursive: true, force: true });
  });

  describe("atomicWrite", () => {
    it("writes JSON data to the specified file", async () => {
      const filePath = path.join(tmpDir, "test.json");
      const data = { version: 1, items: ["a", "b"] };

      await atomicWrite(filePath, data);

      const raw = await fs.readFile(filePath, "utf-8");
      expect(JSON.parse(raw)).toEqual(data);
    });

    it("creates parent directories if they do not exist", async () => {
      const filePath = path.join(tmpDir, "nested", "deep", "test.json");
      await atomicWrite(filePath, { ok: true });

      const raw = await fs.readFile(filePath, "utf-8");
      expect(JSON.parse(raw)).toEqual({ ok: true });
    });

    it("overwrites existing file atomically", async () => {
      const filePath = path.join(tmpDir, "test.json");
      await atomicWrite(filePath, { version: 1 });
      await atomicWrite(filePath, { version: 2 });

      const raw = await fs.readFile(filePath, "utf-8");
      expect(JSON.parse(raw)).toEqual({ version: 2 });
    });

    it("does not leave temp file on success", async () => {
      const filePath = path.join(tmpDir, "test.json");
      await atomicWrite(filePath, { data: true });

      const files = await fs.readdir(tmpDir);
      expect(files).toEqual(["test.json"]);
    });

    it("survives simulated crash (temp file left behind from prior write)", async () => {
      const filePath = path.join(tmpDir, "test.json");

      // Simulate a prior crash that left a temp file
      const staleTmp = `${filePath}.tmp.99999`;
      await fs.writeFile(staleTmp, "stale data", "utf-8");

      // Write the real data
      await atomicWrite(filePath, { version: 1, boards: [] });

      const raw = await fs.readFile(filePath, "utf-8");
      expect(JSON.parse(raw)).toEqual({ version: 1, boards: [] });

      // The stale temp file from a different PID should still exist (not our concern)
      // but our own temp file should be gone
      const files = await fs.readdir(tmpDir);
      const tmpFiles = files.filter((f) => f.includes(`.tmp.${process.pid}`));
      expect(tmpFiles).toHaveLength(0);
    });
  });

  describe("atomicRead", () => {
    const validate = (raw: unknown) => {
      const obj = raw as Record<string, unknown>;
      if (obj.version !== 1 || !Array.isArray(obj.items)) {
        throw new Error("Invalid schema");
      }
      return obj as { version: 1; items: string[] };
    };

    const migrate = (data: Record<string, unknown>) => {
      if (!data.version || data.version === 0) {
        return { version: 1, items: data.items ?? [] };
      }
      return data;
    };

    it("returns null for missing file", async () => {
      const result = await atomicRead(
        path.join(tmpDir, "missing.json"),
        validate,
        migrate,
      );
      expect(result).toBeNull();
    });

    it("reads and validates a correct file", async () => {
      const filePath = path.join(tmpDir, "data.json");
      await fs.writeFile(
        filePath,
        JSON.stringify({ version: 1, items: ["x"] }),
        "utf-8",
      );

      const result = await atomicRead(filePath, validate, migrate);
      expect(result).toEqual({ version: 1, items: ["x"] });
    });

    it("migrates from v0 to v1", async () => {
      const filePath = path.join(tmpDir, "data.json");
      // v0 data: no version field, bare items
      await fs.writeFile(
        filePath,
        JSON.stringify({ items: ["old"] }),
        "utf-8",
      );

      const result = await atomicRead(filePath, validate, migrate);
      expect(result).toEqual({ version: 1, items: ["old"] });
    });

    it("renames corrupted file to .bak on invalid JSON", async () => {
      const filePath = path.join(tmpDir, "data.json");
      await fs.writeFile(filePath, "not valid json {{{", "utf-8");

      const result = await atomicRead(filePath, validate, migrate);
      expect(result).toBeNull();

      // .bak file should exist
      const bakPath = `${filePath}.bak`;
      const bakContent = await fs.readFile(bakPath, "utf-8");
      expect(bakContent).toBe("not valid json {{{");

      // Original file should be gone
      await expect(fs.access(filePath)).rejects.toThrow();
    });

    it("renames corrupted file to .bak on validation failure", async () => {
      const filePath = path.join(tmpDir, "data.json");
      // Valid JSON but wrong schema
      await fs.writeFile(
        filePath,
        JSON.stringify({ version: 1, wrongField: true }),
        "utf-8",
      );

      const result = await atomicRead(filePath, validate, migrate);
      expect(result).toBeNull();

      const bakPath = `${filePath}.bak`;
      await expect(fs.access(bakPath)).resolves.toBeUndefined();
    });
  });

  describe("BoardConfigStore", () => {
    it("returns empty boards when file does not exist", async () => {
      const store = new BoardConfigStore(path.join(tmpDir, "boards.json"));
      const data = await store.load();
      expect(data).toEqual({ version: 1, boards: [] });
    });

    it("saves and loads a board config", async () => {
      const store = new BoardConfigStore(path.join(tmpDir, "boards.json"));
      const board: BoardConfig = {
        id: "board-1",
        repoPath: "/home/user/project",
        columns: BoardConfigStore.defaultColumns(),
        trackerType: "github",
        trackerConfig: { owner: "test", repo: "repo" },
        autonomyLevel: "moderate",
        pollIntervalMs: 60000,
        ciPollIntervalMs: 30000,
        maxIterations: 3,
        filterCriteria: {},
      };

      await store.saveBoard(board);
      const loaded = await store.getBoard("/home/user/project");
      expect(loaded).toEqual(board);
    });

    it("updates existing board by repoPath", async () => {
      const store = new BoardConfigStore(path.join(tmpDir, "boards.json"));
      const board: BoardConfig = {
        id: "board-1",
        repoPath: "/repo",
        columns: BoardConfigStore.defaultColumns(),
        trackerType: "github",
        trackerConfig: { owner: "o", repo: "r" },
        autonomyLevel: "moderate",
        pollIntervalMs: 60000,
        ciPollIntervalMs: 30000,
        maxIterations: 3,
        filterCriteria: {},
      };

      await store.saveBoard(board);
      await store.saveBoard({ ...board, autonomyLevel: "aggressive" });

      const data = await store.load();
      expect(data.boards).toHaveLength(1);
      expect(data.boards[0].autonomyLevel).toBe("aggressive");
    });

    it("deletes a board by repoPath", async () => {
      const store = new BoardConfigStore(path.join(tmpDir, "boards.json"));
      const board: BoardConfig = {
        id: "board-1",
        repoPath: "/repo",
        columns: [],
        trackerType: "local",
        trackerConfig: { owner: "o", repo: "r" },
        autonomyLevel: "moderate",
        pollIntervalMs: 60000,
        ciPollIntervalMs: 30000,
        maxIterations: 3,
        filterCriteria: {},
      };

      await store.saveBoard(board);
      await store.deleteBoard("/repo");

      const data = await store.load();
      expect(data.boards).toHaveLength(0);
    });

    it("provides default columns with expected names", () => {
      const cols = BoardConfigStore.defaultColumns();
      const names = cols.map((c) => c.name);
      expect(names).toEqual([
        "Backlog",
        "Ready",
        "In Progress",
        "In Review",
        "Done",
      ]);
    });

    it("migrates v0 data (no version field) to v1", async () => {
      const filePath = path.join(tmpDir, "boards.json");
      // Simulate v0: bare object without version
      await fs.writeFile(
        filePath,
        JSON.stringify({ boards: [{ id: "b1", repoPath: "/old" }] }),
        "utf-8",
      );

      const store = new BoardConfigStore(filePath);
      const data = await store.load();
      expect(data.version).toBe(1);
      expect(data.boards).toHaveLength(1);
      expect(data.boards[0].id).toBe("b1");
    });

    it("persists board fields, board attributes, and run defaults", async () => {
      const store = new BoardConfigStore(path.join(tmpDir, "boards.json"));
      const board: BoardConfig = {
        id: "board-fields",
        name: "Agent Board",
        repoPath: "/repo",
        columns: BoardConfigStore.defaultColumns(),
        trackerType: "local",
        trackerConfig: {},
        autonomyLevel: "moderate",
        pollIntervalMs: 60000,
        ciPollIntervalMs: 30000,
        maxIterations: 3,
        filterCriteria: {},
        fields: [
          {
            id: "estimate",
            name: "Estimate",
            type: "number",
            taskScoped: true,
            boardScoped: false,
            createdAt: 1000,
            updatedAt: 1000,
          },
        ],
        customFields: { team: "runtime" },
        runDefaults: {
          agentType: "codex",
          baseRef: "main",
          branchName: "kanban/task",
          model: "gpt-5.4",
          reasoningLevel: "high",
          worktreeMode: "new",
          mode: "plan",
          openChat: true,
        },
      };

      await store.saveBoard(board);

      const loaded = await store.getBoardById("board-fields");
      expect(loaded).toMatchObject({
        id: "board-fields",
        fields: board.fields,
        customFields: { team: "runtime" },
        runDefaults: board.runDefaults,
      });
    });
  });

  describe("RuleStore", () => {
    it("returns empty rules when file does not exist", async () => {
      const store = new RuleStore(path.join(tmpDir, "rules.json"));
      const rules = await store.getRules();
      expect(rules).toEqual([]);
    });

    it("saves and loads a rule", async () => {
      const store = new RuleStore(path.join(tmpDir, "rules.json"));
      const rule: AutomationRule = {
        id: "rule-1",
        name: "Auto-start",
        enabled: true,
        trigger: { type: "card_moved" },
        conditions: [{ type: "has_description", params: {} }],
        actions: [{ type: "spawn_agent", params: {} }],
        continueOnError: false,
      };

      await store.saveRule(rule);
      const rules = await store.getRules();
      expect(rules).toHaveLength(1);
      expect(rules[0]).toEqual(rule);
    });

    it("deletes a rule by id", async () => {
      const store = new RuleStore(path.join(tmpDir, "rules.json"));
      const rule: AutomationRule = {
        id: "rule-1",
        name: "Test",
        enabled: true,
        trigger: { type: "manual" },
        conditions: [],
        actions: [],
        continueOnError: false,
      };

      await store.saveRule(rule);
      await store.deleteRule("rule-1");

      const rules = await store.getRules();
      expect(rules).toHaveLength(0);
    });

    it("handles corrupted file gracefully", async () => {
      const filePath = path.join(tmpDir, "rules.json");
      await fs.writeFile(filePath, "corrupted!!!", "utf-8");

      const store = new RuleStore(filePath);
      const rules = await store.getRules();
      expect(rules).toEqual([]);

      // .bak should exist
      await expect(fs.access(`${filePath}.bak`)).resolves.toBeUndefined();
    });
  });

  describe("AnalyticsStore", () => {
    it("returns empty analytics when file does not exist", async () => {
      const store = new AnalyticsStore(path.join(tmpDir, "analytics.json"));
      const data = await store.load();
      expect(data).toEqual({ version: 1, analytics: [] });
    });

    it("saves and retrieves card analytics", async () => {
      const store = new AnalyticsStore(path.join(tmpDir, "analytics.json"));
      const analytics: CardAnalytics = {
        cardId: "card-1",
        externalId: "42",
        stateTransitions: [{ from: "backlog", to: "ready", timestamp: 1000 }],
        tokenUsage: [],
        iterationCount: 1,
        outcome: "in_progress",
      };

      await store.saveCardAnalytics(analytics);
      const loaded = await store.getCardAnalytics("card-1");
      expect(loaded).toEqual(analytics);
    });

    it("handles missing file gracefully", async () => {
      const store = new AnalyticsStore(path.join(tmpDir, "nonexistent", "a.json"));
      const data = await store.load();
      expect(data).toEqual({ version: 1, analytics: [] });
    });
  });

  describe("BoardStateStore", () => {
    it("returns empty cards when file does not exist", async () => {
      const store = new BoardStateStore(path.join(tmpDir, "state.json"));
      const cards = await store.getCards();
      expect(cards).toEqual([]);
    });

    it("saves and loads card positions", async () => {
      const store = new BoardStateStore(path.join(tmpDir, "state.json"));
      const cards: KanbanCard[] = [
        {
          id: "card-1",
          source: "github",
          externalId: "1",
          externalUrl: "https://github.com/o/r/issues/1",
          title: "Fix bug",
          description: "A bug fix",
          columnId: "in-progress",
          positionIndex: 0,
          priority: "high",
          labels: ["bug"],
          pipelineState: "agent_running",
          iterationCount: 0,
          createdAt: 1000,
          updatedAt: 2000,
        },
      ];

      await store.saveCards(cards);
      const loaded = await store.getCards();
      expect(loaded).toEqual(cards);
    });

    it("handles missing file gracefully", async () => {
      const store = new BoardStateStore(path.join(tmpDir, "nope.json"));
      const data = await store.load();
      expect(data).toEqual({ version: 1, cards: [] });
    });

    it("persists task custom fields, run links, and activity", async () => {
      const store = new BoardStateStore(path.join(tmpDir, "state.json"));
      const task: KanbanCard = {
        id: "task-1",
        boardId: "board-1",
        source: "local",
        externalId: "",
        externalUrl: "",
        title: "Implement kanban run sheet",
        description: "Configure agent before launch",
        columnId: "ready",
        positionIndex: 0,
        priority: "high",
        labels: ["agent"],
        pipelineState: "agent_running",
        iterationCount: 0,
        customFields: { estimate: 3 },
        runDefaults: { baseRef: "main", agentType: "codex", mode: "plan" },
        linkedTaskId: "orch-1",
        createdAt: 1000,
        updatedAt: 2000,
      };
      const run: KanbanTaskRun = {
        id: "run-1",
        taskId: "task-1",
        boardId: "board-1",
        orchestratorTaskId: "orch-1",
        status: "running",
        worktreePath: "/tmp/worktree",
        branch: "kanban/task-1",
        baseRef: "main",
        agentType: "codex",
        model: "gpt-5.4",
        reasoningLevel: "high",
        mode: "plan",
        createdAt: 3000,
        startedAt: 3100,
      };

      await store.saveTask(task);
      await store.saveTaskRun(run);
      await store.addTaskActivity({
        id: "activity-1",
        taskId: "task-1",
        boardId: "board-1",
        type: "run",
        message: "Started codex run orch-1",
        toValue: { orchestratorTaskId: "orch-1" },
        createdAt: 3200,
      });

      const loaded = await store.getTask("task-1");
      expect(loaded?.customFields).toEqual({ estimate: 3 });
      expect(loaded?.latestRun).toMatchObject({ id: "run-1", orchestratorTaskId: "orch-1" });
      expect(loaded?.runs).toHaveLength(1);
      expect(loaded?.activity?.[0]).toMatchObject({ id: "activity-1", message: "Started codex run orch-1" });

      await expect(store.getTaskByOrchestratorTask("orch-1")).resolves.toMatchObject({ id: "task-1" });
      await expect(store.getTaskRuns("task-1")).resolves.toHaveLength(1);

      const completed = await store.updateTaskRun("run-1", {
        status: "completed",
        completedAt: 4000,
        summary: "Done",
      });
      expect(completed).toMatchObject({ status: "completed", completedAt: 4000, summary: "Done" });
    });
  });

  describe("corruption recovery", () => {
    it("creates .bak file and returns defaults on corrupted board data", async () => {
      const filePath = path.join(tmpDir, "boards.json");
      const corruptedContent = '{"version": 1, "boards": "not-an-array"}';
      await fs.writeFile(filePath, corruptedContent, "utf-8");

      const store = new BoardConfigStore(filePath);
      const data = await store.load();

      // Should return defaults
      expect(data).toEqual({ version: 1, boards: [] });

      // .bak should contain the corrupted content
      const bakContent = await fs.readFile(`${filePath}.bak`, "utf-8");
      expect(bakContent).toBe(corruptedContent);
    });
  });
});
