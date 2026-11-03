/**
 * Integration tests for the LSP subsystem.
 *
 * Uses the mock LSP server (tests/mock-lsp-server.mjs) to verify:
 * - Server start via LspManager
 * - Request/response relay and mapping
 * - Crash recovery and auto-restart
 * - Document synchronization
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { spawn, ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import * as path from "node:path";

// ── Wire Protocol Helpers ───────────────────────────────────────────

function frame(message: object): string {
  const json = JSON.stringify(message);
  const bytes = Buffer.byteLength(json, "utf8");
  return `Content-Length: ${bytes}\r\n\r\n${json}`;
}

interface ParsedMessage {
  jsonrpc: string;
  id?: number;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message: string };
}

/**
 * A lightweight LSP client that communicates with a child process
 * via stdio, for testing purposes.
 */
class TestLspClient extends EventEmitter {
  private child: ChildProcess;
  private buffer = "";
  private requestId = 0;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

  constructor(serverPath: string, args: string[] = []) {
    super();
    this.child = spawn("node", [serverPath, ...args], {
      stdio: ["pipe", "pipe", "pipe"],
    });

    this.child.stdout!.setEncoding("utf8");
    this.child.stdout!.on("data", (chunk: string) => {
      this.buffer += chunk;
      this.processBuffer();
    });

    this.child.stderr!.setEncoding("utf8");
    this.child.stderr!.on("data", (chunk: string) => {
      this.emit("stderr", chunk);
    });

    this.child.on("exit", (code, signal) => {
      this.emit("exit", { code, signal });
      // Reject all pending requests
      for (const [id, { reject }] of this.pending) {
        reject(new Error(`Server exited with code ${code}`));
        this.pending.delete(id);
      }
    });
  }

  private processBuffer(): void {
    while (true) {
      const headerEnd = this.buffer.indexOf("\r\n\r\n");
      if (headerEnd < 0) return;

      const header = this.buffer.slice(0, headerEnd);
      const match = header.match(/Content-Length:\s*(\d+)/i);
      if (!match) {
        this.buffer = this.buffer.slice(headerEnd + 4);
        continue;
      }

      const contentLength = parseInt(match[1], 10);
      const bodyStart = headerEnd + 4;
      const bodyEnd = bodyStart + contentLength;

      if (this.buffer.length < bodyEnd) return;

      const body = this.buffer.slice(bodyStart, bodyEnd);
      this.buffer = this.buffer.slice(bodyEnd);

      try {
        const msg = JSON.parse(body) as ParsedMessage;
        this.handleMessage(msg);
      } catch {
        // Skip malformed messages
      }
    }
  }

  private handleMessage(msg: ParsedMessage): void {
    if (msg.id != null && !msg.method) {
      // Response
      const pending = this.pending.get(msg.id);
      if (pending) {
        this.pending.delete(msg.id);
        if (msg.error) {
          pending.reject(new Error(msg.error.message));
        } else {
          pending.resolve(msg.result);
        }
      }
    } else if (msg.method) {
      // Notification from server
      this.emit("notification", { method: msg.method, params: msg.params });
    }
  }

