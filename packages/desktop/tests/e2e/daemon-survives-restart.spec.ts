/**
 * End-to-end smoke test: daemon-typed terminal survives renderer restart.
 *
 * Steps:
 *   1. Launch the packaged Electron app.
 *   2. Spawn a daemon-typed terminal session via the IPC harness.
 *   3. Write a unique marker line into the session.
 *   4. Kill the renderer process (not the daemon).
 *   5. Relaunch the app.
 *   6. Assert the marker line appears in the reattached session (warm reattach).
 *
 * Platform: macOS and Linux only. Windows is tracked separately for v1.
 *
 * Prerequisites:
 *   - Run `npm run package` (or `npm run package:mac` / `npm run package:linux`)
 *     before executing this suite. The test reads the packaged binary path from
 *     the CRAB_PACKAGED_APP env var, or falls back to the platform default under
 *     `release/`.
 *   - The daemon entry must be present at
 *     `resources/app.asar/dist-electron/daemon/daemon-main.cjs` in the package.
 */

import { test, expect } from "@playwright/test";
import * as child_process from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

// ---------------------------------------------------------------------------
// Platform guard — skip on Windows for v1
// ---------------------------------------------------------------------------

test.skip(
  process.platform === "win32",
  "Windows e2e smoke test is tracked separately for v1",
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Resolve the packaged app binary path for the current platform. */
function resolvePackagedAppPath(): string {
  if (process.env.CRAB_PACKAGED_APP) {
    return process.env.CRAB_PACKAGED_APP;
  }

  const releaseDir = path.resolve(__dirname, "../../release");

  if (process.platform === "darwin") {
    // electron-builder produces the .app bundle under mac-arm64/ or mac/
    const candidates = [
      path.join(releaseDir, "mac-arm64", "Crab.app", "Contents", "MacOS", "Crab"),
      path.join(releaseDir, "mac", "Crab.app", "Contents", "MacOS", "Crab"),
    ];
    for (const c of candidates) {
      if (fs.existsSync(c)) return c;
    }
    throw new Error(
      `Packaged app not found under ${releaseDir}. Run 'npm run package:mac' first, ` +
        "or set CRAB_PACKAGED_APP to the binary path.",
    );
  }

  if (process.platform === "linux") {
    // electron-builder produces an AppImage
    const entries = fs.existsSync(releaseDir) ? fs.readdirSync(releaseDir) : [];
    const appImage = entries.find((e) => e.endsWith(".AppImage"));
    if (appImage) return path.join(releaseDir, appImage);
    throw new Error(
      `AppImage not found under ${releaseDir}. Run 'npm run package:linux' first, ` +
        "or set CRAB_PACKAGED_APP to the binary path.",
    );
  }

  throw new Error(`Unsupported platform: ${process.platform}`);
}

/** Resolve the daemon socket directory used by the packaged app. */
function daemonSocketDir(): string {
  return path.join(os.homedir(), ".crab", "desktop", "daemon");
}

/** Resolve the daemon socket path. */
function daemonSocketPath(): string {
  return path.join(daemonSocketDir(), "daemon-v2.sock");
}

/** Wait until a Unix socket is accepting connections, up to `timeoutMs`. */
async function waitForSocket(socketPath: string, timeoutMs = 10_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const alive = await new Promise<boolean>((resolve) => {
      const sock = net.createConnection(socketPath);
      sock.once("connect", () => { sock.destroy(); resolve(true); });
      sock.once("error", () => resolve(false));
    });
    if (alive) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`Daemon socket not ready after ${timeoutMs} ms: ${socketPath}`);
}

interface LaunchResult {
  pid: number;
  kill: () => void;
}

/**
 * Launch the packaged Electron app in headless mode.
 *
 * We pass `--no-sandbox` for Linux CI and set `DISPLAY=:99` when Xvfb is
 * available. The app is launched with a dedicated `CRAB_WORKSPACE` so it
 * does not interfere with the developer's real workspace.
 */
