import { app,BrowserWindow,dialog,ipcMain,Notification,shell,safeStorage } from "electron";
import * as path from "node:path";
import * as fs from "node:fs";
import * as os from "node:os";
import { execFile } from "node:child_process";

import { Agent } from "./agent";
import { registerAppShutdown } from "./app-shutdown";
import { StorageConnections } from "./storage-connections";
import { nativeBinaries } from "./native-binaries";
import { loadEnvGlobal,loadEnvLayered } from "./env-loader";
import { getUserShellBaseEnv } from "./env/userShellEnv";
import { SidecarManager,registerSidecarIpc } from "./sidecars";
import { initializeDaemonSubsystem } from "./daemon/init";
import { parsePtyMode } from "./daemon/mode-selection";
import { registerDiagnosticsUiIpc } from "./daemon/diagnostics-ui-ipc";
import { createInProcessBridge } from "./in-process-bridge";
import { TermRouter } from "./term-router";
import { registerTerminalPersistence } from "./terminal-persistence";
import { registerKeychainHandlers } from "./keychain";
import { registerForgeAuth } from "./forge-auth";
import { registerOrchestratorIpc } from "./orchestrator-ipc";
import { SkillManager } from "./skill-manager";
import { registerSkillManagerIpc } from "./skill-manager-ipc";
import { initAutoUpdater,stopAutoUpdater } from "./updater";
import { SshProfileManager } from "./ssh-profiles";
import { SshSessionManager } from "./ssh-manager";
import { registerSshIpcHandlers } from "./ssh-ipc";
import { RemoteTerminalManager } from "./ssh-terminal";
import { PortForwardManager } from "./ssh-port-forward";
import { SshDatabase } from "./ssh-database";
import { RpcRouter,parseRepoId } from "./ssh-router";
import { getProviderRegistry } from "./provider-registry";
import { LocalProvider } from "./local-provider";
import { RemoteProvider } from "./remote-provider";
import { AgentDeployer } from "./ssh-deployer";
import { CapabilityNegotiator } from "./ssh-capabilities";
import { WorkspaceSyncEngine } from "./ssh-workspace-sync";
import { FileTransferManager } from "./ssh-file-transfer";
import { DeltaSyncEngine } from "./ssh-delta-sync";
import { TransferHistoryStore } from "./ssh-transfer-history";
import { TransferScheduler } from "./ssh-transfer-scheduler";
import { BandwidthThrottler } from "./ssh-bandwidth-throttler";
import { TransferCompressor } from "./ssh-transfer-compressor";
import { RemoteProcessManager } from "./ssh-process-manager";
import { RemoteEnvironmentManager } from "./ssh-environment-manager";
import { RemoteToolManager } from "./ssh-tool-manager";
import { SshLatencyMonitor } from "./ssh-latency-monitor";
import { ConnectionQualityMonitor } from "./ssh-connection-quality";
import { BookmarkManager } from "./ssh-bookmarks";
import { RemoteWorkspaceManager } from "./ssh-remote-workspace";
import { RemoteGitManager } from "./ssh-remote-git";
import { RemoteDebugManager } from "./ssh-remote-debug";
import { RemoteFileEditor } from "./ssh-file-editor";
import { wireEnhancedIpcHandlers } from "./ssh-enhanced-wiring";
import { bridgeSyncEventsToRenderer,bridgeAgentDeployToRenderer } from "./ssh-ipc";
import { RemoteWorkspaceOrchestrator } from "./remote-workspace-orchestrator";
import { registerRemoteWorkspaceIpc } from "./remote-workspace-ipc";
import { registerMountIpc } from "./mount-ipc";
import { browserManager } from "./browser/browser-manager";
import { registerAgentWorkflowIpc } from "./agent-workflows/agent-workflow-ipc";
import { LspManager } from "./lsp-manager";
import { registerLspIpc } from "./lsp-ipc";
import { registerFileWatcher,stopAllWatchers } from "./file-watcher";
import { registerTerminalSessionIpc } from "./terminal-reattach";
import { readWorkspaceConfig,writeWorkspaceConfig,defaultWorkspaceConfig,WorkspaceConfig } from "./workspace-config";
import { discoverRepos,DiscoveredRepo } from "./workspace-discovery";
import { StatusPoller,RepoStatus } from "./workspace-poller";

const isDev = !app.isPackaged;

// Load credentials from ~/.config/crab/.env early so that the agent
// subprocess (and any crab CLI invocations) inherit AWS/storage creds
// even though macOS GUI apps don't get the user's shell environment.
loadEnvGlobal();

let mainWindow: BrowserWindow | null = null;
let agent: Agent | null = null;
let resolveAgentReady!: () => void;
let rejectAgentReady!: (reason: unknown) => void;
const agentReady = new Promise<void>((resolve, reject) => {
  resolveAgentReady = resolve;
  rejectAgentReady = reject;
});
// The window can issue RPC before shell/PTY startup has finished. Keep those
// requests pending until the native ping succeeds, or surface its real failure.
void agentReady.catch(() => {});
const sidecarManager = new SidecarManager();
const lspManager = new LspManager();
let shutdownAgentServices: (() => Promise<void>) | undefined;

// Workspace state (managed by IPC handlers)
let workspaceRootPath: string | null = null;
let workspaceConfig: WorkspaceConfig | null = null;
let workspacePoller: StatusPoller | null = null;

// SSH module references — set during app.whenReady() initialization,
// used by workspace IPC handlers to pass to the StatusPoller so it
// can poll remote repos via the ProviderRegistry and RpcRouter.
let _rpcRouter: RpcRouter | null = null;
let _sshSessionManager: SshSessionManager | null = null;

// Orchestrator instance — exposed at module level so the SSH subsystem
// can wire the RPC router's notification stream to it.
import type { Orchestrator } from "./orchestrator";
let orchestrator: Orchestrator | null = null;

/**
 * Returns the orchestrator instance. Used by the SSH subsystem to wire
 * the RPC router's notification stream via bridgeRemoteActivityFeed().
 * Returns null if called before registerIpc() completes.
 */
export function getOrchestrator(): Orchestrator | null {
  return orchestrator;
}
let workspaceRepos: DiscoveredRepo[] = [];

// --no-telemetry launch flag: when present, telemetry is forced off
// regardless of the user's opt-in setting. Useful for CI and
// privacy-conscious environments.
const noTelemetryFlag = process.argv.includes("--no-telemetry");

// Surface any unexpected main-process error as a dialog rather than a
// silent crash. Electron's default uncaught-exception dialog shows a
// generic trace but does not tell the user what to do next; ours
// includes a short note and keeps the app alive long enough for the
// user to read it.
process.on("uncaughtException", (err) => {
  const message = err instanceof Error ? err.stack || err.message : String(err);
  console.error("[crab-desktop]", message);
  // Best effort — if the dialog API itself fails (e.g. during app
  // teardown), fall back to stderr.
  try {
    dialog.showErrorBox("Crab Desktop — unexpected error", message);
  } catch {
    process.stderr.write(`[crab-desktop] ${message}\n`);
  }
});

// ---------------------------------------------------------------------
// Main-process settings file.
//
// A small subset of Settings must be honoured before the renderer even
// mounts: the agent's log level (env-var to the sidecar) and whether
// to auto-open DevTools on launch. Those can't come from localStorage,
// which the main process can't reach. Instead the renderer mirrors a
// minimal slice of its settings into a JSON file under the Electron
// `userData` dir, and main reads that file on boot.
//
// Settings that only affect renderer-side rendering (editor font size,
// preview caps, etc.) continue to live in localStorage alone.
// ---------------------------------------------------------------------
interface MainSettings {
  agentLogLevel: "off" | "info" | "debug" | "trace";
  openDevtoolsOnLaunch: boolean;
}
const DEFAULT_MAIN_SETTINGS: MainSettings = {
  agentLogLevel: "info",
  openDevtoolsOnLaunch: false,
};
function mainSettingsPath(): string {
  return path.join(app.getPath("userData"), "main-settings.json");
}
function readMainSettings(): MainSettings {
  try {
    const raw = fs.readFileSync(mainSettingsPath(), "utf8");
    const parsed = JSON.parse(raw) as Partial<MainSettings>;
    return { ...DEFAULT_MAIN_SETTINGS, ...parsed };
  } catch {
    return DEFAULT_MAIN_SETTINGS;
  }
}
function writeMainSettings(s: MainSettings) {
  try {
    const dir = path.dirname(mainSettingsPath());
    if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(mainSettingsPath(), JSON.stringify(s, null, 2), "utf8");
  } catch {
    // Non-fatal — the renderer's localStorage remains authoritative.
  }
}
function agentEnvFor(level: MainSettings["agentLogLevel"]): NodeJS.ProcessEnv {
  // `tracing-subscriber` honours the `CRAB_DESKTOP_LOG` env var we
  // set up in `agent/src/main.rs`. `off` maps to an empty filter
  // which effectively suppresses everything.
  const env: NodeJS.ProcessEnv = { ...process.env };
  if (level === "off") env.CRAB_DESKTOP_LOG = "off";
  else env.CRAB_DESKTOP_LOG = level;

  // On macOS, GUI apps don't inherit the user's shell PATH. Ensure
  // common tool directories are included so the agent can find the
  // crab CLI, git, and other tools.
  const home = os.homedir();
  const extraPaths = [
    path.join(home, ".cargo", "bin"),
    "/usr/local/bin",
    "/opt/homebrew/bin",
    path.join(home, ".local", "bin"),
  ];
  const currentPath = env.PATH ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  const pathSet = new Set(currentPath.split(path.delimiter));
  for (const p of extraPaths) {
    if (!pathSet.has(p)) pathSet.add(p);
  }
  env.PATH = [...pathSet].join(path.delimiter);

  return env;
}

