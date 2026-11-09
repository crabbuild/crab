import { describe, it, expect } from "vitest";
import {
  sshEnhancedReducer,
  initialSshEnhancedState,
  SshEnhancedAction,
} from "../src/state/ssh-enhanced-state";
import type {
  TransferEntry,
  SyncPair,
  SyncConflict,
  ConnectionQuality,
  RemoteBookmark,
  RecentFile,
  AgentCapabilities,
  RemoteProcess,
} from "../electron/ssh-types";

function makeTransfer(overrides: Partial<TransferEntry> = {}): TransferEntry {
  return {
    id: "t1",
    profileId: "p1",
    direction: "upload",
    localPath: "/local/file.txt",
    remotePath: "/remote/file.txt",
    fileName: "file.txt",
    status: "active",
    bytesTransferred: 0,
    totalBytes: 1000,
    speed: 100,
    eta: 10,
    retryCount: 0,
    ...overrides,
  };
}

describe("sshEnhancedReducer", () => {
  describe("TRANSFER actions", () => {
    it("TRANSFER_ADD adds a transfer entry", () => {
      const entry = makeTransfer();
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry,
      });
      expect(state.transfers.entries.get("t1")).toEqual(entry);
    });

    it("TRANSFER_UPDATE updates an existing entry", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry: makeTransfer(),
      });
      state = sshEnhancedReducer(state, {
        type: "TRANSFER_UPDATE",
        id: "t1",
        updates: { bytesTransferred: 500 },
      });
      expect(state.transfers.entries.get("t1")?.bytesTransferred).toBe(500);
    });

    it("TRANSFER_UPDATE is a no-op for unknown ID", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_UPDATE",
        id: "unknown",
        updates: { bytesTransferred: 500 },
      });
      expect(state).toBe(initialSshEnhancedState);
    });

    it("TRANSFER_REMOVE removes entry and deselects it", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry: makeTransfer(),
      });
      state = sshEnhancedReducer(state, { type: "TRANSFER_SELECT", id: "t1" });
      state = sshEnhancedReducer(state, { type: "TRANSFER_REMOVE", id: "t1" });
      expect(state.transfers.entries.has("t1")).toBe(false);
      expect(state.transfers.selectedIds.has("t1")).toBe(false);
    });

    it("TRANSFER_REMOVE_BATCH removes multiple entries", () => {
      let state = initialSshEnhancedState;
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t1" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t2" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t3" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_REMOVE_BATCH", ids: ["t1", "t3"] });
      expect(state.transfers.entries.has("t1")).toBe(false);
      expect(state.transfers.entries.has("t2")).toBe(true);
      expect(state.transfers.entries.has("t3")).toBe(false);
    });

    it("TRANSFER_CLEAR_COMPLETED removes completed and failed entries", () => {
      let state = initialSshEnhancedState;
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t1", status: "active" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t2", status: "completed" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t3", status: "failed" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_ADD", entry: makeTransfer({ id: "t4", status: "queued" }) });
      state = sshEnhancedReducer(state, { type: "TRANSFER_CLEAR_COMPLETED" });
      expect(state.transfers.entries.has("t1")).toBe(true);
      expect(state.transfers.entries.has("t2")).toBe(false);
      expect(state.transfers.entries.has("t3")).toBe(false);
      expect(state.transfers.entries.has("t4")).toBe(true);
    });

    it("TRANSFER_SELECT and TRANSFER_DESELECT manage selection", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, { type: "TRANSFER_SELECT", id: "t1" });
      expect(state.transfers.selectedIds.has("t1")).toBe(true);
      state = sshEnhancedReducer(state, { type: "TRANSFER_DESELECT", id: "t1" });
      expect(state.transfers.selectedIds.has("t1")).toBe(false);
    });

    it("TRANSFER_SELECT_BATCH adds multiple selections", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SELECT_BATCH",
        ids: ["t1", "t2", "t3"],
      });
      expect(state.transfers.selectedIds.size).toBe(3);
    });

    it("TRANSFER_DESELECT_ALL clears selection", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SELECT_BATCH",
        ids: ["t1", "t2"],
      });
      state = sshEnhancedReducer(state, { type: "TRANSFER_DESELECT_ALL" });
      expect(state.transfers.selectedIds.size).toBe(0);
    });

    it("TRANSFER_SET_BANDWIDTH updates bandwidth limit", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SET_BANDWIDTH",
        limit: 1024,
      });
      expect(state.transfers.bandwidthLimit).toBe(1024);
    });

    it("TRANSFER_SET_THROUGHPUT updates actual throughput", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SET_THROUGHPUT",
        throughput: 512000,
      });
      expect(state.transfers.actualThroughput).toBe(512000);
    });

    it("TRANSFER_SET_COMPRESSION toggles compression", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SET_COMPRESSION",
        enabled: true,
      });
      expect(state.transfers.compressionEnabled).toBe(true);
    });

    it("TRANSFER_SET_ADAPTIVE toggles adaptive throttling", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_SET_ADAPTIVE",
        enabled: true,
      });
      expect(state.transfers.adaptiveThrottling).toBe(true);
    });

    it("TRANSFER_PROGRESS updates transfer progress fields", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry: makeTransfer(),
      });
      state = sshEnhancedReducer(state, {
        type: "TRANSFER_PROGRESS",
        id: "t1",
        bytesTransferred: 750,
        speed: 200,
        eta: 2,
      });
      const entry = state.transfers.entries.get("t1");
      expect(entry?.bytesTransferred).toBe(750);
      expect(entry?.speed).toBe(200);
      expect(entry?.eta).toBe(2);
    });

    it("TRANSFER_STATUS_CHANGED updates status and sets completedAt for completed", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry: makeTransfer(),
      });
      state = sshEnhancedReducer(state, {
        type: "TRANSFER_STATUS_CHANGED",
        id: "t1",
        status: "completed",
      });
      const entry = state.transfers.entries.get("t1");
      expect(entry?.status).toBe("completed");
      expect(entry?.completedAt).toBeGreaterThan(0);
    });

    it("TRANSFER_STATUS_CHANGED sets error on failure", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "TRANSFER_ADD",
        entry: makeTransfer(),
      });
      state = sshEnhancedReducer(state, {
        type: "TRANSFER_STATUS_CHANGED",
        id: "t1",
        status: "failed",
        error: "Connection lost",
      });
      const entry = state.transfers.entries.get("t1");
      expect(entry?.status).toBe("failed");
      expect(entry?.error).toBe("Connection lost");
    });
  });

  describe("SYNC actions", () => {
    const syncPair: SyncPair = {
      id: "sp1",
      profileId: "p1",
      localDir: "/local/dir",
      remoteDir: "/remote/dir",
      ignorePatterns: ["node_modules"],
      status: "active",
      pendingChanges: 0,
    };

    it("SYNC_SET_PAIR adds a sync pair", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "SYNC_SET_PAIR",
        pair: syncPair,
      });
      expect(state.syncPairs.pairs.get("sp1")).toEqual(syncPair);
    });

    it("SYNC_REMOVE_PAIR removes pair and its conflicts", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "SYNC_SET_PAIR",
        pair: syncPair,
      });
      const conflict: SyncConflict = {
        id: "c1",
        syncPairId: "sp1",
        filePath: "/file.txt",
        localMtime: 1000,
        remoteMtime: 2000,
        detectedAt: 3000,
      };
      state = sshEnhancedReducer(state, { type: "SYNC_ADD_CONFLICT", conflict });
      state = sshEnhancedReducer(state, { type: "SYNC_REMOVE_PAIR", id: "sp1" });
      expect(state.syncPairs.pairs.has("sp1")).toBe(false);
      expect(state.syncPairs.conflicts.has("c1")).toBe(false);
    });

    it("SYNC_UPDATE_PAIR updates pair fields", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "SYNC_SET_PAIR",
        pair: syncPair,
      });
      state = sshEnhancedReducer(state, {
        type: "SYNC_UPDATE_PAIR",
        id: "sp1",
        updates: { pendingChanges: 5, lastSyncAt: 12345 },
      });
      const pair = state.syncPairs.pairs.get("sp1");
      expect(pair?.pendingChanges).toBe(5);
      expect(pair?.lastSyncAt).toBe(12345);
    });

    it("SYNC_RESOLVE_CONFLICT marks conflict as resolved", () => {
      const conflict: SyncConflict = {
        id: "c1",
        syncPairId: "sp1",
        filePath: "/file.txt",
        localMtime: 1000,
        remoteMtime: 2000,
        detectedAt: 3000,
      };
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "SYNC_ADD_CONFLICT",
        conflict,
      });
      state = sshEnhancedReducer(state, {
        type: "SYNC_RESOLVE_CONFLICT",
        id: "c1",
        resolution: "local",
      });
      const resolved = state.syncPairs.conflicts.get("c1");
      expect(resolved?.resolution).toBe("local");
      expect(resolved?.resolvedAt).toBeGreaterThan(0);
    });
  });

  describe("QUALITY actions", () => {
    const quality: ConnectionQuality = {
      level: "green",
      latencyMs: 50,
      bandwidthMbps: 100,
      packetLossPercent: 0,
      measuredAt: Date.now(),
    };

    it("QUALITY_UPDATE sets quality for a profile", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "QUALITY_UPDATE",
        profileId: "p1",
        quality,
      });
      expect(state.quality.profiles.get("p1")).toEqual(quality);
    });

    it("QUALITY_REMOVE removes quality for a profile", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "QUALITY_UPDATE",
        profileId: "p1",
        quality,
      });
      state = sshEnhancedReducer(state, { type: "QUALITY_REMOVE", profileId: "p1" });
      expect(state.quality.profiles.has("p1")).toBe(false);
    });
  });

  describe("BOOKMARKS actions", () => {
    const bookmark: RemoteBookmark = {
      id: "b1",
      profileId: "p1",
      path: "/remote/project",
      label: "My Project",
      createdAt: Date.now(),
    };

    it("BOOKMARKS_SET replaces bookmarks for a profile", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "BOOKMARKS_SET",
        profileId: "p1",
        bookmarks: [bookmark],
      });
      expect(state.bookmarks.bookmarks.get("p1")).toEqual([bookmark]);
    });

    it("BOOKMARKS_ADD appends a bookmark", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "BOOKMARKS_ADD",
        bookmark,
      });
      expect(state.bookmarks.bookmarks.get("p1")).toEqual([bookmark]);
    });

    it("BOOKMARKS_REMOVE removes a specific bookmark", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "BOOKMARKS_SET",
        profileId: "p1",
        bookmarks: [bookmark, { ...bookmark, id: "b2", path: "/other" }],
      });
      state = sshEnhancedReducer(state, {
        type: "BOOKMARKS_REMOVE",
        profileId: "p1",
        bookmarkId: "b1",
      });
      expect(state.bookmarks.bookmarks.get("p1")?.length).toBe(1);
      expect(state.bookmarks.bookmarks.get("p1")?.[0].id).toBe("b2");
    });

    it("BOOKMARKS_ADD_RECENT maintains LRU with max 20 entries", () => {
      let state = initialSshEnhancedState;
      for (let i = 0; i < 21; i++) {
        const recent: RecentFile = {
          profileId: "p1",
          path: `/file${i}.txt`,
          fileName: `file${i}.txt`,
          lastAccessedAt: Date.now() + i,
        };
        state = sshEnhancedReducer(state, { type: "BOOKMARKS_ADD_RECENT", recent });
      }
      const recents = state.bookmarks.recents.get("p1");
      expect(recents?.length).toBe(20);
      // Most recent should be first
      expect(recents?.[0].path).toBe("/file20.txt");
    });

    it("BOOKMARKS_ADD_RECENT replaces existing path entry", () => {
      let state = initialSshEnhancedState;
      const recent1: RecentFile = { profileId: "p1", path: "/a.txt", fileName: "a.txt", lastAccessedAt: 100 };
      const recent2: RecentFile = { profileId: "p1", path: "/b.txt", fileName: "b.txt", lastAccessedAt: 200 };
      state = sshEnhancedReducer(state, { type: "BOOKMARKS_ADD_RECENT", recent: recent1 });
      state = sshEnhancedReducer(state, { type: "BOOKMARKS_ADD_RECENT", recent: recent2 });
      const recent1Updated: RecentFile = { profileId: "p1", path: "/a.txt", fileName: "a.txt", lastAccessedAt: 300 };
      state = sshEnhancedReducer(state, { type: "BOOKMARKS_ADD_RECENT", recent: recent1Updated });
      const recents = state.bookmarks.recents.get("p1");
      expect(recents?.length).toBe(2);
      expect(recents?.[0].path).toBe("/a.txt");
      expect(recents?.[0].lastAccessedAt).toBe(300);
    });
  });

  describe("CAPABILITIES actions", () => {
    const caps: AgentCapabilities = {
      protocol: 1,
      version: "2.0.0-alpha.1",
      methods: ["capabilities", "ping", "read_range"],
    };

    it("CAPABILITIES_SET stores capabilities for a profile", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "CAPABILITIES_SET",
        profileId: "p1",
        capabilities: caps,
      });
      expect(state.capabilities.profiles.get("p1")).toEqual(caps);
    });

    it("CAPABILITIES_REMOVE removes capabilities for a profile", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "CAPABILITIES_SET",
        profileId: "p1",
        capabilities: caps,
      });
      state = sshEnhancedReducer(state, { type: "CAPABILITIES_REMOVE", profileId: "p1" });
      expect(state.capabilities.profiles.has("p1")).toBe(false);
    });
  });

  describe("PROCESSES actions", () => {
    const processes: RemoteProcess[] = [
      { pid: 1, name: "node", cpuPercent: 5, memoryPercent: 10, memoryBytes: 100_000_000, user: "dev", runtime: 3600 },
      { pid: 2, name: "python", cpuPercent: 80, memoryPercent: 50, memoryBytes: 500_000_000, user: "dev", runtime: 120 },
    ];

    it("PROCESSES_SET stores process list for a profile", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "PROCESSES_SET",
        profileId: "p1",
        processes,
      });
      expect(state.processes.profiles.get("p1")).toEqual(processes);
    });

    it("PROCESSES_CLEAR removes process list for a profile", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "PROCESSES_SET",
        profileId: "p1",
        processes,
      });
      state = sshEnhancedReducer(state, { type: "PROCESSES_CLEAR", profileId: "p1" });
      expect(state.processes.profiles.has("p1")).toBe(false);
    });

    it("PROCESSES_SET_POLLING tracks polling state", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "PROCESSES_SET_POLLING",
        profileId: "p1",
        active: true,
      });
      expect(state.processes.polling.get("p1")).toBe(true);
      state = sshEnhancedReducer(state, {
        type: "PROCESSES_SET_POLLING",
        profileId: "p1",
        active: false,
      });
      expect(state.processes.polling.get("p1")).toBe(false);
    });
  });

  describe("WORKSPACE actions", () => {
    it("WORKSPACE_OPEN sets the open path for a profile", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "WORKSPACE_OPEN",
        profileId: "p1",
        remotePath: "/home/dev/project",
      });
      expect(state.workspace.openPaths.get("p1")).toBe("/home/dev/project");
    });

    it("WORKSPACE_CLOSE removes path and indexing state", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "WORKSPACE_OPEN",
        profileId: "p1",
        remotePath: "/home/dev/project",
      });
      state = sshEnhancedReducer(state, {
        type: "WORKSPACE_SET_INDEXING",
        profileId: "p1",
        indexing: true,
      });
      state = sshEnhancedReducer(state, { type: "WORKSPACE_CLOSE", profileId: "p1" });
      expect(state.workspace.openPaths.has("p1")).toBe(false);
      expect(state.workspace.indexing.has("p1")).toBe(false);
    });

    it("WORKSPACE_SET_INDEXING tracks indexing state", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "WORKSPACE_SET_INDEXING",
        profileId: "p1",
        indexing: true,
      });
      expect(state.workspace.indexing.get("p1")).toBe(true);
    });
  });

  describe("STREAM actions", () => {
    it("STREAM_START adds a stream entry", () => {
      const state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      expect(state.streams.entries.get("s1")?.active).toBe(true);
    });

    it("STREAM_DATA appends output to an active stream", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      state = sshEnhancedReducer(state, { type: "STREAM_DATA", streamId: "s1", data: "line 1" });
      state = sshEnhancedReducer(state, { type: "STREAM_DATA", streamId: "s1", data: "line 2" });
      expect(state.streams.entries.get("s1")?.output).toEqual(["line 1", "line 2"]);
    });

    it("STREAM_DATA is a no-op for inactive streams", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      state = sshEnhancedReducer(state, { type: "STREAM_END", streamId: "s1", exitCode: 0 });
      const before = state;
      state = sshEnhancedReducer(state, { type: "STREAM_DATA", streamId: "s1", data: "ignored" });
      expect(state).toBe(before);
    });

    it("STREAM_DATA trims output to 1000 lines", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      for (let i = 0; i < 1005; i++) {
        state = sshEnhancedReducer(state, { type: "STREAM_DATA", streamId: "s1", data: `line ${i}` });
      }
      const output = state.streams.entries.get("s1")?.output;
      expect(output?.length).toBe(1000);
      expect(output?.[0]).toBe("line 5");
    });

    it("STREAM_END marks stream as inactive with exit code", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      state = sshEnhancedReducer(state, { type: "STREAM_END", streamId: "s1", exitCode: 1, error: "failed" });
      const stream = state.streams.entries.get("s1");
      expect(stream?.active).toBe(false);
      expect(stream?.exitCode).toBe(1);
      expect(stream?.error).toBe("failed");
    });

    it("STREAM_REMOVE deletes a stream entry", () => {
      let state = sshEnhancedReducer(initialSshEnhancedState, {
        type: "STREAM_START",
        entry: { id: "s1", profileId: "p1", type: "exec", output: [], active: true },
      });
      state = sshEnhancedReducer(state, { type: "STREAM_REMOVE", streamId: "s1" });
      expect(state.streams.entries.has("s1")).toBe(false);
    });
  });

  describe("unknown action", () => {
    it("returns state unchanged for unknown action type", () => {
      const state = sshEnhancedReducer(
        initialSshEnhancedState,
        { type: "UNKNOWN_ACTION" } as unknown as SshEnhancedAction,
      );
      expect(state).toBe(initialSshEnhancedState);
    });
  });
});