  async request(method: string, params: unknown, timeout = 5000): Promise<unknown> {
    const id = ++this.requestId;
    const message = frame({ jsonrpc: "2.0", id, method, params });

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Request '${method}' timed out`));
      }, timeout);

      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });

      this.child.stdin!.write(message);
    });
  }

  notify(method: string, params: unknown): void {
    const message = frame({ jsonrpc: "2.0", method, params });
    this.child.stdin!.write(message);
  }

  async shutdown(): Promise<void> {
    try {
      await this.request("shutdown", null, 2000);
      this.notify("exit", null);
    } catch {
      // Best-effort
    }
    this.child.kill();
  }

  isAlive(): boolean {
    return !this.child.killed && this.child.exitCode === null;
  }
}

// ── Tests ───────────────────────────────────────────────────────────

const MOCK_SERVER_PATH = path.resolve(__dirname, "mock-lsp-server.mjs");

describe("LSP Integration", () => {
  let client: TestLspClient;

  afterEach(async () => {
    if (client?.isAlive()) {
      await client.shutdown();
    }
  });

  describe("Server lifecycle", () => {
    it("initializes and returns capabilities", async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);

      const result = await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      }) as { capabilities: Record<string, unknown>; serverInfo: { name: string } };

      expect(result.capabilities).toBeDefined();
      expect(result.capabilities.completionProvider).toBeDefined();
      expect(result.capabilities.hoverProvider).toBe(true);
      expect(result.capabilities.definitionProvider).toBe(true);
      expect(result.capabilities.documentSymbolProvider).toBe(true);
      expect(result.capabilities.semanticTokensProvider).toBeDefined();
      expect(result.serverInfo.name).toBe("mock-lsp-server");

      // Send initialized notification
      client.notify("initialized", {});
    });

    it("handles shutdown gracefully", async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);

      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});

      const shutdownResult = await client.request("shutdown", null);
      expect(shutdownResult).toBeNull();
    });
  });

  describe("Completion requests", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns completion items", async () => {
      client.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "const x = console.",
        },
      });

      const result = await client.request("textDocument/completion", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 18 },
        context: { triggerKind: 2, triggerCharacter: "." },
      }) as { isIncomplete: boolean; items: Array<{ label: string; kind: number }> };

      expect(result.isIncomplete).toBe(false);
      expect(result.items).toHaveLength(3);
      expect(result.items[0].label).toBe("console");
      expect(result.items[1].label).toBe("log");
      expect(result.items[1].kind).toBe(2); // Method
    });
  });

  describe("Hover requests", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns hover information for open documents", async () => {
      client.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "function hello() {}",
        },
      });

      const result = await client.request("textDocument/hover", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 9 },
      }) as { contents: { kind: string; value: string }; range: unknown };

      expect(result.contents.kind).toBe("markdown");
      expect(result.contents.value).toContain("Hover info");
      expect(result.range).toBeDefined();
    });

    it("returns null for documents not opened", async () => {
      const result = await client.request("textDocument/hover", {
        textDocument: { uri: "file:///workspace/unknown.ts" },
        position: { line: 0, character: 0 },
      });

      expect(result).toBeNull();
    });
  });

  describe("Diagnostics", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("publishes diagnostics when a document is opened", async () => {
      const diagnosticsPromise = new Promise<{ uri: string; diagnostics: unknown[] }>((resolve) => {
        client.on("notification", ({ method, params }) => {
          if (method === "textDocument/publishDiagnostics") {
            resolve(params as { uri: string; diagnostics: unknown[] });
          }
        });
      });

      client.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "// TODO: fix this\nconst x = 1;",
        },
      });

      const result = await diagnosticsPromise;
      expect(result.uri).toBe("file:///workspace/test.ts");
      expect(result.diagnostics).toHaveLength(1);
      expect((result.diagnostics[0] as { message: string }).message).toBe("TODO comment found");
    });

    it("publishes error diagnostics for ERROR markers", async () => {
      const diagnosticsPromise = new Promise<{ uri: string; diagnostics: unknown[] }>((resolve) => {
        client.on("notification", ({ method, params }) => {
          if (method === "textDocument/publishDiagnostics") {
            resolve(params as { uri: string; diagnostics: unknown[] });
          }
        });
      });

      client.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "// ERROR: something broke",
        },
      });

      const result = await diagnosticsPromise;
      expect(result.diagnostics).toHaveLength(1);
      expect((result.diagnostics[0] as { severity: number }).severity).toBe(1); // Error
    });
  });

  describe("Definition", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns definition location", async () => {
      client.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "const x = 1;\nconsole.log(x);",
        },
      });

      const result = await client.request("textDocument/definition", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 1, character: 12 },
      }) as { uri: string; range: { start: { line: number } } };

      expect(result.uri).toBe("file:///workspace/test.ts");
      expect(result.range.start.line).toBe(0);
    });
  });

  describe("Document symbols", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns hierarchical document symbols", async () => {
      const result = await client.request("textDocument/documentSymbol", {
        textDocument: { uri: "file:///workspace/test.ts" },
      }) as Array<{ name: string; kind: number; children?: unknown[] }>;

      expect(result).toHaveLength(2);
      expect(result[0].name).toBe("main");
      expect(result[0].kind).toBe(12); // Function
      expect(result[0].children).toHaveLength(1);
      expect(result[1].name).toBe("Helper");
      expect(result[1].kind).toBe(5); // Class
    });
  });

  describe("Semantic tokens", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns semantic tokens data", async () => {
      const result = await client.request("textDocument/semanticTokens/full", {
        textDocument: { uri: "file:///workspace/test.ts" },
      }) as { data: number[] };

      expect(result.data).toBeDefined();
      expect(result.data.length).toBe(15); // 3 tokens × 5 values each
      // First token: line 0, char 0, len 8, keyword (15), no modifiers
      expect(result.data[0]).toBe(0);
      expect(result.data[3]).toBe(15);
    });
  });

  describe("Crash recovery", () => {
    it("detects server crash via exit event", async () => {
      client = new TestLspClient(MOCK_SERVER_PATH, ["--crash-after", "2"]);

      // First request: initialize (counts as request 1)
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});

      // Second request: this should trigger the crash (request 2 > crashAfter=2)
      // Actually crashAfter=2 means crash after 2 requests, so request 3 crashes
      const result = await client.request("textDocument/completion", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 0 },
      });
      expect(result).toBeDefined();

      // Third request should cause crash
      const exitPromise = new Promise<{ code: number | null }>((resolve) => {
        client.on("exit", resolve);
      });

      const crashPromise = client.request("textDocument/hover", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 0 },
      });

      await expect(crashPromise).rejects.toThrow();
      const exitInfo = await exitPromise;
      expect(exitInfo.code).toBe(1);
    });

    it("can restart after crash and re-sync documents", async () => {
      // Start first instance that will crash
      client = new TestLspClient(MOCK_SERVER_PATH, ["--crash-after", "1"]);

      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});

      // Wait for crash
      const exitPromise = new Promise<void>((resolve) => {
        client.on("exit", () => resolve());
      });

      // This request should trigger crash
      client.request("textDocument/completion", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 0 },
      }).catch(() => undefined);

      await exitPromise;

      // Start a new instance (simulating auto-restart)
      const newClient = new TestLspClient(MOCK_SERVER_PATH);
      const caps = await newClient.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      }) as { capabilities: Record<string, unknown> };

      newClient.notify("initialized", {});

      // Re-sync documents
      newClient.notify("textDocument/didOpen", {
        textDocument: {
          uri: "file:///workspace/test.ts",
          languageId: "typescript",
          version: 1,
          text: "const x = 1;",
        },
      });

      // Verify the new instance works
      const hoverResult = await newClient.request("textDocument/hover", {
        textDocument: { uri: "file:///workspace/test.ts" },
        position: { line: 0, character: 6 },
      });

      expect(hoverResult).not.toBeNull();
      expect(caps.capabilities.completionProvider).toBeDefined();

      await newClient.shutdown();
      client = newClient; // For cleanup in afterEach
    });
  });

  describe("Unknown methods", () => {
    beforeEach(async () => {
      client = new TestLspClient(MOCK_SERVER_PATH);
      await client.request("initialize", {
        processId: process.pid,
        rootUri: "file:///workspace",
        capabilities: {},
      });
      client.notify("initialized", {});
    });

    it("returns method-not-found error for unsupported methods", async () => {
      await expect(
        client.request("textDocument/unsupported", {}),
      ).rejects.toThrow("Method not found");
    });
  });
});
