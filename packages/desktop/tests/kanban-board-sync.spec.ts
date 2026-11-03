import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import {
  BoardSync,
  MIN_POLL_INTERVAL_MS,
  MAX_POLL_INTERVAL_MS,
  DEFAULT_POLL_INTERVAL_MS,
} from "../electron/kanban/board-sync";
import type { SyncChangeset, SyncConflict } from "../electron/kanban/board-sync";
import type { BoardConfig, KanbanCard, KanbanColumn } from "../electron/kanban/kanban-types";
import type { ExternalIssue, TrackerAdapter } from "../electron/kanban/tracker-adapters/types";
import type { BoardFilter, TrackerSource } from "../electron/kanban/kanban-types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn(), debug: vi.fn() },
}));

// ── Test Helpers ─────────────────────────────────────────────────

function createMockAdapter(issues: ExternalIssue[] = []): TrackerAdapter {
  return {
    source: "github" as TrackerSource,
    authenticate: vi.fn().mockResolvedValue(undefined),
    fetchIssues: vi.fn().mockResolvedValue(issues),
    transitionIssue: vi.fn().mockResolvedValue(undefined),
    addComment: vi.fn().mockResolvedValue(undefined),
    linkPr: vi.fn().mockResolvedValue(undefined),
  };
}

function createMockStateStore() {
  const cards: KanbanCard[] = [];
  return {
    load: vi.fn().mockResolvedValue({ version: 1, cards }),
    save: vi.fn().mockResolvedValue(undefined),
    getCards: vi.fn().mockResolvedValue(cards),
    saveCards: vi.fn().mockResolvedValue(undefined),
  };
}

const DEFAULT_COLUMNS: KanbanColumn[] = [
  { id: "backlog", name: "Backlog", trackerStatusMapping: ["backlog", "open"] },
  { id: "ready", name: "Ready", trackerStatusMapping: ["ready", "todo"] },
  { id: "in-progress", name: "In Progress", trackerStatusMapping: ["in progress", "in_progress"] },
  { id: "in-review", name: "In Review", trackerStatusMapping: ["in review", "review"] },
  { id: "done", name: "Done", trackerStatusMapping: ["done", "closed"] },
];

function createBoardConfig(overrides: Partial<BoardConfig> = {}): BoardConfig {
  return {
    id: "board-1",
    repoPath: "/test/repo",
    columns: DEFAULT_COLUMNS,
    trackerType: "github",
    trackerConfig: { owner: "test", repo: "repo" },
    autonomyLevel: "moderate",
    pollIntervalMs: 60000,
    ciPollIntervalMs: 30000,
    maxIterations: 3,
    filterCriteria: {},
    ...overrides,
  };
}

function createExternalIssue(overrides: Partial<ExternalIssue> = {}): ExternalIssue {
  return {
    id: "1",
    title: "Test Issue",
    description: "A test issue",
    status: "open",
    assignee: "user1",
    labels: ["bug"],
    priority: "medium",
    url: "https://github.com/test/repo/issues/1",
    updatedAt: Date.now(),
    ...overrides,
  };
}

function createCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-uuid-1",
    source: "github",
    externalId: "1",
    externalUrl: "https://github.com/test/repo/issues/1",
    title: "Test Issue",
    description: "A test issue",
    columnId: "backlog",
    positionIndex: 0,
    assignee: "user1",
    priority: "medium",
    labels: ["bug"],
    pipelineState: "idle",
    iterationCount: 0,
    createdAt: 1000,
    updatedAt: 2000,
    ...overrides,
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("BoardSync", () => {
  let boardSync: BoardSync;
  let adapter: TrackerAdapter;
  let stateStore: ReturnType<typeof createMockStateStore>;
  let config: BoardConfig;

  beforeEach(() => {
    vi.useFakeTimers();
    adapter = createMockAdapter();
    stateStore = createMockStateStore();
    config = createBoardConfig();
    boardSync = new BoardSync(config, adapter, stateStore as any);
  });

  afterEach(() => {
    boardSync.stop();
    vi.useRealTimers();
  });

  describe("polling interval bounds", () => {
    it("uses the configured poll interval", () => {
      const customConfig = createBoardConfig({ pollIntervalMs: 30000 });
      const sync = new BoardSync(customConfig, adapter, stateStore as any);

      sync.start();

      // Verify the timer was set (initial sync + interval)
      expect(vi.getTimerCount()).toBeGreaterThan(0);
      sync.stop();
    });

    it("clamps interval to minimum of 10 seconds", async () => {
      const customConfig = createBoardConfig({ pollIntervalMs: 1000 });
      const sync = new BoardSync(customConfig, adapter, stateStore as any);

      sync.start();

      // Let the initial sync promise resolve
      await vi.advanceTimersByTimeAsync(0);

      // Advance by 9 seconds — should not have triggered a second sync
      await vi.advanceTimersByTimeAsync(9000);
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1); // Only initial sync

      // Advance to 10 seconds — should trigger
      await vi.advanceTimersByTimeAsync(1000);
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(2);

      sync.stop();
    });

    it("clamps interval to maximum of 3600 seconds", async () => {
      const customConfig = createBoardConfig({ pollIntervalMs: 5_000_000 });
      const sync = new BoardSync(customConfig, adapter, stateStore as any);

      sync.start();

      // Let the initial sync promise resolve
      await vi.advanceTimersByTimeAsync(0);

      // Advance by 3600 seconds — should trigger second sync
      await vi.advanceTimersByTimeAsync(3_600_000);
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(2);

      sync.stop();
    });

    it("uses minimum when pollIntervalMs is 0 or falsy", async () => {
      const customConfig = createBoardConfig({ pollIntervalMs: 0 });
      const sync = new BoardSync(customConfig, adapter, stateStore as any);

      sync.start();

      // Let the initial sync promise resolve
      await vi.advanceTimersByTimeAsync(0);

      // Should use minimum (10s) since 0 is below minimum
      await vi.advanceTimersByTimeAsync(10000);
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(2);

      sync.stop();
    });
  });

  describe("diff logic", () => {
    it("detects new issues and creates cards", () => {
      const issues = [
        createExternalIssue({ id: "1", title: "New Issue", status: "open" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.created).toHaveLength(1);
      expect(changeset.created[0].externalId).toBe("1");
      expect(changeset.created[0].title).toBe("New Issue");
      expect(changeset.created[0].columnId).toBe("backlog");
    });

    it("maps issue status to correct column via trackerStatusMapping", () => {
      const issues = [
        createExternalIssue({ id: "1", status: "in progress" }),
        createExternalIssue({ id: "2", status: "closed" }),
        createExternalIssue({ id: "3", status: "review" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.created[0].columnId).toBe("in-progress");
      expect(changeset.created[1].columnId).toBe("done");
      expect(changeset.created[2].columnId).toBe("in-review");
    });

    it("falls back to first column when status has no mapping", () => {
      const issues = [
        createExternalIssue({ id: "1", status: "unknown_status" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.created[0].columnId).toBe("backlog");
    });

    it("detects changed issues and updates cards", () => {
      // Set up existing card state
      const existingCard = createCard({
        externalId: "1",
        title: "Old Title",
        columnId: "backlog",
      });
      (boardSync as any).cards = [existingCard];

      const issues = [
        createExternalIssue({ id: "1", title: "New Title", status: "open" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.updated).toHaveLength(1);
      expect(changeset.updated[0].title).toBe("New Title");
    });

    it("detects status changes and moves card to mapped column", () => {
      const existingCard = createCard({
        externalId: "1",
        columnId: "backlog",
      });
      (boardSync as any).cards = [existingCard];

      const issues = [
        createExternalIssue({ id: "1", status: "in progress" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.updated).toHaveLength(1);
      expect(changeset.updated[0].columnId).toBe("in-progress");
    });

    it("detects deleted issues and archives cards", () => {
      const existingCard = createCard({
        externalId: "1",
        pipelineState: "idle",
      });
      (boardSync as any).cards = [existingCard];

      // Empty issue list — issue was deleted
      const changeset = boardSync.diff([]);

      expect(changeset.archived).toHaveLength(1);
      expect(changeset.archived[0].archivedAt).toBeDefined();
    });

    it("skips archiving when card has active pipeline state", () => {
      const existingCard = createCard({
        externalId: "1",
        pipelineState: "agent_running",
      });
      (boardSync as any).cards = [existingCard];

      const changeset = boardSync.diff([]);

      expect(changeset.archived).toHaveLength(0);
    });

    it("skips archiving for all pipeline-active states", () => {
      const activeStates: Array<KanbanCard["pipelineState"]> = [
        "agent_running",
        "pr_created",
        "ci_pending",
        "ci_passed",
        "review_pending",
        "review_approved",
        "merging",
      ];

      for (const state of activeStates) {
        const card = createCard({ externalId: "1", pipelineState: state });
        (boardSync as any).cards = [card];

        const changeset = boardSync.diff([]);
        expect(changeset.archived).toHaveLength(0);
      }
    });

    it("archives cards with non-active pipeline states", () => {
      const inactiveStates: Array<KanbanCard["pipelineState"]> = [
        "idle",
        "done",
        "blocked",
        "ci_failed",
        "review_changes_requested",
      ];

      for (const state of inactiveStates) {
        const card = createCard({
          id: `card-${state}`,
          externalId: "1",
          pipelineState: state,
        });
        (boardSync as any).cards = [card];

        const changeset = boardSync.diff([]);
        expect(changeset.archived).toHaveLength(1);
      }
    });

    it("does not report unchanged issues as updates", () => {
      const existingCard = createCard({
        externalId: "1",
        title: "Test Issue",
        description: "A test issue",
        assignee: "user1",
        labels: ["bug"],
        priority: "medium",
        columnId: "backlog",
      });
      (boardSync as any).cards = [existingCard];

      const issues = [
        createExternalIssue({
          id: "1",
          title: "Test Issue",
          description: "A test issue",
          assignee: "user1",
          labels: ["bug"],
          priority: "medium",
          status: "open",
        }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.updated).toHaveLength(0);
      expect(changeset.created).toHaveLength(0);
      expect(changeset.archived).toHaveLength(0);
    });

    it("ignores already-archived cards when detecting deletions", () => {
      const archivedCard = createCard({
        externalId: "1",
        archivedAt: 5000,
      });
      (boardSync as any).cards = [archivedCard];

      const changeset = boardSync.diff([]);

      expect(changeset.archived).toHaveLength(0);
    });
  });

  describe("conflict resolution", () => {
    it("resolves conflicts by preferring remote state", () => {
      const existingCard = createCard({
        externalId: "1",
        columnId: "in-progress",
      });
      (boardSync as any).cards = [existingCard];

      const issues = [
        createExternalIssue({ id: "1", status: "done" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.updated).toHaveLength(1);
      expect(changeset.updated[0].columnId).toBe("done");
    });

    it("logs conflict details with cardId, localState, remoteState, and timestamp", () => {
      const existingCard = createCard({
        id: "card-conflict",
        externalId: "1",
        columnId: "in-progress",
      });
      (boardSync as any).cards = [existingCard];

      const issues = [
        createExternalIssue({ id: "1", status: "done" }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.conflicts).toHaveLength(1);
      expect(changeset.conflicts[0]).toMatchObject({
        cardId: "card-conflict",
        localState: "in-progress",
        remoteState: "done",
      });
      expect(changeset.conflicts[0].timestamp).toBeGreaterThan(0);
    });

    it("accumulates conflicts in the conflict log", () => {
      const card1 = createCard({ id: "c1", externalId: "1", columnId: "backlog" });
      const card2 = createCard({ id: "c2", externalId: "2", columnId: "ready" });
      (boardSync as any).cards = [card1, card2];

      const issues = [
        createExternalIssue({ id: "1", status: "done" }),
        createExternalIssue({ id: "2", status: "in progress" }),
      ];

      boardSync.diff(issues);

      const log = boardSync.getConflictLog();
      expect(log).toHaveLength(2);
    });

    it("getConflictLog returns a copy (not a reference)", () => {
      const card = createCard({ id: "c1", externalId: "1", columnId: "backlog" });
      (boardSync as any).cards = [card];

      boardSync.diff([createExternalIssue({ id: "1", status: "done" })]);

      const log1 = boardSync.getConflictLog();
      const log2 = boardSync.getConflictLog();
      expect(log1).not.toBe(log2);
      expect(log1).toEqual(log2);
    });
  });

  describe("retry behavior", () => {
    it("retries up to 3 times on fetch failure with exponential backoff", async () => {
      const failingAdapter = createMockAdapter();
      (failingAdapter.fetchIssues as ReturnType<typeof vi.fn>)
        .mockRejectedValueOnce(new Error("Network error"))
        .mockRejectedValueOnce(new Error("Network error"))
        .mockRejectedValueOnce(new Error("Network error"));

      const sync = new BoardSync(config, failingAdapter, stateStore as any);

      const syncPromise = sync.forceSync();

      // Advance through backoff delays: 5s + 10s
      await vi.advanceTimersByTimeAsync(5000);
      await vi.advanceTimersByTimeAsync(10000);

      await syncPromise;

      expect(failingAdapter.fetchIssues).toHaveBeenCalledTimes(3);
    });

    it("succeeds on second attempt after first failure", async () => {
      const issues = [createExternalIssue({ id: "1" })];
      const retryAdapter = createMockAdapter();
      (retryAdapter.fetchIssues as ReturnType<typeof vi.fn>)
        .mockRejectedValueOnce(new Error("Temporary failure"))
        .mockResolvedValueOnce(issues);

      const sync = new BoardSync(config, retryAdapter, stateStore as any);

      const syncPromise = sync.forceSync();

      // Advance through first backoff delay: 5s
      await vi.advanceTimersByTimeAsync(5000);

      await syncPromise;

      expect(retryAdapter.fetchIssues).toHaveBeenCalledTimes(2);
      expect(sync.getCards()).toHaveLength(1);
    });

    it("emits sync:error event after all retries exhausted", async () => {
      const failingAdapter = createMockAdapter();
      (failingAdapter.fetchIssues as ReturnType<typeof vi.fn>)
        .mockRejectedValue(new Error("Persistent failure"));

      const sync = new BoardSync(config, failingAdapter, stateStore as any);
      const errorHandler = vi.fn();
      sync.on("sync:error", errorHandler);

      const syncPromise = sync.forceSync();

      // Advance through backoff delays: 5s + 10s
      await vi.advanceTimersByTimeAsync(5000);
      await vi.advanceTimersByTimeAsync(10000);

      await syncPromise;

      expect(errorHandler).toHaveBeenCalledWith({
        reason: "Persistent failure",
        attempts: 3,
      });
    });

    it("applies exponential backoff delays between retries", async () => {
      const failingAdapter = createMockAdapter();
      (failingAdapter.fetchIssues as ReturnType<typeof vi.fn>)
        .mockRejectedValueOnce(new Error("fail 1"))
        .mockRejectedValueOnce(new Error("fail 2"))
        .mockResolvedValueOnce([]);

      const sync = new BoardSync(config, failingAdapter, stateStore as any);

      const syncPromise = sync.forceSync();

      // First retry after 5s
      await vi.advanceTimersByTimeAsync(5000);
      // Second retry after 10s
      await vi.advanceTimersByTimeAsync(10000);

      await syncPromise;

      expect(failingAdapter.fetchIssues).toHaveBeenCalledTimes(3);
    });
  });

  describe("forceSync", () => {
    it("runs a sync cycle immediately", async () => {
      const issues = [createExternalIssue({ id: "1" })];
      (adapter.fetchIssues as ReturnType<typeof vi.fn>).mockResolvedValue(issues);

      await boardSync.forceSync();

      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);
      expect(boardSync.getCards()).toHaveLength(1);
    });

    it("persists cards to state store after sync", async () => {
      const issues = [createExternalIssue({ id: "1" })];
      (adapter.fetchIssues as ReturnType<typeof vi.fn>).mockResolvedValue(issues);

      await boardSync.forceSync();

      expect(stateStore.saveCards).toHaveBeenCalledTimes(1);
      expect(stateStore.saveCards).toHaveBeenCalledWith(
        expect.arrayContaining([
          expect.objectContaining({ externalId: "1" }),
        ]),
      );
    });

    it("emits board:updated event with changeset", async () => {
      const issues = [createExternalIssue({ id: "1" })];
      (adapter.fetchIssues as ReturnType<typeof vi.fn>).mockResolvedValue(issues);

      const handler = vi.fn();
      boardSync.on("board:updated", handler);

      await boardSync.forceSync();

      expect(handler).toHaveBeenCalledTimes(1);
      const changeset: SyncChangeset = handler.mock.calls[0][0];
      expect(changeset.created).toHaveLength(1);
    });
  });

  describe("column mapping", () => {
    it("maps status case-insensitively", () => {
      const issues = [
        createExternalIssue({ id: "1", status: "IN PROGRESS" }),
        createExternalIssue({ id: "2", status: "In Progress" }),
        createExternalIssue({ id: "3", status: "in progress" }),
      ];

      const changeset = boardSync.diff(issues);

      for (const card of changeset.created) {
        expect(card.columnId).toBe("in-progress");
      }
    });

    it("trims whitespace from status before mapping", () => {
      const issues = [
        createExternalIssue({ id: "1", status: "  open  " }),
      ];

      const changeset = boardSync.diff(issues);

      expect(changeset.created[0].columnId).toBe("backlog");
    });
  });

  describe("priority mapping", () => {
    it("maps known priority strings to CardPriority values", () => {
      const priorities = [
        { input: "critical", expected: "critical" },
        { input: "highest", expected: "critical" },
        { input: "blocker", expected: "critical" },
        { input: "high", expected: "high" },
        { input: "medium", expected: "medium" },
        { input: "normal", expected: "medium" },
        { input: "low", expected: "low" },
        { input: "unknown", expected: "none" },
        { input: "", expected: "none" },
      ];

      for (const { input, expected } of priorities) {
        const issues = [createExternalIssue({ id: input || "empty", priority: input })];
        (boardSync as any).cards = [];
        const changeset = boardSync.diff(issues);
        expect(changeset.created[0].priority).toBe(expected);
      }
    });
  });

  describe("start and stop", () => {
    it("starts polling and runs initial sync", () => {
      boardSync.start();

      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);
    });

    it("does not start multiple polling loops on repeated start calls", () => {
      boardSync.start();
      boardSync.start();
      boardSync.start();

      // Only one initial sync
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);
    });

    it("stops polling on stop()", () => {
      boardSync.start();

      // Initial sync
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);

      boardSync.stop();

      // Advance past poll interval — should not trigger another sync
      vi.advanceTimersByTime(120000);
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);
    });
  });

  describe("concurrent sync prevention", () => {
    it("does not run overlapping sync cycles", async () => {
      let resolveFirst: () => void;
      const firstCallPromise = new Promise<ExternalIssue[]>((resolve) => {
        resolveFirst = () => resolve([]);
      });

      (adapter.fetchIssues as ReturnType<typeof vi.fn>)
        .mockReturnValueOnce(firstCallPromise)
        .mockResolvedValue([]);

      // Start first sync
      const firstSync = boardSync.forceSync();

      // Try to start second sync while first is in progress
      const secondSync = boardSync.forceSync();

      // Resolve first
      resolveFirst!();
      await firstSync;
      await secondSync;

      // Only one fetch should have been made
      expect(adapter.fetchIssues).toHaveBeenCalledTimes(1);
    });
  });
});
