/**
 * Unit tests for the DeltaTransferHandler class.
 *
 * Tests delta transfer integration with DeltaSyncEngine and
 * CapabilityNegotiator, including fallback behavior, size threshold,
 * capability checks, timeout handling, and integrity verification.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { DeltaTransferHandler } from "../electron/ssh-delta-transfer";
import type { DeltaSyncEngine, DeltaPlan } from "../electron/ssh-delta-sync";
import type { CapabilityNegotiator } from "../electron/ssh-capabilities";

// ── Mock Factories ───────────────────────────────────────────────

function createMockCapabilityNegotiator(supported: boolean): CapabilityNegotiator {
  return {
    isSupported: (_profileId: string, _feature: string) => supported,
  } as unknown as CapabilityNegotiator;
}

function createMockDeltaSyncEngine(overrides?: Partial<DeltaSyncEngine>): DeltaSyncEngine {
  return {
    computeDelta: vi.fn().mockResolvedValue({
      sourceHash: "abc123",
      targetHash: "def456",
      matchedChunks: 10,
      newChunks: [{ offset: 0, length: 1024, hash: "chunk1" }],
      totalSavingsPercent: 85,
      fallbackToFull: false,
    } satisfies DeltaPlan),
    applyDeltaUpload: vi.fn().mockResolvedValue(undefined),
    applyDeltaDownload: vi.fn().mockResolvedValue(undefined),
    verifyIntegrity: vi.fn().mockResolvedValue(true),
    ...overrides,
  } as unknown as DeltaSyncEngine;
}

function createMockLogger() {
  return {
    warn: vi.fn(),
    debug: vi.fn(),
  };
}

/** Helper to create a getFileSize function that returns a fixed size. */
function fileSizeOf(bytes: number) {
  return (_path: string) => bytes;
}

// ── Tests ────────────────────────────────────────────────────────

