/**
 * Daemon path resolution — determines the daemon entry point path
 * based on whether we're in development or production (packaged) mode.
 *
 * In development, the daemon script lives under `dist-electron/daemon/`.
 * In packaged builds, it is loaded through `app.asar/dist-electron/daemon/`.
 *
 * An env var override (`CRAB_DAEMON_ENTRY`) is supported for testing
 * and custom deployments.
 */

import * as fs from "node:fs";
import * as path from "node:path";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ResolveDaemonPathOptions {
  /**
   * Whether the app is running in development mode.
   * Typically `!app.isPackaged` in Electron.
   */
  isDev: boolean;

  /**
   * The Electron `process.resourcesPath` value.
   * In packaged builds, this points to the `resources/` directory
   * inside the app bundle.
   */
  resourcesPath: string;

  /**
   * The directory containing the compiled electron main process files.
   * In dev, this is typically `dist-electron/` relative to the project root.
   * Defaults to `__dirname` of the calling module (the daemon/ subdirectory).
   */
  electronDistDir: string;

  /**
   * Optional env var override. If set, this path is used directly
   * without any candidate resolution.
   */
  envOverride?: string;
}

export interface ResolvedDaemonPath {
  /** The resolved absolute path to the daemon entry script. */
  entryPath: string;
  /** Whether the path was resolved from an env var override. */
  fromEnv: boolean;
  /** Whether the path was resolved from a dev candidate. */
  isDev: boolean;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** The daemon entry script filename (compiled JS). */
const DAEMON_ENTRY_FILENAME = "daemon-main.cjs";

/** Subdirectory under resources/ in packaged builds. */
const RESOURCES_DAEMON_DIR = "app.asar/dist-electron/daemon";

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/**
 * Resolve the daemon entry point path.
 *
 * Resolution order:
 * 1. `CRAB_DAEMON_ENTRY` env var (if set and file exists)
 * 2. Production: `<resourcesPath>/app.asar/dist-electron/daemon/daemon-main.cjs`
 * 3. Development: `<electronDistDir>/daemon-main.cjs`
 *    (the daemon/ subdirectory of dist-electron)
 *
 * @throws Error if no candidate path exists on disk.
 */
export function resolveDaemonPath(opts: ResolveDaemonPathOptions): ResolvedDaemonPath {
  const { isDev, resourcesPath, electronDistDir, envOverride } = opts;

  // 1. Env var override takes precedence
  if (envOverride) {
    if (fs.existsSync(envOverride)) {
      return { entryPath: envOverride, fromEnv: true, isDev };
    }
    throw new Error(
      `CRAB_DAEMON_ENTRY is set to "${envOverride}" but the file does not exist.`,
    );
  }

  // 2. Build candidate list based on mode
  const candidates: string[] = isDev
    ? [
        // Dev: compiled daemon script in dist-electron/daemon/
        path.join(electronDistDir, DAEMON_ENTRY_FILENAME),
      ]
    : [
        // Load JavaScript through ASAR so node-pty translates its native helper
        // path once. Loading from app.asar.unpacked expands that path twice.
        path.join(resourcesPath, RESOURCES_DAEMON_DIR, DAEMON_ENTRY_FILENAME),
      ];

  // 3. Return the first candidate that exists
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return { entryPath: candidate, fromEnv: false, isDev };
    }
  }

  // 4. No candidate found — throw with helpful message
  const modeLabel = isDev ? "development" : "production";
  throw new Error(
    `PTY daemon entry point not found (${modeLabel} mode).\n` +
      `Searched:\n  ${candidates.join("\n  ")}\n` +
      `Run the build step or set CRAB_DAEMON_ENTRY to override.`,
  );
}

/**
 * Convenience wrapper that reads the env var and detects the current
 * Electron environment automatically.
 *
 * This is the primary entry point for use in the Electron main process.
 * It mirrors the `resolveAgentBinary()` pattern in main.ts.
 *
 * @param isPackaged - `app.isPackaged` from Electron
 * @param resourcesPath - `process.resourcesPath` from Electron
 * @param distDir - Path to the `dist-electron/daemon/` directory.
 *   Defaults to `__dirname` when called from a file inside that directory.
 */
export function resolveDaemonEntryPath(
  isPackaged: boolean,
  resourcesPath: string,
  distDir: string,
): ResolvedDaemonPath {
  return resolveDaemonPath({
    isDev: !isPackaged,
    resourcesPath,
    electronDistDir: distDir,
    envOverride: process.env.CRAB_DAEMON_ENTRY || undefined,
  });
}
