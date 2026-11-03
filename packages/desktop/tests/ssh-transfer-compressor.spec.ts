/**
 * Unit tests for TransferCompressor — verifies extension skip list,
 * compression/decompression round-trip, and Blake3 checksum verification.
 */

import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { Readable } from "node:stream";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { pipeline } from "node:stream/promises";
import { TransferCompressor } from "../electron/ssh-transfer-compressor";

// ── Helpers ──────────────────────────────────────────────────────

function readableFromBuffer(buf: Buffer): Readable {
  return Readable.from(buf);
}

async function streamToBuffer(stream: Readable): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of stream) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}

// ── Tests ────────────────────────────────────────────────────────

describe("TransferCompressor", () => {
  const compressor = new TransferCompressor();

  describe("shouldCompress", () => {
    it("returns true for compressible text files", () => {
      expect(compressor.shouldCompress("/home/user/project/main.ts")).toBe(true);
      expect(compressor.shouldCompress("document.txt")).toBe(true);
      expect(compressor.shouldCompress("/path/to/file.json")).toBe(true);
      expect(compressor.shouldCompress("README.md")).toBe(true);
      expect(compressor.shouldCompress("styles.css")).toBe(true);
    });

    it("returns false for already-compressed archive formats", () => {
      expect(compressor.shouldCompress("archive.zip")).toBe(false);
      expect(compressor.shouldCompress("data.gz")).toBe(false);
      expect(compressor.shouldCompress("backup.bz2")).toBe(false);
      expect(compressor.shouldCompress("file.xz")).toBe(false);
      expect(compressor.shouldCompress("compressed.zst")).toBe(false);
      expect(compressor.shouldCompress("archive.7z")).toBe(false);
      expect(compressor.shouldCompress("files.rar")).toBe(false);
    });

    it("returns false for image formats", () => {
      expect(compressor.shouldCompress("photo.png")).toBe(false);
      expect(compressor.shouldCompress("image.jpg")).toBe(false);
      expect(compressor.shouldCompress("picture.jpeg")).toBe(false);
      expect(compressor.shouldCompress("animation.gif")).toBe(false);
      expect(compressor.shouldCompress("modern.webp")).toBe(false);
    });

    it("returns false for video and audio formats", () => {
      expect(compressor.shouldCompress("video.mp4")).toBe(false);
      expect(compressor.shouldCompress("clip.webm")).toBe(false);
      expect(compressor.shouldCompress("movie.mkv")).toBe(false);
      expect(compressor.shouldCompress("song.mp3")).toBe(false);
      expect(compressor.shouldCompress("audio.aac")).toBe(false);
      expect(compressor.shouldCompress("lossless.flac")).toBe(false);
    });

    it("is case-insensitive for extensions", () => {
      expect(compressor.shouldCompress("FILE.ZIP")).toBe(false);
      expect(compressor.shouldCompress("Image.PNG")).toBe(false);
      expect(compressor.shouldCompress("VIDEO.MP4")).toBe(false);
    });

    it("returns true for files without extensions", () => {
      expect(compressor.shouldCompress("Makefile")).toBe(true);
      expect(compressor.shouldCompress("/usr/bin/node")).toBe(true);
    });

    it("returns true for dotfiles", () => {
      expect(compressor.shouldCompress(".gitignore")).toBe(true);
      expect(compressor.shouldCompress(".env")).toBe(true);
    });
  });

  describe("compressStream / decompressStream round-trip", () => {
    it("compresses and decompresses data correctly", async () => {
      const original = Buffer.from("Hello, world! ".repeat(1000));
      const input = readableFromBuffer(original);

      const compressResult = await compressor.compressStream(input);
      const compressedBuf = await streamToBuffer(compressResult.stream);

      // Compressed data should be smaller than original for repetitive content
      expect(compressedBuf.length).toBeLessThan(original.length);

      // originalSize is populated after the stream is fully consumed
      expect(compressResult.originalSize).toBe(original.length);

      // Decompress and verify round-trip
      const decompressInput = readableFromBuffer(compressedBuf);
      const decompressed = await compressor.decompressStream(decompressInput);
      const result = await streamToBuffer(decompressed);

      expect(result).toEqual(original);
    });

    it("handles empty input", async () => {
      const original = Buffer.alloc(0);
      const input = readableFromBuffer(original);

      const { stream: compressed } = await compressor.compressStream(input);
      const compressedBuf = await streamToBuffer(compressed);

      const decompressInput = readableFromBuffer(compressedBuf);
      const decompressed = await compressor.decompressStream(decompressInput);
      const result = await streamToBuffer(decompressed);

      expect(result).toEqual(original);
    });

    it("handles binary data", async () => {
      const original = Buffer.alloc(4096);
      for (let i = 0; i < original.length; i++) {
        original[i] = i % 256;
      }

      const input = readableFromBuffer(original);
      const { stream: compressed } = await compressor.compressStream(input);
      const compressedBuf = await streamToBuffer(compressed);

      const decompressInput = readableFromBuffer(compressedBuf);
      const decompressed = await compressor.decompressStream(decompressInput);
      const result = await streamToBuffer(decompressed);

      expect(result).toEqual(original);
    });
  });

  describe("verifyChecksum", () => {
    let tmpDir: string;
    let testFile: string;

    beforeAll(() => {
      tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "compressor-test-"));
      testFile = path.join(tmpDir, "test-file.txt");
      fs.writeFileSync(testFile, "Hello, Blake3 checksum verification!");
    });

    afterAll(() => {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    });

    it("returns true for matching hash", async () => {
      // Compute the expected hash using blake3 directly
      const { createHash } = await import("blake3");
      const hasher = createHash();
      hasher.update(fs.readFileSync(testFile));
      const expectedHash = hasher.digest("hex") as string;

      const result = await compressor.verifyChecksum(testFile, expectedHash);
      expect(result).toBe(true);
    });

    it("returns false for non-matching hash", async () => {
      const wrongHash = "0".repeat(64);
      const result = await compressor.verifyChecksum(testFile, wrongHash);
      expect(result).toBe(false);
    });

    it("is case-insensitive for hash comparison", async () => {
      const { createHash } = await import("blake3");
      const hasher = createHash();
      hasher.update(fs.readFileSync(testFile));
      const expectedHash = (hasher.digest("hex") as string).toUpperCase();

      const result = await compressor.verifyChecksum(testFile, expectedHash);
      expect(result).toBe(true);
    });
  });
});
