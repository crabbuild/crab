#!/usr/bin/env node
/**
 * Mock LSP server for integration testing.
 *
 * Speaks the LSP wire protocol (Content-Length framed JSON-RPC) on stdio.
 * Responds to: initialize, textDocument/completion, textDocument/hover,
 * textDocument/publishDiagnostics (sent as notification), textDocument/definition.
 *
 * Usage: node tests/mock-lsp-server.mjs [--crash-after N]
 *
 * Options:
 *   --crash-after N   Exit with code 1 after N requests (for crash recovery tests)
 */

import { createInterface } from "node:readline";

// ── Configuration ───────────────────────────────────────────────────

const args = process.argv.slice(2);
let crashAfter = Infinity;
const crashIdx = args.indexOf("--crash-after");
if (crashIdx >= 0 && args[crashIdx + 1]) {
  crashAfter = parseInt(args[crashIdx + 1], 10);
}

// ── State ───────────────────────────────────────────────────────────

let initialized = false;
let requestCount = 0;
const openDocuments = new Map(); // uri → { text, version }

// ── Wire Protocol ───────────────────────────────────────────────────

let buffer = "";

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  processBuffer();
});

process.stdin.on("end", () => {
  process.exit(0);
});

function processBuffer() {
  while (true) {
    // Look for Content-Length header
    const headerEnd = buffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;

    const header = buffer.slice(0, headerEnd);
    const match = header.match(/Content-Length:\s*(\d+)/i);
    if (!match) {
      // Malformed — skip to next potential header
      buffer = buffer.slice(headerEnd + 4);
      continue;
    }

    const contentLength = parseInt(match[1], 10);
    const bodyStart = headerEnd + 4;
    const bodyEnd = bodyStart + contentLength;

    if (buffer.length < bodyEnd) {
      // Not enough data yet
      return;
    }

    const body = buffer.slice(bodyStart, bodyEnd);
    buffer = buffer.slice(bodyEnd);

    try {
      const message = JSON.parse(body);
      handleMessage(message);
    } catch (err) {
      process.stderr.write(`Mock LSP: failed to parse message: ${err.message}\n`);
    }
  }
}

function send(message) {
  const json = JSON.stringify(message);
  const byteLength = Buffer.byteLength(json, "utf8");
  const frame = `Content-Length: ${byteLength}\r\n\r\n${json}`;
  process.stdout.write(frame);
}

// ── Message Handling ────────────────────────────────────────────────

function handleMessage(msg) {
  // Check crash condition
  if ("id" in msg) {
    requestCount++;
    if (requestCount > crashAfter) {
      process.stderr.write("Mock LSP: simulating crash\n");
      process.exit(1);
    }
  }

  if ("id" in msg && msg.method) {
    // Request
    handleRequest(msg);
  } else if (msg.method && !("id" in msg)) {
    // Notification
    handleNotification(msg);
  }
}

function handleRequest(msg) {
  const { id, method, params } = msg;

  switch (method) {
    case "initialize":
      handleInitialize(id, params);
      break;
    case "shutdown":
      send({ jsonrpc: "2.0", id, result: null });
      break;
    case "textDocument/completion":
      handleCompletion(id, params);
      break;
    case "textDocument/hover":
      handleHover(id, params);
      break;
    case "textDocument/definition":
      handleDefinition(id, params);
      break;
    case "textDocument/documentSymbol":
      handleDocumentSymbol(id, params);
      break;
    case "textDocument/semanticTokens/full":
      handleSemanticTokens(id, params);
      break;
    default:
      // Method not found
      send({
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `Method not found: ${method}` },
      });
  }
}

function handleNotification(msg) {
  const { method, params } = msg;

  switch (method) {
    case "initialized":
      initialized = true;
      break;
    case "textDocument/didOpen":
      openDocuments.set(params.textDocument.uri, {
        text: params.textDocument.text,
        version: params.textDocument.version,
      });
      // Send diagnostics for the opened document
      sendDiagnostics(params.textDocument.uri);
      break;
    case "textDocument/didChange":
      if (openDocuments.has(params.textDocument.uri)) {
        const changes = params.contentChanges;
        if (changes && changes.length > 0) {
          openDocuments.set(params.textDocument.uri, {
            text: changes[changes.length - 1].text,
            version: params.textDocument.version,
          });
        }
      }
      break;
    case "textDocument/didClose":
      openDocuments.delete(params.textDocument.uri);
      break;
    case "textDocument/didSave":
      // Re-send diagnostics on save
      sendDiagnostics(params.textDocument.uri);
      break;
    case "exit":
      process.exit(0);
      break;
  }
}

// ── Request Handlers ────────────────────────────────────────────────

