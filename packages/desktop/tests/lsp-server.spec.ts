/**
 * Unit tests for the LSP server wrapper.
 *
 * Tests Content-Length framing/deframing, request correlation,
 * and crash detection using a mock child process.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { EventEmitter } from "node:events";
import { Readable, Writable } from "node:stream";

// We need to mock child_process.spawn before importing LspServer
vi.mock("node:child_process", () => {
  return {
    spawn: vi.fn(),
  };
});

import { spawn } from "node:child_process";
import { LspServer, LspServerConfig } from "../electron/lsp-server";

const mockedSpawn = vi.mocked(spawn);

/**
 * Create a mock child process with controllable stdin/stdout/stderr.
 */
function createMockChild() {
  const stdout = new Readable({ read() {} });
  const stderr = new Readable({ read() {} });
  const stdin = new Writable({
    write(_chunk, _encoding, callback) {
      callback();
    },
  });

  const child = Object.assign(new EventEmitter(), {
    stdout,
    stderr,
    stdin,
    killed: false,
    exitCode: null as number | null,
    kill: vi.fn(function (this: any) {
      this.killed = true;
    }),
    pid: 12345,
  });

  return child;
}

/**
 * Frame a JSON message with Content-Length header (LSP wire protocol).
 */
function frame(message: object): string {
  const json = JSON.stringify(message);
  const bytes = Buffer.byteLength(json, "utf8");
  return `Content-Length: ${bytes}\r\n\r\n${json}`;
}

/**
 * Extract the JSON body from a Content-Length framed message string.
 */
function deframe(data: string): object | null {
  const match = data.match(/Content-Length:\s*(\d+)\r\n\r\n([\s\S]*)/);
  if (!match) return null;
  const length = parseInt(match[1], 10);
  const body = match[2].slice(0, length);
  return JSON.parse(body);
}

