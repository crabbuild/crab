/**
 * Unit tests for CodexRpcClient (electron/codex-rpc.ts).
 *
 * Uses PassThrough streams to simulate stdin/stdout and vi.useFakeTimers()
 * for deterministic backpressure retry testing.
 *
 * Validates: Requirements CA6.1, CA6.4, CA9.5
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { PassThrough } from "node:stream";
import { CodexRpcClient } from "../electron/codex-rpc";

/** Helper: create a client wired to PassThrough streams. */
function createTestClient(opts?: Parameters<typeof CodexRpcClient.prototype.send>[2] & {
  requestTimeoutMs?: number;
  backpressure?: { attempts: number; capMs: number };
  logger?: (msg: string) => void;
}) {
  const stdin = new PassThrough();   // client writes here (we read to inspect)
  const stdout = new PassThrough();  // we write here to simulate server output

  const client = new CodexRpcClient(stdin, stdout, opts);

  return { client, stdin, stdout };
}

/** Read all frames written to stdin (client → server direction). */
function drainFrames(stdin: PassThrough): string[] {
  const buf = stdin.read();
  if (!buf) return [];
  return buf.toString().split("\n").filter((l: string) => l.length > 0);
}

/** Write a server response frame to stdout. */
function serverWrite(stdout: PassThrough, msg: object): void {
  stdout.write(JSON.stringify(msg) + "\n");
}

