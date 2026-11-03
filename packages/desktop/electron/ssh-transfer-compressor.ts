/**
 * Transfer Compressor — wraps file transfer streams with zstd
 * compression/decompression and provides Blake3 checksum verification.
 *
 * Skips compression for file types that are already compressed (archives,
 * images, video, audio). Uses zstd at compression level 3 for a good
 * balance of speed and ratio on typical source code and documents.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import type { Readable } from "node:stream";
import { PassThrough } from "node:stream";
import { CompressStream, DecompressStream } from "zstd-napi";
import { createHash as createBlake3 } from "blake3";

// ── Constants ────────────────────────────────────────────────────

const ZSTD_COMPRESSION_LEVEL = 3;

/**
 * Extensions for file types that are already compressed.
 * Attempting to compress these wastes CPU with negligible size reduction.
 */
const SKIP_EXTENSIONS = new Set([
  "zip", "gz", "bz2", "xz", "zst", "7z", "rar",
  "png", "jpg", "jpeg", "gif", "webp",
  "mp4", "webm", "mkv", "mp3", "aac", "flac",
]);

// ── Types ────────────────────────────────────────────────────────

export interface CompressedStream {
  stream: Readable;
  originalSize: number;
}

// ── TransferCompressor ───────────────────────────────────────────

export class TransferCompressor {
  /**
   * Determine whether compression should be applied to a file based on
   * its extension. Returns false for file types that are already compressed.
   */
  shouldCompress(filePath: string): boolean {
    const ext = path.extname(filePath).toLowerCase().replace(/^\./, "");
    if (ext === "") return true;
    return !SKIP_EXTENSIONS.has(ext);
  }

  /**
   * Wrap a readable stream with zstd compression at level 3.
   * Returns the compressed stream and an object whose `originalSize`
   * property reflects the total uncompressed bytes once the stream ends.
   */
  async compressStream(input: Readable): Promise<CompressedStream> {
    const state = { originalSize: 0 };

    const counter = new PassThrough({
      transform(chunk, _encoding, callback) {
        state.originalSize += chunk.length;
        callback(null, chunk);
      },
    });

    const zstdStream = new CompressStream({ compressionLevel: ZSTD_COMPRESSION_LEVEL });

    // Pipe input through the byte counter, then through zstd compression.
    input.pipe(counter).pipe(zstdStream);

    // Forward errors from upstream to the output stream.
    input.on("error", (err) => zstdStream.destroy(err));
    counter.on("error", (err) => zstdStream.destroy(err));

    return {
      stream: zstdStream,
      get originalSize() {
        return state.originalSize;
      },
    };
  }

  /**
   * Wrap a readable stream with zstd decompression.
   * The returned stream emits the original uncompressed data.
   */
  async decompressStream(input: Readable): Promise<Readable> {
    const zstdStream = new DecompressStream();

    input.pipe(zstdStream);
    input.on("error", (err) => zstdStream.destroy(err));

    return zstdStream;
  }

  /**
   * Verify that a file on disk matches an expected Blake3 hash.
   * The hash should be a hex-encoded string.
   */
  async verifyChecksum(filePath: string, expectedHash: string): Promise<boolean> {
    const hasher = createBlake3();
    const fileStream = fs.createReadStream(filePath);

    for await (const chunk of fileStream) {
      hasher.update(chunk);
    }

    const actualHash = hasher.digest("hex") as string;
    hasher.dispose();
    return actualHash === expectedHash.toLowerCase();
  }
}
