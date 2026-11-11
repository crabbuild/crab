/**
 * Daemon subsystem initialization.
 *
 * Constructs DaemonSpawner, DaemonClient, CircuitBreaker, AutoRespawn,
 * and DaemonBridge in one place so main.ts stays thin.
 *
 * Returns the assembled subsystem and a dispose function that checkpoints
 * active sessions and closes sockets without killing the daemon process.
 */

import * as path from "node:path";
import * as os from "node:os";

import { DaemonSpawner } from "./daemon-spawner";
import { DaemonClient } from "./daemon-client";
import { CircuitBreaker } from "./circuit-breaker";
import { AutoRespawn } from "./auto-respawn";
import { createDaemonBridge } from "./daemon-bridge";
import { initializePtyMode, parsePtyMode } from "./mode-selection";
import { detectAndHandlePathMismatch, type PathMismatchResult } from "./path-mismatch-detection";
import { resolveDaemonEntryPath } from "./resolve-daemon-path";
import { registerAgentDashboardIpc } from "./agent-dashboard-ipc";

import type { AgentActivityState } from "./types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface IpcMainLike {
  handle(channel: string, listener: (...args: unknown[]) => unknown): void;
  removeHandler(channel: string): void;
}

export interface AppLike {
  on(event: string, listener: (...args: unknown[]) => void): void;
}

export interface BrowserWindowLike {
  isDestroyed(): boolean;
  webContents: {
    send(channel: string, ...args: unknown[]): void;
  };
}

export interface InitDaemonOptions {
  ipcMain: IpcMainLike;
  app: AppLike;
  getMainWindow: () => BrowserWindowLike | null;
  getWorkspacePath: () => string | null;
  log: (level: "info" | "warn" | "error", message: string) => void;
  /** Override the daemon entry path (used in tests / dev). */
  daemonEntryPath?: string;
  /** Override the socket directory (used in tests). */
  socketDir?: string;
  /**
   * Whether the Electron app is packaged (app.isPackaged).
   * Used by resolveDaemonEntryPath when daemonEntryPath is not provided.
   */
  isPackaged?: boolean;
  /**
   * The Electron process.resourcesPath value.
   * Used by resolveDaemonEntryPath in packaged builds.
   */
  resourcesPath?: string;
}

export interface DaemonSubsystem {
  /** Whether the daemon connected successfully at startup. */
  daemonReady: boolean;
  daemonClient: DaemonClient;
  daemonSpawner: DaemonSpawner;
  autoRespawn: AutoRespawn;
  circuitBreaker: CircuitBreaker;
  daemonBridge: ReturnType<typeof createDaemonBridge>;
  /** Whether a path mismatch was detected at startup (stale daemon from a different app location). */
  pathMismatch: boolean;
  /** The entry path recorded in the stale daemon's PID file (null if no mismatch). */
  previousEntryPath: string | null;
  /**
   * Whether the daemon entry path was resolved from the CRAB_DAEMON_ENTRY
   * environment variable override. Surfaced in diagnostics for support debugging.
   */
  daemonEntryFromEnv: boolean;
  /** The resolved daemon entry path. */
  resolvedDaemonEntryPath: string;
  dispose: () => void;
}

// ---------------------------------------------------------------------------
// initializeDaemonSubsystem
// ---------------------------------------------------------------------------