function launchApp(appPath: string, workspaceDir: string): LaunchResult {
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    CRAB_WORKSPACE: workspaceDir,
    // Force daemon mode so the smoke test exercises the daemon path.
    CRAB_PTY_MODE: "daemon",
    // Suppress update checks in CI.
    ELECTRON_DISABLE_SECURITY_WARNINGS: "true",
  };

  if (process.platform === "linux") {
    env.DISPLAY = process.env.DISPLAY ?? ":99";
  }

  const args = process.platform === "linux" ? ["--no-sandbox"] : [];

  const proc = child_process.spawn(appPath, args, {
    env,
    detached: false,
    stdio: "ignore",
  });

  return {
    pid: proc.pid!,
    kill: () => {
      try {
        proc.kill("SIGTERM");
      } catch {
        // already dead
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

test.describe("Daemon survives renderer restart (warm reattach)", () => {
  let workspaceDir: string;
  let appPath: string;

  test.beforeAll(() => {
    appPath = resolvePackagedAppPath();
    workspaceDir = fs.mkdtempSync(path.join(os.tmpdir(), "crab-e2e-"));
  });

  test.afterAll(() => {
    try {
      fs.rmSync(workspaceDir, { recursive: true, force: true });
    } catch {
      // best-effort cleanup
    }
  });

  test(
    "spawns a daemon terminal, kills the renderer, relaunches, and reattaches",
    async () => {
      // Allow up to 3 minutes for the full package + launch + reattach cycle.
      test.setTimeout(180_000);
      // -----------------------------------------------------------------------
      // Phase 1: First launch — spawn a daemon session and write a marker
      // -----------------------------------------------------------------------

      const marker = `crab-e2e-marker-${Date.now()}`;
      const socketPath = daemonSocketPath();
      const socketDir = daemonSocketDir();

      // Launch the app for the first time.
      const firstLaunch = launchApp(appPath, workspaceDir);

      // The session ID we'll use across both phases.
      const sessionId = `e2e-test-session-${Date.now()}`;

      try {
        // Wait for the daemon socket to appear — confirms the daemon started.
        await waitForSocket(socketPath, 30_000);

        // Connect to the daemon directly to spawn a session and write the marker.
        // We use the raw protocol so the test does not depend on the renderer UI.
        const { DaemonClient } = await import("../../electron/daemon/daemon-client");

        // Collect session data so we can detect the marker.
        const receivedData: string[] = [];

        const client = new DaemonClient({
          socketDir,
          daemonEntryPath: appPath,
          onSessionData: (id: string, data: string) => {
            if (id === sessionId) {
              receivedData.push(data);
            }
          },
          onSessionExit: () => {},
          onAgentStateChange: () => {},
        });

        await client.ensureConnected();

        // Spawn a daemon session.
        const spawnResult = await client.request<{ isNew: boolean }>(
          "createOrAttach",
          {
            sessionId,
            cwd: workspaceDir,
            cols: 80,
            rows: 24,
            shellReadySupported: false,
          },
        );

        expect(spawnResult.isNew).toBe(true);

        // Give the shell a moment to start before sending input.
        await new Promise((r) => setTimeout(r, 1_000));

        // Write the marker line.
        await client.notify("write", { sessionId, data: `echo ${marker}\r` });

        // Wait for the marker to appear in the session output.
        const markerDeadline = Date.now() + 10_000;
        let markerSeen = false;
        while (Date.now() < markerDeadline) {
          if (receivedData.some((chunk) => chunk.includes(marker))) {
            markerSeen = true;
            break;
          }
          await new Promise((r) => setTimeout(r, 100));
        }
        expect(markerSeen, `Marker '${marker}' not seen in session output`).toBe(true);

        // Checkpoint the session so the snapshot is persisted before we kill
        // the renderer. The daemon keeps running.
        await client.notify("checkpoint", { sessionId });
        await new Promise((r) => setTimeout(r, 500));

        client.disconnect();
      } finally {
        // Kill the renderer (Electron main process). The daemon stays alive
        // because it is a detached child.
        firstLaunch.kill();
      }

      // Give the renderer a moment to exit.
      await new Promise((r) => setTimeout(r, 1_000));

      // -----------------------------------------------------------------------
      // Phase 2: Second launch — reattach and verify the marker is present
      // -----------------------------------------------------------------------

      const secondLaunch = launchApp(appPath, workspaceDir);

      try {
        // The daemon socket should still be alive (daemon survived).
        await waitForSocket(socketPath, 10_000);

        const { DaemonClient } = await import("../../electron/daemon/daemon-client");

        const client2 = new DaemonClient({
          socketDir,
          daemonEntryPath: appPath,
          onSessionData: () => {},
          onSessionExit: () => {},
          onAgentStateChange: () => {},
        });

        await client2.ensureConnected();

        // Verify the session is still listed as alive.
        const sessions = await client2.request<Array<{ sessionId: string }>>(
          "listSessions",
        );
        const liveSession = sessions.find((s) => s.sessionId === sessionId);
        expect(liveSession, `Session '${sessionId}' not found in listSessions`).toBeDefined();

        // Reattach to the same session — isNew should be false.
        const reattachResult = await client2.request<{
          isNew: boolean;
          snapshot: { snapshotAnsi: string } | null;
        }>("createOrAttach", {
          sessionId,
          cwd: workspaceDir,
          cols: 80,
          rows: 24,
          shellReadySupported: false,
        });

        // Warm reattach: session was already running, so isNew is false.
        expect(reattachResult.isNew).toBe(false);

        // The snapshot returned on reattach should contain the marker line.
        expect(reattachResult.snapshot).not.toBeNull();
        expect(reattachResult.snapshot!.snapshotAnsi).toContain(marker);

        client2.disconnect();
      } finally {
        secondLaunch.kill();
      }
    },
  );
});
