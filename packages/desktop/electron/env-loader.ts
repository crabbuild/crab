/**
 * Layered .env credential resolution for Crab Desktop.
 *
 * Mirrors the Rust CLI's `load_env_layered()` in `crab/src/main.rs`.
 * On macOS, GUI apps don't inherit the user's shell environment, so
 * credentials set via `.env` files are invisible to Electron unless
 * we explicitly load them.
 *
 * Resolution order (highest priority first):
 *   Layer 1: process.env (already set vars are never overwritten)
 *   Layer 2: .env at the git/workspace repo root
 *   Layer 3: Walk up from the workspace root looking for .env or .crab/.env
 *   Layer 4: ~/.config/crab/.env (user-global XDG fallback)
 *   Layer 5: AWS default credential chain (handled by the Rust SDK)
 *
 * Each layer only fills in values not already present in process.env.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

/**
 * Parse a .env file into key-value pairs.
 * Supports:
 *   - KEY=VALUE
 *   - KEY="VALUE" (strips quotes)
 *   - KEY='VALUE' (strips quotes)
 *   - Comments (#) and blank lines are skipped
 *   - Does NOT support multi-line values or variable expansion
 */
function parseEnvFile(filePath: string): Record<string, string> {
  const vars: Record<string, string> = {};
  let content: string;
  try {
    content = fs.readFileSync(filePath, "utf8");
  } catch {
    return vars;
  }

  for (const line of content.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;

    const eqIdx = trimmed.indexOf("=");
    if (eqIdx < 1) continue;

    const key = trimmed.slice(0, eqIdx).trim();
    let value = trimmed.slice(eqIdx + 1).trim();

    // Strip surrounding quotes
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }

    vars[key] = value;
  }

  return vars;
}

/**
 * Load variables from a .env file into process.env without overwriting
 * existing values. Returns the set of keys that were actually loaded.
 */
function loadEnvFile(filePath: string): Set<string> {
  const loaded = new Set<string>();
  if (!fs.existsSync(filePath)) return loaded;

  const vars = parseEnvFile(filePath);
  for (const [key, value] of Object.entries(vars)) {
    if (process.env[key] === undefined) {
      process.env[key] = value;
      loaded.add(key);
    }
  }
  return loaded;
}

/**
 * Load credentials using the layered resolution strategy.
 *
 * @param workspacePath - The workspace/repo root path (if known).
 *   When null, only the global fallback is loaded.
 */
export function loadEnvLayered(workspacePath: string | null): void {
  const loadedPaths = new Set<string>();

  // Layer 2: .env at the workspace/repo root
  if (workspacePath) {
    const repoEnv = path.join(workspacePath, ".env");
    if (fs.existsSync(repoEnv)) {
      loadEnvFile(repoEnv);
      loadedPaths.add(repoEnv);
    }

    // Also check .crab/.env in the repo root
    const crabEnv = path.join(workspacePath, ".crab", ".env");
    if (fs.existsSync(crabEnv)) {
      loadEnvFile(crabEnv);
      loadedPaths.add(crabEnv);
    }
  }

  // Layer 3: Walk up from workspace root looking for .env or .crab/.env
  if (workspacePath) {
    let dir = path.dirname(workspacePath);
    const root = path.parse(dir).root;
    while (dir !== root) {
      const candidate = path.join(dir, ".env");
      if (!loadedPaths.has(candidate) && fs.existsSync(candidate)) {
        loadEnvFile(candidate);
        loadedPaths.add(candidate);
        break;
      }
      const crabCandidate = path.join(dir, ".crab", ".env");
      if (!loadedPaths.has(crabCandidate) && fs.existsSync(crabCandidate)) {
        loadEnvFile(crabCandidate);
        loadedPaths.add(crabCandidate);
        break;
      }
      dir = path.dirname(dir);
    }
  }

  // Layer 4: User-global XDG fallback (~/.config/crab/.env)
  const globalEnv = path.join(os.homedir(), ".config", "crab", ".env");
  if (!loadedPaths.has(globalEnv) && fs.existsSync(globalEnv)) {
    loadEnvFile(globalEnv);
  }
}

/**
 * Early boot env loading — called before any workspace is opened.
 * Loads only the global fallback so that the agent process gets
 * credentials even before a workspace is selected.
 */
export function loadEnvGlobal(): void {
  loadEnvLayered(null);
}
