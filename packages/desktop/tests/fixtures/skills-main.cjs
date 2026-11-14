// Isolate filesystem ownership before loading modules that capture homedir.
const path = require("node:path");
const fs = require("node:fs");
const os = require("node:os");
const root = process.argv[2];
if (!root || !fs.existsSync(path.join(root, "owned-skills-fixture")))
  throw new Error("Missing owned fixture root");
os.homedir = () => root;
const { app, BrowserWindow, ipcMain } = require("electron");
const { SkillManager } = require("../../dist-electron/skill-manager.cjs");
const {
  registerSkillManagerIpc,
} = require("../../dist-electron/skill-manager-ipc.cjs");
app.setPath("userData", path.join(root, "profile"));
app.whenReady().then(async () => {
  const manager = new SkillManager();
  await manager.initialize();
  registerSkillManagerIpc(ipcMain, manager, () => window);
  ipcMain.handle("crab:rpc", async () => ({
    ok: true,
    value: { exists: false, version: 0, jobs: [], error: null },
  }));
  ipcMain.handle("crab:main-settings-update", async () => ({}));
  ipcMain.handle("crab:set-devtools", async () => {});
  const window = new BrowserWindow({
    width: 1100,
    height: 800,
    webPreferences: {
      preload: path.resolve(__dirname, "../../dist-electron/preload.cjs"),
      sandbox: false,
    },
  });
  await window.loadURL(process.argv[3]);
});
app.on("window-all-closed", () => app.quit());
