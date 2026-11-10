import * as fs from "node:fs";
import * as path from "node:path";
import { createHash } from "node:crypto";
import type { IpcMainLike } from "./term-router";

export function registerTerminalPersistence(ipcMain: IpcMainLike, stateDir: string) {
  function statePaths(persistenceKey: string) {
    const key = createHash("sha256").update(persistenceKey).digest("hex");
    const directory = path.join(stateDir, "layouts", key);
    return { statePath: path.join(directory, "state.json"), scrollbackDir: path.join(directory, "scrollback") };
  }

  function scrollbackPath(directory: string, id: string) {
    if (!id || /[/\\]/.test(id)) throw new Error("Invalid terminal session ID");
    return path.join(directory, `${id}.bin`);
  }

  ipcMain.handle("crab:term-save-state", (_event, value) => {
    const req = value as { persistenceKey: string; layout: unknown; scrollbackData: Record<string, string> };
    try {
      const { statePath, scrollbackDir } = statePaths(req.persistenceKey);
      fs.mkdirSync(scrollbackDir, { recursive: true });
      // A partial write must not destroy the last usable layout during an
      // upgrade or crash. Both files live on the same filesystem for rename.
      fs.writeFileSync(`${statePath}.tmp`, JSON.stringify({ version: 1, savedAt: new Date().toISOString(), layout: req.layout }), { mode: 0o600 });
      fs.renameSync(`${statePath}.tmp`, statePath);
      for (const [id, content] of Object.entries(req.scrollbackData)) {
        fs.writeFileSync(scrollbackPath(scrollbackDir, id), content, { mode: 0o600 });
      }
      return { success: true };
    } catch (error) {
      return { success: false, error: (error as NodeJS.ErrnoException).code ?? "unknown" };
    }
  });

  ipcMain.handle("crab:term-restore-state", (_event, value) => {
    try {
      const { statePath, scrollbackDir } = statePaths(value as string);
      const { layout } = JSON.parse(fs.readFileSync(statePath, "utf8"));
      const scrollbackData: Record<string, string> = {};
      for (const tab of [...(layout?.tabs ?? []), ...(layout?.quickTerminal ? [layout.quickTerminal] : [])]) {
        if (typeof tab?.id !== "string") continue;
        // Scrollback is an optional checkpoint. A failed read must not
        // erase pane membership or prevent the live session from attaching.
        try {
          const file = scrollbackPath(scrollbackDir, tab.id);
          if (fs.existsSync(file)) scrollbackData[tab.id] = fs.readFileSync(file, "utf8");
        } catch { /* Keep the layout and any other readable checkpoints. */ }
      }
      return { layout: layout ?? null, scrollbackData };
    } catch {
      return { layout: null, scrollbackData: {} };
    }
  });
  return statePaths;
}
