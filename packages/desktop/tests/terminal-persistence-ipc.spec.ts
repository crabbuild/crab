import { expect, it } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { registerTerminalPersistence } from "../electron/terminal-persistence";

it.each(["tab", "quick terminal"])("preserves the layout and other scrollback when a %s checkpoint is unreadable", (kind) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-terminal-state-"));
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  const statePaths = registerTerminalPersistence({
    handle: (name, handler) => { handlers.set(name, handler); }, removeHandler: () => {},
  }, root);
  const owner = "/repository";
  const tab = (id: string) => ({ id, title: id, shell: "/bin/sh", cwd: owner });
  const layout = {
    version: 1, tabs: [tab("healthy"), ...(kind === "tab" ? [tab("unreadable")] : [])],
    splits: [], activeTabIndex: 0,
    ...(kind === "quick terminal" ? { quickTerminal: tab("unreadable") } : {}),
  };
  try {
    handlers.get("crab:term-save-state")!(null, { persistenceKey: owner, layout, scrollbackData: { healthy: "saved output" } });
    // A directory at the checkpoint path produces a real read failure on
    // every test OS, including privileged runners where chmod is ineffective.
    fs.mkdirSync(path.join(statePaths(owner).scrollbackDir, "unreadable.bin"));
    expect(handlers.get("crab:term-restore-state")!(null, owner)).toEqual({
      layout, scrollbackData: { healthy: "saved output" },
    });
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
