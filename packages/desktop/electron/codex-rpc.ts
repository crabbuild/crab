/**
 * Minimal JSON-RPC 2.0 client over newline-delimited stdio.
 *
 * Frame format: `JSON.stringify(msg) + "\n"` per the Codex App Server
 * protocol, which omits the `jsonrpc` header on the wire.
 * Contract: https://learn.chatgpt.com/docs/app-server#protocol
 *
 * Responsibilities:
 *   - Correlate responses to their `send()` promises.
 *   - Dispatch server-initiated notifications to per-method handlers.
 *   - Dispatch server-initiated requests (those carrying both `id`
 *     and `method`) to a single handler the adapter wires up.
 *   - Retry requests rejected with code `-32001` (server overloaded).
 *   - Reject all pending promises on `close()`.
 */

import { createInterface, type Interface as ReadlineInterface } from "node:readline";
import type { Writable, Readable } from "node:stream";

interface PendingRequest {
  id: number;
  method: string;
  params: object | undefined;
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout> | undefined;
  timeoutMs: number;
  retryCount: number;
}

export type CodexRequestId = number | string;

export class CodexRpcError extends Error {
  constructor(message: string, readonly code: number) { super(message); }
}

interface CodexRequestOptions {
  /** Zero disables the local timer for long-running server operations. */
  timeoutMs?: number;
}

export interface BackpressureConfig {
  attempts: number;
  capMs: number;
}

export interface CodexRpcClientOpts {
  /** Per-request timeout in ms. Default 30000. */
  requestTimeoutMs?: number;
  /** Backpressure retry config. Default { attempts: 3, capMs: 8000 }. */
  backpressure?: BackpressureConfig;
  /** Logger for debug messages. Default: no-op. */
  logger?: (msg: string) => void;
}

export class CodexRpcClient {
  private stdin: Writable;
  private rl: ReadlineInterface;
  private nextId = 1;
  private pending = new Map<number, PendingRequest>();
  private notificationHandlers = new Map<string, (params: unknown) => void>();
  private serverRequestHandler: ((id: CodexRequestId, method: string, params: unknown) => void) | null = null;
  private unhandledNotificationHandler: ((method: string, params: unknown) => void) | null = null;
  private closed = false;
  private closeHandler: (() => void) | null = null;
  private requestTimeoutMs: number;
  private backpressure: BackpressureConfig;
  private log: (msg: string) => void;

  constructor(stdin: Writable, stdout: Readable, opts?: CodexRpcClientOpts) {
    this.stdin = stdin;
    this.requestTimeoutMs = opts?.requestTimeoutMs ?? 30_000;
    this.backpressure = opts?.backpressure ?? { attempts: 3, capMs: 8000 };
    this.log = opts?.logger ?? (() => {});

    this.rl = createInterface({ input: stdout });
    this.rl.on("line", (line: string) => this.handleLine(line));
    this.rl.on("close", () => this.close());
    stdin.on("error", () => this.close());
    stdout.on("error", () => this.close());
  }

