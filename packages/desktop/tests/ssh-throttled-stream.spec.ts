/**
 * Tests for ThrottledTransform — verifies that the transform stream
 * correctly consumes tokens from the BandwidthThrottler for each chunk
 * and forwards data unchanged.
 */

import { describe, it, expect, vi } from "vitest";
import { Readable, Writable } from "node:stream";
import { ThrottledTransform } from "../electron/ssh-throttled-stream";
import type { BandwidthThrottler } from "../electron/ssh-bandwidth-throttler";

// ── Helpers ──────────────────────────────────────────────────────

function createMockThrottler(resolveImmediately = true): BandwidthThrottler {
  return {
    consumeTokens: vi.fn().mockImplementation(() =>
      resolveImmediately ? Promise.resolve() : new Promise(() => {}),
    ),
  } as unknown as BandwidthThrottler;
}

function collectOutput(stream: NodeJS.ReadableStream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on("data", (chunk: Buffer) => chunks.push(chunk));
    stream.on("end", () => resolve(Buffer.concat(chunks)));
    stream.on("error", reject);
  });
}

// ── Tests ────────────────────────────────────────────────────────

describe("ThrottledTransform", () => {
  it("forwards data unchanged after consuming tokens", async () => {
    const throttler = createMockThrottler();
    const transform = new ThrottledTransform({ throttler });

    const input = Buffer.from("hello world");
    const outputPromise = collectOutput(transform);

    transform.write(input);
    transform.end();

    const output = await outputPromise;
    expect(output).toEqual(input);
  });

  it("calls consumeTokens with the chunk length", async () => {
    const throttler = createMockThrottler();
    const transform = new ThrottledTransform({ throttler });

    const chunk1 = Buffer.alloc(1024, "a");
    const chunk2 = Buffer.alloc(512, "b");
    const outputPromise = collectOutput(transform);

    transform.write(chunk1);
    transform.write(chunk2);
    transform.end();

    await outputPromise;

    expect(throttler.consumeTokens).toHaveBeenCalledTimes(2);
    expect(throttler.consumeTokens).toHaveBeenCalledWith(1024);
    expect(throttler.consumeTokens).toHaveBeenCalledWith(512);
  });

  it("preserves chunk order across multiple writes", async () => {
    const throttler = createMockThrottler();
    const transform = new ThrottledTransform({ throttler });

    const chunks = [
      Buffer.from("first"),
      Buffer.from("second"),
      Buffer.from("third"),
    ];
    const outputPromise = collectOutput(transform);

    for (const chunk of chunks) {
      transform.write(chunk);
    }
    transform.end();

    const output = await outputPromise;
    expect(output).toEqual(Buffer.concat(chunks));
  });

  it("propagates errors from consumeTokens", async () => {
    const throttler = {
      consumeTokens: vi.fn().mockRejectedValue(new Error("throttler destroyed")),
    } as unknown as BandwidthThrottler;

    const transform = new ThrottledTransform({ throttler });

    const errorPromise = new Promise<Error>((resolve) => {
      transform.on("error", resolve);
    });

    transform.write(Buffer.from("data"));

    const err = await errorPromise;
    expect(err.message).toBe("throttler destroyed");
  });

  it("works when piped between readable and writable streams", async () => {
    const throttler = createMockThrottler();
    const transform = new ThrottledTransform({ throttler });

    const inputData = Buffer.from("piped data through throttle");
    const source = Readable.from([inputData]);
    const received: Buffer[] = [];

    const sink = new Writable({
      write(chunk, _encoding, callback) {
        received.push(chunk);
        callback();
      },
    });

    await new Promise<void>((resolve, reject) => {
      source.pipe(transform).pipe(sink);
      sink.on("finish", resolve);
      sink.on("error", reject);
    });

    expect(Buffer.concat(received)).toEqual(inputData);
    expect(throttler.consumeTokens).toHaveBeenCalledWith(inputData.length);
  });

  it("respects highWaterMark option", () => {
    const throttler = createMockThrottler();
    const transform = new ThrottledTransform({
      throttler,
      highWaterMark: 4096,
    });

    expect(transform.readableHighWaterMark).toBe(4096);
    expect(transform.writableHighWaterMark).toBe(4096);
  });
});
