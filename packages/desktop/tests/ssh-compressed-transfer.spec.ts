/**
 * Unit tests for CompressedTransferHandler — verifies compressed upload,
 * compressed download, fallback behavior, and capability checking.
 */

import { describe, it, expect, beforeAll, afterAll, vi } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { CompressedTransferHandler } from "../electron/ssh-compressed-transfer";
import { TransferCompressor } from "../electron/ssh-transfer-compressor";

// ── Mock factories ───────────────────────────────────────────────

function createMockCapabilities(supported: boolean) {
  return {
    isSupported: vi.fn((_profileId: string, _feature: string) => supported),
    negotiate: vi.fn(),
    getCached: vi.fn(),
    clearCache: vi.fn(),
  };
}

function createMockRouter(responses: Record<string, unknown> = {}) {
  return {
    route: vi.fn(async (_repoId: string, method: string, _params: unknown) => {
      if (responses[method]) return responses[method];
      throw new Error(`Unexpected RPC method: ${method}`);
    }),
    // Satisfy the RpcRouter interface minimally
    sendToRemoteAgent: vi.fn(),
    on: vi.fn(),
    emit: vi.fn(),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("CompressedTransferHandler", () => {
  let tmpDir: string;

  beforeAll(() => {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "compressed-transfer-test-"));
  });

  afterAll(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe("isCompressionAvailable", () => {
    it("returns true when agent supports compression", () => {
      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      expect(handler.isCompressionAvailable("profile-1")).toBe(true);
      expect(capabilities.isSupported).toHaveBeenCalledWith("profile-1", "compress_file", "decompress_write");
    });

    it("returns false when agent does not support compression", () => {
      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(false);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      expect(handler.isCompressionAvailable("profile-1")).toBe(false);
    });
  });

  describe("uploadCompressed", () => {
    it("compresses and uploads file when agent supports compression", async () => {
      const testFile = path.join(tmpDir, "upload-test.txt");
      const content = "Hello, compressed upload! ".repeat(100);
      fs.writeFileSync(testFile, content);

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        decompress_write: { success: true, verified: true },
      });

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.uploadCompressed("profile-1", testFile, "/remote/file.txt");

      expect(result.success).toBe(true);
      expect(result.compressed).toBe(true);
      expect(result.originalSize).toBe(Buffer.byteLength(content));
      expect(result.compressedSize).toBeGreaterThan(0);
      expect(result.compressedSize).toBeLessThan(result.originalSize);
      expect(result.hash).toHaveLength(64); // Blake3 hex hash

      // Verify the RPC was called with correct params
      expect(router.route).toHaveBeenCalledWith(
        "ssh://profile-1/",
        "decompress_write",
        expect.objectContaining({
          path: "/remote/file.txt",
          algorithm: "zstd",
          expected_hash: expect.any(String),
          data_base64: expect.any(String),
        }),
      );
    });

    it("returns failure when agent does not support compression", async () => {
      const testFile = path.join(tmpDir, "upload-no-cap.txt");
      fs.writeFileSync(testFile, "test content");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(false);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.uploadCompressed("profile-1", testFile, "/remote/file.txt");

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
      expect(router.route).not.toHaveBeenCalled();
    });

    it("skips compression for already-compressed file types", async () => {
      const testFile = path.join(tmpDir, "upload-skip.zip");
      fs.writeFileSync(testFile, "fake zip content");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.uploadCompressed("profile-1", testFile, "/remote/file.zip");

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
      expect(router.route).not.toHaveBeenCalled();
    });

    it("falls back on RPC failure", async () => {
      const testFile = path.join(tmpDir, "upload-rpc-fail.txt");
      fs.writeFileSync(testFile, "test content for rpc failure");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        // decompress_write will throw because it's not in responses
      });
      // Override to throw
      router.route.mockRejectedValue(new Error("RPC connection lost"));

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.uploadCompressed("profile-1", testFile, "/remote/file.txt");

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
    });

    it("falls back when agent reports verification failure", async () => {
      const testFile = path.join(tmpDir, "upload-verify-fail.txt");
      fs.writeFileSync(testFile, "content that fails verification");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        decompress_write: { success: true, verified: false },
      });

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.uploadCompressed("profile-1", testFile, "/remote/file.txt");

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
    });
  });

  describe("downloadCompressed", () => {
    it("downloads and decompresses file when agent supports compression", async () => {
      const localFile = path.join(tmpDir, "download-test.txt");
      const originalContent = "Hello, compressed download! ".repeat(100);

      // Simulate what the agent would return: compress the content
      const compressor = new TransferCompressor();
      const { createHash } = await import("blake3");
      const hasher = createHash();
      hasher.update(Buffer.from(originalContent));
      const expectedHash = hasher.digest("hex") as string;
      hasher.dispose();

      // Compress the content to simulate agent response
      const { Readable } = await import("node:stream");
      const inputStream = Readable.from(Buffer.from(originalContent));
      const { stream: compressedStream } = await compressor.compressStream(inputStream);
      const chunks: Buffer[] = [];
      for await (const chunk of compressedStream) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      }
      const compressedBuffer = Buffer.concat(chunks);

      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        compress_file: {
          hash: expectedHash,
          original_size: Buffer.byteLength(originalContent),
          compressed_size: compressedBuffer.length,
          data_base64: compressedBuffer.toString("base64"),
        },
      });

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/file.txt", localFile);

      expect(result.success).toBe(true);
      expect(result.compressed).toBe(true);
      expect(result.originalSize).toBe(Buffer.byteLength(originalContent));
      expect(result.compressedSize).toBeLessThan(result.originalSize);
      expect(result.hash).toBe(expectedHash);

      // Verify the file was written correctly
      const written = fs.readFileSync(localFile, "utf8");
      expect(written).toBe(originalContent);
    });

    it("returns failure when agent does not support compression", async () => {
      const localFile = path.join(tmpDir, "download-no-cap.txt");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(false);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/file.txt", localFile);

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
      expect(router.route).not.toHaveBeenCalled();
    });

    it("skips compression for already-compressed file types", async () => {
      const localFile = path.join(tmpDir, "download-skip.png");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter();

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/image.png", localFile);

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
      expect(router.route).not.toHaveBeenCalled();
    });

    it("falls back on RPC failure", async () => {
      const localFile = path.join(tmpDir, "download-rpc-fail.txt");

      const compressor = new TransferCompressor();
      const capabilities = createMockCapabilities(true);
      const router = createMockRouter();
      router.route.mockRejectedValue(new Error("RPC connection lost"));

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/file.txt", localFile);

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);
    });

    it("falls back when checksum verification fails", async () => {
      const localFile = path.join(tmpDir, "download-checksum-fail.txt");
      const originalContent = "content for checksum failure test";

      // Compress the content but provide a wrong hash
      const compressor = new TransferCompressor();
      const { Readable } = await import("node:stream");
      const inputStream = Readable.from(Buffer.from(originalContent));
      const { stream: compressedStream } = await compressor.compressStream(inputStream);
      const chunks: Buffer[] = [];
      for await (const chunk of compressedStream) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      }
      const compressedBuffer = Buffer.concat(chunks);

      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        compress_file: {
          hash: "0".repeat(64), // Wrong hash
          original_size: Buffer.byteLength(originalContent),
          compressed_size: compressedBuffer.length,
          data_base64: compressedBuffer.toString("base64"),
        },
      });

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/file.txt", localFile);

      expect(result.success).toBe(false);
      expect(result.compressed).toBe(false);

      // Corrupted file should be cleaned up
      expect(fs.existsSync(localFile)).toBe(false);
    });

    it("creates target directory if it does not exist", async () => {
      const nestedDir = path.join(tmpDir, "nested", "deep", "dir");
      const localFile = path.join(nestedDir, "download-nested.txt");
      const originalContent = "nested directory test content ".repeat(50);

      const compressor = new TransferCompressor();
      const { createHash } = await import("blake3");
      const hasher = createHash();
      hasher.update(Buffer.from(originalContent));
      const expectedHash = hasher.digest("hex") as string;
      hasher.dispose();

      const { Readable } = await import("node:stream");
      const inputStream = Readable.from(Buffer.from(originalContent));
      const { stream: compressedStream } = await compressor.compressStream(inputStream);
      const chunks: Buffer[] = [];
      for await (const chunk of compressedStream) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      }
      const compressedBuffer = Buffer.concat(chunks);

      const capabilities = createMockCapabilities(true);
      const router = createMockRouter({
        compress_file: {
          hash: expectedHash,
          original_size: Buffer.byteLength(originalContent),
          compressed_size: compressedBuffer.length,
          data_base64: compressedBuffer.toString("base64"),
        },
      });

      const handler = new CompressedTransferHandler(compressor, capabilities as any, router as any);
      const result = await handler.downloadCompressed("profile-1", "/remote/file.txt", localFile);

      expect(result.success).toBe(true);
      expect(fs.existsSync(localFile)).toBe(true);
      expect(fs.readFileSync(localFile, "utf8")).toBe(originalContent);
    });
  });
});