describe("CodexRpcClient", () => {
  describe("send()", () => {
    it("resolves on matching response", async () => {
      const { client, stdin, stdout } = createTestClient();

      const promise = client.send("initialize", { clientInfo: { name: "test" } });

      // Read the frame the client wrote
      await new Promise(r => setTimeout(r, 0));
      const frames = drainFrames(stdin);
      expect(frames.length).toBe(1);
      const sent = JSON.parse(frames[0]);
      expect(sent.method).toBe("initialize");
      expect(sent.id).toBeDefined();
      expect(sent.params).toEqual({ clientInfo: { name: "test" } });

      // Simulate server response
      serverWrite(stdout, { id: sent.id, result: { serverInfo: { name: "codex" } } });

      const result = await promise;
      expect(result).toEqual({ serverInfo: { name: "codex" } });

      client.close();
    });

    it("rejects on per-request timeout", async () => {
      vi.useFakeTimers();
      try {
        const { client } = createTestClient({ requestTimeoutMs: 500 });

        const promise = client.send("initialize");

        // Advance past the timeout
        vi.advanceTimersByTime(501);

        await expect(promise).rejects.toThrow("request timeout: initialize");

        client.close();
      } finally {
        vi.useRealTimers();
      }
    });

    it("retries on -32001 (backpressure)", async () => {
      vi.useFakeTimers();
      try {
        const { client, stdin, stdout } = createTestClient({
          requestTimeoutMs: 30_000,
          backpressure: { attempts: 3, capMs: 8000 },
        });

        const promise = client.send("turn/start", { input: [] });

        // Drain the initial request
        await vi.advanceTimersByTimeAsync(0);
        let frames = drainFrames(stdin);
        expect(frames.length).toBe(1);
        const firstSent = JSON.parse(frames[0]);

        // Server responds with -32001 (backpressure)
        serverWrite(stdout, {
          id: firstSent.id,
          error: { code: -32001, message: "Server overloaded; retry later." },
        });

        // Allow the retry timer to fire (exponential backoff)
        await vi.advanceTimersByTimeAsync(10_000);

        // Drain the retry request
        frames = drainFrames(stdin);
        expect(frames.length).toBeGreaterThanOrEqual(1);
        const retrySent = JSON.parse(frames[frames.length - 1]);
        expect(retrySent.method).toBe("turn/start");

        // Server responds successfully this time
        serverWrite(stdout, { id: retrySent.id, result: { turnId: "turn_1" } });

        const result = await promise;
        expect(result).toEqual({ turnId: "turn_1" });

        client.close();
      } finally {
        vi.useRealTimers();
      }
    });

    it("rejects after 3 failed retries; last error code preserved", async () => {
      vi.useFakeTimers();
      try {
        const { client, stdin, stdout } = createTestClient({
          requestTimeoutMs: 30_000,
          backpressure: { attempts: 3, capMs: 8000 },
        });

        const promise = client.send("turn/start", { input: [] });
        // Prevent unhandled rejection warnings — we assert on it below
        promise.catch(() => {});

        // Helper: advance time, drain the frame, respond with -32001
        async function rejectNextRequest() {
          await vi.advanceTimersByTimeAsync(10_000);
          const frames = drainFrames(stdin);
          if (frames.length > 0) {
            const sent = JSON.parse(frames[frames.length - 1]);
            serverWrite(stdout, {
              id: sent.id,
              error: { code: -32001, message: "Server overloaded; retry later." },
            });
          }
        }

        // Initial request
        await vi.advanceTimersByTimeAsync(0);
        let frames = drainFrames(stdin);
        expect(frames.length).toBe(1);
        const firstSent = JSON.parse(frames[0]);
        serverWrite(stdout, {
          id: firstSent.id,
          error: { code: -32001, message: "Server overloaded; retry later." },
        });

        // Retry 1
        await rejectNextRequest();
        // Retry 2
        await rejectNextRequest();
        // Retry 3 (exhaustion)
        await rejectNextRequest();

        await expect(promise).rejects.toMatchObject({
          message: "Server overloaded; retry later.",
          code: -32001,
        });

        client.close();
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("notify()", () => {
    it("writes a frame without id", () => {
      const { client, stdin } = createTestClient();

      client.notify("initialized", { ok: true });

      const frames = drainFrames(stdin);
      expect(frames.length).toBe(1);
      const sent = JSON.parse(frames[0]);
      expect(sent.method).toBe("initialized");
      expect(sent.params).toEqual({ ok: true });
      expect(sent).not.toHaveProperty("id");

      client.close();
    });
  });

  describe("onNotification()", () => {
    it("dispatches by method", async () => {
      const { client, stdout } = createTestClient();

      const received: unknown[] = [];
      client.onNotification("thread/started", (params) => {
        received.push(params);
      });

      serverWrite(stdout, { method: "thread/started", params: { thread: { id: "thr_1" } } });

      // Allow readline to process
      await new Promise(r => setTimeout(r, 10));

      expect(received).toHaveLength(1);
      expect(received[0]).toEqual({ thread: { id: "thr_1" } });

      client.close();
    });

    it("does not dispatch to unregistered methods", async () => {
      const { client, stdout } = createTestClient();

      const received: unknown[] = [];
      client.onNotification("thread/started", (params) => {
        received.push(params);
      });

      // Send a notification for a different method
      serverWrite(stdout, { method: "turn/started", params: { turn: { id: "t_1" } } });

      await new Promise(r => setTimeout(r, 10));

      expect(received).toHaveLength(0);

      client.close();
    });
  });

  describe("onServerRequest()", () => {
    it("fires for messages with both id and method", async () => {
      const { client, stdout } = createTestClient();

      const requests: Array<{ id: number; method: string; params: unknown }> = [];
      client.onServerRequest((id, method, params) => {
        requests.push({ id, method, params });
      });

      serverWrite(stdout, {
        id: 42,
        method: "item/commandExecution/requestApproval",
        params: { command: "rm -rf /tmp/test", reason: "Potentially destructive" },
      });

      await new Promise(r => setTimeout(r, 10));

      expect(requests).toHaveLength(1);
      expect(requests[0]).toEqual({
        id: 42,
        method: "item/commandExecution/requestApproval",
        params: { command: "rm -rf /tmp/test", reason: "Potentially destructive" },
      });

      client.close();
    });
  });

  describe("respond()", () => {
    it("writes the correct frame", () => {
      const { client, stdin } = createTestClient();

      client.respond(42, { decision: "acceptForSession" });

      const frames = drainFrames(stdin);
      expect(frames.length).toBe(1);
      const sent = JSON.parse(frames[0]);
      expect(sent).toEqual({ id: 42, result: { decision: "acceptForSession" } });

      client.close();
    });
  });

  describe("close()", () => {
    it("rejects all pending requests with 'transport closed'", async () => {
      const { client } = createTestClient({ requestTimeoutMs: 60_000 });

      const p1 = client.send("initialize");
      const p2 = client.send("thread/start");

      client.close();

      await expect(p1).rejects.toThrow("transport closed");
      await expect(p2).rejects.toThrow("transport closed");
    });

    it("is idempotent", () => {
      const { client } = createTestClient();

      // Should not throw on multiple close() calls
      client.close();
      client.close();
      client.close();
    });

    it("send() rejects immediately after close", async () => {
      const { client } = createTestClient();

      client.close();

      await expect(client.send("anything")).rejects.toThrow("transport closed");
    });
  });

  describe("malformed lines", () => {
    it("skips malformed lines on stdout without crashing", async () => {
      const logs: string[] = [];
      const { client, stdout } = createTestClient({
        logger: (msg) => logs.push(msg),
      });

      // Write garbage
      stdout.write("this is not json\n");
      stdout.write("{incomplete json\n");
      stdout.write("\n");

      await new Promise(r => setTimeout(r, 10));

      // Client should still be functional
      const received: unknown[] = [];
      client.onNotification("test/ping", (params) => {
        received.push(params);
      });

      serverWrite(stdout, { method: "test/ping", params: { ok: true } });
      await new Promise(r => setTimeout(r, 10));

      expect(received).toHaveLength(1);
      expect(received[0]).toEqual({ ok: true });

      // Malformed lines should have been logged
      expect(logs.some(l => l.includes("malformed"))).toBe(true);

      client.close();
    });
  });

  describe("out-of-order responses", () => {
    it("logs and drops responses with no matching pending request", async () => {
      const logs: string[] = [];
      const { client, stdout } = createTestClient({
        logger: (msg) => logs.push(msg),
      });

      // Send a response for an id that was never requested
      serverWrite(stdout, { id: 999, result: { unexpected: true } });

      await new Promise(r => setTimeout(r, 10));

      // Should have logged the out-of-order response
      expect(logs.some(l => l.includes("out-of-order") && l.includes("999"))).toBe(true);

      client.close();
    });
  });
});
