import { it, expect } from "vitest";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import * as path from "node:path";
import { pathToFileURL } from "node:url";
import { LspServer } from "../electron/lsp-server";

it("relays real TypeScript completions through the production LSP server", async () => {
  const require = createRequire(import.meta.url);
  const entry = path.join(path.dirname(require.resolve("typescript-language-server/package.json")), "lib/cli.mjs");
  const root = mkdtempSync(path.join(tmpdir(), "crab-lsp-"));
  const file = path.join(root, "completion.ts");
  const text = 'const message = "hello";\nmessage.';
  writeFileSync(file, text);
  const server = new LspServer({
    language: "typescript",
    command: process.execPath,
    args: [entry, "--stdio"],
    env: { ELECTRON_RUN_AS_NODE: "1" },
    initializationOptions: { tsserver: { path: require.resolve("typescript/lib/tsserver.js") } },
  }, pathToFileURL(root).href);
  try {
    await server.start();
    const uri = pathToFileURL(file).href;
    server.notify("textDocument/didOpen", {
      textDocument: { uri, languageId: "typescript", version: 1, text },
    });
    const result = await server.request("textDocument/completion", {
      textDocument: { uri }, position: { line: 1, character: 8 },
    }) as { items: { label: string }[] };
    expect(result.items.map(item => item.label)).toContain("charAt");
  } finally {
    await server.shutdown();
    rmSync(root, { recursive: true, force: true });
  }
}, 15_000);