async function createWindow() {
  const mainSettings = readMainSettings();

  mainWindow = new BrowserWindow({
    width: 1280,
    height: 820,
    minWidth: 880,
    minHeight: 560,
    backgroundColor: "#0f1115",
    titleBarStyle: process.platform === "darwin" ? "hiddenInset" : "default",
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webviewTag: true,
    },
  });

  const devServer = process.env.VITE_DEV_SERVER_URL;
  if (isDev && devServer) {
    await mainWindow.loadURL(devServer);
    mainWindow.webContents.openDevTools({ mode: "detach" });
  } else {
    const indexPath = path.join(__dirname, "../dist/index.html");
    await mainWindow.loadFile(indexPath);
    if (mainSettings.openDevtoolsOnLaunch) {
      mainWindow.webContents.openDevTools({ mode: "detach" });
    }
  }

  // Catch renderer load failures so we see them as a dialog instead of
  // a silent black window. The `did-fail-load` event fires for things
  // like CSP rejections, missing files, or a bad index.html path.
  mainWindow.webContents.on("did-fail-load", (_e, code, desc, url) => {
    dialog.showErrorBox(
      "Crab Desktop — renderer failed to load",
      `code=${code}\nurl=${url}\n\n${desc}`,
    );
  });

  // Security: never open new windows in Electron chrome. Send them
  // to the OS browser.
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });

  // Security: validate webview partition against the browser allowlist.
  // Prevents a compromised renderer from smuggling an arbitrary partition
  // string into a guest surface.
  mainWindow.webContents.on("will-attach-webview", (event, webPreferences, params) => {
    const requestedPartition = params?.partition ?? webPreferences?.partition ?? "";
    // Allow the default webview partition (empty) and our browser partition
    const allowedPartitions = new Set([
      "",
      "persist:crab-browser",
    ]);
    if (requestedPartition && !allowedPartitions.has(requestedPartition)) {
      event.preventDefault();
    }
  });

  // Lock the browser trust boundary to this renderer process so that
  // only IPC calls from the main window's renderer are honored.
  browserManager.setTrustedRendererWebContentsId(mainWindow.webContents.id);
}

// Path to the agent log directory. Used by the --log-file flag and
// the "Open logs folder" IPC handler.
function agentLogsDir(): string {
  return path.join(os.homedir(), ".crab", "desktop", "logs");
}

/**
 * Extract the repo_id field from RPC params. Params can be an object
 * with a `repo_id` field.
 */
function extractRepoIdFromParams(params: unknown): string | null {
  if (params && typeof params === "object" && "repo_id" in params) {
    const id = (params as Record<string, unknown>).repo_id;
    return typeof id === "string" ? id : null;
  }
  return null;
}

