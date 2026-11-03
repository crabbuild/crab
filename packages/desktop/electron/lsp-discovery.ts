import { app } from "electron";
import { execSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import { languageDir } from "./lsp-installer";

/**
 * A discovered language server on the user's system.
 */
export interface DiscoveredServer {
  /** Language ID (matches Monaco language IDs). */
  language: string;
  /** Command name (e.g. "rust-analyzer"). */
  command: string;
  /** Resolved absolute path to the binary. */
  path: string;
  /** Optional version string (from --version where supported). */
  version?: string;
  /**
   * Where the server was found:
   * - "bundled"   — ships inside the app (e.g. typescript-language-server)
   * - "installed" — downloaded by crab to ~/.crab/lsp/
   * - "system"    — found on PATH
   */
  source?: "bundled" | "installed" | "system";
}

/**
 * Known language servers and their binary names.
 */
const KNOWN_SERVERS: Array<{
  language: string;
  commands: string[];
  args?: string[];
  versionFlag?: string;
}> = [
  { language: "typescript", commands: ["typescript-language-server"], args: ["--stdio"], versionFlag: "--version" },
  { language: "javascript", commands: ["typescript-language-server"], args: ["--stdio"], versionFlag: "--version" },
  { language: "rust", commands: ["rust-analyzer"], versionFlag: "--version" },
  { language: "python", commands: ["pylsp", "pyright-langserver"], versionFlag: "--version" },
  { language: "go", commands: ["gopls"], args: ["serve"], versionFlag: "version" },
];

// ── Bundled server resolution ───────────────────────────────────────

/**
 * Resolve the path to a server that ships bundled with the app.
 * Currently only `typescript-language-server` is bundled via the
 * `typescript-language-server` npm dependency.
 *
 * Returns { command, args, path } where `command` may be the node
 * runtime (because the bundled server is a .mjs entrypoint).
 */
function resolveBundledTsServer(): { path: string; command: string; args: string[] } | null {
  // Candidate locations (dev vs packaged):
  //   dev:      <repo>/crab-desktop/node_modules/typescript-language-server/lib/cli.mjs
  //   packaged: <resources>/app.asar/node_modules/... (asar unpacked) or
  //             <resources>/app/node_modules/...
  const candidates: string[] = [];

  // In the packaged app, Electron exposes app.getAppPath() pointing at
  // the unpacked app directory (or asar file). node_modules sit inside.
  if (app?.getAppPath) {
    try {
      const appPath = app.getAppPath();
      candidates.push(path.join(appPath, "node_modules", "typescript-language-server", "lib", "cli.mjs"));
      // When the server is asar-unpacked (preferred — .mjs can't run from asar)
      candidates.push(path.join(appPath.replace(/\.asar$/, ".asar.unpacked"), "node_modules", "typescript-language-server", "lib", "cli.mjs"));
    } catch {
      // app may not be ready; fall through to dev candidates
    }
  }

  // Dev candidate: relative to the compiled electron bundle.
  // dist-electron/lsp-discovery.cjs → ../../node_modules/...
  candidates.push(path.join(__dirname, "..", "node_modules", "typescript-language-server", "lib", "cli.mjs"));
  candidates.push(path.join(__dirname, "..", "..", "node_modules", "typescript-language-server", "lib", "cli.mjs"));

  for (const candidate of candidates) {
    try {
      if (fs.existsSync(candidate)) {
        return {
          path: candidate,
          command: process.execPath,
          args: [candidate, "--stdio"],
        };
      }
    } catch {
      // Keep looking
    }
  }
  return null;
}

// ── PATH-based discovery ────────────────────────────────────────────

/**
 * Build the search PATH, augmenting with macOS-specific directories
 * (matching the existing `agentEnvFor` PATH augmentation in main.ts).
 */
function buildSearchPath(): string[] {
  const currentPath = process.env.PATH ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  const dirs = currentPath.split(path.delimiter);

  if (process.platform === "darwin" || process.platform === "linux") {
    const home = os.homedir();
    const extraPaths = [
      path.join(home, ".cargo", "bin"),
      "/opt/homebrew/bin",
      path.join(home, ".local", "bin"),
      "/usr/local/bin",
      path.join(home, "go", "bin"),
    ];
    for (const p of extraPaths) {
      if (!dirs.includes(p)) {
        dirs.push(p);
      }
    }
  }

  return dirs;
}

/**
 * Resolve a command name to an absolute path by searching the given dirs.
 */
function resolveCommand(command: string, searchDirs: string[]): string | null {
  const exe = process.platform === "win32" ? `${command}.exe` : command;
  for (const dir of searchDirs) {
    const fullPath = path.join(dir, exe);
    try {
      fs.accessSync(fullPath, fs.constants.X_OK);
      return fullPath;
    } catch {
      // Not found or not executable here
    }
  }
  return null;
}

/**
 * Try to get the version string from a server binary.
 */
function getVersion(binaryPath: string, versionFlag?: string): string | undefined {
  if (!versionFlag) return undefined;
  try {
    const output = execSync(`"${binaryPath}" ${versionFlag}`, {
      encoding: "utf8",
      timeout: 5000,
      stdio: ["pipe", "pipe", "pipe"],
    }).trim();
    const firstLine = output.split("\n")[0].trim();
    return firstLine || undefined;
  } catch {
    return undefined;
  }
}

// ── Cache ───────────────────────────────────────────────────────────

let cachedResults: DiscoveredServer[] | null = null;

/**
 * Discover available language servers.
 *
 * Resolution order per language:
 *   1. Bundled server (ships with the app)
 *   2. Installed server (downloaded to ~/.crab/lsp/)
 *   3. System PATH
 *
 * Results are cached in memory; call `refresh()` to re-scan.
 */
export function discoverServers(): DiscoveredServer[] {
  if (cachedResults !== null) return cachedResults;

  const searchDirs = buildSearchPath();
  const results: DiscoveredServer[] = [];
  const seenLanguages = new Set<string>();

  for (const server of KNOWN_SERVERS) {
    if (seenLanguages.has(server.language)) continue;

    // 1. Bundled (typescript only for now)
    if (server.language === "typescript" || server.language === "javascript") {
      const bundled = resolveBundledTsServer();
      if (bundled) {
        results.push({
          language: server.language,
          command: "typescript-language-server",
          path: bundled.path,
          version: "bundled",
          source: "bundled",
        });
        seenLanguages.add(server.language);
        continue;
      }
    }

    // 2. Installed via crab (~/.crab/lsp/<language>/)
    const installedPath = findInstalledBinary(server.language);
    if (installedPath) {
      results.push({
        language: server.language,
        command: server.commands[0],
        path: installedPath,
        version: getVersion(installedPath, server.versionFlag),
        source: "installed",
      });
      seenLanguages.add(server.language);
      continue;
    }

    // 3. System PATH
    for (const command of server.commands) {
      const resolved = resolveCommand(command, searchDirs);
      if (resolved) {
        results.push({
          language: server.language,
          command,
          path: resolved,
          version: getVersion(resolved, server.versionFlag),
          source: "system",
        });
        seenLanguages.add(server.language);
        break;
      }
    }
  }

  cachedResults = results;
  return results;
}

/**
 * Check whether a language server has been downloaded to ~/.crab/lsp/.
 * Synchronous sibling of `lsp-installer.findInstalledBinary`.
 */
function findInstalledBinary(language: string): string | null {
  const dir = languageDir(language);
  const ext = process.platform === "win32" ? ".exe" : "";
  const binName =
    language === "rust"
      ? `rust-analyzer${ext}`
      : language === "go"
        ? `gopls${ext}`
        : null;
  if (!binName) return null;
  const full = path.join(dir, binName);
  try {
    fs.accessSync(full, fs.constants.X_OK);
    return full;
  } catch {
    return null;
  }
}

/**
 * Clear the discovery cache and re-scan.
 */
export function refresh(): DiscoveredServer[] {
  cachedResults = null;
  return discoverServers();
}

/**
 * Get the default args for a known server command.
 */
export function getDefaultArgs(command: string): string[] {
  for (const server of KNOWN_SERVERS) {
    if (server.commands.includes(command)) {
      return server.args ?? [];
    }
  }
  return [];
}

/**
 * Resolve the command + args to spawn for a discovered server.
 *
 * Bundled servers need to be launched via the Node runtime (Electron's
 * bundled node); installed and system servers are spawned directly.
 */
export function resolveSpawnSpec(discovered: DiscoveredServer): { command: string; args: string[] } {
  if (discovered.source === "bundled" && discovered.command === "typescript-language-server") {
    // Electron's own executable has a Node runtime we can invoke with
    // ELECTRON_RUN_AS_NODE=1. That's how we run the .mjs entrypoint
    // without requiring the user to have Node on PATH.
    return {
      command: process.execPath,
      args: [discovered.path, "--stdio"],
    };
  }
  return {
    command: discovered.path,
    args: getDefaultArgs(discovered.command),
  };
}