describe("LspServer", () => {
  let mockChild: ReturnType<typeof createMockChild>;
  let config: LspServerConfig;

  beforeEach(() => {
    vi.clearAllMocks();
    mockChild = createMockChild();
    mockedSpawn.mockReturnValue(mockChild as any);

    config = {
      language: "typescript",
      command: "typescript-language-server",
      args: ["--stdio"],
    };
  });

  describe("Content-Length framing", () => {
    it("frames outgoing messages with Content-Length header", async () => {
      const server = new LspServer(config, "file:///workspace");
      const written: string[] = [];

      mockChild.stdin = new Writable({
        write(chunk, _encoding, callback) {
          written.push(chunk.toString());
          callback();
        },
      });

      // Simulate spawn
      const startPromise = server.start();

      // The server should have sent an initialize request
      // Wait a tick for the write to happen
      await new Promise((r) => setTimeout(r, 10));

      expect(written.length).toBeGreaterThan(0);
      const firstMessage = deframe(written[0]);
      expect(firstMessage).not.toBeNull();
      expect((firstMessage as any).method).toBe("initialize");
      expect((firstMessage as any).jsonrpc).toBe("2.0");

      // Respond with InitializeResult
      const response = frame({
        jsonrpc: "2.0",
        id: (firstMessage as any).id,
        result: { capabilities: { completionProvider: { triggerCharacters: ["."] } } },
      });
      mockChild.stdout.push(response);

      const capabilities = await startPromise;
      expect(capabilities.completionProvider).toEqual({ triggerCharacters: ["."] });
    });

    it("deframes incoming messages correctly", async () => {
      const server = new LspServer(config, "file:///workspace");
      const messages: object[] = [];
      server.on("message", (msg) => messages.push(msg));

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Respond to initialize
      const initResponse = frame({
        jsonrpc: "2.0",
        id: 1,
        result: { capabilities: {} },
      });
      mockChild.stdout.push(initResponse);
      await startPromise;

      // Send a notification from the server
      const notification = frame({
        jsonrpc: "2.0",
        method: "textDocument/publishDiagnostics",
        params: { uri: "file:///test.ts", diagnostics: [] },
      });
      mockChild.stdout.push(notification);

      await new Promise((r) => setTimeout(r, 10));

      // Should have received the initialize response + notification
      expect(messages.length).toBe(2);
      expect((messages[1] as any).method).toBe("textDocument/publishDiagnostics");
    });

    it("handles messages split across multiple chunks", async () => {
      const server = new LspServer(config, "file:///workspace");
      const messages: object[] = [];
      server.on("message", (msg) => messages.push(msg));

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Send initialize response split across chunks
      const fullFrame = frame({
        jsonrpc: "2.0",
        id: 1,
        result: { capabilities: { hoverProvider: true } },
      });

      // Split in the middle
      const mid = Math.floor(fullFrame.length / 2);
      mockChild.stdout.push(fullFrame.slice(0, mid));
      await new Promise((r) => setTimeout(r, 5));
      mockChild.stdout.push(fullFrame.slice(mid));

      const capabilities = await startPromise;
      expect(capabilities.hoverProvider).toBe(true);
    });

    it("handles multiple messages in a single chunk", async () => {
      const server = new LspServer(config, "file:///workspace");
      const messages: object[] = [];
      server.on("message", (msg) => messages.push(msg));

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Send initialize response
      const initFrame = frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } });
      mockChild.stdout.push(initFrame);
      await startPromise;

      // Send two notifications concatenated in one chunk
      const notif1 = frame({
        jsonrpc: "2.0",
        method: "window/logMessage",
        params: { type: 3, message: "hello" },
      });
      const notif2 = frame({
        jsonrpc: "2.0",
        method: "window/logMessage",
        params: { type: 3, message: "world" },
      });
      mockChild.stdout.push(notif1 + notif2);

      await new Promise((r) => setTimeout(r, 10));

      // init response + 2 notifications = 3 messages
      expect(messages.length).toBe(3);
      expect((messages[1] as any).params.message).toBe("hello");
      expect((messages[2] as any).params.message).toBe("world");
    });

    it("handles multi-byte UTF-8 characters correctly", async () => {
      const server = new LspServer(config, "file:///workspace");
      const messages: object[] = [];
      server.on("message", (msg) => messages.push(msg));

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Send initialize response
      const initFrame = frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } });
      mockChild.stdout.push(initFrame);
      await startPromise;

      // Send a message with multi-byte characters (emoji, CJK)
      const payload = { jsonrpc: "2.0", method: "test", params: { text: "日本語テスト 🎉" } };
      const framedMsg = frame(payload);
      mockChild.stdout.push(framedMsg);

      await new Promise((r) => setTimeout(r, 10));

      expect(messages.length).toBe(2);
      expect((messages[1] as any).params.text).toBe("日本語テスト 🎉");
    });
  });

  describe("request correlation", () => {
    it("correlates responses to pending requests by ID", async () => {
      const server = new LspServer(config, "file:///workspace");

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Respond to initialize (id=1)
      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } }));
      await startPromise;

      // Send two requests
      const req1 = server.request("textDocument/hover", { position: { line: 0, character: 5 } });
      const req2 = server.request("textDocument/completion", { position: { line: 1, character: 3 } });

      await new Promise((r) => setTimeout(r, 10));

      // Respond out of order (id=3 before id=2)
      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 3, result: { items: ["a", "b"] } }));
      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 2, result: { contents: "hover info" } }));

      const [result1, result2] = await Promise.all([req1, req2]);
      expect(result1).toEqual({ contents: "hover info" });
      expect(result2).toEqual({ items: ["a", "b"] });
    });

    it("rejects pending requests on error responses", async () => {
      const server = new LspServer(config, "file:///workspace");

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } }));
      await startPromise;

      const reqPromise = server.request("textDocument/definition", {});
      await new Promise((r) => setTimeout(r, 10));

      mockChild.stdout.push(frame({
        jsonrpc: "2.0",
        id: 2,
        error: { code: -32601, message: "Method not found" },
      }));

      await expect(reqPromise).rejects.toThrow("Method not found");
    });
  });

  describe("crash detection", () => {
    it("emits 'crashed' with exit code and stderr on unexpected exit", async () => {
      const server = new LspServer(config, "file:///workspace");
      const crashes: any[] = [];
      server.on("crashed", (info) => crashes.push(info));

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Send some stderr before crashing
      mockChild.stderr.push("Error: something went wrong\n");
      mockChild.stderr.push("Segfault at 0xdeadbeef\n");

      // Simulate crash
      mockChild.emit("exit", 1, null);

      // The initialize request should be rejected
      await expect(startPromise).rejects.toThrow();

      expect(crashes.length).toBe(1);
      expect(crashes[0].code).toBe(1);
      expect(crashes[0].stderr).toContain("Error: something went wrong");
      expect(crashes[0].stderr).toContain("Segfault at 0xdeadbeef");
    });

    it("rejects all pending requests on crash", async () => {
      const server = new LspServer(config, "file:///workspace");

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      // Respond to initialize
      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } }));
      await startPromise;

      // Send a request
      const reqPromise = server.request("textDocument/hover", {});
      await new Promise((r) => setTimeout(r, 10));

      // Crash
      mockChild.emit("exit", 139, "SIGSEGV");

      await expect(reqPromise).rejects.toThrow("LSP server exited before response");
    });
  });

  describe("shutdown", () => {
    it("sends shutdown request then exit notification", async () => {
      const server = new LspServer(config, "file:///workspace");
      const written: string[] = [];

      mockChild.stdin = new Writable({
        write(chunk, _encoding, callback) {
          written.push(chunk.toString());
          callback();
        },
      });

      const startPromise = server.start();
      await new Promise((r) => setTimeout(r, 10));

      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: 1, result: { capabilities: {} } }));
      await startPromise;

      // Start shutdown
      const shutdownPromise = server.shutdown();
      await new Promise((r) => setTimeout(r, 10));

      // Find the shutdown request in written data
      const shutdownMsg = written.find((w) => {
        const parsed = deframe(w);
        return parsed && (parsed as any).method === "shutdown";
      });
      expect(shutdownMsg).toBeDefined();

      // Respond to shutdown
      const shutdownParsed = deframe(shutdownMsg!) as any;
      mockChild.stdout.push(frame({ jsonrpc: "2.0", id: shutdownParsed.id, result: null }));
      await new Promise((r) => setTimeout(r, 10));

      // Simulate process exit
      mockChild.emit("exit", 0, null);

      await shutdownPromise;

      // Verify exit notification was sent
      const exitMsg = written.find((w) => {
        const parsed = deframe(w);
        return parsed && (parsed as any).method === "exit";
      });
      expect(exitMsg).toBeDefined();
    });
  });
});