function registerIpc(skillManager: SkillManager) {
  // Keychain handlers for secure credential storage.
  registerKeychainHandlers(ipcMain);

  // Orchestrator IPC handlers (agent task lifecycle, diff, apply).
  const agentServices = registerOrchestratorIpc(ipcMain, () => mainWindow, skillManager);
  orchestrator = agentServices.orchestrator;
  shutdownAgentServices = agentServices.shutdown;

  // Agent Workflow IPC handlers (DAG definitions, run orchestration, review gates).
  registerAgentWorkflowIpc(ipcMain, () => mainWindow, () => orchestrator);

  // Mount IPC handlers (VFS mount management via coordinator socket).
  registerMountIpc();

  // Browser IPC handlers (guest lifecycle, downloads, viewport, devtools).
  browserManager.registerIpcHandlers(ipcMain);

  // Bulk RPC passthrough. The preload script narrows the surface to
  // whitelisted method names before this handler is reached, so we
  // can forward params verbatim.
  //
  // Routing: resolves the correct provider (local or remote) via the
  // ProviderRegistry based on the repo_id in params. For open_local
  // and open_url, registers the provider synchronously before returning
  // the RepoHandle to the renderer (prevents race conditions).
  ipcMain.handle("crab:rpc", async (_evt, method: string, params: unknown) => {
    await agentReady;
    if (!agent) throw new Error("agent not running");

    if (method.startsWith("storage_connections_")) return agent.request(method, params);
    const registry = getProviderRegistry();

    // Handle repo open methods: register provider before returning handle.
    if (method === "open_local" || method === "open_url") {
      const handle = await agent.request(method, params);
      // Register the appropriate provider for this repo_id.
      const repoId = (handle as { repo_id?: string })?.repo_id;
      if (repoId) {
        const parsed = parseRepoId(repoId);
        if (parsed) {
          // Remote repo — RemoteProvider is registered by the SSH
          // subsystem when the connection is established. If one isn't
          // registered yet, the registry's resolve() will return null
          // and we fall through to the RpcRouter-based fallback.
        } else {
          // Local repo — register with the LocalProvider singleton.
          // The LocalProvider is already set as the fallback, but
          // explicit registration ensures resolve() is O(1).
          const localProvider = registry.resolve(repoId);
          if (localProvider) {
            registry.register(repoId, localProvider);
          }
        }
      }
      return handle;
    }

    // Handle repo close: unregister the provider.
    if (method === "close") {
      const repoId = extractRepoIdFromParams(params);
      // Route the close call through the appropriate provider.
      let result: unknown;
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          // Stop file watching before closing the repo.
          void provider.watchStop(repoId).catch(() => {});
          result = await provider.request(method, params as Record<string, unknown>);
        } else {
          result = await agent.request(method, params);
        }
        registry.unregister(repoId);
      } else {
        result = await agent.request(method, params);
      }
      return result;
    }

    // All other calls: resolve provider from repo_id and dispatch.
    const repoId = extractRepoIdFromParams(params);
    if (repoId) {
      const provider = registry.resolve(repoId);
      if (provider) {
        return provider.request(method, params as Record<string, unknown>);
      }
      // No provider found — for remote repos without explicit
      // registration, this shouldn't happen after open. Fall through
      // to the local agent as a safety net.
    }

    // Fallback: no repo_id in params or no provider resolved — send
    // to the local agent directly (preserves existing behavior for
    // repo-less calls like `ping`, `version`, etc.).
    return agent.request(method, params);
  });

  ipcMain.handle("crab:dialog-open-repo", async () => {
    if (!mainWindow) return null;
    const res = await dialog.showOpenDialog(mainWindow, {
      title: "Open a Crab repository",
      properties: ["openDirectory"],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    return res.filePaths[0];
  });

  ipcMain.handle("crab:dialog-select-destination", async () => {
    if (!mainWindow) return null;
    const res = await dialog.showOpenDialog(mainWindow, {
      title: "Select clone destination",
      properties: ["openDirectory", "createDirectory"],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    return res.filePaths[0];
  });

  ipcMain.handle("crab:dialog-pick-worktree-path", async () => {
    if (!mainWindow) return null;
    const res = await dialog.showOpenDialog(mainWindow, {
      title: "Select worktree location",
      properties: ["openDirectory", "createDirectory"],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    return res.filePaths[0];
  });

  ipcMain.handle("crab:dialog-pick-json-file", async (_evt, title?: string) => {
    if (!mainWindow) return null;
    const res = await dialog.showOpenDialog(mainWindow, {
      title: title ?? "Select a JSON file",
      properties: ["openFile"],
      filters: [{ name: "JSON Files", extensions: ["json"] }],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    const filePath = res.filePaths[0];
    try {
      const content = fs.readFileSync(filePath, "utf8");
      return { path: filePath, content };
    } catch {
      return null;
    }
  });

  // Update the main-process-side settings mirror. The renderer calls
  // this whenever the user edits log level / DevTools in the Settings
  // page. Changes that need a sidecar restart (log level) apply on
  // the next launch.
  ipcMain.handle(
    "crab:main-settings-update",
    async (_evt, update: Partial<MainSettings>) => {
      const next = { ...readMainSettings(), ...update };
      writeMainSettings(next);
      return next;
    },
  );

  // DevTools toggle — honoured live so the setting change in the
  // Settings page doesn't require a relaunch.
  ipcMain.handle("crab:set-devtools", async (_evt, open: boolean) => {
    if (!mainWindow || mainWindow.isDestroyed()) return;
    const wc = mainWindow.webContents;
    if (open) {
      if (!wc.isDevToolsOpened()) wc.openDevTools({ mode: "detach" });
    } else if (wc.isDevToolsOpened()) {
      wc.closeDevTools();
    }
  });

  // Expose the --no-telemetry flag to the renderer so the telemetry
  // module can honour it without re-parsing argv in the sandbox.
  ipcMain.handle("crab:no-telemetry-flag", async () => {
    return noTelemetryFlag;
  });

  // Open the agent logs folder in the system file manager.
  ipcMain.handle("crab:open-logs-folder", async () => {
    const logsDir = agentLogsDir();
    if (!fs.existsSync(logsDir)) {
      fs.mkdirSync(logsDir, { recursive: true });
    }
    await shell.openPath(logsDir);
    return { opened: logsDir };
  });

  // ---- Dialogs (prompt / confirm replacements for sandboxed renderer) ----

  ipcMain.handle(
    "crab:dialog-prompt",
    async (_evt, message: string, defaultValue?: string) => {
      if (!mainWindow) return null;
      // Use a simple input dialog via Electron's showMessageBox + a
      // workaround: Electron doesn't have a native prompt, so we use
      // a BrowserWindow-based approach. For simplicity, we'll use the
      // main window's webContents to evaluate a prompt-like dialog.
      // Actually, the cleanest approach is to use dialog.showMessageBox
      // for confirm and a small child window for prompt. But the
      // simplest cross-platform solution is to just disable sandbox
      // for prompt or use a custom HTML dialog.
      //
      // Simplest fix: use a synchronous approach via a modal child window.
      const { BrowserWindow: BW } = require("electron") as typeof import("electron");
      return new Promise<string | null>((resolve) => {
        const promptWin = new BW({
          parent: mainWindow!,
          modal: true,
          width: 400,
          height: 180,
          resizable: false,
          minimizable: false,
          maximizable: false,
          show: false,
          frame: true,
          title: "Crab Desktop",
          webPreferences: {
            nodeIntegration: false,
            contextIsolation: true,
            sandbox: false,
          },
        });
        const escaped = (s: string) => s.replace(/'/g, "\\'").replace(/\n/g, "\\n");
        const html = `<!DOCTYPE html>
<html><head><style>
  body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; padding: 20px; margin: 0; background: #1a1d23; color: #e7eaf1; display: flex; flex-direction: column; height: 100vh; box-sizing: border-box; }
  label { font-size: 13px; margin-bottom: 8px; display: block; }
  input { width: 100%; padding: 6px 10px; border: 1px solid #3a3f4b; border-radius: 4px; background: #0f1115; color: #e7eaf1; font-size: 13px; outline: none; box-sizing: border-box; }
  input:focus { border-color: #4f8cff; }
  .buttons { margin-top: auto; display: flex; justify-content: flex-end; gap: 8px; }
  button { padding: 5px 14px; border-radius: 4px; border: none; font-size: 12px; cursor: pointer; }
  .ok { background: #4f8cff; color: #fff; }
  .ok:hover { background: #3b7af0; }
  .cancel { background: #2a2e38; color: #aab; }
  .cancel:hover { background: #3a3f4b; }
</style></head><body>
  <label>${escaped(message)}</label>
  <input id="inp" value="${escaped(defaultValue ?? "")}" autofocus />
  <div class="buttons">
    <button class="cancel" id="cancelBtn">Cancel</button>
    <button class="ok" id="okBtn">OK</button>
  </div>
  <script>
    const inp = document.getElementById('inp');
    inp.select();
    document.getElementById('okBtn').onclick = () => {
      require = undefined; // safety
      const val = inp.value;
      document.title = 'RESULT:' + val;
    };
    document.getElementById('cancelBtn').onclick = () => {
      document.title = 'CANCEL';
    };
    inp.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') document.getElementById('okBtn').click();
      if (e.key === 'Escape') document.getElementById('cancelBtn').click();
    });
  </script>
</body></html>`;
        promptWin.loadURL(`data:text/html;charset=utf-8,${encodeURIComponent(html)}`);
        promptWin.once("ready-to-show", () => promptWin.show());
        promptWin.webContents.on("page-title-updated", (_e, title) => {
          if (title.startsWith("RESULT:")) {
            resolve(title.slice(7));
            promptWin.close();
          } else if (title === "CANCEL") {
            resolve(null);
            promptWin.close();
          }
        });
        promptWin.on("closed", () => resolve(null));
      });
    },
  );

  ipcMain.handle(
    "crab:dialog-confirm",
    async (_evt, message: string) => {
      if (!mainWindow) return false;
      const res = await dialog.showMessageBox(mainWindow, {
        type: "question",
        buttons: ["Cancel", "OK"],
        defaultId: 1,
        cancelId: 0,
        message,
      });
      return res.response === 1;
    },
  );

  // ---- Repo capabilities ----

  // Returns the CapabilitySet for a given repo_id by resolving the
  // provider and calling its capabilities() method.
  ipcMain.handle("crab:repo-get-capabilities", async (_evt, repoId: string) => {
    const registry = getProviderRegistry();
    const provider = registry.resolve(repoId);
    if (provider) {
      return provider.capabilities(repoId);
    }
    // Fallback: local repos always have full capabilities.
    const { fullCapabilities } = await import("./repo-provider.js");
    return fullCapabilities();
  });

  // ---- File operations ----

  // Reveal a file or folder in the system file manager (Finder/Explorer).
  ipcMain.handle("crab:fs-reveal", async (_evt, fullPath: string) => {
    shell.showItemInFolder(fullPath);
  });

  // Create a new file (empty) or folder.
  // Accepts an optional repoId first parameter for provider-based routing.
  // When repoId is provided, delegates to the resolved provider's fsCreate.
  // When omitted (backward compat), uses local filesystem directly.
  ipcMain.handle(
    "crab:fs-create",
    async (_evt, firstArg: string, secondArg: string | "file" | "directory", thirdArg?: "file" | "directory") => {
      const registry = getProviderRegistry();

      // Detect whether the caller passed repoId as the first argument.
      // If thirdArg is present, the signature is (repoId, fullPath, kind).
      // Otherwise, the signature is (fullPath, kind) for backward compat.
      let repoId: string | null;
      let fullPath: string;
      let kind: "file" | "directory";

      if (thirdArg !== undefined) {
        repoId = firstArg;
        fullPath = secondArg;
        kind = thirdArg;
      } else {
        repoId = null;
        fullPath = firstArg;
        kind = secondArg as "file" | "directory";
      }

      // If repoId is provided, resolve the provider and delegate.
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          return provider.fsCreate(repoId, fullPath, kind);
        }
      }

      // Fallback: local filesystem operations (existing behavior).
      if (kind === "directory") {
        fs.mkdirSync(fullPath, { recursive: true });
      } else {
        const dir = path.dirname(fullPath);
        if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
        fs.writeFileSync(fullPath, "");
      }
      return { path: fullPath, kind };
    },
  );

  // Rename (move) a file or folder.
  // Accepts an optional repoId first parameter for provider-based routing.
  ipcMain.handle(
    "crab:fs-rename",
    async (_evt, firstArg: string, secondArg: string, thirdArg?: string) => {
      const registry = getProviderRegistry();

      // Detect whether the caller passed repoId as the first argument.
      // If thirdArg is present, the signature is (repoId, oldPath, newPath).
      // Otherwise, the signature is (oldPath, newPath) for backward compat.
      let repoId: string | null;
      let oldPath: string;
      let newPath: string;

      if (thirdArg !== undefined) {
        repoId = firstArg;
        oldPath = secondArg;
        newPath = thirdArg;
      } else {
        repoId = null;
        oldPath = firstArg;
        newPath = secondArg;
      }

      // If repoId is provided, resolve the provider and delegate.
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          return provider.fsRename(repoId, oldPath, newPath);
        }
      }

      // Fallback: local filesystem operations (existing behavior).
      const dir = path.dirname(newPath);
      if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
      fs.renameSync(oldPath, newPath);
      return { oldPath, newPath };
    },
  );

  // Delete a file or folder (moves to trash by default).
  // Accepts an optional repoId first parameter for provider-based routing.
  ipcMain.handle(
    "crab:fs-delete",
    async (_evt, firstArg: string, secondArg?: string | boolean, thirdArg?: boolean) => {
      const registry = getProviderRegistry();

      // Detect whether the caller passed repoId as the first argument.
      // Possible signatures:
      //   (repoId, fullPath)           — remote delete, no trash option
      //   (repoId, fullPath, useTrash) — remote delete with trash option
      //   (fullPath)                   — backward compat, trash=true
      //   (fullPath, useTrash)         — backward compat with trash option
      //
      // Heuristic: if secondArg is a string (path-like), then firstArg is repoId.
      // If secondArg is boolean or undefined, then firstArg is fullPath.
      let repoId: string | null;
      let fullPath: string;
      let useTrash: boolean;

      if (typeof secondArg === "string") {
        // Signature: (repoId, fullPath, useTrash?)
        repoId = firstArg;
        fullPath = secondArg;
        useTrash = thirdArg ?? true;
      } else {
        // Signature: (fullPath, useTrash?)
        repoId = null;
        fullPath = firstArg;
        useTrash = (secondArg as boolean | undefined) ?? true;
      }

      // If repoId is provided, resolve the provider and delegate.
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          return provider.fsDelete(repoId, fullPath, useTrash);
        }
      }

      // Fallback: local filesystem operations (existing behavior).
      if (useTrash) {
        await shell.trashItem(fullPath);
      } else {
        const stat = fs.statSync(fullPath);
        if (stat.isDirectory()) {
          fs.rmSync(fullPath, { recursive: true, force: true });
        } else {
          fs.unlinkSync(fullPath);
        }
      }
      return { deleted: fullPath, trash: useTrash };
    },
  );

  // Open a file with the system default application.
  ipcMain.handle("crab:fs-open-external", async (_evt, fullPath: string) => {
    await shell.openPath(fullPath);
  });

  // Write text content to a file (creates parent directories as needed).
  // Accepts an optional repoId first parameter for provider-based routing.
  ipcMain.handle(
    "crab:fs-write-file",
    async (_evt, firstArg: string, secondArg: string, thirdArg?: string) => {
      const registry = getProviderRegistry();

      // Detect whether the caller passed repoId as the first argument.
      // If thirdArg is present, the signature is (repoId, fullPath, content).
      // Otherwise, the signature is (fullPath, content) for backward compat.
      let repoId: string | null;
      let fullPath: string;
      let content: string;

      if (thirdArg !== undefined) {
        repoId = firstArg;
        fullPath = secondArg;
        content = thirdArg;
      } else {
        repoId = null;
        fullPath = firstArg;
        content = secondArg;
      }

      // If repoId is provided, resolve the provider and delegate.
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          return provider.fsWrite(repoId, fullPath, content);
        }
      }

      // Fallback: local filesystem operations (existing behavior).
      const dir = path.dirname(fullPath);
      if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(fullPath, content, "utf8");
      return { path: fullPath, written: content.length };
    },
  );

  // Read text content from a file on disk.
  // Accepts an optional repoId first parameter for provider-based routing.
  ipcMain.handle(
    "crab:fs-read-file",
    async (_evt, firstArg: string, secondArg?: string) => {
      const registry = getProviderRegistry();

      // Detect whether the caller passed repoId as the first argument.
      // If secondArg is present, the signature is (repoId, fullPath).
      // Otherwise, the signature is (fullPath) for backward compat.
      let repoId: string | null;
      let fullPath: string;

      if (secondArg !== undefined) {
        repoId = firstArg;
        fullPath = secondArg;
      } else {
        repoId = null;
        fullPath = firstArg;
      }

      // If repoId is provided, resolve the provider and delegate.
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          return provider.fsRead(repoId, fullPath);
        }
      }

      // Fallback: local filesystem operations (existing behavior).
      if (!fs.existsSync(fullPath)) {
        return { exists: false, content: null };
      }
      const stat = fs.statSync(fullPath);
      if (stat.isDirectory()) {
        return { exists: true, content: null };
      }
      const content = fs.readFileSync(fullPath, "utf8");
      return { exists: true, content };
    },
  );

  // Read binary content from a file on disk and return it as a
  // base64-encoded data URL plus the inferred MIME type. Used by the
  // chat tool cards to render images that Codex inspects via the
  // `imageView` item, and by anywhere else in the app that needs to
  // render an arbitrary file as an `<img>` source. We cap the
  // payload at 8 MB so a stray huge file can't OOM the renderer.
  ipcMain.handle(
    "crab:fs-read-image",
    async (_evt, fullPath: string) => {
      const MAX_BYTES = 8 * 1024 * 1024;
      if (!fs.existsSync(fullPath)) {
        return { exists: false, dataUrl: null, mimeType: null, bytes: 0 };
      }
      const stat = fs.statSync(fullPath);
      if (stat.isDirectory()) {
        return { exists: true, dataUrl: null, mimeType: null, bytes: stat.size };
      }
      if (stat.size > MAX_BYTES) {
        return { exists: true, dataUrl: null, mimeType: null, bytes: stat.size, tooLarge: true };
      }
      const ext = path.extname(fullPath).toLowerCase();
      const mimeType =
        ext === ".png" ? "image/png" :
        ext === ".jpg" || ext === ".jpeg" ? "image/jpeg" :
        ext === ".gif" ? "image/gif" :
        ext === ".webp" ? "image/webp" :
        ext === ".svg" ? "image/svg+xml" :
        ext === ".bmp" ? "image/bmp" :
        ext === ".ico" ? "image/x-icon" :
        ext === ".avif" ? "image/avif" :
        "application/octet-stream";
      const buf = fs.readFileSync(fullPath);
      const dataUrl = `data:${mimeType};base64,${buf.toString("base64")}`;
      return { exists: true, dataUrl, mimeType, bytes: stat.size };
    },
  );

  // ---- Terminal helpers (used by InProcessBridge, constructed in app.whenReady) ----

  function getDefaultShell(): string {
    if (process.platform === "win32") {
      // Prefer PowerShell 7+ (pwsh) over legacy Windows PowerShell over cmd.
      const pwshPaths = [
        path.join(process.env.ProgramFiles || "C:\\Program Files", "PowerShell", "7", "pwsh.exe"),
        path.join(process.env.LOCALAPPDATA || "", "Microsoft", "PowerShell", "pwsh.exe"),
      ];
      for (const p of pwshPaths) {
        if (p && fs.existsSync(p)) return p;
      }
      const winPs = "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe";
      if (fs.existsSync(winPs)) return winPs;
      return process.env.COMSPEC || "cmd.exe";
    }
    if (process.platform === "darwin") return "/bin/zsh";
    return process.env.SHELL || "/bin/bash";
  }

  // --- Detect the user's real login shell environment ---
  //
  // On macOS/Linux, GUI apps inherit a minimal PATH from launchd/systemd
  // that doesn't include user-installed tools. We use the same approach
  // as VS Code: spawn the user's login shell with Node.js inside it to
  // JSON.stringify(process.env), wrapped in unique markers for reliable
  // extraction. This is robust against prompt output, ANSI codes, and
  // multi-line env values.
  //
  // Key design choices (matching VS Code):
  // - stdin is 'ignore' so interactive plugins don't hang waiting for input
  // - Unique UUID markers wrap the JSON to extract it from any noise
  // - The shell is spawned detached so it doesn't block the parent
  // - Works with any tool manager (nvm, mise, volta, fnm, pyenv, etc.)

  let detectedShellEnv: Record<string, string> | null = null;

  async function detectLoginShellEnv(): Promise<Record<string, string> | null> {
    const env = await getUserShellBaseEnv();
    return env as Record<string, string>;
  }

  const shellEnvPromise = detectLoginShellEnv().then((env) => {
    detectedShellEnv = env;
    return env;
  });

  // Build a terminal environment that mirrors what a native terminal app
  // provides. Uses the detected login shell environment when available,
  // falling back to process.env with the login shell flag on the PTY.
  function buildTerminalEnv(extraEnv?: Record<string, string>): Record<string, string> {
    const isWin = process.platform === "win32";

    // Start from the detected shell env (full user PATH) or fall back
    // to the Electron process env.
    const baseEnv = detectedShellEnv ?? (process.env as Record<string, string>);

    // Resolve the path to bundled shell integration scripts.
    // In development: relative to the source tree.
    // In production: under Electron's resourcesPath (extraResources).
    const shellIntegrationDir = isDev
      ? path.join(__dirname, "..", "resources", "shell-integration")
      : path.join(process.resourcesPath, "shell-integration");

    const env: Record<string, string> = {
      ...baseEnv,
      TERM_PROGRAM: "crab",
      TERM_PROGRAM_VERSION: app.getVersion(),
      CRAB_SHELL_INTEGRATION: "1",
      CRAB_SHELL_INTEGRATION_DIR: shellIntegrationDir,
    };

    // Remove npm_config_prefix — it's set by mise/asdf and conflicts with
    // nvm. When present, nvm refuses to modify PATH, which breaks tools
    // installed via nvm's npm (like Claude Code). The user's .zshrc will
    // re-set it if needed via `mise activate`.
    delete env.npm_config_prefix;

    // Unix-specific terminal variables.
    if (!isWin) {
      env.TERM = "xterm-256color";
      env.COLORTERM = "truecolor";
      env.LANG = baseEnv.LANG || process.env.LANG || "en_US.UTF-8";
    }

    if (extraEnv) Object.assign(env, extraEnv);
    return env;
  }

  // Pool hint is a no-op now that the terminal pool is disabled.
  // Keep the handler so the renderer doesn't get an unhandled IPC error.
  ipcMain.handle("crab:term-pool-hint", async (_evt, _cwd: string) => {
    // No-op: pool disabled.
  });

  // List available shells on the system.
  ipcMain.handle("crab:term-list-shells", async () => {
    const shells: string[] = [];
    if (process.platform === "darwin" || process.platform === "linux") {
      const fs = await import("fs");
      try {
        const content = fs.readFileSync("/etc/shells", "utf-8");
        for (const line of content.split("\n")) {
          const trimmed = line.trim();
          if (trimmed && !trimmed.startsWith("#")) shells.push(trimmed);
        }
      } catch {
        // Fallback
        shells.push("/bin/zsh", "/bin/bash", "/bin/sh");
      }
    } else {
      shells.push("powershell.exe", "cmd.exe");
    }
    return shells;
  });

  // ---- Terminal state persistence ----
  const termStateDir = path.join(app.getPath("userData"), "terminal");
  const termLogsDir = path.join(termStateDir, "logs");

  const terminalStatePaths = registerTerminalPersistence(ipcMain, termStateDir);

  // ---- Terminal session logging ----
  // Per-session log writers (main-process side).
  const sessionLogWriters = new Map<string, { path: string; size: number; disabled: boolean }>();

  /** Ensure the logs directory exists. */
  function ensureLogsDir(): boolean {
    try {
      if (!fs.existsSync(termLogsDir)) {
        fs.mkdirSync(termLogsDir, { recursive: true });
      }
      return true;
    } catch {
      return false;
    }
  }

  /** Get or create a log writer for a session. */
  function getLogWriter(sessionId: string): { path: string; size: number; disabled: boolean } {
    let writer = sessionLogWriters.get(sessionId);
    if (!writer) {
      const logPath = path.join(termLogsDir, `${sessionId}.log`);
      let size = 0;
      try {
        if (fs.existsSync(logPath)) {
          size = fs.statSync(logPath).size;
        }
      } catch { /* ignore */ }
      writer = { path: logPath, size, disabled: false };
      sessionLogWriters.set(sessionId, writer);
    }
    return writer;
  }

  /** Write data to a session log, handling rotation and errors. */
  function writeSessionLog(sessionId: string, data: string): void {
    const writer = getLogWriter(sessionId);
    if (writer.disabled) return;
    if (!ensureLogsDir()) {
      writer.disabled = true;
      return;
    }

    try {
      // Check if rotation is needed (10 MB threshold)
      if (writer.size > 10 * 1024 * 1024) {
        const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
        const rotatedPath = path.join(termLogsDir, `${sessionId}.${timestamp}.log`);
        try {
          fs.renameSync(writer.path, rotatedPath);
        } catch { /* ignore rotation failure */ }
        writer.size = 0;
      }

      fs.appendFileSync(writer.path, data, "utf8");
      writer.size += Buffer.byteLength(data, "utf8");
    } catch (err: unknown) {
      const code = (err as NodeJS.ErrnoException)?.code;
      if (code === "ENOSPC" || code === "EACCES") {
        writer.disabled = true;
        // Notify renderer about the disk error
        if (mainWindow && !mainWindow.isDestroyed()) {
          mainWindow.webContents.send("crab:term-log-error", {
            sessionId,
            error: code,
          });
        }
      }
    }
  }

  // Hook into terminal data to write logs
  // (This is done after terminal spawn — see the onData handler above)

  // ---- Terminal log path IPC ----
  ipcMain.handle(
    "crab:term-log-path",
    async (_evt, sessionId: string) => {
      if (!ensureLogsDir()) {
        return { path: "", exists: false, sizeBytes: 0 };
      }
      const logPath = path.join(termLogsDir, `${sessionId}.log`);
      let exists = false;
      let sizeBytes = 0;
      try {
        if (fs.existsSync(logPath)) {
          exists = true;
          sizeBytes = fs.statSync(logPath).size;
        }
      } catch { /* ignore */ }
      return { path: logPath, exists, sizeBytes };
    },
  );

  // ---- Terminal log read IPC (for Log Viewer) ----
  ipcMain.handle(
    "crab:term-log-read",
    async (_evt, sessionId: string) => {
      const logPath = path.join(termLogsDir, `${sessionId}.log`);
      try {
        if (!fs.existsSync(logPath)) {
          return { content: null, error: "not_found" };
        }
        const content = fs.readFileSync(logPath, "utf8");
        return { content, error: null };
      } catch (err: unknown) {
        return { content: null, error: (err as Error).message };
      }
    },
  );

  // ---- Terminal log list IPC (for Log Viewer browsing) ----
  ipcMain.handle("crab:term-log-list", async () => {
    if (!ensureLogsDir()) return [];
    try {
      const entries = fs.readdirSync(termLogsDir);
      const logs: Array<{ sessionId: string; path: string; sizeBytes: number; mtimeMs: number }> = [];
      for (const entry of entries) {
        if (!entry.endsWith(".log")) continue;
        // Skip rotated files (they have timestamp in the name)
        if (/\.\d{4}-\d{2}-\d{2}T/.test(entry)) continue;
        const fullPath = path.join(termLogsDir, entry);
        try {
          const stat = fs.statSync(fullPath);
          const sessionId = entry.replace(/\.log$/, "");
          logs.push({ sessionId, path: fullPath, sizeBytes: stat.size, mtimeMs: stat.mtimeMs });
        } catch { /* skip */ }
      }
      // Sort by most recent first
      logs.sort((a, b) => b.mtimeMs - a.mtimeMs);
      return logs;
    } catch {
      return [];
    }
  });

  // ---- Terminal scrollback/log cleanup IPC ----
  ipcMain.handle(
    "crab:term-clear-session",
    async (_evt, sessionId: string, persistenceKey: string) => {
      // Clearing a terminal must use the same owner as save and restore.
      const { scrollbackDir } = terminalStatePaths(persistenceKey);
      const scrollbackPath = path.join(scrollbackDir, `${sessionId}.bin`);
      try {
        if (fs.existsSync(scrollbackPath)) fs.unlinkSync(scrollbackPath);
      } catch { /* ignore */ }

      // Delete log file
      const logPath = path.join(termLogsDir, `${sessionId}.log`);
      try {
        if (fs.existsSync(logPath)) fs.unlinkSync(logPath);
      } catch { /* ignore */ }

      // Reset the log writer
      sessionLogWriters.delete(sessionId);

      return { success: true };
    },
  );

  // ---- Retention policy cleanup (runs on app start) ----
  function cleanupOldLogs(): void {
    if (!ensureLogsDir()) return;
    const retentionMs = 7 * 24 * 60 * 60 * 1000; // 7 days
    const cutoff = Date.now() - retentionMs;
    try {
      const entries = fs.readdirSync(termLogsDir);
      for (const entry of entries) {
        if (!entry.endsWith(".log")) continue;
        const fullPath = path.join(termLogsDir, entry);
        try {
          const stat = fs.statSync(fullPath);
          if (stat.mtimeMs < cutoff) {
            fs.unlinkSync(fullPath);
          }
        } catch { /* ignore */ }
      }
    } catch { /* ignore */ }
  }
  // Run cleanup on startup
  setTimeout(cleanupOldLogs, 5000);

  // ---- Terminal notifications (native desktop) ----

  ipcMain.handle(
    "crab:term-notify",
    async (
      _evt,
      req: { title: string; body: string; icon: "success" | "error"; terminalId: string; tabIndex: number },
    ) => {
      if (!Notification.isSupported()) return;

      const iconPath = req.icon === "success"
        ? path.join(__dirname, "..", "resources", "icons", "check-circle.png")
        : path.join(__dirname, "..", "resources", "icons", "x-circle.png");

      const notification = new Notification({
        title: req.title,
        body: req.body,
        ...(fs.existsSync(iconPath) ? { icon: iconPath } : {}),
      });

      notification.on("click", () => {
        // Bring app to foreground and activate the source terminal tab.
        if (mainWindow && !mainWindow.isDestroyed()) {
          if (mainWindow.isMinimized()) mainWindow.restore();
          mainWindow.focus();
          mainWindow.webContents.send("crab:term-activate-tab", {
            terminalId: req.terminalId,
            tabIndex: req.tabIndex,
          });
        }
      });

      notification.show();
    },
  );

  // Open a URL in the system browser.
  ipcMain.handle("crab:open-url", async (_evt, url: string) => {
    await shell.openExternal(url);
  });

  // ---- Jupyter kernel management ----
  // Lazy-loaded to avoid crashing the app if zeromq fails to load.
  let JupyterKernelModule: typeof import("./jupyter-kernel.js") | null = null;
  try {
    JupyterKernelModule = require("./jupyter-kernel") as typeof import("./jupyter-kernel.js");
  } catch (e) {
    console.error("[crab-desktop] Failed to load jupyter-kernel module:", e);
  }

  const kernels = new Map<string, any>();
  let kernelCounter = 0;
  let detectedJupyterPath: string | null = null;

  ipcMain.handle("crab:jupyter-list-kernels", async () => {
    if (!JupyterKernelModule) return [];
    const path = detectedJupyterPath || undefined;
    try {
      return await JupyterKernelModule.listKernelSpecs(path);
    } catch {
      return [];
    }
  });

  ipcMain.handle("crab:jupyter-check", async () => {
    // Check if jupyter + ipykernel are available.
    // Use `which` to get the FULL path — spawn() needs an absolute path
    // in packaged Electron apps where PATH is limited.
    const { execSync } = require("node:child_process") as typeof import("node:child_process");

    let jupyterPath: string | null = null;

    // Best approach: use a login shell which sources .zshrc/.bashrc
    // and picks up version managers (mise, pyenv, conda, etc.).
    try {
      const shell = process.env.SHELL || "/bin/zsh";
      const result = execSync(`${shell} -l -c "which jupyter"`, {
        encoding: "utf8",
        timeout: 10000,
      }).trim();
      if (result && result.startsWith("/")) jupyterPath = result;
    } catch { /* login shell failed */ }

    // Fallback: try with manually expanded PATH.
    if (!jupyterPath) {
      try {
        const result = execSync("which jupyter", {
          encoding: "utf8",
          env: {
            ...process.env,
            PATH: `${process.env.PATH}:/usr/local/bin:/opt/homebrew/bin:${process.env.HOME}/.local/bin:${process.env.HOME}/miniconda3/bin:${process.env.HOME}/anaconda3/bin`,
          },
          shell: "/bin/bash",
          timeout: 5000,
        }).trim();
        if (result && result.startsWith("/")) jupyterPath = result;
      } catch { /* not found */ }
    }

    // Fallback: probe known paths directly using fs.existsSync.
    if (!jupyterPath) {
      const { existsSync, readdirSync } = require("node:fs") as typeof import("node:fs");
      const candidates = [
        "/opt/homebrew/bin/jupyter",
        "/usr/local/bin/jupyter",
        `${process.env.HOME}/.local/bin/jupyter`,
        `${process.env.HOME}/miniconda3/bin/jupyter`,
        `${process.env.HOME}/anaconda3/bin/jupyter`,
        `${process.env.HOME}/.pyenv/shims/jupyter`,
      ];
      // Also check mise installs (common on macOS).
      try {
        const miseDir = `${process.env.HOME}/.local/share/mise/installs/python`;
        if (existsSync(miseDir)) {
          const versions = readdirSync(miseDir);
          for (const v of versions.sort().reverse()) {
            candidates.push(`${miseDir}/${v}/bin/jupyter`);
          }
        }
      } catch { /* ignore */ }
      for (const p of candidates) {
        if (existsSync(p)) { jupyterPath = p; break; }
      }
    }

    // Cache the detected path for subsequent calls.
    if (jupyterPath) detectedJupyterPath = jupyterPath;

    let pythonPath: string | null = null;
    try {
      const shell = process.env.SHELL || "/bin/zsh";
      const result = execSync(`${shell} -l -c "which python3"`, {
        encoding: "utf8",
        timeout: 10000,
      }).trim();
      if (result && result.startsWith("/")) pythonPath = result;
    } catch { /* not found */ }

    return { jupyterPath, pythonPath, available: jupyterPath !== null };
  });

  ipcMain.handle("crab:jupyter-install", async () => {
    // Auto-install ipykernel using pip.
    const { execSync } = require("node:child_process") as typeof import("node:child_process");
    const pips = ["pip3", "pip", "/usr/local/bin/pip3", "/opt/homebrew/bin/pip3",
      `${process.env.HOME}/.local/bin/pip3`];
    for (const pip of pips) {
      try {
        const output = execSync(`${pip} install jupyter ipykernel`, {
          encoding: "utf8", timeout: 120000,
          env: { ...process.env, PATH: `${process.env.PATH}:/usr/local/bin:/opt/homebrew/bin` },
        });
        return { success: true, output };
      } catch { /* try next */ }
    }
    return { success: false, output: "Could not find pip. Please install Python first." };
  });

  ipcMain.handle("crab:jupyter-start", async (_evt, kernelName: string, cwd: string, jupyterPath?: string) => {
    if (!JupyterKernelModule) return { id: null, status: "error", message: "Jupyter support not available (zeromq failed to load)" };
    const id = `kernel-${++kernelCounter}`;
    const effectivePath = jupyterPath || detectedJupyterPath || undefined;
    const kernel = new JupyterKernelModule.JupyterKernel(kernelName, cwd, effectivePath);
    kernel.setOutputHandler((output: any) => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("crab:jupyter-output", { id, ...output });
      }
    });
    kernel.setStatusHandler((status: any) => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("crab:jupyter-status", { id, status });
      }
    });
    try {
      await kernel.start();
      kernels.set(id, kernel);
      return { id, status: "ok" };
    } catch (e) {
      return { id: null, status: "error", message: e instanceof Error ? e.message : String(e) };
    }
  });

  ipcMain.handle("crab:jupyter-execute", async (_evt, id: string, code: string) => {
    const kernel = kernels.get(id);
    if (!kernel) return { error: "kernel not found" };
    try {
      const msgId = await kernel.execute(code);
      return { msgId };
    } catch (e) {
      return { error: e instanceof Error ? e.message : String(e) };
    }
  });

  ipcMain.handle("crab:jupyter-interrupt", async (_evt, id: string) => {
    const kernel = kernels.get(id);
    if (kernel) await kernel.interrupt();
  });

  ipcMain.handle("crab:jupyter-restart", async (_evt, id: string) => {
    const kernel = kernels.get(id);
    if (kernel) await kernel.restart();
  });

  ipcMain.handle("crab:jupyter-shutdown", async (_evt, id: string) => {
    const kernel = kernels.get(id);
    if (kernel) {
      await kernel.shutdown();
      kernels.delete(id);
    }
  });

  // ---- Workspace IPC handlers ----

  ipcMain.handle("workspace:pick-directory", async () => {
    if (!mainWindow) return null;
    const res = await dialog.showOpenDialog(mainWindow, {
      title: "Open Workspace",
      properties: ["openDirectory"],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    return res.filePaths[0];
  });

  ipcMain.handle("workspace:create", async () => {
    if (!mainWindow) return { ok: false, error: "No window" };
    const res = await dialog.showOpenDialog(mainWindow, {
      title: "Create Workspace — Choose Root Folder",
      properties: ["openDirectory", "createDirectory"],
    });
    if (res.canceled || res.filePaths.length === 0) return { ok: false, error: "cancelled" };
    const rootPath = res.filePaths[0];

    // Write initial workspace config
    const config = defaultWorkspaceConfig();
    await writeWorkspaceConfig(rootPath, config);

    // Now open the workspace (reuses existing open logic)
    // Stop any existing poller
    if (workspacePoller) {
      workspacePoller.stop();
      workspacePoller = null;
    }
    workspaceRootPath = rootPath;
    workspaceConfig = config;

    // Reload .env credentials scoped to this workspace so the agent
    // and any crab CLI invocations pick up repo-specific storage config.
    loadEnvLayered(rootPath);

    // Run discovery
    const repos = await discoverRepos(
      {
        rootPath,
        maxDepth: config.discovery.maxDepth,
        excludePatterns: config.discovery.excludePatterns,
      },
      { added: [], removed: config.repos.removed },
    );
    workspaceRepos = repos;

    // Send discovered repos to renderer (include rootPath so the
    // renderer doesn't depend on localStorage for the initial set).
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("workspace:discovered", { rootPath, repos });
    }

    // Start status poller — pass registry/rpcRouter/sessionManager so
    // the poller can handle remote repos via the provider-based path.
    const repoPaths = repos.map((r) => r.relativePath);
    workspacePoller = new StatusPoller(
      rootPath,
      repoPaths,
      undefined,
      getProviderRegistry(),
      _rpcRouter,
      _sshSessionManager,
    );
    workspacePoller.on("status", (statuses: RepoStatus[]) => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("workspace:status-update", statuses);
      }
    });
    workspacePoller.start();

    return { ok: true, rootPath, repoCount: repos.length };
  });

  ipcMain.handle("workspace:open", async (_evt, params: { rootPath: string }) => {
    const { rootPath } = params;

    // Stop any existing poller
    if (workspacePoller) {
      workspacePoller.stop();
      workspacePoller = null;
    }

    workspaceRootPath = rootPath;

    // Reload .env credentials scoped to this workspace so the agent
    // and any crab CLI invocations pick up repo-specific storage config.
    loadEnvLayered(rootPath);

    // Read config (or use defaults)
    const config = (await readWorkspaceConfig(rootPath)) ?? defaultWorkspaceConfig();
    workspaceConfig = config;

    // Run discovery
    const repos = await discoverRepos(
      {
        rootPath,
        maxDepth: config.discovery.maxDepth,
        excludePatterns: config.discovery.excludePatterns,
      },
      { added: [], removed: config.repos.removed },
    );
    workspaceRepos = repos;

    // Send discovered repos to renderer (include rootPath so the
    // renderer doesn't depend on localStorage for the initial set).
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("workspace:discovered", { rootPath, repos });
    }

    // Start status poller — pass registry/rpcRouter/sessionManager so
    // the poller can handle remote repos via the provider-based path.
    const repoPaths = repos.map((r) => r.relativePath);
    workspacePoller = new StatusPoller(
      rootPath,
      repoPaths,
      undefined,
      getProviderRegistry(),
      _rpcRouter,
      _sshSessionManager,
    );
    workspacePoller.on("status", (statuses: RepoStatus[]) => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("workspace:status-update", statuses);
      }
    });
    workspacePoller.start();

    return { ok: true, repoCount: repos.length };
  });

  ipcMain.handle("workspace:close", async () => {
    if (workspacePoller) {
      workspacePoller.stop();
      workspacePoller = null;
    }
    workspaceRootPath = null;
    workspaceConfig = null;
    workspaceRepos = [];
    return { ok: true };
  });

  ipcMain.handle("workspace:refresh", async () => {
    if (!workspaceRootPath || !workspaceConfig) {
      return { ok: false, error: "No workspace open" };
    }

    // Re-run discovery with current config
    const repos = await discoverRepos(
      {
        rootPath: workspaceRootPath,
        maxDepth: workspaceConfig.discovery.maxDepth,
        excludePatterns: workspaceConfig.discovery.excludePatterns,
      },
      { added: [], removed: workspaceConfig.repos.removed },
    );
    workspaceRepos = repos;

    // Send updated repos to renderer
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("workspace:discovered", { rootPath: workspaceRootPath, repos });
    }

    // Update poller repo list
    if (workspacePoller) {
      workspacePoller.updateRepos(repos.map((r) => r.relativePath));
    }

    return { ok: true, repoCount: repos.length };
  });

  ipcMain.handle("workspace:update-config", async (_evt, partial: Partial<WorkspaceConfig>) => {
    if (!workspaceRootPath || !workspaceConfig) {
      return { ok: false, error: "No workspace open" };
    }

    // Merge partial config into current config
    const merged: WorkspaceConfig = {
      ...workspaceConfig,
      discovery: {
        ...workspaceConfig.discovery,
        ...(partial.discovery ?? {}),
      },
      repos: {
        ...workspaceConfig.repos,
        ...(partial.repos ?? {}),
      },
      display: {
        ...workspaceConfig.display,
        ...(partial.display ?? {}),
      },
    };
    workspaceConfig = merged;

    // Write to disk
    await writeWorkspaceConfig(workspaceRootPath, merged);

    // Re-run discovery if discovery settings changed
    if (partial.discovery || partial.repos) {
      const repos = await discoverRepos(
        {
          rootPath: workspaceRootPath,
          maxDepth: merged.discovery.maxDepth,
          excludePatterns: merged.discovery.excludePatterns,
        },
        { added: [], removed: merged.repos.removed },
      );
      workspaceRepos = repos;

      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("workspace:discovered", { rootPath: workspaceRootPath, repos });
      }

      if (workspacePoller) {
        workspacePoller.updateRepos(repos.map((r) => r.relativePath));
      }
    }

    return { ok: true };
  });

  ipcMain.handle("workspace:remove-repo", async (_evt, params: { relativePath: string }) => {
    if (!workspaceRootPath || !workspaceConfig) {
      return { ok: false, error: "No workspace open" };
    }

    const { relativePath } = params;

    // Add to removed list so it's excluded from future discovery
    if (!workspaceConfig.repos.removed.includes(relativePath)) {
      workspaceConfig.repos.removed.push(relativePath);
    }

    await writeWorkspaceConfig(workspaceRootPath, workspaceConfig);

    // Re-run discovery
    const repos = await discoverRepos(
      {
        rootPath: workspaceRootPath,
        maxDepth: workspaceConfig.discovery.maxDepth,
        excludePatterns: workspaceConfig.discovery.excludePatterns,
      },
      { added: [], removed: workspaceConfig.repos.removed },
    );
    workspaceRepos = repos;

    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("workspace:discovered", { rootPath: workspaceRootPath, repos });
    }

    if (workspacePoller) {
      workspacePoller.updateRepos(repos.map((r) => r.relativePath));
    }

    return { ok: true };
  });

  ipcMain.handle("workspace:fetch-all", async () => {
    if (!workspaceRootPath || workspaceRepos.length === 0) {
      return { ok: true, results: [] };
    }

    const registry = getProviderRegistry();

    // All repos in a local workspace are local — fetch them directly
    const localRepos = workspaceRepos.map((repo) => ({
      relativePath: repo.relativePath,
      absolutePath: path.resolve(workspaceRootPath!, repo.relativePath),
    }));

    // Execute local fetches — batched in groups of 5 to avoid overwhelming the system
    const results: Array<{ relativePath: string; ok: boolean; error?: string }> = [];
    const BATCH_SIZE = 5;
    for (let i = 0; i < localRepos.length; i += BATCH_SIZE) {
      const batch = localRepos.slice(i, i + BATCH_SIZE);
      const batchResults = await Promise.all(
        batch.map(async (repo) => {
          // Try provider first, fall back to execFile
          const provider = registry.resolve(repo.absolutePath);
          if (provider) {
            try {
              await provider.request("git_fetch_all", { repo_id: repo.absolutePath });
              return { relativePath: repo.relativePath, ok: true };
            } catch {
              // Provider failed — fall back to direct git command
            }
          }

          return new Promise<{ relativePath: string; ok: boolean; error?: string }>((resolve) => {
            execFile(
              "git",
              ["fetch", "--all"],
              { cwd: repo.absolutePath, timeout: 30000 },
              (error, _stdout, stderr) => {
                if (error) {
                  resolve({
                    relativePath: repo.relativePath,
                    ok: false,
                    error: stderr?.trim() || error.message,
                  });
                } else {
                  resolve({ relativePath: repo.relativePath, ok: true });
                }
              },
            );
          });
        }),
      );
      results.push(...batchResults);
    }

    // Trigger a status poll refresh after fetch
    if (workspacePoller) {
      workspacePoller.pollNow().then((statuses) => {
        if (mainWindow && !mainWindow.isDestroyed()) {
          mainWindow.webContents.send("workspace:status-update", statuses);
        }
      });
    }

    const hasErrors = results.some((r) => !r.ok);
    return { ok: !hasErrors, results };
  });

  ipcMain.handle("workspace:pull-all", async () => {
    if (!workspaceRootPath || workspaceRepos.length === 0) {
      return { ok: true, results: [] };
    }

    const registry = getProviderRegistry();

    // Get current branch info from the last poller status
    const currentStatuses = workspacePoller ? await workspacePoller.pollNow() : [];
    const branchByPath = new Map(
      currentStatuses.map((s) => [s.relativePath, s.branch]),
    );

    // All repos in a local workspace are local
    const localRepos: Array<{ relativePath: string; absolutePath: string; branch: string }> = [];
    const skipped: Array<{ relativePath: string; ok: boolean; error?: string }> = [];

    for (const repo of workspaceRepos) {
      const branch = branchByPath.get(repo.relativePath);
      if (!branch) {
        skipped.push({ relativePath: repo.relativePath, ok: false, error: "No branch detected" });
        continue;
      }
      localRepos.push({
        relativePath: repo.relativePath,
        absolutePath: path.resolve(workspaceRootPath, repo.relativePath),
        branch,
      });
    }

    // Execute local pulls — batched in groups of 5
    const pullResults: Array<{ relativePath: string; ok: boolean; error?: string }> = [];
    const BATCH_SIZE = 5;
    for (let i = 0; i < localRepos.length; i += BATCH_SIZE) {
      const batch = localRepos.slice(i, i + BATCH_SIZE);
      const batchResults = await Promise.all(
        batch.map(async (repo) => {
          // Try provider first, fall back to execFile
          const provider = registry.resolve(repo.absolutePath);
          if (provider) {
            try {
              await provider.request("git_pull_branch", { repo_id: repo.absolutePath, branch: repo.branch });
              return { relativePath: repo.relativePath, ok: true };
            } catch {
              // Provider failed — fall back to direct git command
            }
          }

          return new Promise<{ relativePath: string; ok: boolean; error?: string }>((resolve) => {
            execFile(
              "git",
              ["pull", "origin", repo.branch],
              { cwd: repo.absolutePath, timeout: 60000 },
              (error, _stdout, stderr) => {
                if (error) {
                  resolve({
                    relativePath: repo.relativePath,
                    ok: false,
                    error: stderr?.trim() || error.message,
                  });
                } else {
                  resolve({ relativePath: repo.relativePath, ok: true });
                }
              },
            );
          });
        }),
      );
      pullResults.push(...batchResults);
    }

    // Merge all results
    const results = [...skipped, ...pullResults];

    // Trigger a status poll refresh after pull
    if (workspacePoller) {
      workspacePoller.pollNow().then((statuses) => {
        if (mainWindow && !mainWindow.isDestroyed()) {
          mainWindow.webContents.send("workspace:status-update", statuses);
        }
      });
    }

    const hasErrors = results.some((r) => !r.ok);
    return { ok: !hasErrors, results };
  });

  return { getDefaultShell, buildTerminalEnv, shellEnvPromise, writeSessionLog };
}

