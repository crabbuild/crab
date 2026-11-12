import palettes from "../theme-tokens.json";
import { app,nativeTheme,BrowserWindow,dialog,ipcMain,Notification,shell,safeStorage,protocol,webContents } from "electron";
import * as path from "node:path";
import * as fs from "node:fs";
import * as os from "node:os";
import { execFile } from "node:child_process";

import { Agent } from "./agent";
import { JupyterKernel, checkJupyterRuntime, installJupyter, listKernelSpecs, stopJupyterRuntimeCommands } from "./jupyter-kernel";
import type { KernelTarget } from "./jupyter-types";
import type { WebContents } from "electron";
import { nativeSessionLost, noCapabilities } from "./repo-capabilities";
import type { CapabilitiesChangedEvent } from "./repo-provider";
import { rpcReply } from "./rpc-transport";
import type Database from "better-sqlite3";
import { FileTransfers, isFileTransfer } from "./file-transfers";
import { MediaResources } from "./media";
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
import { RemoteRuntimeStore } from "./ssh-runtime";
import { KnownHostsManager, createHostKeyVerifier } from "./ssh-known-hosts";
import { confirmSshHostKey } from "./ssh-host-key-dialog";
import { registerSshIpcHandlers } from "./ssh-ipc";
import { RemoteTerminalManager } from "./ssh-terminal";
import { PortForwardManager } from "./ssh-port-forward";
import { SshDatabase } from "./ssh-database";
import { parseRepoId } from "./ssh-types";
import { RpcRouter } from "./ssh-router";
import { getProviderRegistry } from "./provider-registry";
import { LocalProvider } from "./local-provider";
import { readTextFile } from "./file-read";
import { stopFileWrites } from "./file-write";
import { RemoteProvider } from "./remote-provider";
import { AgentDeployer } from "./ssh-deployer";
import { RemoteFileBrowser } from "./ssh-file-browser";
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

// One main process owns each profile's transfer receipts. A second process
// must not recover a live owner's jobs as interrupted before it opens SQLite.
if (!app.requestSingleInstanceLock()) app.exit(0);
app.on("second-instance", () => {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.show(); mainWindow.focus();
});

// Load credentials from ~/.config/crab/.env early so that the agent
// subprocess (and any crab CLI invocations) inherit AWS/storage creds
// even though macOS GUI apps don't get the user's shell environment.
loadEnvGlobal();

protocol.registerSchemesAsPrivileged([{ scheme: "crab-content", privileges: { standard: true, secure: true, stream: true, supportFetchAPI: true, corsEnabled: true } }]);
let media: MediaResources | null = null;
let fileTransfers: FileTransfers | null = null;
let activityError: string | null = null;
let desktopDatabase: SshDatabase | null = null;

let mainWindow: BrowserWindow | null = null;

function admitFileTransfer(method: string, params: unknown): void { fileTransfers?.fence(method, params); }

function sendAgentEvent(method: string, params: unknown): void {
  fileTransfers?.observe(method, params);
  if (!mainWindow || mainWindow.isDestroyed() || mainWindow.webContents.isDestroyed()) return;
  mainWindow.webContents.send("crab:event", { method, params });
}

function sendRepositoryCapabilities(event: CapabilitiesChangedEvent): void {
  if (event.nativeSessionId === null) fileTransfers?.forget(event.repoId);
  if (event.nativeSessionId === null) void media?.invalidateRepo(event.repoId).catch(error => console.error("Media invalidation failed", error));
  sendAgentEvent("repo:capabilities-changed", { repo_id: event.repoId, capabilities: event.capabilities, native_session_id: event.nativeSessionId, error: event.error });
}

let agent: Agent | null = null;
const sidecarManager = new SidecarManager();
const lspManager = new LspManager();
let isShuttingDown = false;
let quitRequested = false;
const notebookKernels = new Map<string, { repoId: string; owner: WebContents; kernel: JupyterKernel; release: () => void }>();

async function closeNotebookKernel(id: string): Promise<void> {
  const entry = notebookKernels.get(id);
  if (!entry) return;
  try { await entry.kernel.shutdown(); }
  finally {
    if (notebookKernels.get(id) === entry) notebookKernels.delete(id);
    entry.release();
  }
}

