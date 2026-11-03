/**
 * Compressed Transfer Handler — integrates TransferCompressor with
 * file transfers, coordinating compression for uploads and downloads
 * with the remote agent via RPC.
 *
 * For uploads: compresses locally, sends compressed data to the remote
 * agent which decompresses via "decompress_write" RPC.
 *
 * For downloads: requests compressed data from the remote agent via
 * "compress_file" RPC, decompresses locally.
 *
 * Falls back to uncompressed transfer if compression/decompression
 * fails or the agent does not support compression.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { Readable } from "node:stream";
import { TransferCompressor } from "./ssh-transfer-compressor";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { RpcRouter } from "./ssh-router";

// ── Types ────────────────────────────────────────────────────────

export interface CompressedUploadResult {
  success: boolean;
  compressed: boolean;
  originalSize: number;
  compressedSize: number;
  hash: string;
}

export interface CompressedDownloadResult {
  success: boolean;
  compressed: boolean;
  originalSize: number;
  compressedSize: number;
  hash: string;
}

interface DecompressWriteResponse {
  success: boolean;
  verified: boolean;
}

interface CompressFileResponse {
  hash: string;
  original_size: number;
  compressed_size: number;
  data_base64: string;
}

// ── CompressedTransferHandler ────────────────────────────────────

export class CompressedTransferHandler {
  private readonly compressor: TransferCompressor;
  private readonly capabilities: CapabilityNegotiator;
  private readonly router: RpcRouter;

  constructor(
    compressor: TransferCompressor,
    capabilities: CapabilityNegotiator,
    router: RpcRouter,
  ) {
    this.compressor = compressor;
    this.capabilities = capabilities;
    this.router = router;
  }

  /**
   * Upload a local file to the remote server with compression.
   * Compresses locally, sends compressed data to the agent which
   * decompresses via "decompress_write" RPC.
   *
   * Falls back to returning { success: false, compressed: false }
   * if compression fails, allowing the caller to use uncompressed
   * transfer instead.
   */
  async uploadCompressed(
    profileId: string,
    localPath: string,
    remotePath: string,
  ): Promise<CompressedUploadResult> {
    // Verify agent supports compression
    if (!this.isCompressionAvailable(profileId)) {
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }

    // Check if this file type should be compressed
    if (!this.compressor.shouldCompress(localPath)) {
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }

    try {
      // Compute Blake3 hash of the original file for integrity verification
      const originalHash = await this.computeFileHash(localPath);

      // Read the file and compress it
      const fileStream = fs.createReadStream(localPath);
      const compressResult = await this.compressor.compressStream(fileStream);

      // Collect compressed data into a buffer (this consumes the stream,
      // which populates the originalSize counter)
      const compressedBuffer = await this.streamToBuffer(compressResult.stream);
      const compressedSize = compressedBuffer.length;
      const originalSize = compressResult.originalSize;

      // Send compressed data to remote agent for decompression and write
      const repoId = `ssh://${profileId}/`;
      const response = await this.router.route(repoId, "decompress_write", {
        path: remotePath,
        algorithm: "zstd",
        expected_hash: originalHash,
        data_base64: compressedBuffer.toString("base64"),
      }) as DecompressWriteResponse;

      if (!response.success || !response.verified) {
        return { success: false, compressed: false, originalSize, compressedSize, hash: originalHash };
      }

      return {
        success: true,
        compressed: true,
        originalSize,
        compressedSize,
        hash: originalHash,
      };
    } catch {
      // Compression or RPC failed — caller should fall back to uncompressed
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }
  }

  /**
   * Download a file from the remote server with compression.
   * Requests compressed data from the agent via "compress_file" RPC,
   * decompresses locally, and verifies integrity via Blake3.
   *
   * Falls back to returning { success: false, compressed: false }
   * if decompression fails, allowing the caller to use uncompressed
   * transfer instead.
   */
  async downloadCompressed(
    profileId: string,
    remotePath: string,
    localPath: string,
  ): Promise<CompressedDownloadResult> {
    // Verify agent supports compression
    if (!this.isCompressionAvailable(profileId)) {
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }

    // Check if this file type should be compressed
    if (!this.compressor.shouldCompress(remotePath)) {
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }

    try {
      // Request compressed file from remote agent
      const repoId = `ssh://${profileId}/`;
      const response = await this.router.route(repoId, "compress_file", {
        path: remotePath,
        algorithm: "zstd",
        level: 3,
      }) as CompressFileResponse;

      const compressedBuffer = Buffer.from(response.data_base64, "base64");
      const compressedSize = compressedBuffer.length;
      const originalSize = response.original_size;
      const expectedHash = response.hash;

      // Decompress the data locally
      const compressedStream = Readable.from(compressedBuffer);
      const decompressedStream = await this.compressor.decompressStream(compressedStream);

      // Ensure the target directory exists
      const targetDir = path.dirname(localPath);
      await fs.promises.mkdir(targetDir, { recursive: true });

      // Write decompressed data to local file
      await this.streamToFile(decompressedStream, localPath);

      // Verify integrity via Blake3 checksum
      const checksumValid = await this.compressor.verifyChecksum(localPath, expectedHash);
      if (!checksumValid) {
        // Integrity check failed — remove the corrupted file
        await fs.promises.unlink(localPath).catch(() => {});
        return { success: false, compressed: false, originalSize, compressedSize, hash: expectedHash };
      }

      return {
        success: true,
        compressed: true,
        originalSize,
        compressedSize,
        hash: expectedHash,
      };
    } catch {
      // Decompression or RPC failed — caller should fall back to uncompressed
      return { success: false, compressed: false, originalSize: 0, compressedSize: 0, hash: "" };
    }
  }

  /**
   * Check whether compression is available for a given profile.
   * Requires the agent to support the "compression" capability.
   */
  isCompressionAvailable(profileId: string): boolean {
    return this.capabilities.isSupported(profileId, "compression");
  }

  // ── Private helpers ────────────────────────────────────────────

  /**
   * Compute the Blake3 hash of a file on disk.
   */
  private async computeFileHash(filePath: string): Promise<string> {
    const { createHash } = await import("blake3");
    const hasher = createHash();
    const fileStream = fs.createReadStream(filePath);

    for await (const chunk of fileStream) {
      hasher.update(chunk);
    }

    const hash = hasher.digest("hex") as string;
    hasher.dispose();
    return hash;
  }

  /**
   * Collect a readable stream into a single Buffer.
   */
  private async streamToBuffer(stream: Readable): Promise<Buffer> {
    const chunks: Buffer[] = [];
    for await (const chunk of stream) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    }
    return Buffer.concat(chunks);
  }

  /**
   * Write a readable stream to a file on disk.
   */
  private async streamToFile(stream: Readable, filePath: string): Promise<void> {
    const writeStream = fs.createWriteStream(filePath);
    for await (const chunk of stream) {
      const ok = writeStream.write(chunk);
      if (!ok) {
        await new Promise<void>((resolve) => writeStream.once("drain", resolve));
      }
    }
    await new Promise<void>((resolve, reject) => {
      writeStream.end((err?: Error | null) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }
}
