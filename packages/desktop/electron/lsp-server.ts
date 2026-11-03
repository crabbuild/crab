import { spawn, ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";

/**
 * Configuration for a language server process.
 */
export interface LspServerConfig {
  /** Language ID (matches Monaco language IDs). */
  language: string;
  /** Command to spawn. */
  command: string;
  /** Arguments. */
  args: string[];
  /** Optional environment variables. */
  env?: Record<string, string>;
  /** Initialization options passed to the server. */
  initializationOptions?: unknown;
}

/**
 * Minimal subset of LSP ServerCapabilities we care about relaying.
 */
export interface ServerCapabilities {
  completionProvider?: unknown;
  hoverProvider?: unknown;
  definitionProvider?: unknown;
  typeDefinitionProvider?: unknown;
  referencesProvider?: unknown;
  documentSymbolProvider?: unknown;
  codeActionProvider?: unknown;
  documentFormattingProvider?: unknown;
  documentRangeFormattingProvider?: unknown;
  renameProvider?: unknown;
  signatureHelpProvider?: unknown;
  textDocumentSync?: unknown;
  [key: string]: unknown;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

/**
 * Client capabilities declared to the language server during initialization.
 */
const CLIENT_CAPABILITIES = {
  textDocument: {
    synchronization: {
      dynamicRegistration: false,
      willSave: false,
      didSave: true,
      willSaveWaitUntil: false,
    },
    completion: {
      completionItem: {
        snippetSupport: true,
        commitCharactersSupport: true,
        documentationFormat: ["markdown", "plaintext"],
        resolveSupport: { properties: ["documentation", "detail", "additionalTextEdits"] },
      },
      contextSupport: true,
    },
    hover: {
      contentFormat: ["markdown", "plaintext"],
    },
    signatureHelp: {
      signatureInformation: {
        documentationFormat: ["markdown", "plaintext"],
        parameterInformation: { labelOffsetSupport: true },
      },
    },
    definition: { linkSupport: true },
    typeDefinition: { linkSupport: true },
    implementation: { linkSupport: true },
    references: {},
    documentHighlight: {},
    documentSymbol: { hierarchicalDocumentSymbolSupport: true },
    codeAction: {
      codeActionLiteralSupport: {
        codeActionKind: { valueSet: ["quickfix", "refactor", "source"] },
      },
      resolveSupport: { properties: ["edit"] },
    },
    codeLens: {},
    formatting: {},
    rangeFormatting: {},
    onTypeFormatting: {},
    rename: { prepareSupport: true },
    foldingRange: { lineFoldingOnly: true },
    inlayHint: {
      resolveSupport: { properties: ["tooltip", "textEdits", "label.tooltip"] },
    },
    linkedEditingRange: {},
    semanticTokens: {
      requests: { range: false, full: { delta: false } },
      tokenTypes: [
        "namespace", "type", "class", "enum", "interface", "struct",
        "typeParameter", "parameter", "variable", "property", "enumMember",
        "event", "function", "method", "macro", "keyword", "modifier",
        "comment", "string", "number", "regexp", "operator", "decorator",
      ],
      tokenModifiers: [
        "declaration", "definition", "readonly", "static", "deprecated",
        "abstract", "async", "modification", "documentation", "defaultLibrary",
      ],
      formats: ["relative"],
    },
    publishDiagnostics: {
      relatedInformation: true,
      tagSupport: { valueSet: [1, 2] },
    },
  },
  workspace: {
    workspaceFolders: true,
    didChangeConfiguration: { dynamicRegistration: false },
    // The renderer cannot answer server configuration requests. Advertising
    // support makes TypeScript wait indefinitely before returning completions.
    symbol: {
      symbolKind: { valueSet: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26] },
    },
  },
};

/**
 * Per-server process wrapper. Spawns a language server child process,
 * handles Content-Length framing/deframing on stdio, correlates request
 * IDs, and emits parsed JSON messages.
 *
 * Events:
 * - "message" (json: object) — any server → client message
 * - "stderr" (text: string) — server stderr output
 * - "crashed" ({ code, signal, stderr }) — unexpected exit
 */
export class LspServer extends EventEmitter {
  private child: ChildProcessWithoutNullStreams | null = null;
  private buffer = "";
  private contentLength = -1;
  private requestId = 1;
  private pending = new Map<number, PendingRequest>();
  private capabilities: ServerCapabilities | null = null;
  private recentStderr: string[] = [];
  private static readonly MAX_STDERR_LINES = 50;

  constructor(
    private config: LspServerConfig,
    private rootUri: string,
  ) {
    super();
  }

  /**
   * Spawn the language server, send `initialize` request with client
   * capabilities, await `InitializeResult`, then send `initialized`
   * notification.
   */
  async start(): Promise<ServerCapabilities> {
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      ...(this.config.env || {}),
    };

    this.child = spawn(this.config.command, this.config.args, {
      stdio: ["pipe", "pipe", "pipe"],
      env,
    });

    this.child.stdout.on("data", (chunk: Buffer) => {
      this.onStdoutData(chunk.toString("utf8"));
    });

    this.child.stderr.on("data", (chunk: Buffer) => {
      const text = chunk.toString("utf8");
      for (const line of text.split("\n")) {
        if (line.trim()) {
          this.recentStderr.push(line);
          if (this.recentStderr.length > LspServer.MAX_STDERR_LINES) {
            this.recentStderr.shift();
          }
        }
      }
      this.emit("stderr", text);
    });

    this.child.on("exit", (code, signal) => {
      this.child = null;
      // Reject all pending requests
      for (const p of this.pending.values()) {
        p.reject(new Error(`LSP server exited before response (code=${code})`));
      }
      this.pending.clear();
      this.emit("crashed", { code, signal, stderr: this.recentStderr.slice() });
    });

    // Send initialize request
    const initResult = (await this.request("initialize", {
      processId: process.pid,
      rootUri: this.rootUri,
      capabilities: CLIENT_CAPABILITIES,
      workspaceFolders: [{ uri: this.rootUri, name: "workspace" }],
      initializationOptions: this.config.initializationOptions ?? null,
    })) as { capabilities: ServerCapabilities };

    this.capabilities = initResult.capabilities;

    // Send initialized notification
    this.notify("initialized", {});

    return this.capabilities;
  }

  /**
   * Send a JSON-RPC request and await the response.
   */
  async request(method: string, params: unknown): Promise<unknown> {
    if (!this.child) throw new Error("LSP server not running");
    const id = this.requestId++;
    const message = { jsonrpc: "2.0", id, method, params };
    this.writeMessage(message);
    return new Promise<unknown>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
  }

  /**
   * Send a JSON-RPC notification (no response expected).
   */
  notify(method: string, params: unknown): void {
    if (!this.child) return;
    const message = { jsonrpc: "2.0", method, params };
    this.writeMessage(message);
  }

  /**
   * Forward a raw JSON string to the server's stdin with Content-Length framing.
   */
  send(rawJson: string): void {
    if (!this.child) return;
    const bytes = Buffer.byteLength(rawJson, "utf8");
    const header = `Content-Length: ${bytes}\r\n\r\n`;
    this.child.stdin.write(header + rawJson);
  }

  /**
   * Graceful shutdown: send `shutdown` request, await response, send
   * `exit` notification, kill process after 5s grace period.
   */
  async shutdown(): Promise<void> {
    if (!this.child) return;

    // Remove the crash handler so we don't emit "crashed" on graceful exit
    this.child.removeAllListeners("exit");
    const child = this.child;

    try {
      await this.request("shutdown", null);
    } catch {
      // Server may have already exited
    }

    this.notify("exit", null);

    // Grace period: kill after 5s if still alive
    const killTimer = setTimeout(() => {
      if (child && !child.killed) {
        child.kill("SIGKILL");
      }
    }, 5000);

    child.on("exit", () => {
      clearTimeout(killTimer);
    });

    // If the process is still alive after notify, wait for it
    if (!child.killed) {
      await new Promise<void>((resolve) => {
        child.on("exit", () => resolve());
        // Also resolve if already dead
        if (child.exitCode !== null) resolve();
      });
    }

    this.child = null;
    this.pending.clear();
  }

  /**
   * Get the server capabilities (available after start()).
   */
  getCapabilities(): ServerCapabilities | null {
    return this.capabilities;
  }

  /**
   * Check if the server process is running.
   */
  isRunning(): boolean {
    return this.child !== null;
  }

  // ── Content-Length framing ──────────────────────────────────────

  /**
   * Write a JSON-RPC message with Content-Length framing to stdin.
   */
  private writeMessage(message: object): void {
    if (!this.child) return;
    const json = JSON.stringify(message);
    const bytes = Buffer.byteLength(json, "utf8");
    const header = `Content-Length: ${bytes}\r\n\r\n`;
    this.child.stdin.write(header + json);
  }

  /**
   * Parse incoming stdout data using Content-Length framing.
   * Accumulates data in a buffer and extracts complete messages.
   */
  private onStdoutData(data: string): void {
    this.buffer += data;
    this.parseBuffer();
  }

  /**
   * Extract complete messages from the buffer.
   * Protocol: `Content-Length: N\r\n\r\n{json of N bytes}`
   */
  private parseBuffer(): void {
    while (true) {
      if (this.contentLength === -1) {
        // Look for the header separator
        const headerEnd = this.buffer.indexOf("\r\n\r\n");
        if (headerEnd === -1) return; // Need more data

        const header = this.buffer.slice(0, headerEnd);
        const match = header.match(/Content-Length:\s*(\d+)/i);
        if (!match) {
          // Malformed header — skip to after the separator and try again
          this.buffer = this.buffer.slice(headerEnd + 4);
          continue;
        }

        this.contentLength = parseInt(match[1], 10);
        this.buffer = this.buffer.slice(headerEnd + 4);
      }

      // We have a content-length; check if we have enough body bytes.
      // Content-Length is in bytes, but our buffer is a string. We need
      // to handle multi-byte characters correctly.
      const bodyBytes = Buffer.byteLength(this.buffer, "utf8");
      if (bodyBytes < this.contentLength) return; // Need more data

      // Extract exactly contentLength bytes from the buffer
      const body = Buffer.from(this.buffer, "utf8").slice(0, this.contentLength).toString("utf8");
      const remainingBuffer = Buffer.from(this.buffer, "utf8").slice(this.contentLength).toString("utf8");
      this.buffer = remainingBuffer;
      this.contentLength = -1;

      // Parse and dispatch
      let message: any;
      try {
        message = JSON.parse(body);
      } catch {
        continue; // Malformed JSON — skip
      }

      this.handleMessage(message);
    }
  }

  /**
   * Route a parsed message: correlate responses to pending requests,
   * emit all messages for relay to the renderer.
   */
  private handleMessage(message: any): void {
    // Emit every message for the relay layer
    this.emit("message", message);

    // Correlate responses to pending requests
    if ("id" in message && message.id !== undefined && message.id !== null) {
      const id = typeof message.id === "number" ? message.id : parseInt(message.id, 10);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        if ("error" in message) {
          const err = new Error(message.error?.message || "LSP error");
          (err as any).code = message.error?.code;
          (err as any).data = message.error?.data;
          pending.reject(err);
        } else {
          pending.resolve(message.result);
        }
      }
    }
  }
}