describe("DeltaTransferHandler", () => {
  const profileId = "test-profile";
  const localPath = "/tmp/test-file.bin";
  const remotePath = "/remote/test-file.bin";

  beforeEach(() => {
    vi.restoreAllMocks();
  });

  describe("capability check", () => {
    it("falls back when deltaSync capability is not supported", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(false);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(5 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("not supported");
      expect(engine.computeDelta).not.toHaveBeenCalled();
    });

    it("proceeds when deltaSync capability is supported", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(true);
      expect(engine.computeDelta).toHaveBeenCalledWith(profileId, localPath, remotePath);
    });
  });

  describe("size threshold", () => {
    it("skips delta for files smaller than 1 MB", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(512 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("below 1 MB");
      expect(engine.computeDelta).not.toHaveBeenCalled();
    });

    it("attempts delta for files exactly 1 MB", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(1 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(true);
      expect(engine.computeDelta).toHaveBeenCalled();
    });

    it("attempts delta for files larger than 1 MB", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(10 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(true);
    });

    it("skips delta when local file does not exist", async () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: () => -1, // file not found
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "download");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("below 1 MB");
    });
  });

  describe("delta plan fallback", () => {
    it("falls back when delta plan indicates fallbackToFull", async () => {
      const engine = createMockDeltaSyncEngine({
        computeDelta: vi.fn().mockResolvedValue({
          sourceHash: "abc",
          targetHash: "def",
          matchedChunks: 0,
          newChunks: [],
          totalSavingsPercent: 0,
          fallbackToFull: true,
        } satisfies DeltaPlan),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(5 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("full transfer required");
    });
  });

  describe("upload delta transfer", () => {
    it("calls applyDeltaUpload for upload direction", async () => {
      const applyDeltaUpload = vi.fn().mockResolvedValue(undefined);
      const engine = createMockDeltaSyncEngine({ applyDeltaUpload } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(applyDeltaUpload).toHaveBeenCalledWith(
        profileId,
        expect.any(Object),
        localPath,
        remotePath,
      );
    });

    it("verifies source integrity after upload", async () => {
      const verifyIntegrity = vi.fn().mockResolvedValue(true);
      const engine = createMockDeltaSyncEngine({ verifyIntegrity } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(verifyIntegrity).toHaveBeenCalledWith(localPath, "abc123");
    });
  });

  describe("download delta transfer", () => {
    it("calls applyDeltaDownload for download direction", async () => {
      const applyDeltaDownload = vi.fn().mockResolvedValue(undefined);
      const engine = createMockDeltaSyncEngine({ applyDeltaDownload } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      await handler.transfer(profileId, localPath, remotePath, "download");

      expect(applyDeltaDownload).toHaveBeenCalledWith(
        profileId,
        expect.any(Object),
        localPath,
        remotePath,
      );
    });

    it("verifies target integrity after download", async () => {
      const verifyIntegrity = vi.fn().mockResolvedValue(true);
      const engine = createMockDeltaSyncEngine({ verifyIntegrity } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      await handler.transfer(profileId, localPath, remotePath, "download");

      expect(verifyIntegrity).toHaveBeenCalledWith(localPath, "def456");
    });
  });

  describe("integrity verification failure", () => {
    it("falls back when integrity check fails", async () => {
      const verifyIntegrity = vi.fn().mockResolvedValue(false);
      const engine = createMockDeltaSyncEngine({ verifyIntegrity } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const logger = createMockLogger();
      const handler = new DeltaTransferHandler(engine, caps, {
        logger,
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("integrity verification failed");
      expect(logger.warn).toHaveBeenCalled();
    });
  });

  describe("error handling and fallback", () => {
    it("falls back when computeDelta throws", async () => {
      const engine = createMockDeltaSyncEngine({
        computeDelta: vi.fn().mockRejectedValue(new Error("RPC connection lost")),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const logger = createMockLogger();
      const handler = new DeltaTransferHandler(engine, caps, {
        logger,
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("RPC connection lost");
      expect(logger.warn).toHaveBeenCalled();
    });

    it("falls back when applyDeltaUpload throws", async () => {
      const engine = createMockDeltaSyncEngine({
        applyDeltaUpload: vi.fn().mockRejectedValue(new Error("write_chunks failed")),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const logger = createMockLogger();
      const handler = new DeltaTransferHandler(engine, caps, {
        logger,
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("write_chunks failed");
    });

    it("falls back when applyDeltaDownload throws", async () => {
      const engine = createMockDeltaSyncEngine({
        applyDeltaDownload: vi.fn().mockRejectedValue(new Error("read_chunks failed")),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const logger = createMockLogger();
      const handler = new DeltaTransferHandler(engine, caps, {
        logger,
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "download");

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("read_chunks failed");
    });
  });

  describe("timeout handling", () => {
    it("falls back when delta transfer exceeds timeout", async () => {
      const engine = createMockDeltaSyncEngine({
        computeDelta: vi.fn().mockImplementation(
          () => new Promise((resolve) => setTimeout(resolve, 120_000)),
        ),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const logger = createMockLogger();
      const handler = new DeltaTransferHandler(engine, caps, {
        logger,
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      vi.useFakeTimers();

      const resultPromise = handler.transfer(profileId, localPath, remotePath, "upload");

      // Advance past the 60s timeout
      await vi.advanceTimersByTimeAsync(61_000);

      const result = await resultPromise;

      expect(result.usedDelta).toBe(false);
      expect(result.fallbackReason).toContain("timed out");
      expect(logger.warn).toHaveBeenCalled();

      vi.useRealTimers();
    });
  });

  describe("identical files (no chunks differ)", () => {
    it("reports 100% savings when files are identical", async () => {
      const engine = createMockDeltaSyncEngine({
        computeDelta: vi.fn().mockResolvedValue({
          sourceHash: "same_hash",
          targetHash: "same_hash",
          matchedChunks: 15,
          newChunks: [],
          totalSavingsPercent: 100,
          fallbackToFull: false,
        } satisfies DeltaPlan),
      } as unknown as Partial<DeltaSyncEngine>);
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(5 * 1024 * 1024),
      });

      const result = await handler.transfer(profileId, localPath, remotePath, "upload");

      expect(result.usedDelta).toBe(true);
      expect(result.savingsPercent).toBe(100);
      // Should not call apply methods when files are identical
      expect(engine.applyDeltaUpload).not.toHaveBeenCalled();
    });
  });

  describe("shouldAttemptDelta", () => {
    it("returns false when capability is not supported", () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(false);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(5 * 1024 * 1024),
      });

      expect(handler.shouldAttemptDelta(profileId, localPath, "upload")).toBe(false);
    });

    it("returns false when file is below threshold", () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(500 * 1024),
      });

      expect(handler.shouldAttemptDelta(profileId, localPath, "upload")).toBe(false);
    });

    it("returns true when capability is supported and file is large enough", () => {
      const engine = createMockDeltaSyncEngine();
      const caps = createMockCapabilityNegotiator(true);
      const handler = new DeltaTransferHandler(engine, caps, {
        getFileSize: fileSizeOf(2 * 1024 * 1024),
      });

      expect(handler.shouldAttemptDelta(profileId, localPath, "upload")).toBe(true);
    });
  });
});