app.whenReady().then(async () => {
  browserManager.setupSessionPolicies(app);
  // Skill Manager initialization (before IPC so orchestrator can inject skills)
  const skillManager = new SkillManager();
  await skillManager.initialize();
  registerSkillManagerIpc(ipcMain, skillManager, () => mainWindow);

  const { getDefaultShell, buildTerminalEnv, shellEnvPromise, writeSessionLog } = registerIpc(skillManager);
  registerSidecarIpc(sidecarManager);
  registerLspIpc(lspManager, () => mainWindow);
  registerFileWatcher(() => mainWindow);

  // SSH subsystem initialization
  const sshProfileManager = new SshProfileManager();
  const sshDatabase = new SshDatabase();
  const db = sshDatabase.getDb();

  // Core SSH modules
  const sshSessionManager = new SshSessionManager();
  _sshSessionManager = sshSessionManager;
  const sshTerminalManager = new RemoteTerminalManager(sshSessionManager, id => sshProfileManager.get(id));
  const sshPortForwardManager = new PortForwardManager(sshSessionManager);
  registerSshIpcHandlers(sshProfileManager, sshSessionManager, sshTerminalManager, sshPortForwardManager);

  // Enhanced SSH modules — wired after basic handlers so the app starts
  // even if any enhanced module fails to initialize.
  try {
    const rpcRouter = new RpcRouter(sshSessionManager, {
      request: async (method: string, params: unknown) => {
        if (!agent) throw new Error("agent not running");
        return agent.request(method, params);
      },
    });
    _rpcRouter = rpcRouter;
    const agentDeployer = new AgentDeployer();
    const capabilityNegotiator = new CapabilityNegotiator(rpcRouter);
    const fileTransferManager = new FileTransferManager(sshSessionManager);
    const deltaSyncEngine = new DeltaSyncEngine(rpcRouter);
    const workspaceSyncEngine = new WorkspaceSyncEngine(
      rpcRouter,
      sshSessionManager,
      deltaSyncEngine,
      fileTransferManager,
      db,
      capabilityNegotiator,
    );
    const transferHistoryStore = new TransferHistoryStore(db);
    const transferScheduler = new TransferScheduler(fileTransferManager, sshSessionManager, db);
    const latencyMonitor = new SshLatencyMonitor(rpcRouter, sshSessionManager);
    const bandwidthThrottler = new BandwidthThrottler(latencyMonitor);
    const transferCompressor = new TransferCompressor();
    const processManager = new RemoteProcessManager(rpcRouter, sshSessionManager);
    const environmentManager = new RemoteEnvironmentManager(rpcRouter, sshSessionManager);
    const toolManager = new RemoteToolManager(rpcRouter, sshSessionManager);
    const connectionQualityMonitor = new ConnectionQualityMonitor(latencyMonitor, rpcRouter, sshSessionManager);
    const bookmarkManager = new BookmarkManager(db);
    const fileEditor = new RemoteFileEditor(rpcRouter, sshSessionManager);
    const workspaceManager = new RemoteWorkspaceManager(rpcRouter, sshSessionManager, fileEditor);
    const gitManager = new RemoteGitManager(rpcRouter, sshSessionManager);
    const debugManager = new RemoteDebugManager(rpcRouter, sshSessionManager);

    // Create the RemoteProvider singleton and register it with the
    // ProviderRegistry. All remote repos share a single RemoteProvider
    // instance that routes through the RpcRouter.
    const remoteProvider = new RemoteProvider({
      router: rpcRouter,
      fileEditor,
      gitManager,
      capabilityNegotiator,
      sessionManager: sshSessionManager,
    });
    const registry = getProviderRegistry();

    // Bridge RemoteProvider's fs/changed events to the renderer so the
    // useFileWatcher hook picks them up. This mirrors how the local
    // file-watcher.ts sends events via webContents.send("crab:event", ...).
    remoteProvider.on("fs/changed", (evt) => {
      if (!mainWindow || mainWindow.isDestroyed() || mainWindow.webContents.isDestroyed()) return;
      mainWindow.webContents.send("crab:event", {
        method: "fs/changed",
        params: { repo_path: evt.repoPath },
      });
    });

    // Bridge RemoteProvider's capabilities-changed events to the renderer
    // so the UI can reactively enable/disable controls when the SSH
    // connection state transitions (connected ↔ disconnected ↔ reconnecting).
    remoteProvider.on("capabilities-changed", (evt) => {
      if (!mainWindow || mainWindow.isDestroyed() || mainWindow.webContents.isDestroyed()) return;
      mainWindow.webContents.send("crab:event", {
        method: "repo:capabilities-changed",
        params: { repo_id: evt.repoId, capabilities: evt.capabilities },
      });
    });

    // Register the ssh:open-as-repo IPC handler with provider registration.
    // This replaces the stub handler (if any) and ensures the RemoteProvider
    // is registered for the repo_id before the handle is returned to the renderer.
    try { ipcMain.removeHandler("ssh:open-as-repo"); } catch { /* no existing handler */ }
    ipcMain.handle(
      "ssh:open-as-repo",
      async (_event: unknown, profileId: string, remotePath: string) => {
        const status = sshSessionManager.getStatus(profileId);
        if (status !== "connected") {
          throw new Error(`SSH connection not available (status: ${status})`);
        }

        const result = await rpcRouter.sendToRemoteAgent(profileId, "open_local", {
          path: remotePath,
        });

        // Register the RemoteProvider for this repo_id BEFORE returning
        // the handle to the renderer (prevents race conditions).
        const repoId = `ssh://${profileId}${remotePath}`;
        if (!registry.has(repoId)) {
          registry.register(repoId, remoteProvider);
        }

        // Populate forge_provider on the RepoHandle if the remote agent
        // didn't return it (older agent versions or detection failure).
        const handle = result as Record<string, unknown> | null;
        if (handle && !handle.forge_provider) {
          const { detectForgeFromUrl } = await import("./forge-detect.js");
          try {
            const configResult = await rpcRouter.sendToRemoteAgent(
              profileId,
              "git_config_get",
              { repo_id: repoId, keys: ["remote.origin.url"] },
            ) as { values?: Record<string, string | null> };
            const remoteUrl = configResult?.values?.["remote.origin.url"];
            if (remoteUrl) {
              const forgeProvider = detectForgeFromUrl(remoteUrl);
              if (forgeProvider) {
                handle.forge_provider = forgeProvider;
              }
            }
          } catch {
            // Forge detection is best-effort — don't block repo open.
          }
        }

        // Start remote file watching automatically when a remote repo
        // is opened. Fire-and-forget — watching is a UX enhancement.
        void remoteProvider.watchStart(repoId);

        return result;
      },
    );

    wireEnhancedIpcHandlers({
      fileTransferManager,
      bandwidthThrottler,
      transferScheduler,
      transferHistoryStore,
      transferCompressor,
      workspaceSyncEngine,
      processManager,
      environmentManager,
      toolManager,
      connectionQualityMonitor,
      bookmarkManager,
      agentDeployer,
      capabilityNegotiator,
      workspaceManager,
      gitManager,
      debugManager,
      sessionManager: sshSessionManager,
    });

    // Bridge sync and deploy events to the renderer
    bridgeSyncEventsToRenderer(workspaceSyncEngine, fileTransferManager);
    bridgeAgentDeployToRenderer(sshSessionManager);

    // Remote workspace orchestrator — multi-repo discovery and polling
    // for remote workspace sources over SSH.
    const remoteWorkspaceOrchestrator = new RemoteWorkspaceOrchestrator(rpcRouter, sshSessionManager);
    registerRemoteWorkspaceIpc(remoteWorkspaceOrchestrator);

    // When remote repos are discovered, feed their ssh:// formatted IDs
    // into the StatusPoller alongside existing local repo paths. The
    // poller classifies them via parseRepoId() and batches per SSH profile.
    remoteWorkspaceOrchestrator.on(
      "discovered",
      () => {
        if (!workspacePoller) return;

        // Collect current local repo paths
        const localPaths = workspaceRepos.map((r) => r.relativePath);

        // Build ssh:// formatted IDs for all remote repos across all open workspaces
        const remoteRepoIds: string[] = [];
        for (const ws of remoteWorkspaceOrchestrator.getOpenWorkspaces()) {
          for (const repo of ws.repos) {
            remoteRepoIds.push(`ssh://${ws.profileId}${repo.absolute_path}`);
          }
        }

        workspacePoller.updateRepos([...localPaths, ...remoteRepoIds]);
      },
    );

  } catch (err) {
    console.error("[crab-desktop] Failed to initialize enhanced SSH modules:", err);
  }

  // Open the window *first* so the user always has visible UI,
  // regardless of whether the agent comes up cleanly. Otherwise a
  // slow or crashed sidecar leaves the app as a bouncing dock icon
  // with no explanation — a truly awful failure mode.
  try {
    await createWindow();
  } catch (err) {
    const message = err instanceof Error ? err.stack || err.message : String(err);
    await dialog.showMessageBox({
      type: "error",
      message: "Failed to open Crab Desktop window",
      detail: message,
    });
    app.quit();
    return;
  }

  // Auto-updater — check for updates after the window is visible.
  // Skipped in dev mode since there's no published release to check.
  if (!isDev && mainWindow) {
    initAutoUpdater(mainWindow);
  }

  // ---- PTY terminal routing (TermRouter) ----
  // Build the in-process bridge unconditionally, then optionally build
  // the daemon subsystem. Both bridges live for the app lifetime — no
  // removeHandler calls needed.

  const inProcessBridge = createInProcessBridge({
    getMainWindow: () => mainWindow,
    getDefaultShell,
    buildTerminalEnv,
    shellEnvPromise,
    writeSessionLog,
    app,
  });

  let daemonSubsystem: Awaited<ReturnType<typeof initializeDaemonSubsystem>> | null = null;

  try {
    daemonSubsystem = await initializeDaemonSubsystem({
      ipcMain,
      app,
      getMainWindow: () => mainWindow,
      getWorkspacePath: () => workspaceRootPath,
      log: (level, msg) => {
        if (level === "error") console.error(`[pty-daemon] ${msg}`);
        else if (level === "warn") console.warn(`[pty-daemon] ${msg}`);
        else console.log(`[pty-daemon] ${msg}`);
      },
      isPackaged: app.isPackaged,
      resourcesPath: process.resourcesPath,
    });
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Ordinary shells require durable ownership. Explicit local terminals
    // remain available when daemon startup fails.
    console.warn(`[crab-desktop] PTY daemon unavailable (open an explicit local terminal or retry): ${msg}`);
    // Register fallback IPC handlers so the renderer doesn't get
    // "No handler registered" errors when daemon subsystem failed entirely.
    try {
      ipcMain.handle("crab:pty-mode", () => ({
        resolvedMode: "in-process",
        configuredMode: parsePtyMode(process.env.CRAB_PTY_MODE),
        fallback: { reason: "Daemon subsystem init failed", error: msg, timestamp: Date.now() },
      }));
    } catch { /* already registered */ }
    try {
      ipcMain.handle("crab:agent-sessions", () => []);
    } catch { /* already registered */ }
    // Surface to the renderer so the agent dashboard can show a notice.
    // Regular terminal usage is unaffected.
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("crab:daemon-fatal", { reason: msg });
    }
  }

  const daemonReady = daemonSubsystem?.daemonReady ?? false;
  const daemonBridgeHandle = daemonSubsystem?.daemonBridge ?? null;

  // Adapter: TermRouter's DaemonBridge interface expects
  // handleSpawn(payload, rendererId). The daemon bridge now accepts an
  // optional externalRendererId so the router's ID is used for all
  // session mappings and data event forwarding.
  const daemonBridgeForRouter = {
    handleSpawn: async (payload: Parameters<typeof inProcessBridge.handleSpawn>[0], rendererId: string) => {
      if (!daemonBridgeHandle) throw new Error("Daemon not available");
      await shellEnvPromise;
      const result = await daemonBridgeHandle.handleSpawn({
        ...payload,
        shell: payload.shell || getDefaultShell(),
        env: buildTerminalEnv(payload.env),
      }, rendererId);
      return { daemonSessionId: result.id };
    },
    handleWrite: async (id: string, data: string) => {
      await daemonBridgeHandle?.handleWrite(id, data);
    },
    handleResize: async (id: string, cols: number, rows: number) => {
      await daemonBridgeHandle?.handleResize(id, cols, rows);
    },
    handleDetach: async (id: string) => {
      await daemonBridgeHandle?.handleDetach(id);
    },
    handleKill: async (id: string) => {
      await daemonBridgeHandle?.handleKill(id);
    },
    handleSessionData: (id: string, data: string) => {
      daemonBridgeHandle?.handleSessionData(id, data);
    },
    handleSessionExit: (id: string, code: number) => {
      daemonBridgeHandle?.handleSessionExit(id, code);
    },
    dispose: () => {
      daemonBridgeHandle?.dispose();
    },
  };

  // Adapter: TermRouter's InProcessBridge interface expects
  // handleSpawn(payload, rendererId) and handleWrite/Resize/Kill(id, ...).
  // InProcessBridge methods take an extra leading _evt parameter.
  const inProcessBridgeForRouter = {
    handleSpawn: async (payload: Parameters<typeof inProcessBridge.handleSpawn>[0], rendererId: string) => {
      await inProcessBridge.handleSpawn(payload, rendererId);
    },
    handleWrite: async (id: string, data: string) => {
      inProcessBridge.handleWrite(null, id, data);
    },
    handleResize: async (id: string, cols: number, rows: number) => {
      inProcessBridge.handleResize(null, id, cols, rows);
    },
    handleKill: async (id: string) => {
      inProcessBridge.handleKill(null, id);
    },
    dispose: () => inProcessBridge.dispose(),
  };

  // Fallback no-op circuit breaker when daemon subsystem is unavailable.
  const noopCircuitBreaker = {
    shouldAttemptDaemon: false as boolean,
    getSnapshot: () => ({ state: "open" as const, cooldownRemainingMs: 0 }),
    recordFailure: () => {},
    recordSuccess: () => {},
  };

  const termRouter = new TermRouter({
    ipcMain,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    daemonBridge: daemonBridgeForRouter as any,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    inProcessBridge: inProcessBridgeForRouter as any,
    modeResolution: {
      // Dynamic: reflects current daemon connection state so reconnects
      // after auto-respawn immediately enable daemon-backed sessions.
      current: () => ({
        daemonReady: daemonSubsystem?.daemonClient?.isConnected ?? false,
        configuredMode: parsePtyMode(process.env.CRAB_PTY_MODE),
      }),
    },
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    circuitBreaker: (daemonSubsystem?.circuitBreaker ?? noopCircuitBreaker) as any,
    getMainWindow: () => mainWindow,
  });
  termRouter.register();

  if (daemonReady && daemonSubsystem) {
    // Register diagnostics IPC handlers for the Health & Diagnostics panel.
    registerDiagnosticsUiIpc({
      ipcMain,
      client: daemonSubsystem.daemonClient,
      circuitBreaker: daemonSubsystem.circuitBreaker,
      autoRespawn: daemonSubsystem.autoRespawn,
      getActiveSessionCount: () => termRouter.getSessionTypeBreakdown().daemon,
      getMode: () =>
        daemonSubsystem!.daemonClient.isConnected ? "daemon" : "in-process",
      pathMismatch: daemonSubsystem.pathMismatch,
      previousEntryPath: daemonSubsystem.previousEntryPath,
      daemonEntryFromEnv: daemonSubsystem.daemonEntryFromEnv,
      getSessionTypeBreakdown: () => termRouter.getSessionTypeBreakdown(),
    });

    console.log("[crab-desktop] PTY daemon mode active — both bridges registered");

    registerTerminalSessionIpc(ipcMain, daemonSubsystem.daemonClient, daemonSubsystem.daemonBridge, termRouter);
  } else {
    console.log("[crab-desktop] PTY daemon unavailable — in-process bridge only");
    // Register no-op daemon handlers so renderer doesn't get IPC errors.
    ipcMain.handle("crab:term-list-daemon-sessions", () => []);
    ipcMain.handle("crab:term-attach-daemon-session", () => {
      throw new Error("PTY daemon is not available");
    });
    ipcMain.handle("crab:term-detach-daemon-session", () => undefined);
    ipcMain.handle("crab:term-kill-daemon-session", () => {
      throw new Error("PTY daemon is not available");
    });
    ipcMain.handle("crab:term-reattach-sessions", () => []);
  }

  app.on("before-quit", () => {
    termRouter.dispose();
    daemonSubsystem?.dispose();
  });

  // Spin up the agent in the background. If it fails, surface the
  // error via IPC so the renderer can render a banner; otherwise the
  // renderer's first RPC call transparently waits for `start()` to
  // finish.
  try {
    try {
      const shellEnv = await shellEnvPromise;
      if (shellEnv) {
        Object.assign(process.env, shellEnv);
      }
    } catch (err) {
      console.warn("[crab-desktop] Failed to resolve shell environment before starting agent:", err);
    }

    const mainSettings = readMainSettings();

    // Persist only main-process-redacted logs; the native file sink would
    // write provider diagnostics before saved credentials can be removed.
    const logsDir = isDev ? undefined : agentLogsDir();
    if (logsDir) fs.mkdirSync(logsDir, { recursive: true, mode: 0o700 });

    const runtime = nativeBinaries({
      packaged: app.isPackaged,
      appPath: app.getAppPath(),
      resourcesPath: process.resourcesPath,
      platform: process.platform,
      arch: process.arch,
    });
    // Main-process worktrees, Git helpers and the sidecar must use the same release.
    process.env.CRAB_BIN = runtime.crab;
    process.env.PATH = `${path.dirname(runtime.crab)}${path.delimiter}${process.env.PATH ?? ""}`;
    const storageConnections = new StorageConnections(
      path.join(app.getPath("userData"), "storage-connections.enc"), safeStorage, process.platform,
    );
    agent = new Agent(runtime.agent, agentEnvFor(mainSettings.agentLogLevel), [], storageConnections);
    if (logsDir) agent.on("stderr", (line: string) => {
      const day = new Date().toISOString().slice(0, 10);
      fs.appendFile(path.join(logsDir, `agent.log.${day}`), `${line}\n`, { mode: 0o600 }, error => {
        if (error) console.warn("[crab-desktop] Could not append agent log.");
      });
    });
    agent.on("notification", (method, params) => {
      if (!mainWindow || mainWindow.isDestroyed() || mainWindow.webContents.isDestroyed()) {
        return;
      }
      mainWindow.webContents.send("crab:event", { method, params });
    });
    agent.on("exit", (code, signal) => {
      if (!mainWindow || mainWindow.isDestroyed() || mainWindow.webContents.isDestroyed()) {
        return;
      }
      mainWindow.webContents.send("crab:event", {
        method: "agent/exit",
        params: { code, signal },
      });
    });

    // Register the OAuth device flow handler so `auth/start`
    // notifications from the agent trigger browser-open + token polling.
    registerForgeAuth(agent, () => mainWindow);

    // Initialize the ProviderRegistry with the LocalProvider singleton.
    // This must happen after the agent is created so the LocalProvider
    // can delegate RPC calls to it.
    const registry = getProviderRegistry();
    const localProvider = new LocalProvider(agent);
    registry.setLocalProvider(localProvider);

    // Fire-and-forget; we don't want to block the window on the
    // agent's ping round-trip. Failures bubble into the catch below.
    agent.start().then(resolveAgentReady).catch((err) => {
      rejectAgentReady(err);
      const message = err instanceof Error ? err.message : String(err);
      mainWindow?.webContents.send("crab:event", {
        method: "agent/failed",
        params: { message },
      });
    });
  } catch (err) {
    rejectAgentReady(err);
    const message = err instanceof Error ? err.message : String(err);
    mainWindow?.webContents.send("crab:event", {
      method: "agent/failed",
      params: { message },
    });
  }

  // ── SSH Remote Activity Bridge ──────────────────────────────────
  // When the SSH subsystem creates an RPC router, call
  // bridgeRemoteActivityFeed(router, orchestrator) to connect the
  // remote agent's notification stream to the orchestrator. This
  // ensures remote task events (task_event, task_completed,
  // task_failed) flow through the same pipeline as local tasks and
  // display identically in the Task Panel.
  //
  // The bridge is wired lazily: the SSH subsystem calls
  // bridgeRemoteActivityFeed() when a connection is established and
  // the router is ready. The orchestrator instance is available via
  // the module-level `orchestrator` variable set by registerIpc().
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) void createWindow();
});

// Save terminal state before the app quits. The renderer sends its
// layout via IPC when it receives the "before-quit" signal.
app.on("before-quit", () => {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("crab:term-save-request");
  }
});

registerAppShutdown(app, [
  stopAllWatchers,
  stopAutoUpdater,
  () => shutdownAgentServices?.(),
  () => agent?.shutdown(),
  () => sidecarManager.stopAll(),
  () => lspManager.stopAll(),
]);
