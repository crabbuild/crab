/**
 * ThrottledTransform — A Node.js Transform stream that rate-limits data
 * flowing through it by consuming tokens from a BandwidthThrottler.
 *
 * Used to wrap SFTP read/write streams during file transfers so that
 * combined transfer throughput stays within the configured bandwidth limit.
 *
 * Interactive SSH channels (terminals, RPC calls) are NOT wrapped with
 * this transform — only SFTP transfer streams use it.
 */

import { Transform, type TransformCallback } from "node:stream";
import type { BandwidthThrottler } from "./ssh-bandwidth-throttler";

export interface ThrottledTransformOptions {
  /** The BandwidthThrottler instance that governs rate limiting. */
  throttler: BandwidthThrottler;
  /** Optional high water mark for the transform stream buffer. */
  highWaterMark?: number;
}

/**
 * A Transform stream that consumes tokens from a BandwidthThrottler
 * for each chunk passing through. This naturally rate-limits the stream
 * to the configured bandwidth.
 *
 * Pipe between an SFTP stream and a file stream during transfers:
 *   sftpReadStream.pipe(throttledTransform).pipe(localWriteStream)
 *   localReadStream.pipe(throttledTransform).pipe(sftpWriteStream)
 */
export class ThrottledTransform extends Transform {
  private readonly throttler: BandwidthThrottler;

  constructor(options: ThrottledTransformOptions) {
    super({ highWaterMark: options.highWaterMark });
    this.throttler = options.throttler;
  }

  override _transform(
    chunk: Buffer,
    _encoding: BufferEncoding,
    callback: TransformCallback,
  ): void {
    this.throttler
      .consumeTokens(chunk.length)
      .then(() => {
        this.push(chunk);
        callback();
      })
      .catch((err: unknown) => {
        callback(err instanceof Error ? err : new Error(String(err)));
      });
  }
}
