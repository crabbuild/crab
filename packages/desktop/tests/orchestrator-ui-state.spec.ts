/**
 * Unit tests for the PersistedChatUIState extension on Task/PersistedTask.
 *
 * Verifies:
 * - Legacy agent-tasks.json records are ignored after the SQLite cutoff
 * - Writing then reading round-trips a sample state
 * - setTaskUIState and getTaskUIState work correctly
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { Orchestrator, type PersistedChatUIState } from "../electron/orchestrator";

// ─── Mocks ───────────────────────────────────────────────────────────────────

vi.mock("../electron/orchestrator-worktree", () => ({
  createWorktree: vi.fn(async (_repoPath: string, taskId: string, _baseRef: string) => {
    return path.join(os.tmpdir(), `crab-agent-${taskId}`);
  }),
  removeWorktree: vi.fn(async () => {}),
  readTaskMetadata: vi.fn(() => undefined),
}));

vi.mock("../electron/orchestrator-adapters", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../electron/orchestrator-adapters")>();
  return {
    ...actual,
    createAdapter: vi.fn(() => ({
      name: "Mock Agent",
      binary: "mock-agent",
      spawn: vi.fn(),
      cancel: vi.fn(),
      iterate: vi.fn(),
      sendMessage: vi.fn(),
      respondApproval: vi.fn(),
      onEvent: vi.fn(() => () => {}),
      onExit: vi.fn(() => () => {}),
    })),
  };
});

vi.mock("node:child_process", () => ({
  execFile: vi.fn(),
}));

vi.mock("node:util", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:util")>();
  return {
    ...actual,
    promisify: vi.fn(() => {
      return async (...args: any[]) => {
        const cmdArgs = args[1] as string[];
        if (cmdArgs.includes("rev-parse") && cmdArgs.includes("HEAD")) {
          return { stdout: "abc123def456789\n", stderr: "" };
        }
        if (cmdArgs.includes("rev-parse") && cmdArgs.includes("--abbrev-ref")) {
          return { stdout: "main\n", stderr: "" };
        }
        return { stdout: "abc123def456789\n", stderr: "" };
      };
    }),
  };
});

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createTempPersistPath(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-orch-ui-state-test-"));
  return path.join(dir, "agent-tasks.sqlite");
}

function cleanupPath(filePath: string): void {
  try {
    const dir = path.dirname(filePath);
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup.
  }
}

const SAMPLE_UI_STATE: PersistedChatUIState = {
  cardCollapse: {
    "item-abc": { collapsed: true, lastToggledMs: 1700000000000 },
    "item-def": { collapsed: false, lastToggledMs: 1700000001000 },
  },
  groupCollapse: {
    "g:item-abc": { collapsed: true, lastToggledMs: 1700000002000 },
  },
  turnCollapse: {
    0: { collapsed: true, lastToggledMs: 1700000003000 },
    1: { collapsed: false, lastToggledMs: 1700000004000 },
  },
  compactView: true,
  timelineRail: "compact",
  vimNavigation: false,
};

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("Orchestrator UI State persistence", () => {
  let persistPath: string;
  let orchestrator: Orchestrator;

  beforeEach(() => {
    persistPath = createTempPersistPath();
    orchestrator = new Orchestrator({ persistPath, timeoutMs: 5000 });
  });

  afterEach(() => {
    cleanupPath(persistPath);
  });

  describe("SQLite-only cutoff", () => {
    it("does not load legacy JSON records", () => {
      const legacyData = {
        tasks: [
          {
            id: "task-legacy-1",
            prompt: "Fix a bug",
            agentType: "codex",
            status: "reviewing",
            branch: "agent/task-legacy-1",
            worktreePath: "/tmp/crab-agent-task-legacy-1",
            baseRef: "abc123",
            repoPath: "/tmp/repo",
            createdAt: 1700000000000,
            feedback: [],
            todos: [],
          },
        ],
      };
      const legacyJsonPath = persistPath.replace(/\.sqlite$/, ".json");

      const dir = path.dirname(legacyJsonPath);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(legacyJsonPath, JSON.stringify(legacyData, null, 2), "utf-8");

      const orch2 = new Orchestrator({ persistPath: legacyJsonPath, timeoutMs: 5000 });
      const task = orch2.getTask("task-legacy-1");

      expect(task).toBeUndefined();
    });
  });

  describe("setTaskUIState and getTaskUIState", () => {
    it("returns undefined for a non-existent task", () => {
      const result = orchestrator.getTaskUIState("nonexistent");
      expect(result).toBeUndefined();
    });

    it("sets and gets UI state on an existing task", async () => {
      const task = await orchestrator.createTask("Test task", "codex", "/tmp/repo", "main");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      const retrieved = orchestrator.getTaskUIState(task.id);
      expect(retrieved).toEqual(SAMPLE_UI_STATE);
    });

    it("does nothing when setting state on a non-existent task", async () => {
      // Should not throw
      await orchestrator.setTaskUIState("nonexistent", SAMPLE_UI_STATE);
    });

    it("persists uiState to disk after setTaskUIState", async () => {
      const task = await orchestrator.createTask("Test task", "codex", "/tmp/repo", "main");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      expect(fs.existsSync(persistPath)).toBe(true);
      expect(fs.existsSync(persistPath.replace(/\.sqlite$/, ".json"))).toBe(false);

      const orch2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      expect(orch2.getTask(task.id)?.uiState).toEqual(SAMPLE_UI_STATE);
    });
  });

  describe("round-trip persistence", () => {
    it("writing then reading round-trips a sample UI state", async () => {
      const task = await orchestrator.createTask("Round-trip test", "codex", "/tmp/repo", "main");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      // Create a new orchestrator that loads from the same file
      const orch2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const loaded = orch2.getTask(task.id);

      expect(loaded).toBeDefined();
      expect(loaded!.uiState).toEqual(SAMPLE_UI_STATE);
    });

    it("round-trips partial UI state (only some fields set)", async () => {
      const task = await orchestrator.createTask("Partial state", "codex", "/tmp/repo", "main");

      const partialState: PersistedChatUIState = {
        compactView: false,
        timelineRail: "off",
      };

      await orchestrator.setTaskUIState(task.id, partialState);

      const orch2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const loaded = orch2.getTask(task.id);

      expect(loaded!.uiState).toEqual(partialState);
      expect(loaded!.uiState!.cardCollapse).toBeUndefined();
      expect(loaded!.uiState!.groupCollapse).toBeUndefined();
      expect(loaded!.uiState!.turnCollapse).toBeUndefined();
      expect(loaded!.uiState!.vimNavigation).toBeUndefined();
    });
  });

  describe("SQLite storage", () => {
    it("keeps tasks loadable when uiState is unset", async () => {
      const task = await orchestrator.createTask("No UI state", "codex", "/tmp/repo", "main");

      const orch2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const loaded = orch2.getTask(task.id);

      expect(loaded).toBeDefined();
      expect(loaded!.prompt).toBe("No UI state");
      expect(loaded!.uiState).toBeUndefined();
    });

    it("round-trips sidebar hierarchy and pin metadata", async () => {
      const parent = await orchestrator.createTask("Parent task", "codex", "/tmp/repo", "main");
      await orchestrator.setTaskPinned(parent.id, true);
      await orchestrator.renameTask(parent.id, "Renamed parent");
      const child = orchestrator.createChildTaskForAgent(
        parent.id,
        "codex",
        "Child prompt",
        "reviewer",
        "agent-1",
      );
      await orchestrator.persistTasks();

      const orch2 = new Orchestrator({ persistPath, timeoutMs: 5000 });
      const loadedParent = orch2.getTask(parent.id);
      const loadedChild = orch2.getTask(child.id);

      expect(loadedParent).toMatchObject({
        title: "Renamed parent",
      });
      expect(loadedParent!.pinnedAt).toEqual(expect.any(Number));
      expect(loadedParent!.childTaskIds).toContain(child.id);
      expect(loadedChild).toMatchObject({
        parentTaskId: parent.id,
        rootTaskId: parent.id,
        spawnedByAgentId: "agent-1",
        spawnedRole: "reviewer",
        sidebarLabel: "reviewer sub-agent",
      });
      expect(loadedChild!.lastActivityAt).toEqual(expect.any(Number));
    });

    it("can clear and reassign task parents", async () => {
      const parent = await orchestrator.createTask("Parent", "codex", "/tmp/repo", "main");
      const otherParent = await orchestrator.createTask("Other parent", "codex", "/tmp/repo", "main");
      const child = orchestrator.createChildTaskForAgent(parent.id, "codex", "Child", "tests", "agent-2");
      await orchestrator.persistTasks();

      await orchestrator.setTaskParent(child.id, otherParent.id);
      expect(orchestrator.getTask(child.id)?.parentTaskId).toBe(otherParent.id);
      expect(orchestrator.getTask(child.id)?.rootTaskId).toBe(otherParent.id);

      await orchestrator.setTaskParent(child.id, undefined);
      expect(orchestrator.getTask(child.id)?.parentTaskId).toBeUndefined();
      expect(orchestrator.getTask(child.id)?.rootTaskId).toBeUndefined();
    });
  });
});