async function stopNotebookKernels(repoId?: string): Promise<void> {
  const results = await Promise.allSettled([...notebookKernels].filter(([, entry]) => !repoId || entry.repoId === repoId).map(([id]) => closeNotebookKernel(id)));
  const failures = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
  if (failures.length) throw new AggregateError(failures.map(result => result.reason), "Notebook shutdown failed");
}
// Resolve the renderer's unsaved-buffer decision before tearing down native,
// terminal or SSH owners; cancelling Quit must leave the app fully usable.
app.on("before-quit", event => {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  event.preventDefault();
  if (quitRequested) return;
  quitRequested = true;
  mainWindow.close();
});
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

// The shared orchestrator receives its SSH router after subsystem startup.
import type { Orchestrator } from "./orchestrator";
let orchestrator: Orchestrator | null = null;

/**
 * Returns the shared local/remote task owner.
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
    backgroundColor: `rgb(${palettes[nativeTheme.shouldUseDarkColors ? "dark" : "light"].background})`,
    titleBarStyle: process.platform === "darwin" ? "hiddenInset" : "default",
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webviewTag: true,
    },
  });

  const window = mainWindow;
  const owner = window.webContents;
  window.on("close", () => { if (!owner.isDestroyed()) owner.send("crab:term-save-request"); });
  owner.on("will-prevent-unload", event => {
    const choice = dialog.showMessageBoxSync(window, {
      type: "warning", buttons: ["Keep editing", "Discard edits"], defaultId: 0, cancelId: 0,
      message: "Some file edits have not been saved.",
      detail: "Keep editing to retry saving. Discarding loses unsaved edits; Crab waits for saves already in progress.",
    });
    if (choice === 1) event.preventDefault();
    else quitRequested = false;
  });
  window.once("closed", () => {
    if (mainWindow === window) mainWindow = null;
    if (quitRequested) app.quit();
  });
  const ownerId = owner.id;
  const closeMedia = () => { void media?.closeOwner(ownerId).catch(error => console.error("Media cleanup failed", error)); };
  owner.on("did-start-navigation", (_event, _url, inPlace, mainFrame) => { if (mainFrame && !inPlace) closeMedia(); });
  owner.on("render-process-gone", closeMedia);
  owner.on("destroyed", closeMedia);

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

function registerIpc(skillManager: SkillManager, storageConnections: StorageConnections, localAgent: () => Promise<Agent>, db: Database.Database) {
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
  function initializeTransferActivity() {
    try {
      fileTransfers = new FileTransfers(db,
        repoId => dispatchAgent("close", { repo_id: repoId }).finally(() => fileTransfers?.forget(repoId)),
        (repoId, opId, session) => dispatchAgent("rpc/cancel", { repo_id: repoId, op_id: opId, native_session_id: session }),
        change => sendAgentEvent("activity/file-transfer", change),
        message => storageConnections.redactText(message));
      activityError = null;
    } catch (error) {
      activityError = `File-transfer Activity is unavailable: ${storageConnections.redactText(error instanceof Error ? error.message : String(error))}`;
      console.error(activityError);
    }
  }
  initializeTransferActivity();

  async function requestAgent(method: string, params: unknown): Promise<unknown> {
    if (method === "activity_file_transfers") return fileTransfers?.snapshot() ?? { version: 0, jobs: [], error: activityError };
    if (method === "activity_file_sources") {
      if (!fileTransfers) throw new Error(activityError ?? "File-transfer Activity is unavailable");
      const source = params as { job_id: string; start: number; limit: number };
      return fileTransfers.sources(source.job_id, source.start, source.limit);
    }
    if (method === "activity_file_transfers_recover") {
      if (!fileTransfers) initializeTransferActivity();
      if (!fileTransfers) throw new Error(activityError ?? "File-transfer Activity is unavailable");
      return fileTransfers.recover();
    }
    const repoId = extractRepoIdFromParams(params);
    const control = params as Record<string, unknown> | null;
    if (method === "rpc/cancel" && typeof control?.op_id === "string" && fileTransfers?.owns(control.op_id)) {
      return fileTransfers.requestCancel(String(repoId), control.op_id, String(control.native_session_id));
    }
    if (method === "close" && repoId && fileTransfers) return fileTransfers.close(repoId);
    if (isFileTransfer(method)) {
      if (!fileTransfers) throw new Error(activityError ?? "File-transfer Activity is unavailable");
      const repository = repoId ? storageConnections.repositoryContext(repoId) : undefined;
      if (!repository || !repoId) throw new Error("Reopen the repository before transferring files.");
      const provider = getProviderRegistry().resolve(repoId);
      const nativeSessionId = provider?.nativeSession(repoId) ?? null;
      const payload = params as Record<string, unknown>;
      const admitted = { ...payload, native_session_id: payload.native_session_id ?? nativeSessionId };
      return fileTransfers.run(method, admitted, { ...repository, nativeSessionId }, () => dispatchAgent(method, admitted));
    }
    const session = repoId && method === "file_open" ? getProviderRegistry().resolve(repoId)?.nativeSession(repoId) : null;
    const result = await dispatchAgent(method, params);
    if (repoId && method === "file_open" && session && getProviderRegistry().resolve(repoId)?.nativeSession(repoId) === session) {
      const payload = params as Record<string, unknown>;
      if (typeof payload.path === "string") fileTransfers?.capture(repoId, session, payload.path, result);
    }
    if (repoId && method === "file_close") fileTransfers?.forget(repoId, String((params as Record<string, unknown>).file_id));
    return result;
  }

  async function dispatchAgent(method: string, params: unknown): Promise<unknown> {
    if (method.startsWith("storage_connections_")) {
      if (method !== "storage_connections_test") {
        // Device metadata owns no native readiness. Identity changes still join
        // existing repository owners through the vault's shared callback.
        return storageConnections.request(method, params, () => { throw new Error("Unknown storage connection operation"); });
      }
      const repoId = extractRepoIdFromParams(params);
      const remote = repoId ? parseRepoId(repoId) : null;
      if (method === "storage_connections_test" && remote) {
        if (!_rpcRouter) throw new Error("SSH services are not initialized");
        return _rpcRouter.sendToRemoteAgent(remote.profileId, method, params);
      }
      return (await localAgent()).request(method, params);
    }
    const registry = getProviderRegistry();

    // Handle repo open methods: register provider before returning handle.
    if (method === "open_local" || method === "open_url") {
      const handle = await (await localAgent()).request(method, params);
      // Register the appropriate provider for this repo_id.
      const repoId = (handle as { repo_id?: string })?.repo_id;
      if (repoId) {
        const parsed = parseRepoId(repoId);
        if (parsed) {
          // SSH repository handles are registered by the SSH open path.
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
      if (repoId) await media?.closeRepo(repoId);
      if (repoId) await stopNotebookKernels(repoId);
      // Route the close call through the appropriate provider.
      let result: unknown;
      if (repoId) {
        const provider = registry.resolve(repoId);
        if (provider) {
          // Stop file watching before closing the repo.
          void provider.watchStop(repoId).catch(() => {});
          result = await provider.request(method, params as Record<string, unknown>);
        } else {
          result = await (await localAgent()).request(method, params);
        }
        registry.unregister(repoId);
      } else {
        result = await (await localAgent()).request(method, params);
      }
      return result;
    }

    // All other calls: resolve provider from repo_id and dispatch.
    const repoId = extractRepoIdFromParams(params);
    if (repoId) {
      const provider = registry.resolve(repoId);
      if (provider) {
        const payload = params as Record<string, unknown>;
        if (Object.hasOwn(payload, "native_session_id")) {
          const expected = payload.native_session_id;
          if (!expected || provider.nativeSession(repoId) !== expected) {
            // A revoked session drains under its original owner. Cleanup must
            // never send an old file identity into a replacement process.
            if (method === "file_close") return { closed: payload.file_id };
            throw nativeSessionLost();
          }
          // Keep the expected identity until the owner admits the wire request;
          // a repository replacement may be queued after this initial check.
          return provider.request(method, payload);
        }
        return provider.request(method, payload);
      }
      // Unregistered SSH identities throw in resolve; only local startup
      // without an initialized provider reaches the agent below.
    }

    // No repo_id, or a local provider still starting — send
    // to the local agent directly (preserves existing behavior for
    // repo-less calls like `ping`, `version`, etc.).
    return (await localAgent()).request(method, params);
  }
  media = new MediaResources(requestAgent, (owner, id, error) => {
    const contents = webContents.fromId(owner);
    if (contents && !contents.isDestroyed()) contents.send("crab:event", { method: "media/error", params: { media_id: id, error } });
  });
  protocol.handle("crab-content", request => media!.fetch(request));
  ipcMain.handle("crab:rpc", (event, method: string, params: unknown) => rpcReply(async () => {
    if (method === "media_open" || method === "media_close") {
      if (!mainWindow || event.sender.id !== mainWindow.webContents.id || !media) throw new Error("Media requests require the application window");
      if (method === "media_open") return media.open(event.sender.id, params);
      if (!params || typeof params !== "object" || !("media_id" in params) || typeof params.media_id !== "string") throw new Error("Media identity is required");
      await media.close(event.sender.id, params.media_id);
      return { closed: params.media_id };
    }
    return requestAgent(method, params);
  }));

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

  // ---- Repo capabilities ----

  // Returns the CapabilitySet for a given repo_id by resolving the
  // provider and calling its capabilities() method.
  ipcMain.handle("crab:repo-get-capabilities", async (_evt, repoId: string) => {
    const registry = getProviderRegistry();
    const provider = registry.resolve(repoId);
    if (provider) {
      return provider.capabilities(repoId);
    }
    return noCapabilities();
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

  ipcMain.handle("crab:fs-write-file", async (_evt, repoId: string, fullPath: string, content: string, nativeSessionId: string | null) => {
    const provider = getProviderRegistry().resolve(repoId);
    if (!provider) throw new Error("Repository file services are unavailable. Reopen the repository.");
    return provider.fsWrite(repoId, fullPath, content, nativeSessionId);
  });

  ipcMain.handle("crab:fs-read-file", async (_evt, ...args: [string] | [string, string, number | undefined, string | null]) => {
    // The task file viewer still supplies a worktree path without a repo handle.
    // Repository documents always resolve a provider and cannot fall through.
    if (args.length === 1) return readTextFile(args[0]);
    const provider = getProviderRegistry().resolve(args[0]);
    if (!provider) throw new Error("Repository file services are unavailable. Reopen the repository.");
    return provider.fsRead(args[0], args[1], args[2], args[3]);
  });

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

  ipcMain.handle("crab:jupyter-check", (_evt, pythonPath?: string) => checkJupyterRuntime(pythonPath));
  ipcMain.handle("crab:jupyter-list-kernels", async (_evt, pythonPath: string) => listKernelSpecs(pythonPath));
  ipcMain.handle("crab:jupyter-install", async (_evt, pythonPath: string) => installJupyter(pythonPath));

  ipcMain.handle("crab:jupyter-start", async (evt, id: string, repoId: string, pythonPath: string, target: KernelTarget) => {
    if (isShuttingDown) throw new Error("Crab Desktop is shutting down");
    if (target.kind === "local" && parseRepoId(repoId)) throw new Error("A local kernel requires a local working tree.");
    if (!id || notebookKernels.has(id)) throw new Error("Duplicate kernel connection");
    const owner = evt.sender;
    const kernel = new JupyterKernel(target.name, pythonPath, async signal => {
      const env = await getUserShellBaseEnv();
      signal.throwIfAborted();
      const local = target.kind === "local" ? await localAgent() : null;
      return () => {
        signal.throwIfAborted();
        if (isShuttingDown || owner.isDestroyed()) throw new Error("Notebook closed during kernel startup");
        if (target.kind === "remote") return { env, serverUrl: target.url };
        if (!local || !target.nativeSessionId || local.repositorySession(repoId) !== target.nativeSessionId) throw nativeSessionLost();
        const cwd = local.worktreePath(repoId);
        if (!cwd) throw new Error("The repository has no local working tree for this kernel.");
        return { cwd, env };
      };
    });
    const destroyed = () => { void closeNotebookKernel(id).catch(error => console.error("Notebook cleanup failed", error)); };
    owner.once("destroyed", destroyed);
    // The renderer preassigns the ID. The same owner can cancel environment
    // preparation and readiness before the connection invoke has returned.
    notebookKernels.set(id, { repoId, owner, kernel, release: () => owner.removeListener("destroyed", destroyed) });
    kernel.setOutputHandler(output => { if (!owner.isDestroyed()) owner.send("crab:jupyter-output", { id, ...output }); });
    kernel.setStatusHandler((status, error) => {
      if (status === "dead" && notebookKernels.get(id)?.kernel === kernel) {
        notebookKernels.delete(id);
        owner.removeListener("destroyed", destroyed);
      }
      if (!owner.isDestroyed()) owner.send("crab:jupyter-status", { id, status, error });
    });
    try {
      await kernel.start();
      if (isShuttingDown || owner.isDestroyed() || !notebookKernels.has(id)) throw new Error("Notebook closed during kernel startup");
    } catch (error) {
      await closeNotebookKernel(id);
      throw error;
    }
  });

  function ownedKernel(id: string, sender: WebContents): JupyterKernel {
    const entry = notebookKernels.get(id);
    if (!entry || entry.owner !== sender) throw new Error("Kernel not connected");
    return entry.kernel;
  }
  ipcMain.handle("crab:jupyter-execute", (evt, id: string, code: string, requestId: string, cell: number) => ownedKernel(id, evt.sender).execute(code, requestId, cell));
  ipcMain.handle("crab:jupyter-interrupt", (evt, id: string) => ownedKernel(id, evt.sender).interrupt());
  ipcMain.handle("crab:jupyter-restart", (evt, id: string) => ownedKernel(id, evt.sender).restart());
  ipcMain.handle("crab:jupyter-shutdown", async (evt, id: string) => { ownedKernel(id, evt.sender); await closeNotebookKernel(id); });

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

  // SSH subsystem initialization
  // v2.0.0-alpha.1 stores the default SSH data here. Custom app profiles must
  // stay isolated; moving default data requires an explicit migration.
  const sshDataDirectory = app.commandLine.hasSwitch("user-data-dir")
    ? app.getPath("userData") : path.join(os.homedir(), ".crab", "desktop");
  const sshProfileManager = new SshProfileManager(path.join(sshDataDirectory, "ssh-profiles.json"));
  const sshDatabase = new SshDatabase(path.join(sshDataDirectory, "ssh-enhanced.db"));
  const db = sshDatabase.getDb();
  desktopDatabase = sshDatabase;

  // Core SSH modules
  const knownHosts = new KnownHostsManager(path.join(app.getPath("userData"), "ssh-known-hosts.json"));
  const verifyHostKey = createHostKeyVerifier(knownHosts, (request, signal) => {
    if (!mainWindow || mainWindow.isDestroyed()) return Promise.resolve(false);
    return confirmSshHostKey(mainWindow, request, signal);
  });
  const sshSessionManager = new SshSessionManager(verifyHostKey);

  const storageConnections = new StorageConnections(
    path.join(app.getPath("userData"), "storage-connections.enc"), safeStorage, process.platform,
    async (keys, state) => {
      // The vault persists identity before either native owner replaces readers.
      // Join both outcomes so one transport's failure cannot skip the other.
      const outcomes = await Promise.allSettled([
        agent?.storageIdentityChanged(keys, state),
        sshSessionManager.storageIdentityChanged(keys, state),
      ]);
      const failures = outcomes.flatMap(outcome => outcome.status === "rejected" ? [outcome.reason] : []);
      if (failures.length === 1) throw failures[0];
      if (failures.length > 1) throw new AggregateError(failures, "Storage identity changed but some repositories could not recover");
    },
    repoIds => sendAgentEvent("repo:storage-source-changed", { repo_ids: repoIds }),
  );

  // Share construction, not process readiness: Agent retains its own restart
  // and repository owners. A failed prerequisite can be repaired and retried.
  let construction: Promise<Agent> | undefined;
  function localAgent(): Promise<Agent> {
    if (isShuttingDown) return Promise.reject(new Error("Crab Desktop is shutting down"));
    if (agent) return Promise.resolve(agent);
    if (!construction) construction = constructLocalAgent().finally(() => { construction = undefined; });
    return construction;
  }

  const { getDefaultShell, buildTerminalEnv, shellEnvPromise, writeSessionLog } = registerIpc(skillManager, storageConnections, localAgent, db);
  registerSidecarIpc(sidecarManager);
  registerLspIpc(lspManager, () => mainWindow);
  registerFileWatcher(() => mainWindow);

  _sshSessionManager = sshSessionManager;
  const sshTerminalManager = new RemoteTerminalManager(sshSessionManager, id => sshProfileManager.get(id));
  const sshPortForwardManager = new PortForwardManager(sshSessionManager);

  const rpcRouter = new RpcRouter(sshSessionManager, {
    request: async (method: string, params: unknown) => {
      return (await localAgent()).request(method, params);
    },
  }, storageConnections, {}, admitFileTransfer);
  _rpcRouter = rpcRouter;
  orchestrator?.setRpcRouter(rpcRouter);
  const runtimeOptions = { packaged: app.isPackaged, appPath: app.getAppPath(), version: app.getVersion(), cacheDirectory: path.join(app.getPath("userData"), "remote-runtimes") };
  const remoteRuntimes = new RemoteRuntimeStore(runtimeOptions);
  const capabilityNegotiator = new CapabilityNegotiator(rpcRouter);
  sshSessionManager.setAgentDependencies({ createDeployer: () => new AgentDeployer(remoteRuntimes.resolve.bind(remoteRuntimes)), capabilityNegotiator, rpcRouter });
  const fileEditor = new RemoteFileEditor(rpcRouter, sshSessionManager);
  const gitManager = new RemoteGitManager(rpcRouter, sshSessionManager);
  const remoteProvider = new RemoteProvider({
    router: rpcRouter,
    fileEditor,
    gitManager,
    capabilityNegotiator,
    sessionManager: sshSessionManager,
  });
  const registry = getProviderRegistry();

  const fileBrowser = new RemoteFileBrowser(sshSessionManager, registry, remoteProvider);
  registerSshIpcHandlers(sshProfileManager, sshSessionManager, fileBrowser, knownHosts, sshTerminalManager, sshPortForwardManager);

  bridgeAgentDeployToRenderer(sshSessionManager);
  // Remote progress is scoped by the router's live request owner. Forward only
  // that stream; remote OAuth or other unowned notifications stay remote.
  rpcRouter.on("progress", ({ method, params }) => sendAgentEvent(method, params));
  remoteProvider.on("fs/changed", event => sendAgentEvent("fs/changed", { repo_path: event.repoPath }));
  remoteProvider.on("capabilities-changed", sendRepositoryCapabilities);


  // Optional SSH IDE services cannot prevent native bootstrap or repository browsing.
  try {
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
    const workspaceManager = new RemoteWorkspaceManager(rpcRouter, sshSessionManager, fileEditor);
    const debugManager = new RemoteDebugManager(rpcRouter, sshSessionManager);

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
      workspaceManager,
      gitManager,
      debugManager,
      sessionManager: sshSessionManager,
    });

    // Bridge sync and deploy events to the renderer
    bridgeSyncEventsToRenderer(workspaceSyncEngine, fileTransferManager);

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
    if (mainWindow) return;
    termRouter.dispose();
    daemonSubsystem?.dispose();
  });

  async function constructLocalAgent(): Promise<Agent> {
    try {
      const shellEnv = await shellEnvPromise;
      if (shellEnv) {
        Object.assign(process.env, shellEnv);
      }
    } catch (err) {
      console.warn("[crab-desktop] Failed to resolve shell environment before starting agent:", err);
    }

    // A delayed shell lookup cannot create a process after application shutdown.
    if (isShuttingDown) throw new Error("Crab Desktop is shutting down");
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
    // The native process owns and retires its private metadata index.
    const next = new Agent(runtime, agentEnvFor(mainSettings.agentLogLevel), [], storageConnections, admitFileTransfer);
    if (logsDir) next.on("stderr", (line: string) => {
      const day = new Date().toISOString().slice(0, 10);
      fs.appendFile(path.join(logsDir, `agent.log.${day}`), `${line}\n`, { mode: 0o600 }, error => {
        if (error) console.warn("[crab-desktop] Could not append agent log.");
      });
    });
    next.on("notification", sendAgentEvent);
    next.on("exit", (code, signal) => {
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
    registerForgeAuth(next, () => mainWindow);

    // Initialize the ProviderRegistry with the LocalProvider singleton.
    // This must happen after the agent is created so the LocalProvider
    // can delegate RPC calls to it.
    const registry = getProviderRegistry();
    const localProvider = new LocalProvider(next);
    registry.setLocalProvider(localProvider);
    localProvider.on("capabilities-changed", sendRepositoryCapabilities);
    agent = next;

    // Fire-and-forget; we don't want to block the window on the
    // agent's ping round-trip. Failures bubble into the catch below.
    next.start().catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      mainWindow?.webContents.send("crab:event", {
        method: "agent/failed",
        params: { message },
      });
    });
    return next;
  }

  void localAgent().catch((err) => {
    if (isShuttingDown) return;
    const message = err instanceof Error ? err.message : String(err);
    mainWindow?.webContents.send("crab:event", {
      method: "agent/failed",
      params: { message },
    });
  });


});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) void createWindow();
});

registerAppShutdown(app, [
  () => { isShuttingDown = true; },
  stopAutoUpdater,
  stopFileWrites,
  stopNotebookKernels,
  stopJupyterRuntimeCommands,
  stopAllWatchers,
  () => shutdownAgentServices?.(),
  () => fileTransfers?.stop(),
  async () => { try { await media?.closeAll(); } finally { await agent?.shutdown(); } },
  () => _sshSessionManager && Promise.all(_sshSessionManager.getAllConnections().map(connection => _sshSessionManager!.disconnect(connection.profile.id))),
  () => sidecarManager.stopAll(),
  () => lspManager.stopAll(),
], () => desktopDatabase?.close());
