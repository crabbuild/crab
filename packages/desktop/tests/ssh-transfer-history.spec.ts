/**
 * Tests for TransferHistoryStore — SQLite-backed transfer history,
 * statistics, and resume state persistence.
 *
 * Verifies:
 * - record() inserts transfer records
 * - query() supports pagination, search, and sort
 * - getStats() computes aggregate statistics by time period
 * - saveResumeState() persists resume state
 * - getResumeState() retrieves resume state by profileId and remotePath
 * - purgeExpired() removes old history (>90 days) and expired resume states (>7 days)
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import Database from "better-sqlite3";
import { TransferHistoryStore } from "../electron/ssh-transfer-history";
import type { TransferRecord, TransferResumeState } from "../electron/ssh-types";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createInMemoryDb(): Database.Database {
  const db = new Database(":memory:");
  db.pragma("journal_mode = WAL");
  db.pragma("foreign_keys = ON");

  db.exec(`
    CREATE TABLE transfer_history (
      id TEXT PRIMARY KEY,
      profile_id TEXT NOT NULL,
      direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
      local_path TEXT NOT NULL,
      remote_path TEXT NOT NULL,
      file_size INTEGER NOT NULL,
      compressed_size INTEGER,
      duration_ms INTEGER NOT NULL,
      average_speed REAL NOT NULL,
      status TEXT NOT NULL CHECK (status IN ('completed', 'failed')),
      error TEXT,
      retry_count INTEGER NOT NULL DEFAULT 0,
      cipher_suite TEXT NOT NULL,
      mac_algorithm TEXT,
      kex_method TEXT,
      delta_savings_percent REAL,
      timestamp INTEGER NOT NULL
    );

    CREATE INDEX idx_transfer_history_profile_ts
      ON transfer_history(profile_id, timestamp DESC);

    CREATE INDEX idx_transfer_history_search
      ON transfer_history(local_path, remote_path);

    CREATE TABLE transfer_resume_state (
      id TEXT PRIMARY KEY,
      profile_id TEXT NOT NULL,
      direction TEXT NOT NULL CHECK (direction IN ('upload', 'download')),
      local_path TEXT NOT NULL,
      remote_path TEXT NOT NULL,
      bytes_transferred INTEGER NOT NULL,
      total_bytes INTEGER NOT NULL,
      remote_mtime INTEGER NOT NULL,
      remote_size INTEGER NOT NULL,
      partial_checksum TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL
    );

    CREATE INDEX idx_resume_state_lookup
      ON transfer_resume_state(profile_id, remote_path);
  `);

  return db;
}

function makeRecord(overrides: Partial<TransferRecord> = {}): TransferRecord {
  return {
    id: `transfer-${Math.random().toString(36).slice(2)}`,
    profileId: "profile-1",
    direction: "upload",
    localPath: "/local/file.bin",
    remotePath: "/remote/file.bin",
    fileSize: 1024000,
    duration: 5000,
    averageSpeed: 204800,
    status: "completed",
    retryCount: 0,
    cipherSuite: "aes256-gcm@openssh.com",
    timestamp: Date.now(),
    ...overrides,
  };
}

function makeResumeState(overrides: Partial<TransferResumeState> = {}): TransferResumeState {
  const now = Date.now();
  return {
    transferId: `resume-${Math.random().toString(36).slice(2)}`,
    profileId: "profile-1",
    direction: "upload",
    localPath: "/local/bigfile.bin",
    remotePath: "/remote/bigfile.bin",
    bytesTransferred: 512000,
    totalBytes: 1024000,
    remoteMtime: now - 60000,
    remoteSize: 1024000,
    partialChecksum: "abc123def456",
    createdAt: now,
    expiresAt: now + 7 * 86_400_000,
    ...overrides,
  };
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("TransferHistoryStore", () => {
  let db: Database.Database;
  let store: TransferHistoryStore;

  beforeEach(() => {
    db = createInMemoryDb();
    store = new TransferHistoryStore(db);
  });

  afterEach(() => {
    db.close();
  });

  describe("record", () => {
    it("inserts a transfer record into the database", () => {
      const record = makeRecord({ id: "t-001" });
      store.record(record);

      const row = db.prepare("SELECT * FROM transfer_history WHERE id = ?").get("t-001") as any;
      expect(row).toBeDefined();
      expect(row.profile_id).toBe("profile-1");
      expect(row.direction).toBe("upload");
      expect(row.file_size).toBe(1024000);
      expect(row.cipher_suite).toBe("aes256-gcm@openssh.com");
    });

    it("stores optional fields as null when not provided", () => {
      const record = makeRecord({ id: "t-002", compressedSize: undefined, error: undefined });
      store.record(record);

      const row = db.prepare("SELECT * FROM transfer_history WHERE id = ?").get("t-002") as any;
      expect(row.compressed_size).toBeNull();
      expect(row.error).toBeNull();
      expect(row.delta_savings_percent).toBeNull();
    });

    it("stores optional fields when provided", () => {
      const record = makeRecord({
        id: "t-003",
        compressedSize: 800000,
        error: "timeout",
        deltaSavingsPercent: 22.5,
        status: "failed",
      });
      store.record(record);

      const row = db.prepare("SELECT * FROM transfer_history WHERE id = ?").get("t-003") as any;
      expect(row.compressed_size).toBe(800000);
      expect(row.error).toBe("timeout");
      expect(row.delta_savings_percent).toBe(22.5);
      expect(row.status).toBe("failed");
    });
  });

  describe("query", () => {
    beforeEach(() => {
      // Insert 10 records with varying timestamps.
      for (let i = 0; i < 10; i++) {
        store.record(
          makeRecord({
            id: `q-${i.toString().padStart(3, "0")}`,
            localPath: `/local/file-${i}.bin`,
            remotePath: `/remote/host-${i % 2 === 0 ? "alpha" : "beta"}/file-${i}.bin`,
            fileSize: (i + 1) * 1000,
            duration: (10 - i) * 100,
            averageSpeed: (i + 1) * 500,
            timestamp: 1700000000000 + i * 1000,
          }),
        );
      }
    });

    it("returns paginated results", () => {
      const result = store.query({ page: 1, pageSize: 3 });
      expect(result.entries).toHaveLength(3);
      expect(result.total).toBe(10);
    });

    it("returns correct page offset", () => {
      const page1 = store.query({ page: 1, pageSize: 4 });
      const page2 = store.query({ page: 2, pageSize: 4 });
      const page3 = store.query({ page: 3, pageSize: 4 });

      expect(page1.entries).toHaveLength(4);
      expect(page2.entries).toHaveLength(4);
      expect(page3.entries).toHaveLength(2);

      // No overlap between pages.
      const allIds = [...page1.entries, ...page2.entries, ...page3.entries].map((e) => e.id);
      expect(new Set(allIds).size).toBe(10);
    });

    it("sorts by timestamp descending by default", () => {
      const result = store.query({ page: 1, pageSize: 10 });
      const timestamps = result.entries.map((e) => e.timestamp);
      expect(timestamps).toEqual([...timestamps].sort((a, b) => b - a));
    });

    it("sorts by file_size ascending", () => {
      const result = store.query({ page: 1, pageSize: 10, sortBy: "file_size", sortDir: "asc" });
      const sizes = result.entries.map((e) => e.fileSize);
      expect(sizes).toEqual([...sizes].sort((a, b) => a - b));
    });

    it("sorts by duration descending", () => {
      const result = store.query({ page: 1, pageSize: 10, sortBy: "duration", sortDir: "desc" });
      const durations = result.entries.map((e) => e.duration);
      expect(durations).toEqual([...durations].sort((a, b) => b - a));
    });

    it("sorts by average_speed ascending", () => {
      const result = store.query({ page: 1, pageSize: 10, sortBy: "average_speed", sortDir: "asc" });
      const speeds = result.entries.map((e) => e.averageSpeed);
      expect(speeds).toEqual([...speeds].sort((a, b) => a - b));
    });

    it("filters by search term in local_path", () => {
      const result = store.query({ page: 1, pageSize: 50, search: "file-3" });
      expect(result.total).toBe(1);
      expect(result.entries[0].localPath).toContain("file-3");
    });

    it("filters by search term in remote_path (host name)", () => {
      const result = store.query({ page: 1, pageSize: 50, search: "host-alpha" });
      expect(result.total).toBe(5); // indices 0, 2, 4, 6, 8
    });

    it("filters by profileId", () => {
      store.record(makeRecord({ id: "other-profile", profileId: "profile-2", timestamp: Date.now() }));
      const result = store.query({ page: 1, pageSize: 50, profileId: "profile-2" });
      expect(result.total).toBe(1);
      expect(result.entries[0].profileId).toBe("profile-2");
    });

    it("combines search and profileId filters", () => {
      store.record(
        makeRecord({
          id: "combo-1",
          profileId: "profile-2",
          localPath: "/local/special.bin",
          timestamp: Date.now(),
        }),
      );
      const result = store.query({ page: 1, pageSize: 50, profileId: "profile-2", search: "special" });
      expect(result.total).toBe(1);
      expect(result.entries[0].id).toBe("combo-1");
    });

    it("returns empty result for non-matching search", () => {
      const result = store.query({ page: 1, pageSize: 50, search: "nonexistent" });
      expect(result.total).toBe(0);
      expect(result.entries).toHaveLength(0);
    });
  });

  describe("getStats", () => {
    it("computes aggregate statistics for a day period", () => {
      const now = Date.now();
      store.record(makeRecord({ id: "s-1", direction: "upload", fileSize: 5000, averageSpeed: 1000, status: "completed", timestamp: now - 3600000 }));
      store.record(makeRecord({ id: "s-2", direction: "download", fileSize: 3000, averageSpeed: 2000, status: "completed", timestamp: now - 7200000 }));
      store.record(makeRecord({ id: "s-3", direction: "upload", fileSize: 2000, averageSpeed: 1500, status: "failed", timestamp: now - 10800000 }));

      const stats = store.getStats("profile-1", "day");
      expect(stats.totalUploaded).toBe(7000); // 5000 + 2000
      expect(stats.totalDownloaded).toBe(3000);
      expect(stats.averageSpeed).toBeCloseTo(1500, 0); // (1000 + 2000 + 1500) / 3
      expect(stats.successCount).toBe(2);
      expect(stats.failureCount).toBe(1);
      expect(stats.failureRate).toBeCloseTo(33.33, 0);
    });

    it("excludes records outside the time period", () => {
      const now = Date.now();
      const twoDaysAgo = now - 2 * 86_400_000;
      store.record(makeRecord({ id: "old-1", fileSize: 9999, timestamp: twoDaysAgo }));
      store.record(makeRecord({ id: "new-1", fileSize: 1000, timestamp: now - 1000 }));

      const stats = store.getStats("profile-1", "day");
      expect(stats.totalUploaded).toBe(1000);
      expect(stats.successCount).toBe(1);
    });

    it("returns zeros when no records exist", () => {
      const stats = store.getStats("profile-1", "month");
      expect(stats.totalUploaded).toBe(0);
      expect(stats.totalDownloaded).toBe(0);
      expect(stats.averageSpeed).toBe(0);
      expect(stats.successCount).toBe(0);
      expect(stats.failureCount).toBe(0);
      expect(stats.failureRate).toBe(0);
    });

    it("filters by profileId", () => {
      const now = Date.now();
      store.record(makeRecord({ id: "p1-1", profileId: "profile-1", fileSize: 1000, timestamp: now }));
      store.record(makeRecord({ id: "p2-1", profileId: "profile-2", fileSize: 5000, timestamp: now }));

      const stats = store.getStats("profile-1", "week");
      expect(stats.totalUploaded).toBe(1000);
    });
  });

  describe("saveResumeState / getResumeState", () => {
    it("persists and retrieves resume state", () => {
      const state = makeResumeState({ transferId: "r-001", remotePath: "/remote/big.bin" });
      store.saveResumeState(state);

      const retrieved = store.getResumeState("profile-1", "/remote/big.bin");
      expect(retrieved).not.toBeNull();
      expect(retrieved!.transferId).toBe("r-001");
      expect(retrieved!.bytesTransferred).toBe(512000);
      expect(retrieved!.totalBytes).toBe(1024000);
      expect(retrieved!.partialChecksum).toBe("abc123def456");
    });

    it("returns null when no resume state exists", () => {
      const result = store.getResumeState("profile-1", "/nonexistent/path");
      expect(result).toBeNull();
    });

    it("overwrites existing resume state for same transfer", () => {
      const state1 = makeResumeState({ transferId: "r-002", bytesTransferred: 100 });
      store.saveResumeState(state1);

      const state2 = makeResumeState({ transferId: "r-002", bytesTransferred: 500 });
      store.saveResumeState(state2);

      const retrieved = store.getResumeState("profile-1", "/remote/bigfile.bin");
      expect(retrieved!.bytesTransferred).toBe(500);
    });

    it("returns the most recent resume state when multiple exist for same profile+path", () => {
      const now = Date.now();
      store.saveResumeState(makeResumeState({ transferId: "r-old", createdAt: now - 10000 }));
      store.saveResumeState(makeResumeState({ transferId: "r-new", createdAt: now }));

      const retrieved = store.getResumeState("profile-1", "/remote/bigfile.bin");
      expect(retrieved!.transferId).toBe("r-new");
    });
  });

  describe("purgeExpired", () => {
    it("removes history entries older than 90 days", () => {
      const now = Date.now();
      const oldTimestamp = now - 91 * 86_400_000;
      const recentTimestamp = now - 10 * 86_400_000;

      store.record(makeRecord({ id: "old-entry", timestamp: oldTimestamp }));
      store.record(makeRecord({ id: "recent-entry", timestamp: recentTimestamp }));

      store.purgeExpired();

      const result = store.query({ page: 1, pageSize: 50 });
      expect(result.total).toBe(1);
      expect(result.entries[0].id).toBe("recent-entry");
    });

    it("removes expired resume states", () => {
      const now = Date.now();
      const expiredState = makeResumeState({
        transferId: "expired-resume",
        remotePath: "/remote/expired.bin",
        expiresAt: now - 1000, // already expired
      });
      const validState = makeResumeState({
        transferId: "valid-resume",
        remotePath: "/remote/valid.bin",
        expiresAt: now + 86_400_000, // expires tomorrow
      });

      store.saveResumeState(expiredState);
      store.saveResumeState(validState);

      store.purgeExpired();

      expect(store.getResumeState("profile-1", "/remote/expired.bin")).toBeNull();
      expect(store.getResumeState("profile-1", "/remote/valid.bin")).not.toBeNull();
    });

    it("keeps history entries within 90 days", () => {
      const now = Date.now();
      for (let i = 0; i < 5; i++) {
        store.record(makeRecord({ id: `keep-${i}`, timestamp: now - i * 86_400_000 }));
      }

      store.purgeExpired();

      const result = store.query({ page: 1, pageSize: 50 });
      expect(result.total).toBe(5);
    });
  });
});