export async function initializeDaemonSubsystem(
  opts: InitDaemonOptions,
): Promise<DaemonSubsystem> {
  const { ipcMain, app, getMainWindow, getWorkspacePath, log } = opts;

  const socketDir =
    opts.socketDir ?? path.join(os.homedir(), ".crab", "desktop", "daemon");

  // Resolve the daemon entry path. If an explicit override is provided (tests
  // or dev tooling), use it directly. Otherwise use resolveDaemonEntryPath
  // which honours the CRAB_DAEMON_ENTRY env var and falls back to the
  // canonical packaged / dev locations.
  let daemonEntryPath: string | null = null;
  let daemonEntryFromEnv = false;

  if (opts.daemonEntryPath) {
    daemonEntryPath = opts.daemonEntryPath;
    daemonEntryFromEnv = false;
  } else {
    const appIsPackaged =
      opts.isPackaged ??
      (("isPackaged" in (app as unknown as { isPackaged?: boolean }))
        ? (app as unknown as { isPackaged: boolean }).isPackaged
        : false);
    const resourcesPath =
      opts.resourcesPath ??
      (app as unknown as { resourcesPath?: string }).resourcesPath ??
      process.resourcesPath;

    // resolveDaemonEntryPath throws if no candidate exists on disk.
    // In development mode this is expected — the daemon binary may not be
    // built yet. Only explicitly ephemeral terminals can run without it.
    try {
      const resolved = resolveDaemonEntryPath(
        appIsPackaged,
        resourcesPath,
        path.join(__dirname),
      );
      daemonEntryPath = resolved.entryPath;
      daemonEntryFromEnv = resolved.fromEnv;

      if (resolved.fromEnv) {
        log("info", `Daemon entry path overridden via CRAB_DAEMON_ENTRY: ${daemonEntryPath}`);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log("warn", `PTY daemon binary not found — durable terminals unavailable: ${msg}`);
      // Return a minimal subsystem with daemonReady=false. The TermRouter
      // refuses durable spawns with a typed error; explicit ephemeral
      // terminals remain available.

      // Register fallback IPC handlers so the renderer doesn't get
      // "No handler registered" errors.
      ipcMain.handle("crab:pty-mode", () => ({
        resolvedMode: "in-process",
        configuredMode: parsePtyMode(process.env.CRAB_PTY_MODE),
        fallback: { reason: "Daemon binary not found", error: msg, timestamp: Date.now() },
      }));
      ipcMain.handle("crab:agent-sessions", () => []);

      const noopSpawner = new DaemonSpawner({ socketDir, entryPath: "" });
      const noopClient = new DaemonClient({
        socketDir,
        daemonEntryPath: "",
        onSessionData: () => {},
        onSessionExit: () => {},
        onAgentStateChange: () => {},
        onColdRestoreAvailable: () => {},
        onDisconnected: () => {},
      });
      const noopCircuitBreaker = new CircuitBreaker({
        threshold: 3,
        windowMs: 60_000,
        cooldownMs: 5 * 60_000,
      });
      const noopBridge = createDaemonBridge({
        client: noopClient,
        getMainWindow,
        app,
        getWorkspacePath,
      });
      const noopAutoRespawn = new AutoRespawn(
        {
          spawner: noopSpawner,
          getActiveSessionIds: () => [],
          synthesizeExitEvent: () => {},
          reconnect: async () => {},
          log,
        },
        { maxAttempts: 0, retryDelayMs: 1_000, enabled: false },
      );

      return {
        daemonReady: false,
        daemonClient: noopClient,
        daemonSpawner: noopSpawner,
        autoRespawn: noopAutoRespawn,
        circuitBreaker: noopCircuitBreaker,
        daemonBridge: noopBridge,
        pathMismatch: false,
        previousEntryPath: null,
        daemonEntryFromEnv: false,
        resolvedDaemonEntryPath: "",
        dispose: () => {
          ipcMain.removeHandler("crab:pty-mode");
          ipcMain.removeHandler("crab:agent-sessions");
        },
      };
    }
  }

  // After the early return above, daemonEntryPath is guaranteed non-null.
  const resolvedEntryPath: string = daemonEntryPath!;

  // -------------------------------------------------------------------------
  // Spawner + Client
  // -------------------------------------------------------------------------

  const daemonSpawner = new DaemonSpawner({
    socketDir,
    entryPath: resolvedEntryPath,
  });

  // Mutable bridge callbacks — resolved after the bridge is created.
  let bridgeHandleData: ((sessionId: string, data: string) => void) | null = null;
  let bridgeHandleExit: ((sessionId: string, code: number) => void) | null = null;
  let bridgeHandleColdRestore: ((sessionId: string, payload: import("./protocol.js").ColdRestorePayload) => void) | null = null;

  // Mutable agent state-change handler — replaced after registerAgentDashboardIpc
  // is called so the dashboard IPC owns the forwarding logic.
  let agentStateChangeHandler: ((sessionId: string, state: AgentActivityState, previousState: AgentActivityState) => void) | null = null;

  // Mutable runtime daemon-ready flag — updated when the daemon connects or
  // disconnects so push events carry the current value.
  let runtimeDaemonReady = false;

  /**
   * Push a `crab:pty-mode-changed` event to the renderer so components
   * subscribed via `window.crab.onPtyMode` can react immediately to
   * daemon availability changes (connect, disconnect, circuit-breaker open).
   */
  function sendPtyModeChanged(): void {
    const win = getMainWindow();
    if (!win || win.isDestroyed()) return;
    win.webContents.send("crab:pty-mode-changed", {
      daemonReady: runtimeDaemonReady,
      breakerOpen: circuitBreaker.state === "open",
    });
  }

  const daemonClient = new DaemonClient({
    socketDir,
    daemonEntryPath: resolvedEntryPath,
    onSessionData: (sessionId, data) => {
      bridgeHandleData?.(sessionId, data);
    },
    onSessionExit: (sessionId, code) => {
      bridgeHandleExit?.(sessionId, code);
    },
    onAgentStateChange: (
      sessionId: string,
      state: AgentActivityState,
      previousState: AgentActivityState,
    ) => {
      agentStateChangeHandler?.(sessionId, state, previousState);
    },
    onColdRestoreAvailable: (sessionId, payload) => {
      bridgeHandleColdRestore?.(sessionId, payload);
    },
    onDisconnected: () => {
      log("warn", "Daemon connection lost");
      runtimeDaemonReady = false;
      circuitBreaker.recordFailure();
      sendPtyModeChanged();
      if (circuitBreaker.shouldAttemptDaemon) {
        void autoRespawn.handleDisconnect();
      }
    },
  });

  // -------------------------------------------------------------------------
  // Circuit breaker
  // -------------------------------------------------------------------------

  const circuitBreaker = new CircuitBreaker({
    threshold: 3,
    windowMs: 60_000,
    cooldownMs: 5 * 60_000,
    onStateChange: (from, to, reason) => {
      log("info", `Circuit breaker ${from} → ${to}: ${reason}`);
      // Push the updated breaker state to the renderer so affordances
      // (e.g. "New daemon terminal" button) enable/disable immediately.
      sendPtyModeChanged();
    },
  });

  // -------------------------------------------------------------------------
  // Daemon bridge
  // -------------------------------------------------------------------------

  const daemonBridge = createDaemonBridge({
    client: daemonClient,
    getMainWindow,
    app,
    getWorkspacePath,
  });

  // Wire bridge callbacks into the client
  bridgeHandleData = daemonBridge.handleSessionData;
  bridgeHandleExit = daemonBridge.handleSessionExit;
  bridgeHandleColdRestore = daemonBridge.handleColdRestoreAvailable;

  // -------------------------------------------------------------------------
  // Auto-respawn
  // -------------------------------------------------------------------------

  const autoRespawn = new AutoRespawn(
    {
      spawner: daemonSpawner,
      getActiveSessionIds: () => daemonBridge.getDaemonSessionIds(),
      synthesizeExitEvent: (sessionId: string, code: number) => {
        // Delegate to the bridge so the renderer receives crab:term-exit with
        // reason: "daemon-disconnected" and the session mappings are cleaned up.
        daemonBridge.synthesizeExitEvent(sessionId, code);
      },
      reconnect: async () => {
        await daemonClient.ensureConnected();
        // Record success so the circuit breaker can transition from
        // half-open → closed after a successful reconnect.
        circuitBreaker.recordSuccess();
        runtimeDaemonReady = true;
        sendPtyModeChanged();
      },
      log,
    },
    { maxAttempts: 3, retryDelayMs: 1_000, enabled: true },
  );

  // Wrap the daemon-bridge mutating handlers with retry-on-disconnect so a
  // single transient disconnect does not surface as a user-visible error.
  // Non-mutating handlers (handleSessionData, handleSessionExit, warmReattach,
  // getDaemonSessionIds, synthesizeExitEvent, dispose) are passed through as-is.
  const wrappedDaemonBridge = {
    ...daemonBridge,
    handleSpawn: (payload: Parameters<typeof daemonBridge.handleSpawn>[0], externalRendererId?: string) =>
      autoRespawn.withRetryOnDisconnect(async () => {
        await daemonSpawner.ensureRunning();
        await daemonClient.ensureConnected();
        return daemonBridge.handleSpawn(payload, externalRendererId);
      }),
    handleWrite: (id: string, data: string) =>
      autoRespawn.withRetryOnDisconnect(() => daemonBridge.handleWrite(id, data)),
    handleResize: (id: string, cols: number, rows: number) =>
      autoRespawn.withRetryOnDisconnect(() => daemonBridge.handleResize(id, cols, rows)),
    handleKill: (id: string) =>
      autoRespawn.withRetryOnDisconnect(() => daemonBridge.handleKill(id)),
  };

  // -------------------------------------------------------------------------
  // Initialize PTY mode (spawns daemon if needed, registers crab:pty-mode)
  // -------------------------------------------------------------------------

  // Detect and handle a path mismatch before spawning. If the PID file
  // records a different entry path, preserve live terminals. ensureRunning()
  // can retire a missing runtime only after the daemon atomically accepts idle shutdown.
  let pathMismatchResult: PathMismatchResult;
  try {
    pathMismatchResult = detectAndHandlePathMismatch({
      pidPath: daemonSpawner.pidPath,
      socketPath: daemonSpawner.socketPath,
      tokenPath: daemonSpawner.tokenPath,
      currentEntryPath: resolvedEntryPath,
      log,
    });

    if (pathMismatchResult.mismatch && pathMismatchResult.killed) {
      log(
        "info",
        `Replaced stale daemon from a different app location (previousEntryPath: ${pathMismatchResult.pidFileEntryPath ?? "unknown"})`,
      );
    }
  } catch (err) {
    // Path mismatch detection is best-effort — log and continue.
    const msg = err instanceof Error ? err.message : String(err);
    log("warn", `Path mismatch detection failed (continuing): ${msg}`);
    pathMismatchResult = {
      mismatch: false,
      pidFileEntryPath: null,
      currentEntryPath: resolvedEntryPath,
      killed: false,
      cleanedUp: false,
    };
  }

  let daemonReady = false;

  try {
    const modeResult = await initializePtyMode(
      {
        client: daemonClient,
        spawner: daemonSpawner,
        ipcMain,
        log,
      },
      process.env.CRAB_PTY_MODE,
    );

    daemonReady = modeResult.resolvedMode === "daemon";
    runtimeDaemonReady = daemonReady;

    if (daemonReady) {
      log("info", "Daemon subsystem ready — terminals will be durable");
    } else {
      log("info", "Daemon unavailable — open an explicit local terminal or retry durable sessions");
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    log("error", `Daemon initialization failed: ${msg}`);
    daemonReady = false;
    // Ensure crab:pty-mode is registered even on failure so the renderer
    // doesn't get "No handler registered" errors.
    try {
      ipcMain.handle("crab:pty-mode", () => ({
        resolvedMode: "in-process",
        configuredMode: parsePtyMode(process.env.CRAB_PTY_MODE),
        fallback: { reason: "Daemon initialization failed", error: msg, timestamp: Date.now() },
      }));
    } catch {
      // Handler may already be registered if initializePtyMode partially succeeded.
    }
  }

  // -------------------------------------------------------------------------
  // Agent dashboard IPC (registered regardless of daemon availability so
  // the renderer never gets "No handler registered" errors)
  // -------------------------------------------------------------------------

  let agentDashboardDispose: (() => void) | null = null;

  if (daemonReady) {
    const agentDashboard = registerAgentDashboardIpc({
      ipcMain,
      client: daemonClient,
      getMainWindow,
    });
    // Wire the returned handler so crab:agent-state-changed reaches the renderer.
    agentStateChangeHandler = agentDashboard.handleAgentStateChange;
    agentDashboardDispose = agentDashboard.dispose;
  } else {
    // Register a no-op handler so the renderer doesn't get IPC errors.
    ipcMain.handle("crab:agent-sessions", () => []);
    agentDashboardDispose = () => {
      ipcMain.removeHandler("crab:agent-sessions");
    };
  }

  // -------------------------------------------------------------------------
  // crab:daemon-prepare-upgrade IPC
  //
  // Triggers a zero-downtime daemon binary swap. The successor path defaults
  // to the currently resolved daemon entry path so the caller does not need
  // to supply it for the common "apply pending update" case.
  // -------------------------------------------------------------------------

  ipcMain.handle(
    "crab:daemon-prepare-upgrade",
    (async (
      _evt: unknown,
      payload?: { successorPath?: string },
    ): Promise<{ success: boolean; error?: string }> => {
      const successorPath = payload?.successorPath ?? resolvedEntryPath;
      try {
        // v2.0.0-alpha.1 daemons lack the live-replacement guard. Apply it
        // at the app boundary too when upgrading from that tagged version.
        const sessions = await daemonClient.request<import("./types").SessionInfo[]>("listSessions");
        if (sessions.some(session => session.isAlive)) {
          return { success: false, error: "Daemon replacement is deferred while terminals are running. Application updates preserve these sessions; end the terminals before replacing their daemon." };
        }
        const result = await daemonClient.request<{ success: boolean; error?: string }>(
          "prepare-upgrade",
          { successorPath },
        );
        return result;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        log("error", `prepare-upgrade request failed: ${msg}`);
        return { success: false, error: msg };
      }
    }) as (...args: unknown[]) => unknown,
  );

  // -------------------------------------------------------------------------
  // Cold restore IPC handlers
  //
  // crab:term-cold-restore-confirm — user confirmed the restore dialog.
  //   Forwards to the daemon as a cold-restore-resume request so the daemon
  //   spawns a fresh subprocess in the saved cwd with the user-provided env.
  //
  // crab:term-cold-restore-discard — user dismissed the restore dialog.
  //   Forwards to the daemon as a cold-restore-discard request so the daemon
  //   removes the history dir for the session.
  // -------------------------------------------------------------------------

  ipcMain.handle(
    "crab:term-cold-restore-confirm",
    (async (
      _evt: unknown,
      payload: { sessionId: string; env: Record<string, string> },
    ): Promise<void> => {
      try {
        await daemonClient.request("cold-restore-resume", {
          sessionId: payload.sessionId,
          env: payload.env,
        });
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        log("error", `cold-restore-resume request failed: ${msg}`);
      }
    }) as (...args: unknown[]) => unknown,
  );

  ipcMain.handle(
    "crab:term-cold-restore-discard",
    (async (
      _evt: unknown,
      payload: { sessionId: string },
    ): Promise<void> => {
      try {
        await daemonClient.request("cold-restore-discard", {
          sessionId: payload.sessionId,
        });
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        log("error", `cold-restore-discard request failed: ${msg}`);
      }
    }) as (...args: unknown[]) => unknown,
  );

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  function dispose(): void {
    ipcMain.removeHandler("crab:daemon-prepare-upgrade");
    ipcMain.removeHandler("crab:term-cold-restore-confirm");
    ipcMain.removeHandler("crab:term-cold-restore-discard");
    agentDashboardDispose?.();
    daemonBridge.dispose();
    daemonClient.disconnect();
  }

  return {
    daemonReady,
    daemonClient,
    daemonSpawner,
    autoRespawn,
    circuitBreaker,
    daemonBridge: wrappedDaemonBridge,
    pathMismatch: pathMismatchResult.mismatch,
    previousEntryPath: pathMismatchResult.pidFileEntryPath,
    daemonEntryFromEnv,
    resolvedDaemonEntryPath: resolvedEntryPath,
    dispose,
  };
}