  /** Send a request and await the response. */
  send(method: string, params?: object, options?: CodexRequestOptions): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error("transport closed"));
    }

    const id = this.nextId++;
    const frame = JSON.stringify({ id, method, params }) + "\n";
    const timeoutMs = options?.timeoutMs ?? this.requestTimeoutMs;

    return new Promise<unknown>((resolve, reject) => {
      const timer = timeoutMs > 0 ? setTimeout(() => {
        const entry = this.pending.get(id);
        if (entry) {
          this.pending.delete(id);
          reject(new Error(`request timeout: ${method}`));
        }
      }, timeoutMs) : undefined;

      const entry: PendingRequest = {
        id,
        method,
        params,
        resolve,
        reject,
        timer,
        timeoutMs,
        retryCount: 0,
      };
      this.pending.set(id, entry);
      this.stdin.write(frame);
    });
  }

  /** Send a notification (no response expected). */
  notify(method: string, params?: object): void {
    if (this.closed) return;
    const frame = JSON.stringify({ method, params }) + "\n";
    this.stdin.write(frame);
  }

  /** Register a handler for server-initiated notifications. */
  onNotification(method: string, handler: (params: unknown) => void): void {
    this.notificationHandlers.set(method, handler);
  }

  /**
   * Register the handler for server-initiated requests (messages with
   * both `id` and `method`). Exactly one handler per client.
   */
  onServerRequest(handler: (id: CodexRequestId, method: string, params: unknown) => void): void {
    this.serverRequestHandler = handler;
  }

  /**
   * Register a fallback handler for notifications that have no
   * per-method handler registered. Used by the adapter to log
   * unknown notification methods at debug level (CA7.3).
   */
  onUnhandledNotification(handler: (method: string, params: unknown) => void): void {
    this.unhandledNotificationHandler = handler;
  }

  onClose(handler: () => void): void { this.closeHandler = handler; }

  /** Respond to a server-initiated request. */
  respond(id: CodexRequestId, result: object): boolean {
    if (this.closed) return false;
    const frame = JSON.stringify({ id, result }) + "\n";
    this.stdin.write(frame);
    return true;
  }

  /** Respond to a server-initiated request with an error. */
  respondError(id: CodexRequestId, code: number, message: string): void {
    if (this.closed) return;
    const frame = JSON.stringify({ id, error: { code, message } }) + "\n";
    this.stdin.write(frame);
  }

  /**
   * Close the transport: reject all pending request promises with a
   * "transport closed" error, destroy the readline interface, and
   * end stdin. Idempotent.
   */
  close(): void {
    if (this.closed) return;
    this.closed = true;

    const err = new Error("transport closed");
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(err);
    }
    this.pending.clear();

    this.rl.close();
    this.stdin.end();
    this.closeHandler?.();
  }

  private handleLine(line: string): void {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      this.log(`malformed line on stdout: ${line}`);
      return;
    }

    if (msg === null || typeof msg !== "object") {
      this.log(`malformed line on stdout: ${line}`);
      return;
    }

    // Server-initiated request: has both `id` and `method`
    if ("id" in msg && "method" in msg) {
      if (this.serverRequestHandler) {
        this.serverRequestHandler(msg.id, msg.method, msg.params);
      }
      return;
    }

    // Response to a client request: has `id` but no `method`
    if ("id" in msg && !("method" in msg)) {
      const entry = this.pending.get(msg.id);
      if (!entry) {
        this.log(`out-of-order response for id ${msg.id}`);
        return;
      }

      // Check for backpressure error (-32001)
      if (msg.error && msg.error.code === -32001) {
        if (entry.retryCount < this.backpressure.attempts) {
          this.retryRequest(entry);
          return;
        }
        // Exhausted retries — reject with the server error
        clearTimeout(entry.timer);
        this.pending.delete(msg.id);
        entry.reject(new CodexRpcError(msg.error.message ?? "server overloaded", msg.error.code));
        return;
      }

      // Any other error
      if (msg.error) {
        clearTimeout(entry.timer);
        this.pending.delete(msg.id);
        entry.reject(new CodexRpcError(msg.error.message ?? "rpc error", msg.error.code));
        return;
      }

      // Success
      clearTimeout(entry.timer);
      this.pending.delete(msg.id);
      entry.resolve(msg.result);
      return;
    }

    // Notification from server: has `method` but no `id`
    if ("method" in msg && !("id" in msg)) {
      const handler = this.notificationHandlers.get(msg.method);
      if (handler) {
        try {
          this.log(`notif ${msg.method} ${JSON.stringify(msg.params).slice(0, 500)}`);
        } catch {
          this.log(`notif ${msg.method} <unserialisable params>`);
        }
        handler(msg.params);
      } else if (this.unhandledNotificationHandler) {
        this.unhandledNotificationHandler(msg.method, msg.params);
      }
      return;
    }

    this.log(`unrecognized message: ${line}`);
  }

  private retryRequest(entry: PendingRequest): void {
    // Keep the request tracked during backoff so close() rejects it immediately.
    clearTimeout(entry.timer);

    entry.retryCount++;

    // Exponential backoff with jitter, capped at capMs
    const baseDelay = Math.min(
      1000 * Math.pow(2, entry.retryCount - 1),
      this.backpressure.capMs,
    );
    const jitter = Math.random() * baseDelay * 0.5;
    const delay = Math.min(baseDelay + jitter, this.backpressure.capMs);

    entry.timer = setTimeout(() => {
      if (this.closed) {
        entry.reject(new Error("transport closed"));
        return;
      }

      // Assign a new id for the retry
      this.pending.delete(entry.id);
      const newId = this.nextId++;
      entry.id = newId;

      const timer = entry.timeoutMs > 0 ? setTimeout(() => {
        const e = this.pending.get(newId);
        if (e) {
          this.pending.delete(newId);
          e.reject(new Error(`request timeout: ${e.method}`));
        }
      }, entry.timeoutMs) : undefined;
      entry.timer = timer;

      this.pending.set(newId, entry);
      const frame = JSON.stringify({ id: newId, method: entry.method, params: entry.params }) + "\n";
      this.stdin.write(frame);
    }, delay);
  }
}
