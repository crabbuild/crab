/**
 * Unit tests for the PersistedChatUIState extension on Task/PersistedTask.
 *
 * Verifies:
 * - Legacy agent-tasks.json records are ignored after the SQLite cutoff
 * - Writing then reading round-trips a sample state
 * - setTaskUIState and getTaskUIState work correctly
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { execFileSync } from "node:child_process";

import { Orchestrator, type PersistedChatUIState } from "../electron/orchestrator";

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

describe("Orchestrator UI State persistence", () => {
  let directory: string;
  let persistPath: string;
  let repoPath: string;
  let orchestrator: Orchestrator;
  const owners: Orchestrator[] = [];
  const openStore = (file = persistPath) => {
    const owner = new Orchestrator({ persistPath: file, timeoutMs: 5000 });
    owners.push(owner);
    return owner;
  };
  const createTask = (prompt: string) => orchestrator.createTask(prompt, "codex", repoPath, undefined, { worktreeMode: "primary" });

  beforeEach(() => {
    directory = fs.mkdtempSync(path.join(os.tmpdir(), "crab-orch-ui-state-test-"));
    persistPath = path.join(directory, "agent-tasks.sqlite");
    repoPath = path.join(directory, "repo");
    execFileSync("git", ["init", "--quiet", "--initial-branch=main", repoPath]);
    const hooks = path.join(directory, "hooks");
    fs.mkdirSync(hooks);
    execFileSync("git", ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
      "-c", "commit.gpgSign=false", "-c", `core.hooksPath=${hooks}`, "commit", "--quiet", "--allow-empty", "-m", "Initial"], { cwd: repoPath });
    orchestrator = openStore();
  });

  afterEach(async () => {
    await Promise.all(owners.splice(0).map(owner => owner.shutdown()));
    fs.rmSync(directory, { recursive: true, force: true });
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

      const orch2 = openStore(legacyJsonPath);
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
      const task = await createTask("Test task");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      const retrieved = orchestrator.getTaskUIState(task.id);
      expect(retrieved).toEqual(SAMPLE_UI_STATE);
    });

    it("does nothing when setting state on a non-existent task", async () => {
      // Should not throw
      await orchestrator.setTaskUIState("nonexistent", SAMPLE_UI_STATE);
    });

    it("persists uiState to disk after setTaskUIState", async () => {
      const task = await createTask("Test task");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      expect(fs.existsSync(persistPath)).toBe(true);
      expect(fs.existsSync(persistPath.replace(/\.sqlite$/, ".json"))).toBe(false);

      const orch2 = openStore();
      expect(orch2.getTask(task.id)?.uiState).toEqual(SAMPLE_UI_STATE);
    });
  });

  describe("round-trip persistence", () => {
    it("writing then reading round-trips a sample UI state", async () => {
      const task = await createTask("Round-trip test");

      await orchestrator.setTaskUIState(task.id, SAMPLE_UI_STATE);

      // Create a new orchestrator that loads from the same file
      const orch2 = openStore();
      const loaded = orch2.getTask(task.id);

      expect(loaded).toBeDefined();
      expect(loaded!.uiState).toEqual(SAMPLE_UI_STATE);
    });

    it("round-trips partial UI state (only some fields set)", async () => {
      const task = await createTask("Partial state");

      const partialState: PersistedChatUIState = {
        compactView: false,
        timelineRail: "off",
      };

      await orchestrator.setTaskUIState(task.id, partialState);

      const orch2 = openStore();
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
      const task = await createTask("No UI state");

      const orch2 = openStore();
      const loaded = orch2.getTask(task.id);

      expect(loaded).toBeDefined();
      expect(loaded!.prompt).toBe("No UI state");
      expect(loaded!.uiState).toBeUndefined();
    });

    it("round-trips sidebar hierarchy and pin metadata", async () => {
      const parent = await createTask("Parent task");
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

      const orch2 = openStore();
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
      const parent = await createTask("Parent");
      const otherParent = await createTask("Other parent");
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