function handleInitialize(id, params) {
  send({
    jsonrpc: "2.0",
    id,
    result: {
      capabilities: {
        textDocumentSync: {
          openClose: true,
          change: 1, // Full sync
          save: { includeText: true },
        },
        completionProvider: {
          triggerCharacters: [".", ":"],
          resolveProvider: true,
        },
        hoverProvider: true,
        definitionProvider: true,
        referencesProvider: true,
        documentSymbolProvider: true,
        codeActionProvider: { resolveProvider: false },
        documentFormattingProvider: true,
        renameProvider: { prepareProvider: true },
        signatureHelpProvider: {
          triggerCharacters: ["(", ","],
          retriggerCharacters: [",", ")"],
        },
        semanticTokensProvider: {
          full: true,
          legend: {
            tokenTypes: [
              "namespace", "type", "class", "enum", "interface",
              "struct", "typeParameter", "parameter", "variable", "property",
              "enumMember", "event", "function", "method", "macro",
              "keyword", "modifier", "comment", "string", "number",
              "regexp", "operator", "decorator",
            ],
            tokenModifiers: [
              "declaration", "definition", "readonly", "static",
              "deprecated", "abstract", "async", "modification",
              "documentation", "defaultLibrary",
            ],
          },
        },
      },
      serverInfo: {
        name: "mock-lsp-server",
        version: "1.0.0",
      },
    },
  });
}

function handleCompletion(id, params) {
  const { position } = params;

  send({
    jsonrpc: "2.0",
    id,
    result: {
      isIncomplete: false,
      items: [
        {
          label: "console",
          kind: 6, // Variable
          detail: "Console object",
          documentation: { kind: "markdown", value: "The **console** object." },
          insertText: "console",
        },
        {
          label: "log",
          kind: 2, // Method
          detail: "(message: string) => void",
          insertText: "log(${1:message})",
          insertTextFormat: 2, // Snippet
        },
        {
          label: "toString",
          kind: 2, // Method
          detail: "() => string",
          insertText: "toString()",
        },
      ],
    },
  });
}

function handleHover(id, params) {
  const { textDocument, position } = params;
  const doc = openDocuments.get(textDocument.uri);

  if (!doc) {
    send({ jsonrpc: "2.0", id, result: null });
    return;
  }

  send({
    jsonrpc: "2.0",
    id,
    result: {
      contents: {
        kind: "markdown",
        value: `**Hover info** at line ${position.line}, char ${position.character}`,
      },
      range: {
        start: { line: position.line, character: position.character },
        end: { line: position.line, character: position.character + 5 },
      },
    },
  });
}

function handleDefinition(id, params) {
  const { textDocument, position } = params;

  send({
    jsonrpc: "2.0",
    id,
    result: {
      uri: textDocument.uri,
      range: {
        start: { line: 0, character: 0 },
        end: { line: 0, character: 10 },
      },
    },
  });
}

function handleDocumentSymbol(id, params) {
  send({
    jsonrpc: "2.0",
    id,
    result: [
      {
        name: "main",
        kind: 12, // Function
        range: { start: { line: 0, character: 0 }, end: { line: 5, character: 1 } },
        selectionRange: { start: { line: 0, character: 9 }, end: { line: 0, character: 13 } },
        children: [
          {
            name: "x",
            kind: 13, // Variable
            range: { start: { line: 1, character: 2 }, end: { line: 1, character: 12 } },
            selectionRange: { start: { line: 1, character: 6 }, end: { line: 1, character: 7 } },
          },
        ],
      },
      {
        name: "Helper",
        kind: 5, // Class
        range: { start: { line: 7, character: 0 }, end: { line: 15, character: 1 } },
        selectionRange: { start: { line: 7, character: 6 }, end: { line: 7, character: 12 } },
        children: [
          {
            name: "run",
            kind: 6, // Method
            range: { start: { line: 8, character: 2 }, end: { line: 10, character: 3 } },
            selectionRange: { start: { line: 8, character: 2 }, end: { line: 8, character: 5 } },
          },
        ],
      },
    ],
  });
}

function handleSemanticTokens(id, params) {
  // Return a simple set of semantic tokens
  // Format: [deltaLine, deltaStartChar, length, tokenType, tokenModifiers]
  send({
    jsonrpc: "2.0",
    id,
    result: {
      data: [
        0, 0, 8, 15, 0,   // line 0, char 0, len 8, keyword, no modifiers
        0, 9, 4, 12, 1,   // line 0, char 9, len 4, function, declaration
        1, 2, 3, 8, 4,    // line 1, char 2, len 3, variable, readonly
      ],
    },
  });
}

// ── Diagnostic Publishing ───────────────────────────────────────────

function sendDiagnostics(uri) {
  const doc = openDocuments.get(uri);
  if (!doc) return;

  const text = doc.text;
  const diagnostics = [];

  // Simple heuristic: flag lines containing "TODO" as warnings
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const todoIdx = lines[i].indexOf("TODO");
    if (todoIdx >= 0) {
      diagnostics.push({
        range: {
          start: { line: i, character: todoIdx },
          end: { line: i, character: todoIdx + 4 },
        },
        severity: 2, // Warning
        source: "mock-lsp",
        message: "TODO comment found",
        code: "todo-comment",
      });
    }

    // Flag "error" keyword as an error diagnostic
    const errorIdx = lines[i].indexOf("ERROR");
    if (errorIdx >= 0) {
      diagnostics.push({
        range: {
          start: { line: i, character: errorIdx },
          end: { line: i, character: errorIdx + 5 },
        },
        severity: 1, // Error
        source: "mock-lsp",
        message: "ERROR marker found",
        code: "error-marker",
      });
    }
  }

  // Send publishDiagnostics notification
  send({
    jsonrpc: "2.0",
    method: "textDocument/publishDiagnostics",
    params: { uri, diagnostics },
  });
}

// ── Startup ─────────────────────────────────────────────────────────

process.stderr.write("Mock LSP server started\n");
