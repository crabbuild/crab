/**
 * Tests for TransferResumeHandler — resume logic for interrupted transfers.
 *
 * Verifies:
 * - persistInterruption() saves resume state with SHA-256 checksum
 * - verifyAndPrepareResume() validates remote file unchanged + local integrity
 * - Resume discarded after 3 failed attempts
 * - Resume discarded when remote file mtime/size changed
 * - Resume discarded when local partial file checksum mismatches
 * - Upload resume verifies remote partial file size matches persisted offset
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as crypto from "node:crypto";
import Database from "better-sqlite3";
import { TransferHistoryStore } from "../electron/ssh-transfer-history";
import { TransferResumeHandler } from "../electron/ssh-transfer-resume";
import type { TransferResumeState } from "../electron/ssh-types";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createInMemoryDb(): Database.Database {
  const db = new Database(":memory:");
  db.pragma("journal_mode = WAL");

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

function createMockSftp(statResult: { size: number; mtime: number } | Error): any {
  return {
    stat: (_path: string, cb: (err: Error | null, stats?: any) => void) => {
      if (statResult instanceof Error) {
        cb(statResult);
      } else {
        cb(null, statResult);
      }
    },
  };
}

function createTempFile(content: Buffer): string {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "resume-test-"));
  const filePath = path.join(tmpDir, "partial-file.bin");
  fs.writeFileSync(filePath, content);
  return filePath;
}

function sha256(data: Buffer): string {
  return crypto.createHash("sha256").update(data).digest("hex");
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("TransferResumeHandler", () => {
  let db: Database.Database;
  let store: TransferHistoryStore;
  let handler: TransferResumeHandler;
  const tempFiles: string[] = [];

  beforeEach(() => {
    db = createInMemoryDb();
    store = new TransferHistoryStore(db);
    handler = new TransferResumeHandler(store);
  });

  afterEach(() => {
    db.close();
    while (tempFiles.length > 0) {
      const f = tempFiles.pop()!;
      try {
        fs.unlinkSync(f);
        fs.rmdirSync(path.dirname(f));
      } catch { /* ignore */ }
    }
  });

  describe("persistInterruption", () => {
    it("saves resume state with correct checksum for a download", async () => {
      const content = Buffer.from("hello world partial content");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      await handler.persistInterruption(
        {
          transferId: "t-001",
          profileId: "profile-1",
          direction: "download",
          localPath: filePath,
          remotePath: "/remote/file.bin",
          bytesTransferred: content.length,
          totalBytes: 1000,
        },
        1700000000,
        1000,
      );

      const state = store.getResumeState("profile-1", "/remote/file.bin");
      expect(state).not.toBeNull();
      expect(state!.transferId).toBe("t-001");
      expect(state!.bytesTransferred).toBe(content.length);
      expect(state!.remoteMtime).toBe(1700000000);
      expect(state!.remoteSize).toBe(1000);
      expect(state!.partialChecksum).toBe(sha256(content));
    });

    it("does not persist state when bytesTransferred is 0", async () => {
      await handler.persistInterruption(
        {
          transferId: "t-002",
          profileId: "profile-1",
          direction: "download",
          localPath: "/tmp/nonexistent",
          remotePath: "/remote/empty.bin",
          bytesTransferred: 0,
          totalBytes: 1000,
        },
        1700000000,
        1000,
      );

      const state = store.getResumeState("profile-1", "/remote/empty.bin");
      expect(state).toBeNull();
    });

    it("sets expiry to 7 days from creation", async () => {
      const content = Buffer.from("data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const before = Date.now();
      await handler.persistInterruption(
        {
          transferId: "t-003",
          profileId: "profile-1",
          direction: "download",
          localPath: filePath,
          remotePath: "/remote/expiry.bin",
          bytesTransferred: content.length,
          totalBytes: 100,
        },
        1700000000,
        100,
      );
      const after = Date.now();

      const state = store.getResumeState("profile-1", "/remote/expiry.bin");
      expect(state).not.toBeNull();

      const sevenDaysMs = 7 * 24 * 60 * 60 * 1000;
      expect(state!.expiresAt).toBeGreaterThanOrEqual(before + sevenDaysMs);
      expect(state!.expiresAt).toBeLessThanOrEqual(after + sevenDaysMs);
    });
  });

  describe("verifyAndPrepareResume — download", () => {
    it("returns canResume=true when remote file unchanged and local checksum matches", async () => {
      const content = Buffer.from("partial download data here");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const checksum = sha256(content);
      const now = Date.now();

      // Manually insert resume state
      store.saveResumeState({
        transferId: "t-resume-1",
        profileId: "profile-1",
        direction: "download",
        localPath: filePath,
        remotePath: "/remote/resume.bin",
        bytesTransferred: content.length,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: checksum,
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/resume.bin",
        filePath,
        "download",
        sftp,
      );

      expect(result.canResume).toBe(true);
      expect(result.offset).toBe(content.length);
    });

    it("returns canResume=false when no resume state exists", async () => {
      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/no-state.bin",
        "/tmp/whatever",
        "download",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toBe("no resume state found");
    });

    it("returns canResume=false when remote mtime changed", async () => {
      const content = Buffer.from("data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-mtime",
        profileId: "profile-1",
        direction: "download",
        localPath: filePath,
        remotePath: "/remote/mtime-changed.bin",
        bytesTransferred: content.length,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: sha256(content),
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      // Remote mtime is different
      const sftp = createMockSftp({ size: 1000, mtime: 1700001000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/mtime-changed.bin",
        filePath,
        "download",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("mtime changed");
    });

    it("returns canResume=false when remote size changed", async () => {
      const content = Buffer.from("data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-size",
        profileId: "profile-1",
        direction: "download",
        localPath: filePath,
        remotePath: "/remote/size-changed.bin",
        bytesTransferred: content.length,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: sha256(content),
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      // Remote size is different
      const sftp = createMockSftp({ size: 2000, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/size-changed.bin",
        filePath,
        "download",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("size changed");
    });

    it("returns canResume=false when local checksum mismatches (corrupted partial)", async () => {
      const originalContent = Buffer.from("original partial data");
      const corruptedContent = Buffer.from("corrupted_partial_da"); // same length, different content
      const filePath = createTempFile(corruptedContent);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-corrupt",
        profileId: "profile-1",
        direction: "download",
        localPath: filePath,
        remotePath: "/remote/corrupt.bin",
        bytesTransferred: originalContent.length,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: sha256(originalContent), // checksum of original, not corrupted
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/corrupt.bin",
        filePath,
        "download",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("checksum mismatch");
    });

    it("returns canResume=false when resume state is expired", async () => {
      const content = Buffer.from("data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-expired",
        profileId: "profile-1",
        direction: "download",
        localPath: filePath,
        remotePath: "/remote/expired.bin",
        bytesTransferred: content.length,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: sha256(content),
        createdAt: now - 8 * 86_400_000, // 8 days ago
        expiresAt: now - 86_400_000, // expired yesterday
      });

      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/expired.bin",
        filePath,
        "download",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("expired");
    });
  });

  describe("verifyAndPrepareResume — upload", () => {
    it("returns canResume=true when remote partial size matches persisted offset", async () => {
      const content = Buffer.from("upload source file content here");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const bytesAlreadyUploaded = 15;
      const checksum = sha256(content.subarray(0, bytesAlreadyUploaded));
      const now = Date.now();

      // Write a partial local file that represents the source
      // For uploads, the local file is the full source — checksum is of the first N bytes
      const sourceContent = content;
      const sourcePath = createTempFile(sourceContent);
      tempFiles.push(sourcePath);

      store.saveResumeState({
        transferId: "t-upload-1",
        profileId: "profile-1",
        direction: "upload",
        localPath: sourcePath,
        remotePath: "/remote/upload.bin",
        bytesTransferred: bytesAlreadyUploaded,
        totalBytes: sourceContent.length,
        remoteMtime: 1700000000,
        remoteSize: bytesAlreadyUploaded,
        partialChecksum: checksum,
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      // Remote file has exactly bytesAlreadyUploaded bytes
      const sftp = createMockSftp({ size: bytesAlreadyUploaded, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/upload.bin",
        sourcePath,
        "upload",
        sftp,
      );

      expect(result.canResume).toBe(true);
      expect(result.offset).toBe(bytesAlreadyUploaded);
    });

    it("returns canResume=false when remote partial size does not match", async () => {
      const content = Buffer.from("source data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-upload-mismatch",
        profileId: "profile-1",
        direction: "upload",
        localPath: filePath,
        remotePath: "/remote/upload-mismatch.bin",
        bytesTransferred: 5,
        totalBytes: content.length,
        remoteMtime: 1700000000,
        remoteSize: 5,
        partialChecksum: sha256(content.subarray(0, 5)),
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      // Remote file has different size than expected
      const sftp = createMockSftp({ size: 3, mtime: 1700000000 });

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/upload-mismatch.bin",
        filePath,
        "upload",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("does not match");
    });

    it("returns canResume=false when remote file does not exist", async () => {
      const content = Buffer.from("source");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      store.saveResumeState({
        transferId: "t-upload-gone",
        profileId: "profile-1",
        direction: "upload",
        localPath: filePath,
        remotePath: "/remote/upload-gone.bin",
        bytesTransferred: 3,
        totalBytes: content.length,
        remoteMtime: 1700000000,
        remoteSize: 3,
        partialChecksum: sha256(content.subarray(0, 3)),
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      const sftp = createMockSftp(new Error("No such file"));

      const result = await handler.verifyAndPrepareResume(
        "profile-1",
        "/remote/upload-gone.bin",
        filePath,
        "upload",
        sftp,
      );

      expect(result.canResume).toBe(false);
      expect(result.reason).toContain("remote partial file not found");
    });
  });

  describe("resume attempt limits", () => {
    it("discards state after 3 failed resume attempts", async () => {
      const content = Buffer.from("persistent data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      const checksum = sha256(content);

      // Helper to re-insert state (since discard overwrites it)
      const insertState = () => {
        store.saveResumeState({
          transferId: "t-retry",
          profileId: "profile-1",
          direction: "download",
          localPath: filePath,
          remotePath: "/remote/retry.bin",
          bytesTransferred: content.length,
          totalBytes: 1000,
          remoteMtime: 1700000000,
          remoteSize: 1000,
          partialChecksum: checksum,
          createdAt: now,
          expiresAt: now + 7 * 86_400_000,
        });
      };

      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      // First 3 attempts should succeed (canResume=true)
      insertState();
      const r1 = await handler.verifyAndPrepareResume("profile-1", "/remote/retry.bin", filePath, "download", sftp);
      expect(r1.canResume).toBe(true);
      handler.onResumeFailure("profile-1", "/remote/retry.bin");

      insertState();
      const r2 = await handler.verifyAndPrepareResume("profile-1", "/remote/retry.bin", filePath, "download", sftp);
      expect(r2.canResume).toBe(true);
      handler.onResumeFailure("profile-1", "/remote/retry.bin");

      insertState();
      const r3 = await handler.verifyAndPrepareResume("profile-1", "/remote/retry.bin", filePath, "download", sftp);
      expect(r3.canResume).toBe(true);
      handler.onResumeFailure("profile-1", "/remote/retry.bin");

      // 4th attempt should be rejected (3 failures recorded)
      insertState();
      const r4 = await handler.verifyAndPrepareResume("profile-1", "/remote/retry.bin", filePath, "download", sftp);
      expect(r4.canResume).toBe(false);
      expect(r4.reason).toContain("maximum resume attempts");
    });

    it("resets attempt counter on successful resume", async () => {
      const content = Buffer.from("success data");
      const filePath = createTempFile(content);
      tempFiles.push(filePath);

      const now = Date.now();
      const checksum = sha256(content);

      const insertState = () => {
        store.saveResumeState({
          transferId: "t-success",
          profileId: "profile-1",
          direction: "download",
          localPath: filePath,
          remotePath: "/remote/success.bin",
          bytesTransferred: content.length,
          totalBytes: 1000,
          remoteMtime: 1700000000,
          remoteSize: 1000,
          partialChecksum: checksum,
          createdAt: now,
          expiresAt: now + 7 * 86_400_000,
        });
      };

      const sftp = createMockSftp({ size: 1000, mtime: 1700000000 });

      // Two failures
      insertState();
      await handler.verifyAndPrepareResume("profile-1", "/remote/success.bin", filePath, "download", sftp);
      handler.onResumeFailure("profile-1", "/remote/success.bin");

      insertState();
      await handler.verifyAndPrepareResume("profile-1", "/remote/success.bin", filePath, "download", sftp);
      handler.onResumeFailure("profile-1", "/remote/success.bin");

      // Success resets counter
      handler.onResumeSuccess("profile-1", "/remote/success.bin");

      // Should be able to resume again (counter reset)
      insertState();
      const result = await handler.verifyAndPrepareResume("profile-1", "/remote/success.bin", filePath, "download", sftp);
      expect(result.canResume).toBe(true);
    });
  });

  describe("hasResumeState", () => {
    it("returns true when state exists", () => {
      const now = Date.now();
      store.saveResumeState({
        transferId: "t-has",
        profileId: "profile-1",
        direction: "download",
        localPath: "/local/file.bin",
        remotePath: "/remote/has.bin",
        bytesTransferred: 100,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: "abc",
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      expect(handler.hasResumeState("profile-1", "/remote/has.bin")).toBe(true);
    });

    it("returns false when no state exists", () => {
      expect(handler.hasResumeState("profile-1", "/remote/nope.bin")).toBe(false);
    });
  });

  describe("onResumeSuccess", () => {
    it("clears resume state from store", () => {
      const now = Date.now();
      store.saveResumeState({
        transferId: "t-clear",
        profileId: "profile-1",
        direction: "download",
        localPath: "/local/file.bin",
        remotePath: "/remote/clear.bin",
        bytesTransferred: 100,
        totalBytes: 1000,
        remoteMtime: 1700000000,
        remoteSize: 1000,
        partialChecksum: "abc",
        createdAt: now,
        expiresAt: now + 7 * 86_400_000,
      });

      handler.onResumeSuccess("profile-1", "/remote/clear.bin");

      // The state should be discarded (overwritten with expired entry)
      const state = store.getResumeState("profile-1", "/remote/clear.bin");
      // It will exist but be expired
      if (state) {
        expect(state.expiresAt).toBeLessThan(Date.now());
      }
    });
  });
});
